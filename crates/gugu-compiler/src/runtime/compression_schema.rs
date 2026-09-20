//! checked pointer compression 的契约段。
//!
//! 本段把「cage id / generation / offset 的位布局」「cage 上限与粒度」「FFI 交接规则」
//! 「统计口径」「目标能力检查」与「由优化后 LIR 推导的压缩需求」固定成带版本的对象，
//! 与 `stackmap_schema` / `block_return_schema` 共用
//! `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。
//!
//! profile 关闭时不存在 cage：压缩引用与完整地址语义等价，世界不预留任何 cage。开启
//! 后只允许登记的目标能力与粒度组合，且需求（解码点、压缩根槽）必须与开关一致。

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::cage_control::{CAGE_CANONICAL_LIMIT, CAGE_CONTROL_FIELDS, CageControlRecord};
use super::model::RawModelError;
use crate::target::PointerCompression;

/// 压缩契约段 schema。
pub(crate) const COMPRESSION_SCHEMA: u32 = 2;

/// 内建 compression profile 名。
pub(crate) const COMPRESSION_PROFILE_NAME: &str = "mosaic-compression";
/// profile revision；位布局、粒度、FFI 规则、控制记录或统计口径变化都必须递增。
pub(crate) const COMPRESSION_PROFILE_REVISION: u32 = 2;

/// 压缩字里的 cage id 位数。
pub(crate) const CAGE_ID_BITS: u32 = 8;
/// 压缩字里的 generation 位数。
pub(crate) const CAGE_GENERATION_BITS: u32 = 24;
/// 压缩字里的 cage 内 offset 位数。
pub(crate) const CAGE_OFFSET_BITS: u32 = 32;
/// cage id 在 64-bit 字中的位移。
pub(crate) const CAGE_ID_SHIFT: u32 = 56;
/// generation 在 64-bit 字中的位移。
pub(crate) const CAGE_GENERATION_SHIFT: u32 = 32;
/// offset 掩码。
pub(crate) const CAGE_OFFSET_MASK: u64 = 0xffff_ffff;
/// generation 掩码。
pub(crate) const CAGE_GENERATION_MASK: u64 = 0xff_ffff;
/// 空压缩字；解码为空引用而不是地址 0。
pub(crate) const CAGE_NULL_WORD: u64 = 0;
/// generation 从 1 起：0 与空字歧义，永不使用。
pub(crate) const CAGE_GENERATION_MIN: u32 = 1;
/// 单个 cage 的字节上界。
pub(crate) const CAGE_MAX_BYTES: u64 = 1 << 32;
/// cage 尺寸与基址的粒度；与 GC arena 尺寸同源，保证每个 managed arena 恰好一个 island。
pub(crate) const CAGE_GRANULE_BYTES: u64 = super::gc_metadata_contract::GC_ARENA_BYTES;
/// 压缩根在根 map 中的登记种类名；与栈图 `compressed-ref` 判别值一致。
pub(crate) const CAGE_COMPRESSED_ROOT_KIND: &str = "compressed-ref";
/// FFI 交接规则；顺序即登记顺序。
pub(crate) const CAGE_FFI_RULES: [&str; 3] = [
    "resolve-then-pin",
    "no-compressed-pass-through",
    "save-requires-active-lease",
];
/// 压缩统计名；顺序即登记顺序，与 `CompressionStats` 的字段顺序一致。
pub(crate) const CAGE_STATISTICS: [&str; 6] = [
    "compressed_ref_decodes",
    "compression_decode_rejections",
    "compression_foreign_pins",
    "compression_foreign_saves",
    "compression_foreign_copies",
    "compression_foreign_rejections",
];

/// cage profile 的显式开关；默认关闭表示 full-pointer 语义。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CompressionPolicyV1 {
    /// 是否启用 cage profile。
    pub enabled: bool,
    /// 预留的 cage 字节数；未启用时必须为 0。
    pub cage_bytes: u64,
}

impl CompressionPolicyV1 {
    /// 关闭态：不预留 cage，压缩引用与完整地址等价。
    pub(crate) const fn disabled() -> Self {
        Self {
            enabled: false,
            cage_bytes: 0,
        }
    }

    /// 开启态：预留 `cage_bytes` 字节的 cage。
    pub(crate) const fn cage(cage_bytes: u64) -> Self {
        Self {
            enabled: true,
            cage_bytes,
        }
    }
}

/// 由优化后 LIR 推导的压缩需求，不是运行时计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CompressionDemand {
    /// `DecodeCompressedRef` 指令数；每个站点至多解码一次压缩引用。
    pub decode_sites: u32,
    /// 栈图压缩根槽数；每个槽至多解码一次。
    pub compressed_root_slots: u32,
}

impl CompressionDemand {
    /// 返回需求视图的稳定指纹。
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let mut bytes = Vec::with_capacity(8);
        bytes.extend_from_slice(&self.decode_sites.to_le_bytes());
        bytes.extend_from_slice(&self.compressed_root_slots.to_le_bytes());
        crate::frontend::mono::keys::hash_domain("gugu-compression-demand-v1", &bytes)
    }

    /// 校验需求自身：两个字段都是独立的编译期上界，没有交叉约束。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        Ok(())
    }
}

/// cage 控制记录的一个字段：名、字节偏移与宽度。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CageControlField {
    /// 字段名。
    pub name: String,
    /// 字节偏移。
    pub offset: u32,
    /// 字节宽度。
    pub size: u32,
}

/// 已验证的压缩 runtime 契约；版本变化使 RuntimeRawModel 与 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CompressionRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// profile 名。
    pub profile: String,
    /// profile revision。
    pub profile_revision: u32,
    /// cage profile 是否启用。
    pub enabled: bool,
    /// 本契约预留的 cage 字节数；未启用时为 0。
    pub cage_bytes: u64,
    /// cage 尺寸与基址粒度。
    pub cage_granule_bytes: u64,
    /// cage 字节上限。
    pub max_cage_bytes: u64,
    /// 压缩字里的 cage id 位数。
    pub cage_id_bits: u32,
    /// 压缩字里的 generation 位数。
    pub cage_generation_bits: u32,
    /// 压缩字里的 offset 位数。
    pub cage_offset_bits: u32,
    /// cage id 位移。
    pub cage_id_shift: u32,
    /// generation 位移。
    pub cage_generation_shift: u32,
    /// offset 掩码。
    pub cage_offset_mask: u64,
    /// generation 掩码。
    pub cage_generation_mask: u64,
    /// 空压缩字。
    pub null_word: u64,
    /// generation 起点。
    pub generation_min: u32,
    /// generation 上界。
    pub generation_max: u32,
    /// 压缩根的种类名。
    pub compressed_root_kind: String,
    /// cage 控制记录字段表；机器解码序列按它读取记录。
    pub cage_control_fields: Vec<CageControlField>,
    /// cage 控制记录字节数。
    pub cage_control_bytes: u32,
    /// cage 控制记录对齐。
    pub cage_control_align: u32,
    /// FFI 交接规则目录。
    pub ffi_rules: Vec<String>,
    /// 统计口径目录。
    pub statistics: Vec<String>,
    /// 目标能力声明。
    pub capability: PointerCompression,
    /// 上游需求视图。
    pub demand: CompressionDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl CompressionRuntimeContract {
    /// 由需求视图、profile 开关与目标能力构建契约；关闭态仍建立完整契约状态。
    pub(crate) fn build(
        demand: CompressionDemand,
        policy: CompressionPolicyV1,
        capability: PointerCompression,
    ) -> Result<Self, RawModelError> {
        let mut contract = Self {
            schema: COMPRESSION_SCHEMA,
            profile: COMPRESSION_PROFILE_NAME.to_owned(),
            profile_revision: COMPRESSION_PROFILE_REVISION,
            enabled: policy.enabled,
            cage_bytes: policy.cage_bytes,
            cage_granule_bytes: CAGE_GRANULE_BYTES,
            max_cage_bytes: CAGE_MAX_BYTES,
            cage_id_bits: CAGE_ID_BITS,
            cage_generation_bits: CAGE_GENERATION_BITS,
            cage_offset_bits: CAGE_OFFSET_BITS,
            cage_id_shift: CAGE_ID_SHIFT,
            cage_generation_shift: CAGE_GENERATION_SHIFT,
            cage_offset_mask: CAGE_OFFSET_MASK,
            cage_generation_mask: CAGE_GENERATION_MASK,
            null_word: CAGE_NULL_WORD,
            generation_min: CAGE_GENERATION_MIN,
            generation_max: (1 << CAGE_GENERATION_BITS) - 1,
            compressed_root_kind: CAGE_COMPRESSED_ROOT_KIND.to_owned(),
            cage_control_fields: CAGE_CONTROL_FIELDS
                .iter()
                .map(|(name, offset, size)| CageControlField {
                    name: (*name).to_owned(),
                    offset: *offset,
                    size: *size,
                })
                .collect(),
            cage_control_bytes: CageControlRecord::byte_len(),
            cage_control_align: CageControlRecord::byte_align(),
            ffi_rules: CAGE_FFI_RULES
                .iter()
                .map(|rule| (*rule).to_owned())
                .collect(),
            statistics: CAGE_STATISTICS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            capability,
            demand,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 返回内部 schema 版本。
    pub(crate) const fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回 profile 名。
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// 返回 profile revision。
    pub const fn profile_revision(&self) -> u32 {
        self.profile_revision
    }

    /// 返回 cage profile 是否启用。
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// 返回本契约预留的 cage 字节数。
    pub const fn cage_bytes(&self) -> u64 {
        self.cage_bytes
    }

    /// 返回 cage 粒度。
    pub const fn cage_granule_bytes(&self) -> u64 {
        self.cage_granule_bytes
    }

    /// 返回 cage 字节上限。
    pub const fn max_cage_bytes(&self) -> u64 {
        self.max_cage_bytes
    }

    /// 返回 cage id 位移。
    pub const fn cage_id_shift(&self) -> u32 {
        self.cage_id_shift
    }

    /// 返回 generation 位移。
    pub const fn cage_generation_shift(&self) -> u32 {
        self.cage_generation_shift
    }

    /// 返回 offset 掩码。
    pub const fn cage_offset_mask(&self) -> u64 {
        self.cage_offset_mask
    }

    /// 返回 generation 掩码。
    pub const fn cage_generation_mask(&self) -> u64 {
        self.cage_generation_mask
    }

    /// 返回空压缩字。
    pub const fn null_word(&self) -> u64 {
        self.null_word
    }

    /// 返回 generation 起点。
    pub const fn generation_min(&self) -> u32 {
        self.generation_min
    }

    /// 返回 generation 上界。
    pub const fn generation_max(&self) -> u32 {
        self.generation_max
    }

    /// 返回目标能力声明。
    pub const fn capability(&self) -> PointerCompression {
        self.capability
    }

    /// 返回上游需求视图。
    pub const fn demand(&self) -> CompressionDemand {
        self.demand
    }

    /// 返回目标可用的 canonical 位宽。
    pub const fn canonical_bits(&self) -> u8 {
        self.capability.canonical_bits
    }

    /// 返回契约指纹。
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 校验契约：位布局、尺寸、profile、开关与需求闭合。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != COMPRESSION_SCHEMA {
            return Err(RawModelError::new("压缩契约 schema 版本不一致"));
        }
        if self.profile != COMPRESSION_PROFILE_NAME
            || self.profile_revision != COMPRESSION_PROFILE_REVISION
        {
            return Err(RawModelError::new(
                "压缩 profile 名或 revision 与登记值不一致",
            ));
        }
        if CAGE_ID_BITS + CAGE_GENERATION_BITS + CAGE_OFFSET_BITS != 64
            || self.cage_id_bits != CAGE_ID_BITS
            || self.cage_generation_bits != CAGE_GENERATION_BITS
            || self.cage_offset_bits != CAGE_OFFSET_BITS
        {
            return Err(RawModelError::new("压缩引用编码位数不等于 64"));
        }
        if self.cage_id_shift != CAGE_GENERATION_BITS + CAGE_OFFSET_BITS
            || self.cage_generation_shift != CAGE_OFFSET_BITS
            || self.cage_offset_mask != CAGE_OFFSET_MASK
            || self.cage_generation_mask != CAGE_GENERATION_MASK
        {
            return Err(RawModelError::new("压缩引用编码位移或掩码与登记值不一致"));
        }
        if self.null_word != CAGE_NULL_WORD
            || self.generation_min != CAGE_GENERATION_MIN
            || self.generation_max != (1 << CAGE_GENERATION_BITS) - 1
        {
            return Err(RawModelError::new(
                "压缩引用空值与 generation 域与登记值不一致",
            ));
        }
        if self.compressed_root_kind != CAGE_COMPRESSED_ROOT_KIND {
            return Err(RawModelError::new("压缩根种类名与登记值不一致"));
        }
        if self.cage_control_fields.len() != CAGE_CONTROL_FIELDS.len()
            || self
                .cage_control_fields
                .iter()
                .zip(CAGE_CONTROL_FIELDS.iter())
                .any(|(field, (name, offset, size))| {
                    field.name != *name || field.offset != *offset || field.size != *size
                })
        {
            return Err(RawModelError::new("cage 控制记录字段表与登记值不一致"));
        }
        if usize::try_from(self.cage_control_bytes).ok()
            != Some(std::mem::size_of::<CageControlRecord>())
            || usize::try_from(self.cage_control_align).ok()
                != Some(std::mem::align_of::<CageControlRecord>())
        {
            return Err(RawModelError::new(
                "cage 控制记录尺寸或对齐与 Rust 布局不一致",
            ));
        }
        // 机器解码序列按 `CAGE_CANONICAL_LIMIT`（48 位正半区上界）判定 canonical；目标声明的
        // canonical 位宽必须与它同源。未登记压缩能力的目标不参与该检查。
        if self.capability.supported {
            let bits = u32::from(self.capability.canonical_bits);
            if bits == 0 || bits > 64 || CAGE_CANONICAL_LIMIT.trailing_zeros() + 1 != bits {
                return Err(RawModelError::new("目标 canonical 位宽与 cage 编码不一致"));
            }
        }
        if self.ffi_rules.len() != CAGE_FFI_RULES.len()
            || self
                .ffi_rules
                .iter()
                .zip(CAGE_FFI_RULES.iter())
                .any(|(rule, expected)| rule != expected)
        {
            return Err(RawModelError::new("FFI 交接规则目录与登记表不一致"));
        }
        if self.statistics.len() != CAGE_STATISTICS.len()
            || self
                .statistics
                .iter()
                .zip(CAGE_STATISTICS.iter())
                .any(|(name, expected)| name != expected)
        {
            return Err(RawModelError::new("压缩统计名目录与登记表不一致"));
        }
        if self.max_cage_bytes != CAGE_MAX_BYTES || self.cage_granule_bytes != CAGE_GRANULE_BYTES {
            return Err(RawModelError::new("cage 上限或粒度与 GC arena 尺寸不同源"));
        }
        if !self.enabled {
            // 解码点是「开关与需求矛盾」的精确信号，先于笼统的关闭态检查报告。
            if self.demand.decode_sites != 0 {
                return Err(RawModelError::new("存在解码点却没有开启 cage profile"));
            }
            if self.cage_bytes != 0 || self.demand.compressed_root_slots != 0 {
                return Err(RawModelError::new(
                    "未启用 cage profile 时不得预留 cage、出现解码点或压缩根",
                ));
            }
        } else {
            if !self.capability.supported {
                return Err(RawModelError::new("目标不支持 checked pointer compression"));
            }
            if self.cage_bytes == 0 || !self.cage_bytes.is_multiple_of(self.cage_granule_bytes) {
                return Err(RawModelError::new("cage 字节数必须是 arena 粒度的整数倍"));
            }
            if self.cage_bytes > self.max_cage_bytes
                || self.cage_bytes > self.capability.max_cage_bytes
            {
                return Err(RawModelError::new("cage 字节数超过目标能力"));
            }
            if self.cage_granule_bytes < self.capability.min_alignment {
                return Err(RawModelError::new("cage 粒度低于目标最小对齐"));
            }
        }
        self.demand.verify()?;
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("压缩契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("压缩契约可序列化")
    }

    /// 压缩契约的域隔离内容身份。
    ///
    /// `canonical_bytes` 是整段 serde_json（含 `fingerprint` 字段），因此这里对指纹字段清零
    /// 的副本求 hash：否则「先写指纹、再校验相等」永远不可能成立。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        let mut canonical = self.clone();
        canonical.fingerprint = [0; 32];
        *blake3::Hasher::new_derive_key("gugu-compression-runtime-v1")
            .update(&canonical.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    /// 返回人类可读的契约 dump。
    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "compression schema={} profile={} revision={} enabled={} cage-bytes={} granule={} max-cage-bytes={} canonical-bits={} capability-supported={} decodes-sites={} compressed-root-slots={} fingerprint={}",
            self.schema,
            self.profile,
            self.profile_revision,
            self.enabled,
            self.cage_bytes,
            self.cage_granule_bytes,
            self.max_cage_bytes,
            self.capability.canonical_bits,
            self.capability.supported,
            self.demand.decode_sites,
            self.demand.compressed_root_slots,
            hex_lower(self.fingerprint)
        );
        let _ = writeln!(
            out,
            "compression-encoding cage-id-bits={} generation-bits={} offset-bits={} id-shift={} generation-shift={}",
            self.cage_id_bits,
            self.cage_generation_bits,
            self.cage_offset_bits,
            self.cage_id_shift,
            self.cage_generation_shift
        );
        let _ = writeln!(out, "compression-ffi-rules {}", self.ffi_rules.join(","));
        let _ = writeln!(out, "compression-statistics {}", self.statistics.join(","));
        let _ = writeln!(
            out,
            "compression-cage-control bytes={} align={} fields={}",
            self.cage_control_bytes,
            self.cage_control_align,
            self.cage_control_fields
                .iter()
                .map(|field| format!("{}@{}:{}", field.name, field.offset, field.size))
                .collect::<Vec<_>>()
                .join(",")
        );
        out
    }
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
