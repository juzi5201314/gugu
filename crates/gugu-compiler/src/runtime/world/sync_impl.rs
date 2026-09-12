//! 同步协议接入 `RawWorld`：锁、条件变量、OnceLock、Lazy 与取消。

use super::super::channel::ChannelHandle;
use super::super::coroutine::CoroutineHandle;
use super::super::slab::RawInvariant;
use super::super::sync::{
    CancelHandle, Cancelled, CondvarHandle, MutexHandle, MutexLockOutcome, OnceHandle,
    OnceInitAction, RwLockHandle,
};
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
    ) -> Result<Vec<u64>, RawInvariant> {
        let cancel = self
            .sync
            .cancels
            .get_mut(handle.0)
            .ok_or_else(|| RawInvariant::new("未知 CancelSource 句柄"))?;
        Ok(cancel.cancel())
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
    ) -> Result<Result<u64, Cancelled>, RawInvariant> {
        if self.cancel_token_is_cancelled(cancel)? {
            return Ok(Err(Cancelled));
        }
        match self.channels.try_recv(&mut self.wait, channel)? {
            Ok((payload, wake)) => {
                if let Some(node) = wake {
                    self.wake_node(node)?;
                }
                Ok(Ok(payload))
            }
            Err(_) => {
                // 如果取消源在等待过程中被触发，则直接返回 Cancelled，不污染通道队列。
                if self.cancel_token_is_cancelled(cancel)? {
                    Ok(Err(Cancelled))
                } else {
                    let outcome = self.channels.recv(&mut self.wait, channel, coroutine)?;
                    match outcome {
                        super::super::channel::RecvOutcome::Value { payload, wake } => {
                            if let Some(node) = wake {
                                self.wake_node(node)?;
                            }
                            Ok(Ok(payload))
                        }
                        super::super::channel::RecvOutcome::Parked(_) => {
                            // 协程挂起，若取消被触发则注销
                            if self.cancel_token_is_cancelled(cancel)? {
                                Ok(Err(Cancelled))
                            } else {
                                Ok(Ok(0))
                            }
                        }
                        super::super::channel::RecvOutcome::Closed => Ok(Ok(0)),
                    }
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
    ) -> Result<Result<u64, Cancelled>, RawInvariant> {
        if self.cancel_token_is_cancelled(cancel)? {
            return Ok(Err(Cancelled));
        }
        let res = self.join_wait(join, waiter)?;
        match res {
            Ok(completion) => match completion {
                super::super::coroutine::CompletionValue::Bits(val) => Ok(Ok(val)),
                super::super::coroutine::CompletionValue::Managed { handle, .. } => Ok(Ok(handle)),
                super::super::coroutine::CompletionValue::Panic { .. } => Ok(Ok(0)),
            },
            Err(node) => {
                if self.cancel_token_is_cancelled(cancel)? {
                    // 安全注销等待节点，且绝不杀死目标子协程
                    let source = self.wait.join_source(join)?;
                    let _ = self.wait.unlink_source(source, node)?;
                    let _ = self.wait.release_node(node)?;
                    Ok(Err(Cancelled))
                } else {
                    Ok(Ok(0))
                }
            }
        }
    }
}
