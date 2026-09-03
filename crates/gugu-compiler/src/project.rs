use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Component, Path, PathBuf},
};

use serde::Deserialize;

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
    /// 返回 target 种类。
    pub fn kind(&self) -> TargetKind {
        self.kind
    }

    /// 返回 target 名称。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 返回已规范化的入口路径。
    pub fn entry(&self) -> &Path {
        &self.entry
    }

    /// 返回 target 的源码根路径。
    pub fn source_root(&self) -> &Path {
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
    targets: Vec<Target>,
}

impl Package {
    /// 返回 package 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 返回清单路径。
    pub fn manifest(&self) -> &Path {
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

    /// 按选择器返回 target。
    pub fn select_targets(
        &self,
        selection: &TargetSelection,
    ) -> Result<Vec<&Target>, ProjectError> {
        let targets = match selection {
            TargetSelection::DefaultBuild => self
                .targets
                .iter()
                .filter(|target| matches!(target.kind, TargetKind::Lib | TargetKind::Bin))
                .collect(),
            TargetSelection::Lib => self.named_targets(TargetKind::Lib, None)?,
            TargetSelection::Bin(name) => self.named_targets(TargetKind::Bin, name.as_deref())?,
            TargetSelection::Test(name) => self.named_targets(TargetKind::Test, name.as_deref())?,
            TargetSelection::Bench(name) => {
                self.named_targets(TargetKind::Bench, name.as_deref())?
            }
            TargetSelection::Example(name) => {
                self.named_targets(TargetKind::Example, name.as_deref())?
            }
            TargetSelection::All => self
                .targets
                .iter()
                .filter(|target| target.kind != TargetKind::Build)
                .collect(),
        };
        Ok(targets)
    }

    fn named_targets(
        &self,
        kind: TargetKind,
        name: Option<&str>,
    ) -> Result<Vec<&Target>, ProjectError> {
        let targets = self
            .targets
            .iter()
            .filter(|target| target.kind == kind && name.is_none_or(|name| target.name == name))
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
    /// 返回 workspace 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 返回默认选择的 package 根目录。
    pub fn default_members(&self) -> &[PathBuf] {
        &self.default_members
    }
}

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
        let manifest = find_manifest(start.as_ref())?;
        let local = read_manifest(&manifest)?;
        let (workspace_manifest, workspace_raw) = find_workspace_manifest(&manifest, &local)?;
        let workspace_root = workspace_manifest
            .parent()
            .expect("manifest always has a parent")
            .to_path_buf();
        let member_manifests = if let Some(raw) = workspace_raw.workspace.as_ref() {
            discover_workspace_members(&workspace_root, raw)?
        } else {
            Vec::new()
        };

        let mut manifest_paths = BTreeSet::new();
        if local.package.is_some() && manifest == workspace_manifest {
            manifest_paths.insert(manifest.clone());
        }
        for member in member_manifests {
            manifest_paths.insert(member);
        }
        if manifest != workspace_manifest
            && local.package.is_some()
            && !manifest_paths.contains(&manifest)
        {
            return Err(ProjectError::WorkspaceMember {
                path: manifest,
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
            manifest_paths.insert(manifest.clone());
        }

        let mut packages = manifest_paths
            .into_iter()
            .map(|path| build_package(&path))
            .collect::<Result<Vec<_>, _>>()?;
        packages.sort_by(|left, right| {
            relative_path(&workspace_root, &left.root)
                .cmp(&relative_path(&workspace_root, &right.root))
        });
        let default_members = resolve_default_members(&workspace_root, &workspace_raw, &packages)?;
        let current_package = packages
            .iter()
            .find(|package| package.manifest == manifest)
            .map(|package| package.root.clone());
        if manifest != workspace_manifest && current_package.is_none() {
            return Err(ProjectError::WorkspaceMember {
                path: manifest,
                workspace: workspace_root,
            });
        }
        Ok(Self {
            workspace: Workspace {
                root: workspace_root,
                default_members,
            },
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
            .and_then(|root| self.packages.iter().find(|package| &package.root == root))
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
                .filter(|package| package.package_name() == requested || package.name == requested)
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
        // 从成员目录启动时构建当前 package；default-members 只在从 workspace
        // 根（或任何非成员目录）启动时生效。
        if let Some(current) = self.current_package() {
            return Ok(vec![current]);
        }
        if !self.workspace.default_members.is_empty() {
            return Ok(self
                .workspace
                .default_members
                .iter()
                .filter_map(|root| self.packages.iter().find(|package| &package.root == root))
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

/// 项目清单、workspace 或 target 发现失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectError {
    /// 未发现清单。
    ManifestNotFound {
        /// 查找起点。
        start: PathBuf,
    },
    /// 文件系统操作失败。
    Io {
        /// 触发失败的路径。
        path: PathBuf,
        /// 底层错误消息。
        message: String,
    },
    /// TOML 或清单核心字段无效。
    InvalidManifest {
        /// 清单路径。
        path: PathBuf,
        /// 无效原因。
        message: String,
    },
    /// workspace 成员不满足根目录约束。
    WorkspaceMember {
        /// 成员路径。
        path: PathBuf,
        /// workspace 根目录。
        workspace: PathBuf,
    },
    /// 自动 target 或入口无效。
    TargetDiscovery {
        /// package 清单路径。
        package: PathBuf,
        /// 无效原因。
        message: String,
    },
    /// package 选择不存在。
    PackageSelection {
        /// 请求的 package 名称。
        requested: String,
    },
    /// package 短名存在歧义。
    AmbiguousPackage {
        /// 请求的 package 名称。
        requested: String,
    },
    /// target 选择不存在。
    TargetSelection {
        /// 所属 package。
        package: String,
        /// 请求的 target 种类。
        kind: TargetKind,
        /// 请求的 target 名称。
        name: Option<String>,
    },
}

impl fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManifestNotFound { start } => {
                write!(
                    formatter,
                    "从 `{}` 向父目录未找到 gugu.toml",
                    start.display()
                )
            }
            Self::Io { path, message } => {
                write!(formatter, "无法读取 `{}`：{message}", path.display())
            }
            Self::InvalidManifest { path, message } => {
                write!(formatter, "清单 `{}` 无效：{message}", path.display())
            }
            Self::WorkspaceMember { path, workspace } => write!(
                formatter,
                "workspace `{}` 的成员 `{}` 不合法",
                workspace.display(),
                path.display()
            ),
            Self::TargetDiscovery { package, message } => {
                write!(
                    formatter,
                    "package `{}` 的 target 无效：{message}",
                    package.display()
                )
            }
            Self::PackageSelection { requested } => {
                write!(formatter, "未找到 package `{requested}`")
            }
            Self::AmbiguousPackage { requested } => {
                write!(formatter, "package `{requested}` 有歧义，请使用 owner/name")
            }
            Self::TargetSelection {
                package,
                kind,
                name,
            } => match name {
                Some(name) => write!(
                    formatter,
                    "package `{package}` 没有名为 `{name}` 的 {kind} target"
                ),
                None => write!(formatter, "package `{package}` 没有 {kind} target"),
            },
        }
    }
}

impl std::error::Error for ProjectError {}

// 清单 schema 的完整字段面：依赖解析（阶段 05）、feature、patch、build task
// 和 metadata 由后续阶段消费；本阶段先以 serde 结构保证未知核心字段被拒绝。
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawManifest {
    package: Option<RawPackage>,
    workspace: Option<RawWorkspace>,
    lib: Option<RawLib>,
    bin: Vec<RawTarget>,
    test: Vec<RawTarget>,
    bench: Vec<RawTarget>,
    example: Vec<RawTarget>,
    dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "test-dependencies")]
    test_dependencies: Option<BTreeMap<String, toml::Value>>,
    build: Option<toml::Value>,
    target: Option<toml::Value>,
    features: Option<BTreeMap<String, Vec<String>>>,
    patch: Option<toml::Value>,
    metadata: Option<BTreeMap<String, toml::Value>>,
}

// 发布元数据字段（description/license/repository 等）由 `gugu package`/`publish`
// 在阶段 74 消费；本阶段只做 schema 接受与校验。
#[allow(dead_code)]
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawPackage {
    owner: Option<String>,
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    license: Option<String>,
    repository: Option<String>,
    homepage: Option<String>,
    documentation: Option<String>,
    readme: Option<String>,
    authors: Vec<String>,
    keywords: Vec<String>,
    categories: Vec<String>,
    publish: Option<bool>,
    #[serde(rename = "default-run")]
    default_run: Option<String>,
    include: Vec<String>,
    exclude: Vec<String>,
    #[serde(rename = "auto-lib")]
    auto_lib: Option<bool>,
    #[serde(rename = "auto-bins")]
    auto_bins: Option<bool>,
    #[serde(rename = "auto-tests")]
    auto_tests: Option<bool>,
    #[serde(rename = "auto-benches")]
    auto_benches: Option<bool>,
    #[serde(rename = "auto-examples")]
    auto_examples: Option<bool>,
    #[serde(rename = "auto-build")]
    auto_build: Option<bool>,
    metadata: Option<BTreeMap<String, toml::Value>>,
}

// workspace.package 继承、workspace.dependencies 与 workspace.lints 由阶段 05 消费。
#[allow(dead_code)]
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawWorkspace {
    members: Vec<String>,
    exclude: Vec<String>,
    #[serde(rename = "default-members")]
    default_members: Vec<String>,
    package: Option<toml::Value>,
    dependencies: Option<BTreeMap<String, toml::Value>>,
    lints: Option<BTreeMap<String, toml::Value>>,
}

// lib artifacts 由 C 导出产物（staticlib/cdylib） writer 在后端阶段消费。
#[allow(dead_code)]
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawLib {
    path: Option<PathBuf>,
    artifacts: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawTarget {
    name: Option<String>,
    path: Option<PathBuf>,
    #[serde(rename = "required-features")]
    required_features: Vec<String>,
    harness: Option<bool>,
}
fn find_manifest(start: &Path) -> Result<PathBuf, ProjectError> {
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

fn find_workspace_manifest(
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

fn read_manifest(path: &Path) -> Result<RawManifest, ProjectError> {
    let source = fs::read_to_string(path).map_err(|error| ProjectError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    toml::from_str(&source).map_err(|error| ProjectError::InvalidManifest {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn discover_workspace_members(
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
        excluded.extend(expand_directory_pattern(root, pattern)?);
    }
    members.retain(|manifest| {
        manifest
            .parent()
            .is_some_and(|directory| !excluded.contains(directory))
    });
    Ok(members.into_iter().collect())
}

fn resolve_default_members(
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
            if !packages.iter().any(|package| package.root == directory) {
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
fn build_package(manifest: &Path) -> Result<Package, ProjectError> {
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
    Ok(Package {
        root,
        manifest,
        owner: package.owner.clone(),
        name,
        version: package
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_owned()),
        targets,
    })
}

fn discover_targets(
    root: &Path,
    package: &RawPackage,
    raw: &RawManifest,
    manifest: &Path,
) -> Result<Vec<Target>, ProjectError> {
    let mut targets = Vec::new();
    // 规范：lib 的默认名是 package 短名把 `-` 换成 `_`。
    let lib_name = package.name.as_deref().unwrap_or("lib").replace('-', "_");
    if let Some(lib) = raw.lib.as_ref() {
        targets.push(explicit_target(
            root,
            TargetKind::Lib,
            lib.path.clone(),
            Some(lib_name),
            Vec::new(),
            true,
            manifest,
        )?);
    } else if package.auto_lib.unwrap_or(true) {
        add_auto_file(
            &mut targets,
            root,
            TargetKind::Lib,
            AutoFile {
                name: &lib_name,
                path: Path::new("src/lib.gg"),
                required_features: Vec::new(),
                harness: true,
            },
            manifest,
        )?;
    }
    if package.auto_bins.unwrap_or(true) {
        add_auto_file(
            &mut targets,
            root,
            TargetKind::Bin,
            AutoFile {
                name: package.name.as_deref().unwrap_or("main"),
                path: Path::new("src/main.gg"),
                required_features: Vec::new(),
                harness: true,
            },
            manifest,
        )?;
        discover_directory_targets(
            &mut targets,
            root,
            TargetKind::Bin,
            Path::new("src/bin"),
            true,
            manifest,
        )?;
    }
    add_explicit_targets(
        &mut targets,
        root,
        TargetKind::Bin,
        &raw.bin,
        true,
        manifest,
    )?;
    if package.auto_tests.unwrap_or(true) {
        discover_directory_targets(
            &mut targets,
            root,
            TargetKind::Test,
            Path::new("tests"),
            true,
            manifest,
        )?;
    }
    add_explicit_targets(
        &mut targets,
        root,
        TargetKind::Test,
        &raw.test,
        true,
        manifest,
    )?;
    if package.auto_benches.unwrap_or(true) {
        discover_directory_targets(
            &mut targets,
            root,
            TargetKind::Bench,
            Path::new("benches"),
            true,
            manifest,
        )?;
    }
    add_explicit_targets(
        &mut targets,
        root,
        TargetKind::Bench,
        &raw.bench,
        true,
        manifest,
    )?;
    if package.auto_examples.unwrap_or(true) {
        discover_directory_targets(
            &mut targets,
            root,
            TargetKind::Example,
            Path::new("examples"),
            true,
            manifest,
        )?;
    }
    add_explicit_targets(
        &mut targets,
        root,
        TargetKind::Example,
        &raw.example,
        true,
        manifest,
    )?;
    if package.auto_build.unwrap_or(true) {
        add_auto_file(
            &mut targets,
            root,
            TargetKind::Build,
            AutoFile {
                name: "build",
                path: Path::new("build.gg"),
                required_features: Vec::new(),
                harness: true,
            },
            manifest,
        )?;
    }
    targets.sort_by(|left, right| {
        (left.kind, left.name.as_str(), &left.entry).cmp(&(
            right.kind,
            right.name.as_str(),
            &right.entry,
        ))
    });
    check_target_duplicates(&targets, manifest)?;
    let mut source_roots = BTreeSet::new();
    for target in &targets {
        source_roots.insert(target.source_root.clone());
    }
    for source_root in source_roots {
        check_module_conflicts(&source_root, manifest)?;
    }
    Ok(targets)
}

fn add_explicit_targets(
    targets: &mut Vec<Target>,
    root: &Path,
    kind: TargetKind,
    definitions: &[RawTarget],
    default_harness: bool,
    manifest: &Path,
) -> Result<(), ProjectError> {
    for definition in definitions {
        let default_name = definition
            .path
            .as_deref()
            .and_then(Path::file_stem)
            .and_then(|value| value.to_str())
            .unwrap_or("target");
        let name = definition
            .name
            .clone()
            .unwrap_or_else(|| default_name.to_owned());
        validate_target_name(&name, manifest)?;
        targets.push(explicit_target(
            root,
            kind,
            definition.path.clone(),
            Some(name),
            definition.required_features.clone(),
            definition.harness.unwrap_or(default_harness),
            manifest,
        )?);
    }
    Ok(())
}

fn explicit_target(
    root: &Path,
    kind: TargetKind,
    path: Option<PathBuf>,
    name: Option<String>,
    required_features: Vec<String>,
    harness: bool,
    manifest: &Path,
) -> Result<Target, ProjectError> {
    let path = path.unwrap_or_else(|| default_target_path(kind, name.as_deref()));
    let entry = resolve_entry(root, &path, manifest)?;
    let name = name.unwrap_or_else(|| default_target_name(&entry, root));
    validate_target_name(&name, manifest)?;
    Ok(Target {
        kind,
        name,
        source_root: source_root(root, &entry, kind, manifest)?,
        entry,
        required_features: normalize_features(required_features),
        harness,
    })
}

/// 自动发现的 target 入口规格。
struct AutoFile<'a> {
    name: &'a str,
    path: &'a Path,
    required_features: Vec<String>,
    harness: bool,
}

fn add_auto_file(
    targets: &mut Vec<Target>,
    root: &Path,
    kind: TargetKind,
    file: AutoFile<'_>,
    manifest: &Path,
) -> Result<(), ProjectError> {
    if !root.join(file.path).is_file() {
        return Ok(());
    }
    targets.push(explicit_target(
        root,
        kind,
        Some(file.path.to_path_buf()),
        Some(file.name.to_owned()),
        file.required_features,
        file.harness,
        manifest,
    )?);
    Ok(())
}

fn discover_directory_targets(
    targets: &mut Vec<Target>,
    root: &Path,
    kind: TargetKind,
    directory: &Path,
    harness: bool,
    manifest: &Path,
) -> Result<(), ProjectError> {
    let directory = root.join(directory);
    if !directory.is_dir() {
        return Ok(());
    }
    let entries = sorted_entries(&directory, manifest)?;
    let mut names = BTreeSet::new();
    for entry in entries {
        let file_type = entry.file_type().map_err(|error| ProjectError::Io {
            path: entry.path(),
            message: error.to_string(),
        })?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            return Err(ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: "target 名必须是 UTF-8".to_owned(),
            });
        };
        let (name, path) = if file_type.is_file() && file_name.ends_with(".gg") {
            let name = file_name.trim_end_matches(".gg");
            (name.to_owned(), entry.path())
        } else if file_type.is_dir() {
            let main = entry.path().join("main.gg");
            if !main.is_file() {
                continue;
            }
            (file_name.to_owned(), main)
        } else {
            continue;
        };
        validate_target_name(&name, manifest)?;
        if !names.insert(name.clone()) {
            return Err(ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: format!("{kind} target `{name}` 同时存在文件和目录入口"),
            });
        }
        targets.push(explicit_target(
            root,
            kind,
            Some(
                path.strip_prefix(root)
                    .expect("entry is under root")
                    .to_path_buf(),
            ),
            Some(name),
            Vec::new(),
            harness,
            manifest,
        )?);
    }
    Ok(())
}

fn resolve_entry(root: &Path, path: &Path, manifest: &Path) -> Result<PathBuf, ProjectError> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || path.to_string_lossy().contains('\\')
    {
        return Err(ProjectError::TargetDiscovery {
            package: manifest.to_path_buf(),
            message: format!("target 入口 `{}` 不是 package 内相对路径", path.display()),
        });
    }
    let candidate = root.join(path);
    let entry = fs::canonicalize(&candidate).map_err(|error| ProjectError::TargetDiscovery {
        package: manifest.to_path_buf(),
        message: format!("无法读取 target 入口 `{}`：{error}", path.display()),
    })?;
    if !entry.starts_with(root) || !entry.is_file() {
        return Err(ProjectError::TargetDiscovery {
            package: manifest.to_path_buf(),
            message: format!("target 入口 `{}` 不在 package 内或不是文件", path.display()),
        });
    }
    Ok(entry)
}

fn source_root(
    root: &Path,
    entry: &Path,
    kind: TargetKind,
    manifest: &Path,
) -> Result<PathBuf, ProjectError> {
    if kind == TargetKind::Build {
        return Ok(root.to_path_buf());
    }
    let relative = entry
        .strip_prefix(root)
        .map_err(|_| ProjectError::TargetDiscovery {
            package: manifest.to_path_buf(),
            message: format!("入口 `{}` 越过 package 根", entry.display()),
        })?;
    let Some(first) = relative.components().next() else {
        return Err(ProjectError::TargetDiscovery {
            package: manifest.to_path_buf(),
            message: "target 入口缺少源码根".to_owned(),
        });
    };
    let source_root = root.join(first.as_os_str());
    fs::canonicalize(&source_root).map_err(|error| ProjectError::TargetDiscovery {
        package: manifest.to_path_buf(),
        message: format!(
            "无法读取 target 源码根 `{}`：{error}",
            source_root.display()
        ),
    })
}

fn check_target_duplicates(targets: &[Target], manifest: &Path) -> Result<(), ProjectError> {
    for pair in targets.windows(2) {
        if pair[0].kind == pair[1].kind && pair[0].name == pair[1].name {
            return Err(ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: format!("{} target `{}` 重名", pair[0].kind, pair[0].name),
            });
        }
    }
    Ok(())
}

fn check_module_conflicts(root: &Path, manifest: &Path) -> Result<(), ProjectError> {
    let mut files = BTreeSet::new();
    collect_source_files(root, root, &mut files, manifest)?;
    for file in &files {
        let Some(name) = file.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".gg") || name == "mod.gg" {
            continue;
        }
        let stem = name.trim_end_matches(".gg");
        let directory_form = file.with_file_name(stem).join("mod.gg");
        if files.contains(&directory_form) {
            return Err(ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: format!(
                    "模块 `{}` 与 `{}` 同时存在",
                    file.display(),
                    directory_form.display()
                ),
            });
        }
    }
    Ok(())
}

fn collect_source_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<PathBuf>,
    manifest: &Path,
) -> Result<(), ProjectError> {
    for entry in sorted_entries(directory, manifest)? {
        let file_type = entry.file_type().map_err(|error| ProjectError::Io {
            path: entry.path(),
            message: error.to_string(),
        })?;
        if file_type.is_symlink() {
            return Err(ProjectError::TargetDiscovery {
                package: manifest.to_path_buf(),
                message: format!("源码树不允许符号链接 `{}`", entry.path().display()),
            });
        }
        if file_type.is_dir() {
            collect_source_files(root, &entry.path(), files, manifest)?;
        } else if file_type.is_file() && entry.path().extension().is_some_and(|ext| ext == "gg") {
            files.insert(entry.path());
        }
    }
    let _ = root;
    Ok(())
}

fn sorted_entries(directory: &Path, manifest: &Path) -> Result<Vec<fs::DirEntry>, ProjectError> {
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

fn expand_directory_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, ProjectError> {
    let components = split_pattern(pattern).ok_or_else(|| ProjectError::WorkspaceMember {
        path: root.join(pattern),
        workspace: root.to_path_buf(),
    })?;
    let mut directories = Vec::new();
    collect_directories(root, root, &mut directories)?;
    let mut matches = directories
        .into_iter()
        .filter(|directory| {
            let relative = directory
                .strip_prefix(root)
                .expect("directory is under root")
                .components()
                .filter_map(|component| component.as_os_str().to_str())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            matches_pattern(&components, &relative)
        })
        .collect::<Vec<_>>();
    matches.sort();
    if matches.is_empty() {
        return Err(ProjectError::WorkspaceMember {
            path: root.join(pattern),
            workspace: root.to_path_buf(),
        });
    }
    Ok(matches)
}

fn collect_directories(
    root: &Path,
    directory: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<(), ProjectError> {
    output.push(directory.to_path_buf());
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
    for entry in entries {
        let file_type = entry.file_type().map_err(|error| ProjectError::Io {
            path: entry.path(),
            message: error.to_string(),
        })?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let path = entry.path();
            if path.starts_with(root) {
                collect_directories(root, &path, output)?;
            }
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

fn matches_pattern(pattern: &[String], path: &[String]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == "**" {
        return matches_pattern(&pattern[1..], path)
            || (!path.is_empty() && matches_pattern(pattern, &path[1..]));
    }
    !path.is_empty()
        && wildcard_component(&pattern[0], &path[0])
        && matches_pattern(&pattern[1..], &path[1..])
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

fn default_target_path(kind: TargetKind, name: Option<&str>) -> PathBuf {
    match kind {
        TargetKind::Lib => PathBuf::from("src/lib.gg"),
        TargetKind::Bin => name.map_or_else(
            || PathBuf::from("src/main.gg"),
            |name| PathBuf::from(format!("src/bin/{name}.gg")),
        ),
        TargetKind::Test => PathBuf::from(format!("tests/{}.gg", name.unwrap_or("test"))),
        TargetKind::Bench => PathBuf::from(format!("benches/{}.gg", name.unwrap_or("bench"))),
        TargetKind::Example => PathBuf::from(format!("examples/{}.gg", name.unwrap_or("example"))),
        TargetKind::Build => PathBuf::from("build.gg"),
    }
}

fn default_target_name(entry: &Path, root: &Path) -> String {
    entry
        .strip_prefix(root)
        .ok()
        .and_then(|path| path.file_stem())
        .and_then(|value| value.to_str())
        .unwrap_or("target")
        .to_owned()
}

fn normalize_features(mut features: Vec<String>) -> Vec<String> {
    features.sort();
    features.dedup();
    features
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

fn validate_target_name(name: &str, manifest: &Path) -> Result<(), ProjectError> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(ProjectError::TargetDiscovery {
            package: manifest.to_path_buf(),
            message: format!("target 名 `{name}` 不是合法名称"),
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

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    /// 在临时目录里落一个最小 package，并返回其根目录。
    fn package(root: &TempDir, directory: &str, manifest: &str, files: &[(&str, &str)]) -> PathBuf {
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

    fn target_summary(package: &Package) -> Vec<(TargetKind, String)> {
        package
            .targets()
            .iter()
            .map(|target| (target.kind(), target.name().to_owned()))
            .collect()
    }

    #[test]
    fn discovers_single_package_targets() {
        let root = TempDir::new().expect("tempdir");
        let package = package(
            &root,
            "demo",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            &[
                ("src/main.gg", "fn main() {}\n"),
                ("src/lib.gg", "fn util() {}\n"),
                ("src/bin/extra.gg", "fn main() {}\n"),
                ("src/bin/nested/main.gg", "fn main() {}\n"),
                ("tests/basic.gg", "fn checks() {}\n"),
                ("benches/perf.gg", "fn bench() {}\n"),
                ("examples/hello.gg", "fn main() {}\n"),
                ("build.gg", "fn main() {}\n"),
            ],
        );

        let project = Project::discover(&package).expect("project discovers");
        assert_eq!(project.workspace().root(), &package);
        let current = project.current_package().expect("current package");
        assert_eq!(current.name(), "demo");
        assert_eq!(
            target_summary(current),
            vec![
                (TargetKind::Lib, "demo".into()),
                (TargetKind::Bin, "demo".into()),
                (TargetKind::Bin, "extra".into()),
                (TargetKind::Bin, "nested".into()),
                (TargetKind::Test, "basic".into()),
                (TargetKind::Bench, "perf".into()),
                (TargetKind::Example, "hello".into()),
                (TargetKind::Build, "build".into()),
            ]
        );

        // src/bin/nested/main.gg 的 source root 是 src，与默认 target 共享。
        assert!(
            current
                .targets()
                .iter()
                .any(|target| target.name() == "nested"
                    && target.source_root() == package.join("src"))
        );

        // 默认构建集合是 lib + 所有 bin。
        let default = current
            .select_targets(&TargetSelection::DefaultBuild)
            .expect("default targets");
        assert_eq!(default.len(), 4);
        assert!(
            default
                .iter()
                .all(|target| { matches!(target.kind(), TargetKind::Lib | TargetKind::Bin) })
        );
    }

    #[test]
    fn virtual_workspace_resolves_members_globs_and_defaults() {
        let root = TempDir::new().expect("tempdir");
        fs::write(
            root.path().join("gugu.toml"),
            "[workspace]\nmembers = [\"packages/*\", \"tools/*\"]\nexclude = \
             [\"packages/legacy\"]\ndefault-members = [\"packages/app\"]\n",
        )
        .expect("write workspace manifest");
        let app = package(
            &root,
            "packages/app",
            "[package]\nname = \"app\"\n",
            &[("src/main.gg", "fn main() {}\n")],
        );
        let lib = package(
            &root,
            "packages/lib",
            "[package]\nname = \"lib\"\n",
            &[("src/lib.gg", "fn util() {}\n")],
        );
        package(
            &root,
            "packages/legacy",
            "[package]\nname = \"legacy\"\n",
            &[("src/lib.gg", "fn old() {}\n")],
        );
        let codegen = package(
            &root,
            "tools/codegen",
            "[package]\nname = \"codegen\"\n",
            &[("src/lib.gg", "fn gen() {}\n")],
        );

        // 从 workspace 根构建：default-members 只选 app。
        let project = Project::discover(root.path()).expect("workspace discovers");
        assert_eq!(project.workspace().root(), root.path());
        assert!(project.current_package().is_none());
        assert_eq!(
            project.workspace().default_members(),
            std::slice::from_ref(&app)
        );
        assert_eq!(project.packages().len(), 3);
        let defaults = project
            .select_packages(None, false)
            .expect("default packages");
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].root(), app);

        // --workspace 覆盖 default-members，但 exclude 仍然生效。
        let whole = project
            .select_packages(None, true)
            .expect("whole workspace");
        let mut roots = whole
            .iter()
            .map(|package| package.root().to_path_buf())
            .collect::<Vec<_>>();
        roots.sort();
        assert_eq!(roots, vec![app.clone(), lib.clone(), codegen.clone()]);

        // 从成员目录构建：当前 package 定位到 lib，不再使用 default-members。
        let from_member = Project::discover(&lib).expect("member project discovers");
        assert_eq!(
            from_member
                .current_package()
                .expect("member current")
                .root(),
            lib
        );
        let member_defaults = from_member
            .select_packages(None, false)
            .expect("member selection");
        assert_eq!(member_defaults.len(), 1);
        assert_eq!(member_defaults[0].root(), lib);
    }

    #[test]
    fn root_package_workspace_selects_root_package() {
        let root = TempDir::new().expect("tempdir");
        let package_root = package(
            &root,
            ".",
            "[package]\nname = \"root\"\n\n[workspace]\nmembers = [\"sub\"]\n",
            &[
                ("src/main.gg", "fn main() {}\n"),
                ("sub/gugu.toml", "[package]\nname = \"sub\"\n"),
                ("sub/src/lib.gg", "fn s() {}\n"),
            ],
        );

        let project = Project::discover(&package_root).expect("root workspace discovers");
        assert_eq!(project.packages().len(), 2);
        let selected = project
            .select_packages(None, false)
            .expect("root selection");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name(), "root");
    }

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

    #[test]
    fn rejects_module_file_and_directory_conflict() {
        let root = TempDir::new().expect("tempdir");
        let conflict = package(
            &root,
            "conflict",
            "[package]\nname = \"conflict\"\n",
            &[
                ("src/main.gg", "fn main() {}\n"),
                ("src/foo.gg", "fn a() {}\n"),
                ("src/foo/mod.gg", "fn b() {}\n"),
            ],
        );
        assert!(matches!(
            Project::discover(&conflict),
            Err(ProjectError::TargetDiscovery { .. })
        ));

        // 相邻目录没有 mod.gg 时文件形式合法。
        let file_only = package(
            &root,
            "file-only",
            "[package]\nname = \"file-only\"\n",
            &[
                ("src/main.gg", "fn main() {}\n"),
                ("src/foo.gg", "fn a() {}\n"),
                ("src/foo/util.gg", "fn b() {}\n"),
            ],
        );
        assert!(Project::discover(&file_only).is_ok());
    }

    #[test]
    fn rejects_duplicate_targets_and_escaping_entries() {
        let root = TempDir::new().expect("tempdir");
        let duplicated = package(
            &root,
            "dup",
            "[package]\nname = \"dup\"\n\n[[bin]]\nname = \"extra\"\npath = \
             \"src/bin/extra.gg\"\n",
            &[
                ("src/main.gg", "fn main() {}\n"),
                ("src/bin/extra.gg", "fn main() {}\n"),
            ],
        );
        assert!(matches!(
            Project::discover(&duplicated),
            Err(ProjectError::TargetDiscovery { .. })
        ));

        let outside = package(
            &root,
            "escape",
            "[package]\nname = \"escape\"\n\n[[bin]]\nname = \"outer\"\npath = \
             \"../outer.gg\"\n",
            &[
                ("src/main.gg", "fn main() {}\n"),
                ("outer.gg", "fn main() {}\n"),
            ],
        );
        assert!(matches!(
            Project::discover(&outside),
            Err(ProjectError::TargetDiscovery { .. })
        ));
    }

    #[test]
    fn explicit_lib_and_target_paths_override_autodiscovery() {
        let root = TempDir::new().expect("tempdir");
        // 包名带 `-` 时，lib target 名按规范把 `-` 换成 `_`。
        let package = package(
            &root,
            "explicit",
            "[package]\nname = \"my-lib\"\nauto-bins = false\n\n[lib]\npath = \
             \"source/lib.gg\"\n\n[[bin]]\nname = \"cli\"\npath = \"source/cli.gg\"\n",
            &[
                ("source/lib.gg", "fn util() {}\n"),
                ("source/cli.gg", "fn main() {}\n"),
                ("src/main.gg", "fn main() {}\n"),
            ],
        );
        let project = Project::discover(&package).expect("project discovers");
        let current = project.current_package().expect("current package");
        assert_eq!(
            target_summary(current),
            vec![
                (TargetKind::Lib, "my_lib".into()),
                (TargetKind::Bin, "cli".into()),
            ]
        );
        assert_eq!(current.targets()[0].entry(), &package.join("source/lib.gg"));
        assert_eq!(current.targets()[1].entry(), &package.join("source/cli.gg"));
    }
}
