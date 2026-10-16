use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use super::{
    error::ProjectError,
    manifest::{RawManifest, RawWorkspace},
    model::Package,
};

pub(crate) fn discover_workspace_members(
    root: &Path,
    workspace: &RawWorkspace,
) -> Result<Vec<PathBuf>, ProjectError> {
    let mut members = BTreeSet::new();
    for pattern in &workspace.members {
        for directory in expand_directory_pattern(root, pattern)? {
            let manifest = directory.join("gugu.toml");
            if !manifest.is_file() {
                return Err(ProjectError::WorkspaceMember {
                    path: directory,
                    workspace: root.to_path_buf(),
                });
            }
            members.insert(
                fs::canonicalize(manifest).map_err(|error| ProjectError::Io {
                    path: directory,
                    message: error.to_string(),
                })?,
            );
        }
    }
    let mut excluded = BTreeSet::new();
    for pattern in &workspace.exclude {
        for directory in expand_exclude_pattern(root, pattern)? {
            excluded.insert(fs::canonicalize(&directory).unwrap_or(directory));
        }
    }
    members.retain(|manifest| {
        manifest
            .parent()
            .is_some_and(|directory| !excluded.contains(directory))
    });
    Ok(members.into_iter().collect())
}

pub(crate) fn resolve_default_members(
    root: &Path,
    workspace_manifest: &RawManifest,
    packages: &[Package],
) -> Result<Vec<PathBuf>, ProjectError> {
    let Some(workspace) = workspace_manifest.workspace.as_ref() else {
        return Ok(Vec::new());
    };
    let mut defaults = BTreeSet::new();
    for pattern in &workspace.default_members {
        for directory in expand_directory_pattern(root, pattern)? {
            if !packages.iter().any(|package| package.root() == directory) {
                return Err(ProjectError::WorkspaceMember {
                    path: directory,
                    workspace: root.to_path_buf(),
                });
            }
            defaults.insert(directory);
        }
    }
    Ok(defaults.into_iter().collect())
}

fn expand_directory_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, ProjectError> {
    let matches = expand_exclude_pattern(root, pattern)?;
    if matches.is_empty() {
        return Err(ProjectError::WorkspaceMember {
            path: root.join(pattern),
            workspace: root.to_path_buf(),
        });
    }
    Ok(matches)
}

fn expand_exclude_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, ProjectError> {
    let components = split_pattern(pattern).ok_or_else(|| ProjectError::WorkspaceMember {
        path: root.join(pattern),
        workspace: root.to_path_buf(),
    })?;
    let mut matches = Vec::new();
    match_directories(root, root, &components, 0, &mut matches)?;
    matches.sort();
    Ok(matches)
}

/// 按 pattern 分量逐段匹配，只下钻可能匹配的前缀目录，避免全树扫描。
///
/// `**` 匹配零层或任意层：先消费掉 `**` 的零层匹配（继续匹配剩余
/// pattern），再对每个子目录保留 `**` 继续下钻。
fn match_directories(
    root: &Path,
    directory: &Path,
    pattern: &[String],
    index: usize,
    matches: &mut Vec<PathBuf>,
) -> Result<(), ProjectError> {
    if index == pattern.len() {
        matches.push(directory.to_path_buf());
        return Ok(());
    }
    let current = &pattern[index];
    let mut entries = fs::read_dir(directory)
        .map_err(|error| ProjectError::Io {
            path: directory.to_path_buf(),
            message: error.to_string(),
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ProjectError::Io {
            path: directory.to_path_buf(),
            message: error.to_string(),
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut subdirectories = Vec::new();
    for entry in entries {
        let file_type = entry.file_type().map_err(|error| ProjectError::Io {
            path: entry.path(),
            message: error.to_string(),
        })?;
        // 符号链接既不算成员目录，也不是可安全下钻的目标。
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let path = entry.path();
        if !path.starts_with(root) {
            continue;
        }
        if current == "**" || wildcard_component(current, name) {
            subdirectories.push(path);
        }
    }
    if current == "**" {
        // 零层：跳过 `**`，直接匹配剩余 pattern。
        match_directories(root, directory, pattern, index + 1, matches)?;
    }
    for path in subdirectories {
        if current == "**" {
            // 一层：子目录消费 `**` 后继续匹配。
            match_directories(root, &path, pattern, index, matches)?;
        } else {
            match_directories(root, &path, pattern, index + 1, matches)?;
        }
    }
    Ok(())
}

fn split_pattern(pattern: &str) -> Option<Vec<String>> {
    if pattern.is_empty() || pattern.starts_with('/') || pattern.contains('\\') {
        return None;
    }
    let mut components = Vec::new();
    for component in pattern.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        components.push(component.to_owned());
    }
    Some(components)
}

fn wildcard_component(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut pattern_index = 0;
    let mut value_index = 0;
    let mut star = None;
    let mut star_value = 0;
    while value_index < value.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == value[value_index] {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_value = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_value += 1;
            value_index = star_value;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

pub(crate) fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
