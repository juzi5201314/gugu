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

/// 各 workspace package 的依赖解析元数据（含合并 workspace.dependencies）。
pub(crate) fn workspace_package_metadata(
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
    packages
        .iter()
        .map(|package| {
            dependency_manifest::package_metadata(workspace_root, package, &workspace_dependencies)
        })
        .collect()
}

#[cfg(test)]
#[path = "dependencies_tests.rs"]
mod tests;
