//! 协程控制块与raw world的唯一接缝：创建、safepoint复制、完成与owner return。

use std::sync::atomic::Ordering;

use super::super::coroutine::{
    CompletionValue, CoroutineContext, CoroutineHandle, CoroutineState, STACK_SCAN_LOCKED,
    StackDescriptor,
};
use super::super::inbox::ShardIndex;
use super::super::message::{
    IntegrityTag, MessageState, ReturnKind, ReturnMessage, ReturnNodeId, StagedChain,
};
use super::super::size_class::RuntimeSizeClassId;
use super::super::slab::{RawInvariant, Resolution, SlabDescriptorId, SlabGeneration};
use super::super::stack::{
    StackError, StackImage, growth_capacity, initial_capacity, record_resize, shrink_capacity,
};
use super::super::stack_arena::{StackHandle, StackStats};
use super::RawWorld;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CoroutineEntry {
    pub(crate) pc: usize,
    pub(crate) required_frame: usize,
}

/// 仅保存allocator租约与替身内存，不复制hot/cold中的调度、context或完成状态。
#[derive(Debug)]
pub(super) struct CoroutineStorage {
    pub(super) stack: Option<StackHandle>,
    pub(super) image: StackImage,
}

/// 完成trampoline已发布payload；只有system-stack侧能消费此票据。
#[derive(Debug)]
pub(crate) struct FinishTicket {
    coroutine: CoroutineHandle,
    stack: StackHandle,
}

impl RawWorld {
    pub(super) fn stack_failure(&mut self, error: StackError) -> RawInvariant {
        if self.rt0.is_some() {
            let _ = self.fatal(error.fatal(), error.to_string(), None);
        }
        RawInvariant::new(error.to_string())
    }

    pub(super) fn create_coroutine(
        &mut self,
        owner: u32,
        entry: CoroutineEntry,
        limit: u64,
    ) -> Result<CoroutineHandle, RawInvariant> {
        if entry.pc == 0 || usize::try_from(owner).expect("owner下标") >= self.owners.len() {
            return Err(RawInvariant::new("协程entry或owner未登记"));
        }
        let capacity = initial_capacity(
            entry.required_frame,
            usize::try_from(limit).map_err(|_| self.stack_failure(StackError::Overflow))?,
        )
        .map_err(|error| self.stack_failure(error))?;
        let stack = self
            .stacks
            .reserve(owner, capacity, &mut self.provider)
            .map_err(|error| self.stack_failure(error))?;
        let handle = match self.controls.allocate() {
            Ok(handle) => handle,
            Err(error) => {
                self.stacks
                    .release_global(stack, &mut self.provider)
                    .map_err(|failure| self.stack_failure(failure))?;
                return Err(error);
            }
        };
        let (low, capacity) = self
            .stacks
            .bounds(stack)
            .map_err(|error| self.stack_failure(error))?;
        let (slot, cold) = self.controls.get_mut(handle)?;
        slot.stack.install(low, capacity)?;
        cold.context = CoroutineContext {
            rsp: low + capacity,
            rip: entry.pc,
            ..CoroutineContext::default()
        };
        slot.hot
            .transition(CoroutineState::New, CoroutineState::Runnable)?;
        let index = usize::try_from(handle.index).expect("控制块下标");
        if self.coroutine_storage.len() <= index {
            self.coroutine_storage.resize_with(index + 1, || None);
        }
        self.coroutine_storage[index] = Some(CoroutineStorage {
            stack: Some(stack),
            image: StackImage::default(),
        });
        Ok(handle)
    }

    /// 调度者取得context前Acquire认领完整状态；第一次切入才commit宿主页。
    pub(crate) fn enter_coroutine(
        &mut self,
        handle: CoroutineHandle,
    ) -> Result<CoroutineContext, RawInvariant> {
        let (slot, _) = self.controls.get(handle)?;
        if slot.hot.lifecycle()? != CoroutineState::Runnable {
            return Err(RawInvariant::new("只有Runnable协程可以恢复"));
        }
        let stack = self.coroutine_stack(handle)?;
        self.stacks
            .commit(stack, &mut self.provider)
            .map_err(|error| self.stack_failure(error))?;
        let (slot, cold) = self.controls.get(handle)?;
        slot.hot
            .transition(CoroutineState::Runnable, CoroutineState::Running)?;
        Ok(cold.context)
    }

    fn coroutine_stack(&self, handle: CoroutineHandle) -> Result<StackHandle, RawInvariant> {
        self.controls.get(handle)?;
        self.coroutine_storage
            .get(usize::try_from(handle.index).expect("控制块下标"))
            .and_then(Option::as_ref)
            .and_then(|storage| storage.stack)
            .ok_or_else(|| RawInvariant::new("协程已经摘除stack root"))
    }

    /// 替身保存精确safepoint内存；真实机器路径由context fragment保存寄存器。
    pub(crate) fn save_coroutine(
        &mut self,
        handle: CoroutineHandle,
        context: CoroutineContext,
        image: StackImage,
        waiting: bool,
    ) -> Result<(), RawInvariant> {
        let (slot, cold) = self.controls.get_mut(handle)?;
        if slot.hot.lifecycle()? != CoroutineState::Running
            || context.rip == 0
            || slot.hot.state.load(Ordering::Acquire) & STACK_SCAN_LOCKED != 0
            || slot.stack.used(context.rsp)? != image.bytes.len()
        {
            return Err(RawInvariant::new("safepoint保存没有稳定的Running context"));
        }
        cold.context = context;
        slot.stack.recent_high_water = slot.stack.recent_high_water.max(image.bytes.len());
        self.coroutine_storage[usize::try_from(handle.index).expect("控制块下标")]
            .as_mut()
            .expect("storage")
            .image = image;
        if waiting {
            slot.hot
                .transition(CoroutineState::Running, CoroutineState::Parking)?;
            slot.hot
                .transition(CoroutineState::Parking, CoroutineState::Waiting)
        } else {
            slot.hot
                .transition(CoroutineState::Running, CoroutineState::Runnable)
        }
    }

    /// 先由scheduler处理poll/GC，再带最新context进入此system-stack复制入口。
    pub(crate) fn grow_coroutine(
        &mut self,
        handle: CoroutineHandle,
        required: usize,
        epoch: u32,
    ) -> Result<(), RawInvariant> {
        let limit = self
            .rt0_config()?
            .ok_or_else(|| RawInvariant::new("栈增长缺少启动配置"))?
            .stack_max();
        let (slot, cold) = self.controls.get(handle)?;
        let used = slot.stack.used(cold.context.rsp)?;
        let capacity = growth_capacity(
            slot.stack.capacity,
            used,
            required,
            usize::try_from(limit).expect("逻辑栈上限适配目标"),
        )
        .map_err(|error| self.stack_failure(error))?;
        self.resize_coroutine(handle, capacity, epoch, true)
    }

    pub(crate) fn shrink_coroutine(
        &mut self,
        handle: CoroutineHandle,
        epoch: u32,
        pressure: bool,
    ) -> Result<bool, RawInvariant> {
        let (slot, cold) = self.controls.get_mut(handle)?;
        if slot.hot.state.load(Ordering::Acquire) & STACK_SCAN_LOCKED != 0 {
            return Ok(false);
        }
        if u64::from(epoch) <= cold.gc_scan_epoch.load(Ordering::Acquire) {
            return Ok(false);
        }
        cold.gc_scan_epoch
            .store(u64::from(epoch), Ordering::Release);
        let state = slot.hot.lifecycle()?;
        let used = slot.stack.used(cold.context.rsp)?;
        let Some(capacity) = shrink_capacity(&mut slot.stack, state, used, epoch, pressure) else {
            return Ok(false);
        };
        self.resize_coroutine(handle, capacity, epoch, false)?;
        Ok(true)
    }

    fn resize_coroutine(
        &mut self,
        handle: CoroutineHandle,
        capacity: usize,
        epoch: u32,
        grew: bool,
    ) -> Result<(), RawInvariant> {
        let (slot, _) = self.controls.get(handle)?;
        let old_state = slot.hot.state.load(Ordering::Acquire);
        if !matches!(
            slot.hot.lifecycle()?,
            CoroutineState::Runnable | CoroutineState::Waiting
        ) || old_state & STACK_SCAN_LOCKED != 0
        {
            return Err(RawInvariant::new("复制栈必须持有已停在safepoint的协程"));
        }
        slot.hot
            .state
            .compare_exchange(
                old_state,
                old_state | STACK_SCAN_LOCKED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| RawInvariant::new("复制栈未取得scan lock"))?;
        let result = self.copy_locked_stack(handle, capacity, epoch, grew);
        self.controls
            .get(handle)?
            .0
            .hot
            .state
            .store(old_state, Ordering::Release);
        result
    }

    fn copy_locked_stack(
        &mut self,
        handle: CoroutineHandle,
        capacity: usize,
        epoch: u32,
        grew: bool,
    ) -> Result<(), RawInvariant> {
        let old_stack = self.coroutine_stack(handle)?;
        let owner = self
            .stacks
            .owner(old_stack)
            .map_err(|error| self.stack_failure(error))?;
        let new_stack = self
            .stacks
            .acquire(owner, capacity, &mut self.provider)
            .map_err(|error| self.stack_failure(error))?;
        let (low, _) = self
            .stacks
            .bounds(new_stack)
            .map_err(|error| self.stack_failure(error))?;
        let index = usize::try_from(handle.index).expect("控制块下标");
        let (slot, cold) = self.controls.get(handle)?;
        let copied = self.coroutine_storage[index]
            .as_ref()
            .expect("storage")
            .image
            .relocate(cold.context, &slot.stack, low, capacity);
        let (image, context) = match copied {
            Ok(copied) => copied,
            Err(error) => {
                self.stacks
                    .release_global(new_stack, &mut self.provider)
                    .map_err(|failure| self.stack_failure(failure))?;
                return Err(self.stack_failure(error));
            }
        };
        let (slot, cold) = self.controls.get_mut(handle)?;
        slot.stack.install(low, capacity)?;
        record_resize(&mut slot.stack, epoch, grew);
        cold.context = context;
        self.coroutine_storage[index] = Some(CoroutineStorage {
            stack: Some(new_stack),
            image,
        });
        self.stacks
            .recycle(old_stack, owner, &mut self.provider)
            .map_err(|error| self.stack_failure(error))
    }

    /// 在旧栈上完成result/panic transfer；完成记录的Release早于任何stack摘根。
    pub(crate) fn stage_coroutine_finish(
        &mut self,
        handle: CoroutineHandle,
        value: CompletionValue,
    ) -> Result<FinishTicket, RawInvariant> {
        let stack = self.coroutine_stack(handle)?;
        let (slot, cold) = self.controls.get_mut(handle)?;
        if slot.hot.lifecycle()? != CoroutineState::Running {
            return Err(RawInvariant::new("只有Running协程能完成body"));
        }
        cold.join_state.publish(value, |payload, descriptor| {
            self.completion_barriers.push((handle, payload, descriptor));
        })?;
        Ok(FinishTicket {
            coroutine: handle,
            stack,
        })
    }

    /// 单向切到system stack后调用；票据不包含旧rsp，函数也不读取旧stack bytes。
    pub(crate) fn finish_coroutine_on_system(
        &mut self,
        ticket: &FinishTicket,
        owner: u32,
    ) -> Result<(), RawInvariant> {
        let (slot, cold) = self.controls.get_mut(ticket.coroutine)?;
        let state = slot.hot.state.load(Ordering::Acquire);
        if CoroutineState::from_word(state)? != CoroutineState::Running
            || state & STACK_SCAN_LOCKED != 0
            || cold.join_state.status.load(Ordering::Acquire) == 0
        {
            return Err(RawInvariant::new("完成路径尚未取得stack scanner所有权"));
        }
        slot.hot
            .state
            .compare_exchange(
                state,
                state | STACK_SCAN_LOCKED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| RawInvariant::new("完成路径丢失scan lock"))?;
        cold.context = CoroutineContext::default();
        slot.stack = StackDescriptor::default();
        let storage = self.coroutine_storage
            [usize::try_from(ticket.coroutine.index).expect("控制块下标")]
        .as_mut()
        .expect("storage");
        if storage.stack.take() != Some(ticket.stack) {
            return Err(RawInvariant::new("完成票据不是当前stack"));
        }
        storage.image = StackImage::default();
        self.return_stack(ticket.stack, owner)?;
        self.controls.get(ticket.coroutine)?.0.hot.state.store(
            (state & !255) | CoroutineState::Dead as u64,
            Ordering::Release,
        );
        if self
            .controls
            .get(ticket.coroutine)?
            .1
            .join_state
            .join_leases
            == 0
        {
            self.controls.release(ticket.coroutine)?;
            self.coroutine_storage[usize::try_from(ticket.coroutine.index).expect("控制块下标")] =
                None;
        }
        Ok(())
    }

    fn return_stack(&mut self, stack: StackHandle, owner: u32) -> Result<(), RawInvariant> {
        let source = self
            .stacks
            .owner(stack)
            .map_err(|error| self.stack_failure(error))?;
        let token = self.token(source);
        let target = match self.directory.resolve(&token) {
            Resolution::Match => token,
            Resolution::Forward(target) => target,
            _ => return Err(RawInvariant::new("stack owner缺少有效归还路由")),
        };
        if self.token(owner) == target {
            self.stacks
                .adopt(stack, owner)
                .map_err(|error| self.stack_failure(error))?;
            return self
                .stacks
                .recycle(stack, owner, &mut self.provider)
                .map_err(|error| self.stack_failure(error));
        }
        let (_, capacity) = self
            .stacks
            .bounds(stack)
            .map_err(|error| self.stack_failure(error))?;
        let bytes = u32::try_from(capacity)
            .map_err(|_| RawInvariant::new("stack return bytes越过消息车道"))?;
        let class = RuntimeSizeClassId::from_raw(
            u16::try_from(capacity.trailing_zeros() - 9).expect("stack class"),
        );
        let inbox = self.inbox_for(&target)?;
        let node = self.pool.allocate()?;
        self.stacks
            .mark_pending(stack)
            .map_err(|error| self.stack_failure(error))?;
        let mut message = ReturnMessage {
            next: None,
            target,
            kind: ReturnKind::StackSpan,
            descriptor: SlabDescriptorId::from_raw(stack.index),
            unit: 0,
            bytes,
            source_epoch: self.epoch,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(stack.generation),
                class,
                owner_id: target.owner_id,
                route_key: target.route_key,
                checksum: 0,
            },
        };
        message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
        self.pool.store(node, &message, message.integrity.checksum);
        self.pool.link(node, None);
        inbox.publish_batch(
            &StagedChain {
                first: node,
                last: node,
                count: 1,
                bytes: u64::from(bytes),
                target: Some(target),
                shard: Some(ShardIndex::from_raw(0).expect("shard0")),
            },
            &self.pool,
        )
    }

    pub(super) fn load_stack_return(
        &self,
        node: ReturnNodeId,
    ) -> Result<ReturnMessage, RawInvariant> {
        let index = self.pool.descriptor_of(node).raw();
        let (handle, capacity) = self
            .stacks
            .return_identity(index)
            .map_err(|error| RawInvariant::new(error.to_string()))?;
        let class = RuntimeSizeClassId::from_raw(
            u16::try_from(capacity.trailing_zeros() - 9).expect("stack class"),
        );
        Ok(self
            .pool
            .load(node, class, SlabGeneration::from_raw(handle.generation)))
    }

    pub(super) fn service_stack_return(
        &mut self,
        owner: u32,
        message: &ReturnMessage,
    ) -> Result<(), RawInvariant> {
        if message.integrity.checksum != IntegrityTag::compute(&self.integrity_secret, message)
            || message.unit != 0
            || message.source_epoch > self.epoch
        {
            return Err(RawInvariant::new("stack return身份、generation或epoch非法"));
        }
        match self.directory.resolve(&message.target) {
            Resolution::Forward(target) => self.forward_message(message, target),
            Resolution::Match => {
                if self.token(owner) != message.target {
                    return Err(RawInvariant::new("stack return投递到错误owner"));
                }
                let handle = StackHandle {
                    index: message.descriptor.raw(),
                    generation: message.integrity.generation.raw(),
                };
                let (_, capacity) = self
                    .stacks
                    .bounds(handle)
                    .map_err(|error| self.stack_failure(error))?;
                if u64::from(message.bytes) != u64::try_from(capacity).expect("capacity") {
                    return Err(RawInvariant::new("stack return bytes不匹配"));
                }
                self.stacks
                    .reclaim_pending(handle, owner, &mut self.provider)
                    .map_err(|error| self.stack_failure(error))
            }
            _ => Err(RawInvariant::new("stack return缺少活动或转发owner")),
        }
    }

    pub(crate) fn stack_stats(&self) -> StackStats {
        self.stacks.stats()
    }

    pub(crate) fn clone_join(&mut self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        let (_, cold) = self.controls.get_mut(handle)?;
        if cold.join_state.join_leases == 0 {
            return Err(RawInvariant::new("没有活跃Join可以复制"));
        }
        cold.join_state.join_leases = cold
            .join_state
            .join_leases
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("Join lease溢出"))?;
        Ok(())
    }

    pub(crate) fn read_completion(
        &self,
        handle: CoroutineHandle,
    ) -> Result<CompletionValue, RawInvariant> {
        let (slot, cold) = self.controls.get(handle)?;
        if slot.hot.lifecycle()? != CoroutineState::Dead {
            return Err(RawInvariant::new("Join只能读取已完成协程"));
        }
        cold.join_state.read()
    }

    pub(crate) fn release_join(&mut self, handle: CoroutineHandle) -> Result<(), RawInvariant> {
        let (slot, cold) = self.controls.get_mut(handle)?;
        cold.join_state.join_leases = cold
            .join_state
            .join_leases
            .checked_sub(1)
            .ok_or_else(|| RawInvariant::new("Join lease重复归还"))?;
        if cold.join_state.join_leases == 0 && slot.hot.lifecycle()? == CoroutineState::Dead {
            self.controls.release(handle)?;
            self.coroutine_storage[usize::try_from(handle.index).expect("控制块下标")] = None;
        }
        Ok(())
    }

    pub(super) fn shutdown_stacks(&mut self) -> Result<(), RawInvariant> {
        // Terminating停止调度后不执行用户cleanup，只撤销raw所有权并按设施顺序释放reservation。
        for storage in self.coroutine_storage.iter_mut().flatten() {
            if let Some(stack) = storage.stack.take() {
                self.stacks
                    .release_global(stack, &mut self.provider)
                    .map_err(|error| RawInvariant::new(error.to_string()))?;
            }
        }
        for owner in 0..self.owners.len() {
            self.stacks
                .trim_cache(u32::try_from(owner).expect("owner"), 0, &mut self.provider)
                .map_err(|error| RawInvariant::new(error.to_string()))?;
        }
        self.stacks
            .shutdown(&mut self.provider)
            .map_err(|error| RawInvariant::new(error.to_string()))?;
        self.coroutine_storage.clear();
        self.controls = Default::default();
        Ok(())
    }
}
