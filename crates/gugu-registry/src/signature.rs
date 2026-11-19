use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::error::{RegistryError, package_invalid, protocol, signature_unverified};
use crate::protocol::SignatureRecord;

/// 签名验证结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifyOutcome {
    /// 签名有效且 key 受信任。
    Trusted,
    /// key_id 不在用户信任配置中。
    UntrustedKey,
    /// 签名记录格式合法但密码学验证失败。
    Invalid,
}

/// `key_id` = 原始 32 字节公钥的 SHA-256 小写十六进制。
pub fn key_id(public_key: &[u8; 32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Ed25519 规范签名消息，字段按固定顺序、每行一个键值、以 LF 结束。
pub fn signature_message(
    registry: &str,
    package: &str,
    version: &str,
    checksum: &str,
    role: &str,
) -> Vec<u8> {
    format!(
        "gugu-signature-v1\nregistry={registry}\npackage={package}\nversion={version}\nchecksum={checksum}\nrole={role}\n"
    )
    .into_bytes()
}

/// 校验签名记录的字段形状（不做密码学验证）。
pub fn validate_record(record: &SignatureRecord) -> Result<(), RegistryError> {
    if !matches!(record.role.as_str(), "publisher" | "registry") {
        return Err(protocol(format!("签名 role `{}` 非法", record.role)));
    }
    if record.algorithm != "ed25519" {
        return Err(protocol(format!(
            "签名算法 `{}` 不受支持（Protocol v1 仅 ed25519）",
            record.algorithm
        )));
    }
    if record.key_id.len() != 64
        || !record
            .key_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(protocol("签名 key_id 不是 64 位小写十六进制"));
    }
    URL_SAFE_NO_PAD
        .decode(&record.signature)
        .map_err(|_| protocol("签名字段不是合法 base64url"))
        .and_then(|bytes| {
            if bytes.len() == 64 {
                Ok(())
            } else {
                Err(protocol("签名字节长度不是 64"))
            }
        })
}

/// 验证一条签名记录：`message` 为规范签名消息，`trusted_keys` 为
/// `key_id -> base64url 公钥`。返回 Trusted/UntrustedKey/Invalid。
pub fn verify(
    record: &SignatureRecord,
    message: &[u8],
    trusted_keys: &std::collections::BTreeMap<String, String>,
) -> Result<VerifyOutcome, RegistryError> {
    validate_record(record)?;
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(&record.signature)
        .map_err(|_| protocol("签名字段不是合法 base64url"))?;
    let signature =
        Signature::try_from(signature_bytes.as_slice()).map_err(|_| protocol("签名字节非法"))?;
    let Some(encoded_key) = trusted_keys.get(&record.key_id) else {
        return Ok(VerifyOutcome::UntrustedKey);
    };
    let key_bytes = URL_SAFE_NO_PAD.decode(encoded_key).map_err(|_| {
        signature_unverified(format!("受信任 key `{}` 不是合法 base64url", record.key_id))
    })?;
    let key_bytes: [u8; 32] = key_bytes.try_into().map_err(|_| {
        signature_unverified(format!("受信任 key `{}` 不是 32 字节", record.key_id))
    })?;
    if key_id(&key_bytes) != record.key_id {
        return Err(signature_unverified(format!(
            "受信任 key `{}` 的摘要与 key_id 不符",
            record.key_id
        )));
    }
    let public = VerifyingKey::from_bytes(&key_bytes).map_err(|_| {
        signature_unverified(format!("受信任 key `{}` 不是合法公钥", record.key_id))
    })?;
    match public.verify(message, &signature) {
        Ok(()) => Ok(VerifyOutcome::Trusted),
        Err(_) => Ok(VerifyOutcome::Invalid),
    }
}

/// 签名记录列表是否包含至少一条受信任的有效签名。
/// 全部无效/不受信任时返回 Ok(false)；记录本身形状非法返回协议错误。
pub fn has_trusted_signature(
    records: &[SignatureRecord],
    message: &[u8],
    trusted_keys: &std::collections::BTreeMap<String, String>,
) -> Result<bool, RegistryError> {
    for record in records {
        if verify(record, message, trusted_keys)? == VerifyOutcome::Trusted {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 生成发布者签名记录：`secret_key` 为 32 字节私钥。
pub fn sign(secret_key: &[u8; 32], message: &[u8]) -> Result<SignatureRecord, RegistryError> {
    let signing = SigningKey::from_bytes(secret_key);
    let signature = signing.sign(message);
    Ok(SignatureRecord {
        role: "publisher".to_owned(),
        algorithm: "ed25519".to_owned(),
        key_id: key_id(&signing.verifying_key().to_bytes()),
        signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
    })
}

/// 解析用户配置中的签名 key 材料：`base64url(32 字节私钥)`。
pub fn parse_secret_key(encoded: &str) -> Result<[u8; 32], RegistryError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded.trim())
        .map_err(|_| package_invalid("签名私钥不是合法 base64url"))?;
    bytes
        .try_into()
        .map_err(|_| package_invalid("签名私钥不是 32 字节"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fixture() -> ([u8; 32], Vec<u8>, SignatureRecord) {
        let secret = [7u8; 32];
        let message = signature_message(
            "https://packages.example/index/",
            "acme/json",
            "1.2.3",
            &"ab".repeat(32),
            "publisher",
        );
        let record = sign(&secret, &message).expect("sign");
        (secret, message, record)
    }

    #[test]
    fn signed_record_verifies_with_trusted_key() {
        let (secret, message, record) = fixture();
        let public = SigningKey::from_bytes(&secret).verifying_key().to_bytes();
        let trusted = BTreeMap::from([(record.key_id.clone(), URL_SAFE_NO_PAD.encode(public))]);
        assert_eq!(
            verify(&record, &message, &trusted).expect("verify"),
            VerifyOutcome::Trusted
        );
    }

    #[test]
    fn unknown_or_wrong_signature_is_not_trusted() {
        let (secret, message, record) = fixture();
        let public = SigningKey::from_bytes(&secret).verifying_key().to_bytes();
        let trusted = BTreeMap::from([(record.key_id.clone(), URL_SAFE_NO_PAD.encode(public))]);
        assert_eq!(
            verify(&record, &message, &BTreeMap::new()).expect("verify"),
            VerifyOutcome::UntrustedKey
        );
        let wrong = signature_message(
            "https://x/",
            "acme/json",
            "1.2.3",
            &"ab".repeat(32),
            "publisher",
        );
        assert_eq!(
            verify(&record, &wrong, &trusted).expect("verify"),
            VerifyOutcome::Invalid
        );
    }

    #[test]
    fn malformed_records_are_protocol_errors() {
        let (_, _, mut record) = fixture();
        record.role = "root".to_owned();
        assert!(verify(&record, b"x", &BTreeMap::new()).is_err());
    }
}
