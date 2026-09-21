//! x64 encoder 契约：form 目录、revision 与域隔离内容身份。
//!
//! 契约是「描述符表 + 编码规则 + lowering 规则」的对外身份：`ENCODER_REVISION` 覆盖
//! form 表与编码规则，`LOWERING_REVISION` 覆盖逐 op 序列规则。任何一项变化都必须同时
//! 升高对应 revision，使缓存键、fragment 指纹与镜像计划失效。
//!
//! `poll_cost` 口径是「规范操作数下 lowering 后 form 权重的饱和和」，权重当前统一为 1
//! （即机器指令数）；改表即改 backend schema。

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::frontend::mono::keys::hash_domain;
use crate::target::{CpuBaseline, CpuFeature};

use super::table;

/// 编码器契约 schema 版本。
pub(crate) const ENCODER_SCHEMA: u32 = 2;
/// 描述符表与编码规则 revision。
pub(crate) const ENCODER_REVISION: u32 = 2;
/// lowering 规则 revision。
/// 阶段 54 起：`StackCheck` 降级为标记、`StackAddr` 用 frame 占位基址、成组搬运走拷贝组。
pub(crate) const LOWERING_REVISION: u32 = 3;
/// 寄存器分配与 frame 规则 revision：候选过滤、spill 权重、frame 布局顺序与 prologue 形状。
pub(crate) const ALLOCATION_REVISION: u32 = 1;

/// 契约指纹域。
const ENCODER_DOMAIN: &str = "gugu-x64-encoder-v1";
/// form 目录指纹域。
const CATALOG_DOMAIN: &str = "gugu-x64-encoder-catalog-v1";
/// 单条 form 的路径成本权重上限。
const POLL_COST_MAX: u8 = 64;

/// 契约校验失败：目录、revision 或指纹与登记值不一致。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BackendError {
    message: String,
}

impl BackendError {
    /// 用固定文本创建失败。
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 返回失败文本。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BackendError {}

/// form 目录的一条登记项；与 [`table::FORMS`] 同序同长。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct FormEntry {
    /// form 编号；等于表下标。
    pub(crate) id: u16,
    /// 助记符。
    pub(crate) mnemonic: String,
    /// 操作数形状，如 `r64,rm64`。
    pub(crate) operands: String,
    /// 最低可接受 CPU 特性的规范名。
    pub(crate) feature: String,
    /// 每条机器指令的路径成本权重。
    pub(crate) poll_cost: u8,
}

/// 已校验的编码器契约；进入 action key、fragment query key 与镜像计划。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct EncoderContract {
    /// 内部 schema 版本。
    pub(crate) schema: u32,
    /// 描述符表与编码规则 revision。
    pub(crate) revision: u32,
    /// lowering 规则 revision。
    pub(crate) lowering_revision: u32,
    /// 目标 CPU 基线。
    pub(crate) baseline: CpuBaseline,
    /// form 目录。
    pub(crate) forms: Vec<FormEntry>,
    /// form 目录的域隔离指纹。
    pub(crate) catalog_fingerprint: [u8; 32],
    /// 契约指纹。
    pub(crate) fingerprint: [u8; 32],
}

impl EncoderContract {
    /// 按当前描述符表构建契约。
    pub(crate) fn build(baseline: CpuBaseline) -> Self {
        let forms: Vec<FormEntry> = table::FORMS
            .iter()
            .enumerate()
            .map(|(index, form)| FormEntry {
                id: u16::try_from(index).expect("form 数量适配 u16"),
                mnemonic: form.mnemonic.to_owned(),
                operands: form
                    .operands
                    .iter()
                    .map(|kind| kind.name())
                    .collect::<Vec<_>>()
                    .join(","),
                feature: form.feature.name().to_owned(),
                poll_cost: form.poll_cost.get(),
            })
            .collect();
        let catalog_fingerprint = catalog_fingerprint(&forms);
        let mut contract = Self {
            schema: ENCODER_SCHEMA,
            revision: ENCODER_REVISION,
            lowering_revision: LOWERING_REVISION,
            baseline,
            forms,
            catalog_fingerprint,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract
    }

    /// 校验契约与登记值自洽：版本、目录顺序与形状、权重、特性名与指纹。
    pub(crate) fn verify(&self) -> Result<(), BackendError> {
        if self.schema != ENCODER_SCHEMA
            || self.revision != ENCODER_REVISION
            || self.lowering_revision != LOWERING_REVISION
        {
            return Err(BackendError::new("编码器契约版本与登记值不一致"));
        }
        if !super::encode::form_shapes_are_consistent() {
            return Err(BackendError::new("form 表的操作数与访问语义不一致"));
        }
        if self.forms.len() != table::FORMS.len() {
            return Err(BackendError::new("form 目录长度与描述符表不一致"));
        }
        for (index, (entry, form)) in self.forms.iter().zip(table::FORMS.iter()).enumerate() {
            if usize::from(entry.id) != index {
                return Err(BackendError::new("form 目录编号与表下标不一致"));
            }
            if entry.mnemonic != form.mnemonic {
                return Err(BackendError::new("form 目录助记符与描述符表不一致"));
            }
            let operands: String = form
                .operands
                .iter()
                .map(|kind| kind.name())
                .collect::<Vec<_>>()
                .join(",");
            if entry.operands != operands {
                return Err(BackendError::new("form 目录操作数形状与描述符表不一致"));
            }
            if !(1..=POLL_COST_MAX).contains(&entry.poll_cost)
                || entry.poll_cost != form.poll_cost.get()
            {
                return Err(BackendError::new("form 目录路径成本权重越界或与表不一致"));
            }
            let Some(feature) = CpuFeature::from_name(&entry.feature) else {
                return Err(BackendError::new("form 目录出现未登记的 CPU 特性名"));
            };
            if feature != form.feature {
                return Err(BackendError::new("form 目录特性与描述符表不一致"));
            }
        }
        if self.catalog_fingerprint != catalog_fingerprint(&self.forms) {
            return Err(BackendError::new("form 目录指纹与内容不一致"));
        }
        if self.fingerprint != self.compute_fingerprint() {
            return Err(BackendError::new("编码器契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 契约的规范字节。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("编码器契约可序列化")
    }

    /// 契约的域隔离内容身份；覆盖版本、基线与 form 目录。
    ///
    /// 与其他契约一致：规范字节里 `fingerprint` 字段清零后求域哈希，因此指纹可由内容重算。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        let mut canonical = self.clone();
        canonical.fingerprint = [0; 32];
        hash_domain(ENCODER_DOMAIN, &canonical.canonical_bytes())
    }

    /// 返回契约指纹。
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 超出目标基线的 form 数（登记但 verifier 会拒绝）。
    pub(crate) fn beyond_baseline_count(&self) -> u32 {
        u32::try_from(
            self.forms
                .iter()
                .filter(|entry| {
                    CpuFeature::from_name(&entry.feature)
                        .is_some_and(|feature| !self.baseline.allows(feature))
                })
                .count(),
        )
        .expect("form 数量适配 u32")
    }

    /// 返回人类可读的契约 dump：首行摘要，其后每条 form 一行。
    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "encoder schema={} revision={} lowering-revision={} baseline={} forms={} beyond-baseline={} catalog={} fingerprint={}",
            self.schema,
            self.revision,
            self.lowering_revision,
            self.baseline.name(),
            self.forms.len(),
            self.beyond_baseline_count(),
            hex_lower(self.catalog_fingerprint),
            hex_lower(self.fingerprint)
        );
        for entry in &self.forms {
            let _ = writeln!(
                out,
                "encoder-form {} {} {} feature={} poll-cost={}",
                entry.id, entry.mnemonic, entry.operands, entry.feature, entry.poll_cost
            );
        }
        out
    }
}

/// form 目录的域隔离指纹；覆盖编号、助记符、操作数形状、特性与权重。
fn catalog_fingerprint(forms: &[FormEntry]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(CATALOG_DOMAIN);
    for entry in forms {
        hasher.update(&entry.id.to_le_bytes());
        encode_field(&mut hasher, entry.mnemonic.as_bytes());
        encode_field(&mut hasher, entry.operands.as_bytes());
        encode_field(&mut hasher, entry.feature.as_bytes());
        hasher.update(&[entry.poll_cost]);
    }
    *hasher.finalize().as_bytes()
}

/// 长度前缀字段编码；避免拼接歧义。
fn encode_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(
        &u64::try_from(bytes.len())
            .expect("字段长度适配 u64")
            .to_le_bytes(),
    );
    hasher.update(bytes);
}

fn hex_lower(bytes: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
