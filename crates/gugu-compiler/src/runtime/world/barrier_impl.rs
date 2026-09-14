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
    BarrierFlushReason, BarrierPlane, BarrierSite, CardMarkDraft, EdgeDeltaRecord,
    HybridBarrierOutcome,
};
use super::super::barrier_schema::MessageFamilyTag;
use super::super::gc_metadata_contract::GC_ARENA_BYTES;
use super::super::inbox::ShardIndex;
use super::super::message::{
    CardMarkBatch, FlushTrigger, IntegrityTag, MessageState, ProducerStaging, ReturnKind,
    stage_card_mark,
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
    /// 已发布 batch 数。
    pub(crate) published_batches: u64,
    /// 已消费 batch 数。
    pub(crate) consumed_batches: u64,
    /// 全部 flush 次数。
    pub(crate) flushes: u64,
    /// 未产生任何 batch 的 flush 次数。
    pub(crate) empty_flushes: u64,
    /// 六个原因各自的 flush 次数。
    pub(crate) by_reason: [u64; 6],
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
    /// arena 的物理页由 slab/extent 层提交，card table 只登记元数据；arena descriptor
    /// 用 slab 描述符编号表示，使 batch 与 return 消息共享同一身份空间。
    pub(crate) fn register_managed_arena(
        &mut self,
        owner: u32,
        arena_descriptor: SlabDescriptorId,
        arena_generation: u32,
    ) -> Result<(), RawInvariant> {
        let token = self.token(owner);
        self.barrier.register_arena(
            u64::from(arena_descriptor.raw()),
            token,
            arena_generation,
            GC_ARENA_BYTES,
        )?;
        Ok(())
    }

    /// mutator 上下文：在一个 processor 上执行一条 hybrid barrier。
    ///
    /// buffer 满时返回的 flush 原因必须由调用方在 region 外补容量，不能就地扩容。
    pub(crate) fn perform_barrier(
        &mut self,
        processor: usize,
        site: BarrierSite,
    ) -> HybridBarrierOutcome {
        self.barrier.perform_barrier(processor, site)
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
        let drafts = self.barrier.flush_processor(processor, reason);
        if drafts.is_empty() {
            return Ok(0);
        }
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
        self.barrier.record_publish(&[]);
        Ok(published)
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

    /// 返回一个 owner 的 card batch 目标 inbox；与 return 消息共用通道。
    pub(crate) fn card_mark_target(&self, owner: u32) -> OwnerToken {
        self.token(owner)
    }

    /// 六个触发点之一：processor 交接（绑定、retire 或换栈）。
    pub(crate) fn flush_barrier_handoff(
        &mut self,
        owner: u32,
        processor: usize,
    ) -> Result<u32, RawInvariant> {
        self.flush_barrier(owner, processor, BarrierFlushReason::ProcessorHandoff)
    }

    /// 六个触发点之一：进入普通或 dirty bridge。
    pub(crate) fn flush_barrier_foreign(&mut self, owner: u32) -> Result<u32, RawInvariant> {
        self.flush_all_barriers(owner, BarrierFlushReason::ForeignBridge)
    }

    /// 六个触发点之一：memory pressure 的有界 drain。
    pub(crate) fn flush_barrier_pressure(&mut self, owner: u32) -> Result<u32, RawInvariant> {
        self.flush_all_barriers(owner, BarrierFlushReason::MemoryPressure)
    }

    /// 六个触发点之一：minor stop 请求。
    ///
    /// minor cycle 在扫描 remembered set 前必须确认所有 active processor 的 buffer 已
    /// flush、所有旧 epoch batch 已消费；因此请求 stop 时先做同样的冲刷与门禁检查。
    pub(crate) fn request_minor_stop(&mut self, owner: u32) -> Result<bool, RawInvariant> {
        let published = self.flush_all_barriers(owner, BarrierFlushReason::MinorStop)?;
        self.barrier.request_minor_stop();
        Ok(self.barrier.minor_scan_ready() && published == 0)
    }

    /// 六个触发点之一：producer stop gate。
    pub(crate) fn flush_barrier_stop_gate(&mut self, owner: u32) -> Result<u32, RawInvariant> {
        self.flush_all_barriers(owner, BarrierFlushReason::ProducerStopGate)
    }

    /// 冲刷一个 owner 当前全部 processor 的账本；retire、foreign bridge、cache 关闭等
    /// 交接路径都经由它收口。
    pub(super) fn flush_all_barriers(
        &mut self,
        owner: u32,
        reason: BarrierFlushReason,
    ) -> Result<u32, RawInvariant> {
        let processors = self.barrier.processor_count();
        let mut published = 0;
        for processor in 0..processors {
            published += self.flush_barrier(owner, processor, reason)?;
        }
        Ok(published)
    }

    /// 返回 edge summary 的待发布 delta；只做 barrier 侧的本地聚合。
    pub(crate) fn drain_edge_summary(&mut self) -> Vec<EdgeDeltaRecord> {
        self.barrier.drain_edges()
    }
    /// 返回消息族判别值；card batch 只出现在 GC 工作族。
    pub(crate) const fn card_mark_family() -> MessageFamilyTag {
        MessageFamilyTag::CardMark
    }

    /// 返回 return 消息族判别值，供测试断言两族分离。
    pub(crate) const fn return_family() -> MessageFamilyTag {
        MessageFamilyTag::Return
    }

    /// 返回 `ResourceRelease` 的 return 种类，供测试断言 batch 不占用 return 种类。
    pub(crate) const fn release_kind() -> ReturnKind {
        ReturnKind::ResourceRelease
    }

    /// 返回一个 owner 的 barrier 统计快照。
    pub(crate) fn barrier_stats(&self, owner: u32) -> BarrierStats {
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
        let _ = owner;
        stats
    }

    /// 返回 domain 归属的 owner token；card batch 只发给 raw owner。
    pub(crate) fn raw_domain_owner(&self) -> OwnerToken {
        let _ = MemoryDomainId::RUNTIME_RAW;
        self.domain_owner()
    }
}
