//! 等待源、wait-node slab、FIFO 队列与 park。
//!
//! `WaitSourceId` 稠密单调且不复用。node 身份是下标加 generation，表只扩容不移动已公开槽。
//! FIFO 用侵入式 `next`。唤醒只经 `ready_publish`。一个 wait generation 最多成功 ready 一次。

use std::mem::{align_of, size_of};

use super::coroutine::{CoroutineHandle, CoroutineState, CoroutineTable};
use super::scheduler::{ProducerHandle, SchedulerWorld, ready_publish};
use super::slab::RawInvariant;

/// wait-word 的 notified 位；与 `ready_publish` 的 `wait_notified_bit` 一致。
pub(crate) const WAIT_NOTIFIED: u64 = 1;
/// 侵入式链表空哨兵。
pub(crate) const WAIT_LINK_NONE: u64 = u64::MAX;
/// node 已成功 ready。
pub(crate) const WAIT_NODE_READIED: u64 = 1;
/// node 仍登记在 select Building 期，waker 可 CAS winner 但不得 ready。
pub(crate) const WAIT_NODE_BUILDING: u64 = 2;
/// winner：尚未提交。
pub(crate) const WINNER_UNSET: u64 = 0;
/// winner：default 臂。
pub(crate) const WINNER_DEFAULT: u64 = 1;
/// winner 编码里 case 的起点：`2 + case_index`。
pub(crate) const WINNER_CASE_BASE: u64 = 2;
/// `SelectTxn` 相位：正在登记。
pub(crate) const SELECT_PHASE_BUILDING: u64 = 0;
/// `SelectTxn` 相位：已经武装，可被 waker 提交。
pub(crate) const SELECT_PHASE_ARMED: u64 = 1;

/// 单调不复用的等待源身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct WaitSourceId(pub(crate) u64);

impl WaitSourceId {
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }
}

/// 等待源种类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitSourceKind {
    Channel,
    Join,
    Never,
}

/// CoroutineCold.select_scratch 的 32 字节 `SelectTxn` 描述符。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct SelectTxn {
    pub(crate) phase_winner: u64,
    pub(crate) case_count: u64,
    pub(crate) scratch_handle: u64,
    pub(crate) wait_block: u64,
}

impl SelectTxn {
    pub(crate) fn phase(self) -> u64 {
        self.phase_winner >> 32
    }

    pub(crate) fn winner(self) -> u64 {
        self.phase_winner & 0xFFFF_FFFF
    }

    pub(crate) fn set_phase(&mut self, phase: u64) {
        self.phase_winner = (phase << 32) | self.winner();
    }

    pub(crate) fn cas_winner(&mut self, from: u64, to: u64) -> bool {
        if self.winner() != from {
            return false;
        }
        self.phase_winner = (self.phase() << 32) | to;
        true
    }

    pub(crate) fn encode_case(index: u32) -> u64 {
        WINNER_CASE_BASE + u64::from(index)
    }

    pub(crate) fn from_cold(words: [u64; 4]) -> Self {
        Self {
            phase_winner: words[0],
            case_count: words[1],
            scratch_handle: words[2],
            wait_block: words[3],
        }
    }

    pub(crate) fn to_cold(self) -> [u64; 4] {
        [
            self.phase_winner,
            self.case_count,
            self.scratch_handle,
            self.wait_block,
        ]
    }
}

/// processor-local scratch cache 的 0 号 class：内联 8 个 word。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
pub(crate) struct SelectScratchCache {
    pub(crate) inline_words: [u64; 8],
}

impl Default for SelectScratchCache {
    fn default() -> Self {
        Self {
            inline_words: [0; 8],
        }
    }
}

/// wait-node：只保存句柄、generation、case 和下标偏移，禁止裸栈指针。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
pub(crate) struct WaitNode {
    pub(crate) coroutine_index: u64,
    pub(crate) coroutine_generation: u64,
    pub(crate) wait_generation: u64,
    pub(crate) case_index: u64,
    pub(crate) payload_offset: u64,
    pub(crate) result_offset: u64,
    pub(crate) next: u64,
    pub(crate) flags: u64,
}

impl Default for WaitNode {
    fn default() -> Self {
        Self {
            coroutine_index: 0,
            coroutine_generation: 0,
            wait_generation: 0,
            case_index: 0,
            payload_offset: 0,
            result_offset: 0,
            next: WAIT_LINK_NONE,
            flags: 0,
        }
    }
}

const _: () = {
    assert!(size_of::<SelectTxn>() == 32);
    assert!(size_of::<SelectScratchCache>() == 64);
    assert!(align_of::<SelectScratchCache>() == 64);
    assert!(size_of::<WaitNode>() == 64);
    assert!(align_of::<WaitNode>() == 64);
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WaitNodeHandle {
    pub(crate) index: u32,
    pub(crate) generation: u64,
}

#[derive(Debug)]
struct WaitSourceRecord {
    kind: WaitSourceKind,
    generation: u64,
    locked: bool,
    queue: WaitQueue,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WaitQueue {
    pub(crate) head: Option<u32>,
    pub(crate) tail: Option<u32>,
}

#[derive(Debug)]
struct WaitNodeRecord {
    node: WaitNode,
    generation: u64,
    occupied: bool,
    source: WaitSourceId,
    readied: bool,
}

/// 等待平面：源表、node slab、Join 源映射与 never 源。
#[derive(Debug)]
pub(crate) struct WaitPlane {
    next_source: u64,
    sources: Vec<WaitSourceRecord>,
    nodes: Vec<WaitNodeRecord>,
    free_node: Option<u32>,
    join_sources: Vec<Option<WaitSourceId>>,
    never: WaitSourceId,
    /// 每个协程最近一次成功 ready 的 wait generation；0 表示尚未 ready。
    readied_wait: Vec<u64>,
    /// 每个协程当前 wait/select 共享的 wait generation。
    wait_generation: Vec<u64>,
    /// 当前登记的 wait-node，供 loser 注销。
    armed_nodes: Vec<Vec<WaitNodeHandle>>,
    /// 会合/提交写入的 payload，对应 result slot。
    delivered: Vec<Option<u64>>,
    /// 测试注入：使 `try_lock` 失败以覆盖 select 回退路径。
    pub(crate) fail_try_lock: bool,
}

impl WaitPlane {
    pub(crate) fn new() -> Result<Self, RawInvariant> {
        let mut plane = Self {
            next_source: 1,
            sources: Vec::new(),
            nodes: Vec::new(),
            free_node: None,
            join_sources: Vec::new(),
            never: WaitSourceId(0),
            readied_wait: Vec::new(),
            wait_generation: Vec::new(),
            armed_nodes: Vec::new(),
            delivered: Vec::new(),
            fail_try_lock: false,
        };
        plane.never = plane.alloc_source(WaitSourceKind::Never)?;
        Ok(plane)
    }

    pub(crate) fn never_source(&self) -> WaitSourceId {
        self.never
    }

    pub(crate) fn alloc_source(
        &mut self,
        kind: WaitSourceKind,
    ) -> Result<WaitSourceId, RawInvariant> {
        let id = self.next_source;
        self.next_source = id
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("WaitSourceId 溢出"))?;
        let index = usize::try_from(id - 1).expect("源下标");
        debug_assert!(self.sources.len() == index);
        self.sources.push(WaitSourceRecord {
            kind,
            generation: 1,
            locked: false,
            queue: WaitQueue::default(),
        });
        Ok(WaitSourceId(id))
    }

    pub(crate) fn bind_join(
        &mut self,
        handle: CoroutineHandle,
    ) -> Result<WaitSourceId, RawInvariant> {
        let index = usize::try_from(handle.index).expect("控制块下标");
        if self.join_sources.len() <= index {
            self.join_sources.resize(index + 1, None);
        }
        let source = self.alloc_source(WaitSourceKind::Join)?;
        self.join_sources[index] = Some(source);
        Ok(source)
    }

    fn ensure_coro(&mut self, index: usize) {
        if self.readied_wait.len() <= index {
            self.readied_wait.resize(index + 1, 0);
            self.wait_generation.resize(index + 1, 0);
            self.armed_nodes.resize_with(index + 1, Vec::new);
            self.delivered.resize(index + 1, None);
        }
    }

    /// 开启一轮 wait/select：共享 generation，并清空上一轮登记。
    pub(crate) fn begin_wait(&mut self, handle: CoroutineHandle) -> Result<u64, RawInvariant> {
        let index = usize::try_from(handle.index).expect("控制块下标");
        self.ensure_coro(index);
        let next = self.wait_generation[index]
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("wait generation 溢出"))?;
        self.wait_generation[index] = next;
        self.armed_nodes[index].clear();
        Ok(next)
    }

    pub(crate) fn arm_nodes(&mut self, handle: CoroutineHandle, nodes: Vec<WaitNodeHandle>) {
        let index = usize::try_from(handle.index).expect("控制块下标");
        self.ensure_coro(index);
        self.armed_nodes[index] = nodes;
    }

    pub(crate) fn take_armed(&mut self, handle: CoroutineHandle) -> Vec<WaitNodeHandle> {
        let index = usize::try_from(handle.index).expect("控制块下标");
        if self.armed_nodes.len() <= index {
            return Vec::new();
        }
        std::mem::take(&mut self.armed_nodes[index])
    }

    pub(crate) fn is_live(&self, handle: WaitNodeHandle) -> bool {
        self.node_record(handle).is_ok()
    }

    pub(crate) fn write_payload(
        &mut self,
        handle: WaitNodeHandle,
        payload: u64,
    ) -> Result<(), RawInvariant> {
        self.node_record_mut(handle)?.node.payload_offset = payload;
        let index = usize::try_from(self.node(handle)?.coroutine_index).expect("控制块下标");
        self.ensure_coro(index);
        self.delivered[index] = Some(payload);
        Ok(())
    }

    pub(crate) fn take_delivered(&mut self, handle: CoroutineHandle) -> Option<u64> {
        let index = usize::try_from(handle.index).expect("控制块下标");
        if self.delivered.len() <= index {
            return None;
        }
        self.delivered[index].take()
    }

    pub(crate) fn source_of(&self, handle: WaitNodeHandle) -> Result<WaitSourceId, RawInvariant> {
        Ok(self.node_record(handle)?.source)
    }

    pub(crate) fn unlink_source(
        &mut self,
        source: WaitSourceId,
        handle: WaitNodeHandle,
    ) -> Result<bool, RawInvariant> {
        let mut queue = self.source_mut(source)?.queue;
        let found = self.unlink(&mut queue, handle)?;
        self.source_mut(source)?.queue = queue;
        Ok(found)
    }

    pub(crate) fn join_source(
        &self,
        handle: CoroutineHandle,
    ) -> Result<WaitSourceId, RawInvariant> {
        let index = usize::try_from(handle.index).expect("控制块下标");
        self.join_sources
            .get(index)
            .copied()
            .flatten()
            .ok_or_else(|| RawInvariant::new("Join 没有等待源"))
    }

    fn source_index(id: WaitSourceId) -> Result<usize, RawInvariant> {
        id.0.checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| RawInvariant::new("WaitSourceId 无效"))
    }

    fn source_mut(&mut self, id: WaitSourceId) -> Result<&mut WaitSourceRecord, RawInvariant> {
        let index = Self::source_index(id)?;
        self.sources
            .get_mut(index)
            .ok_or_else(|| RawInvariant::new("WaitSourceId 越界"))
    }

    pub(crate) fn kind_of(&self, id: WaitSourceId) -> Result<WaitSourceKind, RawInvariant> {
        let index = Self::source_index(id)?;
        self.sources
            .get(index)
            .map(|source| source.kind)
            .ok_or_else(|| RawInvariant::new("WaitSourceId 越界"))
    }

    pub(crate) fn try_lock(&mut self, id: WaitSourceId) -> Result<bool, RawInvariant> {
        if self.fail_try_lock {
            return Ok(false);
        }
        let source = self.source_mut(id)?;
        if source.locked {
            return Ok(false);
        }
        source.locked = true;
        Ok(true)
    }

    pub(crate) fn lock(&mut self, id: WaitSourceId) -> Result<(), RawInvariant> {
        let source = self.source_mut(id)?;
        if source.locked {
            return Err(RawInvariant::new("等待源重复加锁"));
        }
        source.locked = true;
        Ok(())
    }

    pub(crate) fn unlock(&mut self, id: WaitSourceId) -> Result<(), RawInvariant> {
        let source = self.source_mut(id)?;
        if !source.locked {
            return Err(RawInvariant::new("等待源未持锁"));
        }
        source.locked = false;
        Ok(())
    }

    pub(crate) fn alloc_node(
        &mut self,
        coroutine: CoroutineHandle,
        source: WaitSourceId,
        case_index: u32,
        payload_offset: u64,
        result_offset: u64,
        flags: u64,
        wait_generation: u64,
    ) -> Result<WaitNodeHandle, RawInvariant> {
        let index = if let Some(index) = self.free_node {
            let record = &mut self.nodes[usize::try_from(index).expect("node 下标")];
            self.free_node = if record.node.next == WAIT_LINK_NONE {
                None
            } else {
                Some(u32::try_from(record.node.next).expect("node 下标"))
            };
            index
        } else {
            let index = u32::try_from(self.nodes.len())
                .map_err(|_| RawInvariant::new("wait-node 表溢出"))?;
            self.nodes.push(WaitNodeRecord {
                node: WaitNode::default(),
                generation: 0,
                occupied: false,
                source: WaitSourceId(0),
                readied: false,
            });
            index
        };
        let record = &mut self.nodes[usize::try_from(index).expect("node 下标")];
        record.generation = record
            .generation
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("wait-node generation 溢出"))?;
        record.occupied = true;
        record.source = source;
        record.readied = false;
        record.node = WaitNode {
            coroutine_index: u64::from(coroutine.index),
            coroutine_generation: coroutine.generation,
            wait_generation,
            case_index: u64::from(case_index),
            payload_offset,
            result_offset,
            next: WAIT_LINK_NONE,
            flags,
        };
        Ok(WaitNodeHandle {
            index,
            generation: record.generation,
        })
    }

    pub(crate) fn node(&self, handle: WaitNodeHandle) -> Result<&WaitNode, RawInvariant> {
        Ok(&self.node_record(handle)?.node)
    }

    fn node_record(&self, handle: WaitNodeHandle) -> Result<&WaitNodeRecord, RawInvariant> {
        let record = self
            .nodes
            .get(usize::try_from(handle.index).expect("node 下标"))
            .ok_or_else(|| RawInvariant::new("wait-node 越界"))?;
        if !record.occupied || record.generation != handle.generation {
            return Err(RawInvariant::new("wait-node generation 不匹配"));
        }
        Ok(record)
    }

    fn node_record_mut(
        &mut self,
        handle: WaitNodeHandle,
    ) -> Result<&mut WaitNodeRecord, RawInvariant> {
        let record = self
            .nodes
            .get_mut(usize::try_from(handle.index).expect("node 下标"))
            .ok_or_else(|| RawInvariant::new("wait-node 越界"))?;
        if !record.occupied || record.generation != handle.generation {
            return Err(RawInvariant::new("wait-node generation 不匹配"));
        }
        Ok(record)
    }

    pub(crate) fn enqueue(
        &mut self,
        queue: &mut WaitQueue,
        handle: WaitNodeHandle,
    ) -> Result<(), RawInvariant> {
        {
            let record = self.node_record_mut(handle)?;
            record.node.next = WAIT_LINK_NONE;
        }
        match queue.tail {
            None => {
                queue.head = Some(handle.index);
                queue.tail = Some(handle.index);
            }
            Some(tail) => {
                let tail_record = self
                    .nodes
                    .get_mut(usize::try_from(tail).expect("node 下标"))
                    .ok_or_else(|| RawInvariant::new("FIFO tail 越界"))?;
                tail_record.node.next = u64::from(handle.index);
                queue.tail = Some(handle.index);
            }
        }
        Ok(())
    }

    pub(crate) fn enqueue_source(
        &mut self,
        source: WaitSourceId,
        handle: WaitNodeHandle,
    ) -> Result<(), RawInvariant> {
        let mut queue = self.source_mut(source)?.queue;
        self.enqueue(&mut queue, handle)?;
        self.source_mut(source)?.queue = queue;
        Ok(())
    }

    pub(crate) fn dequeue(
        &mut self,
        queue: &mut WaitQueue,
    ) -> Result<Option<WaitNodeHandle>, RawInvariant> {
        let Some(index) = queue.head else {
            return Ok(None);
        };
        let record = self
            .nodes
            .get(usize::try_from(index).expect("node 下标"))
            .ok_or_else(|| RawInvariant::new("FIFO head 越界"))?;
        if !record.occupied {
            return Err(RawInvariant::new("FIFO 指向空闲 wait-node"));
        }
        let handle = WaitNodeHandle {
            index,
            generation: record.generation,
        };
        let next = record.node.next;
        queue.head = if next == WAIT_LINK_NONE {
            None
        } else {
            Some(u32::try_from(next).expect("node 下标"))
        };
        if queue.head.is_none() {
            queue.tail = None;
        }
        Ok(Some(handle))
    }

    pub(crate) fn take_source_queue(
        &mut self,
        source: WaitSourceId,
    ) -> Result<WaitQueue, RawInvariant> {
        let source = self.source_mut(source)?;
        let queue = source.queue;
        source.queue = WaitQueue::default();
        Ok(queue)
    }

    pub(crate) fn unlink(
        &mut self,
        queue: &mut WaitQueue,
        handle: WaitNodeHandle,
    ) -> Result<bool, RawInvariant> {
        let _ = self.node_record(handle)?;
        let mut prev: Option<u32> = None;
        let mut cursor = queue.head;
        while let Some(index) = cursor {
            let record = self
                .nodes
                .get(usize::try_from(index).expect("node 下标"))
                .ok_or_else(|| RawInvariant::new("FIFO 游标越界"))?;
            let next = record.node.next;
            if index == handle.index && record.generation == handle.generation {
                if let Some(prev) = prev {
                    self.nodes[usize::try_from(prev).expect("node 下标")]
                        .node
                        .next = next;
                } else {
                    queue.head = if next == WAIT_LINK_NONE {
                        None
                    } else {
                        Some(u32::try_from(next).expect("node 下标"))
                    };
                }
                if queue.tail == Some(index) {
                    queue.tail = prev;
                }
                if queue.head.is_none() {
                    queue.tail = None;
                }
                return Ok(true);
            }
            prev = Some(index);
            cursor = if next == WAIT_LINK_NONE {
                None
            } else {
                Some(u32::try_from(next).expect("node 下标"))
            };
        }
        Ok(false)
    }

    pub(crate) fn release_node(&mut self, handle: WaitNodeHandle) -> Result<(), RawInvariant> {
        let next = self
            .free_node
            .map_or(WAIT_LINK_NONE, |index| u64::from(index));
        let record = self.node_record_mut(handle)?;
        record.occupied = false;
        record.node = WaitNode {
            next,
            ..WaitNode::default()
        };
        self.free_node = Some(handle.index);
        Ok(())
    }

    /// 一个 wait generation 最多成功 ready 一次；重复 wake 空操作。
    pub(crate) fn mark_ready(&mut self, handle: WaitNodeHandle) -> Result<bool, RawInvariant> {
        let (coro_index, wait_gen, flags) = {
            let record = self.node_record_mut(handle)?;
            (
                usize::try_from(record.node.coroutine_index).expect("控制块下标"),
                record.node.wait_generation,
                record.node.flags,
            )
        };
        self.ensure_coro(coro_index);
        if self.readied_wait[coro_index] == wait_gen && wait_gen != 0 {
            return Ok(false);
        }
        if flags & WAIT_NODE_BUILDING != 0 {
            return Ok(false);
        }
        let record = self.node_record_mut(handle)?;
        if record.readied || record.node.flags & WAIT_NODE_READIED != 0 {
            return Ok(false);
        }
        record.readied = true;
        record.node.flags |= WAIT_NODE_READIED;
        self.readied_wait[coro_index] = wait_gen;
        Ok(true)
    }

    pub(crate) fn coroutine_of(
        &self,
        handle: WaitNodeHandle,
    ) -> Result<CoroutineHandle, RawInvariant> {
        let node = self.node(handle)?;
        Ok(CoroutineHandle {
            index: u32::try_from(node.coroutine_index).expect("控制块下标"),
            generation: node.coroutine_generation,
        })
    }

    pub(crate) fn arm_building(
        &mut self,
        handle: WaitNodeHandle,
        building: bool,
    ) -> Result<(), RawInvariant> {
        let record = self.node_record_mut(handle)?;
        if building {
            record.node.flags |= WAIT_NODE_BUILDING;
        } else {
            record.node.flags &= !WAIT_NODE_BUILDING;
        }
        Ok(())
    }
}

/// `Running → Parking → Waiting`；只挂起协程。
pub(crate) fn park_wait(
    tables: &mut CoroutineTable,
    handle: CoroutineHandle,
) -> Result<(), RawInvariant> {
    let (slot, _) = tables.get(handle)?;
    slot.hot
        .transition(CoroutineState::Running, CoroutineState::Parking)?;
    let notified = slot
        .hot
        .wait_word
        .load(std::sync::atomic::Ordering::Acquire)
        & WAIT_NOTIFIED
        != 0;
    if notified {
        slot.hot
            .wait_word
            .store(0, std::sync::atomic::Ordering::Release);
        slot.hot
            .transition(CoroutineState::Parking, CoroutineState::Running)?;
        return Ok(());
    }
    slot.hot
        .transition(CoroutineState::Parking, CoroutineState::Waiting)
}

/// 唤醒只走 `ready_publish`。
pub(crate) fn wake_wait(
    scheduler: &mut SchedulerWorld,
    tables: &mut CoroutineTable,
    producer: &mut ProducerHandle,
    processor: u64,
    handle: CoroutineHandle,
    owner_held: bool,
) -> Result<bool, RawInvariant> {
    ready_publish(
        scheduler,
        tables,
        processor,
        handle,
        producer,
        owner_held,
        WAIT_NOTIFIED,
    )
}

pub(crate) fn encode_winner_case(index: u32) -> u64 {
    SelectTxn::encode_case(index)
}

pub(crate) const fn winner_unset() -> u64 {
    WINNER_UNSET
}

pub(crate) const fn winner_default() -> u64 {
    WINNER_DEFAULT
}

pub(crate) const fn phase_building() -> u64 {
    SELECT_PHASE_BUILDING
}

pub(crate) const fn phase_armed() -> u64 {
    SELECT_PHASE_ARMED
}
