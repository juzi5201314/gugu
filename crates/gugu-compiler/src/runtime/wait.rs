//! 等待源、wait-node slab、FIFO 队列与 park。
//!
//! `WaitSourceId` 稠密单调且不复用。node 身份是下标加 generation，表只扩容不移动已公开槽。
//! FIFO 用侵入式 `next`。唤醒只经 `ready_publish`。一个 wait generation 最多成功 ready 一次。

use std::mem::{align_of, size_of};

use super::coroutine::{CompletionValue, CoroutineHandle, CoroutineState, CoroutineTable};
use super::scheduler::{ProducerHandle, SchedulerWorld, ready_publish};
use super::slab::RawInvariant;
use super::sync::CancelHandle;

/// wait-word 的 notified 位；与 `ready_publish` 的 `wait_notified_bit` 一致。
pub(crate) const WAIT_NOTIFIED: u64 = 1;
/// 侵入式链表空哨兵。
pub(crate) const WAIT_LINK_NONE: u64 = u64::MAX;
/// node 已成功 ready。
pub(crate) const WAIT_NODE_READIED: u64 = 1;
/// node 仍登记在 select Building 期，waker 可 CAS winner 但不得 ready。
pub(crate) const WAIT_NODE_BUILDING: u64 = 2;
/// select 节点才消费 cold 中的相位和 winner；普通等待不共享该描述符。
pub(crate) const WAIT_NODE_SELECT: u64 = 4;
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
        debug_assert!(index <= u32::MAX - 2);
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
pub struct WaitNodeHandle {
    pub index: u32,
    pub generation: u64,
}

/// 一轮等待真正提交的结果，不以 payload 0 表示未完成或关闭。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitResult {
    Sent,
    Recv(u64),
    RecvClosed,
    SendClosed,
    Join(CompletionValue),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JoinOutcome {
    Completed(CompletionValue),
    Parked(WaitNodeHandle),
}

#[derive(Clone, Copy, Debug, Default)]
struct WaitDelivery {
    generation: u64,
    result: Option<WaitResult>,
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
    cancel_source: Option<CancelHandle>,
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
    /// 按稠密 coroutine index 存放提交状态；取走结果不解除本轮的唯一认领。
    delivered: Vec<WaitDelivery>,
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
            self.delivered.resize(index + 1, WaitDelivery::default());
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
        debug_assert!(self.armed_nodes[index].is_empty());
        self.delivered[index] = WaitDelivery::default();
        self.armed_nodes[index].clear();
        Ok(next)
    }

    pub(crate) fn arm_nodes(&mut self, handle: CoroutineHandle, nodes: Vec<WaitNodeHandle>) {
        let index = usize::try_from(handle.index).expect("控制块下标");
        self.ensure_coro(index);
        self.armed_nodes[index] = nodes;
    }

    pub(crate) fn armed_nodes(&self, handle: CoroutineHandle) -> &[WaitNodeHandle] {
        self.armed_nodes
            .get(usize::try_from(handle.index).expect("控制块下标"))
            .map_or(&[], Vec::as_slice)
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

    pub(crate) fn publish_result(&mut self, handle: CoroutineHandle, result: WaitResult) {
        let index = usize::try_from(handle.index).expect("控制块下标");
        debug_assert_eq!(
            self.delivered[index].generation,
            self.wait_generation[index]
        );
        debug_assert!(self.delivered[index].result.is_none());
        self.delivered[index].result = Some(result);
    }

    pub(crate) fn is_completed(&self, handle: CoroutineHandle) -> bool {
        let index = usize::try_from(handle.index).expect("控制块下标");
        self.delivered.get(index).is_some_and(|delivery| {
            delivery.generation == self.wait_generation[index] && delivery.result.is_some()
        })
    }

    pub(crate) fn take_delivered(&mut self, handle: CoroutineHandle) -> Option<WaitResult> {
        let index = usize::try_from(handle.index).expect("控制块下标");
        if self.delivered.len() <= index {
            return None;
        }
        self.delivered[index].result.take()
    }

    fn can_claim(
        &self,
        controls: &CoroutineTable,
        handle: CoroutineHandle,
        case: Option<u32>,
    ) -> bool {
        let Ok((_, cold)) = controls.get(handle) else {
            return false;
        };
        let index = usize::try_from(handle.index).expect("控制块下标");
        let Some(generation) = self.wait_generation.get(index).copied() else {
            return false;
        };
        generation != 0
            && self.delivered[index].generation != generation
            && (case.is_none()
                || SelectTxn::from_cold(cold.select_scratch).winner() == WINNER_UNSET)
    }

    pub(crate) fn node_pending(&self, controls: &CoroutineTable, handle: WaitNodeHandle) -> bool {
        let Ok(node) = self.node(handle) else {
            return false;
        };
        let coroutine = CoroutineHandle {
            index: u32::try_from(node.coroutine_index).expect("节点保存 u32 控制块下标"),
            generation: node.coroutine_generation,
        };
        let case = (node.flags & WAIT_NODE_SELECT != 0)
            .then(|| u32::try_from(node.case_index).expect("节点保存 u32 case 下标"));
        self.wait_generation
            .get(usize::try_from(coroutine.index).expect("控制块下标"))
            == Some(&node.wait_generation)
            && self.can_claim(controls, coroutine, case)
    }

    fn claim(&mut self, controls: &mut CoroutineTable, handle: CoroutineHandle, case: Option<u32>) {
        if let Some(case) = case {
            let (_, cold) = controls
                .get_mut(handle)
                .expect("认领前已验证控制块且独占 controls");
            let mut txn = SelectTxn::from_cold(cold.select_scratch);
            let won = txn.cas_winner(WINNER_UNSET, SelectTxn::encode_case(case));
            debug_assert!(won);
            cold.select_scratch = txn.to_cold();
        }
        let index = usize::try_from(handle.index).expect("控制块下标");
        self.delivered[index].generation = self.wait_generation[index];
    }

    /// 源锁和独占 controls 使双方预检、winner 认领成为一个参照步骤。
    pub(crate) fn try_claim(
        &mut self,
        controls: &mut CoroutineTable,
        source: WaitSourceId,
        current: Option<(CoroutineHandle, u32)>,
        peer: Option<WaitNodeHandle>,
    ) -> Result<bool, RawInvariant> {
        if !self.source_mut(source)?.locked {
            return Err(RawInvariant::new("等待提交要求持有源锁"));
        }
        if let Some((handle, case)) = current
            && !self.can_claim(controls, handle, Some(case))
        {
            return Ok(false);
        }
        let peer = if let Some(peer) = peer {
            if !self.node_pending(controls, peer) {
                return Ok(false);
            }
            if self.source_of(peer)? != source {
                return Err(RawInvariant::new("wait-node 等待源不匹配"));
            }
            let coroutine = self.coroutine_of(peer)?;
            if current.is_some_and(|(handle, _)| handle == coroutine) {
                return Ok(false);
            }
            let node = self.node(peer)?;
            let case = (node.flags & WAIT_NODE_SELECT != 0)
                .then(|| u32::try_from(node.case_index).expect("节点保存 u32 case 下标"));
            Some((coroutine, case))
        } else {
            None
        };
        if let Some((handle, case)) = current {
            self.claim(controls, handle, Some(case));
        }
        if let Some((handle, case)) = peer {
            self.claim(controls, handle, case);
        }
        Ok(true)
    }

    /// FIFO 中跳过已提交节点与本 waitset；只查找，不在认领前移除或消费 payload。
    pub(crate) fn next_pending(
        &self,
        controls: &CoroutineTable,
        queue: WaitQueue,
        exclude: Option<CoroutineHandle>,
    ) -> Result<Option<WaitNodeHandle>, RawInvariant> {
        let mut cursor = queue.head;
        while let Some(index) = cursor {
            let record = self
                .nodes
                .get(usize::try_from(index).expect("node 下标"))
                .filter(|record| record.occupied)
                .ok_or_else(|| RawInvariant::new("FIFO 指向空闲 wait-node"))?;
            let node = WaitNodeHandle {
                index,
                generation: record.generation,
            };
            if self.node_pending(controls, node) && Some(self.coroutine_of(node)?) != exclude {
                return Ok(Some(node));
            }
            cursor = if record.node.next == WAIT_LINK_NONE {
                None
            } else {
                Some(u32::try_from(record.node.next).expect("节点 next 为 u32 下标"))
            };
        }
        Ok(None)
    }

    pub(crate) fn cancel_source(
        &self,
        node: WaitNodeHandle,
    ) -> Result<Option<CancelHandle>, RawInvariant> {
        Ok(self.node_record(node)?.cancel_source)
    }

    pub(crate) fn set_cancel_source(
        &mut self,
        node: WaitNodeHandle,
        cancel: Option<CancelHandle>,
    ) -> Result<(), RawInvariant> {
        self.node_record_mut(node)?.cancel_source = cancel;
        Ok(())
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
                cancel_source: None,
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
        record.cancel_source = None;
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

    pub(crate) fn node_completed(&self, controls: &CoroutineTable, handle: WaitNodeHandle) -> bool {
        let Ok(node) = self.node(handle) else {
            return false;
        };
        let coroutine = CoroutineHandle {
            index: u32::try_from(node.coroutine_index).expect("节点保存 u32 控制块下标"),
            generation: node.coroutine_generation,
        };
        let Ok((_, cold)) = controls.get(coroutine) else {
            return false;
        };
        self.wait_generation
            .get(usize::try_from(coroutine.index).expect("控制块下标"))
            == Some(&node.wait_generation)
            && self.is_completed(coroutine)
            && (node.flags & WAIT_NODE_SELECT == 0
                || SelectTxn::from_cold(cold.select_scratch).winner()
                    == WINNER_CASE_BASE + node.case_index)
    }
}

/// 发布已经进入 Parking 的等待；调用者负责发布前后重查已提交结果。
pub(crate) fn park_wait(
    tables: &mut CoroutineTable,
    handle: CoroutineHandle,
) -> Result<(), RawInvariant> {
    tables
        .get(handle)?
        .0
        .hot
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
