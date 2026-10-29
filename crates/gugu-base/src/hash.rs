//! 域隔离摘要：所有稳定身份都用 `blake3(domain || 0 || bytes)`，域名区分用途。

/// 计算域隔离的 blake3 摘要。
pub fn hash_domain(domain: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}
