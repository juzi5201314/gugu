//! TurnRegion 私有区、export summary 与 `RegionTransfer` 的契约段。
//!
//! 本段把「owner-local bump region」「export summary 门禁」「region 状态机」「`RegionTransfer`
//! 消息字段」固定成带版本的对象，与 `barrier_schema`/`gc_metadata_contract`/`pacing_schema`
//! 共用 `build`/`verify`/`canonical_bytes`/`fingerprint`/`dump` 闭环。
//!
//! 参数是编译期实现门禁，不是用户可观察的时序或地址：契约只登记容量 class、对象上界、状态名
//! 与位掩码，不登记宿主地址、线程数或回收时刻。region 的容量上界与 Immix block 同源，因此
//! 一个 region 的 payload 永远不超过单个 managed block。

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::barrier_schema::MessageFamilyTag;
use super::gc_metadata_contract::GC_BLOCK_BYTES;
use super::model::{FieldKind, MessageFieldSchema, MessageSchemaV1, RawModelError};

/// region 契约段的 schema 版本。
pub(crate) const REGION_SCHEMA: u32 = 1;

/// 一个 region 允许容纳的对象数上界。
pub(crate) const REGION_OBJECT_LIMIT: u32 = 64;
/// 单个 owner 同时挂起的 region 数上界。
pub(crate) const REGION_MAX_ACTIVE: u32 = 64;
/// 一个 region 同时允许的 transfer lease 数上界。
pub(crate) const REGION_TRANSFER_LEASE_LIMIT: u32 = 1;
/// region 容量 class 阶梯（字节），上界与 managed block 同源。
pub(crate) const REGION_CAPACITY_CLASSES: [u32; 10] =
    [64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32 * 1024];
/// export summary 位名；顺序即编码顺序，全部为 0 表示 summary 闭合。
pub(crate) const REGION_EXPORT_BITS: [&str; 5] = [
    "external-alias",
    "resource-lease",
    "ffi-address",
    "pending-transfer",
    "live-root",
];
/// region 状态名；顺序即状态机强度。
pub(crate) const REGION_STATE_NAMES: [&str; 7] = [
    "private",
    "publishing",
    "reset-pending",
    "reset",
    "local-promote",
    "region-transfer",
    "received",
];

/// export summary 的一位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegionExport {
    ExternalAlias,
    ResourceLease,
    FfiAddress,
    PendingTransfer,
    LiveRoot,
}

impl RegionExport {
    /// 全部位的登记顺序。
    pub(crate) const ALL: [Self; 5] = [
        Self::ExternalAlias,
        Self::ResourceLease,
        Self::FfiAddress,
        Self::PendingTransfer,
        Self::LiveRoot,
    ];

    /// 返回位掩码。
    pub(crate) const fn bit(self) -> u8 {
        1 << (self as u8)
    }

    /// 返回位名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ExternalAlias => REGION_EXPORT_BITS[0],
            Self::ResourceLease => REGION_EXPORT_BITS[1],
            Self::FfiAddress => REGION_EXPORT_BITS[2],
            Self::PendingTransfer => REGION_EXPORT_BITS[3],
            Self::LiveRoot => REGION_EXPORT_BITS[4],
        }
    }
}

/// 全部 export summary 位；由位目录本身推导，避免出现两处独立的掩码。
pub(crate) const REGION_EXPORT_ALL: u8 = {
    let mut mask = 0;
    let mut index = 0;
    while index < RegionExport::ALL.len() {
        mask |= RegionExport::ALL[index].bit();
        index += 1;
    }
    mask
};

/// `RegionTransfer` 的消息字段集合：只允许稳定 region 身份、generation、type summary、
/// bytes、export state 与目标 owner 身份，任何地址字段都在 verifier 中被拒绝。
pub(crate) fn region_transfer_fields() -> Vec<MessageFieldSchema> {
    let mut fields = vec![
        MessageFieldSchema::new("bytes", FieldKind::Bytes),
        MessageFieldSchema::new("cycle_epoch", FieldKind::Epoch),
        MessageFieldSchema::new("export_state", FieldKind::ExportSummary),
        MessageFieldSchema::new("family", FieldKind::KindTag),
        MessageFieldSchema::new("integrity", FieldKind::Integrity),
        MessageFieldSchema::new("region", FieldKind::UnitIndex),
        MessageFieldSchema::new("region_generation", FieldKind::Generation),
        MessageFieldSchema::new("state", FieldKind::MessageState),
        MessageFieldSchema::new("target.domain", FieldKind::OwnerDomain),
        MessageFieldSchema::new("target.generation", FieldKind::Generation),
        MessageFieldSchema::new("target.owner_id", FieldKind::OwnerId),
        MessageFieldSchema::new("target.route_key", FieldKind::RouteKey),
        MessageFieldSchema::new("type_summary", FieldKind::DescriptorIndex),
    ];
    fields.sort_by(|left, right| left.name.cmp(&right.name));
    fields
}

/// 从优化后 LIR 推导的 TurnRegion 需求，不是运行时计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TurnRegionDemand {
    /// 已建立计划的 region 数量。
    pub regions: u32,
    /// `RegionAlloc` 站点数。
    pub allocations: u32,
    /// `RegionPublish` 站点数。
    pub publish_sites: u32,
    /// `RegionReset` 站点数。
    pub reset_sites: u32,
    /// `PromoteManaged` 站点数。
    pub promote_sites: u32,
    /// `RegionTransfer` 站点数。
    pub transfer_sites: u32,
    /// 需要的容量 class 数量上界。
    pub capacity_classes: u32,
    /// 全部 region 的 payload 字节总和。
    pub total_bytes: u64,
    /// 单个 region 的最大 payload 字节。
    pub max_region_bytes: u64,
}

/// 已验证的 TurnRegion runtime 契约；版本变化使 RuntimeRawModel 与 action key 失效。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TurnRegionRuntimeContract {
    /// 内部 schema 版本。
    pub schema: u32,
    /// 单 region 对象数上界。
    pub object_limit: u32,
    /// 单 owner 活跃 region 数上界。
    pub max_active_regions: u32,
    /// 单 region 的 transfer lease 上界。
    pub transfer_lease_limit: u32,
    /// 容量 class 阶梯（字节）。
    pub capacity_class_bytes: Vec<u32>,
    /// export summary 位名目录。
    pub export_bits: Vec<String>,
    /// region 状态名目录。
    pub states: Vec<String>,
    /// `RegionTransfer` 的字段目录。
    pub(crate) transfer_fields: Vec<MessageFieldSchema>,
    /// 上游需求视图。
    pub demand: TurnRegionDemand,
    /// 契约规范化指纹。
    pub fingerprint: [u8; 32],
}

impl TurnRegionRuntimeContract {
    /// 返回内部 schema 版本。
    pub fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回单 region 对象数上界。
    pub const fn object_limit(&self) -> u32 {
        self.object_limit
    }

    /// 返回单 owner 活跃 region 数上界。
    pub const fn max_active_regions(&self) -> u32 {
        self.max_active_regions
    }

    /// 返回 transfer lease 上界。
    pub const fn transfer_lease_limit(&self) -> u32 {
        self.transfer_lease_limit
    }

    /// 返回容量 class 阶梯。
    pub(crate) fn capacity_class_bytes(&self) -> &[u32] {
        &self.capacity_class_bytes
    }

    /// 返回容量 class 数量。
    pub fn capacity_class_count(&self) -> u32 {
        self.capacity_class_bytes.len() as u32
    }

    /// 返回 export summary 位数量。
    pub fn export_bit_count(&self) -> u32 {
        self.export_bits.len() as u32
    }

    /// 返回 `RegionTransfer` 字段数量。
    pub fn transfer_field_count(&self) -> u32 {
        self.transfer_fields.len() as u32
    }

    /// 返回上游需求视图。
    pub const fn demand(&self) -> TurnRegionDemand {
        self.demand
    }

    /// 返回契约指纹。
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 由上游需求构建契约；参数是固定登记值。
    pub(crate) fn build(demand: TurnRegionDemand) -> Result<Self, RawModelError> {
        let mut contract = Self {
            schema: REGION_SCHEMA,
            object_limit: REGION_OBJECT_LIMIT,
            max_active_regions: REGION_MAX_ACTIVE,
            transfer_lease_limit: REGION_TRANSFER_LEASE_LIMIT,
            capacity_class_bytes: REGION_CAPACITY_CLASSES.to_vec(),
            export_bits: RegionExport::ALL
                .iter()
                .map(|export| export.name().to_owned())
                .collect(),
            states: REGION_STATE_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            transfer_fields: region_transfer_fields(),
            demand,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 校验契约：阶梯、目录、跨契约同源、需求上界与指纹。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != REGION_SCHEMA {
            return Err(RawModelError::new("TurnRegion 契约 schema 不匹配"));
        }
        if self.object_limit == 0 || self.object_limit > REGION_OBJECT_LIMIT {
            return Err(RawModelError::new("region 对象数上界超出登记值"));
        }
        if self.max_active_regions == 0 || self.max_active_regions > REGION_MAX_ACTIVE {
            return Err(RawModelError::new("单 owner 活跃 region 上界超出登记值"));
        }
        if self.transfer_lease_limit != REGION_TRANSFER_LEASE_LIMIT {
            return Err(RawModelError::new(
                "region transfer lease 上界与登记值不一致",
            ));
        }
        if self.capacity_class_bytes != REGION_CAPACITY_CLASSES {
            return Err(RawModelError::new("region 容量 class 阶梯与登记值不一致"));
        }
        let top = *self
            .capacity_class_bytes
            .last()
            .ok_or_else(|| RawModelError::new("region 容量 class 阶梯不能为空"))?;
        if u64::from(top) > u64::from(GC_BLOCK_BYTES) {
            return Err(RawModelError::new(
                "region 容量上界不得超过单个 managed block",
            ));
        }
        if self
            .capacity_class_bytes
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(RawModelError::new("region 容量 class 阶梯必须严格递增"));
        }
        let names: Vec<&str> = self.export_bits.iter().map(String::as_str).collect();
        if names != REGION_EXPORT_BITS {
            return Err(RawModelError::new(
                "region export summary 位目录与登记值不一致",
            ));
        }
        let states: Vec<&str> = self.states.iter().map(String::as_str).collect();
        if states != REGION_STATE_NAMES {
            return Err(RawModelError::new("region 状态目录与登记值不一致"));
        }
        if self.transfer_fields != region_transfer_fields() {
            return Err(RawModelError::new("RegionTransfer 字段目录与登记值不一致"));
        }
        MessageSchemaV1 {
            schema: 1,
            family: MessageFamilyTag::RegionTransfer,
            fields: self.transfer_fields.clone(),
        }
        .verify()?;
        if self.demand.max_region_bytes > u64::from(top) {
            return Err(RawModelError::new(
                "region 需求超过容量阶梯上界，无法预分配",
            ));
        }
        if self.demand.capacity_classes as usize > self.capacity_class_bytes.len() {
            return Err(RawModelError::new("region 容量 class 需求超过阶梯长度"));
        }
        if self.demand.allocations < self.demand.regions {
            return Err(RawModelError::new("region 站点数不得少于 region 数量"));
        }
        if self.demand.reset_sites + self.demand.transfer_sites + self.demand.promote_sites
            > self.demand.projects()
        {
            return Err(RawModelError::new("region 结束站点超过发布站点上界"));
        }
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("TurnRegion 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 返回契约的规范字节；字段顺序即编码顺序。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&self.object_limit.to_le_bytes());
        bytes.extend_from_slice(&self.max_active_regions.to_le_bytes());
        bytes.extend_from_slice(&self.transfer_lease_limit.to_le_bytes());
        bytes.extend_from_slice(&(self.capacity_class_bytes.len() as u32).to_le_bytes());
        for class in &self.capacity_class_bytes {
            bytes.extend_from_slice(&class.to_le_bytes());
        }
        push_texts(&mut bytes, &self.export_bits);
        push_texts(&mut bytes, &self.states);
        bytes.extend_from_slice(&(self.transfer_fields.len() as u32).to_le_bytes());
        for field in &self.transfer_fields {
            push_text(&mut bytes, &field.name);
            bytes.push(field.kind as u8);
        }
        bytes.extend_from_slice(&self.demand.regions.to_le_bytes());
        bytes.extend_from_slice(&self.demand.allocations.to_le_bytes());
        bytes.extend_from_slice(&self.demand.publish_sites.to_le_bytes());
        bytes.extend_from_slice(&self.demand.reset_sites.to_le_bytes());
        bytes.extend_from_slice(&self.demand.promote_sites.to_le_bytes());
        bytes.extend_from_slice(&self.demand.transfer_sites.to_le_bytes());
        bytes.extend_from_slice(&self.demand.capacity_classes.to_le_bytes());
        bytes.extend_from_slice(&self.demand.total_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.demand.max_region_bytes.to_le_bytes());
        bytes
    }

    fn compute_fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-turn-region-runtime-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    /// 返回稳定文本 dump；不含地址、宿主路径与线程编号。
    pub(crate) fn dump(&self) -> String {
        let mut output = String::new();
        writeln!(
            output,
            "region schema={} object-limit={} max-active={} transfer-lease={}",
            self.schema, self.object_limit, self.max_active_regions, self.transfer_lease_limit
        )
        .expect("String写入");
        let classes: Vec<String> = self
            .capacity_class_bytes
            .iter()
            .map(u32::to_string)
            .collect();
        writeln!(output, "region-capacity-classes {}", classes.join(",")).expect("String写入");
        writeln!(output, "region-export-bits {}", self.export_bits.join(",")).expect("String写入");
        writeln!(output, "region-states {}", self.states.join(",")).expect("String写入");
        let fields: Vec<String> = self
            .transfer_fields
            .iter()
            .map(|field| field.name.clone())
            .collect();
        writeln!(output, "region-transfer-fields {}", fields.join(",")).expect("String写入");
        writeln!(
            output,
            "region-demand regions={} allocations={} publish={} reset={} promote={} transfer={} classes={} total-bytes={} max-bytes={}",
            self.demand.regions,
            self.demand.allocations,
            self.demand.publish_sites,
            self.demand.reset_sites,
            self.demand.promote_sites,
            self.demand.transfer_sites,
            self.demand.capacity_classes,
            self.demand.total_bytes,
            self.demand.max_region_bytes
        )
        .expect("String写入");
        writeln!(output, "region-fingerprint {}", hex_lower(self.fingerprint)).expect("String写入");
        output
    }
}

impl TurnRegionDemand {
    /// region 结束站点上界：发布站点数。
    const fn projects(self) -> u32 {
        self.publish_sites
    }
}

fn push_text(bytes: &mut Vec<u8>, text: &str) {
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(0);
}

fn push_texts(bytes: &mut Vec<u8>, texts: &[String]) {
    bytes.extend_from_slice(&(texts.len() as u32).to_le_bytes());
    for text in texts {
        push_text(bytes, text);
    }
}

fn hex_lower(bytes: [u8; 32]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(64);
    for byte in bytes {
        text.push(char::from(TABLE[usize::from(byte >> 4)]));
        text.push(char::from(TABLE[usize::from(byte & 0x0f)]));
    }
    text
}
