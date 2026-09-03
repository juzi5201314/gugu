use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

mod error;
mod manifest;
mod model;
mod targets;
mod workspace;

#[cfg(test)]
mod support;

pub use error::ProjectError;
pub use model::{Package, Target, TargetKind, TargetSelection, Workspace};

/// 从当前路径发现的完整项目模型。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    workspace: Workspace,
    packages: Vec<Package>,
    current_package: Option<PathBuf>,
}

impl Project {
    /// 从目录或源码文件向父目录查找最近的 `gugu.toml` 并建立项目模型。
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, ProjectError> {
        let manifest_path = manifest::find_manifest(start.as_ref())?;
        let local = manifest::read_manifest(&manifest_path)?;
        let (workspace_manifest, workspace_raw) =
            manifest::find_workspace_manifest(&manifest_path, &local)?;
        let workspace_root = workspace_manifest
            .parent()
            .expect("manifest always has a parent")
            .to_path_buf();
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
        if let Some(current) = self.current_package() {
            return Ok(vec![current]);
        }
        if !self.workspace.default_members().is_empty() {
            return Ok(self
                .workspace
                .default_members()
                .iter()
                .filter_map(|root| self.packages.iter().find(|package| package.root() == root))
                .collect());
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
        let mut selected = Vec::new();
        for package in self.select_packages(requested_package, whole_workspace)? {
            selected.extend(
                package
                    .select_targets(selection)?
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
