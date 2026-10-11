//! Gugu runtime artifact 的 target-independent schema、稳定编码和校验。
//!
//! 该 crate 不读取源码、不依赖 compiler frontend；runtime builder 在 build-time 生成
//! `GUGURT01` 容器，compiler façade 只解码经过验证的静态 bytes。

use serde::{Deserialize, Serialize};
use std::fmt;

/// runtime artifact 容器魔数。
pub const ARTIFACT_MAGIC: [u8; 8] = *b"GUGURT01";
/// runtime artifact schema。
pub const ARTIFACT_SCHEMA: u32 = 1;
/// payload 的域隔离标签。
pub const ARTIFACT_DOMAIN: &str = "gugu-runtime-artifact-v1";
/// GBC payload 编码版本。
pub const GBC_SCHEMA: u32 = 1;

/// 当前 build-time 支持的目标。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum TargetName {
    /// x86_64 Linux。
    X86_64Linux,
    /// x86_64 Windows。
    X86_64Windows,
}

impl TargetName {
    /// 返回规范目标名。
    pub const fn name(self) -> &'static str {
        match self {
            Self::X86_64Linux => "x86_64-linux",
            Self::X86_64Windows => "x86_64-windows",
        }
    }
}

/// runtime source 的稳定角色。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum RuntimeSourceRole {
    /// 标准库源。
    StandardLibrary,
    /// runtime 源。
    Runtime,
}

/// builder 接受的精确 source 输入。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeSourceInput<'a> {
    /// package-relative 逻辑路径。
    pub logical_path: &'a str,
    /// UTF-8 源码。
    pub source: &'a str,
    /// 源码角色。
    pub role: RuntimeSourceRole,
}

/// artifact 中的 source manifest 项。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSourceRecordV1 {
    /// package-relative 逻辑路径。
    pub logical_path: String,
    /// 源码内容摘要。
    pub content_digest: [u8; 32],
    /// 源码角色。
    pub role: RuntimeSourceRole,
}

/// runtime module interface 的稳定摘要。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeModuleInterfaceV1 {
    /// schema。
    pub schema: u32,
    /// 稳定模块接口摘要。
    pub fingerprint: [u8; 32],
}

/// artifact 中用于 build-time/frontend 交接的 source blob。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSourceBlobV1 {
    /// package-relative 逻辑路径。
    pub logical_path: String,
    /// build-time 固化的 UTF-8 源码。
    pub source: String,
    /// 源码内容摘要。
    pub content_digest: [u8; 32],
    /// 源码角色。
    pub role: RuntimeSourceRole,
}

/// 已验证的 runtime contract record。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeContractRecordV1 {
    /// stable record key。
    pub key: String,
    /// record 内容摘要。
    pub fingerprint: [u8; 32],
}

/// 泛型 runtime template 的稳定记录。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeGenericTemplateV1 {
    /// stable definition key。
    pub key: String,
    /// template 内容摘要。
    pub fingerprint: [u8; 32],
}

/// 可按 stable key 复用的 runtime 函数产物。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFunctionArtifactV1 {
    /// 稳定 mono key。
    pub stable_mono_key: String,
    /// 函数摘要。
    pub fingerprint: [u8; 32],
    /// 无绝对地址的 relocatable fragment bytes。
    pub fragment: Vec<u8>,
}

/// artifact manifest；字段是所有跨 build 身份输入的可审计摘要。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifactManifestV1 {
    /// artifact schema。
    pub schema: u32,
    /// 目标名。
    pub target: String,
    /// 完整 target descriptor 摘要。
    pub target_descriptor_digest: [u8; 32],
    /// sysroot、解释器与导入库摘要。
    pub sysroot_digest: [u8; 32],
    /// 内建 source manifest。
    pub sources: Vec<RuntimeSourceRecordV1>,
    /// compiler identity 摘要。
    pub compiler_identity_digest: [u8; 32],
    /// 各内部 schema/revision 摘要。
    pub revisions: Vec<(String, u32)>,
    /// analysis policy 摘要。
    pub analysis_policy: [u8; 32],
    /// CPU/target feature 集合摘要。
    pub target_features_digest: [u8; 32],
}

/// artifact payload。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifactPayloadV1 {
    /// runtime module interface。
    pub module_interface: RuntimeModuleInterfaceV1,
    /// build-time 固化的 runtime/std source blob。
    pub sources: Vec<RuntimeSourceBlobV1>,
    /// record-only contract 表。
    pub contracts: Vec<RuntimeContractRecordV1>,
    /// runtime 泛型模板。
    pub generic_templates: Vec<RuntimeGenericTemplateV1>,
    /// 可执行 runtime 函数片段。
    pub functions: Vec<RuntimeFunctionArtifactV1>,
}

/// 完整 runtime artifact。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeArtifactV1 {
    /// 清单。
    pub manifest: RuntimeArtifactManifestV1,
    /// payload。
    pub payload: RuntimeArtifactPayloadV1,
}

/// decoder/build 错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeArtifactError {
    /// bytes 太短或长度不匹配。
    Truncated,
    /// 容器魔数不匹配。
    Magic,
    /// schema 不匹配。
    Schema(u32),
    /// JSON payload 无法解码。
    Json(String),
    /// payload digest 不匹配。
    Digest,
    /// manifest 内容不满足稳定排序或内部一致性。
    Manifest(String),
    /// 请求目标与 artifact 目标不符。
    TargetMismatch { expected: String, actual: String },
}

impl fmt::Display for RuntimeArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("runtime artifact 截断或长度不匹配"),
            Self::Magic => f.write_str("runtime artifact 魔数不匹配"),
            Self::Schema(schema) => write!(f, "runtime artifact schema 不匹配：{schema}"),
            Self::Json(message) => write!(f, "runtime artifact JSON 无效：{message}"),
            Self::Digest => f.write_str("runtime artifact payload 摘要不匹配"),
            Self::Manifest(message) => write!(f, "runtime artifact manifest 无效：{message}"),
            Self::TargetMismatch { expected, actual } => {
                write!(
                    f,
                    "runtime artifact target 不匹配：期望 {expected}，实际 {actual}"
                )
            }
        }
    }
}

impl std::error::Error for RuntimeArtifactError {}

/// build-time artifact builder。
pub struct RuntimeArtifactBuilder;

impl RuntimeArtifactBuilder {
    /// 以精确 source table 创建一个空执行 fragment 的 artifact。
    pub fn build(
        target: TargetName,
        sources: &[RuntimeSourceInput<'_>],
    ) -> Result<RuntimeArtifactV1, RuntimeArtifactError> {
        let mut records = Vec::with_capacity(sources.len());
        for source in sources {
            if source.logical_path.is_empty() || !source.logical_path.ends_with(".gg") {
                return Err(RuntimeArtifactError::Manifest(format!(
                    "非法 source path `{}`",
                    source.logical_path
                )));
            }
            records.push(RuntimeSourceRecordV1 {
                logical_path: source.logical_path.to_owned(),
                content_digest: digest(b"gugu-runtime-source-v1", source.source.as_bytes()),
                role: source.role,
            });
        }
        records.sort();
        if records
            .windows(2)
            .any(|pair| pair[0].logical_path == pair[1].logical_path)
        {
            return Err(RuntimeArtifactError::Manifest(
                "source path 重复".to_owned(),
            ));
        }
        let source_bytes = records
            .iter()
            .flat_map(|record| record.logical_path.as_bytes().iter().copied())
            .collect::<Vec<_>>();
        let manifest = RuntimeArtifactManifestV1 {
            schema: ARTIFACT_SCHEMA,
            target: target.name().to_owned(),
            target_descriptor_digest: digest(
                b"gugu-target-descriptor-v1",
                target.name().as_bytes(),
            ),
            sysroot_digest: digest(b"gugu-target-sysroot-v1", target.name().as_bytes()),
            sources: records,
            compiler_identity_digest: digest(
                b"gugu-runtime-compiler-identity-v1",
                b"gugu-compiler-core-0.1.0",
            ),
            revisions: vec![
                ("artifact".to_owned(), ARTIFACT_SCHEMA),
                ("module-interface".to_owned(), 1),
                ("runtime-contract".to_owned(), 1),
                ("source-manifest".to_owned(), 1),
            ],
            analysis_policy: digest(
                b"gugu-runtime-analysis-policy-v1",
                b"conservative-indirect-callbacks",
            ),
            target_features_digest: digest(b"gugu-target-features-v1", target.name().as_bytes()),
        };
        let mut source_blobs = sources
            .iter()
            .map(|source| RuntimeSourceBlobV1 {
                logical_path: source.logical_path.to_owned(),
                source: source.source.to_owned(),
                content_digest: digest(b"gugu-runtime-source-v1", source.source.as_bytes()),
                role: source.role,
            })
            .collect::<Vec<_>>();
        source_blobs.sort();
        let payload = RuntimeArtifactPayloadV1 {
            module_interface: RuntimeModuleInterfaceV1 {
                schema: 1,
                fingerprint: digest(b"gugu-runtime-module-interface-v1", &source_bytes),
            },
            sources: source_blobs,
            contracts: Vec::new(),
            generic_templates: Vec::new(),
            functions: Vec::new(),
        };
        let artifact = RuntimeArtifactV1 { manifest, payload };
        artifact.validate()?;
        Ok(artifact)
    }
}

impl RuntimeArtifactV1 {
    /// 验证 manifest、排序和 payload 内部身份。
    pub fn validate(&self) -> Result<(), RuntimeArtifactError> {
        if self.manifest.schema != ARTIFACT_SCHEMA {
            return Err(RuntimeArtifactError::Schema(self.manifest.schema));
        }
        if self.manifest.sources.windows(2).any(|pair| {
            pair[0].logical_path >= pair[1].logical_path
                || pair[0].content_digest == pair[1].content_digest
                    && pair[0].logical_path == pair[1].logical_path
        }) {
            return Err(RuntimeArtifactError::Manifest(
                "source manifest 未按 logical path 严格排序".to_owned(),
            ));
        }
        if self
            .manifest
            .revisions
            .windows(2)
            .any(|pair| pair[0].0 >= pair[1].0)
        {
            return Err(RuntimeArtifactError::Manifest(
                "revision manifest 未按 key 严格排序".to_owned(),
            ));
        }
        if self
            .payload
            .sources
            .windows(2)
            .any(|pair| pair[0].logical_path >= pair[1].logical_path)
        {
            return Err(RuntimeArtifactError::Manifest(
                "source blob 未按 logical path 严格排序".to_owned(),
            ));
        }
        if self.payload.sources.len() != self.manifest.sources.len()
            || self
                .payload
                .sources
                .iter()
                .zip(&self.manifest.sources)
                .any(|(blob, record)| {
                    blob.logical_path != record.logical_path
                        || blob.content_digest != record.content_digest
                        || digest(b"gugu-runtime-source-v1", blob.source.as_bytes())
                            != blob.content_digest
                        || blob.role != record.role
                })
        {
            return Err(RuntimeArtifactError::Manifest(
                "source blob 与 manifest 不一致".to_owned(),
            ));
        }
        if self
            .payload
            .functions
            .windows(2)
            .any(|pair| pair[0].stable_mono_key >= pair[1].stable_mono_key)
        {
            return Err(RuntimeArtifactError::Manifest(
                "runtime function manifest 未按 stable key 严格排序".to_owned(),
            ));
        }
        Ok(())
    }

    /// 编码 GUGURT01 容器。
    pub fn encode(&self) -> Result<Vec<u8>, RuntimeArtifactError> {
        self.validate()?;
        let manifest = serde_json::to_vec(&self.manifest)
            .map_err(|error| RuntimeArtifactError::Json(error.to_string()))?;
        let payload = serde_json::to_vec(&self.payload)
            .map_err(|error| RuntimeArtifactError::Json(error.to_string()))?;
        let mut output = Vec::with_capacity(8 + 4 + 16 + manifest.len() + payload.len() + 32);
        output.extend_from_slice(&ARTIFACT_MAGIC);
        output.extend_from_slice(&ARTIFACT_SCHEMA.to_le_bytes());
        output.extend_from_slice(
            &(u64::try_from(manifest.len()).expect("manifest length fits u64")).to_le_bytes(),
        );
        output.extend_from_slice(
            &(u64::try_from(payload.len()).expect("payload length fits u64")).to_le_bytes(),
        );
        output.extend_from_slice(&manifest);
        output.extend_from_slice(&payload);
        output.extend_from_slice(&digest(ARTIFACT_DOMAIN.as_bytes(), &payload));
        Ok(output)
    }

    /// 解码并验证 GUGURT01 容器。
    pub fn decode(bytes: &[u8]) -> Result<Self, RuntimeArtifactError> {
        const HEADER: usize = 8 + 4 + 8 + 8;
        if bytes.len() < HEADER + 32 {
            return Err(RuntimeArtifactError::Truncated);
        }
        if bytes[..8] != ARTIFACT_MAGIC {
            return Err(RuntimeArtifactError::Magic);
        }
        let schema = u32::from_le_bytes(bytes[8..12].try_into().expect("schema"));
        if schema != ARTIFACT_SCHEMA {
            return Err(RuntimeArtifactError::Schema(schema));
        }
        let manifest_len = u64::from_le_bytes(bytes[12..20].try_into().expect("manifest length"));
        let payload_len = u64::from_le_bytes(bytes[20..28].try_into().expect("payload length"));
        let manifest_len =
            usize::try_from(manifest_len).map_err(|_| RuntimeArtifactError::Truncated)?;
        let payload_len =
            usize::try_from(payload_len).map_err(|_| RuntimeArtifactError::Truncated)?;
        let payload_start = HEADER
            .checked_add(manifest_len)
            .ok_or(RuntimeArtifactError::Truncated)?;
        let digest_start = payload_start
            .checked_add(payload_len)
            .ok_or(RuntimeArtifactError::Truncated)?;
        let end = digest_start
            .checked_add(32)
            .ok_or(RuntimeArtifactError::Truncated)?;
        if end != bytes.len() {
            return Err(RuntimeArtifactError::Truncated);
        }
        let manifest: RuntimeArtifactManifestV1 =
            serde_json::from_slice(&bytes[HEADER..payload_start])
                .map_err(|error| RuntimeArtifactError::Json(error.to_string()))?;
        let payload_bytes = &bytes[payload_start..digest_start];
        if bytes[digest_start..] != digest(ARTIFACT_DOMAIN.as_bytes(), payload_bytes) {
            return Err(RuntimeArtifactError::Digest);
        }
        let payload: RuntimeArtifactPayloadV1 = serde_json::from_slice(payload_bytes)
            .map_err(|error| RuntimeArtifactError::Json(error.to_string()))?;
        let artifact = Self { manifest, payload };
        artifact.validate()?;
        Ok(artifact)
    }

    /// 验证目标名称。
    pub fn for_target(&self, target: TargetName) -> Result<(), RuntimeArtifactError> {
        if self.manifest.target != target.name() {
            return Err(RuntimeArtifactError::TargetMismatch {
                expected: target.name().to_owned(),
                actual: self.manifest.target.clone(),
            });
        }
        Ok(())
    }
}

fn digest(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&[0]);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// 将 artifact 输入转换为确定性 source digest，供 builder 与测试共享。
pub fn source_digest(source: &str) -> [u8; 32] {
    digest(b"gugu-runtime-source-v1", source.as_bytes())
}

impl fmt::Display for TargetName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// 保持核心 schema 不依赖 compiler；这两个轻量断言覆盖容器的关键拒绝路径。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_round_trip_is_deterministic() {
        let artifact = RuntimeArtifactBuilder::build(
            TargetName::X86_64Linux,
            &[RuntimeSourceInput {
                logical_path: "std/prelude.gg",
                source: "fn prelude() {}",
                role: RuntimeSourceRole::StandardLibrary,
            }],
        )
        .expect("build artifact");
        let bytes = artifact.encode().expect("encode artifact");
        assert_eq!(
            RuntimeArtifactV1::decode(&bytes).expect("decode artifact"),
            artifact
        );
        assert_eq!(artifact.encode().expect("encode again"), bytes);
    }

    #[test]
    fn artifact_rejects_digest_and_target_mismatch() {
        let artifact =
            RuntimeArtifactBuilder::build(TargetName::X86_64Windows, &[]).expect("build");
        let mut bytes = artifact.encode().expect("encode");
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert_eq!(
            RuntimeArtifactV1::decode(&bytes),
            Err(RuntimeArtifactError::Digest)
        );
        artifact
            .for_target(TargetName::X86_64Linux)
            .expect_err("target mismatch");
    }
    #[test]
    fn artifact_rejects_magic_schema_length_and_unsorted_keys() {
        let artifact = RuntimeArtifactBuilder::build(TargetName::X86_64Linux, &[]).expect("build");
        let bytes = artifact.encode().expect("encode");

        let mut magic = bytes.clone();
        magic[0] ^= 1;
        assert_eq!(
            RuntimeArtifactV1::decode(&magic),
            Err(RuntimeArtifactError::Magic)
        );

        let mut schema = bytes.clone();
        schema[8..12].copy_from_slice(&99_u32.to_le_bytes());
        assert_eq!(
            RuntimeArtifactV1::decode(&schema),
            Err(RuntimeArtifactError::Schema(99))
        );

        let mut length = bytes;
        length[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            RuntimeArtifactV1::decode(&length),
            Err(RuntimeArtifactError::Truncated)
        );

        let mut invalid = artifact;
        invalid.manifest.revisions.reverse();
        assert!(matches!(
            invalid.encode(),
            Err(RuntimeArtifactError::Manifest(_))
        ));
    }
}
