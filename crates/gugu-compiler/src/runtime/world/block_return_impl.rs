//! managed block return：构造、门禁、发布与消费。
//!
//! 四类 unit（`HeapBlock` / `HeapLineRun` / `HeapArena` / `LargeMapping`）全部经现有
//! inbox 走 `ReturnPending → OwnedFree`。同 owner 也发消息，不提供跳过 inbox 的本地捷径：
//! exactly-once 与账本只认 consume。

use super::RawWorld;
use super::heap_impl::heap_error;
use super::shared_heap_impl::is_shared_descriptor;
use crate::runtime::block_return_schema::HEAP_LINE_RUN_MIN_LINES;
use crate::runtime::edge_schema::EDGE_NO_JOB;
use crate::runtime::extent::ExtentOccupancy;
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
    OwnerId, OwnerToken, RawInvariant, Resolution, SlabDescriptorId, SlabGeneration,
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
        if kind == HeapArenaKind::Large {
            if let Some((start, span)) = self
                .heap(owner)?
                .large_span_covering(id)
                .map_err(heap_error)?
            {
                return self.queue_large_mapping_if_ready(owner, manager, id.arena(), start, span);
            }
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
        self.publish_message(owner, &message, shard, Some(FlushTrigger::GcHandoff))?;
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
                if message.kind != ReturnKind::HeapLineRun {
                    let source_owner = self.managed_owner_index(message.target.owner_id)?;
                    let dest_owner = self.managed_owner_index(target.owner_id)?;
                    let bytes = u64::from(message.bytes);
                    self.managed_accounting[source_owner as usize].forward_pending(bytes);
                    self.managed_accounting[source_owner as usize].release(bytes);
                    self.managed_accounting[dest_owner as usize].commit(bytes);
                    self.managed_accounting[dest_owner as usize].take_from_cache(bytes);
                    self.managed_accounting[dest_owner as usize].stage_pending(bytes);
                }
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
    /// live 按物理块计：Allocating/Candidate/Sweeping/Evacuating 的 LocalHeap 块，以及
    /// 仍有 payload 的共享块。ReturnPending 记在 pending，不计入 live。
    pub(crate) fn managed_ledger_invariant(&self, owner: u32) -> Result<(), RawInvariant> {
        let accounting = self.managed_accounting(owner);
        let mut live = 0_u64;
        if let Some(heaps) = self.local_heaps.as_ref()
            && let Some(heap) = heaps.get(owner as usize)
        {
            live = live.saturating_add(heap.live_managed_block_bytes());
        }
        let mut empty_shared = 0_u64;
        for (_, record) in self.shared_registry.blocks_for_owner(owner) {
            if record.extent.is_none() {
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
        self.managed_accounting[owner as usize].consume_pending(bytes);
        self.managed_accounting[owner as usize].park_reclaimable(bytes);
        self.heap_mut(heap_owner)?
            .release_block(id)
            .map_err(heap_error)?;
        let extent = self
            .heap(heap_owner)?
            .block_extent(id)
            .map_err(heap_error)?;
        self.extents.mark_return_queued(extent)?;
        let occupancy = ExtentOccupancy {
            live_slots: 0,
            queued_slots: 0,
            pending_returns: 0,
        };
        let completed = match self.poll_trim_extent(extent, occupancy)? {
            Ok(trimmed) => {
                self.release_managed_extent_bytes(owner, trimmed)?;
                true
            }
            Err(crate::runtime::extent::TrimBlocked::GracePending { .. }) => {
                self.pending_extent_trims
                    .push((extent, self.token(owner).owner_id));
                false
            }
            Err(blocked) => {
                return Err(RawInvariant::new(format!(
                    "managed block 归还被门禁拒绝：{}",
                    blocked.describe()
                )));
            }
        };
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

    fn consume_heap_arena(
        &mut self,
        owner: u32,
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
        let heap = self.heap(heap_owner)?;
        let committed = heap
            .committed_blocks_of(u64::from(descriptor))
            .map_err(heap_error)?;
        for index in committed {
            let id = ManagedBlockId::new(descriptor, index)?;
            let record = heap.block_record(id).map_err(heap_error)?;
            if HeapBlockState::from_raw(record.state) != Some(HeapBlockState::Free) {
                return Err(RawInvariant::new("HeapArena 归还时仍有非 Free block"));
            }
        }
        let bytes = u64::from(message.bytes);
        self.managed_accounting[owner as usize].consume_pending(bytes);
        self.managed_accounting[owner as usize].park_reclaimable(bytes);
        self.managed_arenas
            .retain(|entry| entry.descriptor != descriptor);
        Ok(true)
    }

    fn consume_large_mapping(
        &mut self,
        owner: u32,
        message: &ReturnMessage,
    ) -> Result<bool, RawInvariant> {
        let start = message.unit;
        let span = u32::from(message.integrity.class.raw());
        let descriptor = message.descriptor.raw();
        let arena = self.managed_arena_by_descriptor(descriptor)?;
        let heap_owner = arena.heap_owner;
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
            let extent = self
                .heap(heap_owner)?
                .block_extent(id)
                .map_err(heap_error)?;
            self.extents.mark_return_queued(extent)?;
            let occupancy = ExtentOccupancy {
                live_slots: 0,
                queued_slots: 0,
                pending_returns: 0,
            };
            match self.poll_trim_extent(extent, occupancy)? {
                Ok(trimmed) => self.release_managed_extent_bytes(owner, trimmed)?,
                Err(crate::runtime::extent::TrimBlocked::GracePending { .. }) => {
                    self.pending_extent_trims
                        .push((extent, self.token(owner).owner_id));
                }
                Err(blocked) => {
                    return Err(RawInvariant::new(format!(
                        "large mapping 归还被门禁拒绝：{}",
                        blocked.describe()
                    )));
                }
            }
        }
        let bytes = u64::from(message.bytes);
        self.managed_accounting[owner as usize].consume_pending(bytes);
        self.managed_accounting[owner as usize].park_reclaimable(bytes);
        Ok(true)
    }

    fn consume_shared_heap_block(
        &mut self,
        owner: u32,
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
        let bytes = u64::from(message.bytes);
        self.managed_accounting[owner as usize].consume_pending(bytes);
        self.managed_accounting[owner as usize].park_reclaimable(bytes);
        let extent = record
            .extent
            .ok_or_else(|| RawInvariant::new("共享 HeapBlock 尚未绑定 managed extent"))?;
        self.extents.mark_return_queued(extent)?;
        self.shared_registry.drop_block(id.arena())?;
        let occupancy = ExtentOccupancy {
            live_slots: 0,
            queued_slots: 0,
            pending_returns: 0,
        };
        match self.poll_trim_extent(extent, occupancy)? {
            Ok(trimmed) => {
                self.release_managed_extent_bytes(owner, trimmed)?;
                Ok(true)
            }
            Err(crate::runtime::extent::TrimBlocked::GracePending { .. }) => {
                self.pending_extent_trims
                    .push((extent, self.token(owner).owner_id));
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
        let owner = record.owner;
        let manager = self.token(owner);
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
        self.managed_accounting[owner as usize].stage_pending(u64::from(GC_BLOCK_BYTES));
        let shard = ShardIndex::from_raw(owner % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("共享 HeapBlock 归还的 shard 编号越界"))?;
        self.publish_message(owner, &message, shard, Some(FlushTrigger::GcHandoff))?;
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
        self.managed_accounting[owner as usize].take_from_cache(GC_ARENA_BYTES);
        self.managed_accounting[owner as usize].stage_pending(GC_ARENA_BYTES);
        let shard = ShardIndex::from_raw(owner % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("HeapArena 归还的 shard 编号越界"))?;
        self.publish_message(owner, &message, shard, Some(FlushTrigger::GcHandoff))?;
        Ok(())
    }

    fn release_managed_extent_bytes(&mut self, owner: u32, bytes: u64) -> Result<(), RawInvariant> {
        self.managed_accounting[owner as usize].release(bytes);
        Ok(())
    }

    fn managed_owner_index(&self, owner_id: OwnerId) -> Result<u32, RawInvariant> {
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
        self.publish_message(owner, &message, shard, Some(FlushTrigger::GcHandoff))?;
        for step in 0..span {
            let id = ManagedBlockId::new(descriptor, start + step)?;
            self.heap_mut(owner)?
                .mark_return_queued(id)
                .map_err(heap_error)?;
        }
        Ok(())
    }
}
