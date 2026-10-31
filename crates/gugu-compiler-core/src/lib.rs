//! Gugu runtime artifact 的 target-independent schema、稳定编码和校验。
//!
//! 该 crate 不读取源码、不依赖 compiler frontend；runtime builder 在 build-time 生成
//! `GUGURT01` 容器，compiler façade 只解码经过验证的静态 bytes。

use serde::{Deserialize, Serialize};
use std::fmt;

/// runtime artifact 容器魔数。
pub const ARTIFACT_MAGIC: [u8; 8] = *b"GUGURT01";
/// runtime artifact schema。
pub const ARTIFACT_SCHEMA: u32 = 4;

/// 内建块物化表的 schema：表集或表内布局变化时递增。
/// 4：函数表并入物化表，每个内建实例携带逐函数分析分片。
pub const PREBUILT_SCHEMA: u32 = 4;
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

/// 一个可安装内建实例的物化产物。
///
/// 层字节是前端 canonical 编码，core 只按 key 排序与长度校验；层内语义与校验由安装端负责。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFunctionArtifactV1 {
    /// 实例 MonoKey digest 的小写十六进制。
    pub stable_mono_key: String,
    /// 层字节的内容摘要；只作断言，安装端重算比对。
    pub fingerprint: [u8; 32],
    /// 该实例的分析分片：函数摘要与本 owner 的证明。
    pub analysis: Vec<u8>,
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
    /// 内建块在构建期物化后的派生表；纯源码 artifact 为 None。
    pub prebuilt: Option<RuntimePrebuiltV1>,
}

/// 物化表里一个模块占据的编号区间。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockRangeV1 {
    /// 区间起点（含）。
    pub start: u32,
    /// 区间长度。
    pub len: u32,
}

/// 单个内建模块在物化表里占据的定义/owner 区间。
///
/// 定义表与 owner 表都按模块连续排列，因此每块只需要一对区间；加载期按区间拼出选中的块，
/// 未选中的块不进入会话。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleBlockV1 {
    /// 模块逻辑路径。
    pub path: String,
    /// 该模块占据的定义编号区间。
    pub definitions: BlockRangeV1,
    /// 该模块占据的 owner 编号区间。
    pub owners: BlockRangeV1,
}

/// 内建块在构建期物化后的派生表。
///
/// 每张表都是前端 canonical 编码的字节。加载时按内建块规则安装到当前 world 的编号前缀，
/// 安装前重算指纹并与 [`RuntimePrebuiltV1::fingerprints`] 比对：序列化里的指纹只作断言，
/// 不作为信任来源。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimePrebuiltV1 {
    /// 物化表 schema，取 [`PREBUILT_SCHEMA`]。
    pub schema: u32,
    /// 派生层 revision（层名 → 版本），按层名严格排序。
    pub revisions: Vec<(String, u32)>,
    /// 内建模块的逻辑路径，按 artifact 规范顺序。
    pub module_order: Vec<String>,
    /// canonical(NameResolution 内建片段)。
    pub names: Vec<u8>,
    /// canonical(Identities 内建片段)。
    pub identities: Vec<u8>,
    /// canonical(CheckedSemantics 内建片段)。
    pub checked: Vec<u8>,
    /// canonical(早期常量表内建片段)。
    pub early_constants: Vec<u8>,
    /// canonical(hir::Module 内建片段)。
    pub hir: Vec<u8>,
    /// 按 owner 顺序的 canonical(GirBody) 序列。
    pub gir_bodies: Vec<Vec<u8>>,
    /// canonical(Vec<RecordTypes>)。
    pub record_types: Vec<u8>,
    /// 每张表的 (标识域摘要, 内容摘要)，与表的声明顺序一致。
    pub fingerprints: Vec<([u8; 32], [u8; 32])>,
    /// 函数表各层的 revision（层名 → 版本），按层名严格排序；函数表为空时也可为空。
    pub function_revisions: Vec<(String, u32)>,
    /// 可安装内建实例，按 `stable_mono_key` 严格升序。
    pub functions: Vec<RuntimeFunctionArtifactV1>,
    /// 固定地板集：rt0 与 machine entry 恒需的内建模块，按逻辑路径严格升序。
    pub floor_modules: Vec<String>,
    /// 贡献 `ContractRecord` 的定义所在的内建模块，按逻辑路径严格升序。
    pub contract_modules: Vec<String>,
    /// 按 `module_order` 顺序的模块块，区间合起来覆盖整张定义表与 owner 表。
    pub blocks: Vec<ModuleBlockV1>,
}

impl RuntimePrebuiltV1 {
    /// 校验物化表的结构不变量。
    ///
    /// 这里只查容器可判定的部分：层名与表内容的对应关系、每张表的域摘要口径由前端在安装时
    /// 重算比对，不在 core 里复述。
    pub fn validate(&self) -> Result<(), RuntimeArtifactError> {
        if self.schema != PREBUILT_SCHEMA {
            return Err(RuntimeArtifactError::Schema(self.schema));
        }
        if self.module_order.is_empty()
            || self.module_order.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(RuntimeArtifactError::Manifest(
                "物化表模块顺序必须是非空严格升序的逻辑路径".to_owned(),
            ));
        }
        if self.revisions.is_empty() || self.revisions.windows(2).any(|pair| pair[0].0 >= pair[1].0)
        {
            return Err(RuntimeArtifactError::Manifest(
                "物化表 revision 必须按层名严格排序".to_owned(),
            ));
        }
        if self.fingerprints.is_empty() {
            return Err(RuntimeArtifactError::Manifest(
                "物化表缺少指纹表".to_owned(),
            ));
        }
        for (name, modules) in [
            ("地板集", &self.floor_modules),
            ("契约集", &self.contract_modules),
        ] {
            if modules.windows(2).any(|pair| pair[0] >= pair[1])
                || modules.iter().any(|path| !self.module_order.contains(path))
            {
                return Err(RuntimeArtifactError::Manifest(format!(
                    "{name}必须是 module_order 内的严格升序子集"
                )));
            }
        }
        if self.blocks.len() != self.module_order.len()
            || self
                .blocks
                .iter()
                .zip(&self.module_order)
                .any(|(block, path)| &block.path != path)
        {
            return Err(RuntimeArtifactError::Manifest(
                "模块块必须与 module_order 逐项对应".to_owned(),
            ));
        }
        for (name, ranges) in [
            (
                "定义块",
                self.blocks
                    .iter()
                    .map(|block| block.definitions)
                    .collect::<Vec<_>>(),
            ),
            (
                "owner 块",
                self.blocks
                    .iter()
                    .map(|block| block.owners)
                    .collect::<Vec<_>>(),
            ),
        ] {
            let mut next = 0_u32;
            for range in ranges {
                if range.start != next {
                    return Err(RuntimeArtifactError::Manifest(format!(
                        "{name}必须从 0 起首尾相接"
                    )));
                }
                next = range
                    .start
                    .checked_add(range.len)
                    .ok_or_else(|| RuntimeArtifactError::Manifest(format!("{name}区间溢出 u32")))?;
            }
        }
        if self.floor_modules.is_empty() {
            return Err(RuntimeArtifactError::Manifest(
                "地板集不能为空：prelude 与 runtime 地板模块恒需".to_owned(),
            ));
        }
        self.validate_functions()
    }

    /// 函数表：key 是 64 位小写十六进制且严格升序；有函数时必须声明各层 revision。
    fn validate_functions(&self) -> Result<(), RuntimeArtifactError> {
        if self
            .function_revisions
            .windows(2)
            .any(|pair| pair[0].0 >= pair[1].0)
            || (!self.functions.is_empty() && self.function_revisions.is_empty())
        {
            return Err(RuntimeArtifactError::Manifest(
                "函数表 revision 必须按层名严格排序，且非空函数表必须声明 revision".to_owned(),
            ));
        }
        let well_formed = |key: &str| {
            key.len() == 64
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if self
            .functions
            .iter()
            .any(|function| !well_formed(&function.stable_mono_key))
            || self
                .functions
                .windows(2)
                .any(|pair| pair[0].stable_mono_key >= pair[1].stable_mono_key)
        {
            return Err(RuntimeArtifactError::Manifest(
                "函数表 key 必须是严格升序的 64 位小写十六进制 digest".to_owned(),
            ));
        }
        Ok(())
    }
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
    /// 二进制字段或枚举判别值无效。
    Codec(String),
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
            Self::Codec(message) => write!(f, "runtime artifact 二进制无效：{message}"),
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
            prebuilt: None,
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
            .contracts
            .windows(2)
            .any(|pair| pair[0].key >= pair[1].key)
        {
            return Err(RuntimeArtifactError::Manifest(
                "runtime contract manifest 未按 stable key 严格排序".to_owned(),
            ));
        }
        if self
            .payload
            .generic_templates
            .windows(2)
            .any(|pair| pair[0].key >= pair[1].key)
        {
            return Err(RuntimeArtifactError::Manifest(
                "runtime template manifest 未按 stable key 严格排序".to_owned(),
            ));
        }
        if let Some(prebuilt) = &self.payload.prebuilt {
            prebuilt.validate()?;
        }
        Ok(())
    }

    /// 编码 GUGURT01 容器。
    pub fn encode(&self) -> Result<Vec<u8>, RuntimeArtifactError> {
        self.validate()?;
        let manifest = encode_manifest(&self.manifest)?;
        let payload = encode_payload(&self.payload)?;
        let mut output = Vec::with_capacity(28 + manifest.len() + payload.len() + 32);
        output.extend_from_slice(&ARTIFACT_MAGIC);
        output.extend_from_slice(&ARTIFACT_SCHEMA.to_le_bytes());
        output.extend_from_slice(
            &u64::try_from(manifest.len())
                .map_err(|_| RuntimeArtifactError::Truncated)?
                .to_le_bytes(),
        );
        output.extend_from_slice(
            &u64::try_from(payload.len())
                .map_err(|_| RuntimeArtifactError::Truncated)?
                .to_le_bytes(),
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
        let manifest_len = usize::try_from(u64::from_le_bytes(
            bytes[12..20].try_into().expect("manifest length"),
        ))
        .map_err(|_| RuntimeArtifactError::Truncated)?;
        let payload_len = usize::try_from(u64::from_le_bytes(
            bytes[20..28].try_into().expect("payload length"),
        ))
        .map_err(|_| RuntimeArtifactError::Truncated)?;
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
        let manifest = decode_manifest(&bytes[HEADER..payload_start])?;
        let payload_bytes = &bytes[payload_start..digest_start];
        if bytes[digest_start..] != digest(ARTIFACT_DOMAIN.as_bytes(), payload_bytes) {
            return Err(RuntimeArtifactError::Digest);
        }
        let payload = decode_payload(payload_bytes)?;
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

fn encode_manifest(manifest: &RuntimeArtifactManifestV1) -> Result<Vec<u8>, RuntimeArtifactError> {
    let mut encoder = CodecEncoder::default();
    encoder.u32(manifest.schema);
    encoder.string(&manifest.target)?;
    encoder.digest(&manifest.target_descriptor_digest);
    encoder.digest(&manifest.sysroot_digest);
    encoder.len(manifest.sources.len())?;
    for source in &manifest.sources {
        encoder.string(&source.logical_path)?;
        encoder.digest(&source.content_digest);
        encoder.role(source.role);
    }
    encoder.digest(&manifest.compiler_identity_digest);
    encoder.len(manifest.revisions.len())?;
    for (key, revision) in &manifest.revisions {
        encoder.string(key)?;
        encoder.u32(*revision);
    }
    encoder.digest(&manifest.analysis_policy);
    encoder.digest(&manifest.target_features_digest);
    Ok(encoder.finish())
}

fn decode_manifest(bytes: &[u8]) -> Result<RuntimeArtifactManifestV1, RuntimeArtifactError> {
    let mut decoder = CodecDecoder::new(bytes);
    let schema = decoder.u32()?;
    let target = decoder.string()?;
    let target_descriptor_digest = decoder.digest()?;
    let sysroot_digest = decoder.digest()?;
    let source_count = decoder.len()?;
    let mut sources = Vec::with_capacity(source_count);
    for _ in 0..source_count {
        sources.push(RuntimeSourceRecordV1 {
            logical_path: decoder.string()?,
            content_digest: decoder.digest()?,
            role: decoder.role()?,
        });
    }
    let compiler_identity_digest = decoder.digest()?;
    let revision_count = decoder.len()?;
    let mut revisions = Vec::with_capacity(revision_count);
    for _ in 0..revision_count {
        revisions.push((decoder.string()?, decoder.u32()?));
    }
    let analysis_policy = decoder.digest()?;
    let target_features_digest = decoder.digest()?;
    decoder.finish()?;
    Ok(RuntimeArtifactManifestV1 {
        schema,
        target,
        target_descriptor_digest,
        sysroot_digest,
        sources,
        compiler_identity_digest,
        revisions,
        analysis_policy,
        target_features_digest,
    })
}

fn encode_payload(payload: &RuntimeArtifactPayloadV1) -> Result<Vec<u8>, RuntimeArtifactError> {
    let mut encoder = CodecEncoder::default();
    encoder.u32(payload.module_interface.schema);
    encoder.digest(&payload.module_interface.fingerprint);
    encoder.len(payload.sources.len())?;
    for source in &payload.sources {
        encoder.string(&source.logical_path)?;
        encoder.string(&source.source)?;
        encoder.digest(&source.content_digest);
        encoder.role(source.role);
    }
    encoder.len(payload.contracts.len())?;
    for contract in &payload.contracts {
        encoder.string(&contract.key)?;
        encoder.digest(&contract.fingerprint);
    }
    encoder.len(payload.generic_templates.len())?;
    for template in &payload.generic_templates {
        encoder.string(&template.key)?;
        encoder.digest(&template.fingerprint);
    }
    encoder.u32(u32::from(payload.prebuilt.is_some()));
    if let Some(prebuilt) = &payload.prebuilt {
        encoder.u32(prebuilt.schema);
        encoder.len(prebuilt.revisions.len())?;
        for (layer, revision) in &prebuilt.revisions {
            encoder.string(layer)?;
            encoder.u32(*revision);
        }
        encoder.len(prebuilt.module_order.len())?;
        for path in &prebuilt.module_order {
            encoder.string(path)?;
        }
        encoder.bytes(&prebuilt.names)?;
        encoder.bytes(&prebuilt.identities)?;
        encoder.bytes(&prebuilt.checked)?;
        encoder.bytes(&prebuilt.early_constants)?;
        encoder.bytes(&prebuilt.hir)?;
        encoder.len(prebuilt.gir_bodies.len())?;
        for body in &prebuilt.gir_bodies {
            encoder.bytes(body)?;
        }
        encoder.bytes(&prebuilt.record_types)?;
        encoder.len(prebuilt.fingerprints.len())?;
        for (domain, content) in &prebuilt.fingerprints {
            encoder.digest(domain);
            encoder.digest(content);
        }
        encoder.len(prebuilt.floor_modules.len())?;
        for path in &prebuilt.floor_modules {
            encoder.string(path)?;
        }
        encoder.len(prebuilt.contract_modules.len())?;
        for path in &prebuilt.contract_modules {
            encoder.string(path)?;
        }
        encoder.len(prebuilt.blocks.len())?;
        for block in &prebuilt.blocks {
            encoder.string(&block.path)?;
            encoder.u32(block.definitions.start);
            encoder.u32(block.definitions.len);
            encoder.u32(block.owners.start);
            encoder.u32(block.owners.len);
        }
        encoder.len(prebuilt.function_revisions.len())?;
        for (layer, revision) in &prebuilt.function_revisions {
            encoder.string(layer)?;
            encoder.u32(*revision);
        }
        encoder.len(prebuilt.functions.len())?;
        for function in &prebuilt.functions {
            encoder.string(&function.stable_mono_key)?;
            encoder.digest(&function.fingerprint);
            encoder.bytes(&function.analysis)?;
        }
    }
    Ok(encoder.finish())
}

fn decode_payload(bytes: &[u8]) -> Result<RuntimeArtifactPayloadV1, RuntimeArtifactError> {
    let mut decoder = CodecDecoder::new(bytes);
    let module_interface = RuntimeModuleInterfaceV1 {
        schema: decoder.u32()?,
        fingerprint: decoder.digest()?,
    };
    let source_count = decoder.len()?;
    let mut sources = Vec::with_capacity(source_count);
    for _ in 0..source_count {
        sources.push(RuntimeSourceBlobV1 {
            logical_path: decoder.string()?,
            source: decoder.string()?,
            content_digest: decoder.digest()?,
            role: decoder.role()?,
        });
    }
    let contract_count = decoder.len()?;
    let mut contracts = Vec::with_capacity(contract_count);
    for _ in 0..contract_count {
        contracts.push(RuntimeContractRecordV1 {
            key: decoder.string()?,
            fingerprint: decoder.digest()?,
        });
    }
    let template_count = decoder.len()?;
    let mut generic_templates = Vec::with_capacity(template_count);
    for _ in 0..template_count {
        generic_templates.push(RuntimeGenericTemplateV1 {
            key: decoder.string()?,
            fingerprint: decoder.digest()?,
        });
    }
    let prebuilt = match decoder.u32()? {
        0 => None,
        1 => Some(decode_prebuilt(&mut decoder)?),
        other => {
            return Err(RuntimeArtifactError::Codec(format!(
                "prebuilt 存在标志无效：{other}"
            )));
        }
    };
    decoder.finish()?;
    Ok(RuntimeArtifactPayloadV1 {
        module_interface,
        sources,
        contracts,
        generic_templates,
        prebuilt,
    })
}

/// 解码物化表。
fn decode_prebuilt(
    decoder: &mut CodecDecoder<'_>,
) -> Result<RuntimePrebuiltV1, RuntimeArtifactError> {
    let schema = decoder.u32()?;
    let revision_count = decoder.len()?;
    let mut revisions = Vec::with_capacity(revision_count);
    for _ in 0..revision_count {
        revisions.push((decoder.string()?, decoder.u32()?));
    }
    let module_count = decoder.len()?;
    let mut module_order = Vec::with_capacity(module_count);
    for _ in 0..module_count {
        module_order.push(decoder.string()?);
    }
    let names = decoder.bytes()?;
    let identities = decoder.bytes()?;
    let checked = decoder.bytes()?;
    let early_constants = decoder.bytes()?;
    let hir = decoder.bytes()?;
    let body_count = decoder.len()?;
    let mut gir_bodies = Vec::with_capacity(body_count);
    for _ in 0..body_count {
        gir_bodies.push(decoder.bytes()?);
    }
    let record_types = decoder.bytes()?;
    let fingerprint_count = decoder.len()?;
    let mut fingerprints = Vec::with_capacity(fingerprint_count);
    for _ in 0..fingerprint_count {
        fingerprints.push((decoder.digest()?, decoder.digest()?));
    }
    let floor_count = decoder.len()?;
    let mut floor_modules = Vec::with_capacity(floor_count);
    for _ in 0..floor_count {
        floor_modules.push(decoder.string()?);
    }
    let contract_count = decoder.len()?;
    let mut contract_modules = Vec::with_capacity(contract_count);
    for _ in 0..contract_count {
        contract_modules.push(decoder.string()?);
    }
    let block_count = decoder.len()?;
    let mut blocks = Vec::with_capacity(block_count);
    for _ in 0..block_count {
        blocks.push(ModuleBlockV1 {
            path: decoder.string()?,
            definitions: BlockRangeV1 {
                start: decoder.u32()?,
                len: decoder.u32()?,
            },
            owners: BlockRangeV1 {
                start: decoder.u32()?,
                len: decoder.u32()?,
            },
        });
    }
    let revision_count = decoder.len()?;
    let mut function_revisions = Vec::with_capacity(revision_count);
    for _ in 0..revision_count {
        function_revisions.push((decoder.string()?, decoder.u32()?));
    }
    let function_count = decoder.len()?;
    let mut functions = Vec::with_capacity(function_count);
    for _ in 0..function_count {
        functions.push(RuntimeFunctionArtifactV1 {
            stable_mono_key: decoder.string()?,
            fingerprint: decoder.digest()?,
            analysis: decoder.bytes()?,
        });
    }
    Ok(RuntimePrebuiltV1 {
        schema,
        revisions,
        module_order,
        names,
        identities,
        checked,
        early_constants,
        hir,
        gir_bodies,
        record_types,
        fingerprints,
        function_revisions,
        functions,
        floor_modules,
        contract_modules,
        blocks,
    })
}

#[derive(Default)]
struct CodecEncoder {
    bytes: Vec<u8>,
}

impl CodecEncoder {
    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, value: usize) -> Result<(), RuntimeArtifactError> {
        self.bytes.extend_from_slice(
            &u64::try_from(value)
                .map_err(|_| RuntimeArtifactError::Truncated)?
                .to_le_bytes(),
        );
        Ok(())
    }

    fn digest(&mut self, value: &[u8; 32]) {
        self.bytes.extend_from_slice(value);
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), RuntimeArtifactError> {
        self.len(value.len())?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn string(&mut self, value: &str) -> Result<(), RuntimeArtifactError> {
        self.bytes(value.as_bytes())
    }

    fn role(&mut self, role: RuntimeSourceRole) {
        let tag: u16 = match role {
            RuntimeSourceRole::StandardLibrary => 0,
            RuntimeSourceRole::Runtime => 1,
        };
        self.bytes.extend_from_slice(&tag.to_le_bytes());
    }
}

struct CodecDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> CodecDecoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn finish(self) -> Result<(), RuntimeArtifactError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(RuntimeArtifactError::Codec(
                "artifact 含尾随字节".to_owned(),
            ))
        }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], RuntimeArtifactError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or(RuntimeArtifactError::Truncated)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(RuntimeArtifactError::Truncated)?;
        self.offset = end;
        let mut output = [0_u8; N];
        output.copy_from_slice(bytes);
        Ok(output)
    }

    fn u32(&mut self) -> Result<u32, RuntimeArtifactError> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn len(&mut self) -> Result<usize, RuntimeArtifactError> {
        let length = usize::try_from(u64::from_le_bytes(self.take()?))
            .map_err(|_| RuntimeArtifactError::Truncated)?;
        if length > self.bytes.len() - self.offset {
            return Err(RuntimeArtifactError::Truncated);
        }
        Ok(length)
    }

    fn digest(&mut self) -> Result<[u8; 32], RuntimeArtifactError> {
        self.take()
    }

    fn bytes(&mut self) -> Result<Vec<u8>, RuntimeArtifactError> {
        let length = self.len()?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or(RuntimeArtifactError::Truncated)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(RuntimeArtifactError::Truncated)?;
        self.offset = end;
        Ok(bytes.to_owned())
    }

    fn string(&mut self) -> Result<String, RuntimeArtifactError> {
        String::from_utf8(self.bytes()?)
            .map_err(|error| RuntimeArtifactError::Codec(error.to_string()))
    }

    fn role(&mut self) -> Result<RuntimeSourceRole, RuntimeArtifactError> {
        match u16::from_le_bytes(self.take()?) {
            0 => Ok(RuntimeSourceRole::StandardLibrary),
            1 => Ok(RuntimeSourceRole::Runtime),
            _ => Err(RuntimeArtifactError::Codec(
                "runtime 源角色判别值无效".to_owned(),
            )),
        }
    }
}

fn digest(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&[0]);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// 返回 manifest 的稳定二进制表示。
pub fn canonical_manifest_bytes(manifest: &RuntimeArtifactManifestV1) -> Vec<u8> {
    encode_manifest(manifest).expect("runtime artifact manifest 可编码")
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

    /// 构造一张通过结构校验的物化表。
    fn sample_prebuilt() -> RuntimePrebuiltV1 {
        RuntimePrebuiltV1 {
            schema: PREBUILT_SCHEMA,
            revisions: vec![("gir.body".to_owned(), 6), ("hir.module".to_owned(), 2)],
            module_order: vec!["runtime/core.gg".to_owned(), "std/prelude.gg".to_owned()],
            names: vec![1, 2, 3],
            identities: Vec::new(),
            checked: Vec::new(),
            early_constants: Vec::new(),
            hir: vec![9],
            gir_bodies: vec![vec![4], vec![5, 6]],
            record_types: vec![7],
            fingerprints: vec![([1; 32], [2; 32])],
            function_revisions: vec![("analysis.shard".to_owned(), 1)],
            functions: vec![sample_function("0a"), sample_function("ff")],
            floor_modules: vec!["std/prelude.gg".to_owned()],
            contract_modules: vec!["runtime/core.gg".to_owned()],
            blocks: vec![
                ModuleBlockV1 {
                    path: "runtime/core.gg".to_owned(),
                    definitions: BlockRangeV1 { start: 0, len: 3 },
                    owners: BlockRangeV1 { start: 0, len: 1 },
                },
                ModuleBlockV1 {
                    path: "std/prelude.gg".to_owned(),
                    definitions: BlockRangeV1 { start: 3, len: 2 },
                    owners: BlockRangeV1 { start: 1, len: 2 },
                },
            ],
        }
    }

    /// 一条函数表项；key 以两位前缀加零补齐成 64 位十六进制。
    fn sample_function(prefix: &str) -> RuntimeFunctionArtifactV1 {
        RuntimeFunctionArtifactV1 {
            stable_mono_key: format!("{prefix:0<64}"),
            fingerprint: [3; 32],
            analysis: vec![1],
        }
    }

    /// 带物化表的 artifact。
    fn with_prebuilt(prebuilt: RuntimePrebuiltV1) -> RuntimeArtifactV1 {
        let mut artifact = RuntimeArtifactBuilder::build(
            TargetName::X86_64Linux,
            &[RuntimeSourceInput {
                logical_path: "std/prelude.gg",
                source: "fn prelude() {}",
                role: RuntimeSourceRole::StandardLibrary,
            }],
        )
        .expect("build artifact");
        artifact.payload.prebuilt = Some(prebuilt);
        artifact
    }

    #[test]
    fn prebuilt_tables_round_trip_through_payload_codec() {
        let artifact = with_prebuilt(sample_prebuilt());
        artifact.validate().expect("物化表结构合法");
        let bytes = artifact.encode().expect("encode");
        let decoded = RuntimeArtifactV1::decode(&bytes).expect("decode");
        assert_eq!(decoded, artifact, "物化表必须逐字节往返");
        assert_eq!(artifact.encode().expect("encode again"), bytes);
        assert_eq!(
            canonical_manifest_bytes(&decoded.manifest),
            canonical_manifest_bytes(&artifact.manifest),
            "artifact 身份由 manifest 决定，不随 payload 变化"
        );
    }

    #[test]
    fn prebuilt_tables_reject_unsorted_layers_and_missing_fingerprints() {
        let mut unsorted_modules = sample_prebuilt();
        unsorted_modules.module_order = vec!["std/b.gg".to_owned(), "std/a.gg".to_owned()];
        with_prebuilt(unsorted_modules)
            .validate()
            .expect_err("模块顺序必须严格升序");

        let mut repeated_layer = sample_prebuilt();
        repeated_layer.revisions = vec![("names".to_owned(), 1), ("names".to_owned(), 2)];
        with_prebuilt(repeated_layer)
            .validate()
            .expect_err("层名必须严格排序");

        let mut no_fingerprints = sample_prebuilt();
        no_fingerprints.fingerprints.clear();
        with_prebuilt(no_fingerprints)
            .validate()
            .expect_err("必须带指纹表");

        let mut wrong_schema = sample_prebuilt();
        wrong_schema.schema = PREBUILT_SCHEMA + 1;
        with_prebuilt(wrong_schema)
            .validate()
            .expect_err("schema 必须匹配");
    }

    /// 函数表 key 必须规范且严格升序，非空函数表必须声明 revision。
    #[test]
    fn prebuilt_function_table_rejects_bad_keys_and_missing_revisions() {
        let mut unsorted = sample_prebuilt();
        unsorted.functions.reverse();
        with_prebuilt(unsorted)
            .validate()
            .expect_err("函数表 key 必须严格升序");

        let mut uppercase = sample_prebuilt();
        uppercase.functions[0].stable_mono_key = format!("{:0<64}", "0A");
        with_prebuilt(uppercase)
            .validate()
            .expect_err("函数表 key 必须是小写十六进制");

        let mut short = sample_prebuilt();
        short.functions[0].stable_mono_key = "0a".to_owned();
        with_prebuilt(short)
            .validate()
            .expect_err("函数表 key 必须是 64 位");

        let mut unversioned = sample_prebuilt();
        unversioned.function_revisions.clear();
        with_prebuilt(unversioned)
            .validate()
            .expect_err("非空函数表必须声明 revision");

        let mut empty = sample_prebuilt();
        empty.functions.clear();
        empty.function_revisions.clear();
        with_prebuilt(empty)
            .validate()
            .expect("空函数表无需 revision");
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

    #[test]
    fn artifact_rejects_trailing_truncated_payload_and_invalid_role() {
        let artifact = RuntimeArtifactBuilder::build(
            TargetName::X86_64Linux,
            &[RuntimeSourceInput {
                logical_path: "runtime/x.gg",
                source: "fn x() {}",
                role: RuntimeSourceRole::Runtime,
            }],
        )
        .expect("构造 artifact");
        let bytes = artifact.encode().expect("编码 artifact");
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            RuntimeArtifactV1::decode(&trailing),
            Err(RuntimeArtifactError::Truncated)
        );
        assert_eq!(
            RuntimeArtifactV1::decode(&bytes[..bytes.len() - 1]),
            Err(RuntimeArtifactError::Truncated)
        );
        let mut payload = encode_payload(&artifact.payload).expect("编码 payload");
        let mut decoder = CodecDecoder::new(&payload);
        decoder.u32().expect("接口 schema");
        decoder.digest().expect("接口摘要");
        decoder.len().expect("source 数量");
        decoder.string().expect("source path");
        decoder.string().expect("source 文本");
        decoder.digest().expect("source 摘要");
        let role = decoder.offset;
        payload[role..role + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(
            decode_payload(&payload),
            Err(RuntimeArtifactError::Codec(_))
        ));
        let mut huge = encode_payload(&artifact.payload).expect("编码 payload");
        huge[36..44].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(decode_payload(&huge), Err(RuntimeArtifactError::Truncated));
        assert_eq!(
            RuntimeArtifactV1::decode(br#"{"manifest":{}}"#),
            Err(RuntimeArtifactError::Truncated)
        );
    }
}
