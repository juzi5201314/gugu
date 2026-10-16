use std::{collections::BTreeMap, fmt, path::PathBuf};

use super::semver::{Version, VersionReq};

/// 依赖所属的解析域。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DependencyDomain {
    /// 普通 target graph。
    Normal,
    /// test、bench 和 example graph。
    Test,
    /// host build graph。
    Build,
}

impl fmt::Display for DependencyDomain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Normal => "normal",
            Self::Test => "test",
            Self::Build => "build",
        })
    }
}

/// 清单中的依赖 source 选择。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DependencySource {
    /// 本地 path source。
    Path {
        /// 相对依赖 package 根的路径。
        path: PathBuf,
    },
    /// Git source。
    Git {
        /// 规范仓库 URL。
        url: String,
        /// 不可变 commit。
        rev: Option<String>,
        /// tag 引用。
        tag: Option<String>,
        /// branch 引用。
        branch: Option<String>,
    },
    /// Registry source。
    Registry {
        /// registry 逻辑名或规范身份。
        registry: String,
    },
}

impl DependencySource {
    /// 创建 registry source。
    pub fn registry(registry: impl Into<String>) -> Self {
        Self::Registry {
            registry: registry.into(),
        }
    }

    /// 创建 path source。
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self::Path { path: path.into() }
    }

    /// 创建 Git source。
    pub fn git(url: impl Into<String>) -> Self {
        Self::Git {
            url: url.into(),
            rev: None,
            tag: None,
            branch: None,
        }
    }
}

/// target 条件表达式。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TargetCondition {
    expression: String,
    expr: CfgExpr,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CfgExpr {
    Atom(String),
    KeyValue(String, String),
    All(Vec<CfgExpr>),
    Any(Vec<CfgExpr>),
    Not(Box<CfgExpr>),
}

impl TargetCondition {
    /// 解析 `cfg(...)` 条件。
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        let body = input
            .strip_prefix("cfg(")
            .and_then(|body| body.strip_suffix(')'))
            .ok_or_else(|| format!("target 条件必须是 cfg(...)：`{input}`"))?;
        let mut parser = CfgParser::new(body);
        let expr = parser.parse_expr()?;
        parser.skip_space();
        if !parser.at_end() {
            return Err(format!("target 条件包含多余内容：`{input}`"));
        }
        Ok(Self {
            expression: input.to_owned(),
            expr,
        })
    }

    /// 返回规范条件文本。
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// 在目标名上求值。
    pub fn matches(&self, target: &str) -> bool {
        self.expr.matches(target)
    }
}

impl fmt::Display for TargetCondition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.expression)
    }
}

struct CfgParser<'src> {
    input: &'src str,
    offset: usize,
}

impl<'src> CfgParser<'src> {
    fn new(input: &'src str) -> Self {
        Self { input, offset: 0 }
    }

    fn parse_expr(&mut self) -> Result<CfgExpr, String> {
        self.skip_space();
        let name = self.parse_name()?;
        self.skip_space();
        if self.consume('=') {
            self.skip_space();
            let value = self.parse_string()?;
            if !matches!(
                name.as_str(),
                "target"
                    | "target_arch"
                    | "target_os"
                    | "target_env"
                    | "target_family"
                    | "target_pointer_width"
            ) {
                return Err(format!("未知 target cfg 名称 `{name}`"));
            }
            return Ok(CfgExpr::KeyValue(name, value));
        }
        if self.consume('(') {
            let mut args = Vec::new();
            loop {
                self.skip_space();
                if self.consume(')') {
                    break;
                }
                args.push(self.parse_expr()?);
                self.skip_space();
                if self.consume(')') {
                    break;
                }
                if !self.consume(',') {
                    return Err("target cfg 参数之间需要逗号".to_owned());
                }
            }
            return match name.as_str() {
                "all" => Ok(CfgExpr::All(args)),
                "any" => Ok(CfgExpr::Any(args)),
                "not" if args.len() == 1 => Ok(CfgExpr::Not(Box::new(
                    args.into_iter().next().expect("长度已检查"),
                ))),
                "not" => Err("target cfg not 只能有一个参数".to_owned()),
                _ => Err(format!("未知 target cfg 函数 `{name}`")),
            };
        }
        if name == "true" || name == "false" {
            return Ok(CfgExpr::Atom(name));
        }
        Err(format!("target cfg 原子 `{name}` 不受支持"))
    }

    fn parse_name(&mut self) -> Result<String, String> {
        let start = self.offset;
        while self
            .input
            .as_bytes()
            .get(self.offset)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            self.offset += 1;
        }
        if self.offset == start {
            return Err("target cfg 缺少名称".to_owned());
        }
        Ok(self.input[start..self.offset].to_owned())
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if !self.consume('"') {
            return Err("target cfg 值必须是双引号字符串".to_owned());
        }
        let start = self.offset;
        while self.offset < self.input.len() && self.input.as_bytes()[self.offset] != b'"' {
            if self.input.as_bytes()[self.offset] == b'\\'
                || !self.input.as_bytes()[self.offset].is_ascii()
            {
                return Err("target cfg 字符串只允许 ASCII 且不允许转义".to_owned());
            }
            self.offset += 1;
        }
        if self.offset == self.input.len() {
            return Err("target cfg 字符串缺少结束引号".to_owned());
        }
        let value = self.input[start..self.offset].to_owned();
        self.offset += 1;
        Ok(value)
    }

    fn consume(&mut self, expected: char) -> bool {
        if self.input[self.offset..].starts_with(expected) {
            self.offset += expected.len_utf8();
            true
        } else {
            false
        }
    }

    fn skip_space(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.offset)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.offset += 1;
        }
    }

    fn at_end(&self) -> bool {
        self.offset == self.input.len()
    }
}

impl CfgExpr {
    fn matches(&self, target: &str) -> bool {
        match self {
            Self::Atom(value) => value == "true",
            Self::KeyValue(key, value) => cfg_value(key, target) == value,
            Self::All(values) => values.iter().all(|value| value.matches(target)),
            Self::Any(values) => values.iter().any(|value| value.matches(target)),
            Self::Not(value) => !value.matches(target),
        }
    }
}

fn cfg_value<'target>(key: &str, target: &'target str) -> &'target str {
    match key {
        "target" => target,
        "target_arch" => target.split('-').next().unwrap_or(target),
        "target_os" => target.rsplit('-').next().unwrap_or(target),
        "target_env" if target.ends_with("-windows") => "msvc",
        "target_env" => "gnu",
        "target_family" if target.ends_with("-windows") => "windows",
        "target_family" => "unix",
        "target_pointer_width" => "64",
        _ => "",
    }
}

/// 依赖的 package 身份。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PackageId {
    name: String,
    version: Version,
    source: PackageSource,
}

impl PackageId {
    /// 创建 package ID。
    pub fn new(name: impl Into<String>, version: Version, source: PackageSource) -> Self {
        Self {
            name: name.into(),
            version,
            source,
        }
    }

    /// 返回 package 名称。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 返回精确版本。
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// 返回 source 身份。
    pub fn source(&self) -> &PackageSource {
        &self.source
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}@{} ({})",
            self.name, self.version, self.source
        )
    }
}

/// 锁图中的规范 source 身份。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PackageSource {
    /// 本地 path source。
    Path {
        /// workspace 或当前 package 的规范相对路径。
        path: String,
    },
    /// 已锁定 commit 与 tree hash 的 Git source。
    Git {
        /// 规范仓库 URL。
        url: String,
        /// 完整 commit。
        commit: String,
        /// 规范 tree hash。
        tree: String,
    },
    /// Registry source。
    Registry {
        /// registry 规范身份。
        registry: String,
    },
}

impl fmt::Display for PackageSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path { path } => write!(formatter, "path+{path}"),
            Self::Git { url, commit, tree } => {
                write!(formatter, "git+{url}?commit={commit}&tree={tree}")
            }
            Self::Registry { registry } => write!(formatter, "registry+{registry}"),
        }
    }
}

/// 一条已解析的依赖声明。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DependencySpec {
    /// 源码中的别名。
    pub alias: String,
    /// 被依赖 package 的规范名称。
    pub package: String,
    /// 版本约束。
    pub version: VersionReq,
    /// source 选择。
    pub source: DependencySource,
    /// 要传递给被依赖 package 的 feature。
    pub features: Vec<String>,
    /// 是否启用被依赖 package 的 default feature。
    pub default_features: bool,
    /// 是否为 optional dependency。
    pub optional: bool,
    /// 声明域。
    pub domain: DependencyDomain,
    /// target 条件。
    pub target: Option<TargetCondition>,
}

/// 解析器使用的 package 元数据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageMetadata {
    /// 精确 package ID。
    pub id: PackageId,
    /// 依赖声明。
    pub dependencies: Vec<DependencySpec>,
    /// feature 到 feature 引用的映射。
    pub features: BTreeMap<String, Vec<String>>,
    /// registry package checksum。
    pub checksum: Option<String>,
    /// registry 是否撤回。
    pub yanked: bool,
}

impl PackageMetadata {
    /// 创建 package 元数据。
    pub fn new(id: PackageId) -> Self {
        Self {
            id,
            dependencies: Vec::new(),
            features: BTreeMap::new(),
            checksum: None,
            yanked: false,
        }
    }

    /// 设置依赖声明。
    pub fn with_dependencies(mut self, dependencies: Vec<DependencySpec>) -> Self {
        self.dependencies = dependencies;
        self
    }

    /// 设置 feature 定义。
    pub fn with_features(mut self, features: BTreeMap<String, Vec<String>>) -> Self {
        self.features = features;
        self
    }

    /// 设置 checksum。
    pub fn with_checksum(mut self, checksum: impl Into<String>) -> Self {
        self.checksum = Some(checksum.into());
        self
    }

    /// 设置 yanked 状态。
    pub fn with_yanked(mut self, yanked: bool) -> Self {
        self.yanked = yanked;
        self
    }
}

/// 依赖解析选项与内存 source index。
#[derive(Clone, Debug)]
pub struct ResolveOptions {
    /// target graph 的目标名称。
    pub target: String,
    /// build graph 的宿主名称。
    pub host: String,
    /// 未显式指定 registry 时使用的 registry 身份。
    pub default_registry: String,
    /// 只解析指定 root；为空时解析 workspace 全部 package。
    pub roots: Vec<String>,
    /// root package 各域追加的 feature。
    pub root_features: BTreeMap<String, Vec<String>>,
    /// root package 是否启用默认 feature；未出现的 root 默认启用。
    pub root_default_features: BTreeMap<String, bool>,
    /// registry index 中的候选 package。
    pub registry_packages: Vec<PackageMetadata>,
    /// Git source 中的候选 package。
    pub git_packages: Vec<PackageMetadata>,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            target: "x86_64-linux".to_owned(),
            host: "x86_64-linux".to_owned(),
            default_registry: "default".to_owned(),
            roots: Vec::new(),
            root_features: BTreeMap::new(),
            root_default_features: BTreeMap::new(),
            registry_packages: Vec::new(),
            git_packages: Vec::new(),
        }
    }
}

/// 锁图中的依赖边。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockedDependency {
    /// 源码别名。
    pub alias: String,
    /// 被依赖 package ID。
    pub package: PackageId,
    /// 所属 graph 域。
    pub domain: DependencyDomain,
    /// target 条件文本。
    pub target: Option<String>,
    /// feature 请求。
    pub features: Vec<String>,
    /// 是否启用 default feature。
    pub default_features: bool,
}

/// 锁图中的 package 记录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockedPackage {
    /// package ID。
    pub id: PackageId,
    /// registry checksum。
    pub checksum: Option<String>,
    /// 解析后的依赖边。
    pub dependencies: Vec<LockedDependency>,
    /// 三个域的 feature 并集。
    pub features: BTreeMap<DependencyDomain, Vec<String>>,
}

/// 可确定性编码的 Gugu 锁图。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockGraph {
    /// 锁文件格式版本。
    pub version: u32,
    /// package 记录。
    pub packages: Vec<LockedPackage>,
}
