//! `RawWorld` 上的 `EdgeDelta` 接入：发布、按序消费、credit 结算与 candidate dirty 通知。
//!
//! 归属规则：
//!
//! 1. **发布在 source owner 上下文**：`EdgeSummary` 的聚合结果由 cycle 边界取出，按
//!    destination 的当前 manager 路由，每条记录占用一个共享 credit。
//! 2. **消费在 target owner 上下文**：只有序号接得上的记录才应用，乱序记录连同 node 与
//!    credit 一起保留；应用完成才结算 credit 并让 node 进入 grace。
//! 3. **身份校验先于状态改变**：block generation、manager 身份、消息族与 cycle/topology
//!    全部通过后才触碰平面状态，因此错误消息不会污染已经有效的计数。

use super::super::barrier_schema::MessageFamilyTag;
use super::super::edge::{EdgeApply, EdgeOutcome, held_record};
use super::super::inbox::ShardIndex;
use super::super::local_heap::{BlockRef, HeapArenaKind, ManagedBlockId};
use super::super::mark::MarkError;
use super::super::message::{
    EdgeDelta, FlushTrigger, IntegrityTag, MessageState, ReturnNodeId, flush_staging,
    stage_edge_delta,
};
use super::super::slab::{OwnerToken, RawInvariant};
use super::RawWorld;
use super::heap_impl::heap_error;

impl RawWorld {
    /// 返回边差量平面；未配置时报不变量失败。
    pub(crate) fn edge_plane(&self) -> Result<&super::super::edge::EdgePlane, RawInvariant> {
        self.edges
            .as_ref()
            .ok_or_else(|| RawInvariant::new("edge 平面未按契约配置"))
    }

    /// 返回可变的边差量平面。
    pub(crate) fn edge_plane_mut(
        &mut self,
    ) -> Result<&mut super::super::edge::EdgePlane, RawInvariant> {
        self.edges
            .as_mut()
            .ok_or_else(|| RawInvariant::new("edge 平面未按契约配置"))
    }

    /// 发布全部尚未发布的跨 block 边差量。
    ///
    /// 每条记录都按 destination 的当前 manager 路由到对应 owner 的 inbox，并占用一个共享
    /// credit；`held` 记录（target 侧还没接上序号）在消费前一直是同一份 credit。
    pub(crate) fn publish_edge_deltas(
        &mut self,
    ) -> Result<Vec<super::super::barrier::EdgeDeltaRecord>, RawInvariant> {
        self.barrier.merge_edges()?;
        // 未配置 managed heap 平面时不存在任何 managed block，也就没有任何跨 block 边；
        // 这一模式（pacing-only cycle）下 summary 必须为空，否则是真的丢了记录。
        if self.edges.is_none() {
            let pending = self.barrier.edge_pending_items();
            if pending != 0 {
                return Err(RawInvariant::new(
                    "未配置 GC 平面的 cycle 里出现了跨 block 边变更",
                ));
            }
            return Ok(Vec::new());
        }
        let deltas = self.barrier.publish_edges(self.barrier.cycle_epoch())?;
        self.edge_delta_total += u64::try_from(deltas.len()).expect("delta 数适配 u64");
        for record in &deltas {
            self.publish_edge_delta(record)?;
        }
        self.edge_plane_mut()?
            .note_publish(u64::try_from(deltas.len()).expect("delta 数适配 u64"));
        Ok(deltas)
    }

    /// 把一条已聚合的差量发布给 destination 的当前 manager。
    ///
    /// source 与 destination 的 generation 都取自稳定 block registry；路由目标是 arena 的
    /// **manager** 而不是持有 payload 的 heap owner：管理权转移后 payload 不动，只有 manager 变。
    fn publish_edge_delta(
        &mut self,
        record: &super::super::barrier::EdgeDeltaRecord,
    ) -> Result<(), RawInvariant> {
        let source_owner = self.block_owner(record.source)?;
        let manager = self.block_manager(record.target)?;
        let destination_owner = self.manager_owner_index(manager)?;
        let credit = self
            .mark_plane_mut()?
            .acquire_edge_delta(source_owner, destination_owner)
            .map_err(mark_error)?;
        let target_token = self.token(destination_owner);
        let mut delta = EdgeDelta {
            next: None,
            target: target_token,
            source: record.source,
            destination: record.target,
            cycle_epoch: record.epoch,
            topology_epoch: self.mark_plane()?.topology(),
            sequence: record.sequence,
            delta: record.delta,
            credit,
            bytes: super::super::message::RETURN_NODE_BYTES,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: super::super::slab::SlabGeneration::from_raw(
                    target_token.generation.raw(),
                ),
                class: super::super::size_class::RuntimeSizeClassId::from_raw(0),
                owner_id: target_token.owner_id,
                route_key: target_token.route_key,
                checksum: 0,
            },
        };
        delta.integrity.checksum = IntegrityTag::compute_edge_delta(&self.integrity_secret, &delta);
        let shard = ShardIndex::from_raw(destination_owner % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("edge delta shard 编号越界"))?;
        // staging 一次只承载一个 target：目标改变时先交出旧 chain，与 return 路径同一规则。
        if let Some(previous) = self.return_stagings[source_owner as usize].target()
            && previous != target_token
        {
            let previous_inbox = self.inbox_for(&previous)?;
            flush_staging(
                &self.pool,
                &previous_inbox,
                &mut self.return_stagings[source_owner as usize],
                FlushTrigger::TargetChanged,
            )?;
        }
        let inbox = self.inbox(destination_owner);
        stage_edge_delta(
            &self.pool,
            Some(&inbox),
            &mut self.return_stagings[source_owner as usize],
            &delta,
            shard,
            Some(FlushTrigger::OwnerPressure),
        )?;
        Ok(())
    }

    /// owner 上下文：消费一条 `EdgeDelta`。
    ///
    /// 身份校验全部通过后才触碰平面状态；destination 已经换 manager 时把记录与 credit 一起
    /// 转投新 manager，因此管理权转移不需要重放 source 侧的聚合状态。
    pub(crate) fn service_edge_delta(
        &mut self,
        owner: u32,
        node: ReturnNodeId,
        delta: &EdgeDelta,
    ) -> Result<EdgeOutcome, RawInvariant> {
        if self.token(owner) != delta.target {
            return Err(RawInvariant::new("edge delta 投递到错误 owner"));
        }
        delta
            .integrity
            .verify_edge_delta(&self.integrity_secret, delta)
            .map_err(|error| RawInvariant::new(error.message().to_owned()))?;
        if delta.bytes != super::super::message::RETURN_NODE_BYTES {
            return Err(RawInvariant::new("edge delta 的 bytes 不是单个 node"));
        }
        // 身份校验先于状态改变：source 与 destination 都必须仍是登记的 block generation。
        let _source_owner = self.block_owner(delta.source)?;
        let manager = self.block_manager(delta.destination)?;
        self.mark_plane()?
            .credits()
            .validate_in_flight(
                delta.credit,
                MessageFamilyTag::EdgeDelta,
                delta.cycle_epoch,
                delta.topology_epoch,
            )
            .map_err(mark_error)?;
        if manager.owner_id != self.token(owner).owner_id {
            // 管理权已经转移：记录与 credit 一起转投新 manager，旧 node 可以释放。
            let new_owner = self.manager_owner_index(manager)?;
            self.mark_plane_mut()?
                .forward_edge_delta(delta.credit, new_owner)
                .map_err(mark_error)?;
            self.restage_edge_delta(delta, new_owner)?;
            return Ok(EdgeOutcome::Forwarded);
        }
        let destination = delta.destination.id;
        let record = held_record(delta, node);
        let outcome = self
            .edge_plane_mut()?
            .apply(delta.source, delta.destination, record)?;
        match outcome {
            EdgeApply::Held => {
                self.edge_plane_mut()?.note_dirty(destination);
                self.note_block_mutation(delta.source.id)?;
                Ok(EdgeOutcome::Held)
            }
            EdgeApply::Applied => {
                self.mark_plane_mut()?
                    .consume_edge_delta(
                        owner,
                        delta.credit,
                        delta.cycle_epoch,
                        delta.topology_epoch,
                    )
                    .map_err(mark_error)?;
                self.mark_plane_mut()?
                    .return_credit(delta.credit)
                    .map_err(mark_error)?;
                // 缺口补齐时被连带应用的记录同样在这里收口：先结算它们的 credit，
                // 再把它们的 node 交给既有 grace 路径。
                let released = self.edge_plane_mut()?.take_released();
                for held in &released {
                    self.mark_plane_mut()?
                        .consume_edge_delta(
                            owner,
                            held.credit,
                            delta.cycle_epoch,
                            delta.topology_epoch,
                        )
                        .map_err(mark_error)?;
                    self.mark_plane_mut()?
                        .return_credit(held.credit)
                        .map_err(mark_error)?;
                    self.graced_nodes.push(held.node);
                }
                self.edge_plane_mut()?.note_dirty(destination);
                self.note_block_mutation(delta.source.id)?;
                Ok(EdgeOutcome::Applied)
            }
        }
    }

    /// 把一条记录原样转投给新的 manager；credit 已由调用方重定目标。
    fn restage_edge_delta(
        &mut self,
        delta: &EdgeDelta,
        new_owner: u32,
    ) -> Result<(), RawInvariant> {
        let target_token = self.token(new_owner);
        let mut forwarded = *delta;
        forwarded.target = target_token;
        forwarded.state = MessageState::Forwarded;
        forwarded.integrity.owner_id = target_token.owner_id;
        forwarded.integrity.route_key = target_token.route_key;
        forwarded.integrity.generation =
            super::super::slab::SlabGeneration::from_raw(target_token.generation.raw());
        forwarded.integrity.checksum =
            IntegrityTag::compute_edge_delta(&self.integrity_secret, &forwarded);
        let source_owner = self.block_owner(forwarded.source)?;
        let shard = ShardIndex::from_raw(new_owner % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("edge delta shard 编号越界"))?;
        if let Some(previous) = self.return_stagings[source_owner as usize].target()
            && previous != target_token
        {
            let previous_inbox = self.inbox_for(&previous)?;
            flush_staging(
                &self.pool,
                &previous_inbox,
                &mut self.return_stagings[source_owner as usize],
                FlushTrigger::TargetChanged,
            )?;
        }
        let inbox = self.inbox(new_owner);
        stage_edge_delta(
            &self.pool,
            Some(&inbox),
            &mut self.return_stagings[source_owner as usize],
            &forwarded,
            shard,
            Some(FlushTrigger::OwnerPressure),
        )?;
        Ok(())
    }

    /// 管理权转移：把 `owner` 持有的全部 managed arena 的 manager 改成新 token。
    ///
    /// payload 仍留在原 heap，只有“谁能改这些 block”随之转移；因此已经在飞的 ticket 与
    /// edge delta 会在消费时按新 manager 重新路由。
    pub(crate) fn handover_managed_arenas(
        &mut self,
        owner: u32,
        target: OwnerToken,
    ) -> Result<u64, RawInvariant> {
        let token = self.token(owner);
        let mut moved = 0_u64;
        let mut heap_slots = Vec::new();
        for arena in &mut self.managed_arenas {
            if arena.manager.owner_id == token.owner_id {
                arena.manager = target;
                moved += 1;
                heap_slots.push((arena.heap_owner, arena.heap_slot));
            }
        }
        for (heap_owner, heap_slot) in heap_slots {
            self.heap_mut(heap_owner)?
                .set_arena_manager(heap_slot, target.owner_id.raw());
        }
        Ok(moved)
    }

    /// 返回一个 block 身份当前的 manager token。
    fn block_manager(
        &self,
        block: BlockRef,
    ) -> Result<super::super::slab::OwnerToken, RawInvariant> {
        let arena = self.managed_arena_by_descriptor(block.id.arena())?;
        let generation = self.managed_block_generation(block.id)?;
        if generation != block.generation {
            return Err(RawInvariant::new("block 身份引用已复用的 generation"));
        }
        Ok(arena.manager)
    }

    /// 返回一个 block 身份当前持有 payload 的 owner。
    fn block_owner(&self, block: BlockRef) -> Result<u32, RawInvariant> {
        let arena = self.managed_arena_by_descriptor(block.id.arena())?;
        let generation = self.managed_block_generation(block.id)?;
        if generation != block.generation {
            return Err(RawInvariant::new("block 身份引用已复用的 generation"));
        }
        Ok(arena.heap_owner)
    }

    /// 把 block manager 的稳定身份映射到 owner 槽位编号。
    ///
    /// 复用 raw owner 与 resource owner 的统一映射：manager 可能是任一者，两者共享同一套
    /// owner 身份空间，因此这里不另立一套编号规则。
    pub(super) fn manager_owner_index(&self, manager: OwnerToken) -> Result<u32, RawInvariant> {
        let slot = self
            .owners
            .iter()
            .position(|owner| owner.token() == manager)
            .or_else(|| {
                self.resource_owners
                    .iter()
                    .position(|owner| owner.token() == manager)
                    .map(|index| self.owners.len() + index)
            })
            .ok_or_else(|| RawInvariant::new("block manager 不是本世界的 owner"))?;
        Ok(u32::try_from(slot).expect("owner 槽位适配 u32"))
    }

    /// 把一次 block 变更通知给候选平面：登记 dirty、推进 mutation version 并让绑定组失效。
    ///
    /// nursery block 由 minor cycle 整体搬运与复位，不是候选回收的对象：把它标成候选会导致一个
    /// 仍可能被 evacuate 的 block 被释放，因此这里按 arena 类别排除它。
    pub(crate) fn note_block_mutation(
        &mut self,
        block: ManagedBlockId,
    ) -> Result<(), RawInvariant> {
        self.edge_plane_mut()?.note_dirty(block);
        let heap_owner = self.managed_arena_by_descriptor(block.arena())?.heap_owner;
        let kind = self
            .heap_mut(heap_owner)?
            .block_arena_kind(block)
            .map_err(|error| RawInvariant::new(format!("块类别查询失败: {error:?}")))?;
        self.heap_mut(heap_owner)?.note_block_mutation(block)?;
        if kind != HeapArenaKind::Nursery {
            self.candidates
                .as_mut()
                .ok_or_else(|| RawInvariant::new("候选平面尚未配置"))?
                .note_dirty(block);
        }
        Ok(())
    }

    /// 返回全部正计数的 block 对快照；诊断与候选进度报告使用。
    pub(crate) fn edge_pairs(&self) -> Result<Vec<(BlockRef, BlockRef, i64)>, RawInvariant> {
        Ok(self
            .edges
            .as_ref()
            .ok_or_else(|| RawInvariant::new("边平面尚未配置"))?
            .applied_pairs())
    }

    /// cycle 边界的边平面维护：清退已应用计数为零且没有保留记录的 block 对。
    ///
    /// 只在 flush 之后调用：此时在飞记录已经全部落地，零计数键只剩「重建后重新起序号」的旧键，
    /// 留着它们会让每个 cycle 的 pair 表单调增长。返回清退的对数。
    pub(crate) fn edge_maintenance(&mut self) -> Result<u64, RawInvariant> {
        Ok(self.edge_plane_mut()?.retire_zero_pairs())
    }

    /// 返回边平面的累计统计；诊断与验收报告按它核对发布/应用/保留计数。
    pub(crate) fn edge_stats(&self) -> Result<super::super::edge::EdgeStats, RawInvariant> {
        Ok(self.edge_plane()?.stats())
    }

    /// 返回边消息通路是否已经收敛。
    ///
    /// 收敛的定义是「没有在飞的记录、没有保留的 credit、没有待处理的 dirty block」：三者任一
    /// 非零都表示还有工作没有落进 target 侧的真实计数，此时宣布 cycle 完成会把未应用的工作
    /// 记到下一个 credit epoch 上。
    pub(crate) fn edge_converged(&self) -> Result<bool, RawInvariant> {
        let plane = self.edge_plane()?;
        Ok(plane.held_records() == 0 && plane.pending_credits() == 0 && plane.dirty_count() == 0)
    }

    /// 把消息与候选 cursor 的源码布局渲染成 dump 文本。
    ///
    /// 覆盖三块：工作单位与候选相位目录、`EdgeDelta` 的消息字段布局、以及每个活跃 job 的真实
    /// 游标取值。三者都来自登记值，因此 dump 不会与实现漂移。
    pub(crate) fn dump_gc_layout(&self) -> Result<String, RawInvariant> {
        use crate::runtime::candidate_schema::{
            CANDIDATE_WORK_UNIT, CandidatePhase, render_cursor_row,
        };
        let mut out = format!("work_unit={CANDIDATE_WORK_UNIT}\nphases=");
        for phase in CandidatePhase::ALL {
            out.push_str(phase.name());
            out.push(' ');
        }
        out.push('\n');
        for (name, kind) in crate::runtime::edge_schema::edge_delta_layout() {
            out.push_str(&format!("edge_delta.{name}={kind}\n"));
        }
        for progress in self.candidate_progress()? {
            out.push_str(&format!("cursor {}\n", render_cursor_row(&progress)));
        }
        Ok(out)
    }

    /// 返回一个 block 对当前的已应用计数；候选出边减量的核对入口。
    pub(crate) fn edge_applied_delta(
        &self,
        source: BlockRef,
        destination: BlockRef,
    ) -> Result<i64, RawInvariant> {
        Ok(self
            .edges
            .as_ref()
            .ok_or_else(|| RawInvariant::new("边平面尚未配置"))?
            .applied_delta(source, destination))
    }

    /// 按一次 cycle 的对象搬迁重建 block 对计数。
    ///
    /// 搬迁对是逐对象的 `(旧 block, 新 block)`；一个旧 block 的对象可能落到多个新 block，而
    /// `EdgePlane` 的计数是按 block 对聚合的，无法精确拆分。因此按「接收对象最多」的目标整体迁移
    /// ——这保持全局计数总和不变（不会像复制到每个目标那样逐轮上涨），而误差只影响精度：判错的
    /// 方向由 `validate` 的标记 gate 兜底。
    pub(crate) fn rebuild_edges_after_relocation(
        &mut self,
        relocations: Vec<(ManagedBlockId, ManagedBlockId)>,
    ) -> Result<u64, RawInvariant> {
        if relocations.is_empty() {
            return Ok(0);
        }
        let mut targets: std::collections::BTreeMap<
            ManagedBlockId,
            std::collections::BTreeMap<ManagedBlockId, u64>,
        > = std::collections::BTreeMap::new();
        for (old, new) in relocations {
            *targets.entry(old).or_default().entry(new).or_default() += 1;
            // 两个 block 都要重新进入候选视野：旧块失去入边，新块获得入边。
            self.note_candidate_dirty(old)?;
            self.note_candidate_dirty(new)?;
        }
        let mut moved = 0_u64;
        for (old, received) in targets {
            // 并列时取编号最小的目标，保证同一批搬迁的结论与遍历顺序无关。
            let dominant = received
                .iter()
                .max_by_key(|(block, count)| (**count, std::cmp::Reverse(**block)))
                .map(|(block, _)| *block)
                .ok_or_else(|| RawInvariant::new("搬迁记录没有目标 block"))?;
            let target = self.block_ref_of(dominant)?;
            // 三处都必须跟着对象搬：屏障的聚合项（incoming lease 的真实来源）、边平面的已应用
            // 计数、以及块记录上的 lease 计数。
            self.barrier.relocate_target(old, target);
            let leases = self
                .heap_for(old)?
                .block_record(old)
                .map_err(heap_error)?
                .incoming_leases;
            if leases != 0 {
                let taken = self
                    .heap_mut_for(old)?
                    .take_incoming_leases(old, leases)
                    .map_err(heap_error)?;
                self.heap_mut_for(dominant)?
                    .add_incoming_leases(dominant, taken)
                    .map_err(heap_error)?;
            }
            moved = moved.saturating_add(
                self.edges
                    .as_mut()
                    .ok_or_else(|| RawInvariant::new("边平面尚未配置"))?
                    .relocate_destination(old, Some(target))?,
            );
        }
        Ok(moved)
    }
}

/// 把 mark 平面失败转换成本平面的不变量错误。
fn mark_error(error: MarkError) -> RawInvariant {
    RawInvariant::new(error.to_string())
}
