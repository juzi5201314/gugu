//! `RawWorld` 上的 SharedHeap forwarding 闭环：搬迁生产者、`HandleForward` 消费者与共享平面推进。
//!
//! 归属规则：
//!
//! 1. **搬迁在请求方上下文线性化**：`forward_shared_payload` 先过登记表与 pin 门禁，再由
//!    `SharedHeap::forward` 完成复制与 slot 切换；世界侧只把旧位置记成待结清记录，因此任何
//!    时刻的 current payload 都是完整对象，搬迁过程中没有「半个对象」的中间态。
//! 2. **lease 在目标 owner 结清**：搬迁通知必须由 payload owner 的 service 上下文消费，只有
//!    它能结清 forwarding lease、推进 grace 并释放旧 payload 的世界侧记账。
//! 3. **身份校验先于状态改变**：目标 token、integrity、cycle/topology epoch、owner 归属与在飞
//!    记录全部通过之后才触碰 lease 与 grace，因此错误、重复或陈旧的通知不会污染共享账本。
//! 4. **共享平面只在覆盖全部 owner 的 scope 内推进**：共享对象可以被任意 owner 引用，单 owner
//!    scope 里的「未标记」不等于死亡，因此 sweep 与搬迁都需要全 owner 语义。
//!
//! 单线程参照模型里复制与线性化在请求方上下文一次完成；真实 runtime 由 payload owner 的
//! service 上下文执行同一步骤，因此 lease、grace 与旧 payload 释放这些**消息维度**的不变量
//! 在本模型中仍由目标 owner 消费通知时结清，而不是由搬迁方代劳。

use super::super::inbox::{ServiceBudget, ShardIndex};
use super::super::message::{
    FlushTrigger, HandleForward, IntegrityTag, MessageState, flush_staging, stage_handle_forward,
};
use super::super::shared_heap::{ForwardDeferred, SharedForward, SharedForwardRecord};
use super::super::shared_heap_schema::{
    SharedHandle, SharedHandleSlot, SharedPayloadId, SharedSlotState,
};
use super::super::size_class::RuntimeSizeClassId;
use super::super::slab::{RawInvariant, Resolution, SlabGeneration};
use super::RawWorld;
use super::heap_impl::shared_heap_error;
use super::shared_heap_impl::{SharedPayloadBlock, SharedPendingForward};

/// 一次 shared payload 搬迁的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SharedForwardOutcome {
    /// payload 已切换，旧 payload 进入 grace 并等待目标 owner 结清。
    Forwarded(SharedForwardRecord),
    /// 搬迁被推迟；slot 的 current payload 与 forward generation 都没有改变。
    Deferred(ForwardDeferred),
}

/// 共享平面的累计计数快照。
///
/// 报告一律用相邻两次快照之差得到「本次调用真实做了多少」，因此消费者在别的 drain 里释放的
/// 字节也能被同一份账本观测到，不需要在各个调用点重复记账。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SharedForwardTotals {
    /// 已发布的搬迁通知数。
    pub(crate) forwards: u64,
    /// 已复制的搬迁字节数。
    pub(crate) forwarded_bytes: u64,
    /// 已释放的 payload 字节数；含 grace 结清的旧 payload 与 sweep 释放的存活 payload。
    pub(crate) freed_bytes: u64,
}

impl SharedForwardTotals {
    /// 返回相对 `earlier` 的增量；累计量只增，因此差值就是本次调用的工作量。
    fn since(self, earlier: Self) -> Self {
        Self {
            forwards: self.forwards.saturating_sub(earlier.forwards),
            forwarded_bytes: self.forwarded_bytes.saturating_sub(earlier.forwarded_bytes),
            freed_bytes: self.freed_bytes.saturating_sub(earlier.freed_bytes),
        }
    }
}

/// 一次共享平面推进的结果；进入 cycle 报告与测试。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SharedPlaneReport {
    /// 真正释放的共享对象数。
    pub(crate) released: u64,
    /// 本次推进真实发布出的搬迁通知数。
    pub(crate) forwarded: u64,
    /// 本次推进真实复制的字节数。
    pub(crate) forwarded_bytes: u64,
    /// 本次推进真实释放的 payload 字节数。
    pub(crate) freed_bytes: u64,
    /// 因 pin 被推迟的搬迁次数。
    pub(crate) deferred: u64,
    /// 已封口且不再有存活 payload 的 block 数。
    pub(crate) empty_blocks: u64,
    /// 本平面消费的消息数。
    pub(crate) consumed_messages: u64,
    /// 本平面在消费时转投出去的消息数。
    pub(crate) forwarded_messages: u64,
}

impl RawWorld {
    /// 返回共享平面的累计计数快照。
    pub(crate) const fn shared_forward_totals(&self) -> SharedForwardTotals {
        self.shared_totals
    }

    /// 返回仍在飞的搬迁通知数：已离开生产者、尚未在目标 owner 结清。
    pub(crate) fn shared_forward_pending(&self) -> u64 {
        u64::try_from(self.shared_registry.pending_forwards().count()).expect("在飞搬迁数适配 u64")
    }

    /// 取一个新的共享搬迁 lease；单调非零，因此 0 永远可以表示「没有在飞 lease」。
    fn next_shared_forward_lease(&mut self) -> Result<u32, RawInvariant> {
        self.next_shared_forward_lease = self
            .next_shared_forward_lease
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("共享搬迁 lease 溢出"))?;
        Ok(self.next_shared_forward_lease)
    }

    /// 搬迁一个共享 payload：在同一 owner 的另一个 block 位置重建 payload 并切换 slot 的
    /// current payload。
    ///
    /// `worker` 是发布搬迁通知的 owner 槽位（staging 与 producer gate 属于它），`handle` 的
    /// payload owner 决定通知目标。返回 `Deferred` 时没有任何状态被改动；失败都是不变量失败，
    /// 没有「部分搬迁」的中间态：复制与 slot 切换由参照实现在一个线性化点内完成。
    pub(crate) fn forward_shared_payload(
        &mut self,
        worker: u32,
        handle: SharedHandle,
    ) -> Result<SharedForwardOutcome, RawInvariant> {
        // 1. 登记项与在飞状态：未登记、已交还或已有在飞搬迁都拒绝。
        let record = *self.shared_payload_block(handle)?;
        if record.returned {
            return Err(RawInvariant::new("已交还的共享 payload 不得再搬迁"));
        }
        if record.pending_forward.is_some() {
            return Err(RawInvariant::new("共享 payload 已有在飞搬迁"));
        }
        // 2. slot 状态与 pin：pin 是唯一允许的推迟原因，它不改动任何状态。
        let slot = self
            .shared_heap()?
            .slot_record(handle)
            .ok_or_else(|| RawInvariant::new("共享 handle slot 身份已过期"))?;
        let state = SharedSlotState::from_raw(slot.state)
            .ok_or_else(|| RawInvariant::new("共享 handle slot 状态判别值未登记"))?;
        if state != SharedSlotState::Live {
            return Err(RawInvariant::new("共享 handle slot 状态不允许搬迁"));
        }
        if slot.pin_leases != 0 {
            return Ok(SharedForwardOutcome::Deferred(ForwardDeferred::Pinned));
        }
        // 3. 目标位置：与源同 owner，descriptor 由世界级分配器推进，绝不跨 owner 迁移。
        let owner = record.owner;
        let bytes = record.payload_bytes;
        let (block, block_offset) = self.shared_registry.reserve(owner, bytes)?;
        let descriptor = block.id.arena();
        if self
            .shared_registry
            .block_record(descriptor)
            .is_some_and(|record| record.extent.is_none())
        {
            self.commit_shared_block_pages(owner, descriptor)?;
        }
        let payload = self
            .shared_heap_mut()?
            .allocate_payload(owner, descriptor, block_offset, bytes)
            .map_err(shared_heap_error)?;
        let expected = slot
            .forward_generation
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("共享 forward generation 溢出"))?;
        let lease = self.next_shared_forward_lease()?;
        // 4. 复制与线性化：目标 payload 建立、current 切换、旧 payload 进入 grace。
        let outcome = self
            .shared_heap_mut()?
            .forward(handle, payload.id, lease, expected)
            .map_err(shared_heap_error)?;
        let SharedForward::Forwarded(forwarded) = outcome else {
            return Err(RawInvariant::new("共享 payload 已排除 pin，搬迁不得被推迟"));
        };
        debug_assert_eq!(forwarded.old_payload, record.payload);
        // 5. 世界账本：新位置写回登记项，旧位置进入待结清记录，字节先记进目标 block。
        self.shared_registry
            .relocate(handle, payload.id, block, block_offset)?;
        self.shared_registry.note_payload_added(descriptor, bytes)?;
        self.shared_registry.set_pending_forward(
            handle,
            SharedPendingForward {
                old_payload: record.payload,
                old_block: record.block,
                old_block_offset: record.block_offset,
                bytes,
                lease,
                forward_generation: expected,
            },
        )?;
        // 6. 通知目标 owner：它才是结清 lease 与 grace 的唯一权威。
        self.publish_handle_forward(
            worker,
            owner,
            handle,
            expected,
            record.payload,
            payload.id,
            bytes,
        )?;
        self.shared_totals.forwards = self
            .shared_totals
            .forwards
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("共享搬迁计数溢出"))?;
        self.shared_totals.forwarded_bytes = self
            .shared_totals
            .forwarded_bytes
            .checked_add(u64::from(bytes))
            .ok_or_else(|| RawInvariant::new("共享搬迁字节数溢出"))?;
        Ok(SharedForwardOutcome::Forwarded(forwarded))
    }

    /// 给一条搬迁通知绑定目标身份、签名并写入 staging。
    ///
    /// 生产者与转投共用这一步：`HandleForward` 的完整性覆盖全部身份字段，因此目标 token 必须
    /// 在签名之前确定，转发路径也必须重新签名，而不是沿用原 owner 的校验值。
    fn publish_handle_forward(
        &mut self,
        worker: u32,
        target_index: u32,
        handle: SharedHandle,
        forward_generation: u32,
        old_payload: SharedPayloadId,
        new_payload: SharedPayloadId,
        bytes: u32,
    ) -> Result<(), RawInvariant> {
        let target_token = self.token(target_index);
        let plane = self.mark_plane()?;
        let (cycle_epoch, topology_epoch) = (plane.cycle(), plane.topology());
        let mut message = HandleForward {
            next: None,
            target: target_token,
            handle_table: handle.table(),
            handle_slot: handle.slot(),
            handle_generation: handle.generation(),
            forward_generation,
            old_payload,
            new_payload,
            cycle_epoch,
            topology_epoch,
            bytes,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(target_token.generation.raw()),
                class: RuntimeSizeClassId::from_raw(0),
                owner_id: target_token.owner_id,
                route_key: target_token.route_key,
                checksum: 0,
            },
        };
        message.integrity.checksum =
            IntegrityTag::compute_handle_forward(&self.integrity_secret, &message);
        let shard = ShardIndex::from_raw(target_index % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("handle forward shard 编号越界"))?;
        // staging 一次只承载一个 target：目标改变时先交出旧 chain，与 return 路径同一规则。
        if let Some(previous) = self.return_stagings[worker as usize].target()
            && previous != target_token
        {
            let previous_inbox = self.inbox_for(&previous)?;
            flush_staging(
                &self.pool,
                &previous_inbox,
                &mut self.return_stagings[worker as usize],
                FlushTrigger::TargetChanged,
            )?;
        }
        let inbox = self.inbox(target_index);
        stage_handle_forward(
            &self.pool,
            Some(&inbox),
            &mut self.return_stagings[worker as usize],
            &message,
            shard,
            Some(FlushTrigger::OwnerPressure),
        )?;
        Ok(())
    }

    /// owner 上下文：消费一条 `HandleForward`，结清 lease、推进 grace 并释放旧 payload。
    ///
    /// 校验顺序固定：任何一步失败都不得改变 lease、grace 或世界账本，因此错误、重复与陈旧
    /// 通知都是干净拒绝，而不是「扣掉 lease 才发现消息不对」。
    pub(crate) fn service_handle_forward(
        &mut self,
        owner: u32,
        message: &HandleForward,
    ) -> Result<(), RawInvariant> {
        if self.token(owner) != message.target {
            return Err(RawInvariant::new("handle forward 投递到错误 owner"));
        }
        message
            .integrity
            .verify_handle_forward(&self.integrity_secret, message)?;
        // forward 的 grace 允许跨 cycle 结清，因此 cycle epoch 只拒绝「来自未来」的通知；拓扑
        // 变化会让 manager 身份失效，必须相等。
        let plane = self.mark_plane()?;
        if message.topology_epoch != plane.topology() {
            return Err(RawInvariant::new("handle forward 的 topology epoch 已过期"));
        }
        if message.cycle_epoch > plane.cycle() {
            return Err(RawInvariant::new(
                "handle forward 的 cycle epoch 超过当前 cycle",
            ));
        }
        let handle = SharedHandle::new(
            message.handle_table,
            message.handle_slot,
            message.handle_generation,
        )
        .map_err(|error| RawInvariant::new(error.message().to_owned()))?;
        match self.directory().resolve(&message.target) {
            Resolution::Match => {}
            Resolution::Forward(target) => {
                // 管理权已转移：通知与 lease 一起转投新 owner，不在这里结清。
                let target_index = u32::try_from(self.owner_slot(&target)?)
                    .map_err(|_| RawInvariant::new("owner 槽位超出 u32"))?;
                return self.publish_handle_forward(
                    owner,
                    target_index,
                    handle,
                    message.forward_generation,
                    message.old_payload,
                    message.new_payload,
                    message.bytes,
                );
            }
            Resolution::Retired | Resolution::Unknown => {
                return Err(RawInvariant::new(
                    "handle forward 的目标 owner 已 retire 或未知",
                ));
            }
        }
        let record = *self.shared_payload_block(handle)?;
        if record.owner != owner {
            return Err(RawInvariant::new(
                "handle forward 的 handle 不属于目标 owner",
            ));
        }
        if record.returned {
            return Err(RawInvariant::new("handle forward 的 handle 已交还"));
        }
        if record.payload != message.new_payload {
            return Err(RawInvariant::new(
                "handle forward 的新 payload 与登记项不一致",
            ));
        }
        let pending = self
            .shared_registry
            .pending_forward(handle)
            .ok_or_else(|| RawInvariant::new("handle forward 没有对应的在飞搬迁"))?;
        if pending.forward_generation != message.forward_generation
            || pending.old_payload != message.old_payload
            || pending.bytes != message.bytes
        {
            return Err(RawInvariant::new("handle forward 与在飞搬迁记录不一致"));
        }
        self.shared_heap_mut()?
            .end_forward_lease(handle, pending.lease)
            .map_err(shared_heap_error)?;
        let steps = self.shared_heap_contract()?.forwarding_grace_steps();
        self.shared_heap_mut()?
            .advance_grace(handle, steps)
            .map_err(shared_heap_error)?;
        self.retire_pending_forward(handle, &pending)
    }

    /// 在 grace 已经结清时释放待结清记录的旧 payload 记账；未结清时原样保留。
    ///
    /// 返回旧 payload 是否真的消失：`false` 表示仍有 guard、pin 或票据持有它，调用方（settle
    /// 或下一次 cycle）必须重试，而不是把字节提前记成已释放。
    fn retire_pending_forward(
        &mut self,
        handle: SharedHandle,
        pending: &SharedPendingForward,
    ) -> Result<(), RawInvariant> {
        if self.shared_heap()?.payload_exists(pending.old_payload) {
            return Ok(());
        }
        self.shared_registry
            .note_payload_freed(pending.old_block.id.arena(), pending.bytes)?;
        self.shared_registry.clear_pending_forward(handle)?;
        self.shared_totals.freed_bytes = self
            .shared_totals
            .freed_bytes
            .checked_add(u64::from(pending.bytes))
            .ok_or_else(|| RawInvariant::new("共享释放字节数溢出"))?;
        Ok(())
    }

    /// 重试在飞搬迁的结清：lease 已结清但 grace 尚未走完的记录在这里补步并释放旧 payload。
    ///
    /// 结清发生在目标 owner 的消费路径上，而消费可能因为 guard、pin 或票据而推迟，因此这一步
    /// 是「同一 cycle 没结清」的唯一补救点；它不改动 lease，只推进已经开始的 grace。
    pub(crate) fn settle_shared_forwards(&mut self) -> Result<(), RawInvariant> {
        let pending_list: Vec<(SharedHandle, SharedPendingForward)> =
            self.shared_registry.pending_forwards().collect();
        for (handle, pending) in pending_list {
            let slot = self
                .shared_heap()?
                .slot_record(handle)
                .ok_or_else(|| RawInvariant::new("在飞搬迁的 handle 身份已过期"))?;
            if slot.forwarding_leases != 0 {
                continue;
            }
            self.shared_heap_mut()?
                .advance_grace(handle, 1)
                .map_err(shared_heap_error)?;
            self.retire_pending_forward(handle, &pending)?;
        }
        Ok(())
    }

    /// 推进一次共享平面：结清在飞搬迁、释放未标记对象、搬迁大部分已死的 block。
    ///
    /// 前置是「已开始的 mark cycle」：未标记判定读的就是本 cycle 的 side mark。`scope` 必须
    /// 覆盖全部 owner——共享对象可以被任意 owner 引用，单 owner scope 里的未标记不等于死亡，
    /// 因此这种调用返回空报告，而不是按错误的存活判定释放对象。
    pub(crate) fn run_shared_plane(
        &mut self,
        scope: &[u32],
    ) -> Result<SharedPlaneReport, RawInvariant> {
        let mut report = SharedPlaneReport::default();
        if self.shared_heap.is_none() || !self.mark_configured() {
            return Ok(report);
        }
        if self.shared_heap()?.mark_cycle().is_none() {
            return Err(RawInvariant::new("共享平面需要已开始的 mark cycle"));
        }
        let owners = u32::try_from(self.owners.len()).expect("owner 数适配 u32");
        if !Self::scope_covers_all(scope, owners) {
            debug_assert!(false, "共享平面需要覆盖全部 owner 的 scope");
            return Ok(report);
        }
        let before = self.shared_totals;
        report.freed_bytes = {
            self.settle_shared_forwards()?;
            self.shared_totals
                .freed_bytes
                .saturating_sub(before.freed_bytes)
        };
        report.released = self.release_unmarked_shared(scope)?;
        report.deferred = self.evacuate_shared_blocks(scope)?;
        self.queue_empty_shared_blocks(scope)?;
        // 发布出的通知必须在本 cycle 内被目标 owner 消费：lease 与 grace 结清得越早，旧 payload
        // 越早可以真正回收，termination 也才不会停在 forwarding work 上。
        let budget = ServiceBudget::pressure(u32::MAX, u64::MAX);
        for owner in scope {
            let (forwarded, consumed) = self.drain_inboxes(*owner, &budget, true)?;
            report.forwarded_messages += u64::from(forwarded);
            report.consumed_messages += u64::from(consumed);
        }
        let delta = self.shared_totals.since(before);
        report.forwarded = delta.forwards;
        report.forwarded_bytes = delta.forwarded_bytes;
        report.freed_bytes = delta.freed_bytes;
        report.empty_blocks = self.shared_registry.empty_block_count();
        Ok(report)
    }

    /// 把已封口、无存活 payload、无 pin/guard/grace 的共享 block 发成 HeapBlock 归还。
    fn queue_empty_shared_blocks(&mut self, scope: &[u32]) -> Result<(), RawInvariant> {
        let mut empty = Vec::new();
        for owner in scope {
            for (descriptor, record) in self.shared_registry.blocks_for_owner(*owner) {
                if record.is_empty() && record.extent.is_some() {
                    empty.push(record.block.id);
                }
                let _ = descriptor;
            }
        }
        for id in empty {
            self.queue_heap_block_return(id)?;
        }
        Ok(())
    }

    /// 判断 scope 是否覆盖全部 owner。
    fn scope_covers_all(scope: &[u32], owners: u32) -> bool {
        scope.len() == usize::try_from(owners).expect("owner 数适配 usize")
            && (0..owners).all(|owner| scope.contains(&owner))
    }

    /// 释放本 cycle 未标记的共享对象；返回真正释放的对象数。
    ///
    /// 每个对象先交还（`mark_returned`）再尝试释放：状态、旧 payload 与全部 lease（guard、pin、
    /// 票据、forwarding lease）必须同时归零，否则这一轮只标记交还，留给后续 cycle 重试；已经
    /// 交还的条目直接进入重试路径，不会因为「已交还」而被永久跳过。
    fn release_unmarked_shared(&mut self, scope: &[u32]) -> Result<u64, RawInvariant> {
        let mut released = 0_u64;
        for owner in scope {
            let handles: Vec<SharedHandle> = self
                .shared_registry
                .entries_for_owner(*owner)
                .map(|(handle, _)| handle)
                .collect();
            for handle in handles {
                let entry: SharedPayloadBlock = *self.shared_payload_block(handle)?;
                if !entry.returned {
                    if self
                        .shared_heap()?
                        .is_marked(handle)
                        .map_err(shared_heap_error)?
                    {
                        continue;
                    }
                    if entry.pending_forward.is_some() {
                        continue;
                    }
                    self.shared_registry.mark_returned(handle)?;
                }
                let slot = self
                    .shared_heap()?
                    .slot_record(handle)
                    .ok_or_else(|| RawInvariant::new("共享 handle slot 身份已过期"))?;
                if !Self::slot_releasable(&slot)? {
                    continue;
                }
                let bytes = entry.payload_bytes;
                self.shared_heap_mut()?
                    .release(handle)
                    .map_err(shared_heap_error)?;
                self.shared_registry
                    .note_payload_freed(entry.block.id.arena(), bytes)?;
                self.shared_registry.release_entry(handle)?;
                self.shared_totals.freed_bytes = self
                    .shared_totals
                    .freed_bytes
                    .checked_add(u64::from(bytes))
                    .ok_or_else(|| RawInvariant::new("共享释放字节数溢出"))?;
                released += 1;
            }
        }
        Ok(released)
    }

    /// 判断一个 slot 是否已经可以释放：状态、旧 payload 与全部 lease 必须同时归零。
    fn slot_releasable(slot: &SharedHandleSlot) -> Result<bool, RawInvariant> {
        let state = SharedSlotState::from_raw(slot.state)
            .ok_or_else(|| RawInvariant::new("共享 handle slot 状态判别值未登记"))?;
        Ok(state == SharedSlotState::Live
            && slot.old_payload == 0
            && slot.access_guards == 0
            && slot.pin_leases == 0
            && slot.mark_tickets == 0
            && slot.forwarding_leases == 0)
    }

    /// 搬迁「已释放字节不少于存活字节」的封口 block；返回因 pin 被推迟的 payload 数。
    ///
    /// 判据只用两个真实计数器，不引入新的契约参数：当一块里死掉的字节已经不少于还活着的字节，
    /// 把存活 payload 搬进正在填充的 block 就能让整块归还，收益不再需要靠猜。搬迁后的旧
    /// payload 仍要等 grace 结清，因此这一步只是「搬出来」，归零由消费路径完成。
    fn evacuate_shared_blocks(&mut self, scope: &[u32]) -> Result<u64, RawInvariant> {
        let mut blocks: Vec<(u32, u32)> = Vec::new();
        for owner in scope {
            for (descriptor, record) in self.shared_registry.blocks_for_owner(*owner) {
                if record.sealed
                    && record.live_payloads > 0
                    && record.dead_bytes > 0
                    && record.live_bytes <= record.dead_bytes
                {
                    blocks.push((descriptor, *owner));
                }
            }
        }
        blocks.sort_unstable();
        let mut deferred = 0_u64;
        for (descriptor, owner) in blocks {
            let handles: Vec<SharedHandle> = self
                .shared_registry
                .entries_for_owner(owner)
                .filter(|(_, entry)| entry.block.id.arena() == descriptor)
                .map(|(handle, _)| handle)
                .collect();
            for handle in handles {
                if self.shared_payload_block(handle)?.returned {
                    continue;
                }
                match self.forward_shared_payload(owner, handle)? {
                    SharedForwardOutcome::Forwarded(_) => {}
                    SharedForwardOutcome::Deferred(_) => deferred += 1,
                }
            }
        }
        Ok(deferred)
    }

    /// 管理权转移：共享 block 的 registry owner 与 card table manager 一起改到新 token。
    ///
    /// payload 不动，动的只是「谁拥有这些 block」：搬迁判据、mark ticket 路由与 CardMark 目标
    /// 全部读 registry/manager，因此两者必须在同一个线性化点一起改。card table 按 manager 扫描
    /// 而不是按共享 block 逐个改，是因为 LocalHeap arena 与共享 block 共用同一张表，且两者的
    /// batch 路由读的都是 `CardTable::manager`。
    ///
    /// 该 owner 名下没有共享 block 时直接返回：retire 的终点可以是 domain owner 这类不在世界
    /// owner 表里的注入终点，而 registry 的 owner 字段是稠密槽位编号，无法表达它——没有共享
    /// block 时就没有任何需要编号的管理权，也没有需要改写的 card table。
    pub(crate) fn handover_shared_blocks(
        &mut self,
        owner: u32,
        target: super::super::slab::OwnerToken,
    ) -> Result<u64, RawInvariant> {
        if self
            .shared_registry
            .blocks_for_owner(owner)
            .next()
            .is_none()
        {
            return Ok(0);
        }
        let target_index = self.manager_owner_index(target)?;
        let pending = self.managed_accounting[owner as usize].pending_return_bytes();
        if pending != 0 {
            self.managed_accounting[owner as usize].forward_pending(pending);
            self.managed_accounting[target_index as usize].stage_pending(pending);
        }
        let from = self.token(owner);
        let moved = self.shared_registry.handover_owner(owner, target_index);
        let tables = self.barrier_mut().handover_manager(from, target);
        debug_assert!(
            tables >= u32::try_from(moved).expect("共享 block 数适配 u32"),
            "每个共享 block 的 card table 都必须随管理权一起转移"
        );
        Ok(moved)
    }
}
