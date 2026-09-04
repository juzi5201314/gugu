use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use super::super::{
    error::ProjectError,
    manifest::{self, RawManifest},
    model::Package,
};
use super::{
    dependency_model::*,
    semver::{Version, VersionReq},
};

pub(super) fn package_metadata(
    workspace_root: &Path,
    package: &Package,
    workspace_dependencies: &BTreeMap<String, toml::Value>,
) -> Result<PackageMetadata, ProjectError> {
    let raw = manifest::read_manifest(package.manifest())?;
    let version = Version::parse(package.version())
        .map_err(|message| invalid_manifest(package.manifest(), message))?;
    let id = PackageId::new(
        package.package_name(),
        version,
        PackageSource::Path {
            path: relative_path(workspace_root, package.root()),
        },
    );
    parse_manifest_metadata(
        package.root(),
        package.manifest(),
        &raw,
        id,
        workspace_dependencies,
    )
}

fn parse_manifest_metadata(
    package_root: &Path,
    manifest_path: &Path,
    raw: &RawManifest,
    id: PackageId,
    workspace_dependencies: &BTreeMap<String, toml::Value>,
) -> Result<PackageMetadata, ProjectError> {
    let mut metadata = PackageMetadata::new(id);
    metadata.features = raw.features.clone().unwrap_or_default();
    let mut dependencies = Vec::new();
    parse_table(
        &mut dependencies,
        raw.dependencies.as_ref(),
        DependencyDomain::Normal,
        None,
        package_root,
        manifest_path,
        workspace_dependencies,
    )?;
    parse_table(
        &mut dependencies,
        raw.test_dependencies.as_ref(),
        DependencyDomain::Test,
        None,
        package_root,
        manifest_path,
        workspace_dependencies,
    )?;
    if let Some(target) = raw.target.as_ref().and_then(toml::Value::as_table) {
        for (condition, value) in target {
            let condition = TargetCondition::parse(condition)
                .map_err(|message| invalid_manifest(manifest_path, message))?;
            let table = value
                .as_table()
                .ok_or_else(|| invalid_manifest(manifest_path, "target 条件表必须是 TOML table"))?;
            parse_table(
                &mut dependencies,
                table.get("dependencies").and_then(toml::Value::as_table),
                DependencyDomain::Normal,
                Some(condition.clone()),
                package_root,
                manifest_path,
                workspace_dependencies,
            )?;
            parse_table(
                &mut dependencies,
                table
                    .get("test-dependencies")
                    .and_then(toml::Value::as_table),
                DependencyDomain::Test,
                Some(condition),
                package_root,
                manifest_path,
                workspace_dependencies,
            )?;
        }
    }
    if let Some(build) = raw.build.as_ref().and_then(toml::Value::as_table) {
        parse_table(
            &mut dependencies,
            build.get("dependencies").and_then(toml::Value::as_table),
            DependencyDomain::Build,
            None,
            package_root,
            manifest_path,
            workspace_dependencies,
        )?;
        if let Some(target) = build.get("target").and_then(toml::Value::as_table) {
            for (condition, value) in target {
                let condition = TargetCondition::parse(condition)
                    .map_err(|message| invalid_manifest(manifest_path, message))?;
                let table = value.as_table().ok_or_else(|| {
                    invalid_manifest(manifest_path, "build.target 条件表必须是 TOML table")
                })?;
                parse_table(
                    &mut dependencies,
                    table.get("dependencies").and_then(toml::Value::as_table),
                    DependencyDomain::Build,
                    Some(condition),
                    package_root,
                    manifest_path,
                    workspace_dependencies,
                )?;
            }
        }
    }
    dependencies.sort();
    metadata.dependencies = dependencies;
    Ok(metadata)
}

fn parse_table<T>(
    output: &mut Vec<DependencySpec>,
    table: Option<&T>,
    domain: DependencyDomain,
    target: Option<TargetCondition>,
    package_root: &Path,
    manifest_path: &Path,
    workspace_dependencies: &BTreeMap<String, toml::Value>,
) -> Result<(), ProjectError>
where
    for<'table> &'table T: IntoIterator<Item = (&'table String, &'table toml::Value)>,
{
    let Some(table) = table else {
        return Ok(());
    };
    parse_entries(
        output,
        table.into_iter(),
        domain,
        target,
        package_root,
        manifest_path,
        workspace_dependencies,
    )
}

fn parse_entries<'entry, Entries>(
    output: &mut Vec<DependencySpec>,
    entries: Entries,
    domain: DependencyDomain,
    target: Option<TargetCondition>,
    package_root: &Path,
    manifest_path: &Path,
    workspace_dependencies: &BTreeMap<String, toml::Value>,
) -> Result<(), ProjectError>
where
    Entries: IntoIterator<Item = (&'entry String, &'entry toml::Value)>,
{
    for (alias, raw_value) in entries {
        let value = inherited_value(alias, raw_value, workspace_dependencies, manifest_path)?;
        output.push(parse_dependency(
            alias,
            &value,
            domain,
            target.clone(),
            package_root,
            manifest_path,
        )?);
    }
    Ok(())
}

fn inherited_value(
    alias: &str,
    value: &toml::Value,
    workspace_dependencies: &BTreeMap<String, toml::Value>,
    manifest_path: &Path,
) -> Result<toml::Value, ProjectError> {
    let Some(table) = value.as_table() else {
        return Ok(value.clone());
    };
    if !table
        .get("workspace")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(value.clone());
    }
    let inherited = workspace_dependencies.get(alias).ok_or_else(|| {
        invalid_manifest(
            manifest_path,
            format!("workspace dependency `{alias}` 不存在"),
        )
    })?;
    let Some(inherited_table) = inherited.as_table() else {
        return Ok(inherited.clone());
    };
    let mut merged = inherited_table.clone();
    for (key, local) in table {
        if key == "workspace" {
            continue;
        }
        if key == "features" {
            if let (Some(base), Some(extra)) = (
                merged.get_mut(key).and_then(toml::Value::as_array_mut),
                local.as_array(),
            ) {
                base.extend(extra.iter().cloned());
                continue;
            }
        }
        merged.insert(key.clone(), local.clone());
    }
    Ok(toml::Value::Table(merged))
}

fn parse_dependency(
    alias: &str,
    value: &toml::Value,
    domain: DependencyDomain,
    target: Option<TargetCondition>,
    _package_root: &Path,
    manifest_path: &Path,
) -> Result<DependencySpec, ProjectError> {
    let table = value.as_table();
    if let Some(table) = table {
        const ALLOWED: &[&str] = &[
            "package",
            "version",
            "registry",
            "path",
            "git",
            "rev",
            "branch",
            "tag",
            "features",
            "default-features",
            "optional",
            "workspace",
        ];
        if let Some(unknown) = table.keys().find(|key| !ALLOWED.contains(&key.as_str())) {
            return Err(invalid_manifest(
                manifest_path,
                format!("依赖 `{alias}` 包含未知字段 `{unknown}`"),
            ));
        }
    }
    let package = table
        .and_then(|table| table.get("package"))
        .and_then(toml::Value::as_str)
        .unwrap_or(alias)
        .to_owned();
    let version = VersionReq::parse(
        table
            .and_then(|table| table.get("version"))
            .and_then(toml::Value::as_str)
            .unwrap_or("*"),
    )
    .map_err(|message| {
        invalid_manifest(
            manifest_path,
            format!("依赖 `{alias}` 的版本约束无效：{message}"),
        )
    })?;
    let path = table
        .and_then(|table| table.get("path"))
        .and_then(toml::Value::as_str);
    let git = table
        .and_then(|table| table.get("git"))
        .and_then(toml::Value::as_str);
    if path.is_some() && git.is_some() {
        return Err(invalid_manifest(
            manifest_path,
            format!("依赖 `{alias}` 不能同时设置 path 和 git"),
        ));
    }
    let source = if let Some(path) = path {
        if Path::new(path).is_absolute() || path.contains('\\') || path.contains('\0') {
            return Err(invalid_manifest(
                manifest_path,
                format!("依赖 `{alias}` 的 path 不是规范相对路径"),
            ));
        }
        DependencySource::Path {
            path: PathBuf::from(path),
        }
    } else if let Some(url) = git {
        let rev = table
            .and_then(|table| table.get("rev"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned);
        let tag = table
            .and_then(|table| table.get("tag"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned);
        let branch = table
            .and_then(|table| table.get("branch"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned);
        if usize::from(rev.is_some()) + usize::from(tag.is_some()) + usize::from(branch.is_some())
            > 1
        {
            return Err(invalid_manifest(
                manifest_path,
                format!("依赖 `{alias}` 的 Git 引用最多设置一个"),
            ));
        }
        DependencySource::Git {
            url: url.to_owned(),
            rev,
            tag,
            branch,
        }
    } else {
        DependencySource::Registry {
            registry: table
                .and_then(|table| table.get("registry"))
                .and_then(toml::Value::as_str)
                .unwrap_or("default")
                .to_owned(),
        }
    };
    let features = table
        .and_then(|table| table.get("features"))
        .and_then(toml::Value::as_array)
        .map(|features| {
            features
                .iter()
                .map(|feature| {
                    feature
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "feature 必须是字符串".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .map_err(|message| {
            invalid_manifest(
                manifest_path,
                format!("依赖 `{alias}` 的 feature 无效：{message}"),
            )
        })?
        .unwrap_or_default();
    let default_features = table
        .and_then(|table| table.get("default-features"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    let optional = table
        .and_then(|table| table.get("optional"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    Ok(DependencySpec {
        alias: normalize_alias(alias, manifest_path)?,
        package,
        version,
        source,
        features,
        default_features,
        optional,
        domain,
        target,
    })
}

fn normalize_alias(alias: &str, manifest_path: &Path) -> Result<String, ProjectError> {
    let alias = alias.rsplit('/').next().unwrap_or(alias).replace('-', "_");
    let mut bytes = alias.bytes();
    if !alias.is_empty()
        && (alias.starts_with('_') || bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic()))
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Ok(alias);
    }
    Err(invalid_manifest(
        manifest_path,
        format!("依赖别名 `{alias}` 不是合法 Gugu 标识符"),
    ))
}

pub(super) fn canonical_path(
    parent: &Path,
    path: &Path,
    package: &str,
) -> Result<PathBuf, ProjectError> {
    fs::canonicalize(parent.join(path)).map_err(|error| {
        dependency_error(
            package,
            format!("path source `{}` 不可读取：{error}", path.display()),
        )
    })
}

fn relative_path(root: &Path, path: &Path) -> String {
    let root = root.components().collect::<Vec<_>>();
    let path = path.components().collect::<Vec<_>>();
    let common = root
        .iter()
        .zip(&path)
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = Vec::new();
    result.extend(std::iter::repeat_n("..", root.len() - common));
    result.extend(
        path[common..]
            .iter()
            .filter_map(|component| component.as_os_str().to_str()),
    );
    if result.is_empty() {
        ".".to_owned()
    } else {
        result.join("/")
    }
}

pub(super) fn dependency_error(package: &str, message: impl Into<String>) -> ProjectError {
    ProjectError::DependencyResolution {
        package: package.to_owned(),
        message: message.into(),
    }
}

fn invalid_manifest(path: &Path, message: impl Into<String>) -> ProjectError {
    ProjectError::InvalidManifest {
        path: path.to_path_buf(),
        message: message.into(),
    }
}
