use std::fs;

use tempfile::TempDir;

use super::super::{Project, support::package};
use super::*;

#[test]
fn rejects_std_package_and_std_dependency() {
    let root = TempDir::new().expect("tempdir");
    let std_package = package(
        &root,
        "std-pkg",
        "[package]\nname = \"std\"\n",
        &[("src/main.gg", "fn main() {}\n")],
    );
    assert!(matches!(
        Project::discover(&std_package),
        Err(ProjectError::InvalidManifest { .. })
    ));

    let std_dep = package(
        &root,
        "std-dep",
        "[package]\nname = \"app\"\n\n[dependencies]\nstd = \"1\"\n",
        &[("src/main.gg", "fn main() {}\n")],
    );
    assert!(matches!(
        Project::discover(&std_dep),
        Err(ProjectError::InvalidManifest { .. })
    ));
}

#[test]
fn rejects_unknown_manifest_field_and_missing_manifest() {
    let root = TempDir::new().expect("tempdir");
    let unknown = package(
        &root,
        "unknown-field",
        "[package]\nname = \"app\"\nnot-a-field = true\n",
        &[("src/main.gg", "fn main() {}\n")],
    );
    assert!(matches!(
        Project::discover(&unknown),
        Err(ProjectError::InvalidManifest { .. })
    ));

    // 没有任何 gugu.toml 的已存在目录无法发现清单；不存在路径属于 Io 失败。
    let empty = root.path().join("nowhere");
    fs::create_dir(&empty).expect("create empty dir");
    assert!(matches!(
        Project::discover(&empty),
        Err(ProjectError::ManifestNotFound { .. })
    ));
    assert!(matches!(
        Project::discover(root.path().join("missing")),
        Err(ProjectError::Io { .. })
    ));
}
