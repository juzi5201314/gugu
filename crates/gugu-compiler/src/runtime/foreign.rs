//! 普通 `ForeignBridge`、`DirtyCpu` 与 `ForeignLeaf` 的确定性交接。
//!
//! 状态字仍由协程控制块的既有转换发出。这里只做额度、lease、错误捕获、回调边界和
//! poller 分流，不创建第二套 lifecycle。

use std::collections::VecDeque;

use super::coroutine::{
    CoroutineHandle, CoroutineState, CoroutineTable, FOREIGN_DETACHED, ForeignBridgeState,
};
use super::foreign_schema::{
    BLOCKING_QUEUE_CAP_BYTES, BLOCKING_SERVICE_BUDGET, BLOCKING_WAITER_BYTES, MAX_BLOCKING_WORKERS,
    RETAKE_GRACE_US,
};
use super::scheduler::dirty_target;
use super::slab::RawInvariant;

/// 交给 native 之前必须 pin 的受管根。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ManagedRoot {
    pub(crate) handle: u64,
    pub(crate) pinned: bool,
}

/// 调用点已经选定的交接模式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BridgeMode {
    /// 留在当前 processor 和协程栈上。
    Leaf,
    /// 可能阻塞，先取得 `BridgeCredit`。
    Ordinary,
    /// 立刻放开 processor。`opaque` 表示 native body 不能有 stack map。
    Dirty { opaque: bool },
}

/// admission 的可观察结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmitOutcome {
    Leaf,
    Attached { credit: u64 },
    Waiting,
    DirtyActive,
    DirtyWaiting,
}

/// native 返回后的恢复路径。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReturnPath {
    Fast,
    IdleProcessor,
    Batch,
}

/// poller 的目标族。两边都把完成事件收成同一条 ready 序列。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PollerFamily {
    Linux,
    Windows,
}

impl PollerFamily {
    pub(crate) const fn opcode(self) -> &'static str {
        match self {
            Self::Linux => "epoll",
            Self::Windows => "iocp",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Linux => 0,
            Self::Windows => 1,
        }
    }
}

/// 只有 socket 类操作进入 poller。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IoClass {
    Socket,
    RegularFile,
    ForeignCall,
}

/// 已登记的外部线程。它不能直接碰协程或 GC metadata。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExternalThread {
    id: u64,
}

#[derive(Clone, Copy, Debug)]
struct Watch {
    handle: CoroutineHandle,
    generation: u64,
    eligible_at: u64,
}

#[derive(Clone, Copy, Debug)]
struct Live {
    handle: CoroutineHandle,
    opaque: bool,
    safepoints: u32,
}

#[derive(Default)]
struct Poller {
    pending: Vec<u64>,
    ready: Vec<u64>,
}

/// 外调交接世界。逻辑时钟由调用方传入，测试不睡眠。
pub(crate) struct ForeignWorld {
    table: CoroutineTable,
    next_credit: u64,
    active_blocking: u32,
    waiting: VecDeque<CoroutineHandle>,
    dirty_active: u64,
    dirty_limit: u64,
    dirty_target: u64,
    dirty_wait: VecDeque<CoroutineHandle>,
    idle_processors: u32,
    watches: Vec<Watch>,
    live: Vec<Live>,
    leaf_error: u64,
    captured_error: u64,
    callback_depth: u32,
    panic_abort: bool,
    next_external: u64,
    wake_managed: bool,
    pollers: [Poller; 2],
}

impl ForeignWorld {
    pub(crate) fn new(parallelism: u64) -> Result<Self, RawInvariant> {
        if parallelism == 0 {
            return Err(RawInvariant::new("调度并行度必须为正"));
        }
        let target = dirty_target(parallelism);
        Ok(Self {
            table: CoroutineTable::default(),
            next_credit: 0,
            active_blocking: 0,
            waiting: VecDeque::new(),
            dirty_active: 0,
            dirty_limit: target,
            dirty_target: target,
            dirty_wait: VecDeque::new(),
            idle_processors: u32::try_from(parallelism).unwrap_or(u32::MAX),
            watches: Vec::new(),
            live: Vec::new(),
            leaf_error: 0,
            captured_error: 0,
            callback_depth: 0,
            panic_abort: false,
            next_external: 0,
            wake_managed: false,
            pollers: [Poller::default(), Poller::default()],
        })
    }

    pub(crate) fn spawn_running(&mut self) -> Result<CoroutineHandle, RawInvariant> {
        if self.idle_processors == 0 {
            return Err(RawInvariant::new("没有空闲 LogicalProcessor"));
        }
        let handle = self.table.allocate()?;
        let (slot, _) = self.table.get_mut(handle)?;
        slot.hot
            .transition(CoroutineState::New, CoroutineState::Runnable)?;
        slot.hot.take_running()?;
        self.idle_processors -= 1;
        Ok(handle)
    }

    pub(crate) const fn active_blocking(&self) -> u32 {
        self.active_blocking
    }

    pub(crate) fn waiting_len(&self) -> usize {
        self.waiting.len()
    }

    pub(crate) const fn dirty_active(&self) -> u64 {
        self.dirty_active
    }

    pub(crate) const fn leaf_error(&self) -> u64 {
        self.leaf_error
    }

    pub(crate) const fn captured_error(&self) -> u64 {
        self.captured_error
    }

    pub(crate) const fn panic_abort(&self) -> bool {
        self.panic_abort
    }

    pub(crate) const fn wake_managed(&self) -> bool {
        self.wake_managed
    }

    pub(crate) fn set_parallelism(&mut self, parallelism: u64) -> Result<(), RawInvariant> {
        if parallelism == 0 {
            return Err(RawInvariant::new("调度并行度必须为正"));
        }
        self.dirty_target = dirty_target(parallelism);
        self.dirty_limit = self.dirty_target.max(self.dirty_active);
        self.promote_dirty()
    }

    pub(crate) fn admit(
        &mut self,
        handle: CoroutineHandle,
        mode: BridgeMode,
        roots: &[ManagedRoot],
        stub: u64,
        frame_offset: u64,
        frame_size: u64,
    ) -> Result<AdmitOutcome, RawInvariant> {
        self.require_running(handle)?;
        check_roots(roots)?;
        match mode {
            BridgeMode::Leaf => {
                self.leaf_error = 0;
                Ok(AdmitOutcome::Leaf)
            }
            BridgeMode::Ordinary => self.admit_ordinary(handle, stub, frame_offset, frame_size),
            BridgeMode::Dirty { opaque } => {
                self.admit_dirty(handle, opaque, stub, frame_offset, frame_size)
            }
        }
    }

    pub(crate) fn capture_leaf_error(&mut self, errno: u64) {
        self.leaf_error = errno;
    }

    pub(crate) fn complete(
        &mut self,
        handle: CoroutineHandle,
        errno: u64,
    ) -> Result<ReturnPath, RawInvariant> {
        let mode = self.bridge(handle)?.mode();
        self.captured_error = errno;
        self.bridge_mut(handle)?.capture_error(errno);
        if mode == ForeignBridgeState::DIRTY {
            return self.finish_dirty(handle);
        }
        if mode != ForeignBridgeState::ORDINARY {
            return Err(RawInvariant::new("没有可完成的 ForeignBridge"));
        }
        let detached = self.state_word(handle)? & FOREIGN_DETACHED != 0;
        if detached {
            self.finish_detached(handle)
        } else {
            self.finish_fast(handle)
        }
    }

    pub(crate) fn arm_pressure(
        &mut self,
        handle: CoroutineHandle,
        now_us: u64,
    ) -> Result<(), RawInvariant> {
        let word = self.state_word(handle)?;
        if word & FOREIGN_DETACHED != 0 {
            return Ok(());
        }
        let generation = word >> 8;
        if self
            .watches
            .iter()
            .any(|watch| watch.handle == handle && watch.generation == generation)
        {
            return Ok(());
        }
        let eligible_at = now_us
            .checked_add(RETAKE_GRACE_US)
            .ok_or_else(|| RawInvariant::new("retake 宽限溢出"))?;
        self.watches.push(Watch {
            handle,
            generation,
            eligible_at,
        });
        Ok(())
    }

    pub(crate) fn retake_pressure(
        &mut self,
        handle: CoroutineHandle,
        now_us: u64,
    ) -> Result<bool, RawInvariant> {
        let generation = self.state_word(handle)? >> 8;
        let due = self.watches.iter().any(|watch| {
            watch.handle == handle && watch.generation == generation && now_us >= watch.eligible_at
        });
        if !due {
            return Ok(false);
        }
        self.retake_immediate(handle)
    }

    pub(crate) fn retake_immediate(
        &mut self,
        handle: CoroutineHandle,
    ) -> Result<bool, RawInvariant> {
        let (slot, _) = self.table.get_mut(handle)?;
        if slot.hot.retake_detached().is_err() {
            return Ok(false);
        }
        self.idle_processors = self.idle_processors.saturating_add(1);
        Ok(true)
    }

    pub(crate) fn begin_callback(&mut self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        let mode = self.bridge(handle)?.mode();
        if mode != ForeignBridgeState::ORDINARY {
            return Err(RawInvariant::new("不能从 leaf 或 dirty 回调 Gugu"));
        }
        let _ = self.retake_immediate(handle)?;
        self.callback_depth = self
            .callback_depth
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("回调深度溢出"))?;
        Ok(())
    }

    pub(crate) fn finish_callback(&mut self, panicked: bool) -> Result<(), RawInvariant> {
        if self.callback_depth == 0 {
            return Err(RawInvariant::new("没有进行中的回调"));
        }
        self.callback_depth -= 1;
        if panicked {
            self.panic_abort = true;
        }
        Ok(())
    }

    pub(crate) fn record_safepoint(&mut self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        let live = self
            .live
            .iter_mut()
            .find(|live| live.handle == handle)
            .ok_or_else(|| RawInvariant::new("没有 bridge frame"))?;
        if live.opaque {
            return Err(RawInvariant::new("opaque native 不能伪造 stack map"));
        }
        live.safepoints += 1;
        Ok(())
    }

    pub(crate) fn safepoints(&self, handle: CoroutineHandle) -> u32 {
        self.live
            .iter()
            .find(|live| live.handle == handle)
            .map(|live| live.safepoints)
            .unwrap_or(0)
    }

    pub(crate) fn error_state(&self, handle: CoroutineHandle) -> Result<u64, RawInvariant> {
        Ok(self.bridge(handle)?.error_state())
    }

    pub(crate) fn attach_external(&mut self) -> ExternalThread {
        self.next_external += 1;
        ExternalThread {
            id: self.next_external,
        }
    }

    pub(crate) fn operate_coroutine(&self, thread: ExternalThread) -> Result<(), RawInvariant> {
        self.reject_external(thread)
    }

    pub(crate) fn operate_gc(&self, thread: ExternalThread) -> Result<(), RawInvariant> {
        self.reject_external(thread)
    }

    pub(crate) fn submit(
        &mut self,
        family: PollerFamily,
        class: IoClass,
        token: u64,
    ) -> Result<(), RawInvariant> {
        match class {
            IoClass::Socket => {
                self.pollers[family.index()].pending.push(token);
                Ok(())
            }
            IoClass::RegularFile | IoClass::ForeignCall => {
                Err(RawInvariant::new("该操作走 BlockingBridge，不占 poller"))
            }
        }
    }

    pub(crate) fn complete_io(
        &mut self,
        family: PollerFamily,
        token: u64,
    ) -> Result<(), RawInvariant> {
        let poller = &mut self.pollers[family.index()];
        let position = poller
            .pending
            .iter()
            .position(|item| *item == token)
            .ok_or_else(|| RawInvariant::new("poller 没有这个等待"))?;
        let token = poller.pending.remove(position);
        poller.ready.push(token);
        Ok(())
    }

    pub(crate) fn ready(&self, family: PollerFamily) -> &[u64] {
        &self.pollers[family.index()].ready
    }

    fn admit_ordinary(
        &mut self,
        handle: CoroutineHandle,
        stub: u64,
        frame_offset: u64,
        frame_size: u64,
    ) -> Result<AdmitOutcome, RawInvariant> {
        if self.active_blocking >= MAX_BLOCKING_WORKERS {
            return self.enqueue_waiter(handle, stub, frame_offset, frame_size);
        }
        self.enter_ordinary(handle, false, stub, frame_offset, frame_size)?;
        let credit = self.bridge(handle)?.credit();
        Ok(AdmitOutcome::Attached { credit })
    }

    fn enqueue_waiter(
        &mut self,
        handle: CoroutineHandle,
        stub: u64,
        frame_offset: u64,
        frame_size: u64,
    ) -> Result<AdmitOutcome, RawInvariant> {
        let cap = BLOCKING_QUEUE_CAP_BYTES / BLOCKING_WAITER_BYTES;
        if self.waiting.len() as u32 >= cap {
            return Err(RawInvariant::new("BlockingBridge waiter 达到内存上限"));
        }
        let (slot, _) = self.table.get_mut(handle)?;
        slot.hot
            .transition(CoroutineState::Running, CoroutineState::Waiting)?;
        self.publish(
            handle,
            ForeignBridgeState::ORDINARY,
            0,
            stub,
            frame_offset,
            frame_size,
        )?;
        self.note_live(handle, false);
        self.idle_processors = self.idle_processors.saturating_add(1);
        self.waiting.push_back(handle);
        Ok(AdmitOutcome::Waiting)
    }

    fn admit_dirty(
        &mut self,
        handle: CoroutineHandle,
        opaque: bool,
        stub: u64,
        frame_offset: u64,
        frame_size: u64,
    ) -> Result<AdmitOutcome, RawInvariant> {
        if self.dirty_active >= self.dirty_limit {
            self.enter_dirty_waiting(handle, opaque, stub, frame_offset, frame_size)?;
            return Ok(AdmitOutcome::DirtyWaiting);
        }
        self.dirty_active += 1;
        self.enter_dirty_active(handle, opaque, stub, frame_offset, frame_size)?;
        Ok(AdmitOutcome::DirtyActive)
    }

    fn enter_dirty_waiting(
        &mut self,
        handle: CoroutineHandle,
        opaque: bool,
        stub: u64,
        frame_offset: u64,
        frame_size: u64,
    ) -> Result<(), RawInvariant> {
        let (slot, _) = self.table.get_mut(handle)?;
        slot.hot
            .transition(CoroutineState::Running, CoroutineState::DirtyWaiting)?;
        self.publish(
            handle,
            ForeignBridgeState::DIRTY,
            0,
            stub,
            frame_offset,
            frame_size,
        )?;
        self.note_live(handle, opaque);
        self.idle_processors = self.idle_processors.saturating_add(1);
        self.dirty_wait.push_back(handle);
        Ok(())
    }

    fn enter_dirty_active(
        &mut self,
        handle: CoroutineHandle,
        opaque: bool,
        stub: u64,
        frame_offset: u64,
        frame_size: u64,
    ) -> Result<(), RawInvariant> {
        self.become_detached(handle, CoroutineState::Running)?;
        self.publish(
            handle,
            ForeignBridgeState::DIRTY,
            0,
            stub,
            frame_offset,
            frame_size,
        )?;
        self.note_live(handle, opaque);
        self.idle_processors = self.idle_processors.saturating_add(1);
        Ok(())
    }

    fn enter_ordinary(
        &mut self,
        handle: CoroutineHandle,
        detach: bool,
        stub: u64,
        frame_offset: u64,
        frame_size: u64,
    ) -> Result<(), RawInvariant> {
        let credit = self.alloc_credit()?;
        if detach {
            self.become_detached(handle, CoroutineState::Running)?;
        } else {
            let (slot, _) = self.table.get_mut(handle)?;
            slot.hot
                .transition(CoroutineState::Running, CoroutineState::Foreign)?;
        }
        self.publish(
            handle,
            ForeignBridgeState::ORDINARY,
            credit,
            stub,
            frame_offset,
            frame_size,
        )?;
        self.note_live(handle, false);
        self.active_blocking += 1;
        Ok(())
    }

    fn become_detached(
        &mut self,
        handle: CoroutineHandle,
        from: CoroutineState,
    ) -> Result<(), RawInvariant> {
        let (slot, _) = self.table.get_mut(handle)?;
        slot.hot.transition(from, CoroutineState::Foreign)?;
        slot.hot.retake_detached()?;
        Ok(())
    }

    fn publish(
        &mut self,
        handle: CoroutineHandle,
        mode: u64,
        credit: u64,
        stub: u64,
        frame_offset: u64,
        frame_size: u64,
    ) -> Result<(), RawInvariant> {
        let lease = self.state_word(handle)?;
        let (_, cold) = self.table.get_mut(handle)?;
        cold.foreign_bridge
            .publish(mode, stub, frame_offset, frame_size, lease, credit)
    }

    fn finish_fast(&mut self, handle: CoroutineHandle) -> Result<ReturnPath, RawInvariant> {
        let (slot, _) = self.table.get_mut(handle)?;
        slot.hot
            .transition(CoroutineState::Foreign, CoroutineState::Running)?;
        self.reclaim(handle)?;
        self.service(1)?;
        Ok(ReturnPath::Fast)
    }

    fn finish_detached(&mut self, handle: CoroutineHandle) -> Result<ReturnPath, RawInvariant> {
        if self.idle_processors > 0 {
            self.idle_processors -= 1;
            let (slot, _) = self.table.get_mut(handle)?;
            slot.hot
                .transition(CoroutineState::Foreign, CoroutineState::Running)?;
            self.reclaim(handle)?;
            self.service(1)?;
            return Ok(ReturnPath::IdleProcessor);
        }
        let (slot, _) = self.table.get_mut(handle)?;
        slot.hot.claim_for_batch()?;
        self.reclaim(handle)?;
        self.service(1)?;
        Ok(ReturnPath::Batch)
    }

    fn finish_dirty(&mut self, handle: CoroutineHandle) -> Result<ReturnPath, RawInvariant> {
        let path = self.finish_detached_dirty(handle)?;
        self.transfer_or_release_dirty()?;
        Ok(path)
    }

    fn finish_detached_dirty(
        &mut self,
        handle: CoroutineHandle,
    ) -> Result<ReturnPath, RawInvariant> {
        self.drop_live(handle);
        if self.idle_processors > 0 {
            self.idle_processors -= 1;
            let (slot, _) = self.table.get_mut(handle)?;
            slot.hot
                .transition(CoroutineState::Foreign, CoroutineState::Running)?;
            self.bridge_mut(handle)?.clear();
            return Ok(ReturnPath::IdleProcessor);
        }
        let (slot, _) = self.table.get_mut(handle)?;
        slot.hot.claim_for_batch()?;
        self.bridge_mut(handle)?.clear();
        Ok(ReturnPath::Batch)
    }

    fn transfer_or_release_dirty(&mut self) -> Result<(), RawInvariant> {
        if let Some(waiter) = self.dirty_wait.pop_front() {
            self.become_detached(waiter, CoroutineState::DirtyWaiting)?;
            return Ok(());
        }
        self.dirty_active = self.dirty_active.saturating_sub(1);
        self.dirty_limit = self.dirty_target.max(self.dirty_active);
        self.wake_managed = true;
        Ok(())
    }

    fn promote_dirty(&mut self) -> Result<(), RawInvariant> {
        while self.dirty_active < self.dirty_target {
            let Some(waiter) = self.dirty_wait.pop_front() else {
                break;
            };
            self.dirty_active += 1;
            self.become_detached(waiter, CoroutineState::DirtyWaiting)?;
        }
        Ok(())
    }

    fn reclaim(&mut self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        self.bridge_mut(handle)?.take_credit()?;
        self.bridge_mut(handle)?.clear();
        self.drop_live(handle);
        self.watches.retain(|watch| watch.handle != handle);
        self.active_blocking = self.active_blocking.saturating_sub(1);
        Ok(())
    }

    fn service(&mut self, budget: u32) -> Result<(), RawInvariant> {
        let budget = budget.min(BLOCKING_SERVICE_BUDGET);
        let mut granted = 0;
        while granted < budget && self.active_blocking < MAX_BLOCKING_WORKERS {
            let Some(handle) = self.waiting.pop_front() else {
                break;
            };
            self.resume_waiter(handle)?;
            granted += 1;
        }
        Ok(())
    }

    fn resume_waiter(&mut self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        let (slot, _) = self.table.get_mut(handle)?;
        slot.hot
            .transition(CoroutineState::Waiting, CoroutineState::Runnable)?;
        slot.hot.take_running()?;
        self.become_detached(handle, CoroutineState::Running)?;
        let credit = self.alloc_credit()?;
        let lease = self.state_word(handle)?;
        let bridge = self.bridge_mut(handle)?;
        bridge.set_credit(credit)?;
        bridge.set_lease(lease);
        self.active_blocking += 1;
        Ok(())
    }

    pub(crate) fn lifecycle(
        &self,
        handle: CoroutineHandle,
    ) -> Result<CoroutineState, RawInvariant> {
        Ok(self.table.get(handle)?.0.hot.lifecycle()?)
    }

    pub(crate) fn detached(&self, handle: CoroutineHandle) -> Result<bool, RawInvariant> {
        Ok(self.state_word(handle)? & FOREIGN_DETACHED != 0)
    }

    fn alloc_credit(&mut self) -> Result<u64, RawInvariant> {
        self.next_credit = self
            .next_credit
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("BridgeCredit 溢出"))?;
        Ok(self.next_credit)
    }

    fn note_live(&mut self, handle: CoroutineHandle, opaque: bool) {
        self.live.push(Live {
            handle,
            opaque,
            safepoints: 0,
        });
    }

    fn drop_live(&mut self, handle: CoroutineHandle) {
        self.live.retain(|live| live.handle != handle);
    }

    fn require_running(&self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        let (slot, _) = self.table.get(handle)?;
        if slot.hot.lifecycle()? != CoroutineState::Running {
            return Err(RawInvariant::new("外调只能从 Running 进入"));
        }
        Ok(())
    }

    fn state_word(&self, handle: CoroutineHandle) -> Result<u64, RawInvariant> {
        let (slot, _) = self.table.get(handle)?;
        Ok(slot.hot.state.load(std::sync::atomic::Ordering::Acquire))
    }

    fn bridge(&self, handle: CoroutineHandle) -> Result<&ForeignBridgeState, RawInvariant> {
        Ok(&self.table.get(handle)?.1.foreign_bridge)
    }

    fn bridge_mut(
        &mut self,
        handle: CoroutineHandle,
    ) -> Result<&mut ForeignBridgeState, RawInvariant> {
        Ok(&mut self.table.get_mut(handle)?.1.foreign_bridge)
    }

    fn reject_external(&self, thread: ExternalThread) -> Result<(), RawInvariant> {
        if thread.id == 0 || thread.id > self.next_external {
            return Err(RawInvariant::new("外部线程尚未登记"));
        }
        Err(RawInvariant::new(
            "外部线程不能直接操作 Gugu 协程或 GC metadata",
        ))
    }
}

fn check_roots(roots: &[ManagedRoot]) -> Result<(), RawInvariant> {
    if roots.iter().any(|root| root.handle != 0 && !root.pinned) {
        return Err(RawInvariant::new("交给 native 的受管地址必须 pin"));
    }
    Ok(())
}
