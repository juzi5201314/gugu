use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

use super::{
    error::ProjectError,
    manifest::{RawManifest, RawPackage, RawTarget},
    model::{Target, TargetKind},
    sorted_entries,
};

pub(crate) fn discover_targets(
    root: &Path,
    package: &RawPackage,
    raw: &RawManifest,
    manifest: &Path,
) -> Result<Vec<Target>, ProjectError> {
    let mut targets = Vec::new();
    let lib_name = package.name.as_deref().unwrap_or("lib").replace('-', "_");
    if let Some(lib) = raw.lib.as_ref() {
        targets.push(explicit_target(
            root,
            TargetKind::Lib,
            lib.path.clone(),
            Some(lib_name),
            Vec::new(),
            true,
            manifest,
        )?);
    } else if package.auto_lib.unwrap_or(true) {
        add_auto_file(
            &mut targets,
            root,
            TargetKind::Lib,
            AutoFile {
                name: &lib_name,
                path: Path::new("src/lib.gg"),
                required_features: Vec::new(),
                harness: true,
            },
            manifest,
        )?;
    }
    if package.auto_bins.unwrap_or(true) {
        add_auto_file(
            &mut targets,
            root,
            TargetKind::Bin,
            AutoFile {
                name: package.name.as_deref().unwrap_or("main"),
                path: Path::new("src/main.gg"),
                required_features: Vec::new(),
                harness: true,
            },
            manifest,
        )?;
        discover_directory_targets(
            &mut targets,
            root,
            TargetKind::Bin,
            Path::new("src/bin"),
            true,
            manifest,
        )?;
    }
    add_explicit_targets(
        &mut targets,
        root,
        TargetKind::Bin,
        &raw.bin,
        true,
        manifest,
    )?;
    if package.auto_tests.unwrap_or(true) {
        discover_directory_targets(
            &mut targets,
            root,
            TargetKind::Test,
            Path::new("tests"),
            true,
            manifest,
        )?;
    }
    add_explicit_targets(
        &mut targets,
        root,
        TargetKind::Test,
        &raw.test,
        true,
        manifest,
    )?;
    if package.auto_benches.unwrap_or(true) {
        discover_directory_targets(
            &mut targets,
            root,
            TargetKind::Bench,
            Path::new("benches"),
            true,
            manifest,
        )?;
    }
    add_explicit_targets(
        &mut targets,
        root,
        TargetKind::Bench,
        &raw.bench,
        true,
        manifest,
    )?;
    if package.auto_examples.unwrap_or(true) {
        discover_directory_targets(
            &mut targets,
            root,
            TargetKind::Example,
            Path::new("examples"),
            true,
            manifest,
        )?;
    }
    add_explicit_targets(
        &mut targets,
        root,
        TargetKind::Example,
        &raw.example,
        true,
        manifest,
    )?;
    if package.auto_build.unwrap_or(true) {
        add_auto_file(
            &mut targets,
            root,
            TargetKind::Build,
            AutoFile {
                name: "build",
                path: Path::new("build.gg"),
                required_features: Vec::new(),
                harness: true,
            },
            manifest,
        )?;
    }
    targets.sort_by(|left, right| {
        (left.kind(), left.name(), left.entry()).cmp(&(right.kind(), right.name(), right.entry()))
    });
    check_target_duplicates(&targets, manifest)?;
    let mut source_roots = BTreeSet::new();
    for target in &targets {
        source_roots.insert(target.source_root().to_path_buf());
    }
    for source_root in source_roots {
        check_module_conflicts(&source_root, manifest)?;
    }
    Ok(targets)
}

fn add_explicit_targets(
    targets: &mut Vec<Target>,
    root: &Path,
    kind: TargetKind,
    definitions: &[RawTarget],
    default_harness: bool,
    manifest: &Path,
) -> Result<(), ProjectError> {
    for definition in definitions {
        let default_name = definition
            .path
            .as_deref()
            .and_then(Path::file_stem)
            .and_then(|value| value.to_str())
            .unwrap_or("target");
        let name = definition
            .name
            .clone()
            .unwrap_or_else(|| default_name.to_owned());
        validate_target_name(&name, manifest)?;
        targets.push(explicit_target(
            root,
            kind,
            definition.path.clone(),
            Some(name),
            definition.required_features.clone(),
            definition.harness.unwrap_or(default_harness),
            manifest,
        )?);
    }
    Ok(())
}

fn explicit_target(
    root: &Path,
    kind: TargetKind,
    path: Option<PathBuf>,
    name: Option<String>,
    required_features: Vec<String>,
    harness: bool,
    manifest: &Path,
) -> Result<Target, ProjectError> {
    let path = path.unwrap_or_else(|| default_target_path(kind, name.as_deref()));
    let entry = resolve_entry(root, &path, manifest)?;
    let name = name.unwrap_or_else(|| default_target_name(&entry, root));
    validate_target_name(&name, manifest)?;
    Ok(Target::new(
        kind,
        name,
        entry.clone(),
        source_root(root, &entry, kind, manifest)?,
        normalize_features(required_features),
        harness,
    ))
}

struct AutoFile<'a> {
    name: &'a str,
    path: &'a Path,
    required_features: Vec<String>,
    harness: bool,
}

fn add_auto_file(
    targets: &mut Vec<Target>,
    root: &Path,
    kind: TargetKind,
    file: AutoFile<'_>,
    manifest: &Path,
) -> Result<(), ProjectError> {
    if !root.join(file.path).is_file() {
        return Ok(());
    }
    targets.push(explicit_target(
        root,
        kind,
        Some(file.path.to_path_buf()),
        Some(file.name.to_owned()),
        file.required_features,
        file.harness,
        manifest,
    )?);
    Ok(())
}

fn discover_directory_targets(
    targets: &mut Vec<Target>,
    root: &Path,
    kind: TargetKind,
    directory: &Path,
    harness: bool,
    manifest: &Path,
) -> Result<(), ProjectError> {
    let directory = root.join(directory);
    if !directory.is_dir() {
        return Ok(());
    }
    let entries = sorted_entries(&directory, manifest)?;
    let mut names = BTreeSet::new();
    for entry in entries {
        let file_type = entry.file_type().map_err(|error| ProjectError::Io {
            path: entry.path(),
            message: error.to_string(),
        })?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            return Err(ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: "target 名必须是 UTF-8".to_owned(),
            });
        };
        let (name, path) = if file_type.is_file() && file_name.ends_with(".gg") {
            let name = file_name.trim_end_matches(".gg");
            (name.to_owned(), entry.path())
        } else if file_type.is_dir() {
            let main = entry.path().join("main.gg");
            if !main.is_file() {
                continue;
            }
            (file_name.to_owned(), main)
        } else {
            continue;
        };
        validate_target_name(&name, manifest)?;
        if !names.insert(name.clone()) {
            return Err(ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: format!("{kind} target `{name}` 同时存在文件和目录入口"),
            });
        }
        let relative_path = path
            .strip_prefix(root)
            .map_err(|_| ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: format!("target 入口 `{}` 越过 package 根", path.display()),
            })?
            .to_path_buf();
        targets.push(explicit_target(
            root,
            kind,
            Some(relative_path),
            Some(name),
            Vec::new(),
            harness,
            manifest,
        )?);
    }
    Ok(())
}

fn resolve_entry(root: &Path, path: &Path, manifest: &Path) -> Result<PathBuf, ProjectError> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || path.to_string_lossy().contains('\\')
    {
        return Err(ProjectError::TargetDiscovery {
            package: manifest.to_path_buf(),
            message: format!("target 入口 `{}` 不是 package 内相对路径", path.display()),
        });
    }
    let candidate = root.join(path);
    let entry = fs::canonicalize(&candidate).map_err(|error| ProjectError::TargetDiscovery {
        package: manifest.to_path_buf(),
        message: format!("无法读取 target 入口 `{}`：{error}", path.display()),
    })?;
    if !entry.starts_with(root) || !entry.is_file() {
        return Err(ProjectError::TargetDiscovery {
            package: manifest.to_path_buf(),
            message: format!("target 入口 `{}` 不在 package 内或不是文件", path.display()),
        });
    }
    Ok(entry)
}

fn source_root(
    root: &Path,
    entry: &Path,
    kind: TargetKind,
    manifest: &Path,
) -> Result<PathBuf, ProjectError> {
    if kind == TargetKind::Build {
        return Ok(root.to_path_buf());
    }
    let relative = entry
        .strip_prefix(root)
        .map_err(|_| ProjectError::TargetDiscovery {
            package: manifest.to_path_buf(),
            message: format!("入口 `{}` 越过 package 根", entry.display()),
        })?;
    let Some(first) = relative.components().next() else {
        return Err(ProjectError::TargetDiscovery {
            package: manifest.to_path_buf(),
            message: "target 入口缺少源码根".to_owned(),
        });
    };
    let source_root = root.join(first.as_os_str());
    fs::canonicalize(&source_root).map_err(|error| ProjectError::TargetDiscovery {
        package: manifest.to_path_buf(),
        message: format!(
            "无法读取 target 源码根 `{}`：{error}",
            source_root.display()
        ),
    })
}

fn check_target_duplicates(targets: &[Target], manifest: &Path) -> Result<(), ProjectError> {
    for pair in targets.windows(2) {
        if pair[0].kind() == pair[1].kind() && pair[0].name() == pair[1].name() {
            return Err(ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: format!("{} target `{}` 重名", pair[0].kind(), pair[0].name()),
            });
        }
    }
    Ok(())
}

fn check_module_conflicts(root: &Path, manifest: &Path) -> Result<(), ProjectError> {
    let mut files = BTreeSet::new();
    collect_source_files(root, &mut files, manifest)?;
    for file in &files {
        let Some(name) = file.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".gg") || name == "mod.gg" {
            continue;
        }
        let stem = name.trim_end_matches(".gg");
        let directory_form = file.with_file_name(stem).join("mod.gg");
        if files.contains(&directory_form) {
            return Err(ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: format!(
                    "模块 `{}` 与 `{}` 同时存在",
                    file.display(),
                    directory_form.display()
                ),
            });
        }
    }
    Ok(())
}

fn collect_source_files(
    directory: &Path,
    files: &mut BTreeSet<PathBuf>,
    manifest: &Path,
) -> Result<(), ProjectError> {
    for entry in sorted_entries(directory, manifest)? {
        let file_type = entry.file_type().map_err(|error| ProjectError::Io {
            path: entry.path(),
            message: error.to_string(),
        })?;
        if file_type.is_symlink() {
            return Err(ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: format!("源码树不允许符号链接 `{}`", entry.path().display()),
            });
        }
        if file_type.is_dir() {
            collect_source_files(&entry.path(), files, manifest)?;
        } else if file_type.is_file() && entry.path().extension().is_some_and(|ext| ext == "gg") {
            files.insert(entry.path());
        }
    }
    Ok(())
}

fn default_target_path(kind: TargetKind, name: Option<&str>) -> PathBuf {
    match kind {
        TargetKind::Lib => PathBuf::from("src/lib.gg"),
        TargetKind::Bin => name.map_or_else(
            || PathBuf::from("src/main.gg"),
            |name| PathBuf::from(format!("src/bin/{name}.gg")),
        ),
        TargetKind::Test => PathBuf::from(format!("tests/{}.gg", name.unwrap_or("test"))),
        TargetKind::Bench => PathBuf::from(format!("benches/{}.gg", name.unwrap_or("bench"))),
        TargetKind::Example => PathBuf::from(format!("examples/{}.gg", name.unwrap_or("example"))),
        TargetKind::Build => PathBuf::from("build.gg"),
    }
}

fn default_target_name(entry: &Path, root: &Path) -> String {
    entry
        .strip_prefix(root)
        .ok()
        .and_then(|path| path.file_stem())
        .and_then(|value| value.to_str())
        .unwrap_or("target")
        .to_owned()
}

fn normalize_features(mut features: Vec<String>) -> Vec<String> {
    features.sort();
    features.dedup();
    features
}

fn validate_target_name(name: &str, manifest: &Path) -> Result<(), ProjectError> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(ProjectError::TargetDiscovery {
            package: manifest.to_path_buf(),
            message: format!("target 名 `{name}` 不是合法名称"),
        })
    }
}

#[cfg(test)]
#[path = "targets_tests.rs"]
mod tests;
