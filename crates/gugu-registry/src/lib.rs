//! Gugu registry Protocol v1 客户端：身份 URL、稀疏索引、下载、
//! 发布、yank、Ed25519 签名与可替换传输层。
//!
//! 传输抽象（[`Transport`]）让协议测试使用确定性替身；生产网络由
//! [`http::UreqTransport`] 提供，只允许 HTTPS（loopback HTTP 仅限显式
//! 测试配置）且跨 origin 重定向不转发凭据。

mod client;
mod error;
mod http;
mod identity;
mod protocol;
mod signature;

pub use client::{
    Client, MAX_ARCHIVE_BODY, MAX_DOCUMENT_BODY, Method, PublishOutcome, Request, Response,
    Transport, YankOutcome,
};
pub use error::{
    RegistryError, RegistryErrorKind, auth, config, not_found, offline_missing, package_checksum,
    package_conflict, package_invalid, package_yanked, protocol, signature_required,
    signature_unverified, source_not_allowed,
};
pub use http::UreqTransport;
pub use identity::{index_package_url, is_loopback_host, normalize_index_url, origin};
pub use protocol::{ConfigDocument, IndexDep, IndexRecord, SignatureRecord};
pub use signature::{
    VerifyOutcome, has_trusted_signature, key_id, parse_secret_key, sign, signature_message, verify,
};
