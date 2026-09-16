//! `RawWorld` 上的 mark cycle 接入：root snapshot 门禁、跨 owner ticket 与终止检测。
//!
//! 接入遵循四条归属规则：
//!
//! 1. 每个 owner 的 mark worklist 只由 owner 上下文推进；跨 owner 引用不写对方 arena，而是
//!    发布一条只携带稳定身份的 `MarkTicket`，由目标 owner 在自己的 arena 内反查并标记。
//! 2. 进入 mark 阶段前必须收齐 root snapshot gate 的六类参与者确认，且每项确认都对应真实
//!    动作（冲刷 staging、排空 inbox、检查根分片与 region registry、登记 local worklist）。
//! 3. credit 由同一 `MarkPlane` 账本持有；每个发布的 ticket 占用一个 credit，被消费后由
//!    `settle_owner` 收口，因此「mailbox 为空」永远不是完成条件。
//! 4. 只有七个收敛条件同时为 0 才允许宣布 cycle 完成并推进 `mark_cycle_epoch`。

use super::super::inbox::{ServiceBudget, ShardIndex};
use super::super::local_heap::{CycleReport, HeapError};
use super::super::mark::{
    MarkCycleState, MarkError, MarkObservations, MarkParticipant, MarkPlane, MarkTermination,
};
use super::super::mark_schema::MarkRuntimeContract;
use super::super::message::{
    FlushTrigger, IntegrityTag, MarkTicket, MessageState, flush_staging, stage_mark_ticket,
};
use super::super::slab::{RawInvariant, SlabDescriptorId};
use super::RawWorld;

/// 一次 mark pass 的结果；进入 world 级 major cycle 报告与测试。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MarkPassReport {
    /// 本次 pass 的 cycle epoch。
    pub(crate) cycle: u64,
    /// 本次 pass 的 topology epoch。
    pub(crate) topology: u32,
    /// 本次 pass 真实标记的对象数。
    pub(crate) marked: u64,
    /// 累计发布的 ticket 数。
    pub(crate) tickets_published: u64,
    /// 累计消费的 ticket 数。
    pub(crate) tickets_consumed: u64,
    /// 累计归还的 credit 数。
    pub(crate) credits_returned: u64,
    /// 累计确认的 snapshot 参与者数。
    pub(crate) snapshot_participants: u64,
    /// 本 cycle 的终止记录。
    pub(crate) termination: MarkTermination,
}

impl RawWorld {
    /// 按契约配置 mark 平面与每个 owner 的 worklist。
    pub(crate) fn configure_mark(
        &mut self,
        contract: &MarkRuntimeContract,
    ) -> Result<(), RawInvariant> {
        let owners = u32::try_from(self.owners.len()).expect("owner 数适配 u32");
        let plane = MarkPlane::new(contract, owners)
            .map_err(|error| RawInvariant::new(error.to_string()))?;
        self.mark_worklists = (0..owners).map(|_| Vec::new()).collect();
        self.mark_cycle_epoch = 0;
        self.mark = Some(plane);
        Ok(())
    }

    /// 返回 mark 平面是否已配置。
    pub(crate) fn mark_configured(&self) -> bool {
        self.mark.is_some()
    }

    fn mark_plane(&self) -> Result<&MarkPlane, RawInvariant> {
        self.mark
            .as_ref()
            .ok_or_else(|| RawInvariant::new("mark 平面未按契约配置"))
    }

    fn mark_plane_mut(&mut self) -> Result<&mut MarkPlane, RawInvariant> {
        self.mark
            .as_mut()
            .ok_or_else(|| RawInvariant::new("mark 平面未按契约配置"))
    }

    /// 返回全部 owner 本地 mark worklist 的深度之和。
    pub(crate) fn mark_worklist_items(&self) -> u64 {
        self.mark_worklists
            .iter()
            .map(|worklist| worklist.len() as u64)
            .fold(0_u64, u64::saturating_add)
    }

    /// 开始一个新 mark cycle：固定身份、授权 credit、打开并确认 snapshot、seed 根。
    fn begin_mark_cycle(&mut self, scope: &[u32]) -> Result<u64, RawInvariant> {
        let cycle = self.mark_cycle_epoch + 1;
        let topology = self
            .directory()
            .record(self.token(0).owner_id)
            .map_or(0, |record| record.topology_epoch.raw());
        self.mark_plane_mut()?
            .begin_cycle(cycle, topology)
            .map_err(mark_error)?;
        // producer stop epoch 必须先发布：grace 之后未登记的 participant 不得再开始新 batch。
        let inbox = self.inbox(0);
        self.open_grace(&inbox);
        for owner in scope {
            self.confirm_owner_snapshot(*owner)?;
        }
        let ready = self.mark_plane()?.snapshot_ready();
        if !ready {
            return Err(RawInvariant::new("root snapshot gate 未收齐全部参与者确认"));
        }
        self.mark_plane_mut()?
            .release_snapshot()
            .map_err(mark_error)?;
        self.seed_mark_roots(scope)?;
        Ok(cycle)
    }

    /// 完成一个 owner 的六类 snapshot 确认；每项确认前都执行对应真实动作。
    fn confirm_owner_snapshot(&mut self, owner: u32) -> Result<(), RawInvariant> {
        // 1. producer-stop-epoch：交出本 owner 尚未发布的 return chain。
        self.flush_return_staging(owner, FlushTrigger::ProducerStopping)?;
        self.confirm_participant(owner, MarkParticipant::ProducerStopEpoch)?;
        // 2. remote-consumer：在 snapshot 边界彻底排空 inbox（并冲刷 barrier 账本）。
        let budget = ServiceBudget::pressure(u32::MAX, u64::MAX);
        self.drain_all(owner, &budget)?;
        self.confirm_participant(owner, MarkParticipant::RemoteConsumer)?;
        // 3. root-slice：按 owner 分片登记根；悬空根必须在进入 mark 前暴露。
        for slot in 0..self.managed_roots.len() {
            let value = self.managed_roots[slot];
            if value != 0 {
                self.owner_of(value)?;
            }
        }
        self.confirm_participant(owner, MarkParticipant::RootSlice)?;
        // 4. region-registry：region 平面存在时不得仍有在途移交。
        if let Some(regions) = &self.regions
            && regions.pending() != 0
        {
            return Err(RawInvariant::new(
                "root snapshot 时 region registry 仍有在途移交",
            ));
        }
        self.confirm_participant(owner, MarkParticipant::RegionRegistry)?;
        // 5. handle-access-guard：没有任何 participant 仍处于 publish 区间。
        let inbox = self.inbox(owner);
        if inbox.gate().active() != 0 {
            return Err(RawInvariant::new(
                "root snapshot 时仍有 participant 处于 publish 区间",
            ));
        }
        self.confirm_participant(owner, MarkParticipant::HandleAccessGuard)?;
        // 6. local-worklist：登记边界，worklist 从这里开始为空。
        self.mark_worklists[owner as usize].clear();
        self.confirm_participant(owner, MarkParticipant::LocalWorklist)?;
        Ok(())
    }

    fn confirm_participant(
        &mut self,
        owner: u32,
        kind: MarkParticipant,
    ) -> Result<(), RawInvariant> {
        self.mark_plane_mut()?
            .confirm_snapshot(owner, kind)
            .map_err(mark_error)
    }

    /// 用根槽与 remembered set seed 参与本次 cycle 的 owner worklist。
    fn seed_mark_roots(&mut self, scope: &[u32]) -> Result<u64, RawInvariant> {
        let roots: Vec<u64> = self
            .managed_roots
            .iter()
            .copied()
            .filter(|value| *value != 0)
            .collect();
        let mut seeded = 0_u64;
        for owner in scope {
            let mut queue = std::mem::take(&mut self.mark_worklists[*owner as usize]);
            for value in &roots {
                let target = self.owner_of(*value)?;
                if target == *owner {
                    queue.push(*value);
                    seeded += 1;
                } else if scope.contains(&target) {
                    // 根属于另一参与 owner：在该 owner 的 seed 阶段入队。
                    continue;
                } else {
                    return Err(RawInvariant::new("root 属于未参与本次 mark cycle 的 owner"));
                }
            }
            // remembered set：dirty card 上的对象同样是根，必须在本 pass 内被标记。
            let dirty = self.drain_dirty_cards(*owner)?;
            for (arena_index, card) in dirty {
                queue.extend(self.heap(*owner)?.card_objects(arena_index, card));
            }
            self.mark_worklists[*owner as usize] = queue;
        }
        Ok(seeded)
    }

    /// 排空一个 owner 的 mark worklist；返回本次真实标记的对象数。
    fn drain_mark_worklist(&mut self, owner: u32) -> Result<u64, RawInvariant> {
        let types = self.types()?.clone();
        let mut queue = std::mem::take(&mut self.mark_worklists[owner as usize]);
        let mut marked = 0_u64;
        while let Some(address) = queue.pop() {
            if self
                .heap_mut(owner)?
                .mark_object(address)
                .map_err(heap_error)?
                .is_none()
            {
                continue;
            }
            marked += 1;
            let pointers = self
                .heap_mut(owner)?
                .trace_pointers(address, &types)
                .map_err(heap_error)?;
            for (_, value) in pointers {
                if value == 0 {
                    continue;
                }
                let target_owner = self.owner_of(value)?;
                if target_owner == owner {
                    queue.push(value);
                    continue;
                }
                let target = self
                    .heap(target_owner)?
                    .object_at(value)
                    .map_err(heap_error)?;
                let (descriptor, offset, block) = self
                    .heap(target_owner)?
                    .ticket_identity(value)
                    .map_err(heap_error)?;
                let bytes = u32::try_from(target.payload_bytes)
                    .map_err(|_| RawInvariant::new("mark ticket 目标对象超过 u32 字节"))?;
                let descriptor = SlabDescriptorId::from_raw(
                    u32::try_from(descriptor)
                        .map_err(|_| RawInvariant::new("arena descriptor 超出 u32"))?,
                );
                self.publish_mark_ticket(owner, target_owner, block, descriptor, offset, bytes)?;
            }
        }
        self.mark_worklists[owner as usize] = queue;
        self.mark_plane_mut()?.note_marks(marked);
        Ok(marked)
    }

    /// 发布一条跨 owner mark ticket：源 owner acquire credit，目标 owner 的 inbox 入队。
    fn publish_mark_ticket(
        &mut self,
        source: u32,
        target: u32,
        source_block: u32,
        descriptor: SlabDescriptorId,
        offset: u32,
        bytes: u32,
    ) -> Result<(), RawInvariant> {
        let credit = self
            .mark_plane_mut()?
            .publish_ticket(source, target)
            .map_err(mark_error)?;
        self.stage_ticket(
            source,
            target,
            credit,
            source_block,
            descriptor,
            offset,
            bytes,
        )
    }

    /// 把一条已持有 credit 的 ticket 重新投递给目标 owner。
    ///
    /// 转发必须复用同一个 credit：`ticket.credit` 只在最终目标的 consume 处收口，重发时再
    /// acquire 一个新 credit 会让旧 credit 永远停在 InFlight，termination 不再收敛。
    fn stage_ticket(
        &mut self,
        source: u32,
        target: u32,
        credit: u32,
        source_block: u32,
        descriptor: SlabDescriptorId,
        offset: u32,
        bytes: u32,
    ) -> Result<(), RawInvariant> {
        let plane = self.mark_plane()?;
        let cycle_epoch = plane.cycle();
        let topology_epoch = plane.topology();
        let target_token = self.token(target);
        let mut ticket = MarkTicket {
            next: None,
            target: target_token,
            target_arena: descriptor,
            target_offset: offset,
            source_block,
            cycle_epoch,
            topology_epoch,
            credit,
            bytes,
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
        ticket.integrity.checksum =
            IntegrityTag::compute_mark_ticket(&self.integrity_secret, &ticket);
        let shard = ShardIndex::from_raw(target % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("mark ticket shard 编号越界"))?;
        // staging 一次只承载一个 target：目标改变时先交出旧 chain，与 return 路径同一规则。
        if let Some(previous) = self.return_stagings[source as usize].target()
            && previous != target_token
        {
            let previous_inbox = self.inbox_for(&previous)?;
            flush_staging(
                &self.pool,
                &previous_inbox,
                &mut self.return_stagings[source as usize],
                FlushTrigger::TargetChanged,
            )?;
        }
        let inbox = self.inbox(target);
        let _ = stage_mark_ticket(
            &self.pool,
            Some(&inbox),
            &mut self.return_stagings[source as usize],
            &ticket,
            shard,
            Some(FlushTrigger::OwnerPressure),
        )?;
        Ok(())
    }

    /// owner 上下文：消费一条 mark ticket，把目标对象入队并收口 credit。
    pub(crate) fn service_mark_ticket(
        &mut self,
        owner: u32,
        ticket: &MarkTicket,
    ) -> Result<(), RawInvariant> {
        if self.token(owner) != ticket.target {
            return Err(RawInvariant::new("mark ticket 投递到错误 owner"));
        }
        ticket
            .integrity
            .verify_mark_ticket(&self.integrity_secret, ticket)
            .map_err(|error| RawInvariant::new(error.message().to_owned()))?;
        match self.directory().resolve(&ticket.target) {
            super::super::slab::Resolution::Match => {}
            super::super::slab::Resolution::Forward(target) => {
                let target_index = u32::try_from(self.owner_slot(&target)?)
                    .map_err(|_| RawInvariant::new("owner 槽位超出 u32"))?;
                self.mark_plane_mut()?
                    .forward_ticket(owner, target_index, ticket.credit)
                    .map_err(mark_error)?;
                return self.stage_ticket(
                    owner,
                    target_index,
                    ticket.credit,
                    ticket.source_block,
                    ticket.target_arena,
                    ticket.target_offset,
                    ticket.bytes,
                );
            }
            super::super::slab::Resolution::Retired | super::super::slab::Resolution::Unknown => {
                return Err(RawInvariant::new(
                    "mark ticket 的目标 owner 已 retire 或未知",
                ));
            }
        }
        self.mark_plane_mut()?
            .consume_ticket(
                owner,
                ticket.credit,
                ticket.cycle_epoch,
                ticket.topology_epoch,
            )
            .map_err(mark_error)?;
        let address = self
            .heap(owner)?
            .object_at_ticket(
                u64::from(ticket.target_arena.raw()),
                u64::from(ticket.target_offset),
            )
            .map_err(|_| RawInvariant::new("mark ticket 的目标对象已过期"))?
            .object_start;
        self.mark_worklists[owner as usize].push(address);
        Ok(())
    }

    /// 按真实观测与 credit 账本组装终止记录。
    fn mark_termination(&self) -> Result<MarkTermination, RawInvariant> {
        let owners = u64::try_from(self.owners.len()).expect("owner 数适配 u64");
        let observations = MarkObservations {
            worklist_items: self.mark_worklist_items(),
            published_batches: u64::try_from(
                self.return_stagings
                    .iter()
                    .filter(|staging| staging.bytes() > 0)
                    .count(),
            )
            .expect("staging 数适配 u64"),
            barrier_buffer_keys: self.credit_snapshot().barrier_buffer_keys,
            // 在途 region 移交与转发中的 ticket 都是「已经离开生产者、还没被目标消费」的 GC 工作。
            forwarding_work: self
                .regions
                .as_ref()
                .map_or(0, |regions| regions.pending() as u64),
            producer_epoch_confirmed: owners,
            producer_epoch_total: owners,
        };
        Ok(self.mark_plane()?.termination(observations))
    }

    /// 执行一次到收敛为止的 mark pass。
    ///
    /// `Idle`/`Complete` 时开新 cycle；`Snapshot` 时补齐确认；`Marking`/`Converging` 恢复同一
    /// cycle；`Remark` 直接返回上一轮终止记录。每轮对 scope 内 owner 依次推进 worklist 与
    /// inbox，最后收口 credit；一轮毫无进展即退出，轮数上限防止真正的不收敛。
    pub(crate) fn run_mark_pass(&mut self, scope: &[u32]) -> Result<MarkPassReport, RawInvariant> {
        let state = self.mark_plane()?.state();
        if state == MarkCycleState::Remark {
            let termination = self
                .mark_plane()?
                .last()
                .ok_or_else(|| RawInvariant::new("remark 阶段缺少终止记录"))?;
            return Ok(self.mark_report(termination, 0));
        }
        let mut cycle = self.mark_plane()?.cycle();
        match state {
            MarkCycleState::Idle | MarkCycleState::Complete => {
                cycle = self.begin_mark_cycle(scope)?;
            }
            MarkCycleState::Snapshot => {
                for owner in scope {
                    self.confirm_owner_snapshot(*owner)?;
                }
                if !self.mark_plane()?.snapshot_ready() {
                    return Err(RawInvariant::new("root snapshot gate 未收齐全部参与者确认"));
                }
                self.mark_plane_mut()?
                    .release_snapshot()
                    .map_err(mark_error)?;
                self.seed_mark_roots(scope)?;
            }
            MarkCycleState::Marking | MarkCycleState::Converging | MarkCycleState::Remark => {}
        }
        let budget = ServiceBudget::pressure(u32::MAX, u64::MAX);
        let limit = scope.len() * 4 + 16;
        let mut marked = 0_u64;
        for round in 0..=limit {
            let mut progress = false;
            for owner in scope {
                let marks = self.drain_mark_worklist(*owner)?;
                marked += marks;
                progress |= marks > 0;
                let (_, consumed) = self.drain_inboxes(*owner, &budget, true)?;
                progress |= consumed > 0;
            }
            for owner in scope {
                let returned = self
                    .mark_plane_mut()?
                    .settle_owner(*owner)
                    .map_err(mark_error)?;
                progress |= returned > 0;
            }
            if !progress {
                break;
            }
            if round == limit {
                return Err(RawInvariant::new("mark cycle 无法收敛"));
            }
        }
        let termination = self.mark_termination()?;
        self.mark_plane_mut()?
            .remember(termination)
            .map_err(mark_error)?;
        if termination.converged() {
            self.mark_cycle_epoch = cycle;
        }
        Ok(self.mark_report(termination, marked))
    }

    fn mark_report(&self, termination: MarkTermination, marked: u64) -> MarkPassReport {
        let stats = self.mark.as_ref().map(MarkPlane::stats).unwrap_or_default();
        MarkPassReport {
            cycle: termination.cycle(),
            topology: termination.topology(),
            marked,
            tickets_published: stats.tickets_published,
            tickets_consumed: stats.tickets_consumed,
            credits_returned: stats.credits_returned,
            snapshot_participants: stats.snapshot_participants,
            termination,
        }
    }

    /// 宣布 mark cycle 完成并清空 worklist。
    pub(crate) fn finish_mark_cycle(&mut self) -> Result<(), RawInvariant> {
        self.mark_plane_mut()?.complete().map_err(mark_error)?;
        for worklist in &mut self.mark_worklists {
            worklist.clear();
        }
        Ok(())
    }

    /// 回收一个 owner 未标记的对象。
    pub(crate) fn sweep_owner(&mut self, owner: u32) -> Result<CycleReport, RawInvariant> {
        let mut report = CycleReport::default();
        self.heap_mut(owner)?
            .sweep_unmarked(&mut report)
            .map_err(heap_error)?;
        Ok(report)
    }

    /// 触发一次 world 级 major cycle：mark pass 收敛后才允许 sweep 与推进 epoch。
    pub(crate) fn collect_major(&mut self, owner: u32) -> Result<CycleReport, RawInvariant> {
        let pass = self.run_mark_pass(&[owner])?;
        if !pass.termination.converged() {
            return Err(RawInvariant::new("major cycle 的 mark 未收敛"));
        }
        self.finish_mark_cycle()?;
        let mut report = self.sweep_owner(owner)?;
        report.marked =
            u32::try_from(pass.marked).map_err(|_| RawInvariant::new("标记数超出 u32"))?;
        self.heap_mut(owner)?.note_major_cycle();
        self.heap_cycle_epoch += 1;
        self.advance_barrier_epoch(owner, self.heap_cycle_epoch)?;
        Ok(report)
    }
}

/// 把 LocalHeap 失败转换成本平面的不变量错误。
fn heap_error(error: HeapError) -> RawInvariant {
    error.into_invariant()
}

/// 把 mark 平面失败转换成本平面的不变量错误。
fn mark_error(error: MarkError) -> RawInvariant {
    RawInvariant::new(error.to_string())
}
