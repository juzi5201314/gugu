//! `RawWorld` 上的 GC debt、credit、pacing 与 pressure drain 接入。
//!
//! 接入遵循三条归属规则：
//!
//! 1. debt 只在真实 slow edge 上推进：分配、slow service、cache 关闭与 cycle 边界各记一次。
//!    慢路径门禁全部是标量比较，只有到节奏点才读全局快照，因此本地 fast bump 不读全局状态。
//! 2. forced full cycle 与 pressure drain 必须落到真实动作：冲刷未发布的 return staging、关闭
//!    owner 真实的 source-slab cache、排空 owner inbox、trim/decommit extent、取走 edge delta、
//!    重算 committed 快照。没有 headroom 时走 rt0 的 `OutOfMemory` fatal，而不是把分配失败
//!    伪装成成功。
//! 3. credit 是观测：每次观测都从当前物理状态取数（未 flush 键数、未消费 batch 数、未取走
//!    delta 数、pending return 字节、staging 字节），因此「收敛」是可验证的物理事实；未收敛
//!    只表示本轮 cycle 未完成，不是错误。

use super::super::barrier::BarrierFlushReason;
use super::super::barrier_schema::CARD_GRANULARITY_BYTES;
use super::super::inbox::{DrainStop, ServiceBudget, ShardIndex};
use super::super::message::{FlushTrigger, RingCloseReason};
use super::super::pacing::{
    AssistOutcome, CommittedClasses, CreditSnapshot, EvacuationFootprint, EvacuationOutcome,
    GcWorkCounters, HeadroomDecision, PacingPlane, PressureState, RemarkOutcome,
};
use super::super::slab::RawInvariant;
use super::super::startup_kinds::FatalKind;
use super::RawWorld;
use super::extent_impl::TrimCandidate;

/// 一次 drain 的作用域。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrainScope {
    /// episode 内的有界 owner drain：每 shard 一次有界 service，不完成 cycle。
    Bounded,
    /// 完整 GC cycle：穷尽排空 inbox、过 remark 门禁、推进 cycle epoch 并记录工作量。
    Cycle,
}

/// 一次 pressure drain 的结果；进入统计与 dump。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PressureDrainReport {
    /// 已完成的 forced full cycle 数。
    pub(crate) forced_cycles: u64,
    /// 消费的 return/card 消息数。
    pub(crate) consumed_messages: u64,
    /// 转发的消息数。
    pub(crate) forwarded_messages: u64,
    /// 实际 decommit 的 extent 数。
    pub(crate) trimmed_extents: u32,
    /// 因 lease 或 grace 门禁未通过而保留 committed 的 extent 数。
    pub(crate) blocked_extents: u32,
    /// 因超出 relocation pause 预算而整块延后的 extent 数。
    pub(crate) deferred_extents: u32,
    /// 取走的 edge delta 数。
    pub(crate) edge_deltas: u64,
    /// drain 之后重算的 committed 字节。
    pub(crate) committed_after: u64,
    /// 本 cycle 真实计入滑动窗口的 GC cost unit；有界 drain 不消耗吞吐窗口，恒为 0。
    pub(crate) work_cost: u64,
    /// 本 cycle 的 remark 结局；有界 drain 不执行 remark，保持 `Complete` 默认值。
    pub(crate) remark: RemarkOutcome,
    /// 本 cycle 是否真实完成（remark 通过、credit 收敛并推进了 cycle epoch）。
    pub(crate) cycle_completed: bool,
    /// cycle 边界检查时五个 credit 来源是否收敛。
    pub(crate) credits_converged: bool,
}

impl RawWorld {
    /// 返回 pacing 平面。
    pub(crate) const fn pacing(&self) -> &PacingPlane {
        &self.pacing
    }

    /// 设置 runtime 管理内存的软上限；`None` 关闭 pressure debt。
    pub(crate) fn set_memory_limit(&mut self, limit: Option<u64>) {
        self.pacing.set_soft_memory_limit(limit);
    }

    /// 设置自动 GC 增长目标；`None` 等价于 `GcTarget::Off`。
    pub(crate) fn set_gc_target_percent(&mut self, percent: Option<u32>) {
        self.pacing.set_target_percent(percent);
    }

    /// 返回真实 live record 字节：committed 扣掉三类已分类字节后的残差。
    ///
    /// 残差是账本互斥分类的最后一个成员，也是 `growth_budget` 的输入；它取自
    /// `OwnerAccounting`，因此 `last_live_bytes` 始终是一个物理量而不是估值。
    pub(crate) fn live_record_bytes(&self) -> u64 {
        let mut total = 0_u64;
        for owner in 0..self.owners.len() as u32 {
            total = total.saturating_add(residual_live_bytes(self.accounting(owner)));
        }
        for owner in 0..self.resource_owners.len() as u32 {
            let token = self.resource_token(owner);
            if let Some(accounting) = self.directory().accounting(token.owner_id) {
                total = total.saturating_add(residual_live_bytes(accounting));
            }
        }
        total
    }

    /// 从 rt0 启动配置注入 pacing 参数。
    ///
    /// `GUGU_RUNTIME_GC_TARGET=off` 只关闭按分配 debt 触发的周期，软上限与 OOM 规则不变。
    pub(super) fn apply_startup_pacing(&mut self) -> Result<(), RawInvariant> {
        let Some(config) = self.rt0_config()?.copied() else {
            return Ok(());
        };
        let target = match config.gc_target() {
            super::super::startup::GcTargetConfig::Automatic(percent) => Some(percent),
            super::super::startup::GcTargetConfig::Off => None,
        };
        self.set_gc_target_percent(target);
        self.set_memory_limit(config.memory_limit());
        Ok(())
    }

    /// 返回 runtime 管理的已提交物理字节，即软上限的判定口径。
    ///
    /// provider 的 committed 总量是所有 runtime 管理 range 的物理页：raw plane 的 extent 与
    /// 协程栈 arena 都从同一 provider 提交，因此它是已经去重的总量。`classed_committed_bytes`
    /// 与 `stack_stats().committed_bytes` 是它的两个 disjoint 子集，只能用来校验覆盖关系；
    /// 把 stack committed 再加一次会把同一物理页计入两次，从而虚高 pressure debt。
    /// managed heap 由 LocalHeap 落地后也走同一 provider，因此这里不需要另加口径。
    pub(crate) fn pressure_committed_bytes(&self) -> u64 {
        let committed = self.provider_stats().committed_bytes;
        let covered = self
            .classed_committed_bytes()
            .saturating_add(self.stack_stats().committed_bytes);
        debug_assert!(
            committed >= covered,
            "provider committed {committed} 必须覆盖 raw 分类 {covered}"
        );
        committed
    }

    /// 返回 runtime committed 的三类互斥分类之和。
    pub(crate) fn classed_committed_bytes(&self) -> u64 {
        let mut total = 0_u64;
        for owner in 0..self.owners.len() as u32 {
            total = total.saturating_add(self.accounting(owner).committed_bytes());
        }
        for owner in 0..self.resource_owners.len() as u32 {
            let token = self.resource_token(owner);
            if let Some(accounting) = self.directory().accounting(token.owner_id) {
                total = total.saturating_add(accounting.committed_bytes());
            }
        }
        total
    }

    /// 返回三类可 drain 分类的提交快照。
    pub(crate) fn committed_classes(&self) -> CommittedClasses {
        let mut classes = CommittedClasses::default();
        for owner in 0..self.owners.len() as u32 {
            let accounting = self.accounting(owner);
            classes.pending_return_bytes = classes
                .pending_return_bytes
                .saturating_add(accounting.pending_return_bytes());
            classes.owner_cache_bytes = classes
                .owner_cache_bytes
                .saturating_add(accounting.owner_cache_bytes());
            classes.reclaimable_bytes = classes
                .reclaimable_bytes
                .saturating_add(accounting.reclaimable_bytes());
        }
        for owner in 0..self.resource_owners.len() as u32 {
            let token = self.resource_token(owner);
            let Some(accounting) = self.directory().accounting(token.owner_id) else {
                continue;
            };
            classes.pending_return_bytes = classes
                .pending_return_bytes
                .saturating_add(accounting.pending_return_bytes());
            classes.owner_cache_bytes = classes
                .owner_cache_bytes
                .saturating_add(accounting.owner_cache_bytes());
            classes.reclaimable_bytes = classes
                .reclaimable_bytes
                .saturating_add(accounting.reclaimable_bytes());
        }
        classes
    }

    /// 返回全部 owner 尚未发布的 return staging 字节。
    fn return_staging_bytes(&self) -> u64 {
        self.return_stagings.iter().fold(0_u64, |total, staging| {
            total.saturating_add(staging.bytes())
        })
    }

    /// 按当前物理状态重建 credit 快照。
    ///
    /// 五个来源各自对应一个真实结构：processor buffer、已发布 batch 账本、edge summary、
    /// owner pending 字节与 `RawWorld` 持有的 producer staging；没有对应路径的来源保持为零
    /// 而不是猜测。
    pub(crate) fn credit_snapshot(&self) -> CreditSnapshot {
        let mut buffer_keys = 0_u64;
        for processor in 0..self.barrier.processor_count() {
            if let Some(record) = self.barrier.processor(processor) {
                buffer_keys = buffer_keys.saturating_add(u64::from(record.buffer().len()));
            }
        }
        // 在途 region transfer 与 owner pending return 属于同一个 credit 来源：两者都是
        // 「已经离开生产者、还没被任何 owner 消费」的字节，因此在这里合并而不是新开来源。
        let region_transfers = self
            .regions
            .as_ref()
            .map_or(0, super::super::region::RegionPlane::pending_bytes);
        CreditSnapshot {
            barrier_buffer_keys: buffer_keys,
            card_mark_batches: self.barrier.pending_batch_bytes(),
            edge_deltas: u64::try_from(self.barrier.edges().pending()).expect("delta 数适配 u64"),
            pending_return_bytes: self
                .committed_classes()
                .pending_return_bytes
                .saturating_add(region_transfers),
            staging_bytes: self.return_staging_bytes(),
        }
    }

    /// 在真实 cycle 边界推进 credit 与 debt。
    ///
    /// 返回 `false` 表示 credit 尚未收敛：此时不得推进 epoch，也不得宣布 cycle 完成。
    /// 这是正常的状态机结果，不是错误——调用方按节奏重试即可。
    pub(crate) fn begin_pacing_cycle(&mut self, cycle_epoch: u64) -> Result<bool, RawInvariant> {
        let snapshot = self.credit_snapshot();
        self.pacing.observe_credits(snapshot);
        if !self.pacing.credits().converged() {
            return Ok(false);
        }
        self.pacing
            .begin_cycle(cycle_epoch)
            .map_err(|error| RawInvariant::new(error.to_string()))?;
        Ok(true)
    }

    /// 完成一次 cycle：记录存活字节与工作量基线。
    pub(crate) fn complete_pacing_cycle(&mut self, live_bytes: u64, counters: GcWorkCounters) {
        self.pacing.complete_cycle(live_bytes, counters);
    }

    /// 登记一次分配 debt；只在分配真正成功后调用。
    pub(crate) fn observe_allocation(&mut self, bytes: u64) {
        self.pacing.observe_allocation(bytes);
    }

    /// 推进 pressure 状态机并返回当前状态。
    pub(crate) fn poll_pressure(&mut self) -> PressureState {
        let committed = self.pressure_committed_bytes();
        let classes = self.committed_classes();
        self.pacing.update_pressure(committed, classes)
    }

    /// 请求一次分配的 headroom；按返回值执行真实 drain、forced cycle 或 OOM。
    ///
    /// 未配置软上限、或占用低于 enter 水位时不执行任何 drain，直接放行。请求被拒绝时按规范
    /// 走 rt0 的 `OutOfMemory` fatal，并返回不变量失败——调用方不能继续分配。
    pub(crate) fn request_headroom(
        &mut self,
        bytes: u64,
    ) -> Result<HeadroomDecision, RawInvariant> {
        let decision = self
            .pacing
            .request_headroom(self.pressure_committed_bytes(), bytes);
        match decision {
            HeadroomDecision::Granted | HeadroomDecision::Drain | HeadroomDecision::ForcedCycle => {
                Ok(decision)
            }
            HeadroomDecision::OutOfMemory => {
                let message = format!(
                    "无法在 soft memory limit 内取得 {bytes} 字节 headroom，当前 committed {}",
                    self.pressure_committed_bytes()
                );
                let _ = self.fatal(FatalKind::OutOfMemory, message.clone(), None);
                Err(RawInvariant::new(message))
            }
        }
    }

    /// allocation slow edge：用一次真实有界交接偿还 mark debt，并返回 assist 结局。
    ///
    /// 交接动作是以 memory pressure 原因冲刷本 owner 的 processor barrier 账本：一张交出的
    /// dirty card 折算 `CARD_GRANULARITY_BYTES` 个 mark cost unit（与 barrier 契约同源）。
    /// 没有真实交出任何键时返回 `NoWork` 而不记账，避免虚构进度。edge summary 属于 cycle
    /// credit，必须留给 cycle 边界取走，因此这里不动它。
    pub(crate) fn assist_on_slow_edge(
        &mut self,
        owner: u32,
    ) -> Result<AssistOutcome, RawInvariant> {
        let (_, card_keys) =
            self.flush_all_barriers_counted(owner, BarrierFlushReason::MemoryPressure)?;
        let available = card_keys.saturating_mul(u64::from(CARD_GRANULARITY_BYTES));
        Ok(self.pacing.assist(available))
    }

    /// 排空一个 owner 的 inbox。
    ///
    /// `exhaustive` 为真时循环到 inbox 为空（cycle 必须收敛）；为假时每 shard 只做一次有界
    /// service，剩余消息留给下一个节奏点，因此单次 drain 的暂停是有界的。
    pub(super) fn drain_inboxes(
        &mut self,
        owner: u32,
        budget: &ServiceBudget,
        exhaustive: bool,
    ) -> Result<(u32, u32), RawInvariant> {
        let mut forwarded = 0_u32;
        let mut consumed = 0_u32;
        for index in 0..super::super::OWNER_INBOX_SHARDS {
            let shard = ShardIndex::from_raw(index).expect("shard 编号合法");
            loop {
                let report = self.service(owner, shard, budget)?;
                forwarded += report.forwarded;
                consumed += report.items - report.forwarded;
                if report.items == 0 || report.stop != DrainStop::Budget || !exhaustive {
                    break;
                }
            }
        }
        Ok((forwarded, consumed))
    }

    /// 执行一次完整 GC cycle；`forced` 表示本 episode 的 forced full cycle。
    pub(crate) fn run_gc_cycle(
        &mut self,
        forced: bool,
    ) -> Result<PressureDrainReport, RawInvariant> {
        self.pressure_drain(DrainScope::Cycle, forced)
    }

    /// 执行一次 episode 内的有界 owner drain：有界预算、不完成 cycle。
    pub(crate) fn bounded_owner_drain(&mut self) -> Result<PressureDrainReport, RawInvariant> {
        self.pressure_drain(DrainScope::Bounded, false)
    }

    /// 执行一次 pressure drain。
    ///
    /// 两种作用域共用同一条实现：都先交出尚未发布的 return 链与 owner cache、排空 inbox、
    /// 推进 grace epoch、取走 edge delta、在 pause 预算内 trim/decommit 并重算 committed
    /// 快照；只有 `Cycle` 会过 remark 门禁、推进 cycle epoch 并记录工作量与基线。
    fn pressure_drain(
        &mut self,
        scope: DrainScope,
        forced: bool,
    ) -> Result<PressureDrainReport, RawInvariant> {
        let mut report = PressureDrainReport::default();
        // 1. 先交出未发布的 return 链，再关闭 owner 真实的 source-slab cache；`PressureDrain`
        //    关闭顺带以 memory-pressure 原因冲刷本 owner 的 barrier 账本，因此不需要重复 flush。
        for owner in 0..self.owners.len() as u32 {
            self.flush_return_staging(owner, FlushTrigger::OwnerPressure)?;
            self.close_cache(owner, RingCloseReason::PressureDrain)?;
        }
        // 2. 排空 owner inbox：cycle 需要彻底排空，有界 drain 只做一次有界 service。
        let budget = match scope {
            DrainScope::Cycle => ServiceBudget::pressure(u32::MAX, u64::MAX),
            DrainScope::Bounded => {
                let contract = self.pacing.contract();
                ServiceBudget::pressure(contract.owner_drain_items(), contract.owner_drain_bytes())
            }
        };
        for owner in 0..self.owners.len() as u32 {
            let (forwarded, consumed) =
                self.drain_inboxes(owner, &budget, scope == DrainScope::Cycle)?;
            report.forwarded_messages += u64::from(forwarded);
            report.consumed_messages += u64::from(consumed);
        }
        self.release_graced_nodes()?;
        // 2b. 推进 queue-page grace epoch：trim 的固定步数门禁只能由 epoch 前进推进，
        // 否则空载 extent 永远停在 GracePending，drain 就无法真实归还物理页。
        let inbox = self.inbox(0);
        self.open_grace(&inbox);
        self.close_grace(&inbox);
        self.advance_pending_extent_trims()?;
        // 3. 取走 owner-local edge summary：它属于 cycle credit，必须在 drain 结束前交出。
        report.edge_deltas =
            u64::try_from(self.take_edge_deltas().len()).expect("delta 数适配 u64");
        // 4. cycle 路径把「本 cycle 真实完成的工作」计入滑动窗口：非 forced cycle 超窗的部分
        //    转为 mark debt 由后续 assist 偿还，forced cycle 属于 emergency，可越过吞吐预算。
        //    有界 drain 不计费：它由 pause 预算约束，其工作量在 cycle 边界按 per-cycle delta
        //    一次性计入，避免同一个 cycle 重复计费。
        let counters = self.work_counters();
        if scope == DrainScope::Cycle {
            let cost = self.pacing.cycle_work_cost(counters);
            report.work_cost = self.pacing.worker_work(cost, forced);
            // 5. remark 是 cycle 终止门禁：超预算时发布 continuation 并保持 barrier 开启，
            //    因此本 cycle 不推进 epoch，也不宣布收敛。
            let remark = self
                .pacing
                .remark(cost, true)
                .map_err(|message| RawInvariant::new(message.to_owned()))?;
            report.remark = remark;
            if remark == RemarkOutcome::Complete {
                let next = self.barrier.cycle_epoch().saturating_add(1);
                // 先推进 barrier epoch：它可能把新草稿发布成 card batch，这些批次必须在收敛
                // 检查之前落地，否则新 credit epoch 会以「零在飞」开始却已有发布中的工作。
                self.advance_barrier_epoch(0, next)?;
                for owner in 0..self.owners.len() as u32 {
                    let (forwarded, consumed) = self.drain_inboxes(owner, &budget, true)?;
                    report.forwarded_messages += u64::from(forwarded);
                    report.consumed_messages += u64::from(consumed);
                }
                report.credits_converged = self.begin_pacing_cycle(next)?;
                if report.credits_converged {
                    report.cycle_completed = true;
                    if forced {
                        report.forced_cycles = 1;
                    }
                }
            }
        }
        // 6. trim/decommit：候选按 extent 整块判定，只有完整落在 pause 预算内的前缀才会被
        //    发布；超出的候选整块延后到下个 cycle，绝不部分发布一个 extent。
        let candidates = self.trim_candidates();
        let accepted = self.relocation_batch(&candidates);
        report.deferred_extents =
            u32::try_from(candidates.len().saturating_sub(accepted.len())).expect("候选数适配 u32");
        let trimmed = self.trim_extents(&accepted)?;
        report.trimmed_extents = trimmed.trimmed;
        report.blocked_extents = trimmed.blocked_count();
        // 7. 只有完整 cycle 才记录 live record 字节并复位 cycle 内 debt、窗口与工作量基线。
        if report.cycle_completed {
            let live = self.live_record_bytes();
            self.complete_pacing_cycle(live, counters);
        }
        report.committed_after = self.pressure_committed_bytes();
        let classes = self.committed_classes();
        self.pacing.note_drain(classes);
        self.pacing.update_pressure(report.committed_after, classes);
        Ok(report)
    }

    /// 返回 barrier 平面与 edge 取走的累计计数器快照。
    ///
    /// per-cycle 工作量由相邻两次快照之差得到：累计量只增，因此差值就是本 cycle 的真实工作，
    /// 不会像直接使用累计值那样跨 cycle 单调增长。
    fn work_counters(&self) -> GcWorkCounters {
        let stats = self.barrier_stats();
        GcWorkCounters {
            card_marks: stats.card_marks,
            edge_deltas: stats.edge_deltas,
            published_batches: stats.published_batches,
        }
    }

    /// 选出一批可发布的返回候选：按 extent 整块累加真实 pause footprint，直到再放进一个
    /// extent 就会超过预算为止。
    ///
    /// footprint 取自候选自身的真实度量：`bytes` 是本批撤销的已提交字节，`roots` 是本批触达
    /// 的 descriptor 数；空 extent 不携带任何字段更新，因此 `fields` 恒为 0。每个候选都必须
    /// 完整落在预算内（不在 extent 内部分发布），剩余候选保持不变由下个 cycle 继续。
    fn relocation_batch(&mut self, candidates: &[TrimCandidate]) -> Vec<TrimCandidate> {
        let mut accepted = Vec::new();
        let mut bytes = 0_u64;
        let mut roots = 0_u32;
        for candidate in candidates {
            let footprint = EvacuationFootprint {
                bytes: bytes.saturating_add(candidate.revoked_bytes),
                roots: roots.saturating_add(candidate.descriptors),
                fields: 0,
            };
            if self.pacing.evacuation(footprint) == EvacuationOutcome::Defer {
                break;
            }
            bytes = footprint.bytes;
            roots = footprint.roots;
            accepted.push(*candidate);
        }
        accepted
    }

    /// 推进一次 headroom 请求：先 drain，必要时再执行 forced cycle，最后重新检查。
    ///
    /// 返回 `true` 表示已经取得 headroom；`false` 表示两次机会用尽，调用方必须进入
    /// `OutOfMemory`。缓存估计远离 limit 时不会读任何全局快照。
    pub(crate) fn relieve_pressure(&mut self, bytes: u64) -> Result<bool, RawInvariant> {
        if !self.pacing.headroom_may_fail(bytes) {
            return Ok(true);
        }
        // 估计已经触到 limit：刷新真实快照后再决定，不能用估计值做拒绝判定。
        self.poll_pressure();
        match self.request_headroom(bytes)? {
            HeadroomDecision::Granted => Ok(true),
            HeadroomDecision::Drain => {
                self.bounded_owner_drain()?;
                match self.request_headroom(bytes)? {
                    HeadroomDecision::Granted => Ok(true),
                    HeadroomDecision::ForcedCycle => {
                        self.run_gc_cycle(true)?;
                        Ok(self.request_headroom(bytes)? == HeadroomDecision::Granted)
                    }
                    HeadroomDecision::Drain | HeadroomDecision::OutOfMemory => Ok(false),
                }
            }
            HeadroomDecision::ForcedCycle => {
                self.run_gc_cycle(true)?;
                Ok(self.request_headroom(bytes)? == HeadroomDecision::Granted)
            }
            HeadroomDecision::OutOfMemory => Ok(false),
        }
    }

    /// allocation 上的唯一慢路径：借 assist、按节奏推进 pressure 与自动 cycle，并请求 headroom。
    ///
    /// 门禁全部是标量比较；真实全局快照只在 `pressure_poll_due` 为真或 headroom 可能不足时
    /// 才读。无法取得 headroom 时 `OutOfMemory` 会写入 rt0 的 fatal 报告并让本次分配失败。
    pub(crate) fn pacing_slow_edge(&mut self, owner: u32, bytes: u64) -> Result<(), RawInvariant> {
        if !self.pacing.slow_edge_due() {
            return Ok(());
        }
        // 1. 只有 mark debt 达阈值且窗口还有额度时才做真实的有界 assist。
        if self.pacing.assist_due() {
            let _ = self.assist_on_slow_edge(owner)?;
        }
        // 2. 到节奏点才刷新真实 committed 快照并推进 hysteresis。
        if self.pacing.pressure_poll_due() {
            self.poll_pressure();
        }
        // 3. episode 内按节奏执行一次有界 drain，让 pending/cache/reclaimable 真实下降；
        //    不在每次分配上重扫整堆。
        if self.pacing.take_pressure_drain() {
            self.bounded_owner_drain()?;
        }
        // 4. allocation debt 越过缓存预算且窗口未溢出时尝试自动 cycle；两次尝试之间有分配量
        //    退避，未完成的 cycle 不会在每次分配上重试。
        if !self.pacing.window_exhausted() && self.pacing.take_cycle_attempt() {
            self.run_gc_cycle(false)?;
        }
        // 5. 软上限内请求 headroom：不足时按 drain → forced cycle → OOM 的顺序真实推进。
        self.relieve_pressure(bytes)?;
        Ok(())
    }
}

/// 返回一个 owner 账本中未分类的 live record 字节。
///
/// `OwnerAccounting` 的四类互斥且完备：committed 等于 pending、reclaimable、cache 与 live
/// 之和（`RawWorld::ledger_invariant` 在运行时强制该等式），因此残差就是 live record。
fn residual_live_bytes(accounting: &super::super::slab::OwnerAccounting) -> u64 {
    accounting
        .committed_bytes()
        .saturating_sub(accounting.pending_return_bytes())
        .saturating_sub(accounting.reclaimable_bytes())
        .saturating_sub(accounting.owner_cache_bytes())
}
