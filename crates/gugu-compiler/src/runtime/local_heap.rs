//! LocalHeap Immix arena、TLAB、位图与分代 cycle 的确定性参照实现。
//!
//! 每个 owner 拥有自己的 arena 集合：nursery、old、resource、pinned 与 large。block 的物理页由
//! extent 层按 32 KiB 提交，这里只维护 block 内的 line 状态、side metadata 与对象字节，因此
//! `runtime_committed_bytes` 仍只统计 slab/extent 提交的页。
//!
//! 表示选择：arena 按稠密下标直接索引；block 槽按 arena 内固定下标，payload 只为已提交的 block
//! 分配；object-start 与 mark 位图每 granule 一位，用 `u64` word 做机器字位运算；line 表按契约是
//! 一 line 一字节；mark 用 arena epoch 加 block epoch 的 test-and-mark，避免每个 cycle 清零整张
//! 位图。pin side table 是冷路径小表，按对象身份线性查找，未 pin 对象不占槽位。
//!
//! 淘汰与回收边界：minor cycle 把 nursery 存活对象搬运到 old 并原地更新引用；major cycle 从根与
//! remembered set 标记后做 owner-local line 回收（空 block 留在 arena 内复用，交还 provider 属于
//! 后续阶段）。major 的 block 选择式 evacuation 与 SharedHeap 的跨 owner 传输不在本阶段。

use super::gc_metadata_section::GcRuntimeMetadata;
use super::gc_trace::walk_descriptor;
use super::local_heap_schema::{HeapTriggerProfile, LocalHeapRuntimeContract};
use super::slab::RawInvariant;

/// arena 类别；顺序即稠密下标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapArenaKind {
    Nursery = 0,
    Old = 1,
    Resource = 2,
    Pinned = 3,
    Large = 4,
}

impl HeapArenaKind {
    /// 类别数量。
    pub(crate) const COUNT: usize = 5;

    /// 返回稠密下标。
    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    /// 返回登记名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Nursery => "nursery",
            Self::Old => "old",
            Self::Resource => "resource",
            Self::Pinned => "pinned",
            Self::Large => "large",
        }
    }

    /// 返回该类别新建对象的初始 generation。
    const fn generation(self) -> u8 {
        match self {
            Self::Nursery => GENERATION_NURSERY,
            Self::Old => GENERATION_OLD,
            Self::Resource | Self::Pinned | Self::Large => GENERATION_IMMORTAL,
        }
    }
}

/// `control` word 的 generation 编码。
pub(crate) const GENERATION_NURSERY: u8 = 0;
pub(crate) const GENERATION_AGING: u8 = 1;
pub(crate) const GENERATION_OLD: u8 = 2;
pub(crate) const GENERATION_IMMORTAL: u8 = 3;

/// `control` 的字段掩码与位移。
const CONTROL_TYPE_MASK: u64 = 0xffff_ffff;
const CONTROL_AGE_SHIFT: u32 = 32;
const CONTROL_AGE_MASK: u64 = 0xf << CONTROL_AGE_SHIFT;
const CONTROL_GENERATION_SHIFT: u32 = 36;
const CONTROL_GENERATION_MASK: u64 = 0x3 << CONTROL_GENERATION_SHIFT;
const CONTROL_FORWARDED: u64 = 1 << 38;
const CONTROL_PINNED: u64 = 1 << 39;
const CONTROL_LARGE_OBJECT: u64 = 1 << 41;
const CONTROL_HAS_RESOURCE_INSTANCE: u64 = 1 << 42;
const CONTROL_REPRESENTATION_SHIFT: u32 = 43;
/// `LOCAL_DIRECT` 表示；本阶段唯一实现的 managed representation。
const REPRESENTATION_LOCAL_DIRECT: u64 = 0;

const HEADER_CONTROL: u64 = 0;
const HEADER_SIZE_WORD: u64 = 8;

/// 对象 header 字节数；与契约记录一致。
pub(crate) const OBJECT_HEADER_BYTES: u64 = 16;

const LINE_FREE: u8 = 0;
const LINE_OCCUPIED: u8 = 1;

/// LocalHeap 操作失败：容量不足由调用方补容量，其余是真正的实现不变量失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HeapError {
    /// 目标 arena 没有可用空间；调用方必须先提交更多 block。
    NoCapacity,
    /// 对象、地址或 side table 不满足不变量。
    Invariant(String),
}

impl HeapError {
    fn invalid(message: &str) -> Self {
        Self::Invariant(message.to_owned())
    }

    /// 转换成本平面的不变量错误。
    pub(crate) fn into_invariant(self) -> RawInvariant {
        match self {
            Self::NoCapacity => RawInvariant::new("LocalHeap 目标 arena 没有可用 block"),
            Self::Invariant(message) => RawInvariant::new(message),
        }
    }
}

/// 解码后的对象描述。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HeapObject {
    /// payload 起始地址。
    pub address: u64,
    /// 对象起点地址（header 之后）。
    pub object_start: u64,
    /// payload 字节数。
    pub payload_bytes: u64,
    /// 稠密 `TypeId`。
    pub type_index: u32,
    /// generation 编码。
    pub generation: u8,
    /// 已存活的 minor cycle 数。
    pub age: u8,
    /// 是否 pinned。
    pub pinned: bool,
    /// 是否 large object。
    pub large: bool,
    /// 是否已转发。
    pub forwarded: bool,
}

/// 一个 Immix block；payload 只为已提交的 block 分配。
#[derive(Debug)]
struct HeapBlock {
    /// arena 内字节偏移。
    offset: u64,
    /// 下一个候选 line；等于 `lines_per_block` 表示 block 已满。
    free_line: u32,
    /// mark 位图对应的 cycle epoch。
    mark_epoch: u64,
    /// block 的 payload 字节；长度恒为 block_bytes。
    bytes: Vec<u8>,
}

/// 一个 arena 的 side metadata 与 block 集合。
#[derive(Debug)]
pub(crate) struct HeapArena {
    kind: HeapArenaKind,
    descriptor: u64,
    base: u64,
    blocks: Vec<Option<HeapBlock>>,
    /// 一 line 一字节的 line 表。
    line_live: Vec<u8>,
    /// 每 block 的已占用 line 数。
    block_live: Vec<u32>,
    /// object-start 位图，每 granule 一位。
    object_start: Vec<u64>,
    /// mark 位图，每 granule 一位。
    mark: Vec<u64>,
    mark_epoch: u64,
    committed: u32,
    objects: u32,
    allocated: u64,
    live_bytes: u64,
    pinned: u32,
}

impl HeapArena {
    fn new(
        kind: HeapArenaKind,
        descriptor: u64,
        base: u64,
        contract: &LocalHeapRuntimeContract,
    ) -> Self {
        let blocks = usize::try_from(contract.blocks_per_arena).expect("block 数适配宿主");
        let words =
            usize::try_from(contract.object_start_bits / 64).expect("bitmap word 数适配宿主");
        let lines = usize::try_from(contract.blocks_per_arena * contract.lines_per_block)
            .expect("line 数适配宿主");
        Self {
            kind,
            descriptor,
            base,
            blocks: (0..blocks).map(|_| None).collect(),
            line_live: vec![LINE_FREE; lines],
            block_live: vec![0; blocks],
            object_start: vec![0; words],
            mark: vec![0; words],
            mark_epoch: 0,
            committed: 0,
            objects: 0,
            allocated: 0,
            live_bytes: 0,
            pinned: 0,
        }
    }

    fn block(&self, index: u32) -> Option<&HeapBlock> {
        self.blocks.get(index as usize).and_then(Option::as_ref)
    }

    fn block_mut(&mut self, index: u32) -> Option<&mut HeapBlock> {
        self.blocks.get_mut(index as usize).and_then(Option::as_mut)
    }

    fn mapped(&self, index: u32) -> bool {
        self.blocks.get(index as usize).is_some_and(Option::is_some)
    }

    /// 返回 line 表中的下标。
    fn line_index(&self, block: u32, line: u32) -> usize {
        block as usize * self.lines_per_block() + line as usize
    }

    fn lines_per_block(&self) -> usize {
        self.line_live.len() / self.blocks.len().max(1)
    }

    /// 提交一个 block。
    fn map_block(&mut self, index: u32, block_bytes: u32) -> Result<(), HeapError> {
        let slot = self
            .blocks
            .get_mut(index as usize)
            .ok_or_else(|| HeapError::invalid("LocalHeap block 下标越界"))?;
        if slot.is_none() {
            *slot = Some(HeapBlock {
                offset: u64::from(index) * u64::from(block_bytes),
                free_line: 0,
                mark_epoch: 0,
                bytes: vec![0; block_bytes as usize],
            });
            self.committed += 1;
        }
        Ok(())
    }

    fn granule(&self, offset: u64, granule_bytes: u32) -> usize {
        usize::try_from(offset / u64::from(granule_bytes)).expect("granule 下标适配宿主")
    }

    fn set_object_start(&mut self, granule: usize) {
        self.object_start[granule / 64] |= 1 << (granule % 64);
    }

    fn clear_object_start(&mut self, granule: usize) {
        self.object_start[granule / 64] &= !(1u64 << (granule % 64));
    }

    fn has_object_start(&self, granule: usize) -> bool {
        self.object_start[granule / 64] & (1 << (granule % 64)) != 0
    }

    /// test-and-mark：只在首次标记时返回 true。
    fn mark_granule(&mut self, granule: usize, block: u32) -> bool {
        let epoch = self.mark_epoch;
        let words = self.lines_per_block() * 128 / 16 / 64;
        let blocks = self.blocks.len();
        let slot = self
            .blocks
            .get_mut(block as usize)
            .and_then(Option::as_mut)
            .expect("标记必然发生在已提交 block");
        if slot.mark_epoch != epoch {
            let lines = self.line_live.len() / blocks.max(1);
            let first = block as usize * lines / 128 * 128 / 16;
            let start = first / 64;
            self.mark[start..start + words].fill(0);
            slot.mark_epoch = epoch;
        }
        let mask = 1u64 << (granule % 64);
        if self.mark[granule / 64] & mask != 0 {
            return false;
        }
        self.mark[granule / 64] |= mask;
        true
    }

    fn is_marked(&self, granule: usize, block: u32) -> bool {
        self.block(block)
            .is_some_and(|block| block.mark_epoch == self.mark_epoch)
            && self.mark[granule / 64] & (1 << (granule % 64)) != 0
    }

    /// 返回第一个还有空闲 line 的 block。
    fn next_alloc_block(&self) -> Option<u32> {
        (0..self.blocks.len() as u32).find(|index| {
            self.mapped(*index)
                && self
                    .block(*index)
                    .is_some_and(|block| (block.free_line as usize) < self.lines_per_block())
        })
    }

    /// 返回从 `start` 起的 `span` 个连续空 block。
    fn free_span(&self, span: u32) -> Option<u32> {
        if span == 0 || span > self.blocks.len() as u32 {
            return None;
        }
        (0..=self.blocks.len() as u32 - span).find(|start| {
            (0..span).all(|step| {
                let index = start + step;
                self.mapped(index) && self.block_live[index as usize] == 0
            })
        })
    }

    /// 在 block 内分配一个对象；返回 arena 内字节偏移。
    fn allocate_in_block(
        &mut self,
        block_index: u32,
        total: u64,
        align: u64,
        granule_bytes: u32,
    ) -> Result<u64, HeapError> {
        let block_base = self.block_base(block_index);
        loop {
            let free_line = self
                .block(block_index)
                .ok_or_else(|| HeapError::invalid("block 缺失"))?
                .free_line;
            let lines = self.lines_per_block() as u32;
            if free_line >= lines {
                return Err(HeapError::NoCapacity);
            }
            // 保证每次迭代都推进：当前 line 不是空闲时直接跳过它。
            if self.run_end_line(block_index, free_line) == free_line {
                self.block_mut(block_index)
                    .ok_or_else(|| HeapError::invalid("block 缺失"))?
                    .free_line = free_line + 1;
                continue;
            }
            let cursor = block_base + u64::from(free_line) * 128;
            let align = align.max(u64::from(granule_bytes));
            let header =
                align_up(cursor, align).ok_or_else(|| HeapError::invalid("对象对齐溢出"))?;
            let run_end = block_base + u64::from(self.run_end_line(block_index, free_line)) * 128;
            if header + total <= run_end {
                let first = (header - block_base) / 128;
                // `end_line` 是绝对的结束 line 序号：占用 `first..end_line`，下一条候选就是它。
                let end_line = (header + total - block_base).div_ceil(128);
                for line in first..end_line {
                    let index = self.line_index(block_index, line as u32);
                    if self.line_live[index] == LINE_FREE {
                        self.line_live[index] = LINE_OCCUPIED;
                        self.block_live[block_index as usize] += 1;
                    }
                }
                let next = u32::try_from(end_line).expect("line 序号适配 u32");
                self.block_mut(block_index)
                    .ok_or_else(|| HeapError::invalid("block 缺失"))?
                    .free_line = next;
                return Ok(header);
            }
            // Immix 行为：这个 run 放不下就把剩余 line 记为内部碎片，继续找下一个 run。
            self.consume_lines(block_index, free_line);
        }
    }

    /// 返回从 `line` 起的空闲 run 的结束 line 序号。
    fn run_end_line(&self, block_index: u32, line: u32) -> u32 {
        let lines = self.lines_per_block() as u32;
        let mut end = line;
        while end < lines && self.line_live[self.line_index(block_index, end)] == LINE_FREE {
            end += 1;
        }
        end
    }

    /// 把从 `line` 起的空闲 run 标为占用。
    fn consume_lines(&mut self, block_index: u32, line: u32) {
        let end = self.run_end_line(block_index, line);
        for index in line..end {
            let slot = self.line_index(block_index, index);
            if self.line_live[slot] == LINE_FREE {
                self.line_live[slot] = LINE_OCCUPIED;
                self.block_live[block_index as usize] += 1;
            }
        }
        if let Some(block) = self.block_mut(block_index) {
            block.free_line = end;
        }
    }

    /// 返回 block 在 arena 内的起始字节偏移。
    fn block_base(&self, block: u32) -> u64 {
        u64::from(block) * self.line_live.len() as u64 / self.blocks.len() as u64 * 128
    }

    /// 释放一个 block 内 `[local, local + bytes)` 覆盖的 line。
    fn free_range(&mut self, block_index: u32, local: u64, bytes: u64) {
        let lines = self.lines_per_block() as u64;
        let first = local / 128;
        let end_line = (local + bytes).div_ceil(128);
        let mut lowest = u32::MAX;
        for line in first..end_line {
            if line >= lines {
                break;
            }
            let index = self.line_index(block_index, u32::try_from(line).expect("line 序号"));
            if self.line_live[index] == LINE_OCCUPIED {
                self.line_live[index] = LINE_FREE;
                let live = &mut self.block_live[block_index as usize];
                *live = live.saturating_sub(1);
            }
            lowest = lowest.min(u32::try_from(line).expect("line 序号"));
        }
        if lowest != u32::MAX
            && let Some(block) = self.block_mut(block_index)
        {
            block.free_line = block.free_line.min(lowest);
        }
    }

    /// 返回 arena 内已提交的 block 下标快照。
    fn committed_blocks(&self) -> Vec<u32> {
        (0..self.blocks.len() as u32)
            .filter(|index| self.mapped(*index))
            .collect()
    }

    /// 枚举一个 block 内的全部对象（granule 下标, arena 内偏移）。
    fn objects_in_block(&self, block: u32, granule_bytes: u32) -> Vec<(usize, u64)> {
        let lines = self.lines_per_block() as u32;
        let block_base = u64::from(block) * u64::from(lines) * 128;
        let first = block_base / u64::from(granule_bytes);
        let end = (block_base + u64::from(lines) * 128) / u64::from(granule_bytes);
        (first..end)
            .filter(|granule| self.has_object_start(*granule as usize))
            .map(|granule| (granule as usize, granule * u64::from(granule_bytes)))
            .collect()
    }
}

/// 一个 owner 的 TLAB 窗口。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Tlab {
    pub active: bool,
    pub start_block: u32,
    pub end_block: u32,
}

/// 一个 cycle 的报告。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CycleReport {
    pub scanned_words: u32,
    pub evacuated: u32,
    pub promoted: u32,
    pub reclaimed_blocks: u32,
    pub reclaimed_lines: u32,
    pub marked: u32,
}

/// LocalHeap 累计计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HeapCounters {
    pub committed_blocks: u32,
    pub objects: u32,
    pub live_bytes: u64,
    pub allocated_bytes: u64,
    pub minor_cycles: u64,
    pub major_cycles: u64,
    pub tlab_refills: u64,
    pub pins: u32,
    pub scanned_words: u64,
    pub evacuated_objects: u64,
}

/// pin side table 的一项。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PinEntry {
    pub address: u64,
    pub count: u32,
}

/// 一个 owner 的 LocalHeap。
#[derive(Debug)]
pub(crate) struct LocalHeap {
    block_bytes: u32,
    granule_bytes: u32,
    lines_per_block: u32,
    tlab_span_blocks: u32,
    tenure_age: u8,
    trigger: HeapTriggerProfile,
    arenas: Vec<HeapArena>,
    tlab: Tlab,
    pins: Vec<PinEntry>,
    nursery_bytes: u64,
    minor_cycles: u64,
    major_cycles: u64,
    next_descriptor: u64,
    scanned_words: u64,
    evacuated_objects: u64,
    tlab_refills: u64,
    /// 扫描对象的 pointer word 暂存区；复用避免每次扫描分配。
    scratch: Vec<(u64, u64)>,
    /// 待处理对象的标记栈；同样复用。
    worklist: Vec<u64>,
}

impl LocalHeap {
    /// 按契约创建空 heap；arena 与 block 在需要时提交。
    pub(crate) fn new(contract: &LocalHeapRuntimeContract) -> Self {
        Self {
            block_bytes: contract.block_bytes(),
            granule_bytes: contract.granule_bytes,
            lines_per_block: contract.lines_per_block,
            tlab_span_blocks: contract.tlab_span_blocks,
            tenure_age: contract.trigger().tenure_age,
            trigger: contract.trigger(),
            arenas: Vec::new(),
            tlab: Tlab::default(),
            pins: Vec::new(),
            nursery_bytes: 0,
            minor_cycles: 0,
            major_cycles: 0,
            next_descriptor: 1,
            scanned_words: 0,
            evacuated_objects: 0,
            tlab_refills: 0,
            scratch: Vec::with_capacity(16),
            worklist: Vec::with_capacity(32),
        }
    }

    pub(crate) fn trigger(&self) -> HeapTriggerProfile {
        self.trigger
    }

    pub(crate) fn nursery_bytes(&self) -> u64 {
        self.nursery_bytes
    }

    /// 返回一个 arena 的 block 数。
    pub(crate) fn blocks_per_arena(&self) -> u32 {
        self.arenas
            .first()
            .map_or(0, |arena| arena.blocks.len() as u32)
    }

    /// 返回 TLAB span 覆盖的 block 数。
    pub(crate) fn tlab_span_blocks(&self) -> u32 {
        self.tlab_span_blocks
    }

    /// 返回 arena 内是否存在 `span` 个连续空 block。
    pub(crate) fn free_span(&self, arena_index: usize, span: u32) -> Option<u32> {
        self.arenas.get(arena_index)?.free_span(span)
    }

    /// 返回 arena 是否还能满足一次分配：`span > 1` 时要求连续空 block。
    pub(crate) fn has_allocatable(&self, arena_index: usize, span: u32) -> bool {
        self.arenas.get(arena_index).is_some_and(|arena| {
            arena.free_blocks() > 0 && (span <= 1 || arena.free_span(span).is_some())
        })
    }

    pub(crate) fn minor_due(&self) -> bool {
        self.nursery_bytes >= self.trigger.minor_trigger_bytes
    }

    pub(crate) fn arena_count(&self) -> usize {
        self.arenas.len()
    }

    pub(crate) fn arena(&self, index: usize) -> Option<&HeapArena> {
        self.arenas.get(index)
    }

    pub(crate) fn pins(&self) -> &[PinEntry] {
        &self.pins
    }

    /// 返回某类别的第 `ordinal` 个 arena 下标。
    pub(crate) fn arena_index(&self, kind: HeapArenaKind, ordinal: usize) -> Option<usize> {
        self.arenas_of(kind).nth(ordinal)
    }

    fn arenas_of(&self, kind: HeapArenaKind) -> impl Iterator<Item = usize> + '_ {
        self.arenas
            .iter()
            .enumerate()
            .filter(move |(_, arena)| arena.kind == kind)
            .map(|(index, _)| index)
    }

    /// 登记一个新 arena；返回下标。
    pub(crate) fn attach_arena(
        &mut self,
        kind: HeapArenaKind,
        base: u64,
        contract: &LocalHeapRuntimeContract,
    ) -> usize {
        let descriptor = self.next_descriptor;
        self.next_descriptor += 1;
        self.arenas
            .push(HeapArena::new(kind, descriptor, base, contract));
        self.arenas.len() - 1
    }

    /// 在指定 arena 中提交一个 block。
    pub(crate) fn commit_block(&mut self, index: usize, block: u32) -> Result<(), HeapError> {
        let bytes = self.block_bytes;
        self.arenas
            .get_mut(index)
            .ok_or_else(|| HeapError::invalid("LocalHeap arena 下标越界"))?
            .map_block(block, bytes)
    }

    /// 提交某类别的一个 block；返回它所在的 arena 与 block 下标。
    pub(crate) fn commit_next_block(
        &mut self,
        kind: HeapArenaKind,
    ) -> Result<(usize, u32), HeapError> {
        let arena_index = self.arenas_of(kind).next().ok_or(HeapError::NoCapacity)?;
        let bytes = self.block_bytes;
        let arena = &mut self.arenas[arena_index];
        let block = (0..arena.blocks.len() as u32)
            .find(|index| !arena.mapped(*index))
            .ok_or(HeapError::NoCapacity)?;
        arena.map_block(block, bytes)?;
        Ok((arena_index, block))
    }

    /// 分配一个 managed 对象；返回 payload 地址。
    pub(crate) fn allocate(
        &mut self,
        kind: HeapArenaKind,
        type_index: u32,
        payload_bytes: u64,
        align: u64,
    ) -> Result<u64, HeapError> {
        let total = OBJECT_HEADER_BYTES
            .checked_add(payload_bytes)
            .ok_or_else(|| HeapError::invalid("对象 footprint 溢出"))?;
        let (arena_index, block, offset) = if kind == HeapArenaKind::Large {
            self.allocate_large(total)?
        } else {
            self.allocate_small(kind, total, align)?
        };
        let arena = &mut self.arenas[arena_index];
        let generation = if kind == HeapArenaKind::Old {
            GENERATION_OLD
        } else {
            kind.generation()
        };
        let control = u64::from(type_index)
            | (u64::from(generation) << CONTROL_GENERATION_SHIFT)
            | (REPRESENTATION_LOCAL_DIRECT << CONTROL_REPRESENTATION_SHIFT)
            | if kind == HeapArenaKind::Large {
                CONTROL_LARGE_OBJECT
            } else {
                0
            };
        let block_ref = arena
            .block_mut(block)
            .ok_or_else(|| HeapError::invalid("block 缺失"))?;
        let local = offset - block_ref.offset;
        write_word(&mut block_ref.bytes, local + HEADER_CONTROL, control)?;
        write_word(
            &mut block_ref.bytes,
            local + HEADER_SIZE_WORD,
            payload_bytes,
        )?;
        let granule = arena.granule(offset, self.granule_bytes);
        arena.set_object_start(granule);
        arena.objects += 1;
        arena.allocated += total;
        arena.live_bytes += total;
        if kind == HeapArenaKind::Nursery {
            self.nursery_bytes += total;
        }
        Ok(arena.base + offset + OBJECT_HEADER_BYTES)
    }

    /// 小对象分配：nursery 走 TLAB span，其余类别走该类别的 arena。
    fn allocate_small(
        &mut self,
        kind: HeapArenaKind,
        total: u64,
        align: u64,
    ) -> Result<(usize, u32, u64), HeapError> {
        let arena_index = self.arenas_of(kind).next().ok_or(HeapError::NoCapacity)?;
        if kind == HeapArenaKind::Nursery && !self.tlab.active {
            self.refill_tlab(arena_index)?;
        }
        let candidates: Vec<u32> = if kind == HeapArenaKind::Nursery {
            (self.tlab.start_block..self.tlab.end_block).collect()
        } else {
            let arena = &self.arenas[arena_index];
            arena
                .committed_blocks()
                .into_iter()
                .filter(|block| {
                    arena.block(*block).is_some_and(|block| {
                        (block.free_line as usize) < self.lines_per_block as usize
                    })
                })
                .collect()
        };
        for block in candidates {
            let arena = &mut self.arenas[arena_index];
            if let Ok(offset) = arena.allocate_in_block(block, total, align, self.granule_bytes) {
                return Ok((arena_index, block, offset));
            }
        }
        if kind == HeapArenaKind::Nursery {
            self.tlab.active = false;
        }
        Err(HeapError::NoCapacity)
    }

    /// 大对象：占用若干连续空 block。
    fn allocate_large(&mut self, total: u64) -> Result<(usize, u32, u64), HeapError> {
        let arena_index = self
            .arenas_of(HeapArenaKind::Large)
            .next()
            .ok_or(HeapError::NoCapacity)?;
        let span = u32::try_from(total.div_ceil(u64::from(self.block_bytes)))
            .map_err(|_| HeapError::invalid("大对象 block 数溢出"))?;
        let arena = &mut self.arenas[arena_index];
        let start = arena.free_span(span).ok_or(HeapError::NoCapacity)?;
        let lines = arena.lines_per_block() as u32;
        for step in 0..span {
            let base = arena.line_index(start + step, 0);
            for line in 0..lines {
                arena.line_live[base + line as usize] = LINE_OCCUPIED;
            }
            arena.block_live[(start + step) as usize] = lines;
            let block = arena
                .block_mut(start + step)
                .ok_or_else(|| HeapError::invalid("block 缺失"))?;
            block.free_line = u32::MAX;
        }
        Ok((
            arena_index,
            start,
            u64::from(start) * u64::from(self.block_bytes),
        ))
    }

    /// 取一段新的 TLAB span；没有连续空 block 时报容量不足。
    fn refill_tlab(&mut self, arena_index: usize) -> Result<(), HeapError> {
        let span = self.tlab_span_blocks;
        let arena = &self.arenas[arena_index];
        let start = arena.free_span(span).ok_or(HeapError::NoCapacity)?;
        self.tlab = Tlab {
            active: true,
            start_block: start,
            end_block: start + span,
        };
        self.tlab_refills += 1;
        Ok(())
    }

    /// 定位地址所属的 arena 与其中偏移。
    fn locate(&self, address: u64) -> Result<(usize, u64), HeapError> {
        self.arenas
            .iter()
            .enumerate()
            .find_map(|(index, arena)| {
                (address >= arena.base
                    && address
                        < arena.base + u64::from(self.block_bytes) * arena.blocks.len() as u64)
                    .then(|| (index, address - arena.base))
            })
            .ok_or_else(|| HeapError::invalid("地址不在任何 LocalHeap arena 内"))
    }

    /// 解析一个 managed pointer：返回对象起点（payload 地址）。
    ///
    /// managed pointer 指向 payload；interior pointer 落在对象内部，因此按 granule 向前回表，
    /// 这与 `page_covering_object` 的语义一致。
    pub(crate) fn resolve(&self, address: u64) -> Result<u64, HeapError> {
        if address < OBJECT_HEADER_BYTES {
            return Err(HeapError::invalid("managed pointer 小于 header 宽度"));
        }
        let (arena_index, offset) = self.locate(address)?;
        let arena = &self.arenas[arena_index];
        let header_offset = offset - OBJECT_HEADER_BYTES;
        let granule = arena.granule(header_offset, self.granule_bytes);
        // 对象起点按 granule 计：起点地址必须由命中的 granule 反推，而不是用入参偏移。
        let start_of = |granule: usize| {
            arena.base + granule as u64 * u64::from(self.granule_bytes) + OBJECT_HEADER_BYTES
        };
        if arena.has_object_start(granule) {
            return Ok(start_of(granule));
        }
        // interior：在同一 host page 内向前找最近的对象起点。
        let page_granule =
            self.arenas[arena_index].granule(header_offset / 4096 * 4096, self.granule_bytes);
        let mut candidate = granule;
        while candidate > page_granule {
            candidate -= 1;
            if arena.has_object_start(candidate) {
                return Ok(start_of(candidate));
            }
        }
        Err(HeapError::invalid("interior pointer 找不到覆盖它的对象"))
    }

    /// 读取一个对象的 header 描述。
    pub(crate) fn object_at(&self, address: u64) -> Result<HeapObject, HeapError> {
        let payload = self.resolve(address)?;
        let (arena_index, offset) = self.locate(payload)?;
        let arena = &self.arenas[arena_index];
        let header_offset = offset - OBJECT_HEADER_BYTES;
        let block_index = header_offset / u64::from(self.block_bytes);
        let block = arena
            .block(u32::try_from(block_index).expect("block 下标适配 u32"))
            .ok_or_else(|| HeapError::invalid("对象所在 block 未提交"))?;
        let local = header_offset - block.offset;
        let control = read_word(&block.bytes, local + HEADER_CONTROL)?;
        let size = read_word(&block.bytes, local + HEADER_SIZE_WORD)?;
        Ok(HeapObject {
            address,
            object_start: payload,
            payload_bytes: size,
            type_index: (control & CONTROL_TYPE_MASK) as u32,
            generation: ((control & CONTROL_GENERATION_MASK) >> CONTROL_GENERATION_SHIFT) as u8,
            age: ((control & CONTROL_AGE_MASK) >> CONTROL_AGE_SHIFT) as u8,
            pinned: control & CONTROL_PINNED != 0,
            large: control & CONTROL_LARGE_OBJECT != 0,
            forwarded: control & CONTROL_FORWARDED != 0,
        })
    }

    /// 读写对象 payload 中的 8 字节字段。
    pub(crate) fn field(&self, address: u64, offset: u64) -> Result<u64, HeapError> {
        let (arena_index, payload_offset) = self.locate(address)?;
        let arena = &self.arenas[arena_index];
        let block_index = payload_offset / u64::from(self.block_bytes);
        let block = arena
            .block(u32::try_from(block_index).expect("block 下标适配 u32"))
            .ok_or_else(|| HeapError::invalid("对象所在 block 未提交"))?;
        read_word(&block.bytes, payload_offset - block.offset + offset)
    }

    /// 写入对象 payload 中的 8 字节字段。
    pub(crate) fn set_field(
        &mut self,
        address: u64,
        offset: u64,
        value: u64,
    ) -> Result<(), HeapError> {
        let (arena_index, payload_offset) = self.locate(address)?;
        let arena = &mut self.arenas[arena_index];
        let block_index = payload_offset / u64::from(self.block_bytes);
        let block = arena
            .block_mut(u32::try_from(block_index).expect("block 下标适配 u32"))
            .ok_or_else(|| HeapError::invalid("对象所在 block 未提交"))?;
        write_word(
            &mut block.bytes,
            payload_offset - block.offset + offset,
            value,
        )
    }

    /// 返回当前 cycle 的 arena 描述符快照（arena 下标, descriptor）。
    pub(crate) fn arena_descriptors(&self) -> Vec<(usize, u64)> {
        self.arenas
            .iter()
            .enumerate()
            .map(|(index, arena)| (index, arena.descriptor()))
            .collect()
    }

    /// 返回累计计数。
    pub(crate) fn counters(&self) -> HeapCounters {
        let mut counters = HeapCounters {
            minor_cycles: self.minor_cycles,
            major_cycles: self.major_cycles,
            tlab_refills: self.tlab_refills,
            scanned_words: self.scanned_words,
            evacuated_objects: self.evacuated_objects,
            ..HeapCounters::default()
        };
        for arena in &self.arenas {
            counters.committed_blocks += arena.committed;
            counters.objects += arena.objects;
            counters.live_bytes += arena.live_bytes;
            counters.allocated_bytes += arena.allocated;
            counters.pins += arena.pinned;
        }
        counters
    }

    /// 判断地址是否落在本 heap 的 arena 内。
    pub(crate) fn contains(&self, address: u64) -> bool {
        self.locate(address).is_ok()
    }

    /// 返回地址所属 arena 的下标与 barrier 身份。
    pub(crate) fn arena_of(&self, address: u64) -> Result<(usize, u64), HeapError> {
        let (index, _) = self.locate(address)?;
        Ok((index, self.arenas[index].descriptor))
    }

    /// 返回地址所在 block 的 arena 内下标；按对象 header 归属计算。
    pub(crate) fn block_of(&self, address: u64) -> Result<u32, HeapError> {
        let (_, offset) = self.locate(address)?;
        let header = offset.saturating_sub(OBJECT_HEADER_BYTES);
        Ok(u32::try_from(header / u64::from(self.block_bytes)).expect("block 下标适配 u32"))
    }
}

impl HeapArena {
    pub(crate) fn kind(&self) -> HeapArenaKind {
        self.kind
    }

    pub(crate) fn descriptor(&self) -> u64 {
        self.descriptor
    }

    pub(crate) fn base(&self) -> u64 {
        self.base
    }

    pub(crate) fn committed(&self) -> u32 {
        self.committed
    }

    pub(crate) fn objects(&self) -> u32 {
        self.objects
    }

    pub(crate) fn live_bytes(&self) -> u64 {
        self.live_bytes
    }

    pub(crate) fn has_pins(&self) -> bool {
        self.pinned != 0
    }

    /// 返回仍有空闲 line 的已提交 block 数。
    pub(crate) fn free_blocks(&self) -> u32 {
        (0..self.blocks.len() as u32)
            .filter(|index| {
                self.mapped(*index)
                    && self
                        .block(*index)
                        .is_some_and(|block| (block.free_line as usize) < self.lines_per_block())
            })
            .count() as u32
    }
}

impl LocalHeap {
    /// 在 payload 的可变字节视图上执行一次操作；`payload_base` 是该视图的地址。
    fn with_payload<R>(
        &mut self,
        address: u64,
        visit: impl FnOnce(&mut [u8], u64) -> Result<R, HeapError>,
    ) -> Result<R, HeapError> {
        let block_bytes = self.block_bytes;
        let (arena_index, offset) = self.locate(address)?;
        let base = self.arenas[arena_index].base;
        let block_index = u32::try_from(offset / u64::from(block_bytes)).expect("block 下标");
        let arena = &mut self.arenas[arena_index];
        let block = arena
            .block_mut(block_index)
            .ok_or_else(|| HeapError::invalid("对象所在 block 未提交"))?;
        let local = usize::try_from(offset - block.offset).expect("block 内偏移适配宿主");
        let bytes = block
            .bytes
            .get_mut(local..)
            .ok_or_else(|| HeapError::invalid("payload 越过 block"))?;
        visit(bytes, base + offset)
    }

    /// 收集一个对象的 pointer word 到 scratch。
    fn collect_words(&mut self, address: u64, trace: &[u8]) -> Result<u32, HeapError> {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        let words = self.with_payload(address, |payload, payload_base| {
            let scan = walk_descriptor(trace, payload, payload_base, &mut |word, word_address| {
                scratch.push((word_address - payload_base, u64::from_le_bytes(*word)));
                Ok(())
            })
            .map_err(|error| HeapError::Invariant(error.message().to_owned()))?;
            Ok(scan.pointers)
        });
        self.scratch = scratch;
        words
    }

    /// 把 scratch 中的 word 写回；`evacuate_nursery` 决定是否搬运指向 nursery 的目标。
    fn rewrite_words(
        &mut self,
        address: u64,
        evacuate_nursery: bool,
        report: &mut CycleReport,
    ) -> Result<(), HeapError> {
        let scratch = std::mem::take(&mut self.scratch);
        for (offset, value) in &scratch {
            if *value == 0 {
                continue;
            }
            let target = self.resolve(*value)?;
            let object = self.object_at(target)?;
            let new_value = if object.forwarded || (evacuate_nursery && self.in_nursery(target)) {
                let moved = self.evacuate(*value, report)?;
                moved
            } else {
                *value
            };
            if new_value != *value {
                self.set_field(address, *offset, new_value)?;
            }
        }
        self.scratch = scratch;
        Ok(())
    }

    /// 判断 payload 地址是否位于 nursery。
    pub(crate) fn in_nursery(&self, address: u64) -> bool {
        self.locate(address)
            .is_ok_and(|(index, _)| self.arenas[index].kind == HeapArenaKind::Nursery)
    }

    /// 搬运一个指针目标到 old（或 pinned arena），返回新指针值。
    ///
    /// 已转发对象直接读回 header 的 forward word，因此同一 cycle 内的多次引用只搬运一次。
    fn evacuate(&mut self, value: u64, report: &mut CycleReport) -> Result<u64, HeapError> {
        let object = self.object_at(value)?;
        if object.forwarded {
            let forward =
                self.field(object.object_start - OBJECT_HEADER_BYTES, HEADER_SIZE_WORD)?;
            return Ok(forward + (value - object.object_start));
        }
        let age = object.age.saturating_add(1);
        let generation = if object.pinned || age >= self.tenure_age {
            GENERATION_OLD
        } else {
            GENERATION_AGING
        };
        let destination = if object.pinned {
            HeapArenaKind::Pinned
        } else if object.large {
            HeapArenaKind::Large
        } else {
            HeapArenaKind::Old
        };
        let moved = self.copy_object(&object, destination, generation, age)?;
        let source_header = object.object_start - OBJECT_HEADER_BYTES;
        let control = self.field(source_header, HEADER_CONTROL)? | CONTROL_FORWARDED;
        // 转发地址写在原 header 的第二个 word：原对象不再按普通 descriptor 扫描。
        self.set_field(source_header, HEADER_CONTROL, control)?;
        self.set_field(source_header, HEADER_SIZE_WORD, moved)?;
        report.evacuated += 1;
        if generation != GENERATION_AGING {
            report.promoted += 1;
        }
        self.evacuated_objects += 1;
        Ok(moved + (value - object.object_start))
    }

    /// 复制一个对象到目标类别；返回新 payload 地址。
    fn copy_object(
        &mut self,
        object: &HeapObject,
        kind: HeapArenaKind,
        generation: u8,
        age: u8,
    ) -> Result<u64, HeapError> {
        let payload = self.payload_copy(object)?;
        let moved = self.allocate(kind, object.type_index, object.payload_bytes, 8)?;
        self.with_payload(moved, |target, _| {
            let source = payload
                .get(..target.len().min(payload.len()))
                .ok_or_else(|| HeapError::invalid("payload 副本长度不足"))?;
            target[..source.len()].copy_from_slice(source);
            Ok(())
        })?;
        let header = moved - OBJECT_HEADER_BYTES;
        let control = self.field(header, HEADER_CONTROL)?
            & !(CONTROL_AGE_MASK | CONTROL_GENERATION_MASK | CONTROL_FORWARDED)
            | (u64::from(age) << CONTROL_AGE_SHIFT)
            | (u64::from(generation) << CONTROL_GENERATION_SHIFT);
        self.set_field(header, HEADER_CONTROL, control)?;
        Ok(moved)
    }

    /// 读取对象 payload 的字节副本。
    fn payload_copy(&self, object: &HeapObject) -> Result<Vec<u8>, HeapError> {
        let (arena_index, offset) = self.locate(object.object_start)?;
        let block_index = u32::try_from(offset / u64::from(self.block_bytes)).expect("block");
        let arena = &self.arenas[arena_index];
        let block = arena
            .block(block_index)
            .ok_or_else(|| HeapError::invalid("对象所在 block 未提交"))?;
        let local = usize::try_from(offset - block.offset).expect("block 内偏移");
        let end = local
            .checked_add(usize::try_from(object.payload_bytes).expect("payload 长度适配宿主"))
            .ok_or_else(|| HeapError::invalid("payload 范围溢出"))?;
        block
            .bytes
            .get(local..end)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| HeapError::invalid("payload 越过 block"))
    }

    /// 原子的 pin：nursery/aging 对象先提升到 pinned arena，再增加 side-table 计数。
    ///
    /// 提升是 safepoint 行为：调用方在返回后必须使用新的地址，且全部强引用已被改写。
    pub(crate) fn pin(
        &mut self,
        address: u64,
        types: &GcRuntimeMetadata,
        roots: &mut [u64],
    ) -> Result<(u64, u32), HeapError> {
        let object = self.object_at(address)?;
        let mut current = address;
        if object.generation <= GENERATION_AGING {
            let mut report = CycleReport::default();
            current = self.promote_pinned(address, &mut report)?;
            self.fixup_forwarded(types, roots, &mut report)?;
        }
        let entry = self
            .pins
            .iter_mut()
            .find(|entry| entry.address == current)
            .map(|entry| &mut entry.count);
        let count = match entry {
            Some(count) => {
                *count += 1;
                *count
            }
            None => {
                self.pins.push(PinEntry {
                    address: current,
                    count: 1,
                });
                1
            }
        };
        let header = current - OBJECT_HEADER_BYTES;
        let control = self.field(header, HEADER_CONTROL)? | CONTROL_PINNED;
        self.set_field(header, HEADER_CONTROL, control)?;
        if let Ok((index, _)) = self.locate(current) {
            self.arenas[index].pinned += 1;
        }
        Ok((current, count))
    }

    /// 把 nursery/aging 对象复制到 pinned arena 并留下转发。
    fn promote_pinned(&mut self, address: u64, report: &mut CycleReport) -> Result<u64, HeapError> {
        let object = self.object_at(address)?;
        let moved = self.copy_object(
            &object,
            HeapArenaKind::Pinned,
            GENERATION_IMMORTAL,
            object.age,
        )?;
        let control = self.field(moved - OBJECT_HEADER_BYTES, HEADER_CONTROL)? | CONTROL_PINNED;
        self.set_field(moved - OBJECT_HEADER_BYTES, HEADER_CONTROL, control)?;
        let source_header = object.object_start - OBJECT_HEADER_BYTES;
        let forwarded = self.field(source_header, HEADER_CONTROL)? | CONTROL_FORWARDED;
        self.set_field(source_header, HEADER_CONTROL, forwarded)?;
        self.set_field(source_header, HEADER_SIZE_WORD, moved)?;
        report.evacuated += 1;
        self.evacuated_objects += 1;
        Ok(moved)
    }

    /// 取消 pin：计数归零时清除 header 的 PINNED 位。
    pub(crate) fn unpin(&mut self, address: u64) -> Result<u32, HeapError> {
        let index = self
            .pins
            .iter()
            .position(|entry| entry.address == address)
            .ok_or_else(|| HeapError::invalid("unpin 的对象不在 pin side table"))?;
        self.pins[index].count -= 1;
        let count = self.pins[index].count;
        if count == 0 {
            self.pins.remove(index);
            let header = address - OBJECT_HEADER_BYTES;
            let control = self.field(header, HEADER_CONTROL)? & !CONTROL_PINNED;
            self.set_field(header, HEADER_CONTROL, control)?;
            if let Ok((arena, _)) = self.locate(address) {
                self.arenas[arena].pinned = self.arenas[arena].pinned.saturating_sub(1);
            }
        }
        Ok(count)
    }

    /// 改写全部指向已转发对象的引用（pin 提升后的 fixup）。
    fn fixup_forwarded(
        &mut self,
        types: &GcRuntimeMetadata,
        roots: &mut [u64],
        report: &mut CycleReport,
    ) -> Result<(), HeapError> {
        for slot in roots.iter_mut() {
            if *slot == 0 {
                continue;
            }
            *slot = self.rewrite_pointer(*slot, report)?;
        }
        for arena_index in 0..self.arenas.len() {
            let objects = self.all_objects(arena_index);
            for address in objects {
                let object = self.object_at(address)?;
                let trace = self.trace_for(object.type_index, types)?;
                self.collect_words(address, &trace)?;
                self.rewrite_words(address, false, report)?;
            }
        }
        Ok(())
    }

    /// 只改写已转发指针，保持其他指针不变。
    fn rewrite_pointer(&mut self, value: u64, report: &mut CycleReport) -> Result<u64, HeapError> {
        let object = self.object_at(value)?;
        if !object.forwarded {
            return Ok(value);
        }
        let forward = self.field(object.object_start - OBJECT_HEADER_BYTES, HEADER_SIZE_WORD)?;
        report.evacuated += 0;
        Ok(forward + (value - object.object_start))
    }

    /// 返回 arena 内全部对象的 payload 地址。
    fn all_objects(&self, arena_index: usize) -> Vec<u64> {
        let arena = &self.arenas[arena_index];
        let mut objects = Vec::new();
        for block in arena.committed_blocks() {
            for (_, offset) in arena.objects_in_block(block, self.granule_bytes) {
                objects.push(arena.base + offset + OBJECT_HEADER_BYTES);
            }
        }
        objects
    }

    /// 返回类型的 trace descriptor。
    fn trace_for(&self, type_index: u32, types: &GcRuntimeMetadata) -> Result<Vec<u8>, HeapError> {
        types
            .types()
            .get(usize::try_from(type_index).expect("类型下标适配宿主"))
            .map(|entry| entry.trace.clone())
            .ok_or_else(|| HeapError::invalid("trace 引用了不存在的类型"))
    }

    /// minor cycle：从根与 remembered set 出发搬运 nursery 存活对象。
    pub(crate) fn collect_minor(
        &mut self,
        types: &GcRuntimeMetadata,
        roots: &mut [u64],
        dirty: &[(usize, u32)],
    ) -> Result<CycleReport, HeapError> {
        let mut report = CycleReport::default();
        // 先记下本 cycle 之前就已经 aging 的对象：只有它们在本 cycle 加龄，刚刚搬运过来的
        // 对象必须保持 age=1，不能在同一次 minor 里被重复加龄。
        let aging = self.aging_objects();
        for slot in roots.iter_mut() {
            if *slot != 0 && self.in_nursery(*slot) {
                *slot = self.evacuate(*slot, &mut report)?;
            }
        }
        self.scan_remembered_set(types, dirty, true, &mut report)?;
        self.age_objects(&aging);
        self.reset_nursery();
        self.minor_cycles += 1;
        self.scanned_words += u64::from(report.scanned_words);
        Ok(report)
    }

    /// major cycle：标记后做 owner-local line 回收。
    pub(crate) fn collect_major(
        &mut self,
        types: &GcRuntimeMetadata,
        roots: &mut [u64],
        dirty: &[(usize, u32)],
    ) -> Result<CycleReport, HeapError> {
        for arena in &mut self.arenas {
            arena.mark_epoch += 1;
        }
        let mut report = CycleReport::default();
        let mut worklist = std::mem::take(&mut self.worklist);
        worklist.clear();
        for slot in roots.iter() {
            if *slot != 0 {
                worklist.push(self.resolve(*slot)?);
            }
        }
        for (arena_index, card) in dirty {
            for address in self.card_objects(*arena_index, *card) {
                worklist.push(address);
            }
        }
        while let Some(address) = worklist.pop() {
            let object = self.object_at(address)?;
            let granule = {
                let (arena_index, offset) = self.locate(address)?;
                let block =
                    u32::try_from((offset - OBJECT_HEADER_BYTES) / u64::from(self.block_bytes))
                        .expect("block 下标");
                let arena = &mut self.arenas[arena_index];
                let granule = arena.granule(offset - OBJECT_HEADER_BYTES, self.granule_bytes);
                if !arena.mark_granule(granule, block) {
                    continue;
                }
                granule
            };
            let _ = granule;
            report.marked += 1;
            let trace = self.trace_for(object.type_index, types)?;
            report.scanned_words += self.collect_words(address, &trace)?;
            let scratch = std::mem::take(&mut self.scratch);
            for (_, value) in &scratch {
                if *value != 0 {
                    let target = self.resolve(*value)?;
                    worklist.push(target);
                }
            }
            self.scratch = scratch;
        }
        self.worklist = worklist;
        self.sweep(&mut report)?;
        self.major_cycles += 1;
        self.scanned_words += u64::from(report.scanned_words);
        Ok(report)
    }

    /// 扫描 remembered set：对被写过的 card 上的对象重新扫描。
    fn scan_remembered_set(
        &mut self,
        types: &GcRuntimeMetadata,
        dirty: &[(usize, u32)],
        evacuate_nursery: bool,
        report: &mut CycleReport,
    ) -> Result<(), HeapError> {
        for (arena_index, card) in dirty {
            for address in self.card_objects(*arena_index, *card) {
                if self.in_nursery(address) {
                    continue;
                }
                let object = self.object_at(address)?;
                let trace = self.trace_for(object.type_index, types)?;
                report.scanned_words += self.collect_words(address, &trace)?;
                self.rewrite_words(address, evacuate_nursery, report)?;
            }
        }
        Ok(())
    }

    /// 返回一个 card 覆盖范围内的对象起点。
    fn card_objects(&self, arena_index: usize, card: u32) -> Vec<u64> {
        let arena = match self.arenas.get(arena_index) {
            Some(arena) if arena.kind != HeapArenaKind::Nursery => arena,
            _ => return Vec::new(),
        };
        let start = u64::from(card) * 512;
        let block_bytes = u64::from(self.block_bytes);
        let first_block = (start / block_bytes) as u32;
        let mut objects = Vec::new();
        for block in first_block..=((start + 511) / block_bytes) as u32 {
            if !arena.mapped(block) {
                continue;
            }
            for (_, offset) in arena.objects_in_block(block, self.granule_bytes) {
                if offset >= start && offset < start + 512 {
                    objects.push(arena.base + offset + OBJECT_HEADER_BYTES);
                }
            }
        }
        objects
    }

    /// 返回当前处于 aging 的对象地址。
    fn aging_objects(&self) -> Vec<u64> {
        let mut aging = Vec::new();
        for arena_index in 0..self.arenas.len() {
            if self.arenas[arena_index].kind != HeapArenaKind::Old {
                continue;
            }
            for address in self.all_objects(arena_index) {
                if self
                    .object_at(address)
                    .is_ok_and(|object| object.generation == GENERATION_AGING)
                {
                    aging.push(address);
                }
            }
        }
        aging
    }

    /// 让指定对象原地加龄，达到 tenure 的转为 old。
    fn age_objects(&mut self, addresses: &[u64]) {
        let tenure = self.tenure_age;
        for address in addresses {
            {
                let Ok(object) = self.object_at(*address) else {
                    continue;
                };
                let age = object.age.saturating_add(1);
                let generation = if age >= tenure {
                    GENERATION_OLD
                } else {
                    GENERATION_AGING
                };
                let header = *address - OBJECT_HEADER_BYTES;
                let Ok(control) = self.field(header, HEADER_CONTROL) else {
                    continue;
                };
                let control = control & !(CONTROL_AGE_MASK | CONTROL_GENERATION_MASK)
                    | (u64::from(age) << CONTROL_AGE_SHIFT)
                    | (u64::from(generation) << CONTROL_GENERATION_SHIFT);
                let _ = self.set_field(header, HEADER_CONTROL, control);
            }
        }
    }

    /// 复位 nursery：block 留在 arena 内复用，位图与 line 表全部清空。
    fn reset_nursery(&mut self) {
        for arena_index in 0..self.arenas.len() {
            if self.arenas[arena_index].kind != HeapArenaKind::Nursery {
                continue;
            }
            let arena = &mut self.arenas[arena_index];
            arena.object_start.fill(0);
            arena.line_live.fill(LINE_FREE);
            arena.block_live.fill(0);
            for slot in arena.blocks.iter_mut() {
                if let Some(block) = slot.as_mut() {
                    block.free_line = 0;
                    block.mark_epoch = arena.mark_epoch;
                    block.bytes.fill(0);
                }
            }
            arena.objects = 0;
            arena.live_bytes = 0;
        }
        self.tlab.active = false;
        self.nursery_bytes = 0;
    }

    /// 回收未标记对象占用的 line。
    fn sweep(&mut self, report: &mut CycleReport) -> Result<(), HeapError> {
        for arena_index in 0..self.arenas.len() {
            if self.arenas[arena_index].kind == HeapArenaKind::Nursery {
                continue;
            }
            let objects = self.all_objects(arena_index);
            for address in objects {
                let object = self.object_at(address)?;
                let (_, offset) = self.locate(address)?;
                let header_offset = offset - OBJECT_HEADER_BYTES;
                let block =
                    u32::try_from(header_offset / u64::from(self.block_bytes)).expect("block 下标");
                let granule = self.arenas[arena_index].granule(header_offset, self.granule_bytes);
                if self.arenas[arena_index].is_marked(granule, block) {
                    continue;
                }
                let total = OBJECT_HEADER_BYTES + object.payload_bytes;
                let arena = &mut self.arenas[arena_index];
                let block_bytes = u64::from(self.block_bytes);
                let first_block = header_offset / block_bytes;
                let last_block = (header_offset + total - 1) / block_bytes;
                for covered in first_block..=last_block {
                    let base = covered * block_bytes;
                    let start = header_offset.max(base);
                    let end = (header_offset + total).min(base + block_bytes);
                    arena.free_range(
                        u32::try_from(covered).expect("block 下标适配 u32"),
                        start - base,
                        end - start,
                    );
                }
                arena.clear_object_start(granule);
                arena.objects = arena.objects.saturating_sub(1);
                arena.live_bytes = arena.live_bytes.saturating_sub(total);
                if object.pinned {
                    arena.pinned = arena.pinned.saturating_sub(1);
                    self.pins.retain(|entry| entry.address != address);
                }
                report.reclaimed_lines += u32::try_from(total.div_ceil(128)).expect("line 数");
                if arena.block_live[block as usize] == 0 {
                    report.reclaimed_blocks += 1;
                }
            }
        }
        Ok(())
    }
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    if align == 0 {
        return None;
    }
    value
        .checked_add(align - 1)
        .map(|value| value / align * align)
}

fn read_word(bytes: &[u8], offset: u64) -> Result<u64, HeapError> {
    let offset = usize::try_from(offset).map_err(|_| HeapError::invalid("word 偏移超出宿主"))?;
    let end = offset
        .checked_add(8)
        .ok_or_else(|| HeapError::invalid("word 范围溢出"))?;
    let slice: [u8; 8] = bytes
        .get(offset..end)
        .ok_or_else(|| HeapError::invalid("word 越过 block"))?
        .try_into()
        .expect("word 宽度固定");
    Ok(u64::from_le_bytes(slice))
}

fn write_word(bytes: &mut [u8], offset: u64, value: u64) -> Result<(), HeapError> {
    let offset = usize::try_from(offset).map_err(|_| HeapError::invalid("word 偏移超出宿主"))?;
    let end = offset
        .checked_add(8)
        .ok_or_else(|| HeapError::invalid("word 范围溢出"))?;
    let slot: &mut [u8; 8] = bytes
        .get_mut(offset..end)
        .ok_or_else(|| HeapError::invalid("word 越过 block"))?
        .try_into()
        .expect("word 宽度固定");
    slot.copy_from_slice(&value.to_le_bytes());
    Ok(())
}
