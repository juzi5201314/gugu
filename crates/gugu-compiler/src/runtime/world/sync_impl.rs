//! 同步协议接入 `RawWorld`：锁、条件变量、OnceLock、Lazy 与取消。

use super::super::channel::{ChannelHandle, RecvOutcome};
use super::super::coroutine::CoroutineHandle;
use super::super::slab::RawInvariant;
use super::super::sync::{
    CancelHandle, Cancelled, CondvarHandle, MutexHandle, MutexLockOutcome, OnceHandle,
    OnceInitAction, RwLockHandle,
};
use super::super::wait::{JoinOutcome, WaitNodeHandle, WaitResult};
use super::RawWorld;

impl RawWorld {
    pub(crate) fn mutex_new(&mut self) -> MutexHandle {
        self.sync.create_mutex()
    }

    pub(crate) fn mutex_lock(
        &mut self,
        handle: MutexHandle,
        coroutine: CoroutineHandle,
    ) -> Result<MutexLockOutcome, RawInvariant> {
        let node_id = self.sync.next_wait_node();
        let mutex = self
            .sync
            .mutexes
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 Mutex 句柄"))?;
        Ok(mutex.lock(u64::from(coroutine.index), node_id))
    }

    pub(crate) fn mutex_unlock(
        &mut self,
        handle: MutexHandle,
        coroutine: CoroutineHandle,
    ) -> Result<(), RawInvariant> {
        let mutex = self
            .sync
            .mutexes
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 Mutex 句柄"))?;
        mutex
            .unlock(u64::from(coroutine.index))
            .map_err(|e| RawInvariant::new(e.message()))?;
        Ok(())
    }

    pub(crate) fn rwlock_new(&mut self) -> RwLockHandle {
        self.sync.create_rwlock()
    }

    pub(crate) fn rwlock_read(
        &mut self,
        handle: RwLockHandle,
        coroutine: CoroutineHandle,
    ) -> Result<MutexLockOutcome, RawInvariant> {
        let node_id = self.sync.next_wait_node();
        let rwlock = self
            .sync
            .rwlocks
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 RwLock 句柄"))?;
        Ok(rwlock.read(u64::from(coroutine.index), node_id))
    }

    pub(crate) fn rwlock_write(
        &mut self,
        handle: RwLockHandle,
        coroutine: CoroutineHandle,
    ) -> Result<MutexLockOutcome, RawInvariant> {
        let node_id = self.sync.next_wait_node();
        let rwlock = self
            .sync
            .rwlocks
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 RwLock 句柄"))?;
        Ok(rwlock.write(u64::from(coroutine.index), node_id))
    }

    pub(crate) fn rwlock_unlock_read(
        &mut self,
        handle: RwLockHandle,
        coroutine: CoroutineHandle,
    ) -> Result<(), RawInvariant> {
        let rwlock = self
            .sync
            .rwlocks
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 RwLock 句柄"))?;
        rwlock
            .unlock_read(u64::from(coroutine.index))
            .map_err(|e| RawInvariant::new(e.message()))?;
        Ok(())
    }

    pub(crate) fn rwlock_unlock_write(
        &mut self,
        handle: RwLockHandle,
        coroutine: CoroutineHandle,
    ) -> Result<(), RawInvariant> {
        let rwlock = self
            .sync
            .rwlocks
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 RwLock 句柄"))?;
        rwlock
            .unlock_write(u64::from(coroutine.index))
            .map_err(|e| RawInvariant::new(e.message()))?;
        Ok(())
    }

    pub(crate) fn condvar_new(&mut self) -> CondvarHandle {
        self.sync.create_condvar()
    }

    pub(crate) fn condvar_wait(
        &mut self,
        handle: CondvarHandle,
        mutex: MutexHandle,
        coroutine: CoroutineHandle,
    ) -> Result<(), RawInvariant> {
        // 原子释放 mutex 并在 condvar 上挂起。
        self.mutex_unlock(mutex, coroutine)?;
        let node_id = self.sync.next_wait_node();
        let condvar = self
            .sync
            .condvars
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 Condvar 句柄"))?;
        condvar.wait(u64::from(coroutine.index), node_id);
        Ok(())
    }

    pub(crate) fn condvar_notify_one(
        &mut self,
        handle: CondvarHandle,
    ) -> Result<Option<u64>, RawInvariant> {
        let condvar = self
            .sync
            .condvars
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 Condvar 句柄"))?;
        Ok(condvar.notify_one().map(|(coro, _)| coro))
    }

    pub(crate) fn condvar_notify_all(
        &mut self,
        handle: CondvarHandle,
    ) -> Result<Vec<u64>, RawInvariant> {
        let condvar = self
            .sync
            .condvars
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 Condvar 句柄"))?;
        Ok(condvar
            .notify_all()
            .into_iter()
            .map(|(coro, _)| coro)
            .collect())
    }

    pub(crate) fn once_new(&mut self) -> OnceHandle {
        self.sync.create_once()
    }

    pub(crate) fn once_get(&self, handle: OnceHandle) -> Result<Option<u64>, RawInvariant> {
        let once = self
            .sync
            .onces
            .get(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 OnceLock 句柄"))?;
        once.get().map_err(|e| RawInvariant::new(e.message()))
    }

    pub(crate) fn once_start_init(
        &mut self,
        handle: OnceHandle,
        coroutine: CoroutineHandle,
    ) -> Result<OnceInitAction, RawInvariant> {
        let node_id = self.sync.next_wait_node();
        let once = self
            .sync
            .onces
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 OnceLock 句柄"))?;
        once.start_init(u64::from(coroutine.index), node_id)
            .map_err(|e| RawInvariant::new(e.message()))
    }

    pub(crate) fn once_finish_init(
        &mut self,
        handle: OnceHandle,
        value: u64,
    ) -> Result<Vec<u64>, RawInvariant> {
        let once = self
            .sync
            .onces
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 OnceLock 句柄"))?;
        once.finish_init(value)
            .map_err(|e| RawInvariant::new(e.message()))
    }

    pub(crate) fn once_fail_init(&mut self, handle: OnceHandle) -> Result<Vec<u64>, RawInvariant> {
        let once = self
            .sync
            .onces
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 OnceLock 句柄"))?;
        once.fail_init().map_err(|e| RawInvariant::new(e.message()))
    }

    pub(crate) fn once_set(&mut self, handle: OnceHandle, value: u64) -> Result<(), u64> {
        let once = self.sync.onces.get_mut(handle.0).expect("未知 OnceLock");
        once.set(value)
    }

    pub(crate) fn cancel_source_new(&mut self) -> CancelHandle {
        self.sync.create_cancel()
    }

    pub(crate) fn cancel_source_cancel(
        &mut self,
        handle: CancelHandle,
    ) -> Result<(), RawInvariant> {
        let nodes = self
            .sync
            .cancels
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 CancelSource 句柄"))?
            .cancel();
        for node in nodes {
            if !self.wait.is_live(node)
                || !self
                    .wait
                    .cancel_source(node)?
                    .is_some_and(|source| source.0 == handle.0)
            {
                continue;
            }
            if self.cancel_wait_node(node)? {
                self.wake_node(node)?;
            }
        }
        Ok(())
    }

    fn register_cancel_waiter(
        &mut self,
        cancel: CancelHandle,
        node: WaitNodeHandle,
    ) -> Result<(), RawInvariant> {
        let registered = self
            .sync
            .cancels
            .get_mut(cancel.0)
            .ok_or_else(|| RawInvariant::new("未知 CancelSource 句柄"))?
            .register_waiter(node);
        if registered.is_ok() {
            self.wait.set_cancel_source(node, Some(cancel))?;
        } else {
            self.cancel_wait_node(node)?;
        }
        Ok(())
    }

    fn cancel_wait_node(&mut self, node: WaitNodeHandle) -> Result<bool, RawInvariant> {
        let source = self.wait.source_of(node)?;
        self.wait.lock(source)?;
        let claimed = self
            .wait
            .try_claim(&mut self.controls, source, None, Some(node));
        if matches!(claimed, Ok(true)) {
            let coroutine = self
                .wait
                .coroutine_of(node)
                .expect("取消已认领有效等待节点");
            self.wait.publish_result(coroutine, WaitResult::Cancelled);
        }
        self.wait.unlock(source)?;
        claimed
    }

    pub(super) fn unregister_cancel_waiter(
        &mut self,
        node: WaitNodeHandle,
    ) -> Result<(), RawInvariant> {
        if let Some(cancel) = self.wait.cancel_source(node)? {
            self.sync
                .cancels
                .get_mut(cancel.0)
                .ok_or_else(|| RawInvariant::new("未知 CancelSource 句柄"))?
                .unregister_waiter(node);
            self.wait.set_cancel_source(node, None)?;
        }
        Ok(())
    }

    pub(crate) fn cancel_token_is_cancelled(
        &self,
        handle: CancelHandle,
    ) -> Result<bool, RawInvariant> {
        let cancel = self
            .sync
            .cancels
            .get(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 CancelToken 句柄"))?;
        Ok(cancel.is_cancelled())
    }

    pub(crate) fn cancel_token_check(&self, handle: CancelHandle) -> Result<(), Cancelled> {
        let cancel = self.sync.cancels.get(handle.0).ok_or(Cancelled)?;
        cancel.check()
    }

    /// 取消与 channel recv 的接缝：已取消立即返回 Err(Cancelled)；否则正常 recv。
    pub(crate) fn channel_recv_cancel(
        &mut self,
        channel: ChannelHandle,
        cancel: CancelHandle,
        coroutine: CoroutineHandle,
    ) -> Result<Result<RecvOutcome, Cancelled>, RawInvariant> {
        if self.cancel_token_is_cancelled(cancel)? {
            return Ok(Err(Cancelled));
        }
        let outcome = self
            .channels
            .recv(&mut self.wait, &mut self.controls, channel, coroutine)?;
        match outcome {
            RecvOutcome::Value { wake, .. } => {
                if let Some(node) = wake {
                    self.wake_node(node)?;
                }
                Ok(Ok(outcome))
            }
            RecvOutcome::Closed => Ok(Ok(outcome)),
            RecvOutcome::Parked(node) => {
                self.register_cancel_waiter(cancel, node)?;
                if self.park_current_wait(coroutine)? {
                    return Ok(Ok(outcome));
                }
                match self.wait.take_delivered(coroutine) {
                    Some(WaitResult::Recv(payload)) => Ok(Ok(RecvOutcome::Value {
                        payload,
                        wake: None,
                    })),
                    Some(WaitResult::RecvClosed) => Ok(Ok(RecvOutcome::Closed)),
                    Some(WaitResult::Cancelled) => Ok(Err(Cancelled)),
                    _ => Err(RawInvariant::new("可取消 recv 完成缺少接收或取消结果")),
                }
            }
        }
    }

    /// 取消与 Join wait 的接缝：取消只取消当前等待者，不终止子协程！
    pub(crate) fn join_wait_cancel(
        &mut self,
        join: CoroutineHandle,
        cancel: CancelHandle,
        waiter: CoroutineHandle,
    ) -> Result<Result<JoinOutcome, Cancelled>, RawInvariant> {
        if self.cancel_token_is_cancelled(cancel)? {
            return Ok(Err(Cancelled));
        }
        let outcome = self.begin_join_wait(join, waiter)?;
        if let JoinOutcome::Parked(node) = outcome {
            self.register_cancel_waiter(cancel, node)?;
            if self.park_current_wait(waiter)? {
                return Ok(Ok(outcome));
            }
            match self.wait.take_delivered(waiter) {
                Some(WaitResult::Join(value)) => Ok(Ok(JoinOutcome::Completed(value))),
                Some(WaitResult::Cancelled) => Ok(Err(Cancelled)),
                _ => Err(RawInvariant::new("可取消 Join 完成缺少完成或取消结果")),
            }
        } else {
            Ok(Ok(outcome))
        }
    }
}
