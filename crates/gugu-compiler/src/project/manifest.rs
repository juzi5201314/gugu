use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use super::{error::ProjectError, model::Package, targets::discover_targets};

#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct RawManifest {
    pub(crate) package: Option<RawPackage>,
    pub(crate) workspace: Option<RawWorkspace>,
    pub(crate) lib: Option<RawLib>,
    pub(crate) bin: Vec<RawTarget>,
    pub(crate) test: Vec<RawTarget>,
    pub(crate) bench: Vec<RawTarget>,
    pub(crate) example: Vec<RawTarget>,
    pub(crate) dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "test-dependencies")]
    pub(crate) test_dependencies: Option<BTreeMap<String, toml::Value>>,
    pub(crate) build: Option<toml::Value>,
    pub(crate) target: Option<toml::Value>,
    pub(crate) features: Option<BTreeMap<String, Vec<String>>>,
    pub(crate) patch: Option<toml::Value>,
    pub(crate) metadata: Option<BTreeMap<String, toml::Value>>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct RawPackage {
    pub(crate) owner: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) license: Option<String>,
    pub(crate) repository: Option<String>,
    pub(crate) homepage: Option<String>,
    pub(crate) documentation: Option<String>,
    pub(crate) readme: Option<String>,
    pub(crate) authors: Vec<String>,
    pub(crate) keywords: Vec<String>,
    pub(crate) categories: Vec<String>,
    pub(crate) publish: Option<bool>,
    #[serde(rename = "default-run")]
    pub(crate) default_run: Option<String>,
    pub(crate) include: Vec<String>,
    pub(crate) exclude: Vec<String>,
    #[serde(rename = "auto-lib")]
    pub(crate) auto_lib: Option<bool>,
    #[serde(rename = "auto-bins")]
    pub(crate) auto_bins: Option<bool>,
    #[serde(rename = "auto-tests")]
    pub(crate) auto_tests: Option<bool>,
    #[serde(rename = "auto-benches")]
    pub(crate) auto_benches: Option<bool>,
    #[serde(rename = "auto-examples")]
    pub(crate) auto_examples: Option<bool>,
    #[serde(rename = "auto-build")]
    pub(crate) auto_build: Option<bool>,
    pub(crate) metadata: Option<BTreeMap<String, toml::Value>>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct RawWorkspace {
    pub(crate) members: Vec<String>,
    pub(crate) exclude: Vec<String>,
    #[serde(rename = "default-members")]
    pub(crate) default_members: Vec<String>,
    pub(crate) package: Option<toml::Value>,
    pub(crate) dependencies: Option<BTreeMap<String, toml::Value>>,
    pub(crate) lints: Option<BTreeMap<String, toml::Value>>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct RawLib {
    pub(crate) path: Option<PathBuf>,
    pub(crate) artifacts: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct RawTarget {
    pub(crate) name: Option<String>,
    pub(crate) path: Option<PathBuf>,
    #[serde(rename = "required-features")]
    pub(crate) required_features: Vec<String>,
    pub(crate) harness: Option<bool>,
}

pub(crate) fn find_manifest(start: &Path) -> Result<PathBuf, ProjectError> {
    let start = fs::canonicalize(start).map_err(|error| ProjectError::Io {
        path: start.to_path_buf(),
        message: error.to_string(),
    })?;
    let metadata = fs::metadata(&start).map_err(|error| ProjectError::Io {
        path: start.clone(),
        message: error.to_string(),
    })?;
    let mut directory = if metadata.is_file() {
        start
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| ProjectError::ManifestNotFound {
                start: start.clone(),
            })?
    } else {
        start
    };
    loop {
        let candidate = directory.join("gugu.toml");
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !directory.pop() {
            break;
        }
    }
    Err(ProjectError::ManifestNotFound { start: directory })
}

pub(crate) fn find_workspace_manifest(
    local_manifest: &Path,
    local: &RawManifest,
) -> Result<(PathBuf, RawManifest), ProjectError> {
    if local.workspace.is_some() {
        return Ok((local_manifest.to_path_buf(), read_manifest(local_manifest)?));
    }
    let mut directory = local_manifest
        .parent()
        .expect("manifest has a parent")
        .to_path_buf();
    while directory.pop() {
        let candidate = directory.join("gugu.toml");
        if candidate.is_file() {
            let raw = read_manifest(&candidate)?;
            if raw.workspace.is_some() {
                return Ok((candidate, raw));
            }
        }
    }
    Ok((local_manifest.to_path_buf(), read_manifest(local_manifest)?))
}

pub(crate) fn read_manifest(path: &Path) -> Result<RawManifest, ProjectError> {
    let source = fs::read_to_string(path).map_err(|error| ProjectError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    toml::from_str(&source).map_err(|error| ProjectError::InvalidManifest {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

pub(crate) fn build_package(manifest: &Path) -> Result<Package, ProjectError> {
    let raw = read_manifest(manifest)?;
    let package = raw
        .package
        .as_ref()
        .ok_or_else(|| ProjectError::InvalidManifest {
            path: manifest.to_path_buf(),
            message: "workspace 成员必须包含 [package]".to_owned(),
        })?;
    let name = package
        .name
        .as_ref()
        .ok_or_else(|| ProjectError::InvalidManifest {
            path: manifest.to_path_buf(),
            message: "[package].name 是必填字段".to_owned(),
        })?
        .clone();
    validate_package_component(&name, "package.name", manifest)?;
    if name == "std" {
        return Err(ProjectError::InvalidManifest {
            path: manifest.to_path_buf(),
            message: "package 名 `std` 是保留名称".to_owned(),
        });
    }
    if let Some(owner) = package.owner.as_deref() {
        validate_package_component(owner, "package.owner", manifest)?;
    }
    reject_std_dependency(&raw, manifest)?;
    let root = manifest.parent().expect("manifest has a parent");
    let root = fs::canonicalize(root).map_err(|error| ProjectError::Io {
        path: root.to_path_buf(),
        message: error.to_string(),
    })?;
    let manifest = root.join("gugu.toml");
    let targets = discover_targets(&root, package, &raw, &manifest)?;
    Ok(Package::new(
        root,
        manifest,
        package.owner.clone(),
        name,
        package
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_owned()),
        targets,
    ))
}

fn validate_package_component(
    value: &str,
    field: &str,
    manifest: &Path,
) -> Result<(), ProjectError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(ProjectError::InvalidManifest {
            path: manifest.to_path_buf(),
            message: format!("{field} `{value}` 不是合法 package 名"),
        })
    }
}

fn reject_std_dependency(raw: &RawManifest, manifest: &Path) -> Result<(), ProjectError> {
    let has_std = raw
        .dependencies
        .as_ref()
        .is_some_and(|dependencies| dependencies.contains_key("std"))
        || raw
            .test_dependencies
            .as_ref()
            .is_some_and(|dependencies| dependencies.contains_key("std"));
    if has_std {
        return Err(ProjectError::InvalidManifest {
            path: manifest.to_path_buf(),
            message: "package 不能声明保留依赖别名 `std`".to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod tests;
