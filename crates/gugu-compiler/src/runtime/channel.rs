//! 有/无缓冲 channel：环槽、会合、close 与 try_* 线性化。
//!
//! 每个 `send`/`recv`/`try_*`/`close` 一个线性化点（取源锁）。close 不丢已线性化的缓冲。
//! 大 payload 两阶段：短临界区只做带 generation 的 reservation，拷贝在锁外，第二段发布；
//! 未发布 payload 对 recv/close 不可见。运行时负容量 panic。

use super::coroutine::CoroutineHandle;
use super::slab::RawInvariant;
use super::wait::{
    WAIT_LINK_NONE, WaitNodeHandle, WaitPlane, WaitQueue, WaitSourceId, WaitSourceKind,
};

/// 超过该字节视为大 payload，走两阶段 reservation。
pub(crate) const LARGE_PAYLOAD_BYTES: u32 = 64;

/// 缓冲槽状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChannelSlotState {
    Empty,
    Reserved { generation: u64 },
    Occupied { payload: u64, generation: u64 },
}

/// channel 控制块；128 字节，走 raw class 128。
#[derive(Clone, Copy, Debug)]
#[repr(C, align(64))]
pub(crate) struct ChannelControl {
    pub(crate) source_id: u64,
    pub(crate) generation: u64,
    pub(crate) capacity: u64,
    pub(crate) closed: u64,
    pub(crate) head: u64,
    pub(crate) tail: u64,
    pub(crate) len: u64,
    pub(crate) send_q_head: u64,
    pub(crate) recv_q_head: u64,
    pub(crate) reservation_generation: u64,
    pub(crate) ring_index: u64,
    pub(crate) ring_generation: u64,
    pub(crate) padding: [u8; 32],
}

impl Default for ChannelControl {
    fn default() -> Self {
        Self {
            source_id: 0,
            generation: 0,
            capacity: 0,
            closed: 0,
            head: 0,
            tail: 0,
            len: 0,
            send_q_head: WAIT_LINK_NONE,
            recv_q_head: WAIT_LINK_NONE,
            reservation_generation: 0,
            ring_index: 0,
            ring_generation: 0,
            padding: [0; 32],
        }
    }
}

const _: () = {
    assert!(std::mem::size_of::<ChannelControl>() == 128);
    assert!(std::mem::align_of::<ChannelControl>() == 64);
};

/// 通道身份：下标加 generation。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChannelHandle {
    pub(crate) index: u32,
    pub(crate) generation: u64,
}

/// `try_send` 的立即结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrySendErr {
    Full,
    Closed,
}

/// `try_recv` 的立即结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TryRecvErr {
    Empty,
    Closed,
}

/// 阻塞 send 的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendOutcome {
    Sent { wake: Option<WaitNodeHandle> },
    Parked(WaitNodeHandle),
}

/// 阻塞 recv 的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecvOutcome {
    Value {
        payload: u64,
        wake: Option<WaitNodeHandle>,
    },
    Closed,
    Parked(WaitNodeHandle),
}

#[derive(Debug)]
struct ChannelIdentity {
    generation: u64,
    occupied: bool,
    next_free: Option<u32>,
}

#[derive(Debug)]
struct ChannelRecord {
    control: ChannelControl,
    ring: Vec<ChannelSlotState>,
    send_q: WaitQueue,
    recv_q: WaitQueue,
}

/// 通道表：密集下标、generation、固定容量环。
#[derive(Debug)]
pub(crate) struct ChannelTable {
    records: Vec<ChannelRecord>,
    identities: Vec<ChannelIdentity>,
    free: Option<u32>,
}

impl ChannelTable {
    pub(crate) fn new() -> Self {
        Self {
            records: Vec::new(),
            identities: Vec::new(),
            free: None,
        }
    }

    pub(crate) fn create(
        &mut self,
        wait: &mut WaitPlane,
        capacity: i64,
    ) -> Result<ChannelHandle, RawInvariant> {
        if capacity < 0 {
            return Err(RawInvariant::new("channel 缓冲长度不能为负"));
        }
        let cap = u64::try_from(capacity).expect("非负容量");
        let source = wait.alloc_source(WaitSourceKind::Channel)?;
        let index = if let Some(index) = self.free {
            let identity = &mut self.identities[usize::try_from(index).expect("通道下标")];
            self.free = identity.next_free;
            identity.next_free = None;
            identity.occupied = true;
            identity.generation = identity
                .generation
                .checked_add(1)
                .ok_or_else(|| RawInvariant::new("channel generation 溢出"))?;
            index
        } else {
            let index = u32::try_from(self.identities.len())
                .map_err(|_| RawInvariant::new("通道表溢出"))?;
            self.identities.push(ChannelIdentity {
                generation: 1,
                occupied: true,
                next_free: None,
            });
            self.records.push(ChannelRecord {
                control: ChannelControl::default(),
                ring: Vec::new(),
                send_q: WaitQueue::default(),
                recv_q: WaitQueue::default(),
            });
            index
        };
        let generation = self.identities[usize::try_from(index).expect("通道下标")].generation;
        let record = &mut self.records[usize::try_from(index).expect("通道下标")];
        record.ring = vec![ChannelSlotState::Empty; usize::try_from(cap).expect("容量下标")];
        record.send_q = WaitQueue::default();
        record.recv_q = WaitQueue::default();
        record.control = ChannelControl {
            source_id: source.raw(),
            generation,
            capacity: cap,
            closed: 0,
            head: 0,
            tail: 0,
            len: 0,
            send_q_head: WAIT_LINK_NONE,
            recv_q_head: WAIT_LINK_NONE,
            reservation_generation: 0,
            ring_index: u64::from(index),
            ring_generation: generation,
            padding: [0; 32],
        };
        Ok(ChannelHandle { index, generation })
    }

    fn index(&self, handle: ChannelHandle) -> Result<usize, RawInvariant> {
        let index = usize::try_from(handle.index).expect("通道下标");
        let identity = self
            .identities
            .get(index)
            .filter(|identity| identity.occupied && identity.generation == handle.generation)
            .ok_or_else(|| RawInvariant::new("过期的 channel handle"))?;
        debug_assert!(identity.generation != 0);
        Ok(index)
    }

    pub(crate) fn source(&self, handle: ChannelHandle) -> Result<WaitSourceId, RawInvariant> {
        let index = self.index(handle)?;
        Ok(WaitSourceId(self.records[index].control.source_id))
    }

    pub(crate) fn control(&self, handle: ChannelHandle) -> Result<&ChannelControl, RawInvariant> {
        let index = self.index(handle)?;
        Ok(&self.records[index].control)
    }

    fn record_mut(&mut self, handle: ChannelHandle) -> Result<&mut ChannelRecord, RawInvariant> {
        let index = self.index(handle)?;
        Ok(&mut self.records[index])
    }

    fn sync_queue_heads(record: &mut ChannelRecord) {
        record.control.send_q_head = record.send_q.head.map_or(WAIT_LINK_NONE, u64::from);
        record.control.recv_q_head = record.recv_q.head.map_or(WAIT_LINK_NONE, u64::from);
    }

    pub(crate) fn try_send(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
        payload: u64,
    ) -> Result<Result<Option<WaitNodeHandle>, TrySendErr>, RawInvariant> {
        let source = self.source(handle)?;
        wait.lock(source)?;
        let result = self.try_send_locked(wait, handle, payload);
        wait.unlock(source)?;
        result
    }

    fn try_send_locked(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
        payload: u64,
    ) -> Result<Result<Option<WaitNodeHandle>, TrySendErr>, RawInvariant> {
        let record = self.record_mut(handle)?;
        if record.control.closed != 0 {
            return Ok(Err(TrySendErr::Closed));
        }
        if let Some(waiter) = wait.dequeue(&mut record.recv_q)? {
            Self::sync_queue_heads(record);
            wait.write_payload(waiter, payload)?;
            return Ok(Ok(Some(waiter)));
        }
        if record.control.capacity == 0 {
            return Ok(Err(TrySendErr::Full));
        }
        if record.control.len >= record.control.capacity {
            return Ok(Err(TrySendErr::Full));
        }
        self.publish_slot(handle, payload)?;
        Ok(Ok(None))
    }

    pub(crate) fn try_recv(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
    ) -> Result<Result<(u64, Option<WaitNodeHandle>), TryRecvErr>, RawInvariant> {
        let source = self.source(handle)?;
        wait.lock(source)?;
        let result = self.try_recv_locked(wait, handle);
        wait.unlock(source)?;
        result
    }

    fn try_recv_locked(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
    ) -> Result<Result<(u64, Option<WaitNodeHandle>), TryRecvErr>, RawInvariant> {
        if let Some(value) = self.take_slot(handle)? {
            return Ok(Ok((value, None)));
        }
        let record = self.record_mut(handle)?;
        if let Some(waiter) = wait.dequeue(&mut record.send_q)? {
            Self::sync_queue_heads(record);
            let payload = wait.node(waiter)?.payload_offset;
            return Ok(Ok((payload, Some(waiter))));
        }
        let record = self.record_mut(handle)?;
        if record.control.closed != 0 {
            return Ok(Err(TryRecvErr::Closed));
        }
        Ok(Err(TryRecvErr::Empty))
    }

    pub(crate) fn send(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
        coroutine: CoroutineHandle,
        payload: u64,
        large: bool,
    ) -> Result<SendOutcome, RawInvariant> {
        let source = self.source(handle)?;
        wait.lock(source)?;
        if large {
            return self.send_large(wait, handle, coroutine, payload, source);
        }
        let outcome = self.send_locked(wait, handle, coroutine, payload);
        wait.unlock(source)?;
        outcome
    }

    fn send_large(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
        coroutine: CoroutineHandle,
        payload: u64,
        source: WaitSourceId,
    ) -> Result<SendOutcome, RawInvariant> {
        let record = self.record_mut(handle)?;
        if record.control.closed != 0 {
            wait.unlock(source)?;
            return Err(RawInvariant::new("send on closed channel"));
        }
        if let Some(waiter) = wait.dequeue(&mut record.recv_q)? {
            Self::sync_queue_heads(record);
            wait.write_payload(waiter, payload)?;
            wait.unlock(source)?;
            return Ok(SendOutcome::Sent { wake: Some(waiter) });
        }
        if record.control.capacity > 0 && record.control.len < record.control.capacity {
            let generation = self.reserve_slot(handle)?;
            wait.unlock(source)?;
            wait.lock(source)?;
            if self.record_mut(handle)?.control.closed != 0 {
                self.abort_reservation(handle, generation)?;
                wait.unlock(source)?;
                return Err(RawInvariant::new("send on closed channel"));
            }
            self.publish_reserved(handle, generation, payload)?;
            wait.unlock(source)?;
            return Ok(SendOutcome::Sent { wake: None });
        }
        let outcome = self.park_send(wait, handle, coroutine, payload)?;
        wait.unlock(source)?;
        Ok(outcome)
    }

    fn send_locked(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
        coroutine: CoroutineHandle,
        payload: u64,
    ) -> Result<SendOutcome, RawInvariant> {
        let record = self.record_mut(handle)?;
        if record.control.closed != 0 {
            return Err(RawInvariant::new("send on closed channel"));
        }
        if let Some(waiter) = wait.dequeue(&mut record.recv_q)? {
            Self::sync_queue_heads(record);
            wait.write_payload(waiter, payload)?;
            return Ok(SendOutcome::Sent { wake: Some(waiter) });
        }
        if record.control.capacity > 0 && record.control.len < record.control.capacity {
            self.publish_slot(handle, payload)?;
            return Ok(SendOutcome::Sent { wake: None });
        }
        self.park_send(wait, handle, coroutine, payload)
    }

    fn park_send(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
        coroutine: CoroutineHandle,
        payload: u64,
    ) -> Result<SendOutcome, RawInvariant> {
        let source = self.source(handle)?;
        let generation = wait.begin_wait(coroutine)?;
        let node = wait.alloc_node(coroutine, source, 0, payload, 0, 0, generation)?;
        wait.arm_nodes(coroutine, vec![node]);
        let record = self.record_mut(handle)?;
        wait.enqueue(&mut record.send_q, node)?;
        Self::sync_queue_heads(record);
        Ok(SendOutcome::Parked(node))
    }

    pub(crate) fn recv(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
        coroutine: CoroutineHandle,
    ) -> Result<RecvOutcome, RawInvariant> {
        let source = self.source(handle)?;
        wait.lock(source)?;
        let outcome = self.recv_locked(wait, handle, coroutine);
        wait.unlock(source)?;
        outcome
    }

    fn recv_locked(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
        coroutine: CoroutineHandle,
    ) -> Result<RecvOutcome, RawInvariant> {
        if let Some(value) = self.take_slot(handle)? {
            return Ok(RecvOutcome::Value {
                payload: value,
                wake: None,
            });
        }
        let record = self.record_mut(handle)?;
        if let Some(waiter) = wait.dequeue(&mut record.send_q)? {
            Self::sync_queue_heads(record);
            let payload = wait.node(waiter)?.payload_offset;
            return Ok(RecvOutcome::Value {
                payload,
                wake: Some(waiter),
            });
        }
        let record = self.record_mut(handle)?;
        if record.control.closed != 0 {
            return Ok(RecvOutcome::Closed);
        }
        let source = self.source(handle)?;
        let generation = wait.begin_wait(coroutine)?;
        let node = wait.alloc_node(coroutine, source, 0, 0, 0, 0, generation)?;
        wait.arm_nodes(coroutine, vec![node]);
        let record = self.record_mut(handle)?;
        wait.enqueue(&mut record.recv_q, node)?;
        Self::sync_queue_heads(record);
        Ok(RecvOutcome::Parked(node))
    }

    pub(crate) fn close(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
    ) -> Result<Vec<WaitNodeHandle>, RawInvariant> {
        let source = self.source(handle)?;
        wait.lock(source)?;
        let woken = self.close_locked(wait, handle);
        wait.unlock(source)?;
        woken
    }

    fn close_locked(
        &mut self,
        wait: &mut WaitPlane,
        handle: ChannelHandle,
    ) -> Result<Vec<WaitNodeHandle>, RawInvariant> {
        let record = self.record_mut(handle)?;
        if record.control.closed != 0 {
            return Err(RawInvariant::new("close on closed channel"));
        }
        record.control.closed = 1;
        let mut woken = Vec::new();
        while let Some(node) = wait.dequeue(&mut record.send_q)? {
            woken.push(node);
        }
        while let Some(node) = wait.dequeue(&mut record.recv_q)? {
            woken.push(node);
        }
        Self::sync_queue_heads(record);
        Ok(woken)
    }

    pub(crate) fn is_ready_send(&self, handle: ChannelHandle) -> Result<bool, RawInvariant> {
        let record = &self.records[self.index(handle)?];
        if record.control.closed != 0 {
            return Ok(true);
        }
        Ok(record.recv_q.head.is_some()
            || (record.control.capacity > 0 && record.control.len < record.control.capacity))
    }

    pub(crate) fn is_ready_recv(&self, handle: ChannelHandle) -> Result<bool, RawInvariant> {
        let record = &self.records[self.index(handle)?];
        Ok(record.control.len > 0 || record.send_q.head.is_some() || record.control.closed != 0)
    }

    pub(crate) fn sync_heads(&mut self, handle: ChannelHandle) -> Result<(), RawInvariant> {
        let record = self.record_mut(handle)?;
        Self::sync_queue_heads(record);
        Ok(())
    }

    pub(crate) fn send_queue_mut(
        &mut self,
        handle: ChannelHandle,
    ) -> Result<&mut WaitQueue, RawInvariant> {
        Ok(&mut self.record_mut(handle)?.send_q)
    }

    pub(crate) fn recv_queue_mut(
        &mut self,
        handle: ChannelHandle,
    ) -> Result<&mut WaitQueue, RawInvariant> {
        Ok(&mut self.record_mut(handle)?.recv_q)
    }

    pub(crate) fn unlink_waiter(
        &mut self,
        wait: &mut WaitPlane,
        handle: WaitNodeHandle,
    ) -> Result<bool, RawInvariant> {
        let source = wait.source_of(handle)?;
        for index in 0..self.records.len() {
            if WaitSourceId(self.records[index].control.source_id) != source {
                continue;
            }
            let mut send = self.records[index].send_q;
            let mut recv = self.records[index].recv_q;
            let unlinked_send = wait.unlink(&mut send, handle)?;
            let unlinked_recv = wait.unlink(&mut recv, handle)?;
            self.records[index].send_q = send;
            self.records[index].recv_q = recv;
            Self::sync_queue_heads(&mut self.records[index]);
            return Ok(unlinked_send || unlinked_recv);
        }
        Ok(false)
    }

    fn abort_reservation(
        &mut self,
        handle: ChannelHandle,
        generation: u64,
    ) -> Result<(), RawInvariant> {
        let record = self.record_mut(handle)?;
        let tail = usize::try_from(record.control.tail).expect("环下标");
        match record.ring[tail] {
            ChannelSlotState::Reserved {
                generation: reserved,
            } if reserved == generation => {
                record.ring[tail] = ChannelSlotState::Empty;
                Ok(())
            }
            _ => Err(RawInvariant::new("reservation 已失效")),
        }
    }

    fn reserve_slot(&mut self, handle: ChannelHandle) -> Result<u64, RawInvariant> {
        let record = self.record_mut(handle)?;
        if record.control.len >= record.control.capacity {
            return Err(RawInvariant::new("channel 没有剩余槽可预约"));
        }
        let generation = record
            .control
            .reservation_generation
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("reservation generation 溢出"))?;
        record.control.reservation_generation = generation;
        let tail = usize::try_from(record.control.tail).expect("环下标");
        record.ring[tail] = ChannelSlotState::Reserved { generation };
        Ok(generation)
    }

    fn publish_reserved(
        &mut self,
        handle: ChannelHandle,
        generation: u64,
        payload: u64,
    ) -> Result<(), RawInvariant> {
        let record = self.record_mut(handle)?;
        let tail = usize::try_from(record.control.tail).expect("环下标");
        match record.ring[tail] {
            ChannelSlotState::Reserved {
                generation: reserved,
            } if reserved == generation => {}
            _ => return Err(RawInvariant::new("未发布 reservation 对 recv 不可见")),
        }
        record.ring[tail] = ChannelSlotState::Occupied {
            payload,
            generation,
        };
        record.control.tail = (record.control.tail + 1) % record.control.capacity;
        record.control.len += 1;
        Ok(())
    }

    fn publish_slot(&mut self, handle: ChannelHandle, payload: u64) -> Result<(), RawInvariant> {
        let generation = self.reserve_slot(handle)?;
        self.publish_reserved(handle, generation, payload)
    }

    fn take_slot(&mut self, handle: ChannelHandle) -> Result<Option<u64>, RawInvariant> {
        let record = self.record_mut(handle)?;
        if record.control.len == 0 {
            return Ok(None);
        }
        let head = usize::try_from(record.control.head).expect("环下标");
        let value = match record.ring[head] {
            ChannelSlotState::Occupied { payload, .. } => payload,
            ChannelSlotState::Reserved { .. } | ChannelSlotState::Empty => {
                return Ok(None);
            }
        };
        record.ring[head] = ChannelSlotState::Empty;
        record.control.head = (record.control.head + 1) % record.control.capacity;
        record.control.len -= 1;
        Ok(Some(value))
    }
}
