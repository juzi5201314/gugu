//! ResourceCell slab 与 lease 状态机在 owner-directed 世界中的接入。
//!
//! 本模块是 `RawWorld` 的资源侧实现：地址稳定的 ResourceCell 分配、lease 复制、单向发布、
//! 幂等 close、唯一 release 入队点、受限 cleanup、generation 推进与跨 owner 的
//! `ResourceRelease` 消息归还都只经这里的入口，owner 本地 free structure 与账本保持唯一
//! 真相源。

use super::super::RESOURCE_DEDICATED_ALIGN_LIMIT;
use super::super::inbox::ShardIndex;
use super::super::message::{
    FlushTrigger, ProducerStaging, ReturnKind, ReturnMessage, stage_message,
};
use super::super::owner::Allocation;
use super::super::resource::{
    self, CloseOutcome, LeaseOutcome, ReleaseRegistry, ReleaseTicket, ResourceHandle,
};
use super::super::size_class::RuntimeSizeClassTable;
use super::super::slab::{
    MemoryDomainId, OwnerToken, RawInvariant, RawSlot, SlabDescriptorId, SlotState,
};
use super::{RawWorld, ResourceShape};

impl RawWorld {
    /// 返回 Resource domain 的 class 表。
    pub(crate) const fn resource_classes(&self) -> &RuntimeSizeClassTable {
        &self.resource_classes
    }

    /// 返回统一 release 入口目录。
    pub(crate) const fn registry(&self) -> &ReleaseRegistry {
        &self.registry
    }

    /// 返回受限 cleanup 的累计次数。
    pub(crate) const fn resource_cleanups(&self) -> u64 {
        self.cells.cleanups()
    }

    /// 返回按发生顺序记录的 release。
    pub(crate) fn release_records(&self) -> &[ReleaseTicket] {
        self.cells.records()
    }

    /// 返回尚未被 worker 取走的 release 请求数。
    pub(crate) fn pending_release_requests(&self) -> usize {
        self.release_queue.len()
    }

    /// 返回某个资源 slot 的 header。
    pub(crate) fn resource_cell(
        &self,
        handle: ResourceHandle,
    ) -> Result<&resource::ResourceCell, RawInvariant> {
        self.cells.get(handle.descriptor, handle.index)
    }

    /// 返回某个资源 slot 当前的 lease 数。
    pub(crate) fn resource_leases(&self, handle: ResourceHandle) -> Result<u64, RawInvariant> {
        Ok(self.cells.get(handle.descriptor, handle.index)?.leases)
    }

    /// 校验 ResourceCell header 表与 slab 状态一致。
    pub(crate) fn verify_resource_cells(&self) -> Result<(), RawInvariant> {
        self.cells.verify(&self.table)
    }

    /// 分配一个地址稳定的 ResourceCell；超出 class 阶梯或对齐上界时使用整页 mapping。
    pub(crate) fn allocate_resource(
        &mut self,
        owner: u32,
        shape: ResourceShape,
    ) -> Result<ResourceHandle, RawInvariant> {
        let descriptor_record = self.registry.for_kind(shape.kind_id)?;
        let token = self.resource_owners[owner as usize].token();
        let usable_class = (shape.alignment <= RESOURCE_DEDICATED_ALIGN_LIMIT)
            .then(|| {
                self.resource_classes
                    .lookup(shape.payload_bytes, shape.alignment)
            })
            .flatten()
            .copied();
        let allocation = match usable_class {
            Some(class) => {
                let secret_index = u32::from(class.id.raw());
                let accounting = self
                    .directory
                    .accounting_mut(token.owner_id)
                    .ok_or_else(|| RawInvariant::new("资源分配缺少 owner 账本"))?;
                let codec = self.link_codec.clone();
                let provider = &mut self.provider;
                let table = &mut self.table;
                let slab_epoch = self.epoch;
                self.resource_owners[owner as usize].allocate(
                    &class,
                    table,
                    provider,
                    &codec,
                    accounting,
                    secret_index,
                    slab_epoch,
                )?
            }
            None => {
                let stride = resource::dedicated_stride(shape.payload_bytes, shape.alignment)?;
                let alignment = shape.alignment.max(64);
                let class = resource::dedicated_class(stride, alignment)?;
                let (range, bytes) = {
                    let accounting = self
                        .directory
                        .accounting_mut(token.owner_id)
                        .ok_or_else(|| RawInvariant::new("专用 mapping 缺少 owner 账本"))?;
                    let commit = resource::reserve_mapping(
                        &mut self.provider,
                        accounting,
                        u64::from(stride),
                        alignment,
                        self.epoch,
                    )?;
                    (commit.range, commit.bytes)
                };
                let descriptor = self.table.create(
                    &class,
                    token,
                    range,
                    bytes,
                    u32::from(class.id.raw()),
                    self.epoch,
                )?;
                self.table
                    .transition(descriptor, 0, SlotState::Returned, SlotState::Live)?;
                let record = self
                    .table
                    .descriptor_mut(descriptor)
                    .ok_or_else(|| RawInvariant::new("专用 mapping 描述符缺失"))?;
                record.live += 1;
                record.free -= 1;
                record.bump_cursor = 1;
                Allocation {
                    slot: RawSlot {
                        descriptor,
                        index: 0,
                        generation: record.generation,
                    },
                    level: super::super::owner::AllocationLevel::RangeRequest,
                }
            }
        };
        let descriptor = self
            .table
            .descriptor(allocation.slot.descriptor)
            .ok_or_else(|| RawInvariant::new("新分配的资源 slab 缺失"))?
            .clone();
        self.cells
            .ensure(allocation.slot.descriptor, descriptor.slot_count());
        self.cells.place(
            allocation.slot.descriptor,
            allocation.slot.index,
            &descriptor_record,
            shape.payload_bytes,
            shape.alignment.max(1).trailing_zeros() as u8,
            descriptor.class.raw(),
            u64::from(owner),
            descriptor.generation.raw(),
        )?;
        Ok(allocation.slot)
    }

    /// 复制资源值：增加一个 lease。
    pub(crate) fn resource_acquire(&mut self, handle: ResourceHandle) -> Result<(), RawInvariant> {
        self.cells.acquire(handle.descriptor, handle.index)
    }

    /// 发布到共享图；状态单向进入 Shared。
    pub(crate) fn resource_publish(&mut self, handle: ResourceHandle) -> Result<(), RawInvariant> {
        self.cells.publish(handle.descriptor, handle.index)
    }

    /// 显式幂等 close；返回是否首次关闭。
    pub(crate) fn resource_close(
        &mut self,
        owner: u32,
        handle: ResourceHandle,
    ) -> Result<CloseOutcome, RawInvariant> {
        let outcome = self.cells.close(handle.descriptor, handle.index)?;
        if outcome == CloseOutcome::ClosedNow {
            self.request_release(owner, handle)?;
        }
        self.try_reclaim(owner, handle)?;
        Ok(outcome)
    }

    /// 结束当前执行者持有的 lease。
    pub(crate) fn release_lease(
        &mut self,
        owner: u32,
        handle: ResourceHandle,
    ) -> Result<LeaseOutcome, RawInvariant> {
        let outcome = self.cells.release_lease(handle.descriptor, handle.index)?;
        if outcome == LeaseOutcome::LastLease {
            self.request_release(owner, handle)?;
        }
        self.try_reclaim(owner, handle)?;
        Ok(outcome)
    }

    /// detach：标记后仍走正常 release 路径结束自身 lease。
    pub(crate) fn resource_detach(
        &mut self,
        owner: u32,
        handle: ResourceHandle,
    ) -> Result<LeaseOutcome, RawInvariant> {
        self.cells.mark_detached(handle.descriptor, handle.index)?;
        self.release_lease(owner, handle)
    }

    /// 抢唯一 release 入队点，并让当前执行者充当 release worker。
    fn request_release(&mut self, owner: u32, handle: ResourceHandle) -> Result<(), RawInvariant> {
        self.enqueue_release(handle)?;
        self.drain_release_queue(owner).map(|_| ())
    }

    /// 抢 release 入队点并把 ticket 放入 release queue；已入队时返回 false。
    fn enqueue_release(&mut self, handle: ResourceHandle) -> Result<bool, RawInvariant> {
        if !self
            .cells
            .request_release(handle.descriptor, handle.index)?
        {
            return Ok(false);
        }
        let generation = self
            .table
            .descriptor(handle.descriptor)
            .ok_or_else(|| RawInvariant::new("release 引用未知资源 slab"))?
            .generation;
        let detached = self
            .cells
            .get(handle.descriptor, handle.index)?
            .is_detached();
        self.release_queue.push_back(ReleaseTicket {
            descriptor: handle.descriptor,
            index: handle.index,
            generation,
            detached,
        });
        Ok(true)
    }

    /// release worker：执行一次受限 cleanup，并在 lease 归零时归还 slot。
    pub(crate) fn drain_release_queue(&mut self, owner: u32) -> Result<u32, RawInvariant> {
        let mut drained = 0_u32;
        while let Some(ticket) = self.release_queue.pop_front() {
            let detached = self
                .cells
                .get(ticket.descriptor, ticket.index)?
                .is_detached();
            self.cells.complete_release(
                ticket.descriptor,
                ticket.index,
                ticket.generation,
                detached,
            )?;
            self.try_reclaim(
                owner,
                ResourceHandle {
                    descriptor: ticket.descriptor,
                    index: ticket.index,
                    generation: ticket.generation,
                },
            )?;
            drained += 1;
        }
        Ok(drained)
    }

    /// 唯一回收权：lease 归零且 cleanup 完成后归还 slot，跨 owner 时发布 release 消息。
    fn try_reclaim(&mut self, owner: u32, handle: ResourceHandle) -> Result<(), RawInvariant> {
        if !self
            .cells
            .try_begin_reclaim(handle.descriptor, handle.index)?
        {
            return Ok(());
        }
        let token = self
            .table
            .descriptor(handle.descriptor)
            .ok_or_else(|| RawInvariant::new("回收引用未知资源 slab"))?
            .owner;
        if self.resource_owners[owner as usize].token() == token {
            return self.reclaim_locally(handle);
        }
        // 跨 owner 时先把 slot 推进到唯一 ReturnQueued 状态，再由 owner 消费消息归还。
        let message = self.queue_remote_release(handle)?;
        self.publish_release_message(&message, ShardIndex::from_raw(0).expect("shard 0 合法"))
    }

    /// 外来执行者结束最后一个 lease：执行受限 cleanup、取得回收权并返回 release 消息。
    ///
    /// 调用者负责把返回的稳定消息按目标 owner 发布；消息只携带 descriptor、unit、
    /// generation 与 bytes，不携带任何地址。
    pub(crate) fn prepare_foreign_release(
        &mut self,
        owner: u32,
        handle: ResourceHandle,
    ) -> Result<ReturnMessage, RawInvariant> {
        let outcome = self.cells.release_lease(handle.descriptor, handle.index)?;
        if outcome != LeaseOutcome::LastLease {
            return Err(RawInvariant::new("外来 release 必须结束最后一个 lease"));
        }
        if !self.enqueue_release(handle)? {
            return Err(RawInvariant::new("release 入队点已经被其它路径占用"));
        }
        let ticket = self
            .release_queue
            .pop_front()
            .ok_or_else(|| RawInvariant::new("release queue 缺少刚入队的 ticket"))?;
        if ticket.descriptor != handle.descriptor || ticket.index != handle.index {
            return Err(RawInvariant::new("release queue 的 ticket 与请求不一致"));
        }
        self.cells.complete_release(
            ticket.descriptor,
            ticket.index,
            ticket.generation,
            ticket.detached,
        )?;
        let token = self
            .table
            .descriptor(handle.descriptor)
            .ok_or_else(|| RawInvariant::new("release 引用未知资源 slab"))?
            .owner;
        if self.resource_owners[owner as usize].token() == token {
            return Err(RawInvariant::new("本地 owner 必须走本地回收路径"));
        }
        if !self
            .cells
            .try_begin_reclaim(handle.descriptor, handle.index)?
        {
            return Err(RawInvariant::new("lease 未归零或 cleanup 未完成"));
        }
        self.queue_remote_release(handle)
    }

    /// 已取得回收权的跨 owner 归还：推进到 ReturnQueued 并构造 release 消息。
    fn queue_remote_release(
        &mut self,
        handle: ResourceHandle,
    ) -> Result<ReturnMessage, RawInvariant> {
        let (token, bytes) = {
            let descriptor = self
                .table
                .descriptor(handle.descriptor)
                .ok_or_else(|| RawInvariant::new("release 引用未知资源 slab"))?;
            (descriptor.owner, descriptor.slot_stride)
        };
        self.begin_slot_return(handle)?;
        self.message(token, ReturnKind::ResourceRelease, handle, bytes)
    }

    /// 把 release 消息发布到目标 owner 的 inbox。
    pub(crate) fn publish_release_message(
        &self,
        message: &ReturnMessage,
        shard: ShardIndex,
    ) -> Result<(), RawInvariant> {
        let inbox = self.inbox_for(&message.target)?;
        let pool = self.pool();
        let mut staging = ProducerStaging::new(self.limits);
        stage_message(
            &pool,
            Some(&inbox),
            &mut staging,
            message,
            shard,
            Some(FlushTrigger::OwnerPressure),
        )?;
        Ok(())
    }

    /// 把资源 slot 归入 ReturnQueued：Live → Dead → ReturnQueued，并把 bytes 计入 owner 账本。
    fn begin_slot_return(&mut self, handle: ResourceHandle) -> Result<u64, RawInvariant> {
        let (token, bytes) = {
            let descriptor = self
                .table
                .descriptor(handle.descriptor)
                .ok_or_else(|| RawInvariant::new("资源回收引用未知 slab"))?;
            (descriptor.owner, u64::from(descriptor.slot_stride))
        };
        let owner_index = self.owner_index_of(token)?;
        self.resource_owners[owner_index].begin_return(handle, &mut self.table)?;
        let accounting = self
            .directory
            .accounting_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("资源回收缺少 owner 账本"))?;
        self.resource_owners[owner_index].queue_return(
            handle,
            &mut self.table,
            accounting,
            bytes,
        )?;
        Ok(bytes)
    }

    /// owner 上下文归还资源 slot：ReturnQueued → Returned 并推进 generation。
    fn reclaim_locally(&mut self, handle: ResourceHandle) -> Result<(), RawInvariant> {
        let token = self
            .table
            .descriptor(handle.descriptor)
            .ok_or_else(|| RawInvariant::new("本地回收引用未知资源 slab"))?
            .owner;
        let owner_index = self.owner_index_of(token)?;
        let bytes = self.begin_slot_return(handle)?;
        let codec = self.link_codec.clone();
        let accounting = self
            .directory
            .accounting_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("资源回收缺少 owner 账本"))?;
        self.resource_owners[owner_index].consume_return(
            handle,
            &mut self.table,
            &codec,
            accounting,
            bytes,
        )?;
        self.cells.finish_reclaim(handle.descriptor, handle.index)?;
        Ok(())
    }

    /// 把 Resource domain 的 owner token 映射回本地 owner 槽位。
    fn owner_index_of(&self, token: OwnerToken) -> Result<usize, RawInvariant> {
        self.resource_owners
            .iter()
            .position(|owner| owner.token() == token)
            .ok_or_else(|| RawInvariant::new("资源 slab 的 owner token 没有本地槽位"))
    }

    /// 全部处于 Live 的资源 slot。
    fn live_resource_handles(&self) -> Result<Vec<ResourceHandle>, RawInvariant> {
        let mut handles = Vec::new();
        for (index, descriptor) in self.table.descriptors().iter().enumerate() {
            if descriptor.domain != MemoryDomainId::RESOURCE {
                continue;
            }
            let id = SlabDescriptorId::from_raw(u32::try_from(index).expect("描述符下标适配 u32"));
            for slot in 0..descriptor.slot_count() {
                if self.table.state(id, slot)? == SlotState::Live {
                    handles.push(ResourceHandle {
                        descriptor: id,
                        index: slot,
                        generation: descriptor.generation,
                    });
                }
            }
        }
        Ok(handles)
    }

    /// panic 展开：结束创建协程持有的未发布 lease。
    pub(crate) fn panic_unwind(&mut self, owner: u32) -> Result<u32, RawInvariant> {
        let mut handles = Vec::new();
        for handle in self.live_resource_handles()? {
            let cell = self.cells.get(handle.descriptor, handle.index)?;
            if cell.is_shared() || cell.owner_coroutine != u64::from(owner) {
                continue;
            }
            handles.push(handle);
        }
        let mut released = 0_u32;
        for handle in handles {
            self.release_lease(owner, handle)?;
            released += 1;
        }
        Ok(released)
    }

    /// 进程终止：结束全部 lease、执行 cleanup 并归还全部资源 slot。
    pub(crate) fn shutdown(&mut self) -> Result<u32, RawInvariant> {
        let handles = self.live_resource_handles()?;
        let mut reclaimed = 0_u32;
        for handle in handles {
            let detached = self
                .cells
                .get(handle.descriptor, handle.index)?
                .is_detached();
            let generation = self
                .table
                .descriptor(handle.descriptor)
                .ok_or_else(|| RawInvariant::new("shutdown 引用未知资源 slab"))?
                .generation;
            self.cells
                .request_release(handle.descriptor, handle.index)?;
            self.cells
                .complete_release(handle.descriptor, handle.index, generation, detached)?;
            self.cells
                .force_release_all(handle.descriptor, handle.index)?;
            if self
                .cells
                .try_begin_reclaim(handle.descriptor, handle.index)?
            {
                self.reclaim_locally(handle)?;
                reclaimed += 1;
            }
        }
        self.release_queue.clear();
        Ok(reclaimed)
    }
}
