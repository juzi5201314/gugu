#[path = "dependency_manifest.rs"]
mod dependency_manifest;
#[path = "dependency_model.rs"]
mod dependency_model;
#[path = "lock.rs"]
mod lock;
#[path = "resolver.rs"]
mod resolver;
#[path = "semver.rs"]
mod semver;

pub use dependency_model::*;
pub(crate) use resolver::resolve_project;
pub use semver::{Version, VersionReq};

// 逻辑路径在所有宿主上统一拒绝 Windows 盘符，不能依赖宿主的 Path 解析规则。
fn has_windows_drive_prefix(path: &str) -> bool {
    matches!(path.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic())
}

/// 全部 path package（workspace 成员 + 传递外部 path）的依赖解析元数据，
/// 与 `resolve_project` 使用同一份收集逻辑。
pub(crate) fn dependency_metadata(
    workspace_root: &std::path::Path,
    packages: &[crate::Package],
) -> Result<Vec<PackageMetadata>, crate::ProjectError> {
    let workspace_manifest = workspace_root.join("gugu.toml");
    let workspace_raw = crate::manifest::read_manifest(&workspace_manifest)?;
    let workspace_dependencies = workspace_raw
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.dependencies.clone())
        .unwrap_or_default();
    let (path_packages, _roots) =
        resolver::path_metadata_maps(workspace_root, packages, &workspace_dependencies)?;
    Ok(path_packages.into_values().collect())
}

#[cfg(test)]
#[path = "dependencies_tests.rs"]
mod tests;
