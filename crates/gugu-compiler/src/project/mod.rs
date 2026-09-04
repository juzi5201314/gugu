use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

mod cache;
mod dependencies;
mod error;
mod manifest;
mod model;
mod targets;
mod workspace;

#[cfg(test)]
mod support;

pub use cache::candidates_from_lock;
pub use cache::{
    ActionInputs, ActionKey, CacheError, CachePolicy, DependencyCache, DependencyInput,
    PackageFiles, TargetArtifact, TargetView, default_cache_root, materialize_vendor,
    prepare_dependency_inputs,
};
pub use dependencies::{
    DependencyDomain, DependencySource, DependencySpec, LockGraph, LockedDependency, LockedPackage,
    PackageId, PackageMetadata, PackageSource, ResolveOptions, TargetCondition, Version,
    VersionReq,
};
pub use error::ProjectError;
pub use model::{Package, Target, TargetKind, TargetSelection, Workspace};

/// 从当前路径发现的完整项目模型。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    workspace: Workspace,
    packages: Vec<Package>,
    current_package: Option<PathBuf>,
    started_at_root: bool,
}

impl Project {
    /// 从目录或源码文件向父目录查找最近的 `gugu.toml` 并建立项目模型。
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, ProjectError> {
        let start = fs::canonicalize(start.as_ref()).map_err(|error| ProjectError::Io {
            path: start.as_ref().to_path_buf(),
            message: error.to_string(),
        })?;
        let start = if start.is_file() {
            start.parent().unwrap_or(&start).to_path_buf()
        } else {
            start
        };
        let manifest_path = manifest::find_manifest(&start)?;
        let local = manifest::read_manifest(&manifest_path)?;
        let (workspace_manifest, workspace_raw) =
            manifest::find_workspace_manifest(&manifest_path, &local)?;
        let workspace_root = workspace_manifest
            .parent()
            .expect("manifest always has a parent")
            .to_path_buf();
        let started_at_root = start == workspace_root;
        let member_manifests = workspace_raw
            .workspace
            .as_ref()
            .map(|raw| workspace::discover_workspace_members(&workspace_root, raw))
            .transpose()?
            .unwrap_or_default();

        let mut manifest_paths = BTreeSet::new();
        if local.package.is_some() && manifest_path == workspace_manifest {
            manifest_paths.insert(manifest_path.clone());
        }
        manifest_paths.extend(member_manifests);
        // workspace 根清单同时是 package 时，无论从根还是成员目录启动，
        // 都把它作为 workspace 的保留 package 纳入模型（根 package 本身也是成员）。
        if workspace_raw.package.is_some() {
            manifest_paths.insert(workspace_manifest.clone());
        }
        if manifest_path != workspace_manifest
            && local.package.is_some()
            && !manifest_paths.contains(&manifest_path)
        {
            return Err(ProjectError::WorkspaceMember {
                path: manifest_path,
                workspace: workspace_root,
            });
        }
        if workspace_raw.workspace.is_none() {
            if local.package.is_none() {
                return Err(ProjectError::InvalidManifest {
                    path: workspace_manifest,
                    message: "清单必须包含 [package] 或 [workspace]".to_owned(),
                });
            }
            manifest_paths.insert(manifest_path.clone());
        }

        let mut packages = manifest_paths
            .into_iter()
            .map(|path| manifest::build_package(&path))
            .collect::<Result<Vec<_>, _>>()?;
        packages.sort_by(|left, right| {
            workspace::relative_path(&workspace_root, left.root())
                .cmp(&workspace::relative_path(&workspace_root, right.root()))
        });
        let default_members =
            workspace::resolve_default_members(&workspace_root, &workspace_raw, &packages)?;
        let current_package = packages
            .iter()
            .find(|package| package.manifest() == manifest_path)
            .map(|package| package.root().to_path_buf());
        if manifest_path != workspace_manifest && current_package.is_none() {
            return Err(ProjectError::WorkspaceMember {
                path: manifest_path,
                workspace: workspace_root,
            });
        }
        Ok(Self {
            workspace: Workspace::new(workspace_root, default_members),
            packages,
            current_package,
            started_at_root,
        })
    }

    /// 返回 workspace 信息。
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// 返回按 workspace 相对路径排序的 package。
    pub fn packages(&self) -> &[Package] {
        &self.packages
    }

    /// 按 target、host、feature 和 source index 解析 workspace 依赖。
    pub fn resolve_dependencies(&self, options: ResolveOptions) -> Result<LockGraph, ProjectError> {
        dependencies::resolve_project(self.workspace.root(), &self.packages, options)
    }

    /// 返回 workspace 根锁文件路径。
    pub fn lock_path(&self) -> PathBuf {
        self.workspace.root().join("gugu.lock")
    }
    /// 返回从起始目录找到的当前 package（虚拟 workspace 根没有当前 package）。
    pub fn current_package(&self) -> Option<&Package> {
        self.current_package
            .as_ref()
            .and_then(|root| self.packages.iter().find(|package| package.root() == root))
    }

    /// 按 package 选择规则返回要构建的 package。
    pub fn select_packages(
        &self,
        requested: Option<&str>,
        whole_workspace: bool,
    ) -> Result<Vec<&Package>, ProjectError> {
        if let Some(requested) = requested {
            let matches = self
                .packages
                .iter()
                .filter(|package| {
                    package.package_name() == requested || package.name() == requested
                })
                .collect::<Vec<_>>();
            return match matches.as_slice() {
                [package] => Ok(vec![*package]),
                [] => Err(ProjectError::PackageSelection {
                    requested: requested.to_owned(),
                }),
                _ => Err(ProjectError::AmbiguousPackage {
                    requested: requested.to_owned(),
                }),
            };
        }
        if whole_workspace {
            return Ok(self.packages.iter().collect());
        }
        // 成员目录启动：只构建当前 package，不使用 default-members。
        if !self.started_at_root {
            if let Some(current) = self.current_package() {
                return Ok(vec![current]);
            }
            return Ok(self.packages.iter().collect());
        }
        // workspace 根启动：依次使用 default-members、根 package 或全部成员。
        if !self.workspace.default_members().is_empty() {
            return Ok(self
                .workspace
                .default_members()
                .iter()
                .filter_map(|root| self.packages.iter().find(|package| package.root() == root))
                .collect());
        }
        if let Some(root_package) = self
            .packages
            .iter()
            .find(|package| package.root() == self.workspace.root())
        {
            return Ok(vec![root_package]);
        }
        Ok(self.packages.iter().collect())
    }

    /// 按 package 与 target 选择器返回可编译 target。
    pub fn select_targets(
        &self,
        requested_package: Option<&str>,
        whole_workspace: bool,
        selection: &TargetSelection,
    ) -> Result<Vec<(&Package, &Target)>, ProjectError> {
        self.select_targets_in(requested_package, whole_workspace, selection, &[])
    }

    /// 按 package、target 选择器与启用的 feature 返回可编译 target。
    pub fn select_targets_in(
        &self,
        requested_package: Option<&str>,
        whole_workspace: bool,
        selection: &TargetSelection,
        enabled_features: &[String],
    ) -> Result<Vec<(&Package, &Target)>, ProjectError> {
        let mut selected = Vec::new();
        for package in self.select_packages(requested_package, whole_workspace)? {
            selected.extend(
                package
                    .select_targets_in(selection, enabled_features)?
                    .into_iter()
                    .map(|target| (package, target)),
            );
        }
        Ok(selected)
    }
}

pub(crate) fn sorted_entries(
    directory: &Path,
    manifest: &Path,
) -> Result<Vec<fs::DirEntry>, ProjectError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| ProjectError::Io {
            path: directory.to_path_buf(),
            message: error.to_string(),
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ProjectError::Io {
            path: manifest.to_path_buf(),
            message: error.to_string(),
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}
