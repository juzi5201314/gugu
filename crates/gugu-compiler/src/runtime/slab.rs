//! owner 身份、稳定 slab 描述符与 slot 状态机。
//!
//! owner 是持有某个 raw slab/span 本地回收权的 `MemoryOwner`，不是操作系统线程；协程可以
//! 迁移、worker 可以退出、owner 也可以在 topology 变更时转移，这些变化由 token、
//! generation 与 grace 协议处理。descriptor 页面属于 non-moving metadata range，不放在
//! 用户 payload 中，也不能在 slot 回收时被当作普通对象覆盖。

use std::fmt;

use super::message::{LinkCodec, LinkError};
use super::provider::{ProviderError, RangeId};
use super::size_class::RuntimeSizeClass;

/// raw 平面不变量失败；进入 `RuntimeInvariant` 分类而不丢弃任何记录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RawInvariant {
    message: String,
}

impl RawInvariant {
    /// 用固定文本创建不变量失败。
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 返回诊断文本。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RawInvariant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<LinkError> for RawInvariant {
    fn from(value: LinkError) -> Self {
        Self::new(format!("link 校验失败：{value}"))
    }
}

impl From<ProviderError> for RawInvariant {
    fn from(value: ProviderError) -> Self {
        Self::new(format!("平台 range 请求失败：{value}"))
    }
}

/// raw 平面的确定性种子；route key 与 integrity secret 都由它派生，绝不来自地址。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeSeed {
    state: u64,
    draws: u64,
}

impl RuntimeSeed {
    /// 用固定种子创建；真实 runtime 由启动流程注入 entropy，本模型只接受显式种子。
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            state: seed,
            draws: 0,
        }
    }

    /// 返回已抽取的次数，用于断言 route key 与 secret 不复用。
    pub(crate) const fn draws(&self) -> u64 {
        self.draws
    }

    /// 抽取下一个确定性 64-bit 值。
    pub(crate) fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.draws += 1;
        self.state.rotate_left(31) ^ self.state
    }

    /// 派生一个 per-domain integrity secret。
    pub(crate) fn secret(&mut self) -> [u8; 32] {
        let mut secret = [0_u8; 32];
        for chunk in secret.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_le_bytes());
        }
        secret
    }
}

/// 一个 memory domain 的稠密编号。
#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub(crate) struct MemoryDomainId(u8);

impl MemoryDomainId {
    /// coroutine turn 私有的 TurnRegion 与 export/transfer descriptor。
    pub(crate) const MANAGED_TURN: Self = Self(0);
    /// LocalHeap 的 Immix arena、block、line 与 TLAB。
    pub(crate) const MANAGED_LOCAL: Self = Self(1);
    /// SharedHeap payload、stable handle 与 forwarding grace。
    pub(crate) const MANAGED_SHARED: Self = Self(2);
    /// raw slab、stack span 与 runtime record。
    pub(crate) const RUNTIME_RAW: Self = Self(3);
    /// `ResourceCell` 及其稳定 descriptor。
    pub(crate) const RESOURCE: Self = Self(4);
    /// virtual range、commit/decommit 与 guard page。
    pub(crate) const PLATFORM_RANGE: Self = Self(5);
    /// 外部系统或 FFI 所有的缓冲区。
    pub(crate) const FOREIGN: Self = Self(6);

    /// 全部 domain 的稠密登记顺序。
    pub(crate) const ALL: [Self; 7] = [
        Self::MANAGED_TURN,
        Self::MANAGED_LOCAL,
        Self::MANAGED_SHARED,
        Self::RUNTIME_RAW,
        Self::RESOURCE,
        Self::PLATFORM_RANGE,
        Self::FOREIGN,
    ];

    /// 返回稠密编号。
    pub(crate) const fn raw(self) -> u8 {
        self.0
    }

    /// 返回 domain 的稳定名称。
    pub(crate) const fn name(self) -> &'static str {
        match self.0 {
            0 => "ManagedTurn",
            1 => "ManagedLocal",
            2 => "ManagedShared",
            3 => "RuntimeRaw",
            4 => "Resource",
            5 => "PlatformRange",
            _ => "Foreign",
        }
    }

    /// 由稠密编号还原 domain。
    pub(crate) fn from_raw(raw: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|domain| domain.0 == raw)
    }
}

/// owner 的单调编号；进程内分配且永不复用。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OwnerId(u64);

impl OwnerId {
    /// 返回编号原值。
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    /// 由编号原值还原。
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// 返回作为稠密下标的编号。
    pub(crate) fn index(self) -> Option<usize> {
        usize::try_from(self.0).ok()
    }
}

/// owner 的接管、转移或重建代数。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OwnerGeneration(u64);

impl OwnerGeneration {
    /// 返回代数原值。
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    /// 由代数原值还原。
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// 生命周期内稳定的路由键；不从可移动地址推导，也不复用。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RouteKey(u64);

impl RouteKey {
    /// 返回路由键原值。
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    /// 由路由键原值还原。
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// 拓扑/回收 epoch；发布与确认形成 Release/Acquire 配对。
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Epoch(u32);

impl Epoch {
    /// 返回 epoch 原值。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    /// 由 epoch 原值还原。
    pub(crate) const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// 返回下一个 epoch。
    pub(crate) const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// 消息与描述符携带的 owner 身份。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OwnerToken {
    pub(crate) domain: MemoryDomainId,
    pub(crate) owner_id: OwnerId,
    pub(crate) generation: OwnerGeneration,
    pub(crate) route_key: RouteKey,
}

impl OwnerToken {
    /// 判断 token 是否与 directory 当前记录同时匹配。
    pub(crate) fn matches(&self, record: &OwnerRecord) -> bool {
        self.owner_id == record.owner_id
            && self.generation == record.generation
            && self.route_key == record.route_key
    }
}

/// owner 的生命周期状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OwnerState {
    /// 正常接受 direct publish。
    Active,
    /// 已停止新的 direct publish，等待排空。
    Draining,
    /// 已发布转发目标，旧消息沿旧记录排空。
    Forwarding,
    /// 已释放 descriptor 与路由槽；编号不再复用。
    Retired,
}

impl OwnerState {
    /// 返回状态名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Draining => "Draining",
            Self::Forwarding => "Forwarding",
            Self::Retired => "Retired",
        }
    }
}

/// token 相对 directory 当前记录的解析结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Resolution {
    /// token 与当前记录匹配。
    Match,
    /// 旧 generation 只能沿已发布的转发目标前进。
    Forward(OwnerToken),
    /// owner 已 retire，消息只能进入 domain injection 或按 retired 路径处理。
    Retired,
    /// directory 中没有该 owner。
    Unknown,
}

/// owner 的互斥字节分类账本。
///
/// `pending`、`reclaimable` 与 `cache` 是 `committed` 的互斥分类：一条记录在任一时刻
/// 恰好属于其中一类或已分配为 live record；limit 判断与诊断都不得重复相加。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct OwnerAccounting {
    pending_return_bytes: u64,
    reclaimable_bytes: u64,
    owner_cache_bytes: u64,
    committed_bytes: u64,
}

impl OwnerAccounting {
    /// 返回待 owner 消费的字节。
    pub(crate) const fn pending_return_bytes(&self) -> u64 {
        self.pending_return_bytes
    }

    /// 返回已确认可复用但尚未进入 free structure 的字节。
    pub(crate) const fn reclaimable_bytes(&self) -> u64 {
        self.reclaimable_bytes
    }

    /// 返回 owner-local cache 中已 commit 但未被 live record 使用的字节。
    pub(crate) const fn owner_cache_bytes(&self) -> u64 {
        self.owner_cache_bytes
    }

    /// 返回本 owner 的 committed 字节。
    pub(crate) const fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    /// commit 新 span 时登记物理占用。
    pub(crate) fn commit(&mut self, bytes: u64) {
        self.committed_bytes += bytes;
        self.owner_cache_bytes += bytes;
    }

    /// 记录进入 staging/inbox 的待消费字节。
    pub(crate) fn stage_pending(&mut self, bytes: u64) {
        self.pending_return_bytes += bytes;
    }

    /// owner 消费消息：pending 转为待复用。
    pub(crate) fn consume_pending(&mut self, bytes: u64) {
        self.pending_return_bytes = self.pending_return_bytes.saturating_sub(bytes);
        self.reclaimable_bytes += bytes;
    }

    /// 记录进入 owner free structure：reclaimable 转为 owner-local cache。
    pub(crate) fn park_reclaimable(&mut self, bytes: u64) {
        self.reclaimable_bytes = self.reclaimable_bytes.saturating_sub(bytes);
        self.owner_cache_bytes += bytes;
    }

    /// 从本地 cache 分配出去。
    pub(crate) fn take_from_cache(&mut self, bytes: u64) {
        self.owner_cache_bytes = self.owner_cache_bytes.saturating_sub(bytes);
    }

    /// 转发消息时把 pending 交给目标 owner 的账本。
    pub(crate) fn forward_pending(&mut self, bytes: u64) {
        self.pending_return_bytes = self.pending_return_bytes.saturating_sub(bytes);
    }

    /// 释放 span：从物理占用中扣除。
    pub(crate) fn release(&mut self, bytes: u64) {
        self.committed_bytes = self.committed_bytes.saturating_sub(bytes);
        self.owner_cache_bytes = self.owner_cache_bytes.saturating_sub(bytes);
    }
}

/// 一个 owner 的稳定记录；保存在 non-moving owner directory 中。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OwnerRecord {
    pub(crate) domain: MemoryDomainId,
    pub(crate) owner_id: OwnerId,
    pub(crate) generation: OwnerGeneration,
    pub(crate) route_key: RouteKey,
    pub(crate) state: OwnerState,
    pub(crate) forward_target: Option<OwnerToken>,
    pub(crate) topology_epoch: Epoch,
    pub(crate) accounting: OwnerAccounting,
}

/// non-moving owner directory；retire 通过发布新记录而不是复用旧编号表达。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OwnerDirectory {
    epoch: Epoch,
    owners: Vec<OwnerRecord>,
}

impl OwnerDirectory {
    /// 创建空的 owner directory。
    pub(crate) fn new(epoch: Epoch) -> Self {
        Self {
            epoch,
            owners: Vec::new(),
        }
    }

    /// 返回 directory 当前的 topology epoch。
    pub(crate) const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// 返回全部 owner 记录，按 owner id 稠密升序。
    pub(crate) fn owners(&self) -> &[OwnerRecord] {
        &self.owners
    }

    /// 返回 owner 数量。
    pub(crate) fn len(&self) -> usize {
        self.owners.len()
    }

    /// 登记一个新 owner；编号单调、generation 从 1 开始、route key 由种子派生。
    pub(crate) fn register(
        &mut self,
        seed: &mut RuntimeSeed,
        domain: MemoryDomainId,
        topology_epoch: Epoch,
    ) -> OwnerToken {
        let owner_id = OwnerId(self.owners.len() as u64);
        let record = OwnerRecord {
            domain,
            owner_id,
            generation: OwnerGeneration(1),
            route_key: RouteKey(seed.next()),
            state: OwnerState::Active,
            forward_target: None,
            topology_epoch,
            accounting: OwnerAccounting::default(),
        };
        self.owners.push(record.clone());
        OwnerToken {
            domain: record.domain,
            owner_id: record.owner_id,
            generation: record.generation,
            route_key: record.route_key,
        }
    }

    /// 按 owner id 取记录。
    pub(crate) fn record(&self, owner_id: OwnerId) -> Option<&OwnerRecord> {
        owner_id.index().and_then(|index| self.owners.get(index))
    }

    /// 按 owner id 取可变记录。
    pub(crate) fn record_mut(&mut self, owner_id: OwnerId) -> Option<&mut OwnerRecord> {
        owner_id
            .index()
            .and_then(|index| self.owners.get_mut(index))
    }

    /// 解析 token：只有与当前记录同时匹配才返回 `Match`。
    pub(crate) fn resolve(&self, token: &OwnerToken) -> Resolution {
        let Some(record) = self.record(token.owner_id) else {
            return Resolution::Unknown;
        };
        if token.matches(record) {
            return match record.state {
                OwnerState::Active | OwnerState::Draining => Resolution::Match,
                OwnerState::Forwarding => record
                    .forward_target
                    .map_or(Resolution::Retired, Resolution::Forward),
                OwnerState::Retired => Resolution::Retired,
            };
        }
        if token.generation < record.generation {
            return record
                .forward_target
                .map_or(Resolution::Retired, Resolution::Forward);
        }
        Resolution::Unknown
    }

    /// retire 第一步：把记录从 `Active` 转为 `Draining` 并发布 retire epoch。
    pub(crate) fn begin_drain(
        &mut self,
        token: &OwnerToken,
        epoch: Epoch,
    ) -> Result<(), RawInvariant> {
        let record = self
            .record_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("drain 请求引用未知 owner"))?;
        if !token.matches(record) {
            return Err(RawInvariant::new("drain 请求的 generation 已过期"));
        }
        if record.state != OwnerState::Active {
            return Err(RawInvariant::new("owner 已经进入 Draining 或更晚状态"));
        }
        record.state = OwnerState::Draining;
        record.topology_epoch = epoch;
        self.epoch = epoch;
        Ok(())
    }

    /// retire 第三步：发布 `Forwarding` 记录并指定新的 owner 或 domain injection。
    pub(crate) fn begin_forward(
        &mut self,
        token: &OwnerToken,
        target: OwnerToken,
        epoch: Epoch,
    ) -> Result<(), RawInvariant> {
        if target.owner_id == token.owner_id && target.generation == token.generation {
            return Err(RawInvariant::new("owner 不能把自己作为转发目标"));
        }
        let record = self
            .record_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("转发请求引用未知 owner"))?;
        if !token.matches(record) {
            return Err(RawInvariant::new("转发请求的 generation 已过期"));
        }
        if record.state != OwnerState::Draining {
            return Err(RawInvariant::new(
                "owner 必须先进入 Draining 才能发布转发目标",
            ));
        }
        record.state = OwnerState::Forwarding;
        record.forward_target = Some(target);
        record.topology_epoch = epoch;
        self.epoch = epoch;
        Ok(())
    }

    /// retire 第七、八步：释放 descriptor 与路由槽，编号保留但永不复用。
    pub(crate) fn retire(&mut self, token: &OwnerToken, epoch: Epoch) -> Result<(), RawInvariant> {
        let record = self
            .record_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("retire 请求引用未知 owner"))?;
        if !token.matches(record) {
            return Err(RawInvariant::new("retire 请求的 generation 已过期"));
        }
        if !matches!(record.state, OwnerState::Draining | OwnerState::Forwarding) {
            return Err(RawInvariant::new("owner 必须先排空才能 retire"));
        }
        record.state = OwnerState::Retired;
        record.topology_epoch = epoch;
        self.epoch = epoch;
        Ok(())
    }

    /// 取得 owner 的可写账本。
    pub(crate) fn accounting_mut(&mut self, owner_id: OwnerId) -> Option<&mut OwnerAccounting> {
        self.record_mut(owner_id)
            .map(|record| &mut record.accounting)
    }

    /// 取得 owner 账本。
    pub(crate) fn accounting(&self, owner_id: OwnerId) -> Option<&OwnerAccounting> {
        self.record(owner_id).map(|record| &record.accounting)
    }
}

/// 一个 slab 描述符的稠密编号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SlabDescriptorId(u32);

impl SlabDescriptorId {
    /// 返回编号原值。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    /// 由编号原值还原。
    pub(crate) const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// 返回作为表下标的编号。
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// slab 的复用代数；复用同一 span 必须推进 generation。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SlabGeneration(u64);

impl SlabGeneration {
    /// 返回代数原值。
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    /// 由代数原值还原。
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// slab 描述符的状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlabState {
    /// 正常承载 payload。
    Active,
    /// owner 已停止新的本地分配。
    Draining,
    /// 全部 slot 已 free，等待 grace 后回收。
    Reclaiming,
    /// 已归还平台。
    Released,
}

/// slot 的生命周期状态；exactly-once return 依赖这里的唯一状态迁移。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlotState {
    /// 正在被 live record 使用。
    Live,
    /// 记录已死亡，尚未进入 return 流程。
    Dead,
    /// 已赢得 return 线性化点，消息已或即将发布。
    ReturnQueued,
    /// owner 已消费并放回 free structure。
    Returned,
}

/// 一个 raw slab/span 的稳定描述符。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SlabDescriptor {
    pub(crate) domain: MemoryDomainId,
    pub(crate) class: super::size_class::RuntimeSizeClassId,
    pub(crate) slot_stride: u32,
    pub(crate) alignment: u32,
    /// stride 除法常量；decode 用它反推 slot 编号而不执行不可信 modulus。
    pub(crate) division: super::size_class::StrideDivision,
    pub(crate) span_extent: u64,
    /// slot 前置区域能否承载 free-list link。
    pub(crate) link_usable: bool,
    pub(crate) owner: OwnerToken,
    pub(crate) generation: SlabGeneration,
    pub(crate) state: SlabState,
    pub(crate) live: u32,
    pub(crate) queued: u32,
    pub(crate) free: u32,
    pub(crate) committed_bytes: u64,
    /// free 链头；保存 encoded link，绝不放裸地址。
    pub(crate) free_head: Option<u64>,
    pub(crate) bump_cursor: u32,
    pub(crate) pending_returns: u32,
    pub(crate) integrity_secret: u32,
    pub(crate) range: RangeId,
    pub(crate) slab_epoch: Epoch,
}

impl SlabDescriptor {
    /// 返回 slot 总数。
    pub(crate) fn slot_count(&self) -> u32 {
        u32::try_from(self.span_extent / u64::from(self.slot_stride)).expect("slot 数适配 u32")
    }

    /// 返回某个 slot 的字节偏移。
    pub(crate) fn slot_offset(&self, index: u32) -> u64 {
        u64::from(index) * u64::from(self.slot_stride)
    }

    /// 由 span 内字节偏移反推 slot 编号。
    pub(crate) fn index_of(&self, offset: u64) -> u32 {
        u32::try_from(self.division.index_of(offset)).expect("slot 编号适配 u32")
    }

    /// 判断 slot 偏移是否落在本 span 内且满足对齐。
    pub(crate) fn contains_offset(&self, offset: u64, alignment: u64) -> bool {
        offset < self.span_extent
            && alignment.is_power_of_two()
            && offset % alignment == 0
            && offset % u64::from(self.slot_stride) == 0
    }

    /// 判断 slot 编号是否落在本 span 内。
    pub(crate) fn contains_index(&self, index: u32) -> bool {
        index < self.slot_count()
    }
}

/// 一个 raw slot 的稳定身份。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RawSlot {
    pub(crate) descriptor: SlabDescriptorId,
    pub(crate) index: u32,
    pub(crate) generation: SlabGeneration,
}

/// raw slab/span 的描述符表与 slot 存储。
///
/// descriptor、slot 头部字、slot 状态与显式 free 下标都按稠密描述符编号并行存放：点查是
/// 连续内存上的下标访问，不做地址推导，也不使用映射容器。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SlabTable {
    descriptors: Vec<SlabDescriptor>,
    headers: Vec<Vec<u64>>,
    states: Vec<Vec<SlotState>>,
    explicit_free: Vec<Vec<u32>>,
}

impl SlabTable {
    /// 创建空表。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 返回描述符数量。
    pub(crate) fn len(&self) -> usize {
        self.descriptors.len()
    }

    /// 按稠密编号取描述符。
    pub(crate) fn descriptor(&self, id: SlabDescriptorId) -> Option<&SlabDescriptor> {
        self.descriptors.get(id.index())
    }

    /// 按稠密编号取可变描述符。
    pub(crate) fn descriptor_mut(&mut self, id: SlabDescriptorId) -> Option<&mut SlabDescriptor> {
        self.descriptors.get_mut(id.index())
    }

    /// 返回全部描述符的规范顺序。
    pub(crate) fn descriptors(&self) -> &[SlabDescriptor] {
        &self.descriptors
    }

    /// 登记一个新 span；全部 slot 初始为 `Returned` 并由 bump 路径发放。
    pub(crate) fn create(
        &mut self,
        class: &RuntimeSizeClass,
        owner: OwnerToken,
        range: RangeId,
        span_extent: u64,
        integrity_secret: u32,
        slab_epoch: Epoch,
    ) -> Result<SlabDescriptorId, RawInvariant> {
        if span_extent == 0 || span_extent % u64::from(class.slot_stride) != 0 {
            return Err(RawInvariant::new("span extent 不是 class stride 的整数倍"));
        }
        if span_extent > u64::from(u32::MAX) {
            return Err(RawInvariant::new("span extent 超出槽内偏移编码宽度"));
        }
        let id =
            SlabDescriptorId(u32::try_from(self.descriptors.len()).expect("描述符数量适配 u32"));
        let slots =
            u32::try_from(span_extent / u64::from(class.slot_stride)).expect("slot 数量适配 u32");
        self.descriptors.push(SlabDescriptor {
            domain: class.domain,
            class: class.id,
            slot_stride: class.slot_stride,
            alignment: class.alignment,
            division: class.division(),
            span_extent,
            link_usable: class.link_usable,
            owner,
            generation: SlabGeneration(1),
            state: SlabState::Active,
            live: 0,
            queued: 0,
            free: slots,
            committed_bytes: span_extent,
            free_head: None,
            bump_cursor: 0,
            pending_returns: 0,
            integrity_secret,
            range,
            slab_epoch,
        });
        self.headers.push(vec![0; slots as usize]);
        self.states.push(vec![SlotState::Returned; slots as usize]);
        self.explicit_free.push(Vec::new());
        Ok(id)
    }

    /// 返回某个 slot 的当前状态。
    pub(crate) fn state(
        &self,
        id: SlabDescriptorId,
        index: u32,
    ) -> Result<SlotState, RawInvariant> {
        self.states
            .get(id.index())
            .and_then(|states| states.get(index as usize))
            .copied()
            .ok_or_else(|| RawInvariant::new("slot 编号越过 span"))
    }

    /// 执行一次唯一允许的状态迁移；重复迁移报不变量失败。
    pub(crate) fn transition(
        &mut self,
        id: SlabDescriptorId,
        index: u32,
        from: SlotState,
        to: SlotState,
    ) -> Result<(), RawInvariant> {
        let state = self
            .states
            .get_mut(id.index())
            .and_then(|states| states.get_mut(index as usize))
            .ok_or_else(|| RawInvariant::new("slot 编号越过 span"))?;
        if *state != from {
            return Err(RawInvariant::new(format!(
                "slot 状态迁移非法：期望 {from:?}，实际 {state:?}"
            )));
        }
        *state = to;
        Ok(())
    }

    /// 从 class 的 free structure 弹出一个 slot 编号。
    pub(crate) fn pop_free(
        &mut self,
        id: SlabDescriptorId,
        codec: &LinkCodec,
    ) -> Result<Option<u32>, RawInvariant> {
        let descriptor = self
            .descriptors
            .get(id.index())
            .ok_or_else(|| RawInvariant::new("free 请求引用未知 slab"))?
            .clone();
        let Some(word) = descriptor.free_head else {
            if !self.explicit_free[id.index()].is_empty() {
                let index = self.explicit_free[id.index()]
                    .pop()
                    .ok_or_else(|| RawInvariant::new("显式 free 列表为空"))?;
                self.descriptors[id.index()].free -= 1;
                return Ok(Some(index));
            }
            return Ok(None);
        };
        let index = codec.decode(&descriptor, word)?;
        let next = codec.normalize(self.headers[id.index()][index as usize]);
        let record = &mut self.descriptors[id.index()];
        record.free_head = next;
        record.free -= 1;
        Ok(Some(index))
    }

    /// 把一个 slot 放回 class 的 free structure。
    pub(crate) fn push_free(
        &mut self,
        id: SlabDescriptorId,
        index: u32,
        codec: &LinkCodec,
    ) -> Result<(), RawInvariant> {
        let descriptor = self
            .descriptors
            .get(id.index())
            .ok_or_else(|| RawInvariant::new("free 请求引用未知 slab"))?
            .clone();
        if descriptor.link_usable {
            let offset = descriptor.slot_offset(index);
            let word = codec.encode(&descriptor, offset)?;
            self.headers[id.index()][index as usize] =
                descriptor.free_head.unwrap_or(LinkCodec::NULL);
            self.descriptors[id.index()].free_head = Some(word);
        } else {
            self.explicit_free[id.index()].push(index);
        }
        self.descriptors[id.index()].free += 1;
        Ok(())
    }

    /// 返回处于给定状态的 slot 数量。
    pub(crate) fn count_state(&self, state: SlotState) -> u32 {
        u32::try_from(
            self.states
                .iter()
                .flatten()
                .filter(|current| **current == state)
                .count(),
        )
        .expect("slot 数适配 u32")
    }

    /// 校验 free 链完整性：每一跳都必须通过 encoded link 解码，长度不超 span 容量。
    pub(crate) fn verify_free_chain(
        &self,
        id: SlabDescriptorId,
        codec: &LinkCodec,
    ) -> Result<u32, RawInvariant> {
        let descriptor = self
            .descriptors
            .get(id.index())
            .ok_or_else(|| RawInvariant::new("free 链检查引用未知 slab"))?;
        let mut word = descriptor.free_head;
        let mut walked = 0_u32;
        while let Some(current) = word {
            let index = codec.decode(descriptor, current)?;
            if !descriptor.contains_index(index) {
                return Err(RawInvariant::new("free 链跳转到本 slab 之外的 slot"));
            }
            walked += 1;
            if walked > descriptor.slot_count() {
                return Err(RawInvariant::new("free 链长度超过 span 容量"));
            }
            word = codec.normalize(self.headers[id.index()][index as usize]);
        }
        Ok(walked)
    }

    /// 校验描述符计数与 slot 状态一致。
    pub(crate) fn verify(&self) -> Result<(), RawInvariant> {
        for descriptor in &self.descriptors {
            let slots = descriptor.slot_count();
            if descriptor.live + descriptor.free + descriptor.queued != slots {
                return Err(RawInvariant::new(format!(
                    "slab {} 的 live/free/queued 计数与 slot 总数不一致",
                    descriptor.range.raw()
                )));
            }
        }
        Ok(())
    }
}
