//! runtime raw 平面的契约对象、verifier 与 query driver。
//!
//! 契约对象把 owner 身份、slab 描述符表、dense size class、消息字段集合、grace 协议步骤
//! 与账本分类固定成一个带 schema 版本的可缓存对象：跨 owner 的消息只允许携带 descriptor
//! index、unit index、generation、epoch、bytes 与 integrity，任何地址类字段都在 verifier
//! 中被拒绝。

use serde::{Deserialize, Serialize};

use super::coroutine_schema::{CoroutineDemand, CoroutineRuntimeContract};
use super::inbox::ServiceBudget;
use super::ledger::LedgerSchemaV1;
use super::message::{BatchLimits, RETURN_NODE_ALIGN, RETURN_NODE_BYTES};
use super::platform::PlatformProfile;
use super::platform_schema::{PlatformRangeDemand, PlatformRangeSchemaV1};
use super::resource::{self, RESOURCE_KINDS};
use super::size_class::{DropScanPolicy, RuntimeSizeClassTable};
use super::slab::MemoryDomainId;
use super::startup_schema::{Rt0Demand, Rt0SchemaV1};
use super::{
    BATCH_MAX, CACHE_LINE_BYTES, OWNER_INBOX_SHARDS, QUEUE_PAD_BYTES, RAW_SLAB_PAGE_BYTES,
    RETURN_SLAB_CACHE_SETS, RETURN_SLAB_CACHE_WAYS, TARGET_CACHE_ENTRIES,
};
use crate::{
    Diagnostic, DiagnosticCode, SourceMap, TargetName,
    query::{QueryEngine, QueryKey, QueryKind, QueryResult},
};

/// 契约对象的schema版本；schema 5并入协程布局、stack arena与context代码。
pub(crate) const RAW_MODEL_SCHEMA: u32 = 5;

/// 资源契约段的 schema 版本。
pub(crate) const RESOURCE_SCHEMA: u32 = 1;

/// 契约对象构建或校验失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RawModelError {
    message: String,
}

impl RawModelError {
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

    /// 转换为统一诊断通道的 `E0058`。
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        Diagnostic::error(
            DiagnosticCode::RuntimeRawInvariant,
            self.message.clone(),
            None,
        )
    }
}

impl std::fmt::Display for RawModelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<super::slab::RawInvariant> for RawModelError {
    fn from(value: super::slab::RawInvariant) -> Self {
        Self::new(value.message().to_owned())
    }
}

/// 消息字段的种类。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum FieldKind {
    /// owner 所属 domain。
    OwnerDomain,
    /// owner 单调编号。
    OwnerId,
    /// owner/slab 的 generation。
    Generation,
    /// 生命周期内稳定的路由键。
    RouteKey,
    /// slab/block/range 描述符序号。
    DescriptorIndex,
    /// slot/block/line-run/extent 序号。
    UnitIndex,
    /// 本次待处理的物理字节。
    Bytes,
    /// producer topology/stop epoch。
    Epoch,
    /// generation、class、owner 与 link 校验信息。
    Integrity,
    /// return unit 的种类标签。
    KindTag,
    /// 消息状态。
    MessageState,
    /// intrusive link 编码。
    Link,
    /// release glue 的登记编号。
    ReleaseGlue,
    /// release descriptor 的稠密编号。
    ReleaseDescriptor,
    /// raw payload 字节数。
    PayloadSize,
    /// raw payload 的对齐指数。
    PayloadAlign,
    /// release 描述符的能力位。
    Flags,
    /// managed object 地址；只允许出现在被拒绝的 schema 中。
    ManagedAddress,
    /// raw 指针地址；只允许出现在被拒绝的 schema 中。
    RawPointer,
}

impl FieldKind {
    /// 返回种类名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::OwnerDomain => "owner-domain",
            Self::OwnerId => "owner-id",
            Self::Generation => "generation",
            Self::RouteKey => "route-key",
            Self::DescriptorIndex => "descriptor-index",
            Self::UnitIndex => "unit-index",
            Self::Bytes => "bytes",
            Self::Epoch => "epoch",
            Self::Integrity => "integrity",
            Self::KindTag => "kind-tag",
            Self::MessageState => "message-state",
            Self::Link => "link",
            Self::ReleaseGlue => "release-glue",
            Self::ReleaseDescriptor => "release-descriptor",
            Self::PayloadSize => "payload-size",
            Self::PayloadAlign => "payload-align",
            Self::Flags => "flags",
            Self::ManagedAddress => "managed-address",
            Self::RawPointer => "raw-pointer",
        }
    }

    /// 判断该种类是否是把地址放进消息的非法种类。
    pub(crate) const fn carries_address(self) -> bool {
        matches!(self, Self::ManagedAddress | Self::RawPointer)
    }
}

/// 一个消息字段的登记项。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct MessageFieldSchema {
    pub(crate) name: String,
    pub(crate) kind: FieldKind,
}

impl MessageFieldSchema {
    pub(crate) fn new(name: &str, kind: FieldKind) -> Self {
        Self {
            name: name.to_owned(),
            kind,
        }
    }
}

/// return message 的字段集合。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct MessageSchemaV1 {
    pub(crate) schema: u32,
    pub(crate) fields: Vec<MessageFieldSchema>,
}

impl MessageSchemaV1 {
    /// 返回 runtime raw 消息的规范字段集合。
    pub(crate) fn runtime_raw() -> Self {
        Self {
            schema: 1,
            fields: vec![
                MessageFieldSchema::new("bytes", FieldKind::Bytes),
                MessageFieldSchema::new("descriptor", FieldKind::DescriptorIndex),
                MessageFieldSchema::new("integrity", FieldKind::Integrity),
                MessageFieldSchema::new("kind", FieldKind::KindTag),
                MessageFieldSchema::new("next", FieldKind::Link),
                MessageFieldSchema::new("source_epoch", FieldKind::Epoch),
                MessageFieldSchema::new("state", FieldKind::MessageState),
                MessageFieldSchema::new("target.domain", FieldKind::OwnerDomain),
                MessageFieldSchema::new("target.generation", FieldKind::Generation),
                MessageFieldSchema::new("target.owner_id", FieldKind::OwnerId),
                MessageFieldSchema::new("target.route_key", FieldKind::RouteKey),
                MessageFieldSchema::new("unit", FieldKind::UnitIndex),
            ],
        }
    }

    /// 校验字段集合不携带任何地址，并且覆盖全部必需身份字段。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != 1 {
            return Err(RawModelError::new("return message schema 版本不匹配"));
        }
        if self.fields.is_empty() {
            return Err(RawModelError::new("return message schema 不能为空"));
        }
        for field in &self.fields {
            if field.kind.carries_address() {
                return Err(RawModelError::new(format!(
                    "return message 字段 `{}` 携带地址，违反跨 owner 只发送逻辑序号",
                    field.name
                )));
            }
        }
        for required in [
            FieldKind::OwnerId,
            FieldKind::Generation,
            FieldKind::RouteKey,
            FieldKind::DescriptorIndex,
            FieldKind::UnitIndex,
            FieldKind::Bytes,
            FieldKind::Integrity,
            FieldKind::Epoch,
        ] {
            if !self.fields.iter().any(|field| field.kind == required) {
                return Err(RawModelError::new(format!(
                    "return message schema 缺少必需字段 `{}`",
                    required.name()
                )));
            }
        }
        for pair in self.fields.windows(2) {
            if pair[0].name >= pair[1].name {
                return Err(RawModelError::new(
                    "return message schema 字段没有按名字稳定排序",
                ));
            }
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(4 + self.fields.len() * 24);
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&(self.fields.len() as u32).to_le_bytes());
        for field in &self.fields {
            bytes.extend_from_slice(field.name.as_bytes());
            bytes.push(0);
            bytes.push(field.kind as u8);
        }
        bytes
    }
}

pub(crate) use super::resource_schema::*;

/// raw plane 的调优 profile。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RawPlanePolicyV1 {
    pub(crate) revision: u32,
    pub(crate) shards: u32,
    pub(crate) limits: BatchLimits,
    pub(crate) return_node_bytes: u32,
    pub(crate) return_node_align: u32,
    pub(crate) return_slab_cache_sets: u32,
    pub(crate) return_slab_cache_ways: u32,
    pub(crate) target_cache_entries: u32,
    pub(crate) slab_page_bytes: u64,
    pub(crate) cache_line_bytes: u64,
    pub(crate) queue_pad_bytes: u64,
    pub(crate) service_items: u32,
    pub(crate) service_bytes: u64,
}

impl Default for RawPlanePolicyV1 {
    fn default() -> Self {
        Self {
            revision: 1,
            shards: OWNER_INBOX_SHARDS,
            limits: BatchLimits::default(),
            return_node_bytes: RETURN_NODE_BYTES,
            return_node_align: RETURN_NODE_ALIGN,
            return_slab_cache_sets: RETURN_SLAB_CACHE_SETS,
            return_slab_cache_ways: RETURN_SLAB_CACHE_WAYS,
            target_cache_entries: TARGET_CACHE_ENTRIES,
            slab_page_bytes: RAW_SLAB_PAGE_BYTES,
            cache_line_bytes: CACHE_LINE_BYTES,
            queue_pad_bytes: QUEUE_PAD_BYTES,
            service_items: BATCH_MAX,
            service_bytes: BatchLimits::default().batch_soft_bytes,
        }
    }
}

impl RawPlanePolicyV1 {
    /// 返回正常 service 预算。
    pub(crate) const fn service_budget(&self) -> ServiceBudget {
        ServiceBudget::new(self.service_items, self.service_bytes)
    }
}

/// 由编译产物推导出的 raw plane 需求视图。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RawPlaneDemand {
    /// 协程创建点数量；每个 live 协程占据一个 `CoroutineSlot` 级记录。
    pub(crate) coroutine_sites: u32,
    pub(crate) checked_entries: u32,
    pub(crate) suspend_points: u32,
    /// placement 判定的 resource 分配点数量。
    pub(crate) resource_sites: u32,
    /// placement 判定的 runtime raw 分配点数量。
    pub(crate) runtime_raw_sites: u32,
    /// owner 数量。
    pub(crate) owners: u32,
    /// 常驻 message node 数量下限。
    pub(crate) message_nodes: u32,
}

/// runtime raw 平面的契约对象。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RuntimeRawContractV1 {
    schema: u32,
    target_semantics: String,
    policy: RawPlanePolicyV1,
    classes: RuntimeSizeClassTable,
    resource_classes: RuntimeSizeClassTable,
    message: MessageSchemaV1,
    resources: ResourceSchemaV1,
    platform: PlatformRangeSchemaV1,
    ledger: LedgerSchemaV1,
    rt0: Rt0SchemaV1,
    coroutine: CoroutineRuntimeContract,
    demand: RawPlaneDemand,
    resource_demand: RawResourceDemand,
    grace_steps: u32,
    fingerprint: [u8; 32],
}

impl RuntimeRawContractV1 {
    /// 由目标描述、调优 profile 与需求视图构建契约对象。
    ///
    /// 常驻 message node 容量由 shard 数与 batch item 上限推导：每个 shard 至少能同时容纳
    /// 一个满 batch 的 node，因此它是契约中可证明的下界。
    pub(crate) fn build(
        target: TargetName,
        policy: RawPlanePolicyV1,
        mut demand: RawPlaneDemand,
        mut resource_demand: RawResourceDemand,
        rt0_demand: Rt0Demand,
        profile: PlatformProfile,
    ) -> Result<Self, RawModelError> {
        let classes = RuntimeSizeClassTable::ladder(MemoryDomainId::RUNTIME_RAW)?;
        let resource_classes = RuntimeSizeClassTable::resource_ladder()?;
        demand.message_nodes = policy.shards * policy.limits.items;
        resource_demand.kinds = RESOURCE_KINDS.len() as u32;
        let platform = PlatformRangeSchemaV1::build(profile, platform_range_demand(&demand))?;
        let rt0 = Rt0SchemaV1::build(rt0_demand)?;
        let mut contract = Self {
            schema: RAW_MODEL_SCHEMA,
            target_semantics: target.to_string(),
            policy,
            classes,
            resource_classes,
            message: MessageSchemaV1::runtime_raw(),
            resources: ResourceSchemaV1::fixed(),
            platform,
            ledger: LedgerSchemaV1::fixed(),
            rt0,
            coroutine: CoroutineRuntimeContract::build(CoroutineDemand {
                creation_sites: demand.coroutine_sites,
                checked_entries: demand.checked_entries,
                suspend_points: demand.suspend_points,
            })?,
            demand,
            resource_demand,
            grace_steps: GRACE_STEPS,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    /// 返回 schema 版本。
    pub(crate) const fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回目标语义字符串。
    pub(crate) fn target_semantics(&self) -> &str {
        &self.target_semantics
    }

    /// 返回调优 profile。
    pub(crate) const fn policy(&self) -> &RawPlanePolicyV1 {
        &self.policy
    }

    /// 返回 class 表。
    pub(crate) const fn classes(&self) -> &RuntimeSizeClassTable {
        &self.classes
    }

    /// 返回 Resource domain 的 class 表。
    pub(crate) const fn resource_classes(&self) -> &RuntimeSizeClassTable {
        &self.resource_classes
    }

    /// 返回资源平面契约段。
    pub(crate) const fn resources(&self) -> &ResourceSchemaV1 {
        &self.resources
    }

    /// 返回资源平面需求视图。
    pub(crate) const fn resource_demand(&self) -> &RawResourceDemand {
        &self.resource_demand
    }

    /// 返回资源 class 数量。
    pub(crate) fn resource_class_count(&self) -> u32 {
        self.resource_classes.classes().len() as u32
    }

    /// 返回登记的资源种类数量。
    pub(crate) fn resource_kind_count(&self) -> u32 {
        self.resources.kinds.kinds.len() as u32
    }

    /// 返回统一 release 入口名。
    pub(crate) fn unified_release_entry(&self) -> &str {
        &self.resources.kinds.release_entry
    }

    /// 返回消息字段集合。
    pub(crate) const fn message(&self) -> &MessageSchemaV1 {
        &self.message
    }

    /// 返回需求视图。
    pub(crate) const fn demand(&self) -> &RawPlaneDemand {
        &self.demand
    }

    /// 返回平台范围契约段。
    pub(crate) const fn platform(&self) -> &PlatformRangeSchemaV1 {
        &self.platform
    }

    /// 返回平台范围需求视图。
    pub(crate) const fn platform_demand(&self) -> &PlatformRangeDemand {
        &self.platform.demand
    }

    /// 返回账本契约段。
    pub(crate) const fn ledger(&self) -> &LedgerSchemaV1 {
        &self.ledger
    }

    /// 返回 rt0 启动、终止与报告契约段。
    pub(crate) const fn rt0(&self) -> &Rt0SchemaV1 {
        &self.rt0
    }

    pub(crate) fn coroutine(&self) -> &CoroutineRuntimeContract {
        &self.coroutine
    }

    /// 返回账本分类名。
    pub(crate) fn ledger_categories(&self) -> Vec<String> {
        self.ledger.names()
    }

    /// 返回 queue-page grace 的步骤数。
    pub(crate) const fn grace_steps(&self) -> u32 {
        self.grace_steps
    }

    /// 返回内容身份。
    pub(crate) const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 返回登记的 class 数量。
    pub(crate) fn class_count(&self) -> u32 {
        self.classes.classes().len() as u32
    }

    /// 返回 shard 数量。
    pub(crate) const fn shard_count(&self) -> u32 {
        self.policy.shards
    }

    /// 返回 batch 上限。
    pub(crate) const fn batch_limits(&self) -> BatchLimits {
        self.policy.limits
    }

    /// 返回 const 常驻 message node 容量下限。
    pub(crate) fn message_node_capacity(&self) -> u32 {
        self.demand.message_nodes
    }

    /// 校验 schema、class 表、消息字段、grace 步骤、账本分类与 shard/batch 常量。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != RAW_MODEL_SCHEMA {
            return Err(RawModelError::new("runtime raw 契约 schema 不匹配"));
        }
        if self.target_semantics.is_empty() {
            return Err(RawModelError::new("runtime raw 契约缺少目标语义"));
        }
        if TargetName::parse(&self.target_semantics).is_err() {
            return Err(RawModelError::new("runtime raw 契约的目标语义未登记"));
        }
        if self.policy.shards != OWNER_INBOX_SHARDS {
            return Err(RawModelError::new(
                "owner inbox shard 数量与调度器契约不一致",
            ));
        }
        if self.policy.limits.items != BATCH_MAX || self.policy.limits.items == 0 {
            return Err(RawModelError::new("batch item 上限与登记值不一致"));
        }
        if self.policy.limits.batch_soft_bytes == 0 {
            return Err(RawModelError::new(
                "batch byte 上限必须与 item 上限同时存在",
            ));
        }
        if self.policy.cache_line_bytes != CACHE_LINE_BYTES
            || self.policy.queue_pad_bytes < CACHE_LINE_BYTES
            || self.policy.slab_page_bytes != RAW_SLAB_PAGE_BYTES
        {
            return Err(RawModelError::new(
                "raw plane 的 cache line、queue padding 或 slab page 与契约不一致",
            ));
        }
        if self.policy.return_node_bytes < self.policy.return_node_align
            || u64::from(self.policy.return_node_align) < CACHE_LINE_BYTES
        {
            return Err(RawModelError::new(
                "return node 的 stride 或对齐不满足 cache line 规则",
            ));
        }
        if self.policy.return_slab_cache_sets != RETURN_SLAB_CACHE_SETS
            || self.policy.return_slab_cache_ways != RETURN_SLAB_CACHE_WAYS
            || self.policy.target_cache_entries != TARGET_CACHE_ENTRIES
        {
            return Err(RawModelError::new(
                "source slab 聚合或 target cache 的关联度与契约不一致",
            ));
        }
        self.classes
            .verify()
            .map_err(|error| RawModelError::new(error.message().to_owned()))?;
        if self
            .classes
            .classes()
            .iter()
            .any(|class| class.domain != MemoryDomainId::RUNTIME_RAW)
        {
            return Err(RawModelError::new(
                "runtime raw 契约的 class 必须属于 RuntimeRaw domain",
            ));
        }
        self.message.verify()?;
        self.resource_classes
            .verify()
            .map_err(|error| RawModelError::new(error.message().to_owned()))?;
        if self.resource_classes.classes().iter().any(|class| {
            class.domain != MemoryDomainId::RESOURCE
                || class.policy != DropScanPolicy::ResourceLease
                || class.header_bytes != resource::CELL_HEADER_BYTES
        }) {
            return Err(RawModelError::new(
                "资源 class 必须属于 Resource domain 并使用 64-byte lease header",
            ));
        }
        self.resources.verify()?;
        if self.resource_demand.kinds != RESOURCE_KINDS.len() as u32 {
            return Err(RawModelError::new("资源种类数量与登记目录不一致"));
        }
        if self.grace_steps != GRACE_STEPS {
            return Err(RawModelError::new("queue-page grace 步骤数与契约不一致"));
        }
        self.platform.verify()?;
        if self.platform.demand != platform_range_demand(&self.demand) {
            return Err(RawModelError::new("平台范围需求与 plane 需求视图不一致"));
        }
        self.ledger.verify()?;
        self.rt0.verify()?;
        self.coroutine.verify()?;
        if self.coroutine.demand
            != (CoroutineDemand {
                creation_sites: self.demand.coroutine_sites,
                checked_entries: self.demand.checked_entries,
                suspend_points: self.demand.suspend_points,
            })
        {
            return Err(RawModelError::new("协程需求与LIR需求视图不一致"));
        }
        if self.demand.message_nodes != self.policy.shards * self.policy.limits.items {
            return Err(RawModelError::new(
                "常驻 message node 容量低于 shard 与 batch 上限的乘积",
            ));
        }
        if self.fingerprint != self.compute_fingerprint() {
            return Err(RawModelError::new("runtime raw 契约指纹与内容不一致"));
        }
        Ok(())
    }

    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(self.target_semantics.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&self.policy.revision.to_le_bytes());
        bytes.extend_from_slice(&self.policy.shards.to_le_bytes());
        bytes.extend_from_slice(&self.policy.limits.items.to_le_bytes());
        bytes.extend_from_slice(&self.policy.limits.batch_soft_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.policy.return_node_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.policy.return_node_align.to_le_bytes());
        bytes.extend_from_slice(&self.policy.return_slab_cache_sets.to_le_bytes());
        bytes.extend_from_slice(&self.policy.return_slab_cache_ways.to_le_bytes());
        bytes.extend_from_slice(&self.policy.target_cache_entries.to_le_bytes());
        bytes.extend_from_slice(&self.policy.slab_page_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.policy.cache_line_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.policy.queue_pad_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.policy.service_items.to_le_bytes());
        bytes.extend_from_slice(&self.policy.service_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.classes.canonical_bytes());
        bytes.extend_from_slice(&self.resource_classes.canonical_bytes());
        bytes.extend_from_slice(&self.message.canonical_bytes());
        bytes.extend_from_slice(&self.resources.canonical_bytes());
        bytes.extend_from_slice(&self.platform.canonical_bytes());
        bytes.extend_from_slice(&self.ledger.canonical_bytes());
        bytes.extend_from_slice(&self.rt0.canonical_bytes());
        bytes.extend_from_slice(&self.coroutine.canonical_bytes());
        bytes.extend_from_slice(&self.resource_demand.resource_sites.to_le_bytes());
        bytes.extend_from_slice(&self.resource_demand.acquire_sites.to_le_bytes());
        bytes.extend_from_slice(&self.resource_demand.release_sites.to_le_bytes());
        bytes.extend_from_slice(&self.resource_demand.transfer_sites.to_le_bytes());
        bytes.extend_from_slice(&self.resource_demand.finalize_sites.to_le_bytes());
        bytes.extend_from_slice(&self.resource_demand.owners.to_le_bytes());
        bytes.extend_from_slice(&self.resource_demand.kinds.to_le_bytes());
        bytes.extend_from_slice(&self.demand.coroutine_sites.to_le_bytes());
        bytes.extend_from_slice(&self.demand.resource_sites.to_le_bytes());
        bytes.extend_from_slice(&self.demand.runtime_raw_sites.to_le_bytes());
        bytes.extend_from_slice(&self.demand.owners.to_le_bytes());
        bytes.extend_from_slice(&self.demand.message_nodes.to_le_bytes());
        bytes.extend_from_slice(&self.grace_steps.to_le_bytes());
        bytes
    }

    fn compute_fingerprint(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-runtime-raw-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }

    /// 返回稳定文本 dump；不含地址、宿主路径与线程编号。
    pub(crate) fn dump(&self) -> String {
        let policy = self.policy();
        let mut output = String::new();
        output.push_str(&format!(
            "runtime-raw schema={} target={} policy-revision={} fingerprint={}\n",
            self.schema(),
            self.target_semantics(),
            policy.revision,
            hex(&self.fingerprint())
        ));
        output.push_str(&format!(
            "shards={} batch-items={} batch-bytes={} node-bytes={} node-align={} slab-page={}\n",
            self.shard_count(),
            self.batch_limits().items,
            self.batch_limits().batch_soft_bytes,
            policy.return_node_bytes,
            policy.return_node_align,
            policy.slab_page_bytes
        ));
        output.push_str(&format!(
            "slab-cache=sets:{} ways:{} target-cache={} grace-steps={}\n",
            policy.return_slab_cache_sets,
            policy.return_slab_cache_ways,
            policy.target_cache_entries,
            self.grace_steps()
        ));
        for class in self.classes().classes() {
            output.push_str(&format!(
                "class {} stride={} payload={} align={} slots-per-span={} link={} policy={:?}\n",
                class.id.raw(),
                class.slot_stride,
                class.payload_bytes,
                class.alignment,
                class.slots_per_span,
                class.link_usable,
                class.policy
            ));
        }
        for field in &self.message().fields {
            output.push_str(&format!("message {} {}\n", field.name, field.kind.name()));
        }
        for partition in &self.ledger().partitions {
            output.push_str(&format!(
                "ledger-partition {} plane={} total={} members={}\n",
                partition.name,
                partition.plane.name(),
                partition.total,
                partition.categories.join(",")
            ));
        }
        for category in &self.ledger().categories {
            output.push_str(&format!(
                "ledger {} partition={} residual={} counter={}\n",
                category.name, category.partition, category.residual, category.counter
            ));
        }
        let demand = self.demand();
        output.push_str(&format!(
            "demand coroutine-sites={} resource-sites={} runtime-raw-sites={} owners={} message-nodes={}\n",
            demand.coroutine_sites,
            demand.resource_sites,
            demand.runtime_raw_sites,
            demand.owners,
            self.message_node_capacity()
        ));
        let resources = self.resources();
        output.push_str(&format!(
            "resource schema={} header-bytes={} kinds={} glues={} align-limit={} release-entry={}\n",
            resources.schema,
            resources.header.header_bytes,
            self.resource_kind_count(),
            resources.release_glue_count,
            resources.dedicated_align_limit,
            self.unified_release_entry()
        ));
        for field in &resources.header.fields {
            output.push_str(&format!(
                "resource-cell {} offset={} bytes={} kind={}\n",
                field.name,
                field.offset,
                field.bytes,
                field.kind.name()
            ));
        }
        for bit in &resources.states.bits {
            output.push_str(&format!("resource-state {} bit={}\n", bit.name, bit.bit));
        }
        for transition in &resources.states.transitions {
            output.push_str(&format!(
                "resource-transition {} -> {} on {}\n",
                transition.from, transition.to, transition.trigger
            ));
        }
        for field in &resources.release.fields {
            output.push_str(&format!(
                "resource-release-field {} {}\n",
                field.name,
                field.kind.name()
            ));
        }
        for kind in &resources.kinds.kinds {
            output.push_str(&format!(
                "resource-kind {} {} entry={} close-idempotent={}\n",
                kind.id, kind.name, kind.release_entry, kind.close_idempotent
            ));
        }
        output.push_str(&format!(
            "resource-demand sites={} acquire={} release={} transfer={} finalize={} owners={}\n",
            self.resource_demand.resource_sites,
            self.resource_demand.acquire_sites,
            self.resource_demand.release_sites,
            self.resource_demand.transfer_sites,
            self.resource_demand.finalize_sites,
            self.resource_demand.owners
        ));
        for class in self.resource_classes().classes() {
            output.push_str(&format!(
                "resource-class {} stride={} payload={} header={} align={} policy={:?}\n",
                class.id.raw(),
                class.slot_stride,
                class.payload_bytes,
                class.header_bytes,
                class.alignment,
                class.policy
            ));
        }
        let platform = self.platform();
        output.push_str(&format!(
            "platform schema={} profile={} page={} huge-page={} guard={} mapping-limit={} zero-on-commit={} entropy={} entropy-available={} dump={} huge-page-hint={} low-memory-hint={}\n",
            platform.schema,
            platform.profile(),
            platform.page_bytes(),
            platform.huge_page_bytes(),
            platform.guard_bytes(),
            platform.policy.mapping_limit,
            platform.policy.commit_zeroes,
            platform.policy.entropy_source,
            platform.policy.entropy_available,
            platform.dump_policy_default(),
            platform.policy.huge_page_hint,
            platform.policy.low_memory_hint
        ));
        for op in &platform.ops.ops {
            output.push_str(&format!(
                "range-op {} mutating={} blocking={} faults={}\n",
                op.name,
                op.mutating,
                op.blocking,
                op.fault_classes.join(",")
            ));
        }
        for class in &platform.classes.classes {
            output.push_str(&format!(
                "extent-class bytes={} align={} huge-page={}\n",
                class.bytes, class.alignment, class.huge_page
            ));
        }
        for cost in &platform.states.costs {
            output.push_str(&format!(
                "range-state {} rule={} split-by-commit={}\n",
                cost.state, cost.rule, cost.split_by_commit
            ));
        }
        for transition in &platform.states.transitions {
            output.push_str(&format!(
                "range-transition {} -> {} on {}\n",
                transition.from, transition.to, transition.trigger
            ));
        }
        output.push_str(&format!(
            "range-trim grace-steps={} leases=allocator,scanner,forwarder\n",
            self.grace_steps()
        ));
        for entry in &platform.fault_map {
            output.push_str(&format!(
                "range-fault {} {} -> {}\n",
                entry.profile, entry.error, entry.class
            ));
        }
        let demand = self.platform_demand();
        output.push_str(&format!(
            "range-demand payload={} stack={} metadata={} guard={} owners={}\n",
            demand.payload_extents,
            demand.stack_extents,
            demand.metadata_extents,
            demand.guard_extents,
            demand.owners
        ));
        output.push_str(&self.rt0.dump());
        output.push_str(&self.coroutine.dump());
        output
    }
}

/// 由 plane 需求视图推导平台范围需求下界。
///
/// 规则集中在 `PlatformRangeDemand::derive`；契约只做交叉校验，不重复定义推导。
fn platform_range_demand(demand: &RawPlaneDemand) -> PlatformRangeDemand {
    PlatformRangeDemand::derive(
        demand.owners,
        demand.coroutine_sites,
        demand.resource_sites,
        demand.runtime_raw_sites,
    )
}

/// queue-page grace 的固定步骤数。
pub(crate) const GRACE_STEPS: u32 = 4;

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    text
}

/// 契约 query 的输入集合。
pub(crate) struct RawModelInputs<'a> {
    pub(crate) target: TargetName,
    pub(crate) policy: RawPlanePolicyV1,
    /// 平台 profile；决定平台常量、失败映射与 extent 阶梯的端点。
    pub(crate) profile: PlatformProfile,
    pub(crate) demand: RawPlaneDemand,
    pub(crate) resource_demand: RawResourceDemand,
    /// rt0 启动需求视图：入口存在性与 main 返回类型。
    pub(crate) rt0_demand: Rt0Demand,
    /// 生成契约所依据的 LIR 输入指纹。
    pub(crate) lir_fingerprint: [u8; 32],
    /// placement world 指纹。
    pub(crate) placement_fingerprint: [u8; 32],
    pub(crate) sources: &'a SourceMap,
    pub(crate) hir: &'a crate::frontend::hir::Module,
    pub(crate) gir: &'a crate::frontend::gir::GirWorldV1,
}

/// 通过 query 构建并校验契约对象；失败进入 `E0058`。
pub(crate) fn run(
    inputs: RawModelInputs<'_>,
    queries: &QueryEngine,
) -> Result<RuntimeRawContractV1, Vec<Diagnostic>> {
    let mut key_bytes = serde_json::to_vec(&(
        inputs.policy,
        inputs.demand,
        inputs.resource_demand,
        inputs.rt0_demand,
    ))
    .expect("runtime需求与策略可序列化");
    key_bytes.extend_from_slice(&inputs.lir_fingerprint);
    key_bytes.extend_from_slice(&inputs.placement_fingerprint);
    key_bytes.extend_from_slice(inputs.target.to_string().as_bytes());
    key_bytes.extend_from_slice(inputs.profile.name().as_bytes());
    let key = QueryKey::new(QueryKind::RuntimeRawModel, RAW_MODEL_SCHEMA, &key_bytes);
    let mut fresh = None;
    let result: QueryResult = queries
        .compute(key, |context| {
            context.record_dependency(
                QueryKey::new(
                    QueryKind::BuildLir,
                    crate::lir::SCHEMA,
                    inputs.lir_fingerprint,
                ),
                inputs.lir_fingerprint,
            );
            context.record_dependency(
                QueryKey::new(
                    QueryKind::EscapeAndPlacement,
                    1,
                    inputs.placement_fingerprint,
                ),
                inputs.placement_fingerprint,
            );
            let contract = RuntimeRawContractV1::build(
                inputs.target,
                inputs.policy,
                inputs.demand,
                inputs.resource_demand,
                inputs.rt0_demand,
                inputs.profile,
            )
            .map_err(|error| crate::query::QueryError::Failed(error.message().to_owned()))?;
            super::coroutine_layout::verify_source(contract.coroutine(), inputs.hir, inputs.gir)
                .map_err(|error| crate::query::QueryError::Failed(error.message().to_owned()))?;
            let bytes = serde_json::to_vec(&contract).expect("runtime raw 契约可序列化");
            fresh = Some(contract);
            Ok((bytes, Vec::new()))
        })
        .map_err(|error| {
            vec![Diagnostic::error(
                DiagnosticCode::RuntimeRawInvariant,
                error.to_string(),
                None,
            )]
        })?;
    let contract = match fresh {
        Some(contract) => contract,
        None => serde_json::from_slice(result.payload()).map_err(|_| {
            vec![Diagnostic::error(
                DiagnosticCode::RuntimeRawInvariant,
                "缓存的 runtime raw 契约不是合法 schema",
                None,
            )]
        })?,
    };
    contract
        .verify()
        .map_err(|error| vec![error.diagnostic()])?;
    super::coroutine_layout::verify_source(contract.coroutine(), inputs.hir, inputs.gir)
        .map_err(|error| vec![error.diagnostic()])?;
    let _ = inputs.sources;
    Ok(contract)
}
