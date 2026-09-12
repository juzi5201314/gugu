//! 等待协议接到 `RawWorld`：channel / Join wait / select 只经 `ready_publish` 唤醒。

use super::super::channel::{ChannelHandle, RecvOutcome, SendOutcome, TryRecvErr, TrySendErr};
use super::super::coroutine::{CompletionValue, CoroutineHandle, CoroutineState};
use super::super::message::ReturnKind;
use super::super::select::{SelectCase, SelectOutcome, SelectRng, select_commit};
use super::super::size_class::RuntimeSizeClassId;
use super::super::slab::{RawInvariant, RawSlot};
use super::super::wait::{WaitNodeHandle, park_wait, wake_wait};
use super::RawWorld;

impl RawWorld {
    fn wait_processor(&self) -> Result<u64, RawInvariant> {
        self.scheduler
            .active_snapshot()
            .first()
            .copied()
            .ok_or_else(|| RawInvariant::new("没有 active processor 可唤醒"))
    }

    pub(crate) fn wake_node(&mut self, node: WaitNodeHandle) -> Result<bool, RawInvariant> {
        if !self.wait.is_live(node) {
            return Ok(false);
        }
        let handle = self.wait.coroutine_of(node)?;
        let mut nodes = self.wait.take_armed(handle);
        if nodes.is_empty() {
            nodes.push(node);
        }
        let mut woken = false;
        if self.wait.mark_ready(node)? {
            let processor = self.wait_processor()?;
            let mut producer = super::super::scheduler::ProducerHandle::new(1);
            woken = wake_wait(
                &mut self.scheduler,
                &mut self.controls,
                &mut producer,
                processor,
                handle,
                true,
            )?;
        }
        for candidate in nodes {
            if self.wait.is_live(candidate) {
                let source = self.wait.source_of(candidate)?;
                let _ = self.channels.unlink_waiter(&mut self.wait, candidate)?;
                let _ = self.wait.unlink_source(source, candidate)?;
                self.wait.release_node(candidate)?;
            }
        }
        Ok(woken)
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
        match self.channels.try_send(&mut self.wait, handle, payload)? {
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
        match self.channels.try_recv(&mut self.wait, handle)? {
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
        let outcome = self
            .channels
            .send(&mut self.wait, handle, coroutine, payload, large)?;
        match outcome {
            SendOutcome::Sent { wake } => {
                if let Some(node) = wake {
                    self.wake_node(node)?;
                }
            }
            SendOutcome::Parked(_) => park_wait(&mut self.controls, coroutine)?,
        }
        Ok(outcome)
    }

    pub(crate) fn channel_recv(
        &mut self,
        handle: ChannelHandle,
        coroutine: CoroutineHandle,
    ) -> Result<RecvOutcome, RawInvariant> {
        let outcome = self.channels.recv(&mut self.wait, handle, coroutine)?;
        match outcome {
            RecvOutcome::Value { wake, .. } => {
                if let Some(node) = wake {
                    self.wake_node(node)?;
                }
            }
            RecvOutcome::Parked(_) => park_wait(&mut self.controls, coroutine)?,
            RecvOutcome::Closed => {}
        }
        Ok(outcome)
    }

    pub(crate) fn channel_close(&mut self, handle: ChannelHandle) -> Result<(), RawInvariant> {
        let woken = self.channels.close(&mut self.wait, handle)?;
        self.wake_nodes(woken)
    }

    pub(crate) fn join_wait(
        &mut self,
        handle: CoroutineHandle,
        waiter: CoroutineHandle,
    ) -> Result<Result<CompletionValue, WaitNodeHandle>, RawInvariant> {
        let lifecycle = self.controls.get(handle)?.0.hot.lifecycle()?;
        if lifecycle == CoroutineState::Dead {
            return Ok(Ok(self.controls.get(handle)?.1.join_state.read()?));
        }
        let source = self.wait.join_source(handle)?;
        let generation = self.wait.begin_wait(waiter)?;
        let node = self
            .wait
            .alloc_node(waiter, source, 0, 0, 0, 0, generation)?;
        self.wait.arm_nodes(waiter, vec![node]);
        self.wait.enqueue_source(source, node)?;
        park_wait(&mut self.controls, waiter)?;
        Ok(Err(node))
    }

    pub(crate) fn wake_join(&mut self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        let source = self.wait.join_source(handle)?;
        let mut queue = self.wait.take_source_queue(source)?;
        let mut nodes = Vec::new();
        while let Some(node) = self.wait.dequeue(&mut queue)? {
            nodes.push(node);
        }
        self.wake_nodes(nodes)
    }

    pub(crate) fn select(
        &mut self,
        coroutine: CoroutineHandle,
        cases: &[SelectCase],
        has_default: bool,
    ) -> Result<SelectOutcome, RawInvariant> {
        let mut completed = Vec::new();
        for case in cases {
            if let super::super::select::SelectOp::Wait { join } = case.op {
                let done = self.controls.get(join)?.0.hot.lifecycle()? == CoroutineState::Dead;
                completed.push((join, done));
            }
        }
        let (slot, cold) = self.controls.get_mut(coroutine)?;
        let _ = slot;
        let mut rng = SelectRng::from_cold(cold.select_rng);
        let (outcome, txn) = select_commit(
            &mut self.wait,
            &mut self.channels,
            &completed,
            cases,
            has_default,
            &mut rng,
            coroutine,
        )?;
        let (_, cold) = self.controls.get_mut(coroutine)?;
        cold.select_rng = rng.s;
        cold.select_scratch = txn.to_cold();
        if matches!(outcome, SelectOutcome::Parked | SelectOutcome::Never) {
            park_wait(&mut self.controls, coroutine)?;
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
        let mut staging = super::super::message::ProducerStaging::new(
            super::super::message::BatchLimits::default(),
        );
        self.publish_message(
            &mut staging,
            &message,
            shard,
            Some(super::super::message::FlushTrigger::OwnerPressure),
        )?;
        Ok(allocation.slot)
    }
}
