use std::{fmt, path::PathBuf};

use super::model::TargetKind;

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
    /// 启用了 package 未声明的 feature。
    UnknownFeature {
        /// 所属 package。
        package: String,
        /// 未知 feature 名。
        feature: String,
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
            Self::UnknownFeature { package, feature } => {
                write!(formatter, "package `{package}` 未声明 feature `{feature}`")
            }
        }
    }
}

impl std::error::Error for ProjectError {}
