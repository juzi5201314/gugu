//! GC debt、credit、pacing 与 pressure drain 的确定性回归；不睡眠、不读熵、不启动线程。

use super::RawWorld;
use crate::runtime::barrier::{BarrierFlushReason, BarrierSite};
use crate::runtime::barrier_schema::CARD_GRANULARITY_BYTES;
use crate::runtime::inbox::ServiceBudget;
use crate::runtime::message::BatchLimits;
use crate::runtime::message::ReturnKind;
use crate::runtime::pacing::{
    AssistOutcome, CommittedClasses, CreditPlane, CreditSnapshot, CreditSource,
    EvacuationFootprint, EvacuationOutcome, GcWorkCounters, HeadroomDecision, PacingPlane,
    PressureState, RemarkOutcome,
};
use crate::runtime::pacing_schema::{
    ASSIST_OUTCOME_NAMES, ASSIST_QUANTUM, ASSIST_THRESHOLD, DRAIN_CLASS_NAMES,
    EVACUATION_OUTCOME_NAMES, GcPacingDemand, GcPacingRuntimeContract, MARK_COST_PER_BYTE,
    MIN_GROWTH_BUDGET, PACING_SCHEMA, PRESSURE_CLEAR_RATIO, PRESSURE_ENTER_RATIO,
    REMARK_COST_BUDGET,
};
use crate::runtime::size_class::RuntimeSizeClassId;
use crate::runtime::slab::{MemoryDomainId, SlabDescriptorId};
use crate::runtime::{PlatformProfile, Rt0Demand};
use crate::runtime::{RawResourceDemand, SchedulerDemand, StackMapDemand, SyncDemand, WaitDemand};
use crate::runtime::{barrier_schema::BarrierDemand, gc_metadata_schema::GcMetadataDemand};
use crate::{TargetName, runtime::RawPlaneDemand, runtime::RawPlanePolicyV1};

fn world(owners: u32, nodes: u32) -> RawWorld {
    RawWorld::new(7, owners, nodes, BatchLimits::default()).expect("raw world 可创建")
}

fn shard(index: u32) -> crate::runtime::inbox::ShardIndex {
    crate::runtime::inbox::ShardIndex::from_raw(index).expect("shard 编号合法")
}

/// 一道会产生 card 键的 hybrid 屏障写入：old 指向 nursery 且位于 old generation。
fn card_site(arena: u64, generation: u32, offset: u64, epoch: u64) -> BarrierSite {
    BarrierSite {
        arena_descriptor: arena,
        arena_generation: generation,
        offset,
        cycle_epoch: epoch,
        old_present: true,
        new_present: true,
        new_in_nursery: true,
        owner_old: true,
        marking: true,
        stack_grey: true,
        new_block: Some(9),
        source_block: 3,
        new_owner: 0,
        source_owner: 1,
    }
}

/// 用内建契约创建平面；不修改任何参数。
fn pacing_plane() -> PacingPlane {
    PacingPlane::new(GcPacingRuntimeContract::build(GcPacingDemand::default()).expect("契约可构建"))
}

#[test]
fn pacing_contract_is_self_consistent_and_rejects_drift() {
    let contract = GcPacingRuntimeContract::build(GcPacingDemand::default()).expect("契约可构建");
    assert_eq!(contract.schema(), PACING_SCHEMA);
    assert_eq!(contract.profile(), "mosaic-default");
    assert_eq!(contract.min_growth_budget(), MIN_GROWTH_BUDGET);
    assert_eq!(contract.assist_quantum(), ASSIST_QUANTUM);
    assert_eq!(contract.mark_cost_per_byte(), MARK_COST_PER_BYTE);
    assert_eq!(contract.remark_cost_budget(), REMARK_COST_BUDGET);
    assert_eq!(contract.pressure_enter_ratio(), PRESSURE_ENTER_RATIO);
    assert_eq!(contract.pressure_clear_ratio(), PRESSURE_CLEAR_RATIO);
    assert_eq!(contract.pressure_states, ["steady", "drain", "emergency"]);
    // 窗口预算是 `window × fraction / 100`，且至少容纳一次 assist quantum。
    assert_eq!(contract.gc_cpu_window_budget(), 1 << 22);
    assert!(contract.gc_cpu_window_budget() >= contract.assist_quantum());
    // drain 分类必须与内存账本 committed 分区的独立计数器逐项一致。
    assert_eq!(contract.drain_classes, DRAIN_CLASS_NAMES);
    assert_eq!(contract.assist_outcomes, ASSIST_OUTCOME_NAMES);
    assert_eq!(contract.remark_outcomes, ["complete", "continuation"]);
    assert_eq!(contract.evacuation_outcomes, EVACUATION_OUTCOME_NAMES);
    assert_eq!(contract.credit_sources.len(), 5);
    assert_eq!(contract.pressure_poll_bytes(), 1 << 20);
    assert_eq!(contract.owner_drain_items(), 64);
    assert_eq!(contract.owner_drain_bytes(), 1 << 16);
    assert_eq!(contract.owner_drain_interval_bytes(), 1 << 20);
    assert_ne!(contract.fingerprint(), [0_u8; 32]);
    let dump = contract.dump();
    assert!(dump.contains("pacing schema=2 profile=mosaic-default revision=2"));
    assert!(dump.contains("pacing-drain poll=1048576 items=64 bytes=65536 interval=1048576"));
    assert!(dump.contains("pacing-credit-sources barrier-buffer,card-mark-batch,edge-delta,pending-return,producer-staging"));
    assert!(
        dump.contains(
            "pacing-drain-classes owner-cache-bytes,pending-return-bytes,reclaimable-bytes"
        )
    );
}

#[test]
fn pacing_contract_rejects_parameter_and_catalog_drift() {
    let base = GcPacingRuntimeContract::build(GcPacingDemand::default()).expect("契约可构建");
    for mutate in [
        Box::new(|c: &mut GcPacingRuntimeContract| c.assist_quantum = 1) as Box<dyn Fn(&mut _)>,
        Box::new(|c: &mut GcPacingRuntimeContract| c.gc_cpu_fraction = 0),
        Box::new(|c: &mut GcPacingRuntimeContract| c.min_growth_budget = 3),
        Box::new(|c: &mut GcPacingRuntimeContract| c.pressure_enter_ratio = 60),
        Box::new(|c: &mut GcPacingRuntimeContract| c.pressure_clear_ratio = 0),
        Box::new(|c: &mut GcPacingRuntimeContract| c.evacuation_pause_bytes = 4096),
        Box::new(|c: &mut GcPacingRuntimeContract| c.pressure_poll_bytes = 0),
        Box::new(|c: &mut GcPacingRuntimeContract| c.owner_drain_items = 0),
        Box::new(|c: &mut GcPacingRuntimeContract| c.owner_drain_bytes = 1),
        Box::new(|c: &mut GcPacingRuntimeContract| c.owner_drain_interval_bytes = 0),
        Box::new(|c: &mut GcPacingRuntimeContract| c.drain_classes = vec!["live-bytes".to_owned()]),
        Box::new(|c: &mut GcPacingRuntimeContract| c.credit_sources = vec!["only-one".to_owned()]),
        Box::new(|c: &mut GcPacingRuntimeContract| c.profile = "other".to_owned()),
    ] {
        let mut contract = base.clone();
        mutate(&mut contract);
        assert!(
            contract.verify().is_err(),
            "参数或目录漂移必须被拒绝：{contract:?}"
        );
    }
    // 指纹漂移也必须被拒绝：内容与登记指纹不再一致。
    let mut tampered = base.clone();
    tampered.fingerprint = [9; 32];
    assert!(tampered.verify().is_err());
    // 指纹随需求变化：站点数变化必须改变契约身份。
    let revised = GcPacingRuntimeContract::build(GcPacingDemand {
        alloc_sites: 7,
        ..GcPacingDemand::default()
    })
    .expect("契约可构建");
    assert_ne!(base.fingerprint(), revised.fingerprint());
}

#[test]
fn growth_budget_and_debt_follow_the_documented_formula() {
    let mut plane = pacing_plane();
    // 未分配时 debt 为零，且不触发自动 cycle。
    assert_eq!(plane.allocation_debt(), 0);
    assert_eq!(plane.mark_debt(), 0);
    assert!(!plane.should_start_cycle());
    // 分配量低于 min_growth_budget 时仍无 debt。
    plane.observe_allocation(MIN_GROWTH_BUDGET - 1);
    assert_eq!(plane.allocation_debt(), 0);
    // 越过下限后 debt 等于超出量，mark debt 按 mark_cost_per_byte 折算。
    plane.observe_allocation(4096);
    assert_eq!(plane.growth_budget(), MIN_GROWTH_BUDGET);
    assert_eq!(plane.allocation_debt(), 4095);
    assert_eq!(plane.mark_debt(), 4095 * u64::from(MARK_COST_PER_BYTE));
    assert!(plane.should_start_cycle());
    // 存活量涨大时预算取 max(min_growth_budget, last_live × target%)。
    plane.complete_cycle(MIN_GROWTH_BUDGET * 4, GcWorkCounters::default());
    assert_eq!(plane.growth_budget(), MIN_GROWTH_BUDGET * 4);
    assert_eq!(plane.allocation_debt(), 0);
    // `GcTarget::Off` 只关闭 debt 触发，不关闭其它路径。
    plane.set_target_percent(None);
    plane.observe_allocation(MIN_GROWTH_BUDGET * 8);
    assert_eq!(plane.growth_budget(), MIN_GROWTH_BUDGET);
    assert!(!plane.should_start_cycle());
}

#[test]
fn assist_stays_within_quantum_and_never_invents_progress() {
    let mut plane = pacing_plane();
    // 未达阈值时不 assist。
    assert_eq!(plane.assist(1 << 20), AssistOutcome::None);
    // 超过阈值但没有可消费 work：返回 NoWork 且不改变账本。
    plane.observe_allocation(MIN_GROWTH_BUDGET + ASSIST_THRESHOLD + 1_000_000);
    let debt_before = plane.mark_debt();
    assert_eq!(plane.assist(0), AssistOutcome::NoWork);
    assert_eq!(plane.mark_debt(), debt_before);
    assert_eq!(plane.assists(), 0);
    // 一次 assist 最多偿还一个 quantum。
    let before = plane.mark_debt();
    let outcome = plane.assist(u64::MAX);
    assert_eq!(outcome, AssistOutcome::QuantumTruncated);
    assert_eq!(plane.assist_cost(), ASSIST_QUANTUM);
    assert_eq!(plane.mark_debt(), before - ASSIST_QUANTUM);
    // 可消费 work 小于 quantum 时如实报告 completed work。
    let mut plane = pacing_plane();
    plane.observe_allocation(MIN_GROWTH_BUDGET + ASSIST_THRESHOLD + 32);
    assert_eq!(plane.assist(32), AssistOutcome::WithinQuantum);
    assert_eq!(plane.assist_cost(), 32);
    assert_eq!(plane.mark_debt(), ASSIST_THRESHOLD);
}

#[test]
fn assist_repays_only_the_cost_it_actually_completed() {
    let mut plane = pacing_plane();
    // 拖欠的 mark 工作与 allocation debt 同时非零：偿还量只能从两项合计里扣一次。
    plane.observe_allocation(MIN_GROWTH_BUDGET + ASSIST_THRESHOLD);
    plane.observe_mark_work(ASSIST_THRESHOLD);
    let before = plane.mark_debt();
    assert_eq!(before, ASSIST_THRESHOLD * 2);
    assert_eq!(plane.assist(ASSIST_QUANTUM), AssistOutcome::WithinQuantum);
    assert_eq!(
        plane.mark_debt(),
        before - ASSIST_QUANTUM,
        "债务只能按真实偿还量下降一次"
    );
    // 拖欠工作优先被冲抵，allocation debt 只在余量里按 mark_cost_per_byte 折字节。
    assert_eq!(plane.allocation_debt(), ASSIST_THRESHOLD);
}

#[test]
fn gc_cpu_window_limits_worker_work_and_defers_to_debt() {
    let mut plane = pacing_plane();
    let budget = plane.contract().gc_cpu_window_budget();
    // 普通 worker 在窗口内全额消费。
    assert_eq!(plane.worker_work(budget / 2, false), budget / 2);
    assert!(!plane.window_exhausted());
    // 超出窗口的部分转为 debt，而不是绕过预算。
    let consumed = plane.worker_work(budget, false);
    assert_eq!(consumed, budget - budget / 2);
    assert!(plane.window_exhausted());
    assert_eq!(plane.mark_debt(), budget - budget / 2);
    // emergency 可以越过吞吐预算，但不会凭空产生 credit。
    let emergency = plane.worker_work(4096, true);
    assert_eq!(emergency, 4096);
    // 窗口在 cycle 边界前进并复位，但拖欠的 mark 工作必须跨 cycle 存活由 assist 归还。
    let deferred = budget - budget / 2;
    plane.complete_cycle(0, GcWorkCounters::default());
    assert!(!plane.window_exhausted());
    assert_eq!(
        plane.mark_debt(),
        deferred,
        "deferred mark debt 不得在同一个 drain 内被清零"
    );
}

#[test]
fn cycle_work_cost_is_per_cycle_and_advances_the_baseline() {
    let mut plane = pacing_plane();
    let first = GcWorkCounters {
        card_marks: 5,
        edge_deltas: 2,
        published_batches: 1,
    };
    assert_eq!(plane.cycle_work_cost(first), 8);
    plane.complete_cycle(0, first);
    let second = GcWorkCounters {
        card_marks: 9,
        edge_deltas: 3,
        published_batches: 2,
    };
    assert_eq!(plane.cycle_work_cost(second), 6, "第二个 cycle 只计增量");
    plane.complete_cycle(0, second);
    // 累计值涨到远超 remark 预算，也只影响两次快照之间的差值。
    let huge = GcWorkCounters {
        card_marks: 1 << 30,
        edge_deltas: 0,
        published_batches: 0,
    };
    assert_eq!(plane.cycle_work_cost(huge), (1 << 30) - 9);
}

#[test]
fn remark_over_budget_publishes_continuation_and_requires_open_barrier() {
    let mut plane = pacing_plane();
    // barrier 未开启时不得执行 remark：mark cycle 尚未终止。
    assert!(plane.remark(1, false).is_err());
    assert_eq!(
        plane.remark(REMARK_COST_BUDGET, true).expect("预算内"),
        RemarkOutcome::Complete
    );
    assert_eq!(plane.remark_continuations(), 0);
    assert_eq!(
        plane.remark(REMARK_COST_BUDGET + 1, true).expect("超预算"),
        RemarkOutcome::Continuation
    );
    assert_eq!(plane.remark_continuations(), 1);
}

#[test]
fn evacuation_defers_whole_block_when_any_bound_is_exceeded() {
    let mut plane = pacing_plane();
    let contract = plane.contract().clone();
    // 三项都恰好命中上界时允许整块发布。
    assert_eq!(
        plane.evacuation(EvacuationFootprint {
            bytes: contract.evacuation_pause_bytes(),
            roots: contract.evacuation_pause_roots(),
            fields: contract.evacuation_pause_fields(),
        }),
        EvacuationOutcome::Admit
    );
    // 任一上界超出都整块延后，不允许部分发布。
    for footprint in [
        EvacuationFootprint {
            bytes: contract.evacuation_pause_bytes() + 1,
            roots: 1,
            fields: 1,
        },
        EvacuationFootprint {
            bytes: 1,
            roots: contract.evacuation_pause_roots() + 1,
            fields: 1,
        },
        EvacuationFootprint {
            bytes: 1,
            roots: 1,
            fields: contract.evacuation_pause_fields() + 1,
        },
    ] {
        assert_eq!(plane.evacuation(footprint), EvacuationOutcome::Defer);
    }
    assert_eq!(plane.evacuation_counts(), (1, 3));
}

#[test]
fn credit_requires_every_source_to_converge() {
    let mut plane = pacing_plane();
    // 单个来源为空不是完成条件。
    plane.observe_credits(CreditSnapshot {
        barrier_buffer_keys: 0,
        card_mark_batches: 0,
        edge_deltas: 0,
        pending_return_bytes: 0,
        staging_bytes: 3,
    });
    assert!(!plane.credits().converged());
    assert_eq!(plane.mark_credit_pending(), 3);
    // 全部归零后才是收敛。
    plane.observe_credits(CreditSnapshot::default());
    assert!(plane.credits().converged());
    assert_eq!(plane.mark_credit_pending(), 0);
    // cycle 边界要求 credit 先收敛，且 epoch 只能前进。
    let mut credit = CreditPlane::new(4);
    credit.observe(CreditSource::EdgeDelta, 2);
    assert!(credit.begin_cycle(5).is_err());
    credit.observe(CreditSource::EdgeDelta, 0);
    credit.begin_cycle(5).expect("收敛后可以推进 epoch");
    assert!(credit.begin_cycle(5).is_err());
    // 观测峰值记录在 cycle 内，boundary 之后复位。
    credit.observe(CreditSource::BarrierBuffer, 7);
    assert_eq!(credit.peak(CreditSource::BarrierBuffer), 7);
    credit.observe(CreditSource::BarrierBuffer, 0);
    credit.begin_cycle(6).expect("再次推进");
    assert_eq!(credit.peak(CreditSource::BarrierBuffer), 0);
}

#[test]
fn pressure_hysteresis_opens_once_and_closes_after_all_classes_drain() {
    let mut plane = pacing_plane();
    plane.set_soft_memory_limit(Some(1000));
    // 未配置 limit 时 pressure debt 恒为 0；配置后按超出量计算。
    let mut unbounded = pacing_plane();
    unbounded.set_soft_memory_limit(None);
    assert_eq!(unbounded.pressure_debt(1_000_000), 0);
    assert_eq!(plane.pressure_debt(1400), 400);
    // 达到 enter 水位开启 episode 并进入 Drain。
    let full = CommittedClasses {
        pending_return_bytes: 10,
        owner_cache_bytes: 10,
        reclaimable_bytes: 10,
    };
    assert_eq!(plane.state(), PressureState::Steady);
    assert_eq!(plane.update_pressure(860, full), PressureState::Drain);
    assert_eq!(plane.episode().epoch, 1);
    // 分类为空不构成 drain 证据：一次真实 drain 发生前不得结束 episode。
    assert_eq!(
        plane.update_pressure(600, CommittedClasses::default()),
        PressureState::Drain
    );
    // 分类未全部 drain 完时同样不得结束。
    let partial = CommittedClasses {
        pending_return_bytes: 0,
        owner_cache_bytes: 5,
        reclaimable_bytes: 0,
    };
    assert_eq!(plane.update_pressure(600, partial), PressureState::Drain);
    // 一次真实 owner drain 覆盖三类分类并降到 clear 水位以下，episode 才结束。
    let drained = CommittedClasses::default();
    assert_eq!(
        plane.note_drain(drained),
        3,
        "三类分类都必须被这次 drain 覆盖"
    );
    assert_eq!(plane.update_pressure(600, drained), PressureState::Steady);
    // 达到 soft limit 进入 Emergency。
    plane.set_soft_memory_limit(Some(1000));
    assert_eq!(
        plane.update_pressure(1000, drained),
        PressureState::Emergency
    );
    assert_eq!(plane.state().name(), "emergency");
    // 回到 limit 以下仍需一次真实 drain 才能结束新 episode。
    assert_eq!(plane.update_pressure(600, drained), PressureState::Drain);
    plane.note_drain(drained);
    assert_eq!(plane.update_pressure(600, drained), PressureState::Steady);
    // 结束 episode 后 forced cycle 标记复位。
    assert_eq!(plane.episode().forced_cycles, 0);
}

#[test]
fn one_pressure_episode_forces_at_most_one_full_cycle_then_oom() {
    let mut plane = pacing_plane();
    plane.set_soft_memory_limit(Some(1000));
    // 低于 limit 的请求直接放行。
    assert_eq!(plane.request_headroom(100, 100), HeadroomDecision::Granted);
    // 超过 limit：先 drain，再 forced cycle，最后才是 OOM。
    assert_eq!(plane.request_headroom(1000, 10), HeadroomDecision::Drain);
    assert_eq!(
        plane.request_headroom(1000, 10),
        HeadroomDecision::ForcedCycle
    );
    assert_eq!(
        plane.request_headroom(1000, 10),
        HeadroomDecision::OutOfMemory
    );
    assert_eq!(plane.forced_cycle_total(), 1);
    assert_eq!(plane.headroom_oom(), 1);
    // 同一 episode 内不会第二次强制 full cycle。
    assert_eq!(plane.episode().forced_cycles, 1);
    assert!(!plane.episode().all_classes_drained());
    // 分类 drain 完成并降到 clear 水位后结束 episode，下一 episode 才有权再强制一次。
    plane.note_drain(CommittedClasses::default());
    plane.update_pressure(500, CommittedClasses::default());
    assert_eq!(plane.state(), PressureState::Steady);
    assert_eq!(plane.request_headroom(1000, 10), HeadroomDecision::Drain);
    assert_eq!(
        plane.request_headroom(1000, 10),
        HeadroomDecision::ForcedCycle
    );
    assert_eq!(plane.forced_cycle_total(), 2);
}

#[test]
fn headroom_accounts_the_requested_bytes_without_faking_emergency() {
    let mut plane = pacing_plane();
    plane.set_soft_memory_limit(Some(1000));
    // committed 只有 900，但本次请求 200 字节会把占用推过 limit：必须进入 drain 链。
    assert_eq!(plane.request_headroom(900, 200), HeadroomDecision::Drain);
    assert_eq!(
        plane.state(),
        PressureState::Drain,
        "committed 未达 limit 不得记为 emergency"
    );
    // 同一 episode 内第二次请求升级为 forced cycle，而不是直接 OOM。
    assert_eq!(
        plane.request_headroom(900, 200),
        HeadroomDecision::ForcedCycle
    );
    // 请求本身低于 limit 且 committed 也低于 limit 时直接放行。
    let mut idle = pacing_plane();
    idle.set_soft_memory_limit(Some(1000));
    assert_eq!(idle.request_headroom(100, 100), HeadroomDecision::Granted);
}

#[test]
fn episode_drains_are_paced_by_the_interval_budget() {
    let mut plane = pacing_plane();
    plane.set_soft_memory_limit(Some(1000));
    assert_eq!(
        plane.update_pressure(860, CommittedClasses::default()),
        PressureState::Drain
    );
    assert!(
        plane.take_pressure_drain(),
        "episode 开启立即要求一次 drain"
    );
    assert!(!plane.take_pressure_drain(), "同一节奏点不重复 drain");
    plane.observe_allocation(1024);
    assert!(!plane.take_pressure_drain(), "未到间隔不得重复 drain");
    plane.observe_allocation(1 << 20);
    assert!(plane.take_pressure_drain(), "越过间隔后推进下一次 drain");
}

#[test]
fn world_reports_real_credit_sources_and_drains_them() {
    // 每条消息立即发布：credit 观测面对的是真实 inbox 内容，而不是未发布的 staging。
    let limits = BatchLimits {
        items: 1,
        batch_soft_bytes: 1,
    };
    let mut world = RawWorld::new(7, 1, 64, limits).expect("raw world 可创建");
    // 初始状态：没有任何在飞 credit。
    let snapshot = world.credit_snapshot();
    assert_eq!(snapshot.card_mark_batches, 0);
    assert_eq!(snapshot.edge_deltas, 0);
    assert_eq!(snapshot.pending_return_bytes, 0);
    assert_eq!(snapshot.staging_bytes, 0);
    assert!(world.begin_pacing_cycle(1).expect("cycle 边界"));
    // 分配后 allocation debt 真实推进。
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    assert!(world.pacing().allocated_since_cycle_bytes() > 0);
    // committed 的分类互斥：尚未归还的字节计入 live record，而不是 cache。
    assert!(world.committed_classes().owner_cache_bytes < world.classed_committed_bytes());
    let bytes = u64::from(
        world
            .classes()
            .get(class)
            .expect("class 已登记")
            .slot_stride,
    );
    world
        .queue_return(0, allocation.slot, bytes)
        .expect("归还成功");
    let classes = world.committed_classes();
    assert_eq!(classes.pending_return_bytes, bytes);
    // return pressure 是 pending 与 owner cache 之和：尚未归还的 cache 也必须被 drain 覆盖。
    assert_eq!(
        PacingPlane::return_pressure(classes),
        classes.pending_return_bytes + classes.owner_cache_bytes
    );
    // 发布真实 return message 后，credit 快照读取到同一物理事实。
    let message = world
        .message(
            world.token(0),
            ReturnKind::RawSlot,
            allocation.slot,
            u32::try_from(bytes).expect("stride 适配 u32"),
        )
        .expect("消息可构造");
    world
        .publish_message(0, &message, shard(0), None)
        .expect("发布成功");
    assert_eq!(
        world.credit_snapshot().staging_bytes,
        0,
        "item 上限为 1 时 staging 必须立即冲刷"
    );
    let (_, consumed) = world
        .drain_all(0, &ServiceBudget::new(8, 1 << 16))
        .expect("drain 成功");
    assert!(consumed > 0);
    assert_eq!(world.committed_classes().pending_return_bytes, 0);
    // 消费后 slot 进入 reclaimable，再由 ledger_invariant 证明分类互斥。
    world.ledger_invariant(0).expect("账本互斥成立");
    // credit 快照随后收敛：消息不再挂在 pending 上。
    let snapshot = world.credit_snapshot();
    assert_eq!(snapshot.pending_return_bytes, 0);
    assert_eq!(snapshot.edge_deltas, 0);
}

#[test]
fn pressure_drain_is_a_real_slice_that_frees_committed_bytes() {
    // producer staging 用最小上限：每条消息立即发布，drain 面对的是真实 inbox 内容。
    let limits = BatchLimits {
        items: 1,
        batch_soft_bytes: 1,
    };
    let mut world = RawWorld::new(7, 1, 64, limits).expect("raw world 可创建");
    let class = RuntimeSizeClassId::from_raw(0);
    let bytes = u64::from(
        world
            .classes()
            .get(class)
            .expect("class 已登记")
            .slot_stride,
    );
    // 分配、归还、构造真实 return message 并发布到目标 inbox。
    for _ in 0..8 {
        let allocation = world.allocate(0, class).expect("分配成功");
        world
            .queue_return(0, allocation.slot, bytes)
            .expect("归还成功");
        let message = world
            .message(
                world.token(0),
                ReturnKind::RawSlot,
                allocation.slot,
                u32::try_from(bytes).expect("stride 适配 u32"),
            )
            .expect("消息可构造");
        world
            .publish_message(0, &message, shard(0), None)
            .expect("发布成功");
    }
    assert_eq!(
        world.credit_snapshot().staging_bytes,
        0,
        "item 上限为 1 时 staging 必须立即冲刷"
    );
    let committed_before = world.pressure_committed_bytes();
    assert!(committed_before > 0);
    // 归还并发布后 pending 分类真实非零：消息在 drain 前归属 pending。
    assert!(world.committed_classes().pending_return_bytes > 0);
    assert!(world.credit_snapshot().pending_return_bytes > 0);
    // 第一次 drain：消息被消费，pending 必须归零。
    let first = world.run_gc_cycle(false).expect("drain 成功");
    assert_eq!(first.forced_cycles, 0);
    assert_eq!(first.forwarded_messages, 0);
    assert_eq!(first.consumed_messages, 8);
    assert_eq!(world.committed_classes().pending_return_bytes, 0);
    assert!(world.credit_snapshot().pending_return_bytes == 0);
    assert!(first.blocked_extents > 0 || first.trimmed_extents > 0);
    assert_eq!(first.edge_deltas, 0);
    // 每个完成的 cycle 都真实推进一次 barrier cycle epoch；credit 收敛是推进前提。
    let epoch_after_first = world.barrier().cycle_epoch();
    assert_eq!(epoch_after_first, 1);
    assert_eq!(first.remark, RemarkOutcome::Complete);
    // forced drain 走同一入口，并额外把本次 episode 的 forced cycle 记一。
    let forced = world.run_gc_cycle(true).expect("forced drain 成功");
    assert_eq!(forced.forced_cycles, 1);
    assert_eq!(world.barrier().cycle_epoch(), epoch_after_first + 1);
    world.ledger_invariant(0).expect("drain 后账本仍互斥");
    // 继续 drain 到 extent 过 grace 后，committed 必须真实回落。
    let mut trimmed = first.trimmed_extents + forced.trimmed_extents;
    for _ in 0..16 {
        trimmed += world
            .run_gc_cycle(false)
            .expect("drain 成功")
            .trimmed_extents;
    }
    assert!(trimmed > 0, "空载 extent 过 grace 后必须真实 decommit");
    assert!(world.pressure_committed_bytes() < committed_before);
    world.ledger_invariant(0).expect("trim 后账本仍互斥");
}

#[test]
fn headroom_denial_raises_out_of_memory_fatal_once() {
    let mut world = world(1, 64);
    // 未启动 rt0 时 fatal 不可用，因此这里只验证判定链：limit 足以放行。
    world.set_memory_limit(Some(u64::MAX));
    assert_eq!(
        world.request_headroom(0).expect("放行"),
        HeadroomDecision::Granted
    );
    world.set_memory_limit(None);
    assert_eq!(
        world.request_headroom(0).expect("放行"),
        HeadroomDecision::Granted
    );
    // 极小 limit：drain → forced cycle → OOM 的顺序由平面线性化，world 侧只做三次真实推进。
    // 先分配一次让 committed 超过 limit，否则请求仍在 headroom 内。
    let class = RuntimeSizeClassId::from_raw(0);
    world.allocate(0, class).expect("分配成功");
    world.set_memory_limit(Some(1));
    assert_eq!(
        world.request_headroom(0).expect("需 drain"),
        HeadroomDecision::Drain
    );
    assert_eq!(
        world.request_headroom(0).expect("需 forced cycle"),
        HeadroomDecision::ForcedCycle
    );
    let denial = world.request_headroom(0);
    assert!(denial.is_err(), "无法取得 headroom 必须失败");
    assert_eq!(world.pacing().headroom_oom(), 1);
    assert_eq!(world.pacing().forced_cycle_total(), 1);
}

#[test]
fn frame_pacing_contract_is_wired_into_the_raw_contract() {
    let contract = crate::runtime::RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        RawPlanePolicyV1::default(),
        RawPlaneDemand::default(),
        RawResourceDemand::default(),
        Rt0Demand::default(),
        SchedulerDemand::default(),
        WaitDemand::default(),
        SyncDemand::default(),
        StackMapDemand::default(),
        GcMetadataDemand::empty(),
        BarrierDemand::default(),
        GcPacingDemand {
            alloc_sites: 3,
            barrier_sites: 2,
            slow_edges: 5,
            managed_types: 11,
        },
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    contract.verify().expect("契约自洽");
    assert_eq!(contract.schema(), crate::runtime::model::RAW_MODEL_SCHEMA);
    assert_eq!(contract.pacing().demand().alloc_sites, 3);
    assert_eq!(contract.pacing().demand().slow_edges, 5);
    let dump = contract.dump();
    assert!(dump.contains("pacing schema=2"));
    assert!(
        dump.contains("pacing-demand alloc-sites=3 barrier-sites=2 slow-edges=5 managed-types=11")
    );
    // demand 进入契约指纹：站点数变化必须改变整体身份。
    let revised = crate::runtime::RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        RawPlanePolicyV1::default(),
        RawPlaneDemand::default(),
        RawResourceDemand::default(),
        Rt0Demand::default(),
        SchedulerDemand::default(),
        WaitDemand::default(),
        SyncDemand::default(),
        StackMapDemand::default(),
        GcMetadataDemand::empty(),
        BarrierDemand::default(),
        GcPacingDemand {
            alloc_sites: 4,
            ..GcPacingDemand::default()
        },
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    assert_ne!(contract.fingerprint(), revised.fingerprint());
    // demand 进入 pacing 段身份：站点数变化必须同时改变两处指纹。
    assert_ne!(
        contract.pacing().fingerprint(),
        revised.pacing().fingerprint(),
        "pacing 段身份必须随需求变化"
    );
    assert_ne!(
        contract.pacing().fingerprint(),
        GcPacingRuntimeContract::build(GcPacingDemand::default())
            .expect("契约")
            .fingerprint()
    );
    let _ = MemoryDomainId::RUNTIME_RAW;
}

#[test]
fn allocation_slow_edge_runs_real_cycles_and_records_live_bytes() {
    // 六个 owner 各有 2 MiB arena：只有把分配量推过 min_growth_budget，慢路径才会启动 cycle。
    let mut world = world(6, 64);
    assert_eq!(world.pacing().target_percent(), Some(100));
    assert_eq!(world.pacing().soft_memory_limit(), None);
    let class = RuntimeSizeClassId::from_raw(6);
    let mut cycles = 0_u64;
    for index in 0..4096_u32 {
        if world.allocate(index % 6, class).is_err() {
            break;
        }
        if world.barrier().cycle_epoch() > cycles {
            cycles = world.barrier().cycle_epoch();
            break;
        }
    }
    // 分配 debt 越过增长预算后，真实的自动 cycle 必须已经完成一次。
    assert_eq!(cycles, 1, "allocation debt 必须触发一次真实自动 cycle");
    let plane = world.pacing();
    assert!(
        plane.last_live_bytes() > 0,
        "cycle 必须记录真实 live record 字节"
    );
    assert_eq!(
        plane.allocation_debt(),
        0,
        "cycle 完成后 cycle 内 debt 归零"
    );
    // live record 字节就是账本残差：四类互斥且完备，残差等于 committed 减三类分类。
    let classes = world.committed_classes();
    let classed = world.classed_committed_bytes();
    assert_eq!(
        world.live_record_bytes(),
        classed
            .saturating_sub(classes.pending_return_bytes)
            .saturating_sub(classes.reclaimable_bytes)
            .saturating_sub(classes.owner_cache_bytes)
    );
    assert!(classed > 0, "cycle 后仍有已提交的 arena 字节");
    for owner in 0..6 {
        world.ledger_invariant(owner).expect("账本互斥");
    }
}

#[test]
fn allocation_slow_edge_denies_headroom_when_limit_is_exceeded() {
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    // 先分配一次让 committed 超过随后设置的极小上限。
    world.allocate(0, class).expect("分配成功");
    world.set_memory_limit(Some(1));
    // 下一次分配必须走真实 headroom 链并在用尽 drain 与 forced cycle 后失败。
    let denied = world.allocate(0, class);
    assert!(denied.is_err(), "无法取得 headroom 时分配必须失败");
    assert!(world.pacing().headroom_oom() > 0);
    assert!(world.pacing().forced_cycle_total() > 0);
    // 队列已 drain、credit 已收敛，账本仍保持互斥。
    world.ledger_invariant(0).expect("失败后账本仍互斥");
}

#[test]
fn cycle_keeps_candidates_beyond_the_relocation_budget_for_the_next_cycle() {
    // 两个 owner 的候选总量（4 MiB）超过一次 relocation pause 预算（2 MiB）。
    let limits = BatchLimits {
        items: 1,
        batch_soft_bytes: 1,
    };
    let mut world = RawWorld::new(7, 2, 4096, limits).expect("raw world 可创建");
    let class = RuntimeSizeClassId::from_raw(6);
    let bytes = u64::from(
        world
            .classes()
            .get(class)
            .expect("class 已登记")
            .slot_stride,
    );
    // 先把两个 owner 的 arena 基本填满，再全部归还并发布，构造跨 owner 的空载候选集。
    let mut allocations = Vec::new();
    for owner in 0..2 {
        for _ in 0..512 {
            let Ok(allocation) = world.allocate(owner, class) else {
                break;
            };
            allocations.push((owner, allocation.slot));
        }
    }
    assert!(allocations.len() > 600, "两个 arena 必须被真实填满");
    for (owner, slot) in allocations {
        world.queue_return(owner, slot, bytes).expect("归还成功");
        let message = world
            .message(
                world.token(owner),
                ReturnKind::RawSlot,
                slot,
                u32::try_from(bytes).expect("stride 适配 u32"),
            )
            .expect("消息可构造");
        world
            .publish_message(owner, &message, shard(0), None)
            .expect("发布成功");
    }
    // 走完 grace：第一个 cycle 只能发布落在预算内的前缀，其余整块延后。
    let mut deferred = 0_u32;
    let mut trimmed = 0_u32;
    for _ in 0..8 {
        let report = world.run_gc_cycle(false).expect("drain 成功");
        deferred = deferred.max(report.deferred_extents);
        trimmed += report.trimmed_extents;
    }
    let (admits, defers) = world.pacing().evacuation_counts();
    assert!(admits > 0, "落在预算内的候选必须被发布");
    assert!(defers > 0, "超出预算的候选必须整块延后");
    assert!(deferred > 0, "延后数必须进入 drain 报告");
    assert!(
        trimmed > 0,
        "延后不等于不进展：预算内前缀仍必须真实 decommit"
    );
    world.ledger_invariant(0).expect("账本互斥");
    world.ledger_invariant(1).expect("账本互斥");
}

#[test]
fn assist_flushes_real_card_keys_and_repays_matching_cost() {
    let mut world = world(1, 64);
    world
        .register_managed_arena(0, SlabDescriptorId::from_raw(3), 1)
        .expect("登记 arena");
    // 造出一张真实未 flush 的 dirty card。
    world
        .perform_barrier(0, card_site(3, 1, 512, 0))
        .expect("写屏障成功");
    let buffered = world
        .barrier()
        .processor(0)
        .expect("processor 账本")
        .buffer()
        .len();
    assert!(buffered > 0, "屏障必须留下未 flush 的 card 键");
    // 把 allocation debt 推到 assist 阈值之上。
    world.observe_allocation(MIN_GROWTH_BUDGET + ASSIST_THRESHOLD);
    let outcome = world.assist_on_slow_edge(0).expect("assist 成功");
    assert_eq!(outcome, AssistOutcome::WithinQuantum);
    // 键真的离开了 processor 账本，且偿还量与交出的键数同源。
    assert_eq!(
        world
            .barrier()
            .processor(0)
            .expect("processor 账本")
            .buffer()
            .len(),
        0,
        "assist 必须真实交出 card 键"
    );
    assert_eq!(
        world.pacing().assist_cost(),
        u64::from(CARD_GRANULARITY_BYTES)
    );
    assert_eq!(world.pacing().assist_by_outcome(), [0, 1, 0, 0]);
    assert_eq!(
        world.pacing().mark_debt(),
        ASSIST_THRESHOLD - u64::from(CARD_GRANULARITY_BYTES),
        "allocation debt 按 mark_cost_per_byte 折算后扣减"
    );
}

#[test]
fn assist_without_real_work_records_nothing() {
    let mut world = world(1, 64);
    world.observe_allocation(MIN_GROWTH_BUDGET + ASSIST_THRESHOLD);
    let debt = world.pacing().mark_debt();
    let outcome = world.assist_on_slow_edge(0).expect("assist 成功");
    assert_eq!(outcome, AssistOutcome::NoWork);
    assert_eq!(world.pacing().assist_cost(), 0, "没有交接就没成本");
    assert_eq!(world.pacing().mark_debt(), debt, "空 assist 不得虚构进度");
}

#[test]
fn cycle_stays_incomplete_without_credit_convergence_instead_of_failing() {
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    let bytes = u64::from(
        world
            .classes()
            .get(class)
            .expect("class 已登记")
            .slot_stride,
    );
    // 只登记归还、不发布消息：pending 字节留在账本上，credit 无法收敛。
    world
        .queue_return(0, allocation.slot, bytes)
        .expect("归还成功");
    let report = world.run_gc_cycle(false).expect("未收敛不是错误");
    assert!(!report.cycle_completed, "credit 未收敛不得宣布 cycle 完成");
    assert!(!report.credits_converged);
    assert_eq!(report.edge_deltas, 0);
    // barrier epoch 可以先行（remark 已完整），但 credit 平面不追平：两个水位必须单调一致。
    assert_eq!(
        world.pacing().credits().cycle_epoch(),
        0,
        "credit 未收敛时不得推进 credit epoch"
    );
    assert_eq!(world.credit_snapshot().pending_return_bytes, bytes);
}

#[test]
fn drain_flushes_the_world_owned_return_staging() {
    // 大 batch 上限：消息留在 world 自己的 staging 里，drain 必须真实冲刷它。
    let limits = BatchLimits {
        items: 1 << 20,
        batch_soft_bytes: 1 << 20,
    };
    let mut world = RawWorld::new(7, 1, 64, limits).expect("raw world 可创建");
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    let bytes = u64::from(
        world
            .classes()
            .get(class)
            .expect("class 已登记")
            .slot_stride,
    );
    world
        .queue_return(0, allocation.slot, bytes)
        .expect("归还成功");
    let message = world
        .message(
            world.token(0),
            ReturnKind::RawSlot,
            allocation.slot,
            u32::try_from(bytes).expect("stride 适配 u32"),
        )
        .expect("消息可构造");
    world
        .publish_message(0, &message, shard(0), None)
        .expect("发布成功");
    assert!(
        world.credit_snapshot().staging_bytes > 0,
        "未冲刷字节必须可观测"
    );
    let report = world.run_gc_cycle(false).expect("drain 成功");
    assert_eq!(
        world.credit_snapshot().staging_bytes,
        0,
        "drain 必须冲刷真实 staging"
    );
    assert!(report.consumed_messages >= 1, "冲刷后的消息必须被消费");
}

#[test]
fn steady_state_allocation_reads_no_committed_snapshot() {
    let mut world = world(1, 64);
    world.set_memory_limit(Some(1 << 40));
    let class = RuntimeSizeClassId::from_raw(0);
    let snapshots = world.pacing().pressure_snapshots();
    for _ in 0..64 {
        world.allocate(0, class).expect("分配成功");
    }
    // 软上限远未触及、poll 间隔未到：快路径不得读任何全局 committed 快照。
    assert_eq!(world.pacing().pressure_snapshots(), snapshots);
    assert_eq!(world.pacing().state(), PressureState::Steady);
}

#[test]
fn cycle_does_not_double_count_flush_statistics() {
    let mut world = world(1, 64);
    let processors = u64::try_from(world.barrier().processor_count()).expect("处理器数适配 u64");
    let before = world.barrier_stats().by_reason[BarrierFlushReason::MemoryPressure.index()];
    world.run_gc_cycle(false).expect("cycle 成功");
    let after = world.barrier_stats().by_reason[BarrierFlushReason::MemoryPressure.index()];
    assert_eq!(
        after - before,
        processors,
        "memory-pressure 每 owner 只冲刷一遍 barrier 账本"
    );
}

#[test]
fn consecutive_cycles_advance_the_epoch_with_per_cycle_work() {
    let mut world = world(1, 64);
    let before = world.barrier().cycle_epoch();
    let first = world.run_gc_cycle(false).expect("第一次 cycle");
    let second = world.run_gc_cycle(false).expect("第二次 cycle");
    assert!(first.cycle_completed, "第一次 cycle 必须完成");
    assert!(second.cycle_completed, "累计工作量增长不得挡住第二次 cycle");
    assert_eq!(first.remark, RemarkOutcome::Complete);
    assert_eq!(second.remark, RemarkOutcome::Complete);
    assert_eq!(world.barrier().cycle_epoch(), before + 2);
    world.ledger_invariant(0).expect("账本仍互斥");
}
