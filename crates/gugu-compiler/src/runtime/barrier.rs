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
    CARD_GRANULARITY_BYTES, CARD_MARK_BUFFER_ENTRIES, CARD_MARK_STAMP_ENTRIES,
    CARD_MARKS_PER_WRITE, MessageFamilyTag, SHADE_SLOTS_PER_WRITE, card_index, stamp_slot,
};
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BarrierSite {
    /// 被写入 field 所在 arena 的 descriptor 编号。
    pub(crate) arena_descriptor: u64,
    /// 该 arena 的 generation。
    pub(crate) arena_generation: u32,
    /// 写入地址的 arena 内字节偏移。
    pub(crate) offset: u64,
    /// 当前 cycle epoch。
    pub(crate) cycle_epoch: u64,
    /// 旧值是否非空。
    pub(crate) old_present: bool,
    /// 新值是否非空。
    pub(crate) new_present: bool,
    /// 新值是否指向 nursery/aging；false 时第 5 步不产生 card 键。
    pub(crate) new_in_nursery: bool,
    /// 被写入对象是否位于 old/immortal generation。
    pub(crate) owner_old: bool,
    /// 并发标记是否开启；关闭时第 2、3 步由一个 runtime flag 分支跳过。
    pub(crate) marking: bool,
    /// 当前 coroutine stack 是否仍为 grey；没有 current coroutine 的 runtime/system 写入
    /// 一律按 grey 处理。
    pub(crate) stack_grey: bool,
    /// 新值目标的 block 身份；用于跨 block edge summary。
    pub(crate) new_block: Option<u32>,
    /// 被写入位置的 block 身份。
    pub(crate) source_block: u32,
    /// 新值所属 owner 槽位。
    pub(crate) new_owner: u32,
    /// 被写入位置所属 owner 槽位。
    pub(crate) source_owner: u32,
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
    /// edge summary 的聚合结果。
    pub(crate) edge: Option<EdgeDeltaRecord>,
}

/// 一条 edge summary 记录。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EdgeDeltaRecord {
    /// source block。
    pub(crate) source_block: u32,
    /// target block。
    pub(crate) target_block: u32,
    /// target block 的 generation。
    pub(crate) generation: u32,
    /// 净增或净删；`Add` 表示新增边。
    pub(crate) add: bool,
    /// 聚合到该记录的最后一个 epoch。
    pub(crate) epoch: u64,
}

/// 跨 block edge 的聚合状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EdgeState {
    /// 已发布但尚未被同一或更晚 epoch 纳入的 add。
    published_add_epoch: Option<u64>,
    /// 尚未发布的净效果。
    pending: Option<EdgeDeltaRecord>,
}

/// owner-local edge summary：按 block 对聚合 `EdgeAdd`/`EdgeDrop`。
///
/// 删除不能早于同一条已被发布的 add 纳入同一或更晚 epoch：若 add 已发布而 drop 的 epoch
/// 更早，drop 会被提升到 add 的 epoch 再发布，因此顺序不会颠倒。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct EdgeSummary {
    entries: BTreeMap<(u32, u32, u32), EdgeState>,
}

impl EdgeSummary {
    /// 记录一条新增边。
    pub(crate) fn record_add(
        &mut self,
        source_block: u32,
        target_block: u32,
        generation: u32,
        epoch: u64,
    ) -> EdgeDeltaRecord {
        self.record(source_block, target_block, generation, epoch, true)
    }

    /// 记录一条删除边。
    pub(crate) fn record_drop(
        &mut self,
        source_block: u32,
        target_block: u32,
        generation: u32,
        epoch: u64,
    ) -> EdgeDeltaRecord {
        self.record(source_block, target_block, generation, epoch, false)
    }

    fn record(
        &mut self,
        source_block: u32,
        target_block: u32,
        generation: u32,
        epoch: u64,
        add: bool,
    ) -> EdgeDeltaRecord {
        let key = (source_block, target_block, generation);
        let state = self.entries.entry(key).or_insert(EdgeState {
            published_add_epoch: None,
            pending: None,
        });
        // 同一 edge 的删除不能早于其已经发布的 add 被纳入同一或更晚的 epoch。
        let epoch = if add {
            epoch
        } else {
            epoch.max(state.published_add_epoch.unwrap_or(0))
        };
        if add {
            state.published_add_epoch = Some(epoch);
        }
        let record = match state.pending {
            // 同一 epoch 内 add 与 drop 净零抵消，不发布空 delta。
            Some(pending) if pending.epoch == epoch && pending.add != add => {
                state.pending = None;
                EdgeDeltaRecord {
                    source_block,
                    target_block,
                    generation,
                    add,
                    epoch,
                }
            }
            Some(mut pending) => {
                pending.epoch = pending.epoch.max(epoch);
                pending.add = add;
                state.pending = Some(pending);
                pending
            }
            None => {
                let record = EdgeDeltaRecord {
                    source_block,
                    target_block,
                    generation,
                    add,
                    epoch,
                };
                state.pending = Some(record);
                record
            }
        };
        record
    }

    /// 取出全部待发布 delta，按 `(source, target, generation)` 稳定排序。
    pub(crate) fn drain(&mut self) -> Vec<EdgeDeltaRecord> {
        let mut records = Vec::with_capacity(self.entries.len());
        for ((_, _, _), state) in self.entries.iter_mut() {
            if let Some(record) = state.pending.take() {
                records.push(record);
            }
        }
        records.sort_by_key(|record| (record.source_block, record.target_block, record.generation));
        records
    }

    /// 待发布 delta 数量。
    pub(crate) fn pending(&self) -> usize {
        self.entries
            .values()
            .filter(|state| state.pending.is_some())
            .count()
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

    /// 推进 cycle epoch：epoch 变化时旧键必须整体作废，不能跨 cycle 复用。
    pub(crate) fn advance_epoch(&mut self, cycle_epoch: u64) {
        if cycle_epoch != self.cycle_epoch {
            self.entries.clear();
            self.stamps.iter_mut().for_each(|stamp| *stamp = None);
            self.pending_bytes = 0;
            self.cycle_epoch = cycle_epoch;
        }
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

/// one processor 的 barrier 账本：buffer 加它自己的 flush 统计。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessorBarrier {
    buffer: CardMarkBuffer,
    card_marks: u64,
    card_slot_reuses: u64,
    flushes: u64,
    batches: u64,
    last_flush: Option<BarrierFlushReason>,
}

impl ProcessorBarrier {
    /// 创建空账本。
    pub(crate) fn new(cycle_epoch: u64) -> Self {
        Self {
            buffer: CardMarkBuffer::new(cycle_epoch),
            card_marks: 0,
            card_slot_reuses: 0,
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

    /// 推进 cycle epoch。
    pub(crate) fn advance_epoch(&mut self, cycle_epoch: u64) {
        self.buffer.advance_epoch(cycle_epoch);
    }

    /// 执行一条 hybrid barrier，并把 card 键交给本地账本。
    ///
    /// 第 4 步是实际 store，第 5 步才是账本发布：即使 buffer 已满导致记账失败，store 也已
    /// 完成，调用方必须按 permit 在 region 外补容量后重试账本，不能回滚 store。
    pub(crate) fn perform(
        &mut self,
        site: BarrierSite,
        edges: &mut EdgeSummary,
    ) -> HybridBarrierOutcome {
        // mutator 观测到新 cycle 时，旧键必须整体作废才能记账：epoch 变化在这里收敛，
        // 不要求调用方在每次写入前单独同步。
        self.buffer.advance_epoch(site.cycle_epoch);
        let mut steps = Vec::with_capacity(6);
        steps.push(HybridStep::ReadOld);
        let shaded_old = site.marking && site.old_present;
        if shaded_old {
            steps.push(HybridStep::ShadeOldDeleted);
        }
        let grey = site.stack_grey;
        let shaded_new = site.marking && grey && site.new_present;
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
        let edge = if site.source_owner != site.new_owner || site.new_block.is_some() {
            match site.new_block {
                Some(target_block) => {
                    steps.push(HybridStep::EdgeSummary);
                    let record = if site.new_present {
                        edges.record_add(
                            site.source_block,
                            target_block,
                            site.arena_generation,
                            site.cycle_epoch,
                        )
                    } else {
                        edges.record_drop(
                            site.source_block,
                            target_block,
                            site.arena_generation,
                            site.cycle_epoch,
                        )
                    };
                    Some(record)
                }
                None => None,
            }
        } else {
            None
        };
        HybridBarrierOutcome {
            steps,
            shaded_old,
            shaded_new,
            card_marked,
            flush,
            edge,
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

    /// 单次写入消费的 slot 上界；供契约与测试核对 permit 口径。
    pub(crate) const fn slot_cost() -> (u32, u32) {
        (SHADE_SLOTS_PER_WRITE, CARD_MARKS_PER_WRITE)
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

    /// 推进 cycle epoch：全部 processor 的旧键作废。
    pub(crate) fn advance_epoch(&mut self, cycle_epoch: u64) {
        self.cycle_epoch = cycle_epoch;
        for processor in &mut self.processors {
            processor.advance_epoch(cycle_epoch);
        }
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

    /// 冲刷一个 processor 的账本，并按原因累计统计。
    pub(crate) fn flush_processor(
        &mut self,
        processor: usize,
        reason: BarrierFlushReason,
    ) -> Vec<CardMarkDraft> {
        let drafts = self.processor_mut(processor).flush(reason);
        self.flushes += 1;
        self.flushed_by_reason[reason.index()] += 1;
        if drafts.is_empty() {
            self.empty_flushes += 1;
        }
        drafts
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

    /// 冲刷全部 processor 的账本；返回每个 processor 的草稿。
    pub(crate) fn flush_all(&mut self, reason: BarrierFlushReason) -> Vec<Vec<CardMarkDraft>> {
        (0..self.processors.len())
            .map(|processor| self.processor_mut(processor).flush(reason))
            .collect()
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
    /// edge summary 与 buffer 同属本平面，调用方不需要自己持有聚合状态。
    pub(crate) fn perform_barrier(
        &mut self,
        processor: usize,
        site: BarrierSite,
    ) -> HybridBarrierOutcome {
        if site.cycle_epoch != self.cycle_epoch {
            self.advance_epoch(site.cycle_epoch);
        }
        let mut edges = std::mem::take(&mut self.edges);
        let outcome = self.processor_mut(processor).perform(site, &mut edges);
        self.edges = edges;
        outcome
    }

    /// 取出全部待发布的 edge delta。
    pub(crate) fn drain_edges(&mut self) -> Vec<EdgeDeltaRecord> {
        self.edges.drain()
    }

    /// 返回消息族判别值；card batch 只允许出现在 GC 工作族。
    pub(crate) const fn message_family() -> MessageFamilyTag {
        MessageFamilyTag::CardMark
    }
}

/// 一个 processor 在给定 cycle 内允许写入的最大 distinct card 数。
pub(crate) const fn buffer_capacity() -> u32 {
    CARD_MARK_BUFFER_ENTRIES
}

/// arena 到 card 序号的换算；供 runtime 与测试共用同一常量。
pub(crate) const fn arena_card_index(base: u64, address: u64) -> Option<u64> {
    super::barrier_schema::arena_card(base, address)
}
