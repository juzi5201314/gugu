use std::{fmt, path::PathBuf};

use super::error::ProjectError;

/// Gugu package 中的 target 种类。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TargetKind {
    /// 普通库 target。
    Lib,
    /// 可执行 bin target。
    Bin,
    /// 测试 target。
    Test,
    /// benchmark target。
    Bench,
    /// 示例可执行 target。
    Example,
    /// 只在 host 图执行的 build task。
    Build,
}

impl fmt::Display for TargetKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Lib => "lib",
            Self::Bin => "bin",
            Self::Test => "test",
            Self::Bench => "bench",
            Self::Example => "example",
            Self::Build => "build",
        };
        formatter.write_str(name)
    }
}

/// 自动发现 target 的选择器。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetSelection {
    /// 选择 package 的默认 build 集合：lib 和所有 bin。
    DefaultBuild,
    /// 选择 lib target。
    Lib,
    /// 选择一个指定名称的 bin target。
    Bin(Option<String>),
    /// 选择 test target，可选名称。
    Test(Option<String>),
    /// 选择 bench target，可选名称。
    Bench(Option<String>),
    /// 选择 example target，可选名称。
    Example(Option<String>),
    /// 选择所有用户 target。
    All,
}

/// 已发现且路径已校验的 target。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    kind: TargetKind,
    name: String,
    entry: PathBuf,
    source_root: PathBuf,
    required_features: Vec<String>,
    harness: bool,
}

impl Target {
    pub(crate) fn new(
        kind: TargetKind,
        name: String,
        entry: PathBuf,
        source_root: PathBuf,
        required_features: Vec<String>,
        harness: bool,
    ) -> Self {
        Self {
            kind,
            name,
            entry,
            source_root,
            required_features,
            harness,
        }
    }

    /// 返回 target 种类。
    pub fn kind(&self) -> TargetKind {
        self.kind
    }

    /// 返回 target 名称。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 返回已规范化的入口路径。
    pub fn entry(&self) -> &std::path::Path {
        &self.entry
    }

    /// 返回 target 的源码根路径。
    pub fn source_root(&self) -> &std::path::Path {
        &self.source_root
    }

    /// 返回 target 所需 feature。
    pub fn required_features(&self) -> &[String] {
        &self.required_features
    }

    /// 返回 bench 是否使用内建 harness。
    pub fn harness(&self) -> bool {
        self.harness
    }

    /// 判断该 target 的 required-features 是否全部被启用。
    pub fn features_enabled(&self, enabled_features: &[String]) -> bool {
        self.required_features
            .iter()
            .all(|feature| enabled_features.iter().any(|enabled| enabled == feature))
    }

    /// 判断该 target 是否只属于 host graph。
    pub fn is_host_target(&self) -> bool {
        self.kind == TargetKind::Build
    }
}

/// 已发现的 package。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Package {
    root: PathBuf,
    manifest: PathBuf,
    owner: Option<String>,
    name: String,
    version: String,
    declared_features: Vec<String>,
    targets: Vec<Target>,
}

impl Package {
    pub(crate) fn new(
        root: PathBuf,
        manifest: PathBuf,
        owner: Option<String>,
        name: String,
        version: String,
        declared_features: Vec<String>,
        targets: Vec<Target>,
    ) -> Self {
        Self {
            root,
            manifest,
            owner,
            name,
            version,
            declared_features,
            targets,
        }
    }

    /// 返回 package 根目录。
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// 返回清单路径。
    pub fn manifest(&self) -> &std::path::Path {
        &self.manifest
    }

    /// 返回可选 owner。
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// 返回 package 短名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 返回 package 版本；未填写时为 `0.0.0`。
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 返回规范 package 身份；没有 owner 时只有短名。
    pub fn package_name(&self) -> String {
        match &self.owner {
            Some(owner) => format!("{owner}/{}", self.name),
            None => self.name.clone(),
        }
    }

    /// 返回 package 的全部已发现 target。
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// 返回 package 声明的 feature 名（不含隐式默认）。
    pub fn declared_features(&self) -> &[String] {
        &self.declared_features
    }

    /// 按选择器返回 target。
    pub fn select_targets(
        &self,
        selection: &TargetSelection,
    ) -> Result<Vec<&Target>, ProjectError> {
        self.select_targets_in(selection, &[])
    }

    /// 按选择器与启用的 feature 返回 target。
    pub fn select_targets_in(
        &self,
        selection: &TargetSelection,
        enabled_features: &[String],
    ) -> Result<Vec<&Target>, ProjectError> {
        for feature in enabled_features {
            if !self
                .declared_features
                .iter()
                .any(|declared| declared == feature)
            {
                return Err(ProjectError::UnknownFeature {
                    package: self.package_name(),
                    feature: feature.clone(),
                });
            }
        }
        let targets = match selection {
            TargetSelection::DefaultBuild => self
                .targets
                .iter()
                .filter(|target| matches!(target.kind, TargetKind::Lib | TargetKind::Bin))
                .filter(|target| target.features_enabled(enabled_features))
                .collect(),
            TargetSelection::Lib => self.named_targets(TargetKind::Lib, None, enabled_features)?,
            TargetSelection::Bin(name) => {
                self.named_targets(TargetKind::Bin, name.as_deref(), enabled_features)?
            }
            TargetSelection::Test(name) => {
                self.named_targets(TargetKind::Test, name.as_deref(), enabled_features)?
            }
            TargetSelection::Bench(name) => {
                self.named_targets(TargetKind::Bench, name.as_deref(), enabled_features)?
            }
            TargetSelection::Example(name) => {
                self.named_targets(TargetKind::Example, name.as_deref(), enabled_features)?
            }
            TargetSelection::All => self
                .targets
                .iter()
                .filter(|target| target.kind != TargetKind::Build)
                .filter(|target| target.features_enabled(enabled_features))
                .collect(),
        };
        Ok(targets)
    }

    fn named_targets(
        &self,
        kind: TargetKind,
        name: Option<&str>,
        enabled_features: &[String],
    ) -> Result<Vec<&Target>, ProjectError> {
        let targets = self
            .targets
            .iter()
            .filter(|target| target.kind == kind && name.is_none_or(|name| target.name == name))
            .filter(|target| target.features_enabled(enabled_features))
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return Err(ProjectError::TargetSelection {
                package: self.package_name(),
                kind,
                name: name.map(str::to_owned),
            });
        }
        Ok(targets)
    }
}

/// workspace 根和其 package 集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    root: PathBuf,
    default_members: Vec<PathBuf>,
}

impl Workspace {
    pub(crate) fn new(root: PathBuf, default_members: Vec<PathBuf>) -> Self {
        Self {
            root,
            default_members,
        }
    }

    /// 返回 workspace 根目录。
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// 返回默认选择的 package 根目录。
    pub fn default_members(&self) -> &[PathBuf] {
        &self.default_members
    }
}
