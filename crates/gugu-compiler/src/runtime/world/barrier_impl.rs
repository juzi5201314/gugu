//! `RawWorld` 上的 barrier 接入：arena 登记、六个 flush 触发点与 `CardMarkBatch` 消费。
//!
//! 接入遵循三条归属规则：
//!
//! 1. mutator 只写自己的 processor-local 账本；processor 是 arena allocation owner 时可直接
//!    合并写 card table，否则必须发布 `CardMarkBatch` 给 arena owner。
//! 2. card table 的任何写入都发生在 arena owner 上下文；batch 用 arena descriptor 与
//!    generation 定位表，错配进入 `RuntimeInvariant`。
//! 3. buffer 满、processor 交接、进入 `ForeignBridge`、memory pressure、minor stop 请求与
//!    producer stop gate 六个触发点都调用 `flush_barrier`，且 flush 只发布此前的记账。

use super::super::barrier::{
    BarrierFlushReason, BarrierPlane, BarrierSite, CardMarkDraft, HybridBarrierOutcome,
};
use super::super::gc_metadata_contract::GC_ARENA_BYTES;
use super::super::inbox::ShardIndex;
use super::super::local_heap::BlockRef;
use super::super::message::{
    CardMarkBatch, FlushTrigger, IntegrityTag, MessageState, ProducerStaging, stage_card_mark,
};
use super::super::slab::{MemoryDomainId, OwnerToken, RawInvariant, SlabDescriptorId};
use super::RawWorld;

/// barrier 触发点的统计；进入契约 pressure 视图与 dump。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BarrierStats {
    /// processor-local 记账次数。
    pub(crate) card_marks: u64,
    /// dedup 命中次数。
    pub(crate) slot_reuses: u64,
    /// 已发布的跨 block edge delta 总数。
    pub(crate) edge_published: u64,
    /// 已发布的总批次（card batch 加 edge 批次）。
    pub(crate) published_batches: u64,
    /// 已消费 batch 数。
    pub(crate) consumed_batches: u64,
    /// 全部 flush 次数。
    pub(crate) flushes: u64,
    /// 未产生任何 batch 的 flush 次数。
    pub(crate) empty_flushes: u64,
    /// 六个原因各自的 flush 次数。
    pub(crate) by_reason: [u64; 6],
    /// edge summary 中尚未取出的 delta 数。
    pub(crate) edge_pending: u64,
}

impl RawWorld {
    /// 返回 barrier 平面。
    pub(crate) const fn barrier(&self) -> &BarrierPlane {
        &self.barrier
    }

    /// 返回可变的 barrier 平面。
    pub(crate) fn barrier_mut(&mut self) -> &mut BarrierPlane {
        &mut self.barrier
    }

    /// 登记一个 managed arena 的 card table；同一 descriptor 重复登记保持既有表。
    ///
    /// arena 的物理页由 extent 层提交，card table 只登记元数据；descriptor 由世界级稠密表
    /// 分配，因此 card batch、mark ticket 与 block 身份共享同一身份空间。
    pub(crate) fn register_managed_arena(
        &mut self,
        owner: u32,
        arena_descriptor: u32,
        arena_generation: u32,
    ) -> Result<(), RawInvariant> {
        let token = self.token(owner);
        self.barrier.register_arena(
            u64::from(arena_descriptor),
            token,
            arena_generation,
            GC_ARENA_BYTES,
        )?;
        Ok(())
    }

    /// mutator 上下文：在一个 processor 上执行一条 hybrid barrier。
    ///
    /// buffer 满或站点 epoch 落后于平面时返回的 flush 原因必须由调用方在 region 外补容量，
    /// 不能就地扩容，也不能丢弃账本内容。
    pub(crate) fn perform_barrier(
        &mut self,
        processor: usize,
        site: BarrierSite,
    ) -> Result<HybridBarrierOutcome, RawInvariant> {
        self.barrier.perform_barrier(processor, site)
    }

    /// 推进一个 cycle 边界：heap、barrier 与 mark 的 epoch 由这里唯一前进。
    ///
    /// 三处必须始终相等，因此只有这一个入口会改 epoch：barrier 的 card 键按 cycle epoch 记账、
    /// mark 的 ticket 带 cycle 与 topology、候选的决议按块世代核对，任何一处独立前进都会让跨平面
    /// 的身份校验对不上真实周期。barrier 仍有未发布键时拒绝前进（由调用方先 flush 再重试）。
    pub(crate) fn advance_cycle_epoch(&mut self, owner: u32) -> Result<u64, RawInvariant> {
        let next = self
            .cycle_epoch
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("cycle epoch 溢出"))?;
        self.advance_barrier_epoch(owner, next)?;
        self.cycle_epoch = next;
        Ok(next)
    }

    /// 返回 barrier 平面记账用的 cycle epoch；它必须与 `cycle_epoch` 相等。
    pub(crate) fn barrier_cycle_epoch(&self) -> u64 {
        self.barrier.cycle_epoch()
    }

    /// 返回世界唯一的 cycle epoch。
    pub(crate) fn cycle_epoch(&self) -> u64 {
        self.cycle_epoch
    }

    /// 推进 barrier 平面的 cycle epoch，并把旧 cycle 的 card batch 交给 arena owner。
    ///
    /// epoch 前进是 cycle 边界：全部 processor 的 remembered set 必须先冲刷并发布，任何
    /// 已记账的 card 键都不允许因为 epoch 前进而消失。
    pub(crate) fn advance_barrier_epoch(
        &mut self,
        owner: u32,
        cycle_epoch: u64,
    ) -> Result<u32, RawInvariant> {
        let drafts = self
            .barrier
            .advance_epoch(cycle_epoch, BarrierFlushReason::MinorStop)?;
        self.publish_drafts(owner, drafts)
    }

    /// 把一个平面交出的草稿按归属本地写或跨 owner 发布。
    fn publish_drafts(
        &mut self,
        owner: u32,
        drafts: Vec<CardMarkDraft>,
    ) -> Result<u32, RawInvariant> {
        let owner_token = self.token(owner);
        let mut published = 0_u32;
        for draft in drafts {
            let local = self
                .barrier
                .table(draft.arena_descriptor)
                .is_some_and(|table| table.manager() == owner_token);
            if local {
                self.barrier
                    .consume_locally(draft.arena_descriptor, &draft)?;
                continue;
            }
            self.publish_card_batch(owner, &draft)?;
            published += 1;
        }
        Ok(published)
    }

    /// 冲刷一个 processor 的 barrier 账本，并按归属选择本地写或批量发布。
    ///
    /// 返回实际发布的 batch 数；processor 是 arena owner 时直接写 card table，不计入
    /// 跨 owner 通道。
    pub(crate) fn flush_barrier(
        &mut self,
        owner: u32,
        processor: usize,
        reason: BarrierFlushReason,
    ) -> Result<u32, RawInvariant> {
        Ok(self.flush_barrier_counted(owner, processor, reason)?.0)
    }

    /// 与 `flush_barrier` 共用同一实现，额外返回本次真实交出的 card 键数。
    ///
    /// assist 用这个计数确认「完成的工作」：键数是本次真的离开 processor 账本的 dirty
    /// card 数（本地直接写或成功发布），因此不会把未发生的标记工作算成进度。
    pub(crate) fn flush_barrier_counted(
        &mut self,
        owner: u32,
        processor: usize,
        reason: BarrierFlushReason,
    ) -> Result<(u32, u64), RawInvariant> {
        let drafts = self.barrier.flush_processor(processor, reason)?;
        if drafts.is_empty() {
            return Ok((0, 0));
        }
        let owner_token = self.token(owner);
        let mut published = 0_u32;
        let mut card_keys = 0_u64;
        for draft in drafts {
            let local = self
                .barrier
                .table(draft.arena_descriptor)
                .is_some_and(|table| table.manager() == owner_token);
            if local {
                self.barrier
                    .consume_locally(draft.arena_descriptor, &draft)?;
            } else {
                self.publish_card_batch(owner, &draft)?;
                published += 1;
            }
            card_keys = card_keys.saturating_add(u64::from(draft.card_count));
        }
        self.barrier.record_publish(&[]);
        Ok((published, card_keys))
    }

    /// 把一条 card batch 发布到 arena allocation owner 的 card mailbox。
    ///
    /// 目标 owner 由 arena descriptor 的登记表解析；目标不可达时进入 `RuntimeInvariant`，
    /// 不能把 batch 丢掉当作恢复。
    fn publish_card_batch(
        &mut self,
        owner: u32,
        draft: &CardMarkDraft,
    ) -> Result<(), RawInvariant> {
        let table_owner = self
            .barrier
            .table(draft.arena_descriptor)
            .map(|table| table.manager())
            .ok_or_else(|| RawInvariant::new("card batch 引用未登记的 arena"))?;
        let target_index = self
            .owners
            .iter()
            .position(|candidate| candidate.token() == table_owner)
            .ok_or_else(|| RawInvariant::new("card batch 目标 arena owner 不在 owner 表中"))?;
        let target = self.token(u32::try_from(target_index).expect("owner 下标适配 u32"));
        let shard = ShardIndex::from_raw(
            u32::try_from(target_index).expect("owner 下标适配 u32")
                % super::super::OWNER_INBOX_SHARDS,
        )
        .ok_or_else(|| RawInvariant::new("card batch shard 编号越界"))?;
        let batch = CardMarkBatch {
            next: None,
            target,
            arena: SlabDescriptorId::from_raw(
                u32::try_from(draft.arena_descriptor).expect("arena descriptor 适配 u32"),
            ),
            arena_generation: draft.arena_generation,
            card_start: draft.card_start,
            card_count: draft.card_count,
            cycle_epoch: draft.cycle_epoch,
            bytes: draft.bytes,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: super::super::slab::SlabGeneration::from_raw(u64::from(
                    draft.arena_generation,
                )),
                class: super::super::size_class::RuntimeSizeClassId::from_raw(0),
                owner_id: target.owner_id,
                route_key: target.route_key,
                checksum: 0,
            },
        };
        let mut batch = batch;
        batch.integrity.checksum = IntegrityTag::compute_card_mark(&self.integrity_secret, &batch);
        let inbox = self.inbox_for(&target)?;
        let mut staging = ProducerStaging::new(self.limits);
        stage_card_mark(
            &self.pool,
            Some(&inbox),
            &mut staging,
            &batch,
            shard,
            Some(FlushTrigger::OwnerPressure),
        )?;
        self.barrier.record_publish(std::slice::from_ref(draft));
        let _ = owner;
        Ok(())
    }

    /// owner 上下文：消费一条 card batch 并写 card table。
    ///
    /// 重复 batch 幂等；arena generation 或目标 owner 不匹配进入 `RuntimeInvariant`。
    pub(crate) fn service_card_mark(
        &mut self,
        owner: u32,
        batch: &CardMarkBatch,
    ) -> Result<u32, RawInvariant> {
        if self.token(owner) != batch.target {
            return Err(RawInvariant::new("card batch 投递到错误 owner"));
        }
        if batch.integrity.checksum
            != IntegrityTag::compute_card_mark(&self.integrity_secret, batch)
        {
            return Err(RawInvariant::new("card batch integrity 校验失败"));
        }
        let arena_descriptor = u64::from(batch.arena.raw());
        let table = self
            .barrier
            .table(arena_descriptor)
            .ok_or_else(|| RawInvariant::new("card batch 引用未登记的 arena"))?;
        if table.manager() != batch.target {
            return Err(RawInvariant::new("card batch 不属于该 owner 的 arena"));
        }
        if table.arena_generation() != batch.arena_generation {
            return Err(RawInvariant::new(
                "card batch arena generation 与 card table 不匹配",
            ));
        }
        let draft = CardMarkDraft {
            arena_descriptor,
            arena_generation: batch.arena_generation,
            card_start: batch.card_start,
            card_count: batch.card_count,
            cycle_epoch: batch.cycle_epoch,
            bytes: batch.bytes,
        };
        self.barrier.consume_published(arena_descriptor, &draft)
    }

    /// minor stop 请求：冲刷全部 processor，并返回扫描门禁是否已经满足。
    ///
    /// minor cycle 在扫描 remembered set 前必须确认所有 active processor 的 buffer 已
    /// flush、所有旧 epoch batch 已消费；因此请求 stop 时先做同样的冲刷与门禁检查。
    pub(crate) fn request_minor_stop(&mut self, owner: u32) -> Result<bool, RawInvariant> {
        let published = self.flush_all_barriers(owner, BarrierFlushReason::MinorStop)?;
        self.barrier.request_minor_stop();
        Ok(self.barrier.minor_scan_ready() && published == 0)
    }

    /// 冲刷一个 owner 当前全部 processor 的账本。
    ///
    /// 七个触发点都经由它或 `flush_barrier` 收口：owner `retire`、`drain_all`、source-slab
    /// cache 的 `GcHandoff`/`PressureDrain` 关闭、`enter_foreign` 与 allocation slow edge 上的
    /// assist；`buffer-full` 由 `perform_barrier` 返回值报告，`minor-stop` 由
    /// `request_minor_stop` 收口。
    pub(super) fn flush_all_barriers(
        &mut self,
        owner: u32,
        reason: BarrierFlushReason,
    ) -> Result<u32, RawInvariant> {
        Ok(self.flush_all_barriers_counted(owner, reason)?.0)
    }

    /// 与 `flush_all_barriers` 共用同一实现，额外返回本次真实交出的 card 键数。
    pub(super) fn flush_all_barriers_counted(
        &mut self,
        owner: u32,
        reason: BarrierFlushReason,
    ) -> Result<(u32, u64), RawInvariant> {
        let processors = self.barrier.processor_count();
        let mut published = 0_u32;
        let mut card_keys = 0_u64;
        for processor in 0..processors {
            let (batch_published, keys) = self.flush_barrier_counted(owner, processor, reason)?;
            published += batch_published;
            card_keys = card_keys.saturating_add(keys);
        }
        Ok((published, card_keys))
    }

    /// 返回 barrier 平面的统计快照。
    ///
    /// barrier 平面是全局唯一的：全部 processor 账本、card table 与 edge summary 都在同一
    /// 平面上，因此统计快照不带 owner 参数。
    pub(crate) fn barrier_stats(&self) -> BarrierStats {
        let mut stats = BarrierStats::default();
        let processors = self.barrier.processor_count();
        for processor in 0..processors {
            if let Some(record) = self.barrier.processor(processor) {
                stats.card_marks += record.card_marks();
                stats.slot_reuses += record.card_slot_reuses();
            }
        }
        stats.published_batches = self.barrier.published_batches();
        stats.consumed_batches = self.barrier.consumed_batches();
        stats.flushes = self.barrier.flushes();
        stats.empty_flushes = self.barrier.empty_flushes();
        stats.by_reason = self.barrier.flushed_by_reason();
        stats.edge_pending =
            u64::try_from(self.barrier.edge_pending_items()).expect("pending 适配 u64");
        stats.edge_published = self.edge_delta_total;
        stats
    }

    /// 返回一个 block 的 incoming lease：非零 source block 对的数量。
    ///
    /// 它是候选判定的输入，不是对象级引用计数：同一对 block 的多个字段边只计一次。
    pub(crate) fn block_incoming_leases(&self, target: BlockRef) -> u64 {
        self.barrier.incoming_leases(target)
    }

    /// 返回 domain 归属的 owner token；card batch 只发给 raw owner。
    pub(crate) fn raw_domain_owner(&self) -> OwnerToken {
        let _ = MemoryDomainId::RUNTIME_RAW;
        self.domain_owner()
    }
}
