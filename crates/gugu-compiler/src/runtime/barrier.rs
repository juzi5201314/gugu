//! hybrid write barrier、remembered set 与 edge summary 的确定性参照实现。
//!
//! 本模块是 compiler 侧持有的参照行为：`LogicalProcessor` 的 owner-local `CardMarkBuffer`
//! 与 dedup stamp 表、arena 的 512-byte 粒度 card table、跨 owner 的 `CardMarkBatch` 组装
//! 以及 owner-local edge summary。它固定 schema 与 verifier 对应的状态机，不进入镜像执行
//! 路径，也不复制正常执行路径。
//!
//! 三个不变量在这里被显式维护并被测试断言：
//!
//! 1. **store 先于账本**：`HybridBarrier::perform` 的第 4 步才是实际 field store，第 5 步
//!    才把 card 键写进账本；`HybridBarrierOutcome::steps` 是这一顺序的可检查证据。
//! 2. **dedup 只丢 stamp，不丢键**：stamp 命中条件是该 stamp 当前指向的 entry 仍然等于
//!    该键；conflict 只会覆盖 stamp 槽，已经进入 buffer 的键继续留在 entries 里。
//! 3. **card table 只由 arena owner 写**：mutator 只往自己的 buffer 追加，flush 之后按
//!    `(arena, generation, cycle epoch)` 分组发布 batch；owner 以 Acquire 消费后才写
//!    card byte，重复置位幂等。

use std::collections::BTreeMap;

use super::barrier_schema::{
    CARD_GRANULARITY_BYTES, CARD_MARK_BUFFER_ENTRIES, CARD_MARK_STAMP_ENTRIES, EDGE_BUFFER_ENTRIES,
    EDGE_DELTAS_PER_WRITE, card_index, stamp_slot,
};
use super::local_heap::{BlockRef, ManagedBlockId};
use super::slab::{OwnerToken, RawInvariant};

/// hybrid barrier 的规范步骤；顺序即执行顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum HybridStep {
    /// 读取被覆盖位置的旧值。
    ReadOld,
    /// 标记开启且旧值非空时 shade 旧 target（Yuasa deletion）。
    ShadeOldDeleted,
    /// 当前 stack 仍为 grey 且新值非空时 shade 新 target（Dijkstra insertion）。
    ShadeNewInserted,
    /// 实际 field store。
    Store,
    /// 把 card 键写进 barrier 账本。
    CardMark,
    /// 按本地 card/line summary 聚合 `EdgeAdd`/`EdgeDrop`。
    EdgeSummary,
}

impl HybridStep {
    /// 返回步骤名；与契约的 `hybrid_steps` 一一对应。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ReadOld => "read-old",
            Self::ShadeOldDeleted => "shade-old-deleted",
            Self::ShadeNewInserted => "shade-new-inserted",
            Self::Store => "store",
            Self::CardMark => "card-mark",
            Self::EdgeSummary => "edge-summary",
        }
    }
}

/// `CardMarkBuffer` 的 flush 原因；与契约的 `flush_reasons` 一一对应。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BarrierFlushReason {
    /// buffer 达到固定上界，必须进入 slow path。
    BufferFull,
    /// processor 交接（绑定、retire 或换栈）。
    ProcessorHandoff,
    /// 进入普通 bridge 或 dirty bridge。
    ForeignBridge,
    /// memory pressure 触发的有界 drain。
    MemoryPressure,
    /// minor cycle 请求 stop。
    MinorStop,
    /// producer 即将 park 或进入 stop gate。
    ProducerStopGate,
}

impl BarrierFlushReason {
    /// 返回原因名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::BufferFull => "buffer-full",
            Self::ProcessorHandoff => "processor-handoff",
            Self::ForeignBridge => "foreign-bridge",
            Self::MemoryPressure => "memory-pressure",
            Self::MinorStop => "minor-stop",
            Self::ProducerStopGate => "producer-stop-gate",
        }
    }

    /// 在 `ALL` 中的下标；统计数组按同一顺序索引。
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::BufferFull => 0,
            Self::ProcessorHandoff => 1,
            Self::ForeignBridge => 2,
            Self::MemoryPressure => 3,
            Self::MinorStop => 4,
            Self::ProducerStopGate => 5,
        }
    }

    /// 全部原因，按契约顺序。
    pub(crate) const ALL: [Self; 6] = [
        Self::BufferFull,
        Self::ProcessorHandoff,
        Self::ForeignBridge,
        Self::MemoryPressure,
        Self::MinorStop,
        Self::ProducerStopGate,
    ];
}

/// 一条 remembered-set card 的稳定键；不含任何地址。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct CardKey {
    /// arena descriptor 的稠密编号。
    pub(crate) arena_descriptor: u64,
    /// arena 的 generation。
    pub(crate) arena_generation: u32,
    /// arena 内的 card 序号。
    pub(crate) card_index: u32,
    /// 产生该键的 cycle epoch。
    pub(crate) cycle_epoch: u64,
}

impl CardKey {
    /// 由一个写入地址构造键。
    pub(crate) const fn new(
        arena_descriptor: u64,
        arena_generation: u32,
        byte_address: u64,
        cycle_epoch: u64,
    ) -> Self {
        Self {
            arena_descriptor,
            arena_generation,
            card_index: card_index(byte_address) as u32,
            cycle_epoch,
        }
    }
}

/// 一条待发布的 card batch 草稿：与 `CardMarkBatch` 的字段一一对应，尚未分配 node。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CardMarkDraft {
    pub(crate) arena_descriptor: u64,
    pub(crate) arena_generation: u32,
    pub(crate) card_start: u32,
    pub(crate) card_count: u32,
    pub(crate) cycle_epoch: u64,
    pub(crate) bytes: u32,
}

/// mutator 侧的一次写入位置。
///
/// 只携带稳定 `BlockRef` 与被写 field 的 arena 内偏移：old/new 的 presence 由 `Option`
/// 推导，跨 block 的增删由 `edge_changes` 派生，不再需要布尔“末次操作”表示。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BarrierSite {
    /// 被写入 field 所在 arena 的 descriptor 编号。
    pub(crate) arena_descriptor: u64,
    /// 该 arena 的 generation。
    pub(crate) arena_generation: u32,
    /// 写入地址的 **arena 内**字节偏移。
    pub(crate) offset: u64,
    /// 当前 cycle epoch。
    pub(crate) cycle_epoch: u64,
    /// 被写入位置的稳定 block 身份。
    pub(crate) source: BlockRef,
    /// 被覆盖的旧目标；null 或非 managed 值为 `None`。
    pub(crate) old: Option<BlockRef>,
    /// 新目标；null 为 `None`。
    pub(crate) new: Option<BlockRef>,
    /// 新值是否指向 nursery/aging；false 时第 5 步不产生 card 键。
    pub(crate) new_in_nursery: bool,
    /// 被写入对象是否位于 old/immortal generation。
    pub(crate) owner_old: bool,
    /// 并发标记是否开启；关闭时第 2、3 步由一个 runtime flag 分支跳过。
    pub(crate) marking: bool,
    /// 当前 coroutine stack 是否仍为 grey；没有 current coroutine 的 runtime/system 写入
    /// 一律按 grey 处理。
    pub(crate) stack_grey: bool,
}

impl BarrierSite {
    /// 把本次写入的净边变更写进定长数组，返回写入条数。
    ///
    /// 同一个 block 内部、以及新旧目标相同的写入都不产生边：前者是私有字段写入，后者净效果
    /// 为零。覆盖一个跨 block 目标必须同时撤销旧边并增加新边，两个方向各自成一条记录。
    fn edge_changes(self, out: &mut [Option<EdgeChange>; EDGE_DELTAS_PER_WRITE as usize]) -> usize {
        let source = self.source;
        let mut written = 0;
        if let Some(old) = self.old
            && old.id != source.id
            && self.new != Some(old)
        {
            out[written] = Some(EdgeChange {
                source,
                target: old,
                delta: -1,
            });
            written += 1;
        }
        if let Some(new) = self.new
            && new.id != source.id
        {
            out[written] = Some(EdgeChange {
                source,
                target: new,
                delta: 1,
            });
            written += 1;
        }
        written
    }
}

/// 一条待合并的边变更；由 mutator 直接追加到预留 scratch，不做排序或分配。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EdgeChange {
    pub(crate) source: BlockRef,
    pub(crate) target: BlockRef,
    pub(crate) delta: i64,
}

/// 一次 hybrid barrier 的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HybridBarrierOutcome {
    /// 实际执行到的步骤；顺序即 `HybridStep` 的顺序。
    pub(crate) steps: Vec<HybridStep>,
    /// 旧 target 是否被 shade。
    pub(crate) shaded_old: bool,
    /// 新 target 是否被 shade。
    pub(crate) shaded_new: bool,
    /// 是否产生了 card 键。
    pub(crate) card_marked: bool,
    /// 需要的 flush 原因；buffer 满时必须由调用方在 region 外冲刷。
    pub(crate) flush: Option<BarrierFlushReason>,
    /// 因 scratch 已满而未记入的边变更；调用方 flush 后必须原样重放，不能重做字段写入。
    pub(crate) pending_edges: [Option<EdgeChange>; EDGE_DELTAS_PER_WRITE as usize],
}

/// 一条已发布的 edge delta。
///
/// `delta` 是 block 对的 signed 差量：target 侧把它累加进已应用计数，因此 add/drop 逆序到达
/// 也不会瞬间产生「零 lease 可回收」。`sequence` 在每个 block 对内从 1 开始单调递增。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EdgeDeltaRecord {
    /// source block。
    pub(crate) source: BlockRef,
    /// target block。
    pub(crate) target: BlockRef,
    /// 该 block 对的发布序号。
    pub(crate) sequence: u64,
    /// 本次发布的净差量。
    pub(crate) delta: i64,
    /// 聚合到该记录的最后一个 epoch。
    pub(crate) epoch: u64,
}

/// 一个 block 对的聚合状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EdgeState {
    /// mutator 侧已生效的活跃边计数；0↔非零 的转换即 target 的 incoming lease 变化。
    active: u64,
    /// 已经发布出去的累计计数。
    published: u64,
    /// 该 block 对的发布序号；净零不消耗序号。
    sequence: u64,
    /// 最近一次聚合的 epoch。
    epoch: u64,
}

/// owner-local edge summary：按 block 对聚合增删，并给出可发布的 signed 差量。
///
/// 计数而不是布尔值：同一对 block 可以有多条字段边，删掉其中一条之后剩余边仍然活跃。
/// 计数下溢或无法表示的差量都是不变量失败，不能饱和成零。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct EdgeSummary {
    entries: BTreeMap<(BlockRef, BlockRef), EdgeState>,
}

impl EdgeSummary {
    /// 合并一条边变更；返回是否改变了 target 的 incoming lease（0↔非零）。
    pub(crate) fn apply(&mut self, change: EdgeChange) -> Result<bool, RawInvariant> {
        let key = (change.source, change.target);
        let state = self.entries.entry(key).or_insert(EdgeState {
            active: 0,
            published: 0,
            sequence: 0,
            epoch: 0,
        });
        let before = state.active;
        let active = if change.delta >= 0 {
            let delta = u64::try_from(change.delta).expect("非负 delta 适配 u64");
            state
                .active
                .checked_add(delta)
                .ok_or_else(|| RawInvariant::new("edge 活跃计数溢出"))?
        } else {
            let delta = change.delta.unsigned_abs();
            state
                .active
                .checked_sub(delta)
                .ok_or_else(|| RawInvariant::new("edge 活跃计数下溢"))?
        };
        state.active = active;
        Ok((before == 0) != (active == 0))
    }

    /// 发布全部尚未发布的净差量；净零不分配记录也不消耗序号。
    pub(crate) fn publish(&mut self, epoch: u64) -> Result<Vec<EdgeDeltaRecord>, RawInvariant> {
        let mut records = Vec::new();
        for ((source, target), state) in self.entries.iter_mut() {
            if state.active == state.published {
                continue;
            }
            let active = i64::try_from(state.active)
                .map_err(|_| RawInvariant::new("edge 活跃计数无法用 i64 表示"))?;
            let published = i64::try_from(state.published)
                .map_err(|_| RawInvariant::new("edge 已发布计数无法用 i64 表示"))?;
            let delta = active - published;
            state.published = state.active;
            state.sequence = state
                .sequence
                .checked_add(1)
                .ok_or_else(|| RawInvariant::new("edge sequence 溢出"))?;
            state.epoch = epoch;
            records.push(EdgeDeltaRecord {
                source: *source,
                target: *target,
                sequence: state.sequence,
                delta,
                epoch,
            });
        }
        records.sort_by_key(|record| (record.source, record.target));
        Ok(records)
    }

    /// 把一个目标 block 的全部聚合项重键到新的目标 block。
    ///
    /// `incoming_leases` 的定义是「非零 source 对的数量」，因此搬迁必须**重键**而不是复制：复制会
    /// 让旧块永远背着租约、再也成不了候选，同时让新块的租约数偏高。序号与 epoch 血统随项一起迁移，
    /// 新键不会从头开始接受已经发布过的序号。返回迁移的聚合项数量。
    pub(crate) fn relocate_target(&mut self, old: ManagedBlockId, new: BlockRef) -> usize {
        let keys: Vec<(BlockRef, BlockRef)> = self
            .entries
            .keys()
            .filter(|(_, target)| target.id == old)
            .copied()
            .collect();
        let mut moved = 0;
        for (source, old_target) in keys {
            let Some(state) = self.entries.remove(&(source, old_target)) else {
                continue;
            };
            let entry = self.entries.entry((source, new)).or_insert(EdgeState {
                active: 0,
                published: 0,
                sequence: 0,
                epoch: state.epoch,
            });
            entry.active = entry.active.saturating_add(state.active);
            entry.published = entry.published.saturating_add(state.published);
            entry.sequence = entry.sequence.max(state.sequence);
            entry.epoch = entry.epoch.max(state.epoch);
            moved += 1;
        }
        moved
    }

    /// 返回一个 block 的 incoming lease：非零 source block 对的数量。
    pub(crate) fn incoming_leases(&self, target: BlockRef) -> u64 {
        self.entries
            .iter()
            .filter(|((_, pair_target), state)| *pair_target == target && state.active != 0)
            .count() as u64
    }

    /// 待发布 delta 数量。
    pub(crate) fn pending(&self) -> usize {
        self.entries
            .values()
            .filter(|state| state.active != state.published)
            .count()
    }

    /// 仍然活跃的 block 对数量。
    pub(crate) fn active_pairs(&self) -> usize {
        self.entries
            .values()
            .filter(|state| state.active != 0)
            .count()
    }

    /// 删除已经零计数、无待发布差量且 grace 完成的 block 对。
    ///
    /// 保留 `sequence` 的语义要求在这里被显式放弃：重建同一个 block 对时序号从 1 重新开始，
    /// 旧记录由发布端的 generation 校验拒绝，因此不需要无限积累零计数键。
    pub(crate) fn retire_zero_pairs(&mut self) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, state| state.active != 0 || state.active != state.published);
        before - self.entries.len()
    }
}

/// 一个 dedup stamp 槽：指向某个 entry，并且只有在该 entry 仍等于该键时才算命中。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Stamp {
    arena_descriptor: u64,
    arena_generation: u32,
    card_index: u32,
    cycle_epoch: u64,
    entry: u32,
}

/// `LogicalProcessor` 的 owner-local remembered-set buffer。
///
/// 固定上界是规范选择的表示：card mark 只需要记录“脏过”这一位，重复键可以在本地合并，
/// 达到上界才进入 flush slow path，从而不让每次写入都变成共享 cache-line 写。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CardMarkBuffer {
    entries: Vec<CardKey>,
    stamps: Vec<Option<Stamp>>,
    cycle_epoch: u64,
    pending_bytes: u64,
    last_flush: Option<BarrierFlushReason>,
}

impl CardMarkBuffer {
    /// 创建空 buffer；`entries` 与 stamp 表常驻，不随写入分配。
    pub(crate) fn new(cycle_epoch: u64) -> Self {
        Self {
            entries: Vec::with_capacity(CARD_MARK_BUFFER_ENTRIES as usize),
            stamps: vec![None; CARD_MARK_STAMP_ENTRIES as usize],
            cycle_epoch,
            pending_bytes: 0,
            last_flush: None,
        }
    }

    /// 返回当前有效项数。
    pub(crate) fn len(&self) -> u32 {
        u32::try_from(self.entries.len()).expect("buffer 项数适配 u32")
    }

    /// 返回是否为空。
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 返回当前 cycle epoch。
    pub(crate) fn cycle_epoch(&self) -> u64 {
        self.cycle_epoch
    }

    /// 返回未发布的 pending bytes。
    pub(crate) fn pending_bytes(&self) -> u64 {
        self.pending_bytes
    }

    /// 返回最近一次 flush 原因。
    pub(crate) fn last_flush(&self) -> Option<BarrierFlushReason> {
        self.last_flush
    }

    /// 推进 cycle epoch。
    ///
    /// 旧键不能跨 cycle 复用，但也不能因为 epoch 前进而消失：仍有未发布键时返回 `Err`，
    /// 由调用方先 flush 并把 batch 交给 arena owner，再重新推进 epoch。
    pub(crate) fn advance_epoch(&mut self, cycle_epoch: u64) -> Result<(), BarrierFlushReason> {
        if cycle_epoch == self.cycle_epoch {
            return Ok(());
        }
        if !self.entries.is_empty() {
            // cycle 边界与 minor stop 是同一件事：epoch 前进前必须先 drain remembered set。
            return Err(BarrierFlushReason::MinorStop);
        }
        self.cycle_epoch = cycle_epoch;
        Ok(())
    }

    /// 追回一个 card 键。
    ///
    /// 返回 `Err` 表示 buffer 已满：调用方必须在 region 外先 flush，不能就地扩容，也不能
    /// 把 card table 直接写成共享 fast path。
    pub(crate) fn record(&mut self, key: CardKey) -> Result<(), BarrierFlushReason> {
        debug_assert!(key.cycle_epoch == self.cycle_epoch, "键必须属于当前 cycle");
        debug_assert!(self.entries.len() <= CARD_MARK_BUFFER_ENTRIES as usize);
        let slot = stamp_slot(key.arena_descriptor, key.arena_generation, key.card_index) as usize;
        if let Some(stamp) = self.stamps[slot]
            && stamp.arena_descriptor == key.arena_descriptor
            && stamp.arena_generation == key.arena_generation
            && stamp.card_index == key.card_index
            && stamp.cycle_epoch == key.cycle_epoch
            && self.entries.get(stamp.entry as usize) == Some(&key)
        {
            // 命中的键已经在 buffer 里，本次写入不需要新的 slot。
            return Ok(());
        }
        if self.entries.len() >= CARD_MARK_BUFFER_ENTRIES as usize {
            return Err(BarrierFlushReason::BufferFull);
        }
        let entry = u32::try_from(self.entries.len()).expect("buffer 项数适配 u32");
        self.entries.push(key);
        // conflict 只覆盖 stamp 槽；旧键仍留在 entries 中，不会因为 stamp 冲突而丢失。
        self.stamps[slot] = Some(Stamp {
            arena_descriptor: key.arena_descriptor,
            arena_generation: key.arena_generation,
            card_index: key.card_index,
            cycle_epoch: key.cycle_epoch,
            entry,
        });
        self.pending_bytes += u64::from(CARD_GRANULARITY_BYTES);
        Ok(())
    }

    /// 冲刷 buffer，按 `(arena, generation, cycle epoch)` 分组合并连续 card 区间。
    ///
    /// flush 以 Release 语义发布：返回的草稿只在调用方发布字段 store 之后才交给 arena owner。
    pub(crate) fn flush(&mut self, reason: BarrierFlushReason) -> Vec<CardMarkDraft> {
        self.last_flush = Some(reason);
        if self.entries.is_empty() {
            return Vec::new();
        }
        self.entries
            .sort_by_key(|key| (key.arena_descriptor, key.arena_generation, key.card_index));
        let mut drafts = Vec::new();
        let mut index = 0;
        while index < self.entries.len() {
            let key = self.entries[index];
            let mut end = index + 1;
            while end < self.entries.len()
                && self.entries[end].arena_descriptor == key.arena_descriptor
                && self.entries[end].arena_generation == key.arena_generation
                && self.entries[end].cycle_epoch == key.cycle_epoch
                && self.entries[end].card_index == self.entries[end - 1].card_index + 1
            {
                end += 1;
            }
            drafts.push(CardMarkDraft {
                arena_descriptor: key.arena_descriptor,
                arena_generation: key.arena_generation,
                card_start: key.card_index,
                card_count: u32::try_from(end - index).expect("card 区间适配 u32"),
                cycle_epoch: key.cycle_epoch,
                bytes: u32::try_from(end - index).expect("card 字节数适配 u32"),
            });
            index = end;
        }
        self.entries.clear();
        self.stamps.iter_mut().for_each(|stamp| *stamp = None);
        self.pending_bytes = 0;
        drafts
    }
}

/// arena 的 card table；固定 512-byte 粒度，每 card 一个 dirty byte。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CardTable {
    manager: OwnerToken,
    arena_generation: u32,
    cards: Vec<u8>,
    dirty: u32,
    minor_pending: bool,
    swapped: u64,
}

impl CardTable {
    /// 按 arena 字节数创建 card table。
    pub(crate) fn new(manager: OwnerToken, arena_generation: u32, arena_bytes: u64) -> Self {
        let cards = usize::try_from(arena_bytes / u64::from(CARD_GRANULARITY_BYTES))
            .expect("card 数量适配宿主");
        Self {
            manager,
            arena_generation,
            cards: vec![0; cards],
            dirty: 0,
            minor_pending: false,
            swapped: 0,
        }
    }

    /// 返回管理该 arena 的 owner。
    pub(crate) const fn manager(&self) -> OwnerToken {
        self.manager
    }

    /// 改写管理该 arena 的 owner；管理权转移后 CardMark batch 必须投给新 owner。
    pub(crate) const fn set_manager(&mut self, manager: OwnerToken) {
        self.manager = manager;
    }

    /// 返回 arena generation。
    pub(crate) const fn arena_generation(&self) -> u32 {
        self.arena_generation
    }

    /// 返回 card 总数。
    pub(crate) fn card_count(&self) -> u32 {
        u32::try_from(self.cards.len()).expect("card 数量适配 u32")
    }

    /// 返回当前 dirty card 数。
    pub(crate) const fn dirty(&self) -> u32 {
        self.dirty
    }

    /// 返回是否已有 minor stop 等待 drain。
    pub(crate) const fn minor_pending(&self) -> bool {
        self.minor_pending
    }

    /// 返回累计 swap 次数。
    pub(crate) const fn swapped(&self) -> u64 {
        self.swapped
    }

    /// 消费一条 batch：owner 上下文校验 generation 后写 card byte，重复置位幂等。
    pub(crate) fn consume(&mut self, draft: &CardMarkDraft) -> Result<u32, RawInvariant> {
        if draft.arena_generation != self.arena_generation {
            return Err(RawInvariant::new(format!(
                "card batch arena generation {} 与 card table {} 不匹配",
                draft.arena_generation, self.arena_generation
            )));
        }
        let start = usize::try_from(draft.card_start)
            .map_err(|_| RawInvariant::new("card 起点超出宿主宽度"))?;
        let end = start
            .checked_add(usize::try_from(draft.card_count).expect("card 数量适配宿主"))
            .ok_or_else(|| RawInvariant::new("card 区间溢出"))?;
        if end > self.cards.len() {
            return Err(RawInvariant::new("card batch 引用越界的 card 区间"));
        }
        let mut marked = 0;
        for byte in &mut self.cards[start..end] {
            if *byte == 0 {
                *byte = 1;
                self.dirty += 1;
            }
            marked += 1;
        }
        Ok(marked)
    }

    /// 请求一次 minor stop；在 remembered set 被扫描前必须先把 dirty card 取走。
    pub(crate) fn request_minor_stop(&mut self) {
        self.minor_pending = true;
    }

    /// 取走 dirty card 集合并清零；返回的 card 仍需 owner 侧扫描。
    pub(crate) fn swap_dirty(&mut self) -> Vec<u32> {
        let mut cards = Vec::with_capacity(self.dirty as usize);
        for (index, byte) in self.cards.iter_mut().enumerate() {
            if *byte != 0 {
                *byte = 0;
                cards.push(u32::try_from(index).expect("card 序号适配 u32"));
            }
        }
        self.dirty = 0;
        self.minor_pending = false;
        self.swapped += 1;
        cards
    }
}

/// one processor 的 barrier 账本：card buffer、边 scratch 加它自己的 flush 统计。
///
/// edge scratch 是常驻预留（`EDGE_BUFFER_ENTRIES` 项），mutator 只做 `push`：不排序、不分配、
/// 不在这里合并进有序稀疏表。合并发生在 source-owner 的 slow edge（`flush`）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessorBarrier {
    buffer: CardMarkBuffer,
    edges: Vec<EdgeChange>,
    card_marks: u64,
    card_slot_reuses: u64,
    edge_changes: u64,
    flushes: u64,
    batches: u64,
    last_flush: Option<BarrierFlushReason>,
}

impl ProcessorBarrier {
    /// 创建空账本。
    pub(crate) fn new(cycle_epoch: u64) -> Self {
        Self {
            buffer: CardMarkBuffer::new(cycle_epoch),
            edges: Vec::with_capacity(EDGE_BUFFER_ENTRIES as usize),
            card_marks: 0,
            card_slot_reuses: 0,
            edge_changes: 0,
            flushes: 0,
            batches: 0,
            last_flush: None,
        }
    }

    /// 返回 buffer 的只读视图。
    pub(crate) const fn buffer(&self) -> &CardMarkBuffer {
        &self.buffer
    }

    /// 返回累计记账次数。
    pub(crate) const fn card_marks(&self) -> u64 {
        self.card_marks
    }

    /// 返回 dedup 命中（无需新 slot）的次数。
    pub(crate) const fn card_slot_reuses(&self) -> u64 {
        self.card_slot_reuses
    }

    /// 返回累计记录的边变更数。
    pub(crate) const fn edge_changes(&self) -> u64 {
        self.edge_changes
    }

    /// 返回边 scratch 中尚未合并的项数。
    pub(crate) fn edge_pending(&self) -> u32 {
        u32::try_from(self.edges.len()).expect("边 scratch 项数适配 u32")
    }

    /// 返回边 scratch 剩余的预留项数。
    pub(crate) fn edge_slots_left(&self) -> u32 {
        EDGE_BUFFER_ENTRIES.saturating_sub(self.edge_pending())
    }

    /// 返回累计 flush 次数。
    pub(crate) const fn flushes(&self) -> u64 {
        self.flushes
    }

    /// 返回累计发布 batch 数。
    pub(crate) const fn batches(&self) -> u64 {
        self.batches
    }

    /// 返回最近一次 flush 原因。
    pub(crate) const fn last_flush(&self) -> Option<BarrierFlushReason> {
        self.last_flush
    }

    /// 推进 cycle epoch；buffer 仍有未发布键时返回需要 flush 的原因。
    pub(crate) fn advance_epoch(&mut self, cycle_epoch: u64) -> Result<(), BarrierFlushReason> {
        self.buffer.advance_epoch(cycle_epoch)
    }

    /// 重放因 scratch 已满而未能记入的边变更；只追加，不触碰 card 账本。
    pub(crate) fn replay_edges(&mut self, pending: &[Option<EdgeChange>]) {
        for change in pending.iter().flatten() {
            self.edges.push(*change);
            self.edge_changes += 1;
        }
    }

    /// 取走 scratch 中的全部边变更；source-owner 在 slow edge 把它们合并进有序稀疏表。
    pub(crate) fn take_edges(&mut self) -> Vec<EdgeChange> {
        std::mem::take(&mut self.edges)
    }

    /// 执行一条 hybrid barrier，并把 card 键与边变更交给本地 scratch。
    ///
    /// 第 4 步是实际 store，第 5、6 步才是账本发布：即使 buffer 或边 scratch 已满导致记账
    /// 失败，store 也已完成，调用方必须按 permit 在 region 外补容量后重放记账，不能回滚 store，
    /// 也不能重复记录已经生效的 card 键。
    pub(crate) fn perform(&mut self, site: BarrierSite) -> HybridBarrierOutcome {
        let mut pending_edges = [None; EDGE_DELTAS_PER_WRITE as usize];
        // 站点 epoch 与账本不一致：本次写入不记账，也绝不就地改写 epoch，而是把强制
        // flush 交给调用方。旧键只能经 flush→发布离开账本，不会被静默丢弃。
        if site.cycle_epoch != self.buffer.cycle_epoch() {
            return HybridBarrierOutcome {
                steps: vec![HybridStep::ReadOld, HybridStep::Store],
                shaded_old: false,
                shaded_new: false,
                card_marked: false,
                flush: Some(BarrierFlushReason::MinorStop),
                pending_edges,
            };
        }
        let mut steps = Vec::with_capacity(6);
        steps.push(HybridStep::ReadOld);
        let shaded_old = site.marking && site.old.is_some();
        if shaded_old {
            steps.push(HybridStep::ShadeOldDeleted);
        }
        let grey = site.stack_grey;
        let shaded_new = site.marking && grey && site.new.is_some();
        if shaded_new {
            steps.push(HybridStep::ShadeNewInserted);
        }
        steps.push(HybridStep::Store);
        let mut flush = None;
        let mut card_marked = false;
        if site.owner_old && site.new_in_nursery {
            steps.push(HybridStep::CardMark);
            let key = CardKey::new(
                site.arena_descriptor,
                site.arena_generation,
                site.offset,
                site.cycle_epoch,
            );
            let before = self.buffer.len();
            match self.buffer.record(key) {
                Ok(()) => {
                    card_marked = true;
                    self.card_marks += 1;
                    if self.buffer.len() == before {
                        self.card_slot_reuses += 1;
                    }
                }
                // buffer 满：账本这一步失败，调用方必须在 region 外 flush 后重做记账。
                Err(reason) => flush = Some(reason),
            }
        }
        // 跨 block 的写入进入 owner-local edge summary：source 与 target 属于同一 block 时是
        // 私有字段写入，不产生 block edge。这里只追加到预留 scratch；合并与发布都在 slow edge。
        let mut changes = [None; EDGE_DELTAS_PER_WRITE as usize];
        let written = site.edge_changes(&mut changes);
        if written != 0 {
            steps.push(HybridStep::EdgeSummary);
            if self.edge_slots_left() >= EDGE_DELTAS_PER_WRITE {
                for change in changes.iter().flatten() {
                    self.edges.push(*change);
                }
                self.edge_changes += u64::try_from(written).expect("边变更数适配 u64");
            } else {
                // 额度不足：调用方在 region 外 flush 之后原样重放这两条变更。
                flush = Some(BarrierFlushReason::BufferFull);
                pending_edges = changes;
            }
        }
        HybridBarrierOutcome {
            steps,
            shaded_old,
            shaded_new,
            card_marked,
            flush,
            pending_edges,
        }
    }

    /// 冲刷本地账本；返回待发布的 batch 草稿。
    pub(crate) fn flush(&mut self, reason: BarrierFlushReason) -> Vec<CardMarkDraft> {
        let drafts = self.buffer.flush(reason);
        if !drafts.is_empty() || self.buffer.pending_bytes() == 0 {
            self.flushes += 1;
            self.last_flush = Some(reason);
            self.batches += u64::try_from(drafts.len()).expect("batch 数适配 u64");
        }
        drafts
    }
}

/// 全部 processor 的 barrier 账本与全部 arena 的 card table。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BarrierPlane {
    processors: Vec<ProcessorBarrier>,
    tables: BTreeMap<u64, CardTable>,
    edges: EdgeSummary,
    cycle_epoch: u64,
    published_batches: u64,
    consumed_batches: u64,
    pending_batch_bytes: u64,
    flushes: u64,
    empty_flushes: u64,
    flushed_by_reason: [u64; 6],
}

impl Default for BarrierPlane {
    fn default() -> Self {
        Self::new(0)
    }
}

impl BarrierPlane {
    /// 创建空平面。
    pub(crate) fn new(cycle_epoch: u64) -> Self {
        Self {
            processors: Vec::new(),
            tables: BTreeMap::new(),
            edges: EdgeSummary::default(),
            cycle_epoch,
            published_batches: 0,
            consumed_batches: 0,
            pending_batch_bytes: 0,
            flushes: 0,
            empty_flushes: 0,
            flushed_by_reason: [0; 6],
        }
    }

    /// 返回当前 cycle epoch。
    pub(crate) const fn cycle_epoch(&self) -> u64 {
        self.cycle_epoch
    }

    /// 返回 processor 数量。
    pub(crate) fn processor_count(&self) -> usize {
        self.processors.len()
    }

    /// 返回已发布的 batch 数。
    pub(crate) const fn published_batches(&self) -> u64 {
        self.published_batches
    }

    /// 返回已消费的 batch 数。
    pub(crate) const fn consumed_batches(&self) -> u64 {
        self.consumed_batches
    }

    /// 返回尚未被 arena owner 消费的 batch 字节。
    pub(crate) const fn pending_batch_bytes(&self) -> u64 {
        self.pending_batch_bytes
    }

    /// 管理权转移：把 manager 等于 `from` 的全部 card table 改成 `to`；返回改动的表数。
    ///
    /// batch 路由读的就是 `CardTable::manager`（`flush_barrier` 按它决定本地写还是跨 owner
    /// 发布，`service_card_mark` 按它校验投递目标），因此转移必须按 manager 扫描而不是按 arena
    /// 种类分别处理：漏掉任一类都会让 CardMark 继续投给已经退役的 owner。
    pub(crate) fn handover_manager(&mut self, from: OwnerToken, to: OwnerToken) -> u32 {
        let mut changed = 0_u32;
        for table in self.tables.values_mut() {
            if table.manager() == from {
                table.set_manager(to);
                changed = changed.checked_add(1).expect("card table 数适配 u32");
            }
        }
        changed
    }

    /// 返回 arena 数量。
    pub(crate) fn arena_count(&self) -> usize {
        self.tables.len()
    }

    /// 返回 edge summary 视图。
    pub(crate) const fn edges(&self) -> &EdgeSummary {
        &self.edges
    }

    /// 登记一个 arena 的 card table；重复登记同一 descriptor 直接返回既有表。
    pub(crate) fn register_arena(
        &mut self,
        arena_descriptor: u64,
        manager: OwnerToken,
        arena_generation: u32,
        arena_bytes: u64,
    ) -> Result<&CardTable, RawInvariant> {
        if let std::collections::btree_map::Entry::Vacant(entry) =
            self.tables.entry(arena_descriptor)
        {
            entry.insert(CardTable::new(manager, arena_generation, arena_bytes));
        }
        self.tables
            .get(&arena_descriptor)
            .ok_or_else(|| RawInvariant::new("arena card table 登记后缺失"))
    }

    /// 推进 cycle epoch：先冲刷全部 processor 并返回草稿，再切换 epoch。
    ///
    /// 返回的草稿必须由调用方发布给对应 arena owner；epoch 前进不允许让任何已记账的 card
    /// 键消失，因此这里先 flush 再切换，且切换后 buffer 必须为空。
    pub(crate) fn advance_epoch(
        &mut self,
        cycle_epoch: u64,
        reason: BarrierFlushReason,
    ) -> Result<Vec<CardMarkDraft>, RawInvariant> {
        // epoch 只前进：回退会把两批携带不同 epoch 的卡键混进同一账本，因此这里是运行时
        // 拒绝而不是断言——平面是 crate 内共享状态，调用方不能依赖 debug 构建。
        if cycle_epoch < self.cycle_epoch {
            return Err(RawInvariant::new(format!(
                "barrier 平面的 cycle epoch 不能从 {} 回退到 {cycle_epoch}",
                self.cycle_epoch
            )));
        }
        if cycle_epoch == self.cycle_epoch {
            return Ok(Vec::new());
        }
        let mut drafts = Vec::new();
        let count = self.processors.len();
        for processor in 0..count {
            drafts.append(&mut self.flush_processor(processor, reason)?);
        }
        for processor in &mut self.processors {
            processor
                .advance_epoch(cycle_epoch)
                .expect("flush 之后 buffer 必须为空");
        }
        self.cycle_epoch = cycle_epoch;
        Ok(drafts)
    }

    /// 确保 processor 账本存在并返回可变引用。
    pub(crate) fn processor_mut(&mut self, processor: usize) -> &mut ProcessorBarrier {
        while self.processors.len() <= processor {
            self.processors
                .push(ProcessorBarrier::new(self.cycle_epoch));
        }
        &mut self.processors[processor]
    }

    /// 返回 processor 账本。
    pub(crate) fn processor(&self, processor: usize) -> Option<&ProcessorBarrier> {
        self.processors.get(processor)
    }

    /// owner-local 写入：processor 是 arena owner 时直接合并写 card table。
    pub(crate) fn consume_locally(
        &mut self,
        arena_descriptor: u64,
        draft: &CardMarkDraft,
    ) -> Result<u32, RawInvariant> {
        let table = self
            .tables
            .get_mut(&arena_descriptor)
            .ok_or_else(|| RawInvariant::new("card batch 引用未登记的 arena"))?;
        table.consume(draft)
    }

    /// 记录一条从跨 owner 通道发布出去的 batch。
    pub(crate) fn record_publish(&mut self, drafts: &[CardMarkDraft]) {
        self.published_batches += u64::try_from(drafts.len()).expect("batch 数适配 u64");
        for draft in drafts {
            self.pending_batch_bytes += u64::from(draft.bytes);
        }
    }

    /// owner 侧消费一条已经过校验的 batch。
    pub(crate) fn consume_published(
        &mut self,
        arena_descriptor: u64,
        draft: &CardMarkDraft,
    ) -> Result<u32, RawInvariant> {
        let marked = self.consume_locally(arena_descriptor, draft)?;
        self.consumed_batches += 1;
        self.pending_batch_bytes = self
            .pending_batch_bytes
            .saturating_sub(u64::from(draft.bytes));
        Ok(marked)
    }

    /// 冲刷一个 processor 的账本，把边 scratch 合并进有序稀疏表，并按原因累计统计。
    ///
    /// 合并是 source-owner 的 slow edge：计数下溢等不变量失败在这里暴露，而不是被静默丢弃。
    pub(crate) fn flush_processor(
        &mut self,
        processor: usize,
        reason: BarrierFlushReason,
    ) -> Result<Vec<CardMarkDraft>, RawInvariant> {
        let (drafts, changes) = {
            let record = self.processor_mut(processor);
            let drafts = record.flush(reason);
            (drafts, record.take_edges())
        };
        for change in changes {
            self.edges.apply(change)?;
        }
        self.flushes += 1;
        self.flushed_by_reason[reason.index()] += 1;
        if drafts.is_empty() {
            self.empty_flushes += 1;
        }
        Ok(drafts)
    }

    /// 把全部 processor 的边 scratch 合并进 source-owner 的有序稀疏表。
    ///
    /// cycle 边界发布差量前必须调用它：仍然停留在 scratch 里的变更尚未进入聚合表，直接发布
    /// 会让它们跨到下个 cycle，甚至在下个 cycle 的 epoch 上被当成新边。
    pub(crate) fn merge_edges(&mut self) -> Result<(), RawInvariant> {
        let count = self.processors.len();
        for processor in 0..count {
            let changes = self.processor_mut(processor).take_edges();
            for change in changes {
                self.edges.apply(change)?;
            }
        }
        Ok(())
    }

    /// 返回仍未合并或未发布的边变更总数：scratch 与聚合表一起统计。
    pub(crate) fn edge_pending_items(&self) -> usize {
        let scratch: usize = self
            .processors
            .iter()
            .map(|record| record.edge_pending() as usize)
            .sum();
        scratch + self.edges.pending()
    }

    /// 重放一个 processor 上因额度不足而未记入的边变更。
    pub(crate) fn replay_pending_edges(
        &mut self,
        processor: usize,
        pending: &[Option<EdgeChange>],
    ) {
        self.processor_mut(processor).replay_edges(pending);
    }

    /// 发布全部尚未发布的净差量；net-zero 的 block 对不产生记录也不消耗序号。
    pub(crate) fn publish_edges(
        &mut self,
        epoch: u64,
    ) -> Result<Vec<EdgeDeltaRecord>, RawInvariant> {
        self.edges.publish(epoch)
    }

    /// 把一个目标 block 的聚合项与租约重键到新的目标 block；返回迁移项数。
    pub(crate) fn relocate_target(&mut self, old: ManagedBlockId, new: BlockRef) -> usize {
        self.edges.relocate_target(old, new)
    }

    /// 返回一个 block 的 incoming lease：非零 source block 对的数量。
    pub(crate) fn incoming_leases(&self, target: BlockRef) -> u64 {
        self.edges.incoming_leases(target)
    }

    /// 返回仍活跃的 block 对数量。
    pub(crate) fn active_pairs(&self) -> usize {
        self.edges.active_pairs()
    }

    /// 删除已零计数、无待发布差量的 block 对。
    pub(crate) fn retire_zero_pairs(&mut self) -> usize {
        self.edges.retire_zero_pairs()
    }

    /// 返回累计 flush 次数。
    pub(crate) const fn flushes(&self) -> u64 {
        self.flushes
    }

    /// 返回未产生任何 batch 的 flush 次数。
    pub(crate) const fn empty_flushes(&self) -> u64 {
        self.empty_flushes
    }

    /// 返回六个原因各自的 flush 次数，顺序与 `BarrierFlushReason::ALL` 一致。
    pub(crate) const fn flushed_by_reason(&self) -> [u64; 6] {
        self.flushed_by_reason
    }

    /// minor stop 门禁：只有所有 buffer 已 flush 且所有 batch 已消费时才允许扫描。
    pub(crate) fn minor_scan_ready(&self) -> bool {
        self.processors
            .iter()
            .all(|processor| processor.buffer().is_empty())
            && self.pending_batch_bytes == 0
    }

    /// 请求一次 minor stop。
    pub(crate) fn request_minor_stop(&mut self) {
        for table in self.tables.values_mut() {
            table.request_minor_stop();
        }
    }

    /// 返回某个 arena 的 card table。
    pub(crate) fn table(&self, arena_descriptor: u64) -> Option<&CardTable> {
        self.tables.get(&arena_descriptor)
    }

    /// 返回某个 arena 的可变 card table。
    pub(crate) fn table_mut(&mut self, arena_descriptor: u64) -> Option<&mut CardTable> {
        self.tables.get_mut(&arena_descriptor)
    }

    /// mutator 上下文：在一个 processor 上执行一条 hybrid barrier。
    ///
    /// edge scratch 与 card buffer 同属 processor 账本，合并由 `flush_processor` 在 slow edge
    /// 完成。站点 epoch 必须已经由 `advance_epoch` 推进过：这里不代调用方推进，避免在无处
    /// 发布旧键的层级上丢弃 remembered-set 内容。
    pub(crate) fn perform_barrier(
        &mut self,
        processor: usize,
        site: BarrierSite,
    ) -> Result<HybridBarrierOutcome, RawInvariant> {
        if site.cycle_epoch != self.cycle_epoch {
            return Err(RawInvariant::new(format!(
                "站点 cycle epoch {} 与平面 epoch {} 不一致，调用方必须先 flush 并推进 epoch",
                site.cycle_epoch, self.cycle_epoch
            )));
        }
        Ok(self.processor_mut(processor).perform(site))
    }
}
