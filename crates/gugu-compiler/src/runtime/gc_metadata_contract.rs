//! GC metadata runtime 契约段：与栈图契约段同源，把类型表大小、trace/value program
//! 字节数、根/vtable/source/alloc 计数与 arena 布局汇总为一个 fingerprint 进入
//! `RuntimeRawContractV1`。
//!
//! 真实类型表、program 与 type/meta section 字节由前端生成，经
//! `RuntimeRawModel` 验证后进入契约和 `ImagePlan`；本段同时保留 demand 视图与识别常量。

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::gc_metadata_schema::GcMetadataDemand;
use super::model::RawModelError;

/// GC metadata 契约段 schema 版本。
pub(crate) const GC_METADATA_SCHEMA: u32 = 1;
/// GC metadata section 主版本。
pub(crate) const GC_METADATA_SECTION_VERSION: u16 = 1;
/// GC metadata section 魔数。
pub(crate) const GC_METADATA_MAGIC: &[u8; 8] = b"GUGUGC01";

/// GC arena 布局常量：2 MiB arena、32 KiB block、128 byte line；与 slab/size class
/// 对齐，运行时分配按此颗粒。
pub(crate) const GC_ARENA_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const GC_BLOCK_BYTES: u32 = 32 * 1024;
pub(crate) const GC_LINE_BYTES: u32 = 128;

/// 栈图 flags 位：bit 0 `HAS_HEAP_DIRECT`、bit 1 `HAS_HEAP_INTERIOR`、
/// bit 2 `HAS_VALUE_ACTIONS`、bit 3 `HAS_RESOURCE`、
/// bit 4 `HAS_DEFERRED_RELEASE`、bit 5 `UNSIZED_VIEW`、
/// bit 6 `VARIABLE_SIZE`、bit 7 `PIN_SENSITIVE`（预留）。
pub(crate) const GC_TYPE_FLAG_NAMES: [&str; 8] = [
    "has-heap-direct",
    "has-heap-interior",
    "has-value-actions",
    "has-resource",
    "has-deferred-release",
    "unsized-view",
    "variable-size",
    "pin-sensitive",
];

/// Trace program op 名称（顺序即数值）。
pub(crate) const TRACE_OP_NAMES: [&str; 5] = [
    "end",      // 0x00
    "direct",   // 0x01
    "interior", // 0x02
    "repeat",   // 0x03
    "switch",   // 0x04
];

/// Value program op 名称（按 ABI 操作码顺序登记）。
pub(crate) const VALUE_OP_NAMES: [&str; 7] = [
    "end",              // 0x00
    "aggregate",        // 0x10
    "repeat-value",     // 0x11
    "switch-value",     // 0x12
    "cow-publish",      // 0x13
    "acquire-resource", // 0x14
    "release-resource", // 0x15
];

/// 已验证的 GC metadata runtime 契约。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct GcMetadataRuntimeContract {
    pub schema: u32,
    pub section_version: u16,
    pub magic: String,
    pub arena_bytes: u64,
    pub block_bytes: u32,
    pub line_bytes: u32,
    pub type_flag_names: Vec<String>,
    pub trace_op_names: Vec<String>,
    pub value_op_names: Vec<String>,
    pub demand: GcMetadataDemand,
    /// 已编码并经过 verifier 的真实镜像 section。
    #[serde(default)]
    pub type_section: Vec<u8>,
    #[serde(default)]
    pub metadata_section: Vec<u8>,
    pub fingerprint: [u8; 32],
}

impl GcMetadataRuntimeContract {
    pub(crate) fn build(demand: GcMetadataDemand) -> Result<Self, RawModelError> {
        if demand.arena_bytes != 0
            && (demand.arena_bytes != GC_ARENA_BYTES
                || demand.block_bytes != GC_BLOCK_BYTES
                || demand.line_bytes != GC_LINE_BYTES)
        {
            return Err(RawModelError::new("GC arena/block/line 与契约常量不一致"));
        }
        let mut contract = Self {
            schema: GC_METADATA_SCHEMA,
            section_version: GC_METADATA_SECTION_VERSION,
            magic: std::str::from_utf8(GC_METADATA_MAGIC)
                .expect("GC metadata magic 是合法 UTF-8")
                .to_owned(),
            arena_bytes: GC_ARENA_BYTES,
            block_bytes: GC_BLOCK_BYTES,
            line_bytes: GC_LINE_BYTES,
            type_flag_names: GC_TYPE_FLAG_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            trace_op_names: TRACE_OP_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            value_op_names: VALUE_OP_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            demand,
            type_section: Vec::new(),
            metadata_section: Vec::new(),
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    pub(crate) fn with_sections(
        mut self,
        type_section: Vec<u8>,
        metadata_section: Vec<u8>,
    ) -> Result<Self, RawModelError> {
        self.type_section = type_section;
        self.metadata_section = metadata_section;
        self.fingerprint = self.compute_fingerprint();
        self.verify()?;
        Ok(self)
    }

    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != GC_METADATA_SCHEMA {
            return Err(RawModelError::new("GC metadata 契约 schema 不匹配"));
        }
        if self.section_version != GC_METADATA_SECTION_VERSION {
            return Err(RawModelError::new("GC metadata section 版本不匹配"));
        }
        if self.magic.as_bytes() != GC_METADATA_MAGIC {
            return Err(RawModelError::new("GC metadata magic 不匹配"));
        }
        if self.arena_bytes != GC_ARENA_BYTES
            || self.block_bytes != GC_BLOCK_BYTES
            || self.line_bytes != GC_LINE_BYTES
        {
            return Err(RawModelError::new("GC arena 布局与契约常量不一致"));
        }
        if self.type_flag_names.len() != GC_TYPE_FLAG_NAMES.len()
            || self.trace_op_names.len() != TRACE_OP_NAMES.len()
            || self.value_op_names.len() != VALUE_OP_NAMES.len()
        {
            return Err(RawModelError::new("GC metadata 名称表长度与登记不一致"));
        }
        for (index, name) in self.type_flag_names.iter().enumerate() {
            if name != GC_TYPE_FLAG_NAMES[index] {
                return Err(RawModelError::new("GC type flag 名称与登记不一致"));
            }
        }
        for (index, name) in self.trace_op_names.iter().enumerate() {
            if name != TRACE_OP_NAMES[index] {
                return Err(RawModelError::new("GC trace op 名称与登记不一致"));
            }
        }
        for (index, name) in self.value_op_names.iter().enumerate() {
            if name != VALUE_OP_NAMES[index] {
                return Err(RawModelError::new("GC value op 名称与登记不一致"));
            }
        }
        if !self.type_section.is_empty() || !self.metadata_section.is_empty() {
            crate::runtime::gc_metadata_section::verify_sections(
                &self.type_section,
                &self.metadata_section,
            )?;
            if self.demand.type_section_bytes != 0
                && self.demand.type_section_bytes as usize != self.type_section.len()
            {
                return Err(RawModelError::new("GC type section 长度与需求不一致"));
            }
            if self.demand.metadata_section_bytes != 0
                && self.demand.metadata_section_bytes as usize != self.metadata_section.len()
            {
                return Err(RawModelError::new("GC metadata section 长度与需求不一致"));
            }
        }
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("GC metadata 契约指纹与内容不一致"));
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&self.section_version.to_le_bytes());
        bytes.extend_from_slice(self.magic.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&self.arena_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.block_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.line_bytes.to_le_bytes());
        for name in &self.type_flag_names {
            bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
        }
        for name in &self.trace_op_names {
            bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
        }
        for name in &self.value_op_names {
            bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
        }
        // demand 身份只有一处定义：契约直接并入 `GcMetadataDemand` 的规范指纹，
        // 避免同一组计数在两处各自序列化而产生漂移。
        bytes.extend_from_slice(&self.demand.fingerprint());
        bytes.extend_from_slice(&(self.type_section.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&self.type_section);
        bytes.extend_from_slice(&(self.metadata_section.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&self.metadata_section);
        bytes
    }

    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        crate::frontend::mono::keys::hash_domain(
            "gugu-gc-metadata-contract-v1",
            &self.canonical_bytes(),
        )
    }

    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "gc-metadata schema={} magic={} arena={} block={} line={}",
            self.schema, self.magic, self.arena_bytes, self.block_bytes, self.line_bytes
        );
        let _ = writeln!(
            out,
            "gc-metadata-types types={} trace_bytes={} value_bytes={} vtables={} glue={} roots={} sources={} alloc_sites={}",
            self.demand.type_count,
            self.demand.trace_program_bytes,
            self.demand.value_program_bytes,
            self.demand.vtable_count,
            self.demand.glue_count,
            self.demand.root_range_count,
            self.demand.source_count,
            self.demand.alloc_site_count
        );
        let _ = writeln!(
            out,
            "gc-metadata-sections type_bytes={} metadata_bytes={} type_fingerprint={} metadata_fingerprint={}",
            self.type_section.len(),
            self.metadata_section.len(),
            hex_lower(crate::frontend::mono::keys::hash_domain(
                "gugu-gc-type-section-v1",
                &self.type_section
            )),
            hex_lower(crate::frontend::mono::keys::hash_domain(
                "gugu-gc-metadata-section-v1",
                &self.metadata_section
            )),
        );
        let _ = writeln!(
            out,
            "gc-metadata-fingerprint {}",
            hex_lower(self.fingerprint)
        );
        out
    }
}

fn hex_lower(bytes: [u8; 32]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}
