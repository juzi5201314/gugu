use std::{fs, path::PathBuf};

use tempfile::TempDir;

use super::model::{Package, TargetKind};

/// 在临时目录里落一个最小 package，并返回其根目录。
pub(super) fn package(
    root: &TempDir,
    directory: &str,
    manifest: &str,
    files: &[(&str, &str)],
) -> PathBuf {
    let package_root = root.path().join(directory);
    for (path, contents) in files {
        let file = package_root.join(path);
        fs::create_dir_all(file.parent().expect("parent exists")).expect("create dir");
        fs::write(&file, contents).expect("write file");
    }
    fs::create_dir_all(&package_root).expect("create package root");
    fs::write(package_root.join("gugu.toml"), manifest).expect("write manifest");
    package_root
}

pub(super) fn target_summary(package: &Package) -> Vec<(TargetKind, String)> {
    package
        .targets()
        .iter()
        .map(|target| (target.kind(), target.name().to_owned()))
        .collect()
}
