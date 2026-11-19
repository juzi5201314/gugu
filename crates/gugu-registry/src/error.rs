use std::fmt;

/// Registry/发布路径的稳定诊断类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryErrorKind {
    /// `config.json` 字段、协议版本或身份 URL 不一致。
    Config,
    /// token 缺失、无效或权限不足。
    Auth,
    /// registry 没有请求的 package/version。
    NotFound,
    /// 索引记录、媒体类型、传输或核心字段违反 Protocol v1。
    Protocol,
    /// 已存在 package ID 的 checksum 与待发布归档不同。
    PackageConflict,
    /// 归档内容流、缓存、vendor 或 lock checksum 不一致。
    PackageChecksum,
    /// 归档路径、清单、依赖 source 或发布元数据非法。
    PackageInvalid,
    /// 新解析试图选择 yanked 版本，或严格策略拒绝已锁定 yanked 版本。
    PackageYanked,
    /// 默认模式下已提供但不受信任或验证失败的签名（warning）。
    SignatureUnverified,
    /// 严格模式下没有有效受信任签名。
    SignatureRequired,
    /// 离线模式缺少已验证索引、归档或缓存。
    OfflineMissing,
    /// registry identity 不在 allowlist，或 source 类型违反策略。
    SourceNotAllowed,
}

impl RegistryErrorKind {
    /// 稳定的诊断名。
    pub fn code(self) -> &'static str {
        match self {
            Self::Config => "registry-config",
            Self::Auth => "registry-auth",
            Self::NotFound => "registry-not-found",
            Self::Protocol => "registry-protocol",
            Self::PackageConflict => "package-conflict",
            Self::PackageChecksum => "package-checksum",
            Self::PackageInvalid => "package-invalid",
            Self::PackageYanked => "package-yanked",
            Self::SignatureUnverified => "signature-unverified",
            Self::SignatureRequired => "signature-required",
            Self::OfflineMissing => "offline-missing",
            Self::SourceNotAllowed => "source-not-allowed",
        }
    }
}

/// Registry 协议错误；`kind.code()` 是稳定诊断名。
#[derive(Debug)]
pub struct RegistryError {
    kind: RegistryErrorKind,
    message: String,
}

impl RegistryError {
    pub fn new(kind: RegistryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> RegistryErrorKind {
        self.kind
    }

    /// `<code>: <message>`。
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.kind.code(), self.message)
    }
}

impl std::error::Error for RegistryError {}

pub fn config(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::Config, message)
}

pub fn auth(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::Auth, message)
}

pub fn not_found(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::NotFound, message)
}

pub fn protocol(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::Protocol, message)
}

pub fn package_conflict(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::PackageConflict, message)
}

pub fn package_checksum(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::PackageChecksum, message)
}

pub fn package_invalid(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::PackageInvalid, message)
}

pub fn package_yanked(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::PackageYanked, message)
}

pub fn signature_unverified(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::SignatureUnverified, message)
}

pub fn signature_required(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::SignatureRequired, message)
}

pub fn offline_missing(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::OfflineMissing, message)
}

pub fn source_not_allowed(message: impl Into<String>) -> RegistryError {
    RegistryError::new(RegistryErrorKind::SourceNotAllowed, message)
}
