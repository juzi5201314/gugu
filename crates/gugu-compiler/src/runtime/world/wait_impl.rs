//! 等待协议接到 `RawWorld`：channel / Join wait / select 只经 `ready_publish` 唤醒。

use super::super::channel::{ChannelHandle, RecvOutcome, SendOutcome, TryRecvErr, TrySendErr};
use super::super::coroutine::{CompletionValue, CoroutineHandle, CoroutineState};
use super::super::message::ReturnKind;
use super::super::select::{SelectCase, SelectOutcome, SelectRng, arm_select, select_commit};
use super::super::size_class::RuntimeSizeClassId;
use super::super::slab::{RawInvariant, RawSlot};
use super::super::wait::{
    JoinOutcome, SelectTxn, WAIT_NODE_SELECT, WAIT_NOTIFIED, WaitNodeHandle, WaitResult, park_wait,
    phase_building, wake_wait,
};
use super::RawWorld;

impl RawWorld {
    fn wait_processor(&self) -> Result<u64, RawInvariant> {
        self.scheduler
            .active_snapshot()
            .first()
            .copied()
            .ok_or_else(|| RawInvariant::new("没有 active processor 可唤醒"))
    }

    pub(crate) fn take_wait_result(
        &mut self,
        coroutine: CoroutineHandle,
    ) -> Result<Option<WaitResult>, RawInvariant> {
        self.controls.get(coroutine)?;
        Ok(self.wait.take_delivered(coroutine))
    }

    pub(crate) fn wake_node(&mut self, node: WaitNodeHandle) -> Result<bool, RawInvariant> {
        if !self.wait.node_completed(&self.controls, node) {
            return Ok(false);
        }
        let handle = self.wait.coroutine_of(node)?;
        let flags = self.wait.node(node)?.flags;
        let (slot, cold) = self.controls.get(handle)?;
        if flags & WAIT_NODE_SELECT != 0
            && SelectTxn::from_cold(cold.select_scratch).phase() == phase_building()
        {
            return Ok(false);
        }
        if slot.hot.lifecycle()? == CoroutineState::Parking {
            slot.hot
                .wait_word
                .fetch_or(WAIT_NOTIFIED, std::sync::atomic::Ordering::Release);
            if slot.hot.lifecycle()? == CoroutineState::Parking {
                return Ok(false);
            }
        }
        if slot.hot.lifecycle()? != CoroutineState::Waiting || !self.wait.mark_ready(node)? {
            return Ok(false);
        }
        let processor = self.wait_processor()?;
        let mut producer = super::super::scheduler::ProducerHandle::new(1);
        let woken = wake_wait(
            &mut self.scheduler,
            &mut self.controls,
            &mut producer,
            processor,
            handle,
            true,
        )?;
        if woken {
            self.cleanup_waiters(handle)?;
        }
        Ok(woken)
    }

    pub(super) fn cleanup_waiters(&mut self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        for node in self.wait.take_armed(handle) {
            if !self.wait.is_live(node) {
                continue;
            }
            let source = self.wait.source_of(node)?;
            self.wait.lock(source)?;
            let result = match self.wait.kind_of(source)? {
                super::super::wait::WaitSourceKind::Channel => {
                    self.channels.unlink_waiter(&mut self.wait, node)
                }
                _ => self.wait.unlink_source(source, node),
            };
            self.wait.unlock(source)?;
            result?;
            self.unregister_cancel_waiter(node)?;
            self.wait.release_node(node)?;
        }
        Ok(())
    }

    pub(super) fn park_current_wait(
        &mut self,
        coroutine: CoroutineHandle,
    ) -> Result<bool, RawInvariant> {
        self.park_current_wait_with(coroutine, |_| {})
    }

    pub(super) fn park_current_wait_with(
        &mut self,
        coroutine: CoroutineHandle,
        before_waiting: impl FnOnce(&mut Self),
    ) -> Result<bool, RawInvariant> {
        if self.wait.is_completed(coroutine) {
            let hot = &self.controls.get(coroutine)?.0.hot;
            hot.wait_word
                .fetch_and(!WAIT_NOTIFIED, std::sync::atomic::Ordering::Release);
            hot.transition(CoroutineState::Parking, CoroutineState::Running)?;
            self.cleanup_waiters(coroutine)?;
            return Ok(false);
        }
        before_waiting(self);
        park_wait(&mut self.controls, coroutine)?;
        // 配对 waker 的 Release 通知；是否产出值仍以真实交付槽为准。
        self.controls
            .get(coroutine)?
            .0
            .hot
            .wait_word
            .load(std::sync::atomic::Ordering::Acquire);
        if self.wait.is_completed(coroutine) {
            let winner = self
                .wait
                .armed_nodes(coroutine)
                .iter()
                .copied()
                .find(|node| self.wait.node_completed(&self.controls, *node));
            if let Some(node) = winner {
                self.wake_node(node)?;
            }
            debug_assert_eq!(
                self.controls.get(coroutine)?.0.hot.lifecycle()?,
                CoroutineState::Runnable
            );
        }
        Ok(true)
    }

    fn wake_nodes(
        &mut self,
        nodes: impl IntoIterator<Item = WaitNodeHandle>,
    ) -> Result<(), RawInvariant> {
        for node in nodes {
            self.wake_node(node)?;
        }
        Ok(())
    }

    pub(crate) fn channel_new(&mut self, capacity: i64) -> Result<ChannelHandle, RawInvariant> {
        self.channels.create(&mut self.wait, capacity)
    }

    pub(crate) fn channel_try_send(
        &mut self,
        handle: ChannelHandle,
        payload: u64,
    ) -> Result<Result<(), TrySendErr>, RawInvariant> {
        match self
            .channels
            .try_send(&mut self.wait, &mut self.controls, handle, payload)?
        {
            Ok(wake) => {
                if let Some(node) = wake {
                    self.wake_node(node)?;
                }
                Ok(Ok(()))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    pub(crate) fn channel_try_recv(
        &mut self,
        handle: ChannelHandle,
    ) -> Result<Result<u64, TryRecvErr>, RawInvariant> {
        match self
            .channels
            .try_recv(&mut self.wait, &mut self.controls, handle)?
        {
            Ok((payload, wake)) => {
                if let Some(node) = wake {
                    self.wake_node(node)?;
                }
                Ok(Ok(payload))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    pub(crate) fn channel_send(
        &mut self,
        handle: ChannelHandle,
        coroutine: CoroutineHandle,
        payload: u64,
        large: bool,
    ) -> Result<SendOutcome, RawInvariant> {
        let outcome = self.channels.send(
            &mut self.wait,
            &mut self.controls,
            handle,
            coroutine,
            payload,
            large,
        )?;
        match outcome {
            SendOutcome::Sent { wake } => {
                if let Some(node) = wake {
                    self.wake_node(node)?;
                }
                Ok(outcome)
            }
            SendOutcome::Parked(_) if self.park_current_wait(coroutine)? => Ok(outcome),
            SendOutcome::Parked(_) => match self.wait.take_delivered(coroutine) {
                Some(WaitResult::Sent) => Ok(SendOutcome::Sent { wake: None }),
                Some(WaitResult::SendClosed) => Err(RawInvariant::new("send on closed channel")),
                _ => Err(RawInvariant::new("send 完成缺少发送结果")),
            },
        }
    }

    pub(crate) fn channel_recv(
        &mut self,
        handle: ChannelHandle,
        coroutine: CoroutineHandle,
    ) -> Result<RecvOutcome, RawInvariant> {
        let outcome = self
            .channels
            .recv(&mut self.wait, &mut self.controls, handle, coroutine)?;
        match outcome {
            RecvOutcome::Value { wake, .. } => {
                if let Some(node) = wake {
                    self.wake_node(node)?;
                }
                Ok(outcome)
            }
            RecvOutcome::Closed => Ok(outcome),
            RecvOutcome::Parked(_) if self.park_current_wait(coroutine)? => Ok(outcome),
            RecvOutcome::Parked(_) => match self.wait.take_delivered(coroutine) {
                Some(WaitResult::Recv(payload)) => Ok(RecvOutcome::Value {
                    payload,
                    wake: None,
                }),
                Some(WaitResult::RecvClosed) => Ok(RecvOutcome::Closed),
                _ => Err(RawInvariant::new("recv 完成缺少接收结果")),
            },
        }
    }

    pub(crate) fn channel_close(&mut self, handle: ChannelHandle) -> Result<(), RawInvariant> {
        let woken = self
            .channels
            .close(&mut self.wait, &mut self.controls, handle)?;
        self.wake_nodes(woken)
    }

    pub(crate) fn join_wait(
        &mut self,
        handle: CoroutineHandle,
        waiter: CoroutineHandle,
    ) -> Result<JoinOutcome, RawInvariant> {
        let outcome = self.begin_join_wait(handle, waiter)?;
        if matches!(outcome, JoinOutcome::Parked(_)) && !self.park_current_wait(waiter)? {
            match self.wait.take_delivered(waiter) {
                Some(WaitResult::Join(value)) => Ok(JoinOutcome::Completed(value)),
                _ => Err(RawInvariant::new("Join 完成缺少完成记录")),
            }
        } else {
            Ok(outcome)
        }
    }

    fn completed_join(
        &self,
        handle: CoroutineHandle,
    ) -> Result<Option<CompletionValue>, RawInvariant> {
        let (slot, cold) = self.controls.get(handle)?;
        if slot.hot.lifecycle()? == CoroutineState::Dead {
            cold.join_state.read().map(Some)
        } else {
            Ok(None)
        }
    }

    pub(super) fn begin_join_wait(
        &mut self,
        handle: CoroutineHandle,
        waiter: CoroutineHandle,
    ) -> Result<JoinOutcome, RawInvariant> {
        self.controls.get(waiter)?;
        let source = self.wait.join_source(handle)?;
        self.wait.lock(source)?;
        let completed = self.completed_join(handle);
        self.wait.unlock(source)?;
        if let Some(value) = completed? {
            return Ok(JoinOutcome::Completed(value));
        }
        let generation = self.wait.begin_wait(waiter)?;
        let node = self
            .wait
            .alloc_node(waiter, source, 0, 0, 0, 0, generation)?;
        self.wait.arm_nodes(waiter, vec![node]);
        self.wait.lock(source)?;
        let result = (|| {
            if let Some(value) = self.completed_join(handle)? {
                return Ok(JoinOutcome::Completed(value));
            }
            let hot = &self.controls.get(waiter)?.0.hot;
            hot.wait_word.store(0, std::sync::atomic::Ordering::Relaxed);
            hot.transition(CoroutineState::Running, CoroutineState::Parking)?;
            self.wait.enqueue_source(source, node)?;
            Ok(JoinOutcome::Parked(node))
        })();
        self.wait.unlock(source)?;
        if !matches!(result, Ok(JoinOutcome::Parked(_))) {
            self.cleanup_waiters(waiter)?;
        }
        result
    }

    pub(crate) fn wake_join(&mut self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        let (slot, cold) = self.controls.get(handle)?;
        if slot.hot.lifecycle()? != CoroutineState::Dead
            || cold
                .join_state
                .status
                .load(std::sync::atomic::Ordering::Acquire)
                & 3
                == 0
        {
            return Err(RawInvariant::new("Join 唤醒要求已发布的 Dead 完成记录"));
        }
        let source = self.wait.join_source(handle)?;
        self.wait.lock(source)?;
        let result = (|| {
            let mut queue = self.wait.take_source_queue(source)?;
            let mut nodes = Vec::new();
            while let Some(node) = self.wait.dequeue(&mut queue)? {
                if self
                    .wait
                    .try_claim(&mut self.controls, source, None, Some(node))?
                {
                    let value = self
                        .controls
                        .get(handle)?
                        .1
                        .join_state
                        .read()
                        .expect("Dead 完成记录已预检");
                    let waiter = self.wait.coroutine_of(node).expect("已认领 Join 节点有效");
                    self.wait.publish_result(waiter, WaitResult::Join(value));
                    nodes.push(node);
                }
            }
            Ok::<_, RawInvariant>(nodes)
        })();
        self.wait.unlock(source)?;
        self.wake_nodes(result?)
    }

    pub(crate) fn select(
        &mut self,
        coroutine: CoroutineHandle,
        cases: &[SelectCase],
        has_default: bool,
    ) -> Result<SelectOutcome, RawInvariant> {
        let mut rng = SelectRng::from_cold(self.controls.get(coroutine)?.1.select_rng);
        let result = select_commit(
            &mut self.wait,
            &mut self.channels,
            &mut self.controls,
            cases,
            has_default,
            &mut rng,
            coroutine,
        );
        self.controls.get_mut(coroutine)?.1.select_rng = rng.s;
        let (mut outcome, wake) = match result {
            Ok(result) => result,
            Err(error) => {
                self.cleanup_waiters(coroutine)?;
                return Err(error);
            }
        };
        if let Some(node) = wake {
            self.wake_node(node)?;
        }
        if matches!(outcome, SelectOutcome::Parked | SelectOutcome::Never) {
            if !self.park_current_wait(coroutine)? {
                outcome = arm_select(&mut self.wait, &mut self.controls, coroutine)?;
            }
        } else {
            self.cleanup_waiters(coroutine)?;
        }
        Ok(outcome)
    }

    /// 跨 owner 归还 wait-node：只带 descriptor/unit/generation/bytes/integrity。
    pub(crate) fn return_wait_node(
        &mut self,
        owner: u32,
        target: u32,
    ) -> Result<RawSlot, RawInvariant> {
        let class = RuntimeSizeClassId::from_raw(0);
        let stride = self
            .classes()
            .get(class)
            .ok_or_else(|| RawInvariant::new("wait-node class 未登记"))?
            .slot_stride;
        let allocation = self.allocate(owner, class)?;
        self.queue_return(owner, allocation.slot, u64::from(stride))?;
        let token = self.token(target);
        let message = self.message(token, ReturnKind::WaitNode, allocation.slot, stride)?;
        let shard = super::super::inbox::ShardIndex::from_raw(0)
            .ok_or_else(|| RawInvariant::new("wait-node shard"))?;
        self.publish_message(
            owner,
            &message,
            shard,
            Some(super::super::message::FlushTrigger::OwnerPressure),
        )?;
        Ok(allocation.slot)
    }
}
