//! GC debt、owner credit、pacing 与 pressure drain 的确定性参照实现。
//!
//! 本模块是 `pacing_schema` 契约的运行时对偶：契约固定参数与目录，这里执行固定算法——
//! allocation/mark debt、assist quantum、`gc_cpu_fraction` 滑动 cost window、remark cost
//! budget、evacuation pause budget、pressure hysteresis 与 forced full cycle 的
//! exactly-once 计数。所有计数都是整数 cost unit 与字节，不读宿主时钟、不创建线程。
//!
//! 三个归属边界：
//!
//! 1. debt 只决定自动周期与 assist 的节奏，不决定回收本身；真正的 drain、trim、cycle 由
//!    `world` 在 owner 上下文执行，本模块只发布决定。
//! 2. credit 只统计当前 cycle 尚未归还的在飞占用；「mailbox 为空」不是完成条件，只有
//!    `CreditPlane::converged` 为真才允许 remark 与终止检测。
//! 3. pressure episode 由 runtime owner 线性化：开启、forced cycle 与结束都在本模块内完成
//!    状态迁移，调用方只能按返回值决定的顺序推进。

use super::pacing_schema::{
    ASSIST_OUTCOME_NAMES, EVACUATION_OUTCOME_NAMES, GcPacingRuntimeContract, REMARK_OUTCOME_NAMES,
};

/// pressure 状态；强度顺序与 `PRESSURE_STATE_NAMES` 一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PressureState {
    /// 未进入 episode。
    Steady,
    /// 已开启 episode，正在按有界预算 drain。
    Drain,
    /// 占用已达到 soft limit。
    Emergency,
}

impl PressureState {
    /// 返回状态判别值；与契约状态目录顺序一致。
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Steady => 0,
            Self::Drain => 1,
            Self::Emergency => 2,
        }
    }

    /// 返回状态名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Steady => "steady",
            Self::Drain => "drain",
            Self::Emergency => "emergency",
        }
    }

    /// 是否处于 episode 内。
    pub(crate) const fn in_episode(self) -> bool {
        !matches!(self, Self::Steady)
    }
}

/// 一次 headroom 请求的决定；调用方必须按返回的步骤推进，不能跳过 drain。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeadroomDecision {
    /// 请求已被满足，可以继续分配。
    Granted,
    /// 需要在 owner 上下文执行一次有界 drain，然后用新的 committed 快照重试。
    Drain,
    /// 需要启动本 episode 唯一一次 forced full cycle，然后用新的 committed 快照重试。
    ForcedCycle,
    /// 已用尽 drain 与 forced cycle 仍无法取得 headroom。
    OutOfMemory,
}

/// assist 的结局；与契约 `assist_outcomes` 目录一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AssistOutcome {
    /// 未达到阈值，不需要 assist。
    None,
    /// 完成的工作在 quantum 内。
    WithinQuantum,
    /// 可消费的工作超过 quantum，只偿还了一部分。
    QuantumTruncated,
    /// 没有可消费的 work；不得虚构进度。
    NoWork,
}

impl AssistOutcome {
    /// 返回结局判别值。
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::None => 0,
            Self::WithinQuantum => 1,
            Self::QuantumTruncated => 2,
            Self::NoWork => 3,
        }
    }

    /// 返回结局名。
    pub(crate) const fn name(self) -> &'static str {
        ASSIST_OUTCOME_NAMES[self.index()]
    }
}

/// remark 的结局。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum RemarkOutcome {
    /// 在预算内完成；也是 `PressureDrainReport` 的默认值。
    #[default]
    Complete,
    /// 超过预算，必须发布 continuation 并保持 barrier 开启。
    Continuation,
}

impl RemarkOutcome {
    /// 返回结局判别值。
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Complete => 0,
            Self::Continuation => 1,
        }
    }

    /// 返回结局名。
    pub(crate) const fn name(self) -> &'static str {
        REMARK_OUTCOME_NAMES[self.index()]
    }
}

/// 一次 relocation 预算检查的结局。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EvacuationOutcome {
    /// 完整 footprint 在三个上界内，允许整 block 发布。
    Admit,
    /// 任一上界超出，整 block 延后。
    Defer,
}

impl EvacuationOutcome {
    /// 返回结局判别值。
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Admit => 0,
            Self::Defer => 1,
        }
    }

    /// 返回结局名。
    pub(crate) const fn name(self) -> &'static str {
        EVACUATION_OUTCOME_NAMES[self.index()]
    }
}

/// 一次 pause 预算判定所需的真实工作量度量。
///
/// 它同时服务真实的 relocation 与空 extent 的 trim 批：两者记的都是「本次暂停要付出多少
/// 工作」，但来源不同——relocation 记 payload 复制字节与 root/field 更新数，trim 批记撤销的
/// 已提交字节与 descriptor 数（空 extent 没有字段工作，因此 `fields` 恒为 0）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EvacuationFootprint {
    /// 本次暂停搬动或撤销的字节数。
    pub(crate) bytes: u64,
    /// 需要更新的 exact root 数；trim 批是批内 descriptor 数。
    pub(crate) roots: u32,
    /// 需要更新的字段数；空 extent 的 trim 批恒为 0。
    pub(crate) fields: u32,
}

/// owner credit 的来源；顺序与契约 `credit_sources` 目录一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreditSource {
    /// processor-local remembered-set buffer 中的未 flush 键。
    BarrierBuffer,
    /// 已发布但尚未被 arena owner 消费的 card batch。
    CardMarkBatch,
    /// 已聚合但尚未被取走的跨 block edge delta。
    EdgeDelta,
    /// 尚未由 owner 消费的 return/forwarding 链。
    PendingReturn,
    /// producer staging 中尚未发布的 chain。
    ProducerStaging,
    /// 已 acquire 且尚未归还的 mark owner credit。
    MarkCredit,
    /// 已发布但尚未被目标 owner 消费的 mark ticket。
    MarkMailbox,
    /// 各 owner 本地 mark worklist 的深度之和。
    MarkWorklist,
    /// 在途转发中的 GC 工作消息。
    ForwardingWork,
}

impl CreditSource {
    /// 全部来源；顺序即契约目录顺序。
    pub(crate) const ALL: [Self; 9] = [
        Self::BarrierBuffer,
        Self::CardMarkBatch,
        Self::EdgeDelta,
        Self::PendingReturn,
        Self::ProducerStaging,
        Self::MarkCredit,
        Self::MarkMailbox,
        Self::MarkWorklist,
        Self::ForwardingWork,
    ];

    /// 返回来源判别值。
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::BarrierBuffer => 0,
            Self::CardMarkBatch => 1,
            Self::EdgeDelta => 2,
            Self::PendingReturn => 3,
            Self::ProducerStaging => 4,
            Self::MarkCredit => 5,
            Self::MarkMailbox => 6,
            Self::MarkWorklist => 7,
            Self::ForwardingWork => 8,
        }
    }
}

/// 一个 cycle 的 owner credit 账本。
///
/// credit 是「尚未归还的在飞工作」观测，不是引用计数：每个来源各自登记当前在飞量，只有
/// 全部来源同时归零才能宣布 cycle 收敛。`observe` 由 runtime 在真实交接点用当前物理状态
/// 调用（buffer 未 flush 键数、未消费 batch 数、未取走 delta 数、未消费 return 字节、
/// staging 未发布字节），因此不会出现“接口被定义但没人调用”的平行路径；`epoch` 只前进。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CreditPlane {
    cycle_epoch: u64,
    outstanding: [u64; 9],
    peak: [u64; 9],
    observations: u64,
}

impl Default for CreditPlane {
    fn default() -> Self {
        Self::new(0)
    }
}

impl CreditPlane {
    /// 以给定 cycle epoch 创建空账本。
    pub(crate) const fn new(cycle_epoch: u64) -> Self {
        Self {
            cycle_epoch,
            outstanding: [0; 9],
            peak: [0; 9],
            observations: 0,
        }
    }

    /// 返回当前 cycle epoch。
    pub(crate) const fn cycle_epoch(&self) -> u64 {
        self.cycle_epoch
    }

    /// 返回某个来源当前尚未归还的 credit。
    pub(crate) const fn outstanding(&self, source: CreditSource) -> u64 {
        self.outstanding[source.index()]
    }

    /// 返回某个来源在本 cycle 内的观测峰值。
    pub(crate) const fn peak(&self, source: CreditSource) -> u64 {
        self.peak[source.index()]
    }

    /// 返回尚未归还的 credit 总数，即 `mark_credit_pending` 口径。
    pub(crate) fn pending(&self) -> u64 {
        self.outstanding.iter().copied().sum()
    }

    /// 返回累计观测次数。
    pub(crate) const fn observations(&self) -> u64 {
        self.observations
    }

    /// 所有来源与本地 worklist 是否都已清空。
    ///
    /// 单个 mailbox 或单个来源为空不是完成条件：必须全部归零。
    pub(crate) fn converged(&self) -> bool {
        self.outstanding.iter().all(|items| *items == 0)
    }

    /// 登记一个来源当前的在飞量；重复观测同值不产生新状态。
    pub(crate) fn observe(&mut self, source: CreditSource, items: u64) {
        let slot = source.index();
        self.outstanding[slot] = items;
        if items > self.peak[slot] {
            self.peak[slot] = items;
        }
        self.observations += 1;
    }

    /// 一次性登记全部九个来源；runtime 在 cycle 边界用同一个物理快照调用。
    pub(crate) fn observe_all(&mut self, snapshot: CreditSnapshot) {
        for source in CreditSource::ALL {
            self.observe(source, snapshot.get(source));
        }
    }

    /// 开始新 cycle；仍有未归还 credit 时拒绝。
    pub(crate) fn begin_cycle(&mut self, cycle_epoch: u64) -> Result<(), CreditError> {
        if cycle_epoch <= self.cycle_epoch {
            return Err(CreditError::StaleEpoch {
                current: self.cycle_epoch,
                requested: cycle_epoch,
            });
        }
        if !self.converged() {
            return Err(CreditError::Outstanding {
                pending: self.pending(),
            });
        }
        self.cycle_epoch = cycle_epoch;
        self.peak = [0; 9];
        Ok(())
    }
}

/// 全部 credit 来源的物理在飞量快照。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CreditSnapshot {
    /// processor-local buffer 中尚未 flush 的 card 键数。
    pub(crate) barrier_buffer_keys: u64,
    /// 已发布但尚未被 arena owner 消费的 card batch 数。
    pub(crate) card_mark_batches: u64,
    /// 已聚合但尚未被取走的 edge delta 数。
    pub(crate) edge_deltas: u64,
    /// 尚未由 owner 消费的 return/forwarding 字节。
    pub(crate) pending_return_bytes: u64,
    /// producer staging 中尚未发布的字节。
    pub(crate) staging_bytes: u64,
    /// 已 acquire 且尚未归还的 mark owner credit 数。
    pub(crate) mark_credit: u64,
    /// 已发布但尚未被目标 owner 消费的 mark ticket 数。
    pub(crate) mark_mailbox: u64,
    /// 各 owner 本地 mark worklist 深度之和。
    pub(crate) mark_worklist: u64,
    /// 在途转发中的 GC 工作消息数。
    pub(crate) forwarding_work: u64,
}

impl CreditSnapshot {
    /// 按来源取值；顺序与契约 `credit_sources` 一致。
    pub(crate) const fn get(self, source: CreditSource) -> u64 {
        match source {
            CreditSource::BarrierBuffer => self.barrier_buffer_keys,
            CreditSource::CardMarkBatch => self.card_mark_batches,
            CreditSource::EdgeDelta => self.edge_deltas,
            CreditSource::PendingReturn => self.pending_return_bytes,
            CreditSource::ProducerStaging => self.staging_bytes,
            CreditSource::MarkCredit => self.mark_credit,
            CreditSource::MarkMailbox => self.mark_mailbox,
            CreditSource::MarkWorklist => self.mark_worklist,
            CreditSource::ForwardingWork => self.forwarding_work,
        }
    }
}

/// credit 账本的失败分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreditError {
    /// cycle epoch 只能前进。
    StaleEpoch { current: u64, requested: u64 },
    /// 仍有未归还 credit。
    Outstanding { pending: u64 },
}

impl std::fmt::Display for CreditError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleEpoch { current, requested } => write!(
                formatter,
                "credit cycle epoch 只能前进：当前 {current}，请求 {requested}"
            ),
            Self::Outstanding { pending } => {
                write!(formatter, "credit 尚未收敛：仍有 {pending} 项在飞")
            }
        }
    }
}

/// 一个 episode 的统计。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PressureEpisodeStats {
    /// episode 序号；0 表示尚未开启过。
    pub(crate) epoch: u64,
    /// 本 episode 已执行的 forced full cycle 数；至多为 1。
    pub(crate) forced_cycles: u64,
    /// 本 episode 已返回过 `Drain` headroom 决策的次数；至多为 1。
    pub(crate) headroom_drains: u32,
    /// 已完成的真实 owner drain 次数。
    pub(crate) drains: u64,
    /// 是否已经各自完成一次 pending/cache/reclaimable 分类 drain。
    pub(crate) classes_drained: [bool; 3],
}

impl PressureEpisodeStats {
    /// 是否已经完成三类分类 drain。
    pub(crate) const fn all_classes_drained(&self) -> bool {
        self.classes_drained[0] && self.classes_drained[1] && self.classes_drained[2]
    }
}

/// committed 字节的三类可 drain 分类快照。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CommittedClasses {
    /// 尚未由 owner 消费的 pending 字节。
    pub(crate) pending_return_bytes: u64,
    /// owner-local cache 中已 commit 的字节。
    pub(crate) owner_cache_bytes: u64,
    /// 已确认可复用但尚未进入 free structure 的字节。
    pub(crate) reclaimable_bytes: u64,
}

impl CommittedClasses {
    /// 按契约分类顺序索引取值。
    pub(crate) const fn get(self, index: usize) -> u64 {
        match index {
            0 => self.pending_return_bytes,
            1 => self.owner_cache_bytes,
            _ => self.reclaimable_bytes,
        }
    }

    /// 是否某个分类已经归零，因而「已完成一次 owner drain」。
    pub(crate) const fn drained(self, index: usize) -> bool {
        self.get(index) == 0
    }
}

/// 一个 cycle 内真实完成工作的累计计数器快照。
///
/// 四个来源都是只增不减的全局累计量（processor card 记账、owner 取走的 edge delta 数、
/// 发布的 batch 数、mark cycle 累计标记数）；per-cycle 工作量由相邻两次快照之差得到，
/// 不引入第二套计数口径。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct GcWorkCounters {
    /// processor 账本累计的 card mark 次数。
    pub(crate) card_marks: u64,
    /// owner 已取走的 edge delta 累计数。
    pub(crate) edge_deltas: u64,
    /// 已发布的 card batch 累计数。
    pub(crate) published_batches: u64,
    /// mark cycle 累计标记的对象数。
    pub(crate) marks: u64,
}

impl GcWorkCounters {
    /// 返回相对 `baseline` 的 per-cycle 工作量；累计量只增，因此饱和减法就是真实差值。
    pub(crate) fn since(self, baseline: Self) -> u64 {
        self.card_marks
            .saturating_sub(baseline.card_marks)
            .saturating_add(self.edge_deltas.saturating_sub(baseline.edge_deltas))
            .saturating_add(
                self.published_batches
                    .saturating_sub(baseline.published_batches),
            )
            .saturating_add(self.marks.saturating_sub(baseline.marks))
    }
}

/// pacing 与 pressure 的执行平面。
///
/// 字段按「cycle 账本」「cost window」「pressure episode」「统计」四组排列；所有组都只有
/// 定点整数，不含堆分配，使契约参数与运行时状态一一对应。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PacingPlane {
    contract: GcPacingRuntimeContract,
    /// `GUGU_RUNTIME_GC_TARGET` 的百分数；`None` 表示 `off`。
    target_percent: Option<u32>,
    /// `GUGU_RUNTIME_MEMORY_LIMIT` 的软上限；未配置时 pressure debt 恒为 0。
    soft_memory_limit: Option<u64>,
    last_live_bytes: u64,
    allocated_since_cycle_bytes: u64,
    /// 缓存的增长预算；`complete_cycle` 与 `set_target_percent` 维护，慢路径只做标量比较。
    growth_budget_bytes: u64,
    pending_mark_work: u64,
    /// 本 cycle 已完成工作的计数器基线；per-cycle 工作量由两次快照之差得到。
    work_baseline: GcWorkCounters,
    credits: CreditPlane,
    gc_cpu_consumed: u64,
    gc_cpu_window_epoch: u64,
    /// 最近一次真实快照的 committed 估计；其后每次分配累加，低水位快路不做全局读取。
    committed_estimate: u64,
    /// 距上次真实 committed 快照的累计分配字节。
    allocated_since_poll_bytes: u64,
    /// 距上次 episode 内有界 drain 的累计分配字节。
    allocated_since_drain_bytes: u64,
    /// 距上次自动 cycle 尝试的累计分配字节。
    allocated_since_cycle_attempt_bytes: u64,
    /// episode 内是否到了执行一次有界 drain 的节奏点。
    pressure_drain_due: bool,
    /// 真实 committed 快照的观测次数；用于锁定慢路径不读全局状态。
    pressure_snapshots: u64,
    state: PressureState,
    episode: PressureEpisodeStats,
    forced_cycle_total: u64,
    assists: u64,
    assist_cost: u64,
    assist_by_outcome: [u64; 4],
    remark_continuations: u64,
    evacuation_admits: u64,
    evacuation_defers: u64,
    headroom_oom: u64,
    /// 最近一次 pressure 快照的 committed 字节；dump 用它重算 pressure debt。
    last_committed_bytes: u64,
    /// 最近一次 pressure 快照的三类分类；dump 用它重算 return pressure。
    last_classes: CommittedClasses,
}

impl Default for PacingPlane {
    fn default() -> Self {
        Self::new(GcPacingRuntimeContract::build(Default::default()).expect("内建 pacing 契约自洽"))
    }
}

impl PacingPlane {
    /// 以已验证契约创建平面；默认不设软上限、按 100% 增长目标。
    pub(crate) fn new(contract: GcPacingRuntimeContract) -> Self {
        let growth_budget_bytes = contract.min_growth_budget();
        Self {
            contract,
            target_percent: Some(100),
            soft_memory_limit: None,
            last_live_bytes: 0,
            allocated_since_cycle_bytes: 0,
            growth_budget_bytes,
            pending_mark_work: 0,
            work_baseline: GcWorkCounters::default(),
            credits: CreditPlane::default(),
            gc_cpu_consumed: 0,
            gc_cpu_window_epoch: 0,
            committed_estimate: 0,
            allocated_since_poll_bytes: 0,
            allocated_since_drain_bytes: 0,
            allocated_since_cycle_attempt_bytes: 0,
            pressure_drain_due: false,
            pressure_snapshots: 0,
            state: PressureState::Steady,
            episode: PressureEpisodeStats::default(),
            forced_cycle_total: 0,
            assists: 0,
            assist_cost: 0,
            assist_by_outcome: [0; 4],
            remark_continuations: 0,
            evacuation_admits: 0,
            evacuation_defers: 0,
            headroom_oom: 0,
            last_committed_bytes: 0,
            last_classes: CommittedClasses::default(),
        }
    }

    /// 返回契约。
    pub(crate) const fn contract(&self) -> &GcPacingRuntimeContract {
        &self.contract
    }

    /// 返回 credit 账本。
    pub(crate) const fn credits(&self) -> &CreditPlane {
        &self.credits
    }

    /// 返回 cycle credit 是否已经全部收敛；world 与终止检测读取。
    pub(crate) fn credits_converged(&self) -> bool {
        self.credits.converged()
    }

    /// 用一个物理在飞量快照更新全部 credit 来源；runtime 在 cycle 边界与交接点调用。
    pub(crate) fn observe_credits(&mut self, snapshot: CreditSnapshot) {
        self.credits.observe_all(snapshot);
    }

    /// 设置自动 GC 增长目标的百分数；`None` 等价于 `GcTarget::Off`。
    pub(crate) fn set_target_percent(&mut self, target: Option<u32>) {
        self.target_percent = target;
        self.growth_budget_bytes = self.compute_growth_budget(self.last_live_bytes);
    }

    /// 返回自动 GC 增长目标。
    pub(crate) const fn target_percent(&self) -> Option<u32> {
        self.target_percent
    }

    /// 设置 runtime 管理内存的软上限。
    pub(crate) fn set_soft_memory_limit(&mut self, limit: Option<u64>) {
        self.soft_memory_limit = limit;
    }

    /// 返回软上限。
    pub(crate) const fn soft_memory_limit(&self) -> Option<u64> {
        self.soft_memory_limit
    }

    /// 返回当前 pressure 状态。
    pub(crate) const fn state(&self) -> PressureState {
        self.state
    }

    /// 返回当前 episode 统计。
    pub(crate) const fn episode(&self) -> PressureEpisodeStats {
        self.episode
    }

    /// 返回本次 episode 之后累计执行的 forced full cycle 数。
    pub(crate) const fn forced_cycle_total(&self) -> u64 {
        self.forced_cycle_total
    }

    /// 返回 `mark_credit_pending` 口径。
    pub(crate) fn mark_credit_pending(&self) -> u64 {
        self.credits.pending()
    }

    /// 返回累计 assist 次数。
    pub(crate) const fn assists(&self) -> u64 {
        self.assists
    }

    /// 返回累计 assist 偿还的 cost unit。
    pub(crate) const fn assist_cost(&self) -> u64 {
        self.assist_cost
    }

    /// 返回四种 assist 结局的累计计数。
    pub(crate) const fn assist_by_outcome(&self) -> [u64; 4] {
        self.assist_by_outcome
    }

    /// 返回累计 remark continuation 数。
    pub(crate) const fn remark_continuations(&self) -> u64 {
        self.remark_continuations
    }

    /// 返回 relocation 允许与延后的累计数。
    pub(crate) const fn evacuation_counts(&self) -> (u64, u64) {
        (self.evacuation_admits, self.evacuation_defers)
    }

    /// 返回因无法取得 headroom 而进入 `OutOfMemory` 的次数。
    pub(crate) const fn headroom_oom(&self) -> u64 {
        self.headroom_oom
    }

    /// 返回窗口内已消费的 GC cost unit。
    pub(crate) const fn gc_cpu_consumed(&self) -> u64 {
        self.gc_cpu_consumed
    }

    /// 返回自上次完成 cycle 后累计分配的字节。
    pub(crate) const fn allocated_since_cycle_bytes(&self) -> u64 {
        self.allocated_since_cycle_bytes
    }

    /// 返回上次完成 cycle 时的存活字节。
    pub(crate) const fn last_live_bytes(&self) -> u64 {
        self.last_live_bytes
    }

    /// 返回最近一次真实快照的 committed 估计；其后每次分配都累加。
    pub(crate) const fn committed_estimate(&self) -> u64 {
        self.committed_estimate
    }

    /// 返回真实 committed 快照的观测次数。
    pub(crate) const fn pressure_snapshots(&self) -> u64 {
        self.pressure_snapshots
    }

    /// 登记一次分配；只累计 debt 与估计，不做任何全局读取。
    pub(crate) fn observe_allocation(&mut self, bytes: u64) {
        self.allocated_since_cycle_bytes = self.allocated_since_cycle_bytes.saturating_add(bytes);
        self.committed_estimate = self.committed_estimate.saturating_add(bytes);
        self.allocated_since_poll_bytes = self.allocated_since_poll_bytes.saturating_add(bytes);
        self.allocated_since_drain_bytes = self.allocated_since_drain_bytes.saturating_add(bytes);
        self.allocated_since_cycle_attempt_bytes = self
            .allocated_since_cycle_attempt_bytes
            .saturating_add(bytes);
    }

    /// 登记尚未消费的 mark 工作。
    pub(crate) fn observe_mark_work(&mut self, cost: u64) {
        self.pending_mark_work = self.pending_mark_work.saturating_add(cost);
    }

    /// 计算 `max(min_growth_budget, floor(live × target / 100))`；`GcTarget::Off` 取下限。
    fn compute_growth_budget(&self, live_bytes: u64) -> u64 {
        let target = self.target_percent.map_or(0, |percent| {
            u64::from(percent).saturating_mul(live_bytes) / 100
        });
        self.contract.min_growth_budget().max(target)
    }

    /// 返回增长预算：`max(min_growth_budget, floor(last_live × target / 100))`。
    pub(crate) fn growth_budget(&self) -> u64 {
        self.compute_growth_budget(self.last_live_bytes)
    }

    /// 返回缓存的增长预算；与 `growth_budget` 同值，供零全局读取的慢路径门禁使用。
    pub(crate) const fn growth_budget_cached(&self) -> u64 {
        self.growth_budget_bytes
    }

    /// 返回 allocation debt。
    pub(crate) fn allocation_debt(&self) -> u64 {
        self.allocated_since_cycle_bytes
            .saturating_sub(self.growth_budget_bytes)
    }

    /// 返回 mark debt：allocation debt 折算的 cost unit 加尚未消费的 mark 工作。
    pub(crate) fn mark_debt(&self) -> u64 {
        self.allocation_debt()
            .saturating_mul(u64::from(self.contract.mark_cost_per_byte()))
            .saturating_add(self.pending_mark_work)
    }

    /// 返回 pressure debt；未配置 soft limit 时恒为 0。
    pub(crate) fn pressure_debt(&self, committed: u64) -> u64 {
        match self.soft_memory_limit {
            Some(limit) => committed.saturating_sub(limit),
            None => 0,
        }
    }

    /// 返回 return pressure：尚未消费的 pending 与 owner cache 之和。
    pub(crate) const fn return_pressure(classes: CommittedClasses) -> u64 {
        classes
            .pending_return_bytes
            .saturating_add(classes.owner_cache_bytes)
    }

    /// 判断 allocation debt 是否应当触发一次自动 cycle。
    ///
    /// `GcTarget::Off` 只关闭这条触发条件，不关闭 memory limit、pending 回收与 OOM 规则。
    pub(crate) fn should_start_cycle(&self) -> bool {
        self.target_percent.is_some() && self.allocation_debt() > 0
    }

    /// 返回 soft limit 的 enter 水位。
    fn enter_watermark(&self) -> Option<u64> {
        self.soft_memory_limit.map(|limit| {
            limit.saturating_mul(u64::from(self.contract.pressure_enter_ratio())) / 100
        })
    }

    /// 缓存的 committed 估计是否已经触达 enter 水位。
    fn enter_reached(&self) -> bool {
        self.enter_watermark()
            .is_some_and(|enter| self.committed_estimate >= enter)
    }

    /// 是否应当刷新一次真实 committed 快照：处于 episode、估计触达 enter 水位，或距上次
    /// 快照又分配了 `pressure_poll_bytes`。三者都是标量比较，低水位时不产生全局读取。
    pub(crate) fn pressure_poll_due(&self) -> bool {
        self.state.in_episode()
            || self.enter_reached()
            || self.allocated_since_poll_bytes >= self.contract.pressure_poll_bytes()
    }

    /// 按缓存估计判断 headroom 是否可能不足；为真时调用方必须先刷新真实快照再决定。
    pub(crate) fn headroom_may_fail(&self, bytes: u64) -> bool {
        self.soft_memory_limit
            .is_some_and(|limit| self.committed_estimate.saturating_add(bytes) > limit)
    }

    /// 是否应当借一次 assist：mark debt 达阈值且窗口还有额度。
    pub(crate) fn assist_due(&self) -> bool {
        self.mark_debt() >= self.contract.assist_threshold() && self.gc_cpu_allowance() > 0
    }

    /// 取走一次 episode 内有界 drain 的节奏点；episode 开启与升级会立即臂化它。
    pub(crate) fn take_pressure_drain(&mut self) -> bool {
        let cadence =
            self.allocated_since_drain_bytes >= self.contract.owner_drain_interval_bytes();
        if !(self.state.in_episode() && (self.pressure_drain_due || cadence)) {
            return false;
        }
        self.pressure_drain_due = false;
        self.allocated_since_drain_bytes = 0;
        true
    }

    /// 取走一次自动 cycle 的尝试机会；两次尝试之间至少间隔 `min_growth_budget` 的分配量，
    /// 因此未完成的 cycle 会按节奏退避，而不是在每次分配上重扫整堆。
    pub(crate) fn take_cycle_attempt(&mut self) -> bool {
        if !self.should_start_cycle()
            || self.allocated_since_cycle_attempt_bytes < self.contract.min_growth_budget()
        {
            return false;
        }
        self.allocated_since_cycle_attempt_bytes = 0;
        true
    }

    /// 返回相对基线推进的 per-cycle 工作量。
    pub(crate) fn cycle_work_cost(&self, counters: GcWorkCounters) -> u64 {
        counters.since(self.work_baseline)
    }

    /// 完成一次 cycle：记录存活字节与工作量基线，只重设 cycle 域状态。
    ///
    /// `pending_mark_work` 是「拖欠的 mark 工作」，跨 cycle 存活并由后续 assist 归还；
    /// 在这里清零会让「超窗工作转为 debt 再偿还」的机制在同一个 drain 内被抹掉。
    pub(crate) fn complete_cycle(&mut self, live_bytes: u64, counters: GcWorkCounters) {
        self.last_live_bytes = live_bytes;
        self.growth_budget_bytes = self.compute_growth_budget(live_bytes);
        self.allocated_since_cycle_bytes = 0;
        self.allocated_since_cycle_attempt_bytes = 0;
        self.work_baseline = counters;
        self.gc_cpu_consumed = 0;
        self.gc_cpu_window_epoch = self.gc_cpu_window_epoch.saturating_add(1);
    }

    /// 返回本次 cycle 已消费的 GC cost unit 是否已经用尽窗口额度。
    pub(crate) const fn window_exhausted(&self) -> bool {
        self.gc_cpu_consumed >= self.contract.gc_cpu_window_budget()
    }

    /// 是否需要在本次分配上执行慢路径探测。
    ///
    /// 全部条件都是标量比较：窗口溢出、处于 episode、到了刷新真实快照的节奏、本 cycle
    /// 分配量已达缓存的增长预算，或估计触达 soft limit 的 enter 水位。都不成立时普通分配
    /// 不付出任何全局查询。
    pub(crate) fn slow_edge_due(&self) -> bool {
        self.window_exhausted()
            || self.pressure_poll_due()
            || self.allocated_since_cycle_bytes >= self.growth_budget_bytes
    }

    /// 在一个 slow edge 上执行一次 assist。
    ///
    /// 只有调用方真实完成并交接的工作才减少 `mark_debt`：`available` 以 cost unit 计，
    /// 一张 dirty card 折算 `CARD_GRANULARITY_BYTES` 个 mark cost unit（与 barrier 契约同源）。
    /// 没有可消费 work 时返回 `NoWork` 且不改变账本，避免虚构进度。
    pub(crate) fn assist(&mut self, available: u64) -> AssistOutcome {
        let debt = self.mark_debt();
        if debt < self.contract.assist_threshold() {
            self.assist_by_outcome[AssistOutcome::None.index()] += 1;
            return AssistOutcome::None;
        }
        if available == 0 {
            self.assist_by_outcome[AssistOutcome::NoWork.index()] += 1;
            return AssistOutcome::NoWork;
        }
        self.assists += 1;
        let quantum = self.contract.assist_quantum();
        let consumed = available
            .min(quantum)
            .min(debt)
            .min(self.gc_cpu_allowance());
        if consumed == 0 {
            // 窗口额度已用尽：这一轮没有实际进度，必须如实报告而不是记账。
            self.assists -= 1;
            self.assist_by_outcome[AssistOutcome::NoWork.index()] += 1;
            return AssistOutcome::NoWork;
        }
        self.assist_cost = self.assist_cost.saturating_add(consumed);
        self.gc_cpu_consumed = self.gc_cpu_consumed.saturating_add(consumed);
        // cost unit 先冲抵拖欠的 mark 工作，余量才按 mark_cost_per_byte 折成 allocation
        // 字节：两项各扣一次会让债务以两倍速度归还。不足一字节的余量留在债务里（偏保守）。
        let from_pending = consumed.min(self.pending_mark_work);
        self.pending_mark_work -= from_pending;
        let remaining = consumed - from_pending;
        self.allocated_since_cycle_bytes = self
            .allocated_since_cycle_bytes
            .saturating_sub(remaining / u64::from(self.contract.mark_cost_per_byte()));
        let outcome = if available > quantum {
            AssistOutcome::QuantumTruncated
        } else {
            AssistOutcome::WithinQuantum
        };
        self.assist_by_outcome[outcome.index()] += 1;
        outcome
    }

    /// 返回当前窗口仍允许 GC 消费的 cost unit。
    pub(crate) fn gc_cpu_allowance(&self) -> u64 {
        self.contract
            .gc_cpu_window_budget()
            .saturating_sub(self.gc_cpu_consumed)
    }

    /// 普通 GC worker 工作：超出窗口预算时转为 debt，由后续 assist 偿还。
    ///
    /// 返回实际消费的 cost unit；不足的部分由调用方登记为 `observe_mark_work`。
    pub(crate) fn worker_work(&mut self, cost: u64, emergency: bool) -> u64 {
        // emergency drain 可以暂时越过吞吐预算来恢复内存安全，但不能跳过任何校验。
        let allowance = if emergency {
            u64::MAX
        } else {
            self.gc_cpu_allowance()
        };
        let consumed = cost.min(allowance);
        self.gc_cpu_consumed = self.gc_cpu_consumed.saturating_add(consumed);
        let deferred = cost.saturating_sub(consumed);
        if deferred > 0 {
            self.observe_mark_work(deferred);
        }
        consumed
    }

    /// 执行一次 remark。
    ///
    /// barrier 必须先处于开启状态；mark 阶段尚未收敛时发布 continuation，超预算时同样发布
    /// continuation：两种情况都不恢复普通 barrier，也不允许在此宣布 cycle 收敛。
    pub(crate) fn remark(
        &mut self,
        cost: u64,
        barrier_open: bool,
        mark_converged: bool,
    ) -> Result<RemarkOutcome, &'static str> {
        if !barrier_open {
            return Err("remark 要求 hybrid barrier 处于开启状态");
        }
        if !mark_converged || cost > self.contract.remark_cost_budget() {
            self.remark_continuations += 1;
            Ok(RemarkOutcome::Continuation)
        } else {
            Ok(RemarkOutcome::Complete)
        }
    }

    /// 判定一次 pause 的 footprint 是否整块落在所有上界内。
    ///
    /// 任一维度越界就整批延后，不允许部分发布；调用方负责给出真实度量，不得用估算值。
    pub(crate) fn evacuation(&mut self, footprint: EvacuationFootprint) -> EvacuationOutcome {
        let admit = footprint.bytes <= self.contract.evacuation_pause_bytes()
            && footprint.roots <= self.contract.evacuation_pause_roots()
            && footprint.fields <= self.contract.evacuation_pause_fields();
        if admit {
            self.evacuation_admits += 1;
            EvacuationOutcome::Admit
        } else {
            self.evacuation_defers += 1;
            EvacuationOutcome::Defer
        }
    }

    /// 推进一个 cycle 边界：credit 必须先收敛，然后按新 epoch 重建账本。
    pub(crate) fn begin_cycle(&mut self, cycle_epoch: u64) -> Result<(), CreditError> {
        self.credits.begin_cycle(cycle_epoch)
    }

    /// 按 committed 快照推进 pressure 状态机。
    ///
    /// `committed` 是 `heap_committed_bytes + runtime_committed_bytes`；`classes` 是 runtime
    /// committed 的三类分类。开启条件达到 enter 水位，结束条件必须同时满足 clear 水位与三类
    /// 分类各自完成一次 drain。
    pub(crate) fn update_pressure(
        &mut self,
        committed: u64,
        classes: CommittedClasses,
    ) -> PressureState {
        // 快照先落地：dump、debt 重算与估计都读同一份观测，轮询节奏也在此复位。
        self.last_committed_bytes = committed;
        self.last_classes = classes;
        self.committed_estimate = committed;
        self.allocated_since_poll_bytes = 0;
        self.pressure_snapshots = self.pressure_snapshots.saturating_add(1);
        let Some(limit) = self.soft_memory_limit else {
            self.state = PressureState::Steady;
            return self.state;
        };
        let enter = limit.saturating_mul(u64::from(self.contract.pressure_enter_ratio())) / 100;
        let clear = limit.saturating_mul(u64::from(self.contract.pressure_clear_ratio())) / 100;
        if committed >= limit {
            if !self.state.in_episode() {
                self.open_episode();
            }
            self.state = PressureState::Emergency;
            return self.state;
        }
        if !self.state.in_episode() {
            if committed >= enter {
                self.open_episode();
                self.state = PressureState::Drain;
            }
            return self.state;
        }
        // 结束条件：降到 clear 水位以下，且三类分类都已经过一次真实 owner drain 覆盖。
        // 「当前快照为 0」不算 drain 证据——那会让本来为空的分类不经过任何 drain 就算完成。
        if committed < clear && self.episode.drains > 0 && self.episode.all_classes_drained() {
            let epoch = self.episode.epoch;
            let drains = self.episode.drains;
            self.state = PressureState::Steady;
            // episode 结束后 forced cycle 与 headroom 机会复位，下一次 episode 才有权再用。
            self.episode = PressureEpisodeStats {
                epoch,
                forced_cycles: 0,
                headroom_drains: 0,
                drains,
                classes_drained: [false; 3],
            };
            self.pressure_drain_due = false;
            self.allocated_since_drain_bytes = 0;
            return self.state;
        }
        if self.state == PressureState::Emergency && committed < limit {
            self.state = PressureState::Drain;
        }
        self.state
    }

    /// 登记一次 owner drain 的分类结果。
    pub(crate) fn note_drained_classes(&mut self, classes: CommittedClasses) {
        for index in 0..3 {
            if classes.drained(index) {
                self.episode.classes_drained[index] = true;
            }
        }
    }

    /// 推进 headroom 请求；调用方必须按返回值执行真实 drain 与 forced cycle。
    ///
    /// 未配置软上限时直接 `Granted`，平台分配失败仍按统一 `OutOfMemory` 处理。已配置上限时，
    /// 每个 episode 至多返回一次 `ForcedCycle`；两次机会用尽仍不满足才返回 `OutOfMemory`。
    pub(crate) fn request_headroom(&mut self, committed: u64, bytes: u64) -> HeadroomDecision {
        let Some(limit) = self.soft_memory_limit else {
            return HeadroomDecision::Granted;
        };
        let requested = committed.saturating_add(bytes);
        if requested <= limit {
            return HeadroomDecision::Granted;
        }
        if !self.state.in_episode() {
            self.open_episode();
        }
        // 水位口径与 `update_pressure` 一致：占用达到 soft limit 才是 Emergency，否则只是
        // Drain；用 `requested` 判定会让所有超出请求都被记成 Emergency。
        self.state = if committed >= limit {
            PressureState::Emergency
        } else {
            PressureState::Drain
        };
        // headroom 的两次机会用独立计数，不借用真实 owner drain 的次数。
        if self.episode.headroom_drains == 0 {
            self.episode.headroom_drains = 1;
            return HeadroomDecision::Drain;
        }
        if self.episode.forced_cycles == 0 {
            self.episode.forced_cycles = 1;
            self.forced_cycle_total += 1;
            return HeadroomDecision::ForcedCycle;
        }
        self.headroom_oom += 1;
        HeadroomDecision::OutOfMemory
    }

    /// 登记一次完成的 owner drain；返回 drain 覆盖的分类数。
    pub(crate) fn note_drain(&mut self, classes: CommittedClasses) -> u32 {
        // drain 后的分类快照就是 dump 与 return pressure 的最新物理事实。
        self.last_classes = classes;
        self.episode.drains = self.episode.drains.saturating_add(1);
        let before = self.episode.classes_drained;
        self.note_drained_classes(classes);
        self.episode
            .classes_drained
            .iter()
            .zip(before)
            .filter(|(now, was)| **now && !was)
            .count() as u32
    }

    /// 开启一个新 episode：分配 epoch 并复位 episode 级标记。
    fn open_episode(&mut self) {
        self.episode.epoch = self.episode.epoch.saturating_add(1);
        self.episode.forced_cycles = 0;
        self.episode.drains = 0;
        self.episode.classes_drained = [false; 3];
        self.episode.headroom_drains = 0;
        // episode 一开启就要求一次真实 drain，此后按分配节奏推进。
        self.pressure_drain_due = true;
        self.allocated_since_drain_bytes = 0;
    }

    /// 返回固定文本 dump；不含地址与宿主信息。
    pub(crate) fn dump(&self) -> String {
        use std::fmt::Write;
        let mut output = String::new();
        writeln!(
            output,
            "pacing-state target={} limit={} state={} episode={} forced={} drains={}",
            self.target_percent
                .map_or_else(|| "off".to_owned(), |percent| percent.to_string()),
            self.soft_memory_limit
                .map_or_else(|| "off".to_owned(), |limit| limit.to_string()),
            self.state.name(),
            self.episode.epoch,
            self.episode.forced_cycles,
            self.episode.drains,
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-debt allocated={} last-live={} growth-budget={} allocation-debt={} mark-debt={}",
            self.allocated_since_cycle_bytes,
            self.last_live_bytes,
            self.growth_budget(),
            self.allocation_debt(),
            self.mark_debt(),
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-pressure committed={} pressure-debt={} return-pressure={} mark-credit={}",
            self.last_committed_bytes,
            self.pressure_debt(self.last_committed_bytes),
            Self::return_pressure(self.last_classes),
            self.mark_credit_pending(),
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-drain due={} poll-bytes={} attempt-bytes={} interval-bytes={} snapshots={}",
            self.pressure_drain_due,
            self.allocated_since_poll_bytes,
            self.allocated_since_cycle_attempt_bytes,
            self.allocated_since_drain_bytes,
            self.pressure_snapshots,
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-credit epoch={} pending={} observations={}",
            self.credits.cycle_epoch(),
            self.credits.pending(),
            self.credits.observations(),
        )
        .expect("String写入");
        for source in CreditSource::ALL {
            writeln!(
                output,
                "pacing-credit-source {} outstanding={}",
                super::pacing_schema::CREDIT_SOURCE_NAMES[source.index()],
                self.credits.outstanding(source),
            )
            .expect("String写入");
        }
        writeln!(
            output,
            "pacing-assists total={} cost={} none={} within-quantum={} quantum-truncated={} no-work={}",
            self.assists(),
            self.assist_cost(),
            self.assist_by_outcome()[AssistOutcome::None.index()],
            self.assist_by_outcome()[AssistOutcome::WithinQuantum.index()],
            self.assist_by_outcome()[AssistOutcome::QuantumTruncated.index()],
            self.assist_by_outcome()[AssistOutcome::NoWork.index()],
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-outcomes remark-continuations={} evacuation-admit={} evacuation-defer={} headroom-oom={} forced-cycles={}",
            self.remark_continuations(),
            self.evacuation_counts().0,
            self.evacuation_counts().1,
            self.headroom_oom(),
            self.forced_cycle_total(),
        )
        .expect("String写入");
        writeln!(
            output,
            "pacing-gc-cpu consumed={} window={} budget={}",
            self.gc_cpu_consumed,
            self.gc_cpu_window_epoch,
            self.contract.gc_cpu_window_budget(),
        )
        .expect("String写入");
        output
    }
}
