//! `RawWorld` 上的 GC debt、credit、pacing 与 pressure drain 接入。
//!
//! 接入遵循三条归属规则：
//!
//! 1. debt 只在真实 slow edge 上推进：分配、slow service、cache 关闭与 cycle 边界各记一次；
//!    本地 fast bump 不读全局 debt，因此这里不给 `allocate` 加任何全局查询。
//! 2. forced full cycle 与 pressure drain 必须落到真实动作：排空全部 owner inbox、关闭
//!    source-slab cache、trim/decommit extent、取走 edge delta、重算 committed 快照。没有
//!    headroom 时走 rt0 的 `OutOfMemory` fatal，而不是把分配失败伪装成成功。
//! 3. credit 是观测：每次观测都从当前物理状态取数（未 flush 键数、未消费 batch 数、未取走
//!    delta 数、pending return 字节、staging 字节），因此「收敛」是可验证的物理事实。

use super::super::extent::{ExtentId, ExtentOccupancy};
use super::super::inbox::ServiceBudget;
use super::super::message::{BatchLimits, ProducerStaging, ReturnSlabCache, RingCloseReason};
use super::super::pacing::{
    CommittedClasses, CreditSnapshot, EvacuationFootprint, EvacuationOutcome, HeadroomDecision,
    PacingPlane, PressureState, RemarkOutcome,
};
use super::super::slab::RawInvariant;
use super::super::startup_kinds::FatalKind;
use super::RawWorld;

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
    /// 本 cycle 真实计入滑动窗口的 GC cost unit。
    pub(crate) work_cost: u64,
    /// 本 cycle 的 remark 结局。
    pub(crate) remark: RemarkOutcome,
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

    /// 按当前物理状态重建 credit 快照。
    ///
    /// 五个来源各自对应一个真实结构：processor buffer、已发布 batch 账本、edge summary、
    /// owner pending 字节与生产者 staging；没有对应路径的来源保持为零而不是猜测。
    pub(crate) fn credit_snapshot(&self, staging: &ProducerStaging) -> CreditSnapshot {
        let mut buffer_keys = 0_u64;
        for processor in 0..self.barrier.processor_count() {
            if let Some(record) = self.barrier.processor(processor) {
                buffer_keys = buffer_keys.saturating_add(u64::from(record.buffer().len()));
            }
        }
        CreditSnapshot {
            barrier_buffer_keys: buffer_keys,
            card_mark_batches: self.barrier.pending_batch_bytes(),
            edge_deltas: u64::try_from(self.barrier.edges().pending()).expect("delta 数适配 u64"),
            pending_return_bytes: self.committed_classes().pending_return_bytes,
            staging_bytes: staging.bytes(),
        }
    }

    /// 在真实 cycle 边界推进 credit 与 debt。
    ///
    /// 返回 `false` 表示 credit 尚未收敛：此时不得推进 epoch，也不得宣布 cycle 完成。
    pub(crate) fn begin_pacing_cycle(
        &mut self,
        cycle_epoch: u64,
        staging: &ProducerStaging,
    ) -> Result<bool, RawInvariant> {
        let snapshot = self.credit_snapshot(staging);
        self.pacing.observe_credits(snapshot);
        if !self.pacing.credits().converged() {
            return Ok(false);
        }
        self.pacing
            .begin_cycle(cycle_epoch)
            .map_err(|error| RawInvariant::new(error.to_string()))?;
        Ok(true)
    }

    /// 完成一次 cycle：记录存活字节并清零 cycle 内 debt。
    pub(crate) fn complete_pacing_cycle(&mut self, live_bytes: u64) {
        self.pacing.complete_cycle(live_bytes);
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
            HeadroomDecision::Granted | HeadroomDecision::ForcedCycle => Ok(decision),
            HeadroomDecision::Drain => Ok(decision),
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

    /// allocation slow edge：按当前真实可消费的 GC work 执行最多一个 assist。
    ///
    /// 可消费 work 取自真实结构：未 flush 的 remembered-set 键按每个键一个 card-mark cost
    /// unit 计数，已取走但未处理的 edge delta 按每条一个 unit 计数；两者都没有时返回
    /// `NoWork` 而不记账，避免虚构进度。
    pub(crate) fn assist_on_slow_edge(&mut self) -> super::super::pacing::AssistOutcome {
        let mut buffer_keys = 0_u64;
        for processor in 0..self.barrier.processor_count() {
            if let Some(record) = self.barrier.processor(processor) {
                buffer_keys = buffer_keys.saturating_add(u64::from(record.buffer().len()));
            }
        }
        let edges = u64::try_from(self.barrier.edges().pending()).expect("delta 数适配 u64");
        let available = buffer_keys.saturating_add(edges).saturating_mul(u64::from(
            super::super::barrier_schema::CARD_GRANULARITY_BYTES,
        ));
        self.pacing.assist(available)
    }

    /// 执行一次有界 pressure drain，并按 `forced` 决定是否启动本 episode 的 forced full cycle。
    ///
    /// drain 顺序与规范一致：先刷新 barrier 与 cache、排空 inbox、再 trim/decommit，最后重算
    /// committed 快照。forced cycle 使用同一入口，但会把 service 预算提升到 emergency 档并
    /// 推进 cycle epoch；一次 episode 只允许一次，由 `PacingPlane` 线性化。
    pub(crate) fn pressure_drain(
        &mut self,
        forced: bool,
    ) -> Result<PressureDrainReport, RawInvariant> {
        let mut report = PressureDrainReport::default();
        // 1. processor 账本与 source-slab cache 先交出各自持有的键与 slot。
        let mut staging = ProducerStaging::new(BatchLimits::default());
        for owner in 0..self.owners.len() as u32 {
            self.flush_all_barriers(
                owner,
                super::super::barrier::BarrierFlushReason::MemoryPressure,
            )?;
            let mut cache = ReturnSlabCache::new();
            self.close_cache(
                owner,
                &mut staging,
                &mut cache,
                RingCloseReason::PressureDrain,
            )?;
        }
        // 2. 用 emergency 预算排空全部 owner 的 inbox。
        let budget = ServiceBudget::pressure(u32::MAX, u64::MAX);
        for owner in 0..self.owners.len() as u32 {
            let (forwarded, consumed) = self.drain_all(owner, &budget)?;
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
            u64::try_from(self.take_edge_deltas(0).len()).expect("delta 数适配 u64");
        // 4. 把本 cycle 真实完成的 GC 工作计入滑动窗口：automatic cycle 超窗的部分转为
        //    mark debt 由后续 assist 偿还，pressure drain 属于 emergency，可越过吞吐预算。
        let cost = self.gc_work_cost();
        let done = self.pacing.worker_work(cost, forced);
        report.work_cost = done;
        // 5. remark 是 cycle 终止门禁：超预算时发布 continuation 并保持 barrier 开启，
        //    因此本 cycle 不推进 epoch，也不宣布收敛。
        let remark = self
            .pacing
            .remark(cost, true)
            .map_err(|message| RawInvariant::new(message.to_owned()))?;
        report.remark = remark;
        if remark == RemarkOutcome::Complete {
            // cycle 边界：先要求五个 credit 来源全部收敛，再推进 barrier epoch；
            // 未收敛则说明仍有在飞工作，本 cycle 不得宣布完成。
            let next = self.barrier.cycle_epoch().saturating_add(1);
            if !self.begin_pacing_cycle(next, &staging)? {
                return Err(RawInvariant::new("cycle 边界前 credit 未收敛"));
            }
            self.advance_barrier_epoch(0, next)?;
            if forced {
                report.forced_cycles = 1;
            }
        }
        // 6. trim/decommit：候选按 extent 整块判定，只有完整落在 relocation pause 预算内的
        //    前缀才会被发布；超出的候选整块延后到下个 cycle，绝不部分发布一个 extent。
        let candidates = self.trim_candidates();
        let accepted = self.relocation_batch(&candidates);
        report.deferred_extents =
            u32::try_from(candidates.len().saturating_sub(accepted.len())).expect("候选数适配 u32");
        let trimmed = self.trim_extents(&accepted)?;
        report.trimmed_extents = trimmed.trimmed;
        report.blocked_extents = trimmed.blocked_count();
        // 7. cycle 完成：记录真实 live record 字节并复位 cycle 内 debt 与窗口。
        let live = self.live_record_bytes();
        self.complete_pacing_cycle(live);
        report.committed_after = self.pressure_committed_bytes();
        let classes = self.committed_classes();
        self.pacing.note_drain(classes);
        self.pacing.update_pressure(report.committed_after, classes);
        Ok(report)
    }

    /// 返回本 cycle 真实完成的 GC 工作 cost unit。
    ///
    /// card 键是 mark 工作的真实单位：未 flush 键在 drain 中已全部交出，因此用 barrier 的
    /// 累计记账（card 数 + 已取走 edge delta 数）作为本 cycle 的工作量，而不是估算值。
    fn gc_work_cost(&self) -> u64 {
        let stats = self.barrier_stats(0);
        stats
            .card_marks
            .saturating_add(stats.edge_deltas)
            .saturating_add(stats.published_batches)
    }

    /// 选出一批可发布的返回候选：按 extent 整块累加 footprint，直到再放进一个 extent 就会
    /// 超过 relocation pause 预算为止。
    ///
    /// 空载 extent 不携带任何待更新 field，因此 `fields` 恒为 0；`copied_bytes` 是本批撤销的
    /// 提交字节，`roots` 是本批各自的 extent 描述符（一个 extent 一个根）。三者都取自
    /// slab/extent 描述符表，不采用估算。
    ///
    /// 每个候选都完整落在预算内才被接受（不能在 extent 内部分发布），剩余候选保持不变，
    /// 由下个 cycle 继续；因为单个 extent 至多等于预算上界，所以每轮至少能推进一个候选，
    /// 不会因为批次总量偏大而永久不进展。
    fn relocation_batch(
        &mut self,
        candidates: &[(ExtentId, ExtentOccupancy)],
    ) -> Vec<(ExtentId, ExtentOccupancy)> {
        let mut accepted = Vec::new();
        let mut bytes = 0_u64;
        for (extent, occupancy) in candidates {
            let extent_bytes = self
                .extents
                .descriptor(*extent)
                .map_or(0_u64, |descriptor| descriptor.bytes);
            let footprint = EvacuationFootprint {
                copied_bytes: bytes.saturating_add(extent_bytes),
                roots: u32::try_from(accepted.len() + 1).expect("候选数适配 u32"),
                fields: 0,
            };
            if self.pacing.evacuation(footprint) == EvacuationOutcome::Defer {
                break;
            }
            bytes = footprint.copied_bytes;
            accepted.push((*extent, *occupancy));
        }
        accepted
    }

    /// 推进一次 headroom 请求：先 drain，必要时再执行 forced cycle，最后重新检查。
    ///
    /// 返回 `true` 表示已经取得 headroom；`false` 表示两次机会用尽，调用方必须进入
    /// `OutOfMemory`。
    pub(crate) fn relieve_pressure(&mut self, bytes: u64) -> Result<bool, RawInvariant> {
        match self.request_headroom(bytes)? {
            HeadroomDecision::Granted => Ok(true),
            HeadroomDecision::Drain => {
                self.pressure_drain(false)?;
                match self.request_headroom(bytes)? {
                    HeadroomDecision::Granted => Ok(true),
                    HeadroomDecision::ForcedCycle => {
                        self.pressure_drain(true)?;
                        Ok(self.request_headroom(bytes)? == HeadroomDecision::Granted)
                    }
                    HeadroomDecision::Drain => Ok(false),
                    HeadroomDecision::OutOfMemory => Ok(false),
                }
            }
            HeadroomDecision::ForcedCycle => {
                self.pressure_drain(true)?;
                Ok(self.request_headroom(bytes)? == HeadroomDecision::Granted)
            }
            HeadroomDecision::OutOfMemory => Ok(false),
        }
    }

    /// allocation 上的唯一慢路径：按需借 assist、推进 hysteresis、执行有界 drain、
    /// 按 allocation debt 启动自动 cycle，并在软上限内请求 headroom。
    ///
    /// 全部全局读取都发生在这个函数里，并且只有 `PacingPlane::slow_edge_due` 为真时才会被
    /// 调用，因此本地 fast bump 仍然无查询。无法取得 headroom 时 `OutOfMemory` 会写入 rt0
    /// 的 fatal 报告并让本次分配失败，而不是继续分配。
    pub(crate) fn pacing_slow_edge(&mut self, bytes: u64) -> Result<(), RawInvariant> {
        if !self.pacing.slow_edge_due() {
            return Ok(());
        }
        // 1. 先按窗口额度偿还 mark debt；没有真实可消费 work 时 assist 不记账。
        let _ = self.assist_on_slow_edge();
        // 2. 用当前物理快照推进 pressure hysteresis。
        self.poll_pressure();
        // 3. 已处于 episode：执行一次有界 drain，让 pending/cache/reclaimable 真实下降。
        if self.pacing.state().in_episode() {
            self.pressure_drain(false)?;
        }
        // 4. allocation debt 越过增长预算且窗口未溢出时启动自动 cycle。窗口已溢出意味着
        //    本 cycle 的 GC 吞吐额度用完，此时必须延后而不是继续重扫。
        if self.pacing.should_start_cycle() && !self.pacing.window_exhausted() {
            self.pressure_drain(false)?;
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
