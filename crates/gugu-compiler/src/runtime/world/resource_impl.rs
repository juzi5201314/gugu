//! ResourceCell slab 与 lease 状态机在 owner-directed 世界中的接入。
//!
//! 本模块是 `RawWorld` 的资源侧实现：地址稳定的 ResourceCell 分配、lease 复制、单向发布、
//! 幂等 close、唯一 release 入队点、受限 cleanup、generation 推进与跨 owner 的
//! `ResourceRelease` 消息归还都只经这里的入口，owner 本地 free structure 与账本保持唯一
//! 真相源。

use super::super::RESOURCE_DEDICATED_ALIGN_LIMIT;
use super::super::extent::{ExtentOccupancy, TrimReport};
use super::super::inbox::ShardIndex;
use super::super::message::{
    FlushTrigger, ProducerStaging, ReturnKind, ReturnMessage, stage_message,
};
use super::super::owner::Allocation;
use super::super::resource::{
    self, CloseOutcome, LeaseOutcome, ReleaseRegistry, ReleaseTicket, ResourceHandle,
};
use super::super::size_class::{RuntimeSizeClass, RuntimeSizeClassTable};
use super::super::slab::{
    MemoryDomainId, OwnerToken, RawInvariant, RawSlot, SlabDescriptorId, SlabState, SlotState,
};
use super::{RawWorld, ResourceShape, ServiceBudget};

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

    /// 校验资源句柄仍指向当前 live cell。
    fn validate_resource_handle(&self, handle: ResourceHandle) -> Result<(), RawInvariant> {
        let descriptor = self
            .table
            .descriptor(handle.descriptor)
            .ok_or_else(|| RawInvariant::new("资源句柄引用未知 slab"))?;
        if descriptor.domain != MemoryDomainId::RESOURCE {
            return Err(RawInvariant::new("资源句柄引用非 Resource slab"));
        }
        if descriptor.generation != handle.generation {
            return Err(RawInvariant::new("资源句柄引用过期 slab generation"));
        }
        if self.table.state(handle.descriptor, handle.index)? != SlotState::Live {
            return Err(RawInvariant::new("资源句柄不再指向 live slot"));
        }
        let cell = self.cells.get(handle.descriptor, handle.index)?;
        if cell.generation != handle.cell_generation.raw() {
            return Err(RawInvariant::new("资源句柄引用过期 cell generation"));
        }
        Ok(())
    }

    /// 将资源句柄转换为 slab owner API 所需的 descriptor generation。
    fn slab_slot(&self, handle: ResourceHandle) -> Result<RawSlot, RawInvariant> {
        let generation = self
            .table
            .descriptor(handle.descriptor)
            .ok_or_else(|| RawInvariant::new("资源句柄引用未知 slab"))?
            .generation;
        Ok(RawSlot {
            descriptor: handle.descriptor,
            index: handle.index,
            generation,
        })
    }

    /// 返回某个资源 slot 的 header。
    pub(crate) fn resource_cell(
        &self,
        handle: ResourceHandle,
    ) -> Result<&resource::ResourceCell, RawInvariant> {
        self.validate_resource_handle(handle)?;
        self.cells.get(handle.descriptor, handle.index)
    }

    /// 返回某个资源 slot 当前的 lease 数。
    pub(crate) fn resource_leases(&self, handle: ResourceHandle) -> Result<u64, RawInvariant> {
        if self.validate_resource_handle(handle).is_ok() {
            return Ok(self.cells.get(handle.descriptor, handle.index)?.leases);
        }
        let cell = self.cells.get(handle.descriptor, handle.index)?;
        if self.table.state(handle.descriptor, handle.index)? == SlotState::Returned
            && cell.leases == 0
            && cell.generation == handle.cell_generation.raw().saturating_add(1)
        {
            return Ok(0);
        }
        self.validate_resource_handle(handle)?;
        unreachable!("资源句柄校验应返回错误")
    }

    /// 校验 ResourceCell header 表与 slab 状态一致。
    pub(crate) fn verify_resource_cells(&self) -> Result<(), RawInvariant> {
        self.cells.verify(&self.table)
    }

    /// 复用同 owner、同 dedicated layout 的空 slot，避免每次大资源分配都新增 mapping。
    fn reuse_dedicated_slot(
        &mut self,
        token: OwnerToken,
        class: &RuntimeSizeClass,
    ) -> Result<Option<Allocation>, RawInvariant> {
        let descriptor_id =
            self.table
                .descriptors()
                .iter()
                .enumerate()
                .find_map(|(index, descriptor)| {
                    let id = SlabDescriptorId::from_raw(u32::try_from(index).ok()?);
                    (descriptor.domain == MemoryDomainId::RESOURCE
                        && descriptor.owner == token
                        && descriptor.class == class.id
                        && descriptor.slot_stride == class.slot_stride
                        && descriptor.alignment == class.alignment
                        && descriptor.slot_count() == 1
                        && descriptor.state == SlabState::Active
                        && descriptor.free == 1
                        && self.table.state(id, 0).ok() == Some(SlotState::Returned))
                    .then_some(id)
                });
        let Some(descriptor) = descriptor_id else {
            return Ok(None);
        };
        let descriptor_domain = self
            .table
            .descriptor(descriptor)
            .ok_or_else(|| RawInvariant::new("dedicated descriptor 缺失"))?
            .domain;
        let codec = self.provenance.codec_for(descriptor_domain).clone();
        let index = self
            .table
            .pop_free(descriptor, &codec)?
            .ok_or_else(|| RawInvariant::new("dedicated descriptor 的 free slot 缺失"))?;
        self.table
            .transition(descriptor, index, SlotState::Returned, SlotState::Live)?;
        let generation = {
            let record = self
                .table
                .descriptor_mut(descriptor)
                .ok_or_else(|| RawInvariant::new("dedicated descriptor 缺失"))?;
            record.live += 1;
            record.generation
        };
        let accounting = self
            .directory
            .accounting_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("dedicated 复用缺少 owner 账本"))?;
        accounting.take_from_cache(u64::from(class.slot_stride));
        Ok(Some(Allocation {
            slot: RawSlot {
                descriptor,
                index,
                generation,
            },
            level: super::super::owner::AllocationLevel::RangeRequest,
        }))
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
                let codec = self.provenance.codec_for(class.domain).clone();
                let provider = &mut self.provider;
                let table = &mut self.table;
                let extents = &mut self.extents;
                let slab_epoch = self.epoch;
                let extent_class = super::super::owner::span_extent_class();
                self.resource_owners[owner as usize].allocate(
                    owner,
                    &class,
                    extent_class,
                    table,
                    extents,
                    provider,
                    &codec,
                    accounting,
                    secret_index,
                    slab_epoch,
                    0,
                )?
            }
            None => {
                let stride = resource::dedicated_stride(shape.payload_bytes, shape.alignment)?;
                let alignment = shape.alignment.max(64);
                let class = resource::dedicated_class(stride, alignment)?;
                if let Some(allocation) = self.reuse_dedicated_slot(token, &class)? {
                    allocation
                } else {
                    let (extent, bytes) = {
                        let accounting = self
                            .directory
                            .accounting_mut(token.owner_id)
                            .ok_or_else(|| RawInvariant::new("专用 mapping 缺少 owner 账本"))?;
                        let extent_class = super::super::extent::class_for_bytes(u64::from(stride))
                            .ok_or_else(|| {
                                RawInvariant::new("专用 mapping 超出 extent 阶梯上界")
                            })?;
                        let commit = resource::reserve_mapping(
                            &mut self.extents,
                            &mut self.provider,
                            accounting,
                            owner,
                            extent_class,
                            self.epoch,
                        )?;
                        (commit.extent, commit.bytes)
                    };
                    let descriptor = self.table.create(
                        &class,
                        token,
                        extent,
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
        let cell_generation = self
            .cells
            .get(allocation.slot.descriptor, allocation.slot.index)?
            .generation;
        Ok(ResourceHandle {
            descriptor: allocation.slot.descriptor,
            index: allocation.slot.index,
            generation: allocation.slot.generation,
            cell_generation: super::super::slab::SlabGeneration::from_raw(cell_generation),
        })
    }

    /// 复制资源值：增加一个 lease。
    pub(crate) fn resource_acquire(&mut self, handle: ResourceHandle) -> Result<(), RawInvariant> {
        self.validate_resource_handle(handle)?;
        self.cells.acquire(handle.descriptor, handle.index)
    }

    /// 发布到共享图；状态单向进入 Shared。
    pub(crate) fn resource_publish(&mut self, handle: ResourceHandle) -> Result<(), RawInvariant> {
        self.validate_resource_handle(handle)?;
        self.cells.publish(handle.descriptor, handle.index)
    }

    /// 显式幂等 close；返回是否首次关闭。
    pub(crate) fn resource_close(
        &mut self,
        owner: u32,
        handle: ResourceHandle,
    ) -> Result<CloseOutcome, RawInvariant> {
        self.validate_resource_handle(handle)?;
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
        self.validate_resource_handle(handle)?;
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
        self.validate_resource_handle(handle)?;
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
        let generation = handle.cell_generation;
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

    /// 从 release queue 取出指定 cell 的 ticket，避免误取其它资源的请求。
    fn take_release_ticket(
        &mut self,
        handle: ResourceHandle,
    ) -> Result<ReleaseTicket, RawInvariant> {
        let position = self
            .release_queue
            .iter()
            .position(|ticket| {
                ticket.descriptor == handle.descriptor
                    && ticket.index == handle.index
                    && ticket.generation == handle.cell_generation
            })
            .ok_or_else(|| RawInvariant::new("release queue 缺少指定资源 ticket"))?;
        self.release_queue
            .remove(position)
            .ok_or_else(|| RawInvariant::new("release queue ticket 在取出时丢失"))
    }

    /// release worker：执行一次受限 cleanup，并在 lease 归零时归还 slot。
    pub(crate) fn drain_release_queue(&mut self, owner: u32) -> Result<u32, RawInvariant> {
        let mut drained = 0_u32;
        while let Some(ticket) = self.release_queue.pop_front() {
            let slab_generation = self
                .table
                .descriptor(ticket.descriptor)
                .ok_or_else(|| RawInvariant::new("release 引用未知资源 slab"))?
                .generation;
            self.cells.complete_release(
                ticket.descriptor,
                ticket.index,
                ticket.generation,
                ticket.detached,
            )?;
            self.try_reclaim(
                owner,
                ResourceHandle {
                    descriptor: ticket.descriptor,
                    index: ticket.index,
                    generation: slab_generation,
                    cell_generation: ticket.generation,
                },
            )?;
            drained += 1;
        }
        Ok(drained)
    }

    /// 唯一回收权：lease 归零且 cleanup 完成后归还 slot，跨 owner 时发布 release 消息。
    fn try_reclaim(&mut self, owner: u32, handle: ResourceHandle) -> Result<(), RawInvariant> {
        let caller_token = self
            .resource_owners
            .get(owner as usize)
            .ok_or_else(|| RawInvariant::new("release 使用了未知 owner"))?
            .token();
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
        if caller_token == token {
            return self.reclaim_locally(handle);
        }
        // 跨 owner 时先把 slot 推进到唯一 ReturnQueued 状态，再由 owner 消费消息归还。
        let message = match self.queue_remote_release(handle) {
            Ok(message) => message,
            Err(error) => {
                self.rollback_remote_reclaim(handle)?;
                return Err(error);
            }
        };
        if let Err(error) =
            self.publish_release_message(&message, ShardIndex::from_raw(0).expect("shard 0 合法"))
        {
            self.rollback_remote_reclaim(handle)?;
            return Err(error);
        }
        Ok(())
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
        self.validate_resource_handle(handle)?;
        let token = self
            .table
            .descriptor(handle.descriptor)
            .ok_or_else(|| RawInvariant::new("release 引用未知资源 slab"))?
            .owner;
        let caller = self
            .resource_owners
            .get(owner as usize)
            .ok_or_else(|| RawInvariant::new("release 使用了未知 owner"))?;
        if caller.token() == token {
            return Err(RawInvariant::new("本地 owner 必须走本地回收路径"));
        }
        if self.cells.get(handle.descriptor, handle.index)?.leases != 1 {
            return Err(RawInvariant::new("外来 release 必须结束最后一个 lease"));
        }
        let queued = self.enqueue_release(handle)?;
        if !queued
            && !self
                .cells
                .get(handle.descriptor, handle.index)?
                .is_release_done()
        {
            return Err(RawInvariant::new("已有 release 入队但 cleanup 尚未完成"));
        }
        self.cells.release_lease(handle.descriptor, handle.index)?;
        if queued {
            let ticket = self.take_release_ticket(handle)?;
            self.cells.complete_release(
                ticket.descriptor,
                ticket.index,
                ticket.generation,
                ticket.detached,
            )?;
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
        let message_slot = RawSlot {
            descriptor: handle.descriptor,
            index: handle.index,
            generation: handle.cell_generation,
        };
        self.message(token, ReturnKind::ResourceRelease, message_slot, bytes)
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
        let slot = self.slab_slot(handle)?;
        self.resource_owners[owner_index].begin_return(slot, &mut self.table)?;
        let accounting = self
            .directory
            .accounting_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("资源回收缺少 owner 账本"))?;
        self.resource_owners[owner_index].queue_return(slot, &mut self.table, accounting, bytes)?;
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
        let descriptor_domain = self
            .table
            .descriptor(handle.descriptor)
            .map(|record| record.domain)
            .unwrap_or(MemoryDomainId::RESOURCE);
        let codec = self.provenance.codec_for(descriptor_domain).clone();
        let slot = self.slab_slot(handle)?;
        let accounting = self
            .directory
            .accounting_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("资源回收缺少 owner 账本"))?;
        self.resource_owners[owner_index].consume_return(
            slot,
            &mut self.table,
            &codec,
            accounting,
            bytes,
        )?;
        self.cells.finish_reclaim(handle.descriptor, handle.index)?;
        Ok(())
    }

    /// 回滚远程消息发布失败留下的 ReturnQueued 与回收权。
    fn rollback_remote_reclaim(&mut self, handle: ResourceHandle) -> Result<(), RawInvariant> {
        let (token, bytes) = {
            let descriptor = self
                .table
                .descriptor(handle.descriptor)
                .ok_or_else(|| RawInvariant::new("远程回滚引用未知资源 slab"))?;
            (descriptor.owner, u64::from(descriptor.slot_stride))
        };
        let owner_index = self.owner_index_of(token)?;
        let slot = self.slab_slot(handle)?;
        let accounting = self
            .directory
            .accounting_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("远程回滚缺少 owner 账本"))?;
        self.resource_owners[owner_index].cancel_return(
            slot,
            &mut self.table,
            accounting,
            bytes,
        )?;
        self.cells
            .clear_reclaiming(handle.descriptor, handle.index)?;
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
                    let cell_generation = self.cells.get(id, slot)?.generation;
                    handles.push(ResourceHandle {
                        descriptor: id,
                        index: slot,
                        generation: descriptor.generation,
                        cell_generation: super::super::slab::SlabGeneration::from_raw(
                            cell_generation,
                        ),
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

    /// 排空 shutdown 前已经发布的 return message，并释放 message node。
    fn drain_shutdown_messages(&mut self) -> Result<(), RawInvariant> {
        let budget = ServiceBudget::pressure(u32::MAX, u64::MAX);
        loop {
            let mut progress = 0_u64;
            for owner in 0..self.inboxes.len() as u32 {
                let (forwarded, consumed) = self.drain_all(owner, &budget)?;
                progress += u64::from(forwarded) + u64::from(consumed);
            }
            if progress == 0 {
                break;
            }
        }
        self.release_graced_nodes()?;
        Ok(())
    }

    /// 取出尚未由 worker 消费的 release ticket，并完成受限 cleanup。
    fn complete_pending_releases(&mut self) -> Result<(), RawInvariant> {
        while let Some(ticket) = self.release_queue.pop_front() {
            self.cells.complete_release(
                ticket.descriptor,
                ticket.index,
                ticket.generation,
                ticket.detached,
            )?;
        }
        Ok(())
    }

    /// 统计尚未归还的 Resource slot。
    fn active_resource_slots(&self) -> Result<u32, RawInvariant> {
        let mut active = 0_u32;
        for (index, descriptor) in self.table.descriptors().iter().enumerate() {
            if descriptor.domain != MemoryDomainId::RESOURCE {
                continue;
            }
            let id = SlabDescriptorId::from_raw(u32::try_from(index).expect("描述符下标适配 u32"));
            for slot in 0..descriptor.slot_count() {
                if self.table.state(id, slot)? != SlotState::Returned {
                    active += 1;
                }
            }
        }
        Ok(active)
    }

    /// 释放已经没有 live slot 的 Resource domain extent。
    ///
    /// 与 raw plane 相同：先经 lease、slot、在途 return 与 grace 四条门禁，再撤销物理页并把
    /// extent 合并回 buddy 阶梯。被门禁拒绝的 extent 保留 committed 状态，由下一次 trim 重试。
    fn release_resource_ranges(&mut self) -> Result<TrimReport, RawInvariant> {
        let mut report = TrimReport::default();
        let candidates: Vec<_> = self
            .table
            .descriptors()
            .iter()
            .enumerate()
            .filter_map(|(index, descriptor)| {
                if descriptor.domain == MemoryDomainId::RESOURCE
                    && descriptor.live == 0
                    && descriptor.queued == 0
                    && descriptor.state != SlabState::Released
                {
                    Some((
                        SlabDescriptorId::from_raw(u32::try_from(index).ok()?),
                        descriptor.extent,
                        descriptor.committed_bytes,
                        descriptor.owner,
                        descriptor.pending_returns,
                    ))
                } else {
                    None
                }
            })
            .collect();
        for (descriptor, extent, bytes, owner, pending_returns) in candidates {
            let occupancy = ExtentOccupancy {
                live_slots: 0,
                queued_slots: 0,
                pending_returns,
            };
            match self.poll_trim_extent(extent, occupancy)? {
                Ok(_) => {}
                Err(blocked) => {
                    report.blocked.push((extent, blocked));
                    continue;
                }
            }
            let accounting = self
                .directory
                .accounting_mut(owner.owner_id)
                .ok_or_else(|| RawInvariant::new("释放 Resource extent 缺少 owner 账本"))?;
            accounting.release(bytes);
            let record = self
                .table
                .descriptor_mut(descriptor)
                .ok_or_else(|| RawInvariant::new("释放 Resource extent 缺少 descriptor"))?;
            record.state = SlabState::Released;
            record.committed_bytes = 0;
            report.trimmed += 1;
        }
        Ok(report)
    }

    /// 进程终止：排空远程 return、结束全部 lease、执行 cleanup 并归还全部资源 slot。
    pub(crate) fn shutdown(&mut self) -> Result<u32, RawInvariant> {
        let active = self.active_resource_slots()?;
        self.drain_shutdown_messages()?;
        self.complete_pending_releases()?;
        for handle in self.live_resource_handles()? {
            let detached = self
                .cells
                .get(handle.descriptor, handle.index)?
                .is_detached();
            self.cells
                .request_release(handle.descriptor, handle.index)?;
            self.cells.complete_release(
                handle.descriptor,
                handle.index,
                handle.cell_generation,
                detached,
            )?;
            self.cells
                .force_release_all(handle.descriptor, handle.index)?;
            if self
                .cells
                .try_begin_reclaim(handle.descriptor, handle.index)?
            {
                self.reclaim_locally(handle)?;
            }
        }
        self.complete_pending_releases()?;
        if !self.release_queue.is_empty() || self.active_resource_slots()? != 0 {
            return Err(RawInvariant::new("shutdown 未归还全部 Resource slot"));
        }
        self.release_resource_ranges()?;
        Ok(active)
    }
}
