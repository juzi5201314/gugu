//! 资源平面的契约 schema：cell header、状态位与迁移表、release 描述符与资源种类目录。
//!
//! 这些类型把 ResourceCell 的 64-byte header、五个状态位、受限 cleanup 的字段集合、统一
//! release 入口与资源需求视图固定成可缓存、可校验的契约；state machine 的参照实现见
//! \`resource\` 模块。

use serde::{Deserialize, Serialize};

use super::model::{FieldKind, MessageFieldSchema, RESOURCE_SCHEMA, RawModelError};
use super::resource::{
    self, CELL_HEADER_LAYOUT, CELL_TRANSITIONS, RESOURCE_KINDS, UNIFIED_RELEASE_ENTRY,
};

/// ResourceCell header 字段的种类。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum CellFieldKind {
    /// lease 计数。
    LeaseCount,
    /// 状态位集合。
    StateBits,
    /// raw payload 字节数。
    PayloadSize,
    /// 创建或发布后的 owner 协程。
    OwnerCoroutine,
    /// release glue 登记编号。
    ReleaseGlue,
    /// release descriptor 稠密编号。
    ReleaseDescriptor,
    /// slab class 编号。
    SlabClass,
    /// payload 对齐指数。
    PayloadAlign,
    /// detach 等标志位。
    Flags,
    /// free index 或复用链。
    FreeLink,
    /// 回收 generation。
    Generation,
    /// 保留字段，必须为 0。
    Reserved,
}

impl CellFieldKind {
    /// 返回种类名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::LeaseCount => "lease-count",
            Self::StateBits => "state-bits",
            Self::PayloadSize => "payload-size",
            Self::OwnerCoroutine => "owner-coroutine",
            Self::ReleaseGlue => "release-glue",
            Self::ReleaseDescriptor => "release-descriptor",
            Self::SlabClass => "slab-class",
            Self::PayloadAlign => "payload-align",
            Self::Flags => "flags",
            Self::FreeLink => "free-link",
            Self::Generation => "generation",
            Self::Reserved => "reserved",
        }
    }
}

/// 按字段名返回 header 字段种类。
fn cell_field_kind(name: &str) -> Option<CellFieldKind> {
    Some(match name {
        "leases" => CellFieldKind::LeaseCount,
        "state" => CellFieldKind::StateBits,
        "payload_size" => CellFieldKind::PayloadSize,
        "owner_coroutine" => CellFieldKind::OwnerCoroutine,
        "release_glue" => CellFieldKind::ReleaseGlue,
        "release_descriptor_id" => CellFieldKind::ReleaseDescriptor,
        "slab_class" => CellFieldKind::SlabClass,
        "payload_align_log2" => CellFieldKind::PayloadAlign,
        "flags" => CellFieldKind::Flags,
        "next_free" => CellFieldKind::FreeLink,
        "generation" => CellFieldKind::Generation,
        "reserved" => CellFieldKind::Reserved,
        _ => return None,
    })
}

/// 一个 ResourceCell header 字段。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct CellFieldSchema {
    pub(crate) name: String,
    pub(crate) offset: u32,
    pub(crate) bytes: u32,
    pub(crate) kind: CellFieldKind,
}

/// ResourceCell 的 64-byte header schema。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct CellHeaderSchemaV1 {
    pub(crate) schema: u32,
    pub(crate) header_bytes: u32,
    pub(crate) fields: Vec<CellFieldSchema>,
}

impl CellHeaderSchemaV1 {
    /// 由参照实现的规范字段表构造。
    pub(crate) fn fixed() -> Self {
        let fields = CELL_HEADER_LAYOUT
            .iter()
            .map(|field| CellFieldSchema {
                name: field.name.to_owned(),
                offset: field.offset,
                bytes: field.bytes,
                kind: cell_field_kind(field.name).expect("header 字段必须登记种类"),
            })
            .collect();
        Self {
            schema: RESOURCE_SCHEMA,
            header_bytes: resource::CELL_HEADER_BYTES,
            fields,
        }
    }

    /// 校验字段连续、宽度非零、合计等于 header 字节数。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != RESOURCE_SCHEMA {
            return Err(RawModelError::new("ResourceCell header schema 版本不匹配"));
        }
        if self.header_bytes != resource::CELL_HEADER_BYTES {
            return Err(RawModelError::new("ResourceCell header 字节数与契约不一致"));
        }
        let mut offset = 0_u32;
        for field in &self.fields {
            if field.offset != offset || field.bytes == 0 {
                return Err(RawModelError::new(format!(
                    "ResourceCell header 字段 {} 的偏移或宽度不连续",
                    field.name
                )));
            }
            offset += field.bytes;
        }
        if offset != self.header_bytes {
            return Err(RawModelError::new(
                "ResourceCell header 字段合计与 header 字节数不一致",
            ));
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + self.fields.len() * 24);
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&self.header_bytes.to_le_bytes());
        for field in &self.fields {
            bytes.extend_from_slice(field.name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&field.offset.to_le_bytes());
            bytes.extend_from_slice(&field.bytes.to_le_bytes());
            bytes.push(field.kind as u8);
        }
        bytes
    }
}

/// 一个状态位登记项。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct StateBitSchema {
    pub(crate) name: String,
    pub(crate) bit: u32,
}

/// 一项状态迁移。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct TransitionSchema {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) trigger: String,
}

/// ResourceCell 的状态位与迁移 schema。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResourceStateSchemaV1 {
    pub(crate) schema: u32,
    pub(crate) bits: Vec<StateBitSchema>,
    pub(crate) transitions: Vec<TransitionSchema>,
}

impl ResourceStateSchemaV1 {
    /// 由参照实现的状态位与迁移表构造。
    pub(crate) fn fixed() -> Self {
        let bits = [
            ("shared", resource::STATE_SHARED),
            ("closed", resource::STATE_CLOSED),
            ("release-queued", resource::STATE_RELEASE_QUEUED),
            ("release-done", resource::STATE_RELEASE_DONE),
            ("reclaiming", resource::STATE_RECLAIMING),
        ]
        .into_iter()
        .map(|(name, bit)| StateBitSchema {
            name: name.to_owned(),
            bit,
        })
        .collect();
        let transitions = CELL_TRANSITIONS
            .iter()
            .map(|transition| TransitionSchema {
                from: transition.from.to_owned(),
                to: transition.to.to_owned(),
                trigger: transition.trigger.to_owned(),
            })
            .collect();
        Self {
            schema: RESOURCE_SCHEMA,
            bits,
            transitions,
        }
    }

    /// 校验状态位稠密且迁移表非空。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != RESOURCE_SCHEMA {
            return Err(RawModelError::new("ResourceCell 状态 schema 版本不匹配"));
        }
        let expected = [
            resource::STATE_SHARED,
            resource::STATE_CLOSED,
            resource::STATE_RELEASE_QUEUED,
            resource::STATE_RELEASE_DONE,
            resource::STATE_RECLAIMING,
        ];
        if self.bits.len() != expected.len()
            || self
                .bits
                .iter()
                .zip(expected)
                .any(|(entry, bit)| entry.bit != bit)
        {
            return Err(RawModelError::new("ResourceCell 状态位集合与契约不一致"));
        }
        if self.transitions.len() != CELL_TRANSITIONS.len() {
            return Err(RawModelError::new("ResourceCell 状态迁移数量与契约不一致"));
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        for bit in &self.bits {
            bytes.extend_from_slice(bit.name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&bit.bit.to_le_bytes());
        }
        for transition in &self.transitions {
            for part in [&transition.from, &transition.to, &transition.trigger] {
                bytes.extend_from_slice(part.as_bytes());
                bytes.push(0);
            }
        }
        bytes
    }
}

/// release 描述符的字段 schema；只允许标量、glue 与稳定描述符。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReleaseDescriptorSchemaV1 {
    pub(crate) schema: u32,
    pub(crate) fields: Vec<MessageFieldSchema>,
}

impl ReleaseDescriptorSchemaV1 {
    /// 返回受限 cleanup 的规范字段集合。
    pub(crate) fn fixed() -> Self {
        Self {
            schema: RESOURCE_SCHEMA,
            fields: vec![
                MessageFieldSchema::new("flags", FieldKind::Flags),
                MessageFieldSchema::new("kind", FieldKind::KindTag),
                MessageFieldSchema::new("payload_align_log2", FieldKind::PayloadAlign),
                MessageFieldSchema::new("payload_size", FieldKind::PayloadSize),
                MessageFieldSchema::new("release_descriptor_id", FieldKind::ReleaseDescriptor),
                MessageFieldSchema::new("release_glue", FieldKind::ReleaseGlue),
            ],
        }
    }

    /// 校验字段无地址、覆盖必需身份且按名字稳定排序。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != RESOURCE_SCHEMA {
            return Err(RawModelError::new("release 描述符 schema 版本不匹配"));
        }
        for field in &self.fields {
            if field.kind.carries_address() {
                return Err(RawModelError::new(format!(
                    "release 描述符字段 {} 携带地址，违反受限 cleanup 契约",
                    field.name
                )));
            }
        }
        for required in [
            FieldKind::ReleaseGlue,
            FieldKind::ReleaseDescriptor,
            FieldKind::KindTag,
            FieldKind::Flags,
        ] {
            if !self.fields.iter().any(|field| field.kind == required) {
                return Err(RawModelError::new(format!(
                    "release 描述符 schema 缺少必需字段 {}",
                    required.name()
                )));
            }
        }
        for pair in self.fields.windows(2) {
            if pair[0].name >= pair[1].name {
                return Err(RawModelError::new("release 描述符字段没有按名字稳定排序"));
            }
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(4 + self.fields.len() * 24);
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        for field in &self.fields {
            bytes.extend_from_slice(field.name.as_bytes());
            bytes.push(0);
            bytes.push(field.kind as u8);
        }
        bytes
    }
}

/// 一个资源种类登记项。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResourceKindSchema {
    pub(crate) id: u8,
    pub(crate) name: String,
    pub(crate) release_entry: String,
    pub(crate) close_idempotent: bool,
}

/// File/socket/process/lock/FFI 的统一 release 入口目录。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResourceKindCatalogV1 {
    pub(crate) schema: u32,
    pub(crate) release_entry: String,
    pub(crate) kinds: Vec<ResourceKindSchema>,
}

impl ResourceKindCatalogV1 {
    /// 由参照实现登记的资源种类构造。
    pub(crate) fn fixed() -> Self {
        let kinds = RESOURCE_KINDS
            .iter()
            .map(|entry| ResourceKindSchema {
                id: entry.id,
                name: entry.name.to_owned(),
                release_entry: entry.release_entry.to_owned(),
                close_idempotent: entry.close_idempotent,
            })
            .collect();
        Self {
            schema: RESOURCE_SCHEMA,
            release_entry: UNIFIED_RELEASE_ENTRY.to_owned(),
            kinds,
        }
    }

    /// 校验编号稠密、入口唯一且 close 幂等。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != RESOURCE_SCHEMA {
            return Err(RawModelError::new("资源种类 schema 版本不匹配"));
        }
        if self.release_entry != UNIFIED_RELEASE_ENTRY {
            return Err(RawModelError::new("资源种类没有登记统一 release 入口"));
        }
        if self.kinds.len() != RESOURCE_KINDS.len() {
            return Err(RawModelError::new("资源种类数量与契约不一致"));
        }
        for (index, kind) in self.kinds.iter().enumerate() {
            if usize::from(kind.id) != index {
                return Err(RawModelError::new("资源种类编号不稠密"));
            }
            if kind.release_entry != self.release_entry {
                return Err(RawModelError::new(format!(
                    "资源种类 {} 走了非统一 release 入口",
                    kind.name
                )));
            }
            if !kind.close_idempotent {
                return Err(RawModelError::new(format!(
                    "资源种类 {} 的 close 必须幂等",
                    kind.name
                )));
            }
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(self.release_entry.as_bytes());
        bytes.push(0);
        for kind in &self.kinds {
            bytes.push(kind.id);
            bytes.extend_from_slice(kind.name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(kind.release_entry.as_bytes());
            bytes.push(0);
            bytes.push(u8::from(kind.close_idempotent));
        }
        bytes
    }
}

/// 资源平面的完整契约段。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResourceSchemaV1 {
    pub(crate) schema: u32,
    pub(crate) header: CellHeaderSchemaV1,
    pub(crate) states: ResourceStateSchemaV1,
    pub(crate) release: ReleaseDescriptorSchemaV1,
    pub(crate) kinds: ResourceKindCatalogV1,
    pub(crate) release_glue_count: u32,
    pub(crate) dedicated_align_limit: u32,
}

impl ResourceSchemaV1 {
    /// 构建资源契约段。
    pub(crate) fn fixed() -> Self {
        Self {
            schema: RESOURCE_SCHEMA,
            header: CellHeaderSchemaV1::fixed(),
            states: ResourceStateSchemaV1::fixed(),
            release: ReleaseDescriptorSchemaV1::fixed(),
            kinds: ResourceKindCatalogV1::fixed(),
            release_glue_count: RESOURCE_KINDS.len() as u32,
            dedicated_align_limit: super::RESOURCE_DEDICATED_ALIGN_LIMIT,
        }
    }

    /// 校验各子 schema 与 glue 数量。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != RESOURCE_SCHEMA {
            return Err(RawModelError::new("资源契约段 schema 版本不匹配"));
        }
        self.header.verify()?;
        self.states.verify()?;
        self.release.verify()?;
        self.kinds.verify()?;
        if self.release_glue_count != RESOURCE_KINDS.len() as u32 {
            return Err(RawModelError::new("release glue 数量与契约不一致"));
        }
        if self.dedicated_align_limit != super::RESOURCE_DEDICATED_ALIGN_LIMIT {
            return Err(RawModelError::new(
                "专用整页 mapping 的对齐上界与契约不一致",
            ));
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&self.header.canonical_bytes());
        bytes.extend_from_slice(&self.states.canonical_bytes());
        bytes.extend_from_slice(&self.release.canonical_bytes());
        bytes.extend_from_slice(&self.kinds.canonical_bytes());
        bytes.extend_from_slice(&self.release_glue_count.to_le_bytes());
        bytes.extend_from_slice(&self.dedicated_align_limit.to_le_bytes());
        bytes
    }
}

/// 由编译产物推导出的资源平面需求视图。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RawResourceDemand {
    /// placement 判定的 resource 分配点数量。
    pub(crate) resource_sites: u32,
    /// lease 复制动作数量。
    pub(crate) acquire_sites: u32,
    /// lease 结束动作数量。
    pub(crate) release_sites: u32,
    /// 跨 owner transfer 动作数量。
    pub(crate) transfer_sites: u32,
    /// finalize 动作数量。
    pub(crate) finalize_sites: u32,
    /// owner 数量。
    pub(crate) owners: u32,
    /// 登记的资源种类数量；由契约固定。
    pub(crate) kinds: u32,
}
