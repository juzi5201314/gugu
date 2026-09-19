//! managed block return：构造、门禁、发布与消费。
//!
//! 四类 unit（`HeapBlock` / `HeapLineRun` / `HeapArena` / `LargeMapping`）全部经现有
//! inbox 走 `ReturnPending → OwnedFree`。同 owner 也发消息，不提供跳过 inbox 的本地捷径：
//! exactly-once 与账本只认 consume。

use super::PendingExtentTrim;
use super::RawWorld;
use super::heap_impl::heap_error;
use super::shared_heap_impl::is_shared_descriptor;
use crate::runtime::block_return_schema::HEAP_LINE_RUN_MIN_LINES;
use crate::runtime::edge_schema::EDGE_NO_JOB;
use crate::runtime::extent::{ExtentId, ExtentOccupancy, TrimBlocked};
use crate::runtime::gc_metadata_contract::{GC_ARENA_BYTES, GC_BLOCK_BYTES, GC_LINE_BYTES};
use crate::runtime::inbox::ShardIndex;
use crate::runtime::local_heap::{HeapArenaKind, ManagedBlockId};
use crate::runtime::local_heap_schema::{
    HEAP_BLOCK_RETURN_QUEUED, HEAP_BLOCKS_PER_ARENA, HEAP_LINES_PER_BLOCK, HeapBlockState,
};
use crate::runtime::message::{
    FlushTrigger, IntegrityTag, MessageState, ReturnKind, ReturnMessage,
};
use crate::runtime::size_class::RuntimeSizeClassId;
use crate::runtime::slab::{
    MemoryDomainId, OwnerId, OwnerToken, RawInvariant, Resolution, SlabDescriptorId, SlabGeneration,
};

impl RawWorld {
    /// 发布一条 `HeapBlock` 归还：门禁全部成立才发消息，并把 `GC_BLOCK_BYTES` 记入 pending。
    pub(crate) fn queue_heap_block_return(
        &mut self,
        id: ManagedBlockId,
    ) -> Result<(), RawInvariant> {
        if is_shared_descriptor(id.arena()) {
            return self.queue_shared_heap_block_return(id);
        }
        let arena = self.managed_arena_by_descriptor(id.arena())?;
        let owner = arena.heap_owner;
        let manager = arena.manager;
        let kind = arena.kind;
        let record = self.heap(owner)?.block_record(id).map_err(heap_error)?;
        if record.reserved & HEAP_BLOCK_RETURN_QUEUED != 0 {
            return Ok(());
        }
        if HeapBlockState::from_raw(record.state) != Some(HeapBlockState::ReturnPending) {
            return Err(RawInvariant::new(
                "queue_heap_block_return 要求块已处于 ReturnPending",
            ));
        }
        if kind == HeapArenaKind::Large
            && let Some((start, span)) = self
                .heap(owner)?
                .large_span_covering(id)
                .map_err(heap_error)?
        {
            return self.queue_large_mapping_if_ready(owner, manager, id.arena(), start, span);
        }
        let (pinned, resources) = self
            .heap(owner)?
            .block_pin_and_resource_counts(id)
            .map_err(heap_error)?;
        let live_lines = self.heap(owner)?.block_live_lines(id).map_err(heap_error)?;
        if record.incoming_leases != 0
            || record.allocator_leases != 0
            || record.scanner_leases != 0
            || record.evacuation_leases != 0
            || pinned != 0
            || resources != 0
            || live_lines != 0
            || record.candidate_job != EDGE_NO_JOB
        {
            return Err(RawInvariant::new("HeapBlock 归还门禁未全部归零，拒绝发布"));
        }
        let class = u16::try_from(self.heap_block_class)
            .map_err(|_| RawInvariant::new("heap block class 越过 integrity 车道"))?;
        let mut message = ReturnMessage {
            next: None,
            target: manager,
            kind: ReturnKind::HeapBlock,
            descriptor: SlabDescriptorId::from_raw(id.arena()),
            unit: id.index(),
            bytes: GC_BLOCK_BYTES,
            source_epoch: self.epoch,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(u64::from(record.generation)),
                class: RuntimeSizeClassId::from_raw(class),
                owner_id: manager.owner_id,
                route_key: manager.route_key,
                checksum: 0,
            },
        };
        message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
        self.managed_accounting[owner as usize].stage_pending(u64::from(GC_BLOCK_BYTES));
        let shard = ShardIndex::from_raw(owner % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("HeapBlock 归还的 shard 编号越界"))?;
        self.publish_staged_managed_return(owner, &message, shard, u64::from(GC_BLOCK_BYTES))?;
        self.heap_mut(owner)?
            .mark_return_queued(id)
            .map_err(heap_error)?;
        Ok(())
    }

    /// 消费四类 managed return；重复 consume 是 exactly-once 失败。
    pub(super) fn service_managed_return(
        &mut self,
        owner: u32,
        message: &ReturnMessage,
    ) -> Result<bool, RawInvariant> {
        message.integrity.verify(&self.integrity_secret, message)?;
        if message.source_epoch > self.epoch {
            return Err(RawInvariant::new(
                "managed return 的 source epoch 越过当前 epoch",
            ));
        }
        match self.directory.resolve(&message.target) {
            Resolution::Match => {}
            Resolution::Forward(target) => {
                // 账本永远跟随页的提交 owner，不随消息路由迁移：这里只把消息转投给新的 manager
                // 上下文，pending 字节仍在物理 owner 名下等待它的 consume 结清。
                self.forward_message(message, target)?;
                return Ok(false);
            }
            Resolution::Retired | Resolution::Unknown => {
                return Err(RawInvariant::new(
                    "managed return 的目标 owner 已 retire 或未知",
                ));
            }
        }
        if self.token(owner) != message.target {
            return Err(RawInvariant::new(
                "managed return 投递到非目标 owner 的上下文",
            ));
        }
        match message.kind {
            ReturnKind::HeapBlock => self.consume_heap_block(owner, message),
            ReturnKind::HeapLineRun => self.consume_heap_line_run(owner, message),
            ReturnKind::HeapArena => self.consume_heap_arena(owner, message),
            ReturnKind::LargeMapping => self.consume_large_mapping(owner, message),
            _ => Err(RawInvariant::new(
                "service_managed_return 收到非 managed kind",
            )),
        }
    }

    /// 校验账本：committed 等于 pending、reclaimable、cache 与仍占用的 managed 块之和。
    ///
    /// live 按物理块计：Allocating/Candidate/Sweeping/Evacuating 的 LocalHeap 块，以及仍有 payload
    /// 的共享块。ReturnPending 记在 pending，不计入 live。共享块记在**提交了它物理页的 owner**
    /// 名下，而不是当前的 manager：账本跟随页，不跟随消息路由。
    pub(crate) fn managed_ledger_invariant(&self, owner: u32) -> Result<(), RawInvariant> {
        let accounting = self.managed_accounting(owner);
        let mut live = 0_u64;
        if let Some(heaps) = self.local_heaps.as_ref()
            && let Some(heap) = heaps.get(owner as usize)
        {
            live = live.saturating_add(heap.live_managed_block_bytes());
        }
        let mut empty_shared = 0_u64;
        for (_, record) in self.shared_registry.block_records() {
            let Some(extent) = record.extent else {
                continue;
            };
            if self.managed_extent_owner_index(extent)? != owner {
                continue;
            }
            if record.is_empty() {
                empty_shared = empty_shared.saturating_add(u64::from(GC_BLOCK_BYTES));
            } else {
                live = live.saturating_add(u64::from(GC_BLOCK_BYTES));
            }
        }
        let classified = accounting.pending_return_bytes()
            + accounting.reclaimable_bytes()
            + accounting.owner_cache_bytes()
            + live
            + empty_shared;
        if accounting.committed_bytes() != classified {
            return Err(RawInvariant::new(format!(
                "managed 账本分类不互斥：committed {}，分类合计 {classified}",
                accounting.committed_bytes()
            )));
        }
        Ok(())
    }
}

impl RawWorld {
    /// 消费一条 `HeapBlock` 归还：状态结算、物理页结清与账本扣减。
    ///
    /// 账本与 extent 一律按**物理 owner**（`arena.heap_owner`）结算，而不是 serviced 的 manager
    /// 上下文：归还消息可以转投给新的 manager，物理页却仍由提交它的 owner 持有。
    fn consume_heap_block(
        &mut self,
        owner: u32,
        message: &ReturnMessage,
    ) -> Result<bool, RawInvariant> {
        let id = ManagedBlockId::new(message.descriptor.raw(), message.unit)?;
        if is_shared_descriptor(id.arena()) {
            return self.consume_shared_heap_block(owner, message, id);
        }
        let arena = self.managed_arena_by_descriptor(id.arena())?;
        let heap_owner = arena.heap_owner;
        let record = self
            .heap(heap_owner)?
            .block_record(id)
            .map_err(heap_error)?;
        if HeapBlockState::from_raw(record.state) != Some(HeapBlockState::ReturnPending) {
            return Err(RawInvariant::new(
                "HeapBlock 不处于 ReturnPending，归还只能发生一次",
            ));
        }
        if u64::from(record.generation) != record_generation(message) {
            return Err(RawInvariant::new("HeapBlock 归还引用过期 generation"));
        }
        let mut record = record;
        record.state = HeapBlockState::OwnedFree.raw();
        self.heap_mut(heap_owner)?
            .update_block_record(id, record)
            .map_err(heap_error)?;
        let bytes = u64::from(message.bytes);
        self.managed_accounting[heap_owner as usize].consume_pending(bytes);
        self.managed_accounting[heap_owner as usize].park_reclaimable(bytes);
        self.heap_mut(heap_owner)?
            .release_block(id)
            .map_err(heap_error)?;
        let completed = self.settle_managed_block_extent(heap_owner, id)?;
        self.heap_mut(heap_owner)?
            .clear_return_queued(id)
            .map_err(heap_error)?;
        self.maybe_queue_empty_arena(id.arena())?;
        Ok(completed)
    }

    fn consume_heap_line_run(
        &mut self,
        _owner: u32,
        message: &ReturnMessage,
    ) -> Result<bool, RawInvariant> {
        let arena_desc = message.descriptor.raw();
        let block_index = message.unit >> 8;
        let start_line = message.unit & 0xFF;
        let line_count = u32::from(message.integrity.class.raw());
        debug_assert!(block_index < HEAP_BLOCKS_PER_ARENA);
        debug_assert!(start_line < HEAP_LINES_PER_BLOCK);
        let id = ManagedBlockId::new(arena_desc, block_index)?;
        let arena = self.managed_arena_by_descriptor(arena_desc)?;
        let heap_owner = arena.heap_owner;
        let record = self
            .heap(heap_owner)?
            .block_record(id)
            .map_err(heap_error)?;
        if HeapBlockState::from_raw(record.state) != Some(HeapBlockState::Allocating) {
            return Err(RawInvariant::new(
                "HeapLineRun 只能归还仍处于 Allocating 的半空块",
            ));
        }
        self.heap_mut(heap_owner)?
            .restore_queued_line_run(id, start_line, line_count)
            .map_err(heap_error)?;
        Ok(true)
    }

    /// 消费一条 `HeapArena` 整区归还：复核整区已空、结清剩余物理页并摘除 arena。
    ///
    /// 账本按物理 owner：`_owner`（serviced manager 上下文）只用于消息路由，不参与结算。
    fn consume_heap_arena(
        &mut self,
        _owner: u32,
        message: &ReturnMessage,
    ) -> Result<bool, RawInvariant> {
        let descriptor = message.descriptor.raw();
        let arena = *self.managed_arena_by_descriptor(descriptor)?;
        let heap_owner = arena.heap_owner;
        if matches!(arena.kind, HeapArenaKind::Resource | HeapArenaKind::Pinned) {
            return Err(RawInvariant::new(
                "Resource/Pinned arena 禁止发布 HeapArena 整区消息",
            ));
        }
        let committed = self
            .heap(heap_owner)?
            .committed_blocks_of(u64::from(descriptor))
            .map_err(heap_error)?;
        for index in committed {
            let id = ManagedBlockId::new(descriptor, index)?;
            let record = self
                .heap(heap_owner)?
                .block_record(id)
                .map_err(heap_error)?;
            if HeapBlockState::from_raw(record.state) != Some(HeapBlockState::Free) {
                return Err(RawInvariant::new("HeapArena 归还时仍有非 Free block"));
            }
            // 从未激活、也从未归还过的块仍持有 extent：这里必须结清，否则 arena 摘除后没有任何
            // 记录还能引用这份物理页与账本字节。
            if self
                .heap(heap_owner)?
                .block_has_committed_pages(id)
                .map_err(heap_error)?
            {
                self.settle_managed_block_extent(heap_owner, id)?;
            }
        }
        self.heap_mut(heap_owner)?
            .retire_arena(descriptor)
            .map_err(heap_error)?;
        self.managed_arenas
            .retain(|entry| entry.descriptor != descriptor);
        Ok(true)
    }

    /// 消费一条 `LargeMapping` 归还：span 的每个成员单独结清物理页，账本按物理 owner 结算。
    fn consume_large_mapping(
        &mut self,
        _owner: u32,
        message: &ReturnMessage,
    ) -> Result<bool, RawInvariant> {
        let start = message.unit;
        let span = u32::from(message.integrity.class.raw());
        let descriptor = message.descriptor.raw();
        let heap_owner = self.managed_arena_by_descriptor(descriptor)?.heap_owner;
        for step in 0..span {
            let id = ManagedBlockId::new(descriptor, start + step)?;
            let record = self
                .heap(heap_owner)?
                .block_record(id)
                .map_err(heap_error)?;
            if HeapBlockState::from_raw(record.state) != Some(HeapBlockState::ReturnPending) {
                return Err(RawInvariant::new(
                    "LargeMapping 覆盖的 block 必须全部处于 ReturnPending",
                ));
            }
            let mut record = record;
            record.state = HeapBlockState::OwnedFree.raw();
            self.heap_mut(heap_owner)?
                .update_block_record(id, record)
                .map_err(heap_error)?;
            self.heap_mut(heap_owner)?
                .release_block(id)
                .map_err(heap_error)?;
            self.heap_mut(heap_owner)?
                .clear_return_queued(id)
                .map_err(heap_error)?;
            // 每个 span 成员的物理页单独结清：一个成员可能即时 trim，另一个进入 grace。
            self.settle_managed_block_extent(heap_owner, id)?;
        }
        let bytes = u64::from(message.bytes);
        self.managed_accounting[heap_owner as usize].consume_pending(bytes);
        self.managed_accounting[heap_owner as usize].park_reclaimable(bytes);
        // span 的结清可能让整区变空；`HeapBlock` 的消费点之外没有别的触发者，遗漏这里会让
        // 只由 large mapping 腾空的 arena 永远退不掉。
        self.maybe_queue_empty_arena(descriptor)?;
        Ok(true)
    }

    /// 把一个 managed 块的 extent 交给 trim 门禁：发布 `ReturnQueued`、解绑块上的 extent，
    /// 过 grace 后把字节从**物理 owner**的 committed 扣掉。
    ///
    /// 解绑必须在门禁之前完成：extent 一旦交给 trim，块就不再拥有物理页；复用这种块时由
    /// `settle_activated_blocks` 重新提交新 extent。返回 `true` 表示本次真正完成 decommit。
    fn settle_managed_block_extent(
        &mut self,
        heap_owner: u32,
        id: ManagedBlockId,
    ) -> Result<bool, RawInvariant> {
        let extent = self
            .heap(heap_owner)?
            .block_extent(id)
            .map_err(heap_error)?;
        self.heap_mut(heap_owner)?
            .detach_block_extent(id)
            .map_err(heap_error)?;
        self.extents.mark_return_queued(extent)?;
        let occupancy = ExtentOccupancy {
            live_slots: 0,
            queued_slots: 0,
            pending_returns: 0,
        };
        match self.poll_trim_extent(extent, occupancy)? {
            Ok(trimmed) => {
                self.managed_accounting[heap_owner as usize].release(trimmed);
                Ok(true)
            }
            Err(TrimBlocked::GracePending { .. }) => {
                self.pending_extent_trims.push(PendingExtentTrim {
                    extent,
                    owner_id: self.token(heap_owner).owner_id,
                    domain: MemoryDomainId::MANAGED_LOCAL,
                });
                Ok(false)
            }
            Err(blocked) => Err(RawInvariant::new(format!(
                "managed block 归还被门禁拒绝：{}",
                blocked.describe()
            ))),
        }
    }

    /// 返回一个 managed extent 的物理 owner 槽位：账本永远跟随页的提交者，而不是 manager。
    fn managed_extent_owner_index(&self, extent: ExtentId) -> Result<u32, RawInvariant> {
        let token = self
            .extents
            .descriptor(extent)
            .ok_or_else(|| RawInvariant::new("managed extent 描述缺失"))?
            .owner;
        self.managed_owner_index(token.owner_id)
    }

    /// 消费一条共享 `HeapBlock` 归还：payload 已空，结清 extent 并按物理 owner 扣账。
    fn consume_shared_heap_block(
        &mut self,
        _owner: u32,
        message: &ReturnMessage,
        id: ManagedBlockId,
    ) -> Result<bool, RawInvariant> {
        let record = *self
            .shared_registry
            .block_record(id.arena())
            .ok_or_else(|| RawInvariant::new("共享 HeapBlock 引用未知 block"))?;
        if u64::from(record.block.generation) != record_generation(message) {
            return Err(RawInvariant::new("共享 HeapBlock 归还引用过期 generation"));
        }
        if !record.is_empty() {
            return Err(RawInvariant::new("共享 HeapBlock 仍有存活 payload"));
        }
        let extent = record
            .extent
            .ok_or_else(|| RawInvariant::new("共享 HeapBlock 尚未绑定 managed extent"))?;
        // 账本跟随页的提交 owner：manager 只用于路由，不参与字节账本。
        let physical = self.managed_extent_owner_index(extent)?;
        let bytes = u64::from(message.bytes);
        self.managed_accounting[physical as usize].consume_pending(bytes);
        self.managed_accounting[physical as usize].park_reclaimable(bytes);
        self.extents.mark_return_queued(extent)?;
        self.shared_registry.drop_block(id.arena())?;
        let occupancy = ExtentOccupancy {
            live_slots: 0,
            queued_slots: 0,
            pending_returns: 0,
        };
        match self.poll_trim_extent(extent, occupancy)? {
            Ok(trimmed) => {
                self.managed_accounting[physical as usize].release(trimmed);
                Ok(true)
            }
            Err(TrimBlocked::GracePending { .. }) => {
                self.pending_extent_trims.push(PendingExtentTrim {
                    extent,
                    owner_id: self.token(physical).owner_id,
                    domain: MemoryDomainId::MANAGED_SHARED,
                });
                Ok(false)
            }
            Err(blocked) => Err(RawInvariant::new(format!(
                "共享 HeapBlock 归还被门禁拒绝：{}",
                blocked.describe()
            ))),
        }
    }

    fn queue_shared_heap_block_return(&mut self, id: ManagedBlockId) -> Result<(), RawInvariant> {
        let record = *self
            .shared_registry
            .block_record(id.arena())
            .ok_or_else(|| RawInvariant::new("共享 HeapBlock 引用未知 block"))?;
        if !record.is_empty() {
            return Err(RawInvariant::new("共享空块归还时仍有存活 payload"));
        }
        let extent = record
            .extent
            .ok_or_else(|| RawInvariant::new("共享 HeapBlock 尚未绑定 managed extent"))?;
        // `record.owner` 是 manager，只决定消息路由；字节账本与 producer staging 都属于提交
        // 物理页的 owner，一次管理权转移不会让另一份页账本凭空出现。
        let manager = self.token(record.owner);
        let physical = self.managed_extent_owner_index(extent)?;
        let class = u16::try_from(self.heap_block_class)
            .map_err(|_| RawInvariant::new("heap block class 越过 integrity 车道"))?;
        let mut message = ReturnMessage {
            next: None,
            target: manager,
            kind: ReturnKind::HeapBlock,
            descriptor: SlabDescriptorId::from_raw(id.arena()),
            unit: 0,
            bytes: GC_BLOCK_BYTES,
            source_epoch: self.epoch,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(u64::from(record.block.generation)),
                class: RuntimeSizeClassId::from_raw(class),
                owner_id: manager.owner_id,
                route_key: manager.route_key,
                checksum: 0,
            },
        };
        message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
        self.managed_accounting[physical as usize].stage_pending(u64::from(GC_BLOCK_BYTES));
        let shard = ShardIndex::from_raw(physical % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("共享 HeapBlock 归还的 shard 编号越界"))?;
        self.publish_staged_managed_return(physical, &message, shard, u64::from(GC_BLOCK_BYTES))?;
        Ok(())
    }

    fn maybe_queue_empty_arena(&mut self, descriptor: u32) -> Result<(), RawInvariant> {
        let arena = *self.managed_arena_by_descriptor(descriptor)?;
        if matches!(arena.kind, HeapArenaKind::Resource | HeapArenaKind::Pinned) {
            return Ok(());
        }
        let heap = self.heap(arena.heap_owner)?;
        let committed = heap
            .committed_blocks_of(u64::from(descriptor))
            .map_err(heap_error)?;
        if committed.len() != usize::try_from(HEAP_BLOCKS_PER_ARENA).expect("64 适配 usize") {
            return Ok(());
        }
        for index in committed {
            let id = ManagedBlockId::new(descriptor, index)?;
            let record = heap.block_record(id).map_err(heap_error)?;
            if HeapBlockState::from_raw(record.state) != Some(HeapBlockState::Free) {
                return Ok(());
            }
        }
        let manager = arena.manager;
        let owner = arena.heap_owner;
        let mut message = ReturnMessage {
            next: None,
            target: manager,
            kind: ReturnKind::HeapArena,
            descriptor: SlabDescriptorId::from_raw(descriptor),
            unit: 0,
            bytes: u32::try_from(GC_ARENA_BYTES).expect("arena 字节适配 u32"),
            source_epoch: self.epoch,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(1),
                class: RuntimeSizeClassId::from_raw(0),
                owner_id: manager.owner_id,
                route_key: manager.route_key,
                checksum: 0,
            },
        };
        message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
        // `HeapArena` 只表达「整区已空、可以摘除」，不搬运字节：每个 block 的 32 KiB 已经由
        // `settle_managed_block_extent` 结清；再记一次 2 MiB 会把没有 committed 对应的字节加进
        // cache，`managed_ledger_invariant` 随即失败。
        let shard = ShardIndex::from_raw(owner % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("HeapArena 归还的 shard 编号越界"))?;
        self.publish_message(owner, &message, shard, Some(FlushTrigger::GcHandoff))?;
        Ok(())
    }

    /// 发布一条已经 stage 过 pending 的 managed return；发布失败时回滚刚记入的 pending 字节。
    ///
    /// `publish_message` 会因为 node pool 耗尽这类契约违约失败；若此时 pending 已经记入账本，
    /// 世界上就多出一份没有消息、也没有 `HEAP_BLOCK_RETURN_QUEUED` 位的字节。
    fn publish_staged_managed_return(
        &mut self,
        owner: u32,
        message: &ReturnMessage,
        shard: ShardIndex,
        pending_bytes: u64,
    ) -> Result<(), RawInvariant> {
        match self.publish_message(owner, message, shard, Some(FlushTrigger::GcHandoff)) {
            Ok(_) => Ok(()),
            Err(error) => {
                self.managed_accounting[owner as usize].cancel_pending(pending_bytes);
                Err(error)
            }
        }
    }

    /// 把一个 owner 的全局目录编号映射到 raw owner 槽位。
    pub(super) fn managed_owner_index(&self, owner_id: OwnerId) -> Result<u32, RawInvariant> {
        self.owners
            .iter()
            .position(|owner| owner.token().owner_id == owner_id)
            .and_then(|index| u32::try_from(index).ok())
            .ok_or_else(|| RawInvariant::new("managed 账本找不到对应 owner 下标"))
    }
}

fn record_generation(message: &ReturnMessage) -> u64 {
    message.integrity.generation.raw()
}

impl RawWorld {
    /// 扫描半空块，把长度 ≥ `HEAP_LINE_RUN_MIN_LINES` 的连续 `LINE_FREE` 区间发成 `HeapLineRun`。
    ///
    /// line-run 只改变 bump 可用区间，不移动物理页，因此不入 `ManagedAccounting`。
    /// exactly-once 由 `LINE_QUEUED` 表达。
    pub(super) fn queue_line_runs_after_sweep(
        &mut self,
        id: ManagedBlockId,
    ) -> Result<(), RawInvariant> {
        let arena = self.managed_arena_by_descriptor(id.arena())?;
        let owner = arena.heap_owner;
        let manager = arena.manager;
        let live_lines = self.heap(owner)?.block_live_lines(id).map_err(heap_error)?;
        if live_lines == 0 {
            return Ok(());
        }
        let record = self.heap(owner)?.block_record(id).map_err(heap_error)?;
        if HeapBlockState::from_raw(record.state) != Some(HeapBlockState::Allocating) {
            return Ok(());
        }
        let runs = self
            .heap(owner)?
            .free_line_runs(id, HEAP_LINE_RUN_MIN_LINES)
            .map_err(heap_error)?;
        for (start_line, count) in runs {
            let bytes = count.saturating_mul(GC_LINE_BYTES);
            let class = u16::try_from(count)
                .map_err(|_| RawInvariant::new("line-run 长度越过 integrity 车道"))?;
            let unit = (id.index() << 8) | start_line;
            let mut message = ReturnMessage {
                next: None,
                target: manager,
                kind: ReturnKind::HeapLineRun,
                descriptor: SlabDescriptorId::from_raw(id.arena()),
                unit,
                bytes,
                source_epoch: self.epoch,
                state: MessageState::Staged,
                integrity: IntegrityTag {
                    generation: SlabGeneration::from_raw(u64::from(record.generation)),
                    class: RuntimeSizeClassId::from_raw(class),
                    owner_id: manager.owner_id,
                    route_key: manager.route_key,
                    checksum: 0,
                },
            };
            message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
            let shard = ShardIndex::from_raw(owner % super::super::OWNER_INBOX_SHARDS)
                .ok_or_else(|| RawInvariant::new("HeapLineRun 归还的 shard 编号越界"))?;
            self.publish_message(owner, &message, shard, Some(FlushTrigger::GcHandoff))?;
            self.heap_mut(owner)?
                .mark_line_run_queued(id, start_line, count)
                .map_err(heap_error)?;
        }
        Ok(())
    }

    /// 连续 empty 的 large span 全部 ReturnPending 后发一条 `LargeMapping`。
    fn queue_large_mapping_if_ready(
        &mut self,
        owner: u32,
        manager: OwnerToken,
        descriptor: u32,
        start: u32,
        span: u32,
    ) -> Result<(), RawInvariant> {
        let heap = self.heap(owner)?;
        let mut generation = 0_u32;
        for step in 0..span {
            let id = ManagedBlockId::new(descriptor, start + step)?;
            let record = heap.block_record(id).map_err(heap_error)?;
            if HeapBlockState::from_raw(record.state) != Some(HeapBlockState::ReturnPending) {
                return Ok(());
            }
            if record.reserved & HEAP_BLOCK_RETURN_QUEUED != 0 {
                return Ok(());
            }
            if step == 0 {
                generation = record.generation;
            }
        }
        let bytes = span.saturating_mul(GC_BLOCK_BYTES);
        let class = u16::try_from(span)
            .map_err(|_| RawInvariant::new("large mapping span 越过 integrity 车道"))?;
        let mut message = ReturnMessage {
            next: None,
            target: manager,
            kind: ReturnKind::LargeMapping,
            descriptor: SlabDescriptorId::from_raw(descriptor),
            unit: start,
            bytes,
            source_epoch: self.epoch,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(u64::from(generation)),
                class: RuntimeSizeClassId::from_raw(class),
                owner_id: manager.owner_id,
                route_key: manager.route_key,
                checksum: 0,
            },
        };
        message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
        self.managed_accounting[owner as usize].stage_pending(u64::from(bytes));
        let shard = ShardIndex::from_raw(owner % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("LargeMapping 归还的 shard 编号越界"))?;
        self.publish_staged_managed_return(owner, &message, shard, u64::from(bytes))?;
        for step in 0..span {
            let id = ManagedBlockId::new(descriptor, start + step)?;
            self.heap_mut(owner)?
                .mark_return_queued(id)
                .map_err(heap_error)?;
        }
        Ok(())
    }
}
