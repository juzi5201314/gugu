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

use super::edge_schema::EDGE_NO_JOB;
use super::gc_metadata_section::GcRuntimeMetadata;
use super::gc_trace::walk_descriptor;
use super::local_heap_schema::{
    HEAP_BLOCK_EVAC_SOURCE, HEAP_BLOCK_LARGE_INDEX_MASK, HEAP_BLOCK_LARGE_INDEX_SHIFT,
    HEAP_BLOCK_LARGE_MEMBER, HEAP_BLOCK_RETURN_QUEUED, HeapBlockRecord, HeapBlockState,
    HeapTriggerProfile, LocalHeapRuntimeContract,
};
use super::slab::RawInvariant;

/// 全局稠密 block 身份；arena descriptor 在世界生命周期内不复用。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct ManagedBlockId(pub(crate) u32);

impl ManagedBlockId {
    pub(crate) fn new(descriptor: u32, block: u32) -> Result<Self, RawInvariant> {
        if block >= 64 {
            return Err(RawInvariant::new("arena 的 block 下标必须小于 64"));
        }
        descriptor
            .checked_mul(64)
            .and_then(|base| base.checked_add(block))
            .map(Self)
            .ok_or_else(|| RawInvariant::new("全局 block 身份溢出"))
    }

    pub(crate) const fn arena(self) -> u32 {
        self.0 / 64
    }

    pub(crate) const fn index(self) -> u32 {
        self.0 % 64
    }

    /// 返回身份的原始编码（`descriptor * 64 + block`）。
    ///
    /// 需要把块身份放进定长消息字段时用它：只取 `index()` 会丢掉 arena 部分，让接收端无法判断
    /// 这是哪个 arena 的 block。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }
}

/// 消息与候选游标只保存稳定身份，不保存 payload 地址。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct BlockRef {
    pub(crate) id: ManagedBlockId,
    pub(crate) generation: u32,
}

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
/// `LOCAL_DIRECT` 表示：字段是完整地址，关闭态与 large/pinned 分配的默认值。
const REPRESENTATION_LOCAL_DIRECT: u64 = 0;
/// `TURN_REGION` 表示：region 对象的字段表示由 region descriptor 解释，本阶段不压缩。
const REPRESENTATION_TURN_REGION: u64 = 1;
/// `SHARED_HANDLE` 表示；SharedHeap 参照模型只有 `SharedPayloadRecord`，没有 control header，
/// 因此本常量暂无落地点。
const REPRESENTATION_SHARED_HANDLE: u64 = 2;
/// `COMPRESSED_REF` 表示：字段按 `cage id | generation | offset` 压缩字解释。
///
/// 只写给 capture 槽确为压缩字的对象（压缩闭包环境）；header 与字段表示必须同源。
const REPRESENTATION_COMPRESSED_REF: u64 = 3;

const HEADER_CONTROL: u64 = 0;
const HEADER_SIZE_WORD: u64 = 8;

/// 对象 header 字节数；与契约记录一致。
pub(crate) const OBJECT_HEADER_BYTES: u64 = 16;

const LINE_FREE: u8 = 0;
const LINE_OCCUPIED: u8 = 1;
/// 已发布 `HeapLineRun`、等待 consume 后才能再被 bump 占用。
const LINE_QUEUED: u8 = 2;

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
    /// control bits 43..44 的 managed representation（0..3）。
    pub representation: u8,
}

/// 一个 Immix block；payload 只为已提交的 block 分配。
#[derive(Debug)]
struct HeapBlock {
    record: super::local_heap_schema::HeapBlockRecord,
    /// arena 内字节偏移。
    offset: u64,
    /// 下一个候选 line；等于 `lines_per_block` 表示 block 已满。
    free_line: u32,
    /// mark 位图对应的 cycle epoch。
    mark_epoch: u64,
    /// 该 block 对应的 managed extent；未绑定时为 `None`。
    extent: Option<super::extent::ExtentId>,
    /// block 的 payload 字节；长度恒为 block_bytes。
    bytes: Vec<u8>,
}

/// 一个 arena 的 side metadata 与 block 集合。
#[derive(Debug)]
pub(crate) struct HeapArena {
    kind: HeapArenaKind,
    descriptor: u32,
    manager_owner: u64,
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
    /// 整区已空、已从分配扫描与 card 键中摘除；descriptor 在世界生命周期内不再复用。
    retired: bool,
}

impl HeapArena {
    fn new(
        kind: HeapArenaKind,
        descriptor: u32,
        base: u64,
        contract: &LocalHeapRuntimeContract,
    ) -> Self {
        let blocks = usize::try_from(contract.blocks_per_arena).expect("block 数适配宿主");
        debug_assert_eq!(blocks, 64, "block id 编码与 2 MiB / 32 KiB 契约同源");
        let words =
            usize::try_from(contract.object_start_bits / 64).expect("bitmap word 数适配宿主");
        let lines = usize::try_from(contract.blocks_per_arena * contract.lines_per_block)
            .expect("line 数适配宿主");
        Self {
            kind,
            descriptor,
            manager_owner: 0,
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
            retired: false,
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

    /// 判断一个已提交 block 当前是否可作为分配目标。
    ///
    /// 只有 `Allocating` 与 `Free` 允许写入：`Candidate`/`Sweeping`/`Evacuating` 已被候选平面
    /// 持有，`ReturnPending`/`OwnedFree` 已经交给 owner inbox，写入会破坏归还门禁与
    /// exactly-once。
    fn allocatable(&self, index: u32) -> bool {
        self.block(index).is_some_and(|block| {
            matches!(
                HeapBlockState::from_raw(block.record.state),
                Some(HeapBlockState::Allocating | HeapBlockState::Free)
            ) && (block.free_line as usize) < self.lines_per_block()
        })
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
                record: super::local_heap_schema::HeapBlockRecord {
                    block_id: ManagedBlockId::new(self.descriptor, index)
                        .map_err(|error| HeapError::invalid(error.message()))?
                        .0,
                    generation: 1,
                    arena_descriptor: self.descriptor,
                    block_index: index,
                    manager_owner: self.manager_owner,
                    candidate_job: u32::MAX,
                    state: HeapBlockState::Free.raw(),
                    ..Default::default()
                },
                offset: u64::from(index) * u64::from(block_bytes),
                free_line: 0,
                mark_epoch: 0,
                extent: None,
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
    ///
    /// mark 位图按「一个 granule 一位」组织，因此 epoch 切换时只能清掉**本 block 自己**的
    /// granule 区间：起点由该 block 的 granule 下标推导（`block * per_block / 64`），而不是
    /// 从 arena 起点累加字节。按字节累加会让 block ≥ 1 清掉 block 0 的位并保留自己的陈旧位。
    fn mark_granule(&mut self, granule: usize, block: u32, granule_bytes: u32) -> bool {
        let epoch = self.mark_epoch;
        let granule_bytes = usize::try_from(granule_bytes).expect("granule 字节数适配宿主");
        let per_block = (self.lines_per_block() * 128) / granule_bytes;
        let words = per_block / 64;
        let block_index = usize::try_from(block).expect("block 下标适配宿主");
        let start = block_index * per_block / 64;
        let slot = self
            .blocks
            .get_mut(block_index)
            .and_then(Option::as_mut)
            .expect("标记必然发生在已提交 block");
        if slot.mark_epoch != epoch {
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
        (0..self.blocks.len() as u32).find(|index| self.allocatable(*index))
    }

    /// 返回从 `start` 起的 `span` 个连续空 block。
    ///
    /// 空（`block_live == 0`）还不够：块必须处于可分配状态。`ReturnPending`/`OwnedFree` 的块
    /// 已交给归还路径，`Candidate`/`Sweeping`/`Evacuating` 的块被候选平面持有，把它们的空
    /// 区间交给 large 分配会绕过归还门禁。
    fn free_span(&self, span: u32) -> Option<u32> {
        if span == 0 || span > self.blocks.len() as u32 {
            return None;
        }
        (0..=self.blocks.len() as u32 - span).find(|start| {
            (0..span).all(|step| {
                let index = start + step;
                self.mapped(index)
                    && self.block_live[index as usize] == 0
                    && matches!(
                        HeapBlockState::from_raw(
                            self.block(index).expect("已映射块必然可读").record.state
                        ),
                        Some(HeapBlockState::Allocating | HeapBlockState::Free)
                    )
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
    pub arena: usize,
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
    scanned_words: u64,
    evacuated_objects: u64,
    /// 本 cycle 搬迁过的 `(旧 block, 新 block)` 对；由世界在 cycle 边界取走用于重建 block 对计数。
    relocations: Vec<(ManagedBlockId, ManagedBlockId)>,
    tlab_refills: u64,
    /// 扫描对象的 pointer word 暂存区；复用避免每次扫描分配。
    scratch: Vec<(u64, u64)>,
    /// 本轮分配把 `Free` 块推进到 `Allocating` 的物理字节，由世界侧扣 cache。
    activated_cache_bytes: u64,
    /// 本次分配激活的、**没有**已提交页的块；世界侧必须在分配后重新提交 32 KiB extent。
    activated_blocks: Vec<ManagedBlockId>,
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
            scanned_words: 0,
            evacuated_objects: 0,
            relocations: Vec::new(),
            tlab_refills: 0,
            scratch: Vec::with_capacity(16),
            activated_cache_bytes: 0,
            activated_blocks: Vec::new(),
        }
    }

    pub(crate) fn trigger(&self) -> HeapTriggerProfile {
        self.trigger
    }

    pub(crate) fn nursery_bytes(&self) -> u64 {
        self.nursery_bytes
    }

    /// 返回一个 arena 的 block 数；沿用第一个未摘除 arena 的容量。
    pub(crate) fn blocks_per_arena(&self) -> u32 {
        self.arenas
            .iter()
            .find(|arena| !arena.retired)
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

    /// 按登记顺序选取已有容量；nursery 的活动 TLAB 必须仍属于所选 arena。
    pub(crate) fn allocatable_arena(&self, kind: HeapArenaKind, span: u32) -> Option<usize> {
        if kind == HeapArenaKind::Nursery && self.tlab.active {
            let arena = &self.arenas[self.tlab.arena];
            if !arena.retired
                && (self.tlab.start_block..self.tlab.end_block)
                    .any(|index| arena.allocatable(index))
            {
                return Some(self.tlab.arena);
            }
        }
        self.arenas_of(kind)
            .find(|index| self.has_allocatable(*index, span))
    }

    pub(crate) fn uncommitted_arena(&self, kind: HeapArenaKind) -> Option<usize> {
        self.arenas_of(kind)
            .find(|index| self.arenas[*index].committed < 64)
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
            .filter(move |(_, arena)| arena.kind == kind && !arena.retired)
            .map(|(index, _)| index)
    }

    /// 登记一个新 arena；返回下标。
    pub(crate) fn attach_arena(
        &mut self,
        kind: HeapArenaKind,
        descriptor: u32,
        base: u64,
        contract: &LocalHeapRuntimeContract,
    ) -> usize {
        self.arenas
            .push(HeapArena::new(kind, descriptor, base, contract));
        self.arenas.len() - 1
    }

    pub(crate) fn set_arena_manager(&mut self, index: usize, owner: u64) {
        let arena = &mut self.arenas[index];
        arena.manager_owner = owner;
        for block in arena.blocks.iter_mut().flatten() {
            block.record.manager_owner = owner;
        }
    }

    /// 把一个已清空、已结清 extents 的 arena 从分配扫描中摘除。
    ///
    /// 块槽位与位图整体丢弃，descriptor 在 world 生命周期内不再复用（世界侧的 `managed_arenas`
    /// 登记同步移除），因此后续分配一定走新登记的新 arena，不会命中没有页的旧块。
    pub(crate) fn retire_arena(&mut self, descriptor: u32) -> Result<(), HeapError> {
        let index = self.arena_index_by_descriptor(u64::from(descriptor))?;
        if self.tlab.active && self.tlab.arena == index {
            self.tlab.active = false;
        }
        let arena = &mut self.arenas[index];
        arena.retired = true;
        for slot in arena.blocks.iter_mut() {
            *slot = None;
        }
        arena.committed = 0;
        arena.line_live.fill(LINE_FREE);
        arena.block_live.fill(0);
        arena.object_start.fill(0);
        arena.mark.fill(0);
        arena.objects = 0;
        arena.live_bytes = 0;
        Ok(())
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
    ///
    /// `compressed` 只在 cage 开启、placement 允许且非 large 分配时由调用方置真，此时
    /// control 的 representation 写 `COMPRESSED_REF`，其余一律 `LOCAL_DIRECT`。
    pub(crate) fn allocate(
        &mut self,
        arena_index: usize,
        type_index: u32,
        payload_bytes: u64,
        align: u64,
        compressed: bool,
    ) -> Result<u64, HeapError> {
        let kind = self
            .arenas
            .get(arena_index)
            .ok_or_else(|| HeapError::invalid("分配引用未知 arena"))?
            .kind;
        let total = OBJECT_HEADER_BYTES
            .checked_add(payload_bytes)
            .ok_or_else(|| HeapError::invalid("对象 footprint 溢出"))?;
        let (arena_index, block, offset) = if kind == HeapArenaKind::Large {
            self.allocate_large(arena_index, total)?
        } else {
            self.allocate_small(arena_index, total, align)?
        };
        let descriptor = self.arenas[arena_index].descriptor;
        let arena = &mut self.arenas[arena_index];
        let generation = if kind == HeapArenaKind::Old {
            GENERATION_OLD
        } else {
            kind.generation()
        };
        let control = u64::from(type_index)
            | (u64::from(generation) << CONTROL_GENERATION_SHIFT)
            | (if compressed {
                REPRESENTATION_COMPRESSED_REF
            } else {
                REPRESENTATION_LOCAL_DIRECT
            } << CONTROL_REPRESENTATION_SHIFT)
            | if kind == HeapArenaKind::Large {
                CONTROL_LARGE_OBJECT
            } else {
                0
            };
        let block_ref = arena
            .block_mut(block)
            .ok_or_else(|| HeapError::invalid("block 缺失"))?;
        if HeapBlockState::from_raw(block_ref.record.state) == Some(HeapBlockState::Free) {
            if block_ref.extent.is_some() {
                // 仍有已提交页：只把激活字节记进 cache 结算。
                self.activated_cache_bytes = self
                    .activated_cache_bytes
                    .saturating_add(u64::from(self.block_bytes));
            } else {
                // 已归还并 trim 过的块没有物理页：世界侧必须重新提交 extent 再写入。
                self.activated_blocks.push(
                    ManagedBlockId::new(descriptor, block)
                        .map_err(|error| HeapError::invalid(error.message()))?,
                );
            }
        }
        block_ref.record.state = HeapBlockState::Allocating.raw();
        block_ref.record.mutation_version = block_ref
            .record
            .mutation_version
            .checked_add(1)
            .ok_or_else(|| HeapError::invalid("block mutation version 溢出"))?;
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
        arena_index: usize,
        total: u64,
        align: u64,
    ) -> Result<(usize, u32, u64), HeapError> {
        let kind = self.arenas[arena_index].kind;
        if kind == HeapArenaKind::Nursery && (!self.tlab.active || self.tlab.arena != arena_index) {
            self.refill_tlab(arena_index)?;
        }
        let range = if kind == HeapArenaKind::Nursery {
            self.tlab.start_block..self.tlab.end_block
        } else {
            0..self.arenas[arena_index].blocks.len() as u32
        };
        for block in range {
            let arena = &mut self.arenas[arena_index];
            // 只有可分配状态的块接受写入：候选/归还中的块即使还有空闲 line 也必须跳过。
            if !arena.allocatable(block) {
                continue;
            }
            let slot = arena
                .block_mut(block)
                .expect("allocatable 覆盖已提交 block");
            slot.record.allocator_leases += 1;
            let result = arena.allocate_in_block(block, total, align, self.granule_bytes);
            arena
                .block_mut(block)
                .expect("分配期间 block 已提交")
                .record
                .allocator_leases -= 1;
            match result {
                Ok(offset) => return Ok((arena_index, block, offset)),
                Err(HeapError::NoCapacity) => {}
                Err(error) => return Err(error),
            }
        }
        if kind == HeapArenaKind::Nursery {
            self.release_tlab()?;
        }
        Err(HeapError::NoCapacity)
    }

    /// 大对象：占用若干连续空 block。
    fn allocate_large(
        &mut self,
        arena_index: usize,
        total: u64,
    ) -> Result<(usize, u32, u64), HeapError> {
        let descriptor = self.arenas[arena_index].descriptor;
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
            // span 占满整块：直接把 block 的 line 计数写成满值。`release_block` 已把计数复位
            // 为 0，因此这里必须赋值而不是累加；`reclaim_object` 释放大对象时按覆盖 line 逐条
            // 递减，最终回到 0，候选 seed 与 span 判空都依赖这个计数。
            arena.block_live[usize::try_from(start + step).expect("block 下标适配宿主")] = lines;
            let block = arena
                .block_mut(start + step)
                .ok_or_else(|| HeapError::invalid("block 缺失"))?;
            if HeapBlockState::from_raw(block.record.state) == Some(HeapBlockState::Free) {
                if block.extent.is_some() {
                    self.activated_cache_bytes = self
                        .activated_cache_bytes
                        .saturating_add(u64::from(self.block_bytes));
                } else {
                    self.activated_blocks.push(
                        ManagedBlockId::new(descriptor, start + step)
                            .map_err(|error| HeapError::invalid(error.message()))?,
                    );
                }
            }
            block.free_line = u32::MAX;
            block.record.state = HeapBlockState::Allocating.raw();
            if step == 0 {
                block.record.reserved =
                    (span << HEAP_BLOCK_LARGE_INDEX_SHIFT) & HEAP_BLOCK_LARGE_INDEX_MASK;
            } else {
                block.record.reserved = HEAP_BLOCK_LARGE_MEMBER
                    | ((start << HEAP_BLOCK_LARGE_INDEX_SHIFT) & HEAP_BLOCK_LARGE_INDEX_MASK);
            }
        }
        Ok((
            arena_index,
            start,
            u64::from(start) * u64::from(self.block_bytes),
        ))
    }

    /// 取一段新的 TLAB span；没有连续空 block 时报容量不足。
    fn refill_tlab(&mut self, arena_index: usize) -> Result<(), HeapError> {
        self.release_tlab()?;
        let span = self.tlab_span_blocks;
        let arena = &self.arenas[arena_index];
        let start = arena.free_span(span).ok_or(HeapError::NoCapacity)?;
        for index in start..start + span {
            self.arenas[arena_index]
                .block_mut(index)
                .expect("TLAB 覆盖已提交 block")
                .record
                .allocator_leases += 1;
        }
        self.tlab = Tlab {
            active: true,
            arena: arena_index,
            start_block: start,
            end_block: start + span,
        };
        self.tlab_refills += 1;
        Ok(())
    }

    fn release_tlab(&mut self) -> Result<(), HeapError> {
        if self.tlab.active {
            for index in self.tlab.start_block..self.tlab.end_block {
                let record = &mut self.arenas[self.tlab.arena]
                    .block_mut(index)
                    .ok_or_else(|| HeapError::invalid("TLAB block 缺失"))?
                    .record;
                record.allocator_leases = record
                    .allocator_leases
                    .checked_sub(1)
                    .ok_or_else(|| HeapError::invalid("TLAB allocator lease 下溢"))?;
            }
            self.tlab.active = false;
        }
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
            representation: ((control >> CONTROL_REPRESENTATION_SHIFT) & 0x3) as u8,
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

    /// 返回 payload 内字段在 **arena 内**的字节偏移。
    ///
    /// card 表按 arena 内的 512 byte 粒度索引，因此 barrier 的 card 键必须用这个偏移，
    /// 而不是调用者传入的 payload 字段偏移。
    pub(crate) fn field_offset(&self, address: u64, offset: u64) -> Result<u64, HeapError> {
        let (_, payload_offset) = self.locate(address)?;
        payload_offset
            .checked_add(offset)
            .ok_or_else(|| HeapError::invalid("字段的 arena 内偏移溢出"))
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
    ///
    /// 已摘除的 arena 不再产生 card 键：它的块槽位与位图整体丢弃，descriptor 也不会复用。
    pub(crate) fn arena_descriptors(&self) -> Vec<(usize, u64)> {
        self.arenas
            .iter()
            .enumerate()
            .filter(|(_, arena)| !arena.retired)
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
        Ok((index, u64::from(self.arenas[index].descriptor)))
    }

    /// 返回地址所在 block 的 arena 内下标；按对象 header 归属计算。
    pub(crate) fn block_of(&self, address: u64) -> Result<u32, HeapError> {
        let (_, offset) = self.locate(address)?;
        let header = offset.saturating_sub(OBJECT_HEADER_BYTES);
        Ok(u32::try_from(header / u64::from(self.block_bytes)).expect("block 下标适配 u32"))
    }

    /// 解析一个 payload 地址所属的稳定 block 身份。
    pub(crate) fn block_ref(&self, address: u64) -> Result<BlockRef, HeapError> {
        let payload = self.resolve(address)?;
        let (arena, offset) = self.locate(payload)?;
        let index = u32::try_from((offset - OBJECT_HEADER_BYTES) / u64::from(self.block_bytes))
            .map_err(|_| HeapError::invalid("block 下标超出 u32"))?;
        let record = &self.arenas[arena]
            .block(index)
            .ok_or_else(|| HeapError::invalid("对象 block 未提交"))?
            .record;
        Ok(BlockRef {
            id: ManagedBlockId(record.block_id),
            generation: record.generation,
        })
    }

    /// 返回一个 block 的当前 generation；该身份不属于本 heap 时报不变量失败。
    ///
    /// `ManagedBlockId` 的 arena 部分就是 arena descriptor，因此必须按 descriptor 定位 arena：
    /// 只按 arena 内下标扫描会让 arena ≥ 1 的 block 读到 arena 0 同号 block 的世代。
    pub(crate) fn block_generation(&self, id: ManagedBlockId) -> Result<u32, HeapError> {
        let arena = self.arena_by_descriptor(u64::from(id.arena()))?;
        arena
            .block(id.index())
            .map(|block| block.record.generation)
            .ok_or_else(|| HeapError::invalid("block 身份不属于该 LocalHeap"))
    }

    /// 推进一个 block 的 mutation version；候选 job 按它判断快照是否仍然有效。
    pub(crate) fn note_block_mutation(&mut self, id: ManagedBlockId) -> Result<(), RawInvariant> {
        let arena = self
            .arenas
            .iter_mut()
            .find(|arena| u64::from(arena.descriptor) == u64::from(id.arena()))
            .ok_or_else(|| RawInvariant::new("block 身份不属于该 LocalHeap"))?;
        let block = arena
            .block_mut(id.index())
            .ok_or_else(|| RawInvariant::new("block 未提交"))?;
        block.record.mutation_version = block
            .record
            .mutation_version
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("block mutation version 溢出"))?;
        Ok(())
    }

    /// 返回一个已提交 block 的记录快照；候选平面按它判断 lease、状态与版本。
    pub(crate) fn block_record(&self, id: ManagedBlockId) -> Result<HeapBlockRecord, HeapError> {
        let arena = self.arena_by_descriptor(u64::from(id.arena()))?;
        arena
            .block(id.index())
            .map(|block| block.record)
            .ok_or_else(|| HeapError::invalid("block 身份不属于该 LocalHeap"))
    }

    /// 以唯一写入路径更新一个已提交 block 的记录。
    ///
    /// 调用方先 `block_record` 读出，再改自己负责的字段写回：descriptor 与 block 下标必须与
    /// 目标 block 一致，因此写错身份会在这里失败而不是静默改到别的 block。
    pub(crate) fn update_block_record(
        &mut self,
        id: ManagedBlockId,
        record: HeapBlockRecord,
    ) -> Result<(), HeapError> {
        if record.arena_descriptor != id.arena() || record.block_index != id.index() {
            return Err(HeapError::invalid("块记录的 arena 与下标和目标身份不一致"));
        }
        let arena = self
            .arenas
            .iter_mut()
            .find(|arena| u64::from(arena.descriptor) == u64::from(id.arena()))
            .ok_or_else(|| HeapError::invalid("block 身份不属于该 LocalHeap"))?;
        let block = arena
            .block_mut(id.index())
            .ok_or_else(|| HeapError::invalid("block 未提交"))?;
        block.record = record;
        Ok(())
    }

    /// 把一个已提交 block 绑定到它的 managed extent。
    pub(crate) fn attach_block_extent(
        &mut self,
        id: ManagedBlockId,
        extent: super::extent::ExtentId,
    ) -> Result<(), HeapError> {
        let arena = self
            .arenas
            .iter_mut()
            .find(|arena| u64::from(arena.descriptor) == u64::from(id.arena()))
            .ok_or_else(|| HeapError::invalid("block 身份不属于该 LocalHeap"))?;
        let block = arena
            .block_mut(id.index())
            .ok_or_else(|| HeapError::invalid("block 未提交"))?;
        block.extent = Some(extent);
        Ok(())
    }

    /// 返回一个已提交 block 绑定的 managed extent。
    pub(crate) fn block_extent(
        &self,
        id: ManagedBlockId,
    ) -> Result<super::extent::ExtentId, HeapError> {
        let arena = self.arena_by_descriptor(u64::from(id.arena()))?;
        arena
            .block(id.index())
            .and_then(|block| block.extent)
            .ok_or_else(|| HeapError::invalid("block 尚未绑定 managed extent"))
    }

    /// 返回本 heap 全部 arena 的 live 字节之和。
    pub(crate) fn live_bytes(&self) -> u64 {
        self.arenas.iter().map(|arena| arena.live_bytes).sum()
    }

    /// 返回仍占用物理页、尚未进入账本 pending 的块字节。
    ///
    /// Allocating / Candidate / Sweeping / Evacuating 计 live。`ReturnPending` 只有在
    /// `HEAP_BLOCK_RETURN_QUEUED` 置位后才由 pending 账本持有；入队前仍计 live。
    /// OwnedFree / Free 计 cache。
    pub(crate) fn live_managed_block_bytes(&self) -> u64 {
        let mut total = 0_u64;
        for arena in &self.arenas {
            for slot in arena.blocks.iter().flatten() {
                match HeapBlockState::from_raw(slot.record.state) {
                    Some(
                        HeapBlockState::Allocating
                        | HeapBlockState::Candidate
                        | HeapBlockState::Sweeping
                        | HeapBlockState::Evacuating,
                    ) => total = total.saturating_add(u64::from(self.block_bytes)),
                    Some(HeapBlockState::ReturnPending)
                        if slot.record.reserved & HEAP_BLOCK_RETURN_QUEUED == 0 =>
                    {
                        total = total.saturating_add(u64::from(self.block_bytes));
                    }
                    _ => {}
                }
            }
        }
        total
    }

    /// 取出并清零本轮 `Free → Allocating` 激活的 cache 字节。
    pub(crate) fn take_activated_cache_bytes(&mut self) -> u64 {
        let bytes = self.activated_cache_bytes;
        self.activated_cache_bytes = 0;
        bytes
    }

    /// 取走自上次调用以来「没有已提交页」的激活块；缓冲区由调用方用
    /// `return_activated_blocks` 交回，稳态因此不重新申请。
    pub(crate) fn take_activated_blocks(&mut self) -> Vec<ManagedBlockId> {
        std::mem::take(&mut self.activated_blocks)
    }

    /// 交还 `take_activated_blocks` 取走的缓冲区。
    pub(crate) fn return_activated_blocks(&mut self, mut buffer: Vec<ManagedBlockId>) {
        buffer.clear();
        self.activated_blocks = buffer;
    }

    /// 返回一个已提交 block 的分配游标；测试与诊断使用。
    pub(crate) fn block_free_line(&self, id: ManagedBlockId) -> Result<u32, HeapError> {
        let arena = self.arena_by_descriptor(u64::from(id.arena()))?;
        let block = arena
            .block(id.index())
            .ok_or_else(|| HeapError::invalid("block 身份不属于该 LocalHeap"))?;
        Ok(block.free_line)
    }

    /// 返回该 block 是否仍持有已提交的物理页（extent 仍绑定）。
    pub(crate) fn block_has_committed_pages(&self, id: ManagedBlockId) -> Result<bool, HeapError> {
        let arena = self.arena_by_descriptor(u64::from(id.arena()))?;
        Ok(arena
            .block(id.index())
            .is_some_and(|block| block.extent.is_some()))
    }

    /// 解绑块上的 extent：物理页已经交给 trim（或已被 trim），重用时必须重新提交。
    pub(crate) fn detach_block_extent(&mut self, id: ManagedBlockId) -> Result<(), HeapError> {
        let arena = self
            .arenas
            .iter_mut()
            .find(|arena| u64::from(arena.descriptor) == u64::from(id.arena()))
            .ok_or_else(|| HeapError::invalid("block 身份不属于该 LocalHeap"))?;
        let block = arena
            .block_mut(id.index())
            .ok_or_else(|| HeapError::invalid("block 未提交"))?;
        block.extent = None;
        Ok(())
    }

    /// 扫描连续 `LINE_FREE` 区间，长度 ≥ `min_lines` 的 run 记成 `(start_line, count)`。
    pub(crate) fn free_line_runs(
        &self,
        id: ManagedBlockId,
        min_lines: u32,
    ) -> Result<Vec<(u32, u32)>, HeapError> {
        let arena = self.arena_by_descriptor(u64::from(id.arena()))?;
        let lines = arena.lines_per_block() as u32;
        let mut runs = Vec::new();
        let mut line = 0_u32;
        while line < lines {
            if arena.line_live[arena.line_index(id.index(), line)] != LINE_FREE {
                line += 1;
                continue;
            }
            let end = arena.run_end_line(id.index(), line);
            let count = end - line;
            if count >= min_lines {
                runs.push((line, count));
            }
            line = end;
        }
        Ok(runs)
    }

    /// 把已发布的 line-run 标成 `LINE_QUEUED`，阻止 bump 在 consume 前再次占用。
    pub(crate) fn mark_line_run_queued(
        &mut self,
        id: ManagedBlockId,
        start_line: u32,
        count: u32,
    ) -> Result<(), HeapError> {
        let arena = self
            .arenas
            .iter_mut()
            .find(|arena| u64::from(arena.descriptor) == u64::from(id.arena()))
            .ok_or_else(|| HeapError::invalid("block 身份不属于该 LocalHeap"))?;
        for step in 0..count {
            let slot = arena.line_index(id.index(), start_line + step);
            if arena.line_live[slot] != LINE_FREE {
                return Err(HeapError::invalid("line-run 覆盖了非空闲 line"));
            }
            arena.line_live[slot] = LINE_QUEUED;
        }
        Ok(())
    }

    /// consume 后把 `LINE_QUEUED` 还原成 `LINE_FREE`，bump 可以再次占用。
    pub(crate) fn restore_queued_line_run(
        &mut self,
        id: ManagedBlockId,
        start_line: u32,
        count: u32,
    ) -> Result<(), HeapError> {
        let arena = self
            .arenas
            .iter_mut()
            .find(|arena| u64::from(arena.descriptor) == u64::from(id.arena()))
            .ok_or_else(|| HeapError::invalid("block 身份不属于该 LocalHeap"))?;
        for step in 0..count {
            let slot = arena.line_index(id.index(), start_line + step);
            if arena.line_live[slot] != LINE_QUEUED {
                return Err(HeapError::invalid("line-run consume 覆盖了非 queued line"));
            }
            arena.line_live[slot] = LINE_FREE;
        }
        // 归还的 run 现在真的可分配了：bump 游标必须能退回到 run 起点，否则这块空闲 line 在
        // 本 block 后续分配里永远不可见（consume 时游标可能已经被推过整个 run）。
        if let Some(block) = arena.block_mut(id.index()) {
            block.free_line = block.free_line.min(start_line);
        }
        Ok(())
    }

    /// 读取 large-object span：起始块返回 `(start, span)`，成员块返回 `None`。
    pub(crate) fn large_span_of(
        &self,
        id: ManagedBlockId,
    ) -> Result<Option<(u32, u32)>, HeapError> {
        let record = self.block_record(id)?;
        if record.reserved & HEAP_BLOCK_LARGE_MEMBER != 0 {
            return Ok(None);
        }
        let span = (record.reserved & HEAP_BLOCK_LARGE_INDEX_MASK) >> HEAP_BLOCK_LARGE_INDEX_SHIFT;
        if span <= 1 {
            return Ok(None);
        }
        Ok(Some((id.index(), span)))
    }

    /// 返回覆盖该块的 large span 起点与长度；普通块为 `None`。
    pub(crate) fn large_span_covering(
        &self,
        id: ManagedBlockId,
    ) -> Result<Option<(u32, u32)>, HeapError> {
        let record = self.block_record(id)?;
        if record.reserved & HEAP_BLOCK_LARGE_MEMBER != 0 {
            let start =
                (record.reserved & HEAP_BLOCK_LARGE_INDEX_MASK) >> HEAP_BLOCK_LARGE_INDEX_SHIFT;
            let start_id = ManagedBlockId::new(id.arena(), start)
                .map_err(|error| HeapError::invalid(error.message()))?;
            return self.large_span_of(start_id);
        }
        self.large_span_of(id)
    }

    /// 若该块属于 empty large span，把全部成员推进到 `ReturnPending`。
    pub(crate) fn promote_empty_large_span(&mut self, id: ManagedBlockId) -> Result<(), HeapError> {
        let Some((start, span)) = self.large_span_covering(id)? else {
            return Ok(());
        };
        for step in 0..span {
            let member = ManagedBlockId::new(id.arena(), start + step)
                .map_err(|error| HeapError::invalid(error.message()))?;
            if self.block_live_lines(member)? != 0 {
                return Ok(());
            }
        }
        for step in 0..span {
            let member = ManagedBlockId::new(id.arena(), start + step)
                .map_err(|error| HeapError::invalid(error.message()))?;
            let mut record = self.block_record(member)?;
            record.state = HeapBlockState::ReturnPending.raw();
            self.update_block_record(member, record)?;
        }
        Ok(())
    }

    /// 返回已发布、尚未 consume 的 line-run 字节。
    pub(crate) fn queued_line_bytes(&self) -> u64 {
        let mut lines = 0_u64;
        for arena in &self.arenas {
            for slot in &arena.line_live {
                if *slot == LINE_QUEUED {
                    lines += 1;
                }
            }
        }
        lines.saturating_mul(128)
    }

    /// 标记块的归还消息已发布，重复入队会失败。
    pub(crate) fn mark_return_queued(&mut self, id: ManagedBlockId) -> Result<(), HeapError> {
        let mut record = self.block_record(id)?;
        if record.reserved & HEAP_BLOCK_RETURN_QUEUED != 0 {
            return Err(HeapError::invalid("该 block 的归还消息已经发布"));
        }
        record.reserved |= HEAP_BLOCK_RETURN_QUEUED;
        self.update_block_record(id, record)
    }

    /// consume 完成后清掉归还入队位。
    pub(crate) fn clear_return_queued(&mut self, id: ManagedBlockId) -> Result<(), HeapError> {
        let mut record = self.block_record(id)?;
        record.reserved &= !HEAP_BLOCK_RETURN_QUEUED;
        self.update_block_record(id, record)
    }

    /// 统计一个 block 内的 pin 与含 resource 实例的对象数：候选 gate 的直接输入。
    ///
    /// 只看 object-start 位图给出的对象起点，不扫描 payload；pin 与 resource 都是分配时写入
    /// header 的真实状态，因此这里不引入第二份计数。
    pub(crate) fn block_pin_and_resource_counts(
        &self,
        id: ManagedBlockId,
    ) -> Result<(u32, u32), HeapError> {
        let arena = self.arena_by_descriptor(u64::from(id.arena()))?;
        if !arena.mapped(id.index()) {
            return Err(HeapError::invalid("请求统计的 block 尚未提交"));
        }
        let mut pinned = 0_u32;
        let mut resources = 0_u32;
        for (_, offset) in arena.objects_in_block(id.index(), self.granule_bytes) {
            let control = self.field(arena.base + offset, HEADER_CONTROL)?;
            if control & CONTROL_PINNED != 0 {
                pinned = pinned.saturating_add(1);
            }
            if control & CONTROL_HAS_RESOURCE_INSTANCE != 0 {
                resources = resources.saturating_add(1);
            }
        }
        Ok((pinned, resources))
    }
}

impl HeapArena {
    pub(crate) fn kind(&self) -> HeapArenaKind {
        self.kind
    }

    pub(crate) fn descriptor(&self) -> u64 {
        u64::from(self.descriptor)
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

    /// 返回仍有空闲 line、且当前处于可分配状态的已提交 block 数。
    pub(crate) fn free_blocks(&self) -> u32 {
        (0..self.blocks.len() as u32)
            .filter(|index| self.allocatable(*index))
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
    ///
    /// 直接借用类型表中的 descriptor：这里不复制 descriptor，也不在每次扫描时重建 metadata。
    fn collect_words(
        &mut self,
        address: u64,
        type_index: u32,
        types: &GcRuntimeMetadata,
    ) -> Result<u32, HeapError> {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        let words = self.with_payload(address, |payload, payload_base| {
            let scan = walk_descriptor(
                type_index,
                types,
                payload,
                payload_base,
                &mut |word, word_address| {
                    scratch.push((word_address - payload_base, u64::from_le_bytes(*word)));
                    Ok(())
                },
            )
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
                self.evacuate(*value, report)?
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
        self.note_relocation(object.object_start, moved)?;
        let source_id = self.block_ref(object.object_start)?.id;
        let mut source_record = self.block_record(source_id)?;
        if HeapBlockState::from_raw(source_record.state) != Some(HeapBlockState::Candidate) {
            source_record.state = HeapBlockState::Evacuating.raw();
            self.update_block_record(source_id, source_record)?;
        }
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
        let span = u32::try_from(
            (OBJECT_HEADER_BYTES + object.payload_bytes).div_ceil(u64::from(self.block_bytes)),
        )
        .map_err(|_| HeapError::invalid("copy block 数溢出"))?;
        let arena = self
            .allocatable_arena(kind, span)
            .ok_or(HeapError::NoCapacity)?;
        // 搬迁后的副本必须保留源对象的字段表示：压缩环境的 capture 槽仍是压缩字，
        // header representation 不能在搬迁中丢失。
        let compressed = object.representation == REPRESENTATION_COMPRESSED_REF as u8;
        let moved = self.allocate(
            arena,
            object.type_index,
            object.payload_bytes,
            8,
            compressed,
        )?;
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
        self.note_relocation(object.object_start, moved)?;
        report.evacuated += 1;
        self.evacuated_objects += 1;
        Ok(moved)
    }

    /// 记录一次对象搬迁；新旧 block 相同则不记（同块内复制不是 relocation）。
    ///
    /// 这里必须用**不解析转发**的查询：写转发指针之后 `resolve` 会把旧地址解析成新地址，
    /// 于是新旧 block 永远相等，搬迁记录会静默丢失。
    fn note_relocation(&mut self, old_payload: u64, new_payload: u64) -> Result<(), HeapError> {
        let old_block = self.block_identity_at(old_payload)?;
        let new_block = self.block_identity_at(new_payload)?;
        if old_block != new_block {
            self.relocations.push((old_block, new_block));
        }
        Ok(())
    }

    /// 返回一个 payload 地址所在 block 的稳定身份，不跟随转发指针。
    fn block_identity_at(&self, address: u64) -> Result<ManagedBlockId, HeapError> {
        let (arena_index, offset) = self.locate(address)?;
        let header = offset.saturating_sub(OBJECT_HEADER_BYTES);
        let index = u32::try_from(header / u64::from(self.block_bytes))
            .map_err(|_| HeapError::invalid("block 下标超出 u32"))?;
        let block = self.arenas[arena_index]
            .block(index)
            .ok_or_else(|| HeapError::invalid("地址所在 block 未提交"))?;
        Ok(ManagedBlockId(block.record.block_id))
    }

    /// 取走本 cycle 的搬迁记录；世界在 cycle 边界用它重建 block 对计数。
    pub(crate) fn take_relocations(&mut self) -> Vec<(ManagedBlockId, ManagedBlockId)> {
        std::mem::take(&mut self.relocations)
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
                self.collect_words(address, object.type_index, types)?;
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

    /// 推进 mark cycle：全部 arena 的 mark epoch 前进一。
    pub(crate) fn begin_mark_cycle(&mut self) {
        for arena in &mut self.arenas {
            arena.mark_epoch += 1;
        }
    }

    /// 登记一次完成的 world 级 major cycle。
    ///
    /// world 负责 mark pass 与 sweep 的编排，本方法只把「一个 major cycle 已完成」计入
    /// LocalHeap 自己的累计计数，使 `HeapCounters::major_cycles` 仍是真实观测。
    pub(crate) fn note_major_cycle(&mut self) {
        self.major_cycles += 1;
    }

    /// 标记一个对象；首次标记返回对象描述，重复标记返回 `None`。
    ///
    /// 这是 world 级 mark pass 的唯一标记入口：对象可能位于任意 arena（包括其它类别），
    /// 因此按 payload 地址反查 arena/block/granule，再由 arena 的位图做 test-and-mark。
    pub(crate) fn mark_object(&mut self, address: u64) -> Result<Option<HeapObject>, HeapError> {
        let block_bytes = u64::from(self.block_bytes);
        let granule_bytes = self.granule_bytes;
        let payload = self.resolve(address)?;
        let (arena_index, offset) = self.locate(payload)?;
        let header_offset = offset - OBJECT_HEADER_BYTES;
        let block = u32::try_from(header_offset / block_bytes)
            .map_err(|_| HeapError::invalid("block 下标超出 u32"))?;
        let arena = &mut self.arenas[arena_index];
        let granule = arena.granule(header_offset, granule_bytes);
        if !arena.mark_granule(granule, block, granule_bytes) {
            return Ok(None);
        }
        self.object_at(address).map(Some)
    }

    /// 收集一个对象的 pointer word；返回 `(payload 内偏移, 值)` 列表。
    ///
    /// world 级 mark pass 用它把对象图交给 `mark_worklists`；scratch 在返回时被取走，
    /// 因此调用方拿到的是本对象独占的副本，不会与下一次扫描互相覆盖。
    pub(crate) fn trace_pointers(
        &mut self,
        address: u64,
        types: &GcRuntimeMetadata,
    ) -> Result<Vec<(u64, u64)>, HeapError> {
        let object = self.object_at(address)?;
        self.collect_words(address, object.type_index, types)?;
        Ok(std::mem::take(&mut self.scratch))
    }

    /// 返回一个地址对应的 ticket 身份：`(arena descriptor, header 偏移, block)`。
    ///
    /// 跨 owner 的 mark ticket 只携带稳定身份，目标 owner 用 `object_at_ticket` 反查对象。
    pub(crate) fn ticket_identity(&self, address: u64) -> Result<(u64, u32, u32), HeapError> {
        let payload = self.resolve(address)?;
        let (arena_index, offset) = self.locate(payload)?;
        let header_offset = offset - OBJECT_HEADER_BYTES;
        let block = u32::try_from(header_offset / u64::from(self.block_bytes))
            .map_err(|_| HeapError::invalid("block 下标超出 u32"))?;
        let descriptor = self.arenas[arena_index].descriptor;
        let offset = u32::try_from(header_offset)
            .map_err(|_| HeapError::invalid("对象 header 偏移超出 u32"))?;
        Ok((u64::from(descriptor), offset, block))
    }

    /// 按 ticket 身份在本 heap 内反查目标对象。
    ///
    /// descriptor 不属于本 heap、偏移越过容量、或该 granule 没有 object-start 都表示 ticket
    /// 已过期；三者都进入不变量失败而不是返回空对象。
    pub(crate) fn object_at_ticket(
        &self,
        arena_descriptor: u64,
        header_offset: u64,
    ) -> Result<HeapObject, HeapError> {
        let block_bytes = u64::from(self.block_bytes);
        let arena = self.arena_by_descriptor(arena_descriptor)?;
        let capacity = block_bytes * arena.blocks.len() as u64;
        if header_offset >= capacity {
            return Err(HeapError::invalid("mark ticket 的对象偏移越过 arena 容量"));
        }
        let granule = arena.granule(header_offset, self.granule_bytes);
        if !arena.has_object_start(granule) {
            return Err(HeapError::invalid("mark ticket 的目标对象已过期"));
        }
        let address = arena.base + header_offset + OBJECT_HEADER_BYTES;
        self.object_at(address)
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
                report.scanned_words += self.collect_words(address, object.type_index, types)?;
                self.rewrite_words(address, evacuate_nursery, report)?;
            }
        }
        Ok(())
    }

    /// 返回一个 card 覆盖范围内的对象起点。
    pub(crate) fn card_objects(&self, arena_index: usize, card: u32) -> Vec<u64> {
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

    /// 返回本 heap 内某个 arena descriptor 对应的 arena。
    fn arena_by_descriptor(&self, arena_descriptor: u64) -> Result<&HeapArena, HeapError> {
        self.arenas
            .iter()
            .find(|arena| u64::from(arena.descriptor) == arena_descriptor)
            .ok_or_else(|| HeapError::invalid("arena descriptor 不属于该 LocalHeap"))
    }

    /// 返回一个 arena 已提交的 block 下标快照。
    ///
    /// 枚举按 `(descriptor, block)` 推进：两者在 cycle 内稳定，因此候选阶段不需要复制对象清单，
    /// 也能在任意 block 之间暂停。
    pub(crate) fn committed_blocks_of(&self, arena_descriptor: u64) -> Result<Vec<u32>, HeapError> {
        Ok(self
            .arena_by_descriptor(arena_descriptor)?
            .committed_blocks())
    }

    /// 返回一个 block 内的全部对象：`(payload 地址, block 内 header 偏移)`。
    ///
    /// 对象起点来自分配时维护的 object-start 位图，因此这里不扫描 payload，也不依赖对象大小；
    /// header 偏移与 `ticket_identity` 使用同一基准，便于候选阶段核对同一对象。
    pub(crate) fn block_objects(
        &self,
        arena_descriptor: u64,
        block: u32,
    ) -> Result<Vec<(u64, u64)>, HeapError> {
        let arena = self.arena_by_descriptor(arena_descriptor)?;
        if !arena.mapped(block) {
            return Err(HeapError::invalid("请求枚举的 block 尚未提交"));
        }
        let block_base = u64::from(block)
            * u64::try_from(arena.lines_per_block()).expect("每 block 的 line 数适配 u64")
            * 128;
        Ok(arena
            .objects_in_block(block, self.granule_bytes)
            .into_iter()
            .map(|(_, offset)| {
                (
                    arena.base + offset + OBJECT_HEADER_BYTES,
                    offset - block_base,
                )
            })
            .collect())
    }

    /// 查询一个对象是否在当前 mark epoch 被标记。
    ///
    /// mark 位图按 block 维护，`is_marked` 已经处理了 epoch 陈旧位，因此候选阶段可以据此区分
    /// “本周期已经标记”与“上一周期的陈旧标记”。
    pub(crate) fn marked_in_current_epoch(&self, address: u64) -> Result<bool, HeapError> {
        let payload = self.resolve(address)?;
        let (arena_index, offset) = self.locate(payload)?;
        let arena = &self.arenas[arena_index];
        let header = offset - OBJECT_HEADER_BYTES;
        let block = u32::try_from(header / u64::from(self.block_bytes))
            .map_err(|_| HeapError::invalid("block 下标超出 u32"))?;
        let granule = arena.granule(header, self.granule_bytes);
        Ok(arena.is_marked(granule, block))
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

    /// 回收未标记对象占用的 line；world 级 major cycle 的 sweep 阶段。
    pub(crate) fn sweep_unmarked(&mut self, report: &mut CycleReport) -> Result<(), HeapError> {
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
                self.reclaim_object(address, object, arena_index, block, report)?;
            }
        }
        Ok(())
    }

    /// 清扫一个 block 内未标记的对象；候选平面提交后的唯一 sweep 消费者入口。
    ///
    /// 返回被回收的对象数。调用方必须保证该 block 已经处于 `Sweeping`：本函数不检查状态，
    /// 状态门禁由候选平面的 `CommitGroup` 决议给出。
    pub(crate) fn sweep_block(
        &mut self,
        id: ManagedBlockId,
        report: &mut CycleReport,
    ) -> Result<u32, HeapError> {
        let arena_index = self.arena_index_by_descriptor(u64::from(id.arena()))?;
        let objects = self.block_objects(u64::from(id.arena()), id.index())?;
        let mut reclaimed = 0_u32;
        for (payload, _) in objects {
            let object = self.object_at(payload)?;
            let (_, offset) = self.locate(payload)?;
            let header_offset = offset - OBJECT_HEADER_BYTES;
            let granule = self.arenas[arena_index].granule(header_offset, self.granule_bytes);
            if self.arenas[arena_index].is_marked(granule, id.index()) {
                continue;
            }
            self.reclaim_object(payload, object, arena_index, id.index(), report)?;
            reclaimed = reclaimed.saturating_add(1);
        }
        Ok(reclaimed)
    }

    /// 回收一个未标记对象：清 line、清 object-start 位并更新 arena 计数。
    fn reclaim_object(
        &mut self,
        address: u64,
        object: HeapObject,
        arena_index: usize,
        block: u32,
        report: &mut CycleReport,
    ) -> Result<(), HeapError> {
        let (_, offset) = self.locate(address)?;
        let header_offset = offset - OBJECT_HEADER_BYTES;
        let granule = self.arenas[arena_index].granule(header_offset, self.granule_bytes);
        let total = OBJECT_HEADER_BYTES + object.payload_bytes;
        let arena = &mut self.arenas[arena_index];
        let block_bytes = u64::from(self.block_bytes);
        let first_block = header_offset / block_bytes;
        let last_block = (header_offset + total - 1) / block_bytes;
        for covered in first_block..=last_block {
            let index = u32::try_from(covered).expect("block 下标适配 u32");
            if object.large {
                // 大对象独占它的整个 span：`allocate_large` 把 span 内每个块的 line 全部标成
                // 占用，释放必须按整块归还；只按对象实际覆盖的 line 递减会让尾部块里对象未覆盖
                // 的 line 永远占用，`promote_empty_large_span` 的判空随之失败。
                arena.free_range(index, 0, block_bytes);
                continue;
            }
            let base = covered * block_bytes;
            let start = header_offset.max(base);
            let end = (header_offset + total).min(base + block_bytes);
            arena.free_range(index, start - base, end - start);
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
        Ok(())
    }

    /// 把一个 block 从 `OwnedFree` 交回 `Free`：清空对象、line 与 mark 位并推进世代。
    ///
    /// consume 之后、再分配之前才允许调用。物理清空推迟到这一步，因此 `ReturnPending`
    /// 期间对象已经不可解析（sweep 已清），但 generation 仍保持到 consume。
    pub(crate) fn release_block(&mut self, id: ManagedBlockId) -> Result<(), HeapError> {
        let arena_index = self.arena_index_by_descriptor(u64::from(id.arena()))?;
        let block_index = id.index();
        let granule_bytes = self.granule_bytes;
        let lines = self.arenas[arena_index].lines_per_block();
        let block_lines = usize::try_from(block_index)
            .ok()
            .and_then(|index| index.checked_mul(lines))
            .ok_or_else(|| HeapError::invalid("block 的 line 区间溢出"))?;
        let arena = &mut self.arenas[arena_index];
        if !arena.mapped(block_index) {
            return Err(HeapError::invalid("释放的 block 尚未提交"));
        }
        let state = HeapBlockState::from_raw(
            arena
                .block(block_index)
                .ok_or_else(|| HeapError::invalid("释放的 block 未提交"))?
                .record
                .state,
        );
        if state != Some(HeapBlockState::OwnedFree) {
            return Err(HeapError::invalid(
                "release_block 只允许从 OwnedFree 进入 Free",
            ));
        }
        let granule_bytes = usize::try_from(granule_bytes).expect("granule 字节数");
        let per_block = (lines * 128) / granule_bytes;
        // `block_lines` 是 line 下标而不是字节偏移：block 的起始 granule 必须由「line 起点 ×
        // 每 line 字节数」换算，直接用 line 下标除以 granule 字节数会清到别的 block 的
        // object-start 位，把同一 arena 里更早的存活对象抹成不可解析。
        let block_granules = block_lines * 128 / granule_bytes;
        let words = per_block.div_ceil(64);
        let mark_start = block_index as usize * per_block / 64;
        arena.line_live[block_lines..block_lines + lines].fill(LINE_FREE);
        arena.block_live[block_index as usize] = 0;
        for granule in block_granules..block_granules + per_block {
            arena.clear_object_start(granule);
        }
        if mark_start + words <= arena.mark.len() {
            arena.mark[mark_start..mark_start + words].fill(0);
        }
        let epoch = arena.mark_epoch;
        let block = arena
            .block_mut(block_index)
            .ok_or_else(|| HeapError::invalid("释放的 block 未提交"))?;
        block.free_line = 0;
        block.mark_epoch = epoch;
        block.bytes.fill(0);
        let mut record = block.record;
        record.state = HeapBlockState::Free.raw();
        record.generation = record
            .generation
            .checked_add(1)
            .ok_or_else(|| HeapError::invalid("block 世代溢出"))?;
        record.incoming_leases = 0;
        record.allocator_leases = 0;
        record.scanner_leases = 0;
        record.evacuation_leases = 0;
        record.candidate_job = EDGE_NO_JOB;
        // 归还完成意味着该块不再是任何 large span 的成员，也没有未结清的归还消息：span 身份与
        // large 元数据必须随物理清空一起消失，否则复用这块的 span 会把旧 span 的成员语义带进
        // 新分配。`consume_*` 之后还会调用 `clear_return_queued`，这里是幂等清除。
        record.reserved &= !(HEAP_BLOCK_EVAC_SOURCE
            | HEAP_BLOCK_LARGE_MEMBER
            | HEAP_BLOCK_LARGE_INDEX_MASK
            | HEAP_BLOCK_RETURN_QUEUED);
        record.mutation_version = record
            .mutation_version
            .checked_add(1)
            .ok_or_else(|| HeapError::invalid("block mutation version 溢出"))?;
        block.record = record;
        Ok(())
    }

    /// 返回一个 block 当前占用的 line 数；`0` 表示块内没有任何对象。
    pub(crate) fn block_live_lines(&self, id: ManagedBlockId) -> Result<u32, HeapError> {
        let arena = self.arena_by_descriptor(u64::from(id.arena()))?;
        arena
            .block_live
            .get(id.index() as usize)
            .copied()
            .ok_or_else(|| HeapError::invalid("block 下标越过 arena 容量"))
    }

    /// 返回一个 block 所属 arena 的类别；候选平面据此排除 nursery block。
    pub(crate) fn block_arena_kind(&self, id: ManagedBlockId) -> Result<HeapArenaKind, HeapError> {
        self.arena_by_descriptor(u64::from(id.arena()))
            .map(|arena| arena.kind)
    }

    /// 按 descriptor 解析 arena 下标。
    fn arena_index_by_descriptor(&self, arena_descriptor: u64) -> Result<usize, HeapError> {
        self.arenas
            .iter()
            .position(|arena| u64::from(arena.descriptor) == arena_descriptor)
            .ok_or_else(|| HeapError::invalid("arena descriptor 不属于该 LocalHeap"))
    }

    /// 从块记录上取走至多 `count` 个 incoming lease；返回实际取走的数量。
    pub(crate) fn take_incoming_leases(
        &mut self,
        id: ManagedBlockId,
        count: u64,
    ) -> Result<u64, HeapError> {
        let mut record = self.block_record(id)?;
        let taken = record.incoming_leases.min(count);
        record.incoming_leases -= taken;
        self.update_block_record(id, record)?;
        Ok(taken)
    }

    /// 给块记录加上 `count` 个 incoming lease。
    pub(crate) fn add_incoming_leases(
        &mut self,
        id: ManagedBlockId,
        count: u64,
    ) -> Result<(), HeapError> {
        let mut record = self.block_record(id)?;
        record.incoming_leases = record
            .incoming_leases
            .checked_add(count)
            .ok_or_else(|| HeapError::invalid("incoming lease 计数溢出"))?;
        self.update_block_record(id, record)
    }

    /// 把一个 block 标为候选组的成员：状态推进到 `candidate` 并写入 `candidate_job`。
    ///
    /// 候选绑定**不**占用 scanner lease：`validate` 相位要求 scanner/allocator/evacuation
    /// lease 归零才能提交，若绑定自己就占一个 scanner lease，这个 gate 永远不可能通过。候选状态
    /// 本身已经阻止 allocator 继续往该 block 分配，因此绑定用状态表达而不是借 lease 表达。
    ///
    /// 只接受 `Allocating`/`Candidate`/`Evacuating`。来自 evacuation 的块在改写成 `Candidate`
    /// 之前把 `HEAP_BLOCK_EVAC_SOURCE` 记在 `reserved` 最低位，供 `CommitGroup` 识别。
    pub(crate) fn mark_block_candidate(
        &mut self,
        id: ManagedBlockId,
        job: u32,
    ) -> Result<HeapBlockRecord, HeapError> {
        let mut record = self.block_record(id)?;
        let state = HeapBlockState::from_raw(record.state);
        match state {
            Some(HeapBlockState::Allocating | HeapBlockState::Candidate) => {}
            Some(HeapBlockState::Evacuating) => {
                record.reserved |= HEAP_BLOCK_EVAC_SOURCE;
            }
            _ => {
                return Err(HeapError::invalid(
                    "只有 allocating/candidate/evacuating 的 block 能成为候选成员",
                ));
            }
        }
        record.candidate_job = job;
        record.state = HeapBlockState::Candidate.raw();
        self.update_block_record(id, record)?;
        Ok(record)
    }

    /// 解除一个 block 的候选绑定。
    ///
    /// `Candidate` 且没有 evacuation 来源位时退回 `Allocating`；有 `HEAP_BLOCK_EVAC_SOURCE`
    /// 时退回 `Evacuating` 并保留该位，直到 `CommitGroup` 消费。
    pub(crate) fn unmark_block_candidate(
        &mut self,
        id: ManagedBlockId,
    ) -> Result<HeapBlockRecord, HeapError> {
        let mut record = self.block_record(id)?;
        record.candidate_job = EDGE_NO_JOB;
        if HeapBlockState::from_raw(record.state) == Some(HeapBlockState::Candidate) {
            if record.reserved & HEAP_BLOCK_EVAC_SOURCE != 0 {
                record.state = HeapBlockState::Evacuating.raw();
            } else {
                record.state = HeapBlockState::Allocating.raw();
            }
        }
        self.update_block_record(id, record)?;
        Ok(record)
    }

    /// 统计一个 block 内处于当前 mark epoch 的对象数；候选验证的标记 gate 输入。
    pub(crate) fn block_marked_objects(&self, id: ManagedBlockId) -> Result<u32, HeapError> {
        let arena = self.arena_by_descriptor(u64::from(id.arena()))?;
        if !arena.mapped(id.index()) {
            return Err(HeapError::invalid("统计标记的 block 尚未提交"));
        }
        let block_bytes = u64::from(self.block_bytes);
        let block_base = u64::from(id.index()) * block_bytes;
        let mut marked = 0_u32;
        for (_, offset) in arena.objects_in_block(id.index(), self.granule_bytes) {
            let header_offset = offset.max(block_base);
            let granule = arena.granule(header_offset, self.granule_bytes);
            let block = u32::try_from(header_offset / block_bytes)
                .map_err(|_| HeapError::invalid("block 下标超出 u32"))?;
            if arena.is_marked(granule, block) {
                marked = marked.saturating_add(1);
            }
        }
        Ok(marked)
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
