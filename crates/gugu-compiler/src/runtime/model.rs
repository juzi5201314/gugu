//! runtime raw 平面的契约对象、verifier 与 query driver。
//!
//! 契约对象把 owner 身份、slab 描述符表、dense size class、消息字段集合、grace 协议步骤
//! 与账本分类固定成一个带 schema 版本的可缓存对象：跨 owner 的消息只允许携带 descriptor
//! index、unit index、generation、epoch、bytes 与 integrity，任何地址类字段都在 verifier
//! 中被拒绝。

use serde::{Deserialize, Serialize};

use super::barrier_schema::{BarrierDemand, BarrierRuntimeContract, MessageFamilyTag};
use super::block_return_schema::{BlockReturnDemand, BlockReturnRuntimeContract};
use super::combining_schema::{CombiningDemand, CombiningPolicyV1, CombiningRuntimeContract};
use super::compression_schema::{
    CompressionDemand, CompressionPolicyV1, CompressionRuntimeContract,
};
use super::coroutine_schema::{CoroutineDemand, CoroutineRuntimeContract};
use super::edge_schema::{EdgeDemand, EdgeRuntimeContract};
use super::gc_metadata_contract::GcMetadataRuntimeContract;
use super::gc_metadata_schema::GcMetadataDemand;
use super::inbox::ServiceBudget;
use super::ledger::LedgerSchemaV1;
use super::local_heap_schema::{LocalHeapDemand, LocalHeapRuntimeContract};
use super::message::{BatchLimits, RETURN_NODE_ALIGN, RETURN_NODE_BYTES};
use super::pacing_schema::{GcPacingDemand, GcPacingRuntimeContract};
use super::platform::PlatformProfile;
use super::platform_schema::{PlatformRangeDemand, PlatformRangeSchemaV1};
use super::provenance_schema::{ProvenanceDemand, ProvenancePolicyV1, ProvenanceRuntimeContract};
use super::region_schema::TurnRegionRuntimeContract;
use super::resource::{self, RESOURCE_KINDS};
use super::routing_schema::{RoutingDemand, RoutingPolicyV1, RoutingRuntimeContract};
use super::scheduler_schema::{SchedulerDemand, SchedulerRuntimeContract};
use super::shared_heap_schema::{SharedHeapDemand, SharedHeapRuntimeContract};
use super::size_class::{DropScanPolicy, RuntimeSizeClassTable};
use super::slab::MemoryDomainId;
use super::stackmap_schema::{StackMapDemand, StackMapRuntimeContract};
use super::startup_schema::{Rt0Demand, Rt0SchemaV1};
use super::sync_schema::{SyncDemand, SyncRuntimeContract};
use super::wait_schema::{WaitDemand, WaitRuntimeContract};
use super::{
    BATCH_MAX, CACHE_LINE_BYTES, MarkDemand, MarkRuntimeContract, OWNER_INBOX_SHARDS,
    QUEUE_PAD_BYTES, RAW_SLAB_PAGE_BYTES, RETURN_SLAB_CACHE_SETS, RETURN_SLAB_CACHE_WAYS,
    TARGET_CACHE_ENTRIES,
};
use crate::{
    Diagnostic, DiagnosticCode, SourceMap, TargetName,
    query::{QueryEngine, QueryKey, QueryKind, QueryResult},
};

/// `RuntimeRawContractV1` 的 schema 版本。
///
/// 版本 25 相对版本 24 的变化：并入 `LogicalProcessorPrefix` 的 poll/ownership/TLAB/
/// TurnRegion 布局偏移，并把 `SchedulerRuntimeContract` 升到 schema 2。backend 只消费
/// 契约里的 `offset_of!` 结果，禁止手写第二份数字。
pub(crate) const RAW_MODEL_SCHEMA: u32 = 25;

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
    /// region export summary 位掩码。
    ExportSummary,
    /// raw payload 的对齐指数。
    PayloadAlign,
    /// arena 内的 card 序号。
    CardIndex,
    /// card 区间长度。
    CardCount,
    /// release 描述符的能力位。
    Flags,
    /// managed object 地址；只允许出现在被拒绝的 schema 中。
    ManagedAddress,
    /// raw 指针地址；只允许出现在被拒绝的 schema 中。
    RawPointer,
    /// mark owner credit 的稠密编号。
    Credit,
    /// 产生 mark 工作的 source block 序号。
    SourceBlock,
    /// 边变更的 target block 序号。
    TargetBlock,
    /// block 对内单调递增的发布序号。
    Sequence,
    /// signed 边差量。
    Delta,
    /// shared payload 的逻辑身份；与地址类字段严格分离。
    PayloadIdentity,
    /// typed combining 的同类合并键。
    MergeKey,
    /// typed combining 的标量参数。
    Scalar,
    /// typed combining 的 response slot。
    ResponseSlot,
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
            Self::CardIndex => "card-index",
            Self::ExportSummary => "export-summary",
            Self::CardCount => "card-count",
            Self::Flags => "flags",
            Self::ManagedAddress => "managed-address",
            Self::RawPointer => "raw-pointer",
            Self::Credit => "credit",
            Self::SourceBlock => "source-block",
            Self::TargetBlock => "target-block",
            Self::Sequence => "sequence",
            Self::Delta => "delta",
            Self::PayloadIdentity => "payload-identity",
            Self::MergeKey => "merge-key",
            Self::Scalar => "scalar",
            Self::ResponseSlot => "response-slot",
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

/// runtime 消息的字段集合；族判别值与 return 共用同一条传输通道。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct MessageSchemaV1 {
    pub(crate) schema: u32,
    /// 消息族判别值；同一传输通道上的 return 与 GC 工作消息严格分离。
    pub(crate) family: MessageFamilyTag,
    pub(crate) fields: Vec<MessageFieldSchema>,
}

impl MessageSchemaV1 {
    /// 返回 runtime raw return 消息的规范字段集合。
    pub(crate) fn runtime_raw() -> Self {
        Self {
            schema: 1,
            family: MessageFamilyTag::Return,
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

    /// 返回 GC 工作消息族的 `CardMarkBatch` 字段集合。
    pub(crate) fn card_mark() -> Self {
        Self {
            schema: 1,
            family: MessageFamilyTag::CardMark,
            fields: super::barrier_schema::card_mark_fields(),
        }
    }

    /// 返回 GC 工作消息族的 `MarkTicket` 字段集合。
    pub(crate) fn mark_ticket() -> Self {
        Self {
            schema: 1,
            family: MessageFamilyTag::MarkTicket,
            fields: super::mark_schema::mark_ticket_fields(),
        }
    }

    /// 返回 GC 工作消息族的 `EdgeDelta` 字段集合。
    pub(crate) fn edge_delta() -> Self {
        Self {
            schema: 1,
            family: MessageFamilyTag::EdgeDelta,
            fields: super::mark_schema::edge_delta_fields(),
        }
    }

    /// 返回 GC 工作消息族的 `HandleForward` 字段集合。
    pub(crate) fn handle_forward() -> Self {
        Self {
            schema: 1,
            family: MessageFamilyTag::HandleForward,
            fields: super::shared_heap_schema::handle_forward_fields(),
        }
    }

    /// 返回消息族。
    pub(crate) const fn family(&self) -> MessageFamilyTag {
        self.family
    }

    /// 校验字段集合不携带任何地址，并且覆盖全部必需身份字段。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        self.verify_family(self.family)
    }

    /// 按指定族校验字段集合：schema、地址字段、必需身份字段与稳定排序。
    pub(crate) fn verify_family(&self, family: MessageFamilyTag) -> Result<(), RawModelError> {
        if self.schema != 1 || self.family != family {
            return Err(RawModelError::new("runtime 消息 schema 版本或消息族不匹配"));
        }
        if self.fields.is_empty() {
            return Err(RawModelError::new("runtime 消息 schema 不能为空"));
        }
        for field in &self.fields {
            if field.kind.carries_address() {
                return Err(RawModelError::new(format!(
                    "runtime 消息字段 `{}` 携带地址，违反跨 owner 只发送逻辑序号",
                    field.name
                )));
            }
        }
        for required in required_fields(self.family) {
            if !self.fields.iter().any(|field| field.kind == required) {
                return Err(RawModelError::new(format!(
                    "runtime 消息 schema 缺少必需字段 `{}`",
                    required.name()
                )));
            }
        }
        for pair in self.fields.windows(2) {
            if pair[0].name >= pair[1].name {
                return Err(RawModelError::new(
                    "runtime 消息 schema 字段没有按名字稳定排序",
                ));
            }
        }
        if family == MessageFamilyTag::MarkTicket {
            // 目标形式由判别值决定：缺少任何一个身份字段都会让消费端无法判断该解析 arena
            // 偏移还是 handle slot。
            for (name, kind) in [
                ("target-kind", FieldKind::KindTag),
                ("target-handle-table", FieldKind::DescriptorIndex),
                ("target-handle-generation", FieldKind::Generation),
            ] {
                let field = self
                    .fields
                    .iter()
                    .find(|field| field.name == name)
                    .ok_or_else(|| {
                        RawModelError::new(format!("MarkTicket schema 缺少字段 `{name}`"))
                    })?;
                if field.kind != kind {
                    return Err(RawModelError::new(format!(
                        "MarkTicket 字段 `{name}` 的种类与登记不一致"
                    )));
                }
            }
        }
        if family == MessageFamilyTag::HandleForward {
            // 两个 payload identity 与两条 generation 车道必须按名字存在：只数 kind 无法区分
            // old/new payload，也无法区分 handle/forward generation。
            for (name, kind) in [
                ("handle_table", FieldKind::DescriptorIndex),
                ("handle_slot", FieldKind::UnitIndex),
                ("handle_generation", FieldKind::Generation),
                ("forward_generation", FieldKind::Generation),
                ("old_payload", FieldKind::PayloadIdentity),
                ("new_payload", FieldKind::PayloadIdentity),
            ] {
                let field = self
                    .fields
                    .iter()
                    .find(|field| field.name == name)
                    .ok_or_else(|| {
                        RawModelError::new(format!("HandleForward schema 缺少字段 `{name}`"))
                    })?;
                if field.kind != kind {
                    return Err(RawModelError::new(format!(
                        "HandleForward 字段 `{name}` 的种类与登记不一致"
                    )));
                }
            }
        }
        Ok(())
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + self.fields.len() * 24);
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.push(self.family.raw());
        bytes.extend_from_slice(&[0; 3]);
        bytes.extend_from_slice(&(self.fields.len() as u32).to_le_bytes());
        for field in &self.fields {
            bytes.extend_from_slice(field.name.as_bytes());
            bytes.push(0);
            bytes.push(field.kind as u8);
        }
        bytes
    }
}

/// 返回一个消息族必须覆盖的身份字段。
fn required_fields(family: MessageFamilyTag) -> Vec<FieldKind> {
    let mut required = vec![
        FieldKind::OwnerId,
        FieldKind::Generation,
        FieldKind::RouteKey,
        FieldKind::DescriptorIndex,
        FieldKind::Bytes,
        FieldKind::Integrity,
        FieldKind::Epoch,
    ];
    match family {
        // return 消息用 unit 描述被归还的 slot/line-run/extent 序号。
        MessageFamilyTag::Return => required.push(FieldKind::UnitIndex),
        // card batch 用 card 区间描述 remembered-set 键，不占用 return unit。
        MessageFamilyTag::CardMark => {
            required.push(FieldKind::CardIndex);
            required.push(FieldKind::CardCount);
        }
        // region transfer 用 region 序号描述被移交的私有区，并携带 export summary。
        MessageFamilyTag::RegionTransfer => {
            required.push(FieldKind::UnitIndex);
            required.push(FieldKind::ExportSummary);
        }
        // mark ticket 用目标对象偏移描述待标记对象，并携带 owner credit 与 source block。
        MessageFamilyTag::MarkTicket => {
            required.push(FieldKind::UnitIndex);
            required.push(FieldKind::Credit);
            required.push(FieldKind::SourceBlock);
            // 目标种类与 handle 身份：跨 owner 目标必须能分辨 arena 偏移与 handle slot。
            required.push(FieldKind::DescriptorIndex);
            required.push(FieldKind::KindTag);
        }
        // edge delta 用稳定 block 身份描述边端点，并携带 sequence、signed 差量与 credit。
        MessageFamilyTag::EdgeDelta => {
            required.push(FieldKind::SourceBlock);
            required.push(FieldKind::TargetBlock);
            required.push(FieldKind::Sequence);
            required.push(FieldKind::Delta);
            required.push(FieldKind::Credit);
        }
        // handle forward 用 handle slot/table 与两个 payload identity 描述一次搬迁；两条
        // generation 车道（handle/forward）与 target owner 身份同名检查在 `verify_family` 中。
        MessageFamilyTag::HandleForward => {
            required.push(FieldKind::UnitIndex);
            required.push(FieldKind::PayloadIdentity);
        }
    }
    required
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
    /// cage profile 开关；默认关闭即 full-pointer 语义。
    pub(crate) compression: CompressionPolicyV1,
    /// 路由 profile 开关；默认 direct 即 owner inbox 直达语义。
    pub(crate) routing: RoutingPolicyV1,
    /// release 安全 profile 开关；默认 release 即基线 provenance 检查语义。
    pub(crate) provenance: ProvenancePolicyV1,
    /// combining profile 开关；默认 direct 即冷操作不进记录池。
    pub(crate) combining: CombiningPolicyV1,
}

impl Default for RawPlanePolicyV1 {
    fn default() -> Self {
        Self {
            revision: 5,
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
            compression: CompressionPolicyV1::disabled(),
            routing: RoutingPolicyV1::direct(),
            provenance: ProvenancePolicyV1::release(),
            combining: CombiningPolicyV1::direct(),
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
    /// TurnRegion 需求视图；由优化后 LIR 的 region 指令推导。
    pub(crate) turn_region: super::region_schema::TurnRegionDemand,
    /// SharedHeap 需求视图；由优化后 LIR 的 handle 指令与 placement 推导。
    pub(crate) shared_heap: SharedHeapDemand,
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
    scheduler: SchedulerRuntimeContract,
    wait: WaitRuntimeContract,
    sync: SyncRuntimeContract,
    stackmap: StackMapRuntimeContract,
    gc_metadata: GcMetadataRuntimeContract,
    barrier: BarrierRuntimeContract,
    pacing: GcPacingRuntimeContract,
    region: TurnRegionRuntimeContract,
    local_heap: LocalHeapRuntimeContract,
    mark: MarkRuntimeContract,
    edge: EdgeRuntimeContract,
    shared_heap: SharedHeapRuntimeContract,
    block_return: BlockReturnRuntimeContract,
    compression: CompressionRuntimeContract,
    routing: RoutingRuntimeContract,
    provenance: ProvenanceRuntimeContract,
    combining: CombiningRuntimeContract,
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
    #[expect(
        clippy::too_many_arguments,
        reason = "RawPlane 契约由各子系统 demand 与平台档案共同推导"
    )]
    pub(crate) fn build(
        target: TargetName,
        policy: RawPlanePolicyV1,
        mut demand: RawPlaneDemand,
        mut resource_demand: RawResourceDemand,
        rt0_demand: Rt0Demand,
        scheduler_demand: SchedulerDemand,
        wait_demand: WaitDemand,
        sync_demand: SyncDemand,
        stackmap_demand: StackMapDemand,
        gc_metadata_demand: GcMetadataDemand,
        barrier_demand: BarrierDemand,
        pacing_demand: GcPacingDemand,
        mark_demand: MarkDemand,
        local_heap_demand: LocalHeapDemand,
        compression_demand: CompressionDemand,
        profile: PlatformProfile,
    ) -> Result<Self, RawModelError> {
        let classes = RuntimeSizeClassTable::ladder(MemoryDomainId::RUNTIME_RAW)?;
        let resource_classes = RuntimeSizeClassTable::resource_ladder()?;
        demand.message_nodes = policy.shards * policy.limits.items;
        resource_demand.kinds = RESOURCE_KINDS.len() as u32;
        let platform = PlatformRangeSchemaV1::build(profile, platform_range_demand(&demand))?;
        let rt0 = Rt0SchemaV1::build(rt0_demand)?;
        let sync = SyncRuntimeContract::build(sync_demand, profile)?;
        let stackmap = StackMapRuntimeContract::build(stackmap_demand)?;
        let gc_metadata = GcMetadataRuntimeContract::build(gc_metadata_demand)?;
        let barrier = BarrierRuntimeContract::build(barrier_demand)?;
        let pacing = GcPacingRuntimeContract::build(pacing_demand)?;
        let region = TurnRegionRuntimeContract::build(demand.turn_region)?;
        let local_heap = LocalHeapRuntimeContract::build(local_heap_demand, profile)?;
        // credit 池上界是「常驻 message node 容量加根槽数」：任何在飞 mark ticket 占一个
        // non-moving node，根 seed 不占 node 但每根槽每 cycle 至多一次。
        let mark = MarkRuntimeContract::build(
            mark_demand,
            u64::from(demand.message_nodes) + u64::from(mark_demand.root_sites),
        )?;
        let edge = EdgeRuntimeContract::build(
            EdgeDemand::derive(&barrier.demand, &mark.demand)?,
            &barrier,
            &mark,
        )?;
        let shared_heap = SharedHeapRuntimeContract::build(demand.shared_heap)?;
        let block_return = BlockReturnRuntimeContract::build(BlockReturnDemand::derive(
            &local_heap.demand,
            &shared_heap.demand,
        )?)?;
        // 压缩契约由显式 profile 开关与目标能力共同驱动：关闭态不预留 cage，开启态逐项核对
        // 粒度、上限与 canonical 位宽；目标能力检查失败必须在这里，而不是在世界预留前才暴露。
        let compression = CompressionRuntimeContract::build(
            compression_demand,
            policy.compression,
            crate::target::PointerCompression::for_target(target),
        )?;
        // 路由契约由显式 profile 开关驱动：direct 是默认且不分配 bucket 表，radix 只在
        // profile 开启后由世界接入 return 族的发布路径。
        let routing = RoutingRuntimeContract::build(
            RoutingDemand::derive(
                demand.owners,
                demand.runtime_raw_sites,
                demand.resource_sites,
            ),
            policy.routing,
        )?;
        // provenance 契约由安全 profile 驱动：release 是默认基线，debug/security 只追加
        // 显式登记的额外检查；per-domain secret 的登记目录与 MemoryDomainId 一一对应。
        let provenance = ProvenanceRuntimeContract::build(
            ProvenanceDemand::derive(
                demand.owners,
                classes.classes().len() as u32,
                resource_classes.classes().len() as u32,
            ),
            policy.provenance,
        )?;
        // combining 契约由 tag 目录与记录池需求驱动：direct 是默认且不创建记录，combined
        // 只在 profile 开启后把四条冷路径记录进非移动池；需求推导集中在 `combining_demand`。
        let combining =
            CombiningRuntimeContract::build(combining_demand(&demand), policy.combining)?;
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
            scheduler: SchedulerRuntimeContract::build(scheduler_demand)?,
            wait: WaitRuntimeContract::build(wait_demand, profile)?,
            sync,
            stackmap,
            gc_metadata,
            barrier,
            pacing,
            region,
            local_heap,
            mark,
            edge,
            shared_heap,
            block_return,
            compression,
            routing,
            provenance,
            combining,
            demand,
            resource_demand,
            grace_steps: GRACE_STEPS,
            fingerprint: [0; 32],
        };
        contract.fingerprint = contract.compute_fingerprint();
        contract.verify()?;
        Ok(contract)
    }

    pub(crate) fn with_gc_sections(
        mut self,
        type_section: Vec<u8>,
        metadata_section: Vec<u8>,
    ) -> Result<Self, RawModelError> {
        self.gc_metadata = self
            .gc_metadata
            .with_sections(type_section, metadata_section)?;
        self.fingerprint = self.compute_fingerprint();
        self.verify()?;
        Ok(self)
    }

    /// 以新的 cage profile 重建压缩契约段；其余段逐字段保持，指纹与校验同步刷新。
    ///
    /// 只供显式开启 cage profile 的消费者（harness / bench / 后续后端）使用：默认编译路径
    /// 保持关闭态，不带任何 cage 预留，因此不存在第二份契约装配点。
    pub(crate) fn with_compression(
        mut self,
        policy: CompressionPolicyV1,
        demand: CompressionDemand,
    ) -> Result<Self, RawModelError> {
        let target = TargetName::parse(&self.target_semantics)
            .map_err(|_| RawModelError::new("runtime raw 契约的目标语义未登记"))?;
        self.compression = CompressionRuntimeContract::build(
            demand,
            policy,
            crate::target::PointerCompression::for_target(target),
        )?;
        self.policy.compression = policy;
        self.fingerprint = self.compute_fingerprint();
        self.verify()?;
        Ok(self)
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

    /// 返回 TurnRegion 契约段。
    pub(crate) const fn region(&self) -> &TurnRegionRuntimeContract {
        &self.region
    }

    /// 返回 LocalHeap Immix/TLAB/分代契约段。
    pub(crate) const fn local_heap(&self) -> &LocalHeapRuntimeContract {
        &self.local_heap
    }

    /// 返回 MarkMailbox、owner credit 与终止检测契约段。
    pub(crate) const fn mark(&self) -> &MarkRuntimeContract {
        &self.mark
    }

    /// 返回 GC 工作消息族的 `MarkTicket` 字段集合。
    pub(crate) fn mark_ticket_message(&self) -> &MessageSchemaV1 {
        &self.mark.ticket_fields
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
    /// 返回调度契约段。
    pub(crate) fn scheduler(&self) -> &SchedulerRuntimeContract {
        &self.scheduler
    }

    /// 返回等待契约段。
    pub(crate) fn wait(&self) -> &WaitRuntimeContract {
        &self.wait
    }

    /// 返回同步契约段。
    pub(crate) fn sync(&self) -> &SyncRuntimeContract {
        &self.sync
    }

    /// 返回栈图契约段。
    pub(crate) fn stackmap(&self) -> &StackMapRuntimeContract {
        &self.stackmap
    }

    /// 返回 GC metadata 契约段。
    #[allow(dead_code, reason = "契约段由 runtime raw 与 ImagePlan 消费")]
    pub(crate) fn gc_metadata(&self) -> &GcMetadataRuntimeContract {
        &self.gc_metadata
    }

    /// 返回 hybrid write barrier 与 remembered-set 契约段。
    pub(crate) fn barrier(&self) -> &BarrierRuntimeContract {
        &self.barrier
    }

    /// 返回 `EdgeDelta` 消息与候选回收契约段。
    pub(crate) fn edge(&self) -> &EdgeRuntimeContract {
        &self.edge
    }

    /// 返回 SharedHeap stable handle、guard 与 forwarding grace 契约段。
    pub(crate) fn shared_heap(&self) -> &SharedHeapRuntimeContract {
        &self.shared_heap
    }

    /// 返回 owner-directed managed block return 契约段。
    pub(crate) fn block_return(&self) -> &BlockReturnRuntimeContract {
        &self.block_return
    }

    /// 返回 owner-directed managed block return 契约段（可变）。
    ///
    /// 只供契约不变量测试构造「子段内部自洽、跨段却对不上」的非法状态：正常路径只经 `build`
    /// 生成契约，不存在需要写这一段的生产代码。
    #[cfg(test)]
    pub(crate) fn block_return_mut(&mut self) -> &mut BlockReturnRuntimeContract {
        &mut self.block_return
    }

    /// 返回 checked pointer compression 契约段。
    pub(crate) fn compression(&self) -> &CompressionRuntimeContract {
        &self.compression
    }

    /// 返回 temporal radix fan-out 契约段。
    pub(crate) fn routing(&self) -> &RoutingRuntimeContract {
        &self.routing
    }

    /// 返回 raw link provenance 与 release 安全 profile 契约段。
    pub(crate) fn provenance(&self) -> &ProvenanceRuntimeContract {
        &self.provenance
    }

    /// 返回 raw link provenance 契约段（可变）。
    ///
    /// 只供契约不变量测试构造「子段内部自洽、跨段却对不上」的非法状态：正常路径只经
    /// `build` 生成契约，不存在需要写这一段的生产代码。
    #[cfg(test)]
    pub(crate) fn provenance_mut(&mut self) -> &mut ProvenanceRuntimeContract {
        &mut self.provenance
    }

    /// 返回 typed combining 冷操作契约段。
    pub(crate) fn combining(&self) -> &CombiningRuntimeContract {
        &self.combining
    }

    /// 返回 `HandleForward` 消息字段集合。
    pub(crate) fn handle_forward_message(&self) -> &MessageSchemaV1 {
        self.shared_heap.handle_forward_fields()
    }

    /// 返回 GC debt、credit、pacing 与 pressure 契约段。
    pub(crate) fn pacing(&self) -> &GcPacingRuntimeContract {
        &self.pacing
    }

    /// 返回 GC 工作消息族的 `CardMarkBatch` 字段集合。
    pub(crate) fn card_mark_message(&self) -> &MessageSchemaV1 {
        &self.barrier.message
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
        self.scheduler.verify()?;
        // 调度段的字段必须与登记的调优 profile 和 processor 前缀同源：段内自洽但
        // profile/偏移对不上同样是非法状态。
        let tuning = &crate::runtime::scheduler_schema::RUNTIME_TUNING_PROFILE;
        if self.scheduler.local_capacity != tuning.local_capacity
            || self.scheduler.remote_shards != tuning.remote_shards
            || self.scheduler.batch_max != tuning.batch_max
            || self.scheduler.service_interval != tuning.service_interval
            || self.scheduler.service_batch != tuning.service_batch
            || self.scheduler.queue_pad_bytes != tuning.queue_pad_bytes
            || self.scheduler.cache_line_bytes != tuning.cache_line_bytes
            || self.scheduler.poll_flags_offset != crate::runtime::processor::poll_flags_offset()
            || self.scheduler.ownership_offset != crate::runtime::processor::ownership_offset()
            || self.scheduler.tlab_cursor_offset != crate::runtime::processor::tlab_cursor_offset()
            || self.scheduler.tlab_limit_offset != crate::runtime::processor::tlab_limit_offset()
            || self.scheduler.turn_region_cursor_offset
                != crate::runtime::processor::turn_region_cursor_offset()
            || self.scheduler.turn_region_limit_offset
                != crate::runtime::processor::turn_region_limit_offset()
        {
            return Err(RawModelError::new(
                "调度契约与登记调优 profile 或 processor 前缀偏移不一致",
            ));
        }
        self.wait.verify()?;
        self.sync.verify()?;
        self.stackmap.verify()?;
        self.gc_metadata.verify()?;
        self.barrier.verify()?;
        self.pacing.verify()?;
        self.region.verify()?;
        self.local_heap.verify()?;
        if self.local_heap.demand().managed_types != self.gc_metadata.demand.type_count
            || self.local_heap.demand().barrier_sites != self.barrier.demand.card_mark_sites
        {
            return Err(RawModelError::new(
                "LocalHeap 需求与 barrier/gc metadata 契约不一致",
            ));
        }
        self.mark.verify()?;
        self.edge.verify(&self.barrier, &self.mark)?;
        self.shared_heap.verify()?;
        self.block_return.verify()?;
        self.compression.verify()?;
        // 策略是 cage 段的唯一来源：子段自洽但开关/尺寸与 raw policy 对不上同样是非法状态。
        if self.policy.compression
            != (CompressionPolicyV1 {
                enabled: self.compression.enabled,
                cage_bytes: self.compression.cage_bytes,
            })
        {
            return Err(RawModelError::new("压缩契约段与 raw policy 不一致"));
        }
        self.routing.verify()?;
        // 路由模式同样以 policy 为唯一来源：契约段与 raw policy 的模式必须一致。
        if self.policy.routing.mode != self.routing.mode {
            return Err(RawModelError::new("routing 契约段与 raw policy 不一致"));
        }
        if self.routing.demand
            != RoutingDemand::derive(
                self.demand.owners,
                self.demand.runtime_raw_sites,
                self.demand.resource_sites,
            )
        {
            return Err(RawModelError::new("routing 需求与 raw 平面派生值不一致"));
        }
        self.provenance.verify()?;
        // 安全 profile 同样以 policy 为唯一来源：契约段与 raw policy 的 profile 必须一致。
        if self.policy.provenance
            != (ProvenancePolicyV1 {
                profile: self.provenance.mode,
            })
        {
            return Err(RawModelError::new("provenance 契约段与 raw policy 不一致"));
        }
        if self.provenance.demand
            != ProvenanceDemand::derive(
                self.demand.owners,
                self.classes.classes().len() as u32,
                self.resource_classes.classes().len() as u32,
            )
        {
            return Err(RawModelError::new("provenance 需求与 raw 平面派生值不一致"));
        }
        self.combining.verify()?;
        // combining 模式同样以 policy 为唯一来源：契约段与 raw policy 的模式必须一致。
        if self.policy.combining.mode != self.combining.mode {
            return Err(RawModelError::new("combining 契约段与 raw policy 不一致"));
        }
        if self.combining.demand != combining_demand(&self.demand) {
            return Err(RawModelError::new("combining 需求与 raw 平面派生值不一致"));
        }
        if self.block_return.demand()
            != BlockReturnDemand::derive(&self.local_heap.demand(), &self.shared_heap.demand)?
        {
            return Err(RawModelError::new(
                "block return 需求与 LocalHeap/SharedHeap 派生值不一致",
            ));
        }
        if self.block_return.block_bytes != self.local_heap.block_bytes
            || self.block_return.arena_bytes != self.local_heap.arena_bytes
            || self.block_return.line_bytes != self.local_heap.line_bytes
            || self.block_return.grace_steps != self.grace_steps
        {
            return Err(RawModelError::new(
                "block return 尺寸与 LocalHeap / grace 契约不一致",
            ));
        }
        if self.edge.demand().edge_sites != self.barrier.demand.edge_summary_sites
            || self.edge.demand().reserve_slots != self.barrier.demand.shade_slots
        {
            return Err(RawModelError::new("edge 需求与 barrier 契约不一致"));
        }
        if self.mark.demand.root_sites != self.gc_metadata.demand.root_range_count
            || self.mark.demand.barrier_sites != self.barrier.demand.card_mark_sites
            || self.mark.demand.edge_delta_sites != self.barrier.demand.edge_summary_sites
            || self.mark.demand.ticket_sites != self.shared_heap.demand.mark_sites
            || self.barrier.demand.shared_field_sites != self.shared_heap.demand.barrier_sites
        {
            return Err(RawModelError::new(
                "mark 需求与 gc metadata/barrier/SharedHeap 契约不一致",
            ));
        }
        if self.mark.credit_pool
            != u64::from(self.demand.message_nodes) + u64::from(self.mark.demand.root_sites)
        {
            return Err(RawModelError::new(
                "mark credit 池上界与常驻 node 容量加根槽数不一致",
            ));
        }
        if self.region.demand() != self.demand.turn_region {
            return Err(RawModelError::new("TurnRegion 需求与LIR需求视图不一致"));
        }
        if self.message.family() != MessageFamilyTag::Return
            || self.barrier.message.family() != MessageFamilyTag::CardMark
            || self.mark.ticket_fields.family() != MessageFamilyTag::MarkTicket
            || self.shared_heap.handle_forward.family() != MessageFamilyTag::HandleForward
            || super::region_schema::region_transfer_fields() != self.region.transfer_fields
        {
            return Err(RawModelError::new(
                "runtime raw 契约的消息族判别与登记不一致",
            ));
        }
        if self.demand.message_nodes < u32::from(self.barrier.demand.card_mark_sites != 0) {
            return Err(RawModelError::new("card-mark 站点存在但常驻 node 容量为零"));
        }
        if self.scheduler.demand.spawn_sites != self.demand.coroutine_sites
            || self.scheduler.demand.suspend_points != self.demand.suspend_points
        {
            return Err(RawModelError::new("调度需求与LIR需求视图不一致"));
        }
        if self.scheduler.demand.yield_sites > self.scheduler.demand.suspend_points {
            return Err(RawModelError::new("调度 yield 需求超过挂起点上界"));
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
        bytes.extend_from_slice(&self.scheduler.canonical_bytes());
        bytes.extend_from_slice(&self.wait.canonical_bytes());
        bytes.extend_from_slice(&self.sync.canonical_bytes());
        bytes.extend_from_slice(&self.stackmap.canonical_bytes());
        bytes.extend_from_slice(&self.gc_metadata.canonical_bytes());
        bytes.extend_from_slice(&self.barrier.canonical_bytes());
        bytes.extend_from_slice(&self.pacing.canonical_bytes());
        bytes.extend_from_slice(&self.region.canonical_bytes());
        bytes.extend_from_slice(&self.local_heap.canonical_bytes());
        bytes.extend_from_slice(&self.mark.canonical_bytes());
        bytes.extend_from_slice(&self.shared_heap.canonical_bytes());
        bytes.extend_from_slice(&self.block_return.canonical_bytes());
        bytes.extend_from_slice(&self.compression.canonical_bytes());
        bytes.extend_from_slice(&self.routing.canonical_bytes());
        bytes.extend_from_slice(&self.provenance.canonical_bytes());
        bytes.extend_from_slice(&self.combining.canonical_bytes());
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
        output.push_str(&self.scheduler.dump());
        output.push_str(&self.wait.dump());
        output.push_str(&format!(
            "sync schema={} profile={} primitives={} total-ops={} fingerprint={}\n",
            self.sync.schema(),
            self.sync.profile(),
            self.sync.primitive_count(),
            self.sync.demand().total_ops(),
            hex(&self.sync.fingerprint())
        ));
        output.push_str(&self.stackmap.dump());
        output.push_str(&self.gc_metadata.dump());
        output.push_str(&self.barrier.dump());
        output.push_str(&self.pacing.dump());
        output.push_str(&self.region.dump());
        output.push_str(&self.local_heap.dump());
        output.push_str(&self.mark.dump());
        output.push_str(&self.shared_heap.dump());
        self.edge.dump_into(&mut output);
        output.push_str(&self.block_return.dump());
        output.push_str(&self.compression.dump());
        output.push_str(&self.routing.dump());
        output.push_str(&self.provenance.dump());
        output.push_str(&self.combining.dump());
        output.push_str(&format!(
            "runtime-message return-fields={} card-mark-fields={} mark-ticket-fields={} edge-delta-fields={} handle-forward-fields={} card-mark-family={}\n",
            self.message.fields.len(),
            self.card_mark_message().fields.len(),
            self.mark_ticket_message().fields.len(),
            self.edge.edge_delta_fields.fields.len(),
            self.handle_forward_message().fields.len(),
            match self.card_mark_message().family() {
                MessageFamilyTag::Return => "return",
                MessageFamilyTag::CardMark => "card-mark",
                MessageFamilyTag::RegionTransfer => "region-transfer",
                MessageFamilyTag::MarkTicket => "mark-ticket",
                MessageFamilyTag::EdgeDelta => "edge-delta",
                MessageFamilyTag::HandleForward => "handle-forward",
            },
        ));
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

/// 由 plane 需求视图推导 combining 记录池需求下界。
///
/// 规则集中在 `CombiningDemand::derive`；契约只做交叉校验，不重复定义推导。
fn combining_demand(demand: &RawPlaneDemand) -> CombiningDemand {
    CombiningDemand::derive(
        demand.owners,
        demand.runtime_raw_sites,
        demand.resource_sites,
        u32::try_from(super::extent::EXTENT_CLASS_LADDER.len()).expect("extent class 数量适配 u32"),
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
    pub(crate) scheduler_demand: SchedulerDemand,
    /// 等待源需求视图：channel / Join / select 调用计数。
    pub(crate) wait_demand: WaitDemand,
    /// 同步需求视图：atomic / mutex / rwlock / condvar / once / cancel 操作计数。
    pub(crate) sync_demand: SyncDemand,
    /// 栈图需求视图：逻辑函数、安全点、kind 分类与根字数。
    pub(crate) stackmap_demand: StackMapDemand,
    /// GC metadata 需求视图：类型表大小、trace/value program 字节数与 arena 布局。
    pub(crate) gc_metadata_demand: GcMetadataDemand,
    /// barrier 需求视图：permit 额度、reserved/bare 屏障与 edge summary 站点。
    pub(crate) barrier_demand: BarrierDemand,
    /// pacing 需求视图：分配站点、屏障站点、assist slow edge 与受管类型数。
    pub(crate) pacing_demand: GcPacingDemand,
    /// mark 需求视图：根站点、屏障站点、shared 站点与 edge delta 站点。
    pub(crate) mark_demand: MarkDemand,
    /// LocalHeap 需求视图：placement 站点、类型 footprint 与屏障站点。
    pub(crate) local_heap_demand: LocalHeapDemand,
    /// 压缩需求视图：解码点与压缩根槽上界。
    pub(crate) compression_demand: CompressionDemand,
    /// 已由 frontend 编码的真实 type/meta section。
    pub(crate) gc_type_section: &'a [u8],
    pub(crate) gc_metadata_section: &'a [u8],
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
        inputs.scheduler_demand,
        inputs.wait_demand,
        inputs.sync_demand,
        inputs.stackmap_demand,
        inputs.gc_metadata_demand,
        inputs.barrier_demand,
        inputs.pacing_demand,
        inputs.mark_demand,
        inputs.local_heap_demand,
        inputs.compression_demand,
        inputs.gc_type_section,
        inputs.gc_metadata_section,
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
                inputs.scheduler_demand,
                inputs.wait_demand,
                inputs.sync_demand,
                inputs.stackmap_demand,
                inputs.gc_metadata_demand,
                inputs.barrier_demand,
                inputs.pacing_demand,
                inputs.mark_demand,
                inputs.local_heap_demand,
                inputs.compression_demand,
                inputs.profile,
            )
            .and_then(|contract| {
                contract.with_gc_sections(
                    inputs.gc_type_section.to_vec(),
                    inputs.gc_metadata_section.to_vec(),
                )
            })
            .map_err(|error| crate::query::QueryError::Failed(error.message().to_owned()))?;
            super::coroutine_layout::verify_source(contract.coroutine(), inputs.hir, inputs.gir)
                .map_err(|error| crate::query::QueryError::Failed(error.message().to_owned()))?;
            super::channel_layout::verify_source(contract.wait(), inputs.hir, inputs.gir)
                .map_err(|error| crate::query::QueryError::Failed(error.message().to_owned()))?;
            super::sync_layout::verify_source(contract.sync(), inputs.hir, inputs.gir)
                .map_err(|error| crate::query::QueryError::Failed(error.message().to_owned()))?;
            super::barrier_layout::verify_source(contract.barrier(), inputs.hir, inputs.gir)
                .map_err(|error| crate::query::QueryError::Failed(error.message().to_owned()))?;
            super::local_heap_layout::verify_source(contract.local_heap(), inputs.hir, inputs.gir)
                .map_err(|error| crate::query::QueryError::Failed(error.message().to_owned()))?;
            super::shared_heap_layout::verify_source(
                contract.shared_heap(),
                inputs.hir,
                inputs.gir,
            )
            .map_err(|error| crate::query::QueryError::Failed(error.message().to_owned()))?;
            super::mark_layout::verify_source(contract.mark(), inputs.hir, inputs.gir)
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
    let contract = contract
        .with_gc_sections(
            inputs.gc_type_section.to_vec(),
            inputs.gc_metadata_section.to_vec(),
        )
        .map_err(|error| vec![error.diagnostic()])?;
    contract
        .verify()
        .map_err(|error| vec![error.diagnostic()])?;
    super::coroutine_layout::verify_source(contract.coroutine(), inputs.hir, inputs.gir)
        .map_err(|error| vec![error.diagnostic()])?;
    super::channel_layout::verify_source(contract.wait(), inputs.hir, inputs.gir)
        .map_err(|error| vec![error.diagnostic()])?;
    super::sync_layout::verify_source(contract.sync(), inputs.hir, inputs.gir)
        .map_err(|error| vec![error.diagnostic()])?;
    super::barrier_layout::verify_source(contract.barrier(), inputs.hir, inputs.gir)
        .map_err(|error| vec![error.diagnostic()])?;
    super::local_heap_layout::verify_source(contract.local_heap(), inputs.hir, inputs.gir)
        .map_err(|error| vec![error.diagnostic()])?;
    super::shared_heap_layout::verify_source(contract.shared_heap(), inputs.hir, inputs.gir)
        .map_err(|error| vec![error.diagnostic()])?;
    super::mark_layout::verify_source(contract.mark(), inputs.hir, inputs.gir)
        .map_err(|error| vec![error.diagnostic()])?;
    let _ = inputs.sources;
    Ok(contract)
}
