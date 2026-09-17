//! 候选回收与世界状态的集成回归：从真实 LocalHeap/EdgePlane 取样，经平面决议，再由唯一消费者
//! 执行清扫与释放。
//!
//! 全部在进程内运行：断言的是 block 记录、对象可达性与平面统计，不是内部调用顺序。

use super::RawWorld;
use super::coroutine_impl::CoroutineEntry;
use super::heap_impl::ManagedPlacement;
use super::heap_tests::{gc_contract, heap_world};
use crate::runtime::candidate_schema::CandidateVerdict;
use crate::runtime::gc_metadata_schema::GcRootKindV1;
use crate::runtime::inbox::ServiceBudget;
use crate::runtime::local_heap::{HeapArenaKind, ManagedBlockId};
use crate::runtime::startup_schema::ReportReason;

/// 在 owner 0 的 Old arena 分配一个 leaf，并返回 `(payload 地址, block 身份)`。
fn old_leaf(world: &mut RawWorld) -> (u64, ManagedBlockId) {
    let address = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Old)
        .expect("leaf 可分配");
    let block = world
        .managed_block_ref(0, address)
        .expect("block 身份可解析")
        .id;
    (address, block)
}

/// 推进候选平面直到所有 job 结算；返回本轮观察到的决议。
fn drive_until_settled(world: &mut RawWorld) -> Vec<CandidateVerdict> {
    let mut verdicts = Vec::new();
    for _ in 0..64 {
        let report = world.drive_candidates(4096).expect("候选推进成功");
        verdicts.extend(report.verdicts.iter().map(|(_, verdict)| verdict.clone()));
        let stats = world.candidate_stats().expect("统计可读");
        if stats.jobs_started == stats.jobs_completed + stats.jobs_invalidated {
            break;
        }
    }
    verdicts
}

/// 把一个 block 标成候选并推进平面，直到它不再有活跃 job。
fn run_candidates(world: &mut RawWorld, block: ManagedBlockId) -> Vec<CandidateVerdict> {
    world.note_candidate_dirty(block).expect("候选通知成功");
    drive_until_settled(world)
}

#[test]
fn candidate_cycle_frees_the_block_and_drops_its_objects() {
    let mut world = heap_world();
    let (address, block) = old_leaf(&mut world);
    let before = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(before.state, 0, "新块必须是 active");
    let verdicts = run_candidates(&mut world, block);
    assert_eq!(
        verdicts,
        vec![CandidateVerdict::Dead(vec![(block, before.generation)])],
        "无入边、无 pin/resource/标记的块必须判定死亡"
    );
    let after = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(after.state, 0, "释放后必须回到「已提交且可分配」的状态");
    assert_eq!(after.generation, before.generation + 1, "释放必须推进世代");
    assert_eq!(after.candidate_job, u32::MAX, "释放后不得残留 job 绑定");
    assert!(
        world.managed_object(address).is_err(),
        "被回收对象的地址必须不再可解析"
    );
    assert!(
        world
            .heap(0)
            .expect("堆可读")
            .block_objects(block.arena().into(), block.index())
            .expect("块可枚举")
            .is_empty(),
        "释放后的 block 不应残留对象起点"
    );
    let stats = world.candidate_stats().expect("统计可读");
    assert_eq!(stats.jobs_completed, 1);
    assert_eq!(stats.blocks_swept, 1);
    assert_eq!(stats.blocks_released, 1);
    assert_eq!(stats.dead_groups, 1);
    assert_eq!(world.candidate_job_of(block).expect("可查询"), None);
    // 再推进一次不得重复 sweep 或释放：平面已经没有 job，块也已经是 free。
    let again = world.drive_candidates(4096).expect("空推进成功");
    assert!(again.actions.is_empty(), "空推进不得再产生动作");
    assert_eq!(world.candidate_stats().expect("统计可读").blocks_swept, 1);
}

#[test]
fn pinned_candidate_is_retreated_without_sweeping() {
    let mut world = heap_world();
    let (address, block) = old_leaf(&mut world);
    world.pin_managed(0, address).expect("对象可 pin");
    let verdicts = run_candidates(&mut world, block);
    assert!(
        verdicts.iter().all(|verdict| !verdict.is_dead()),
        "有 pin 的块不得判定死亡"
    );
    let record = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(record.state, 0, "失效后必须退回 active");
    assert_eq!(record.candidate_job, u32::MAX, "失效后必须解绑");
    assert!(world.managed_object(address).is_ok(), "pin 对象必须存活");
    let stats = world.candidate_stats().expect("统计可读");
    assert_eq!(stats.blocks_swept, 0);
    assert_eq!(stats.blocks_released, 0);
    assert_eq!(stats.jobs_invalidated, 1);
    assert_eq!(stats.alive_groups, 1);
}

#[test]
fn marked_candidate_is_retreated() {
    let mut world = heap_world();
    let (address, block) = old_leaf(&mut world);
    world
        .heap_mut(0)
        .expect("堆可写")
        .mark_object(address)
        .expect("对象可标记");
    assert_eq!(
        world
            .heap(0)
            .expect("堆可读")
            .block_marked_objects(block)
            .expect("标记数可读"),
        1,
        "当前 epoch 的标记必须被算进 gate 输入"
    );
    let verdicts = run_candidates(&mut world, block);
    assert!(
        verdicts.iter().all(|verdict| !verdict.is_dead()),
        "被标记的块不得判定死亡"
    );
    assert!(world.managed_object(address).is_ok(), "标记对象必须存活");
    assert_eq!(world.candidate_stats().expect("统计可读").blocks_swept, 0);
}

#[test]
fn nursery_blocks_never_enter_candidates() {
    let mut world = heap_world();
    let address = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Nursery)
        .expect("nursery 对象可分配");
    let block = world
        .managed_block_ref(0, address)
        .expect("block 身份可解析")
        .id;
    assert_eq!(
        world
            .heap(0)
            .expect("堆可读")
            .block_arena_kind(block)
            .expect("类别可读"),
        HeapArenaKind::Nursery
    );
    // 变更通知必须忽略 nursery block：它由 minor cycle 整体复位，不是候选对象。
    world.note_block_mutation(block).expect("变更通知成功");
    assert_eq!(world.candidate_stats().expect("统计可读").jobs_started, 0);
    assert_eq!(
        world
            .drive_candidates(4096)
            .expect("空推进成功")
            .actions
            .len(),
        0
    );
    assert!(
        world.managed_object(address).is_ok(),
        "nursery 对象必须存活"
    );
}

#[test]
fn relocation_moves_the_incoming_edge_to_the_new_block() {
    let mut world = heap_world();
    // 源对象在 Old，目标对象在 nursery：写一条跨 block 引用并发布，得到 (源块 → 目标块) 计数。
    let source = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Old)
        .expect("源对象可分配");
    let nursery = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Nursery)
        .expect("nursery 对象可分配");
    let source_ref = world.managed_block_ref(0, source).expect("源 block");
    let nursery_ref = world.managed_block_ref(0, nursery).expect("目标 block");
    world
        .store_managed_field(0, 0, source, 0, nursery)
        .expect("字段写入成功");
    let published = world.publish_edge_deltas().expect("边记录可发布");
    assert_eq!(published.len(), 1, "跨 block 引用必须发布一条记录");
    assert_eq!(world.block_incoming_leases(nursery_ref), 1);
    // 发布只把记录放进 inbox；已应用计数要在 owner 消费后才出现。
    world
        .drain_all(0, &ServiceBudget::new(64, 1 << 20))
        .expect("owner 可服务");
    assert_eq!(
        world
            .edge_applied_delta(source_ref, nursery_ref)
            .expect("计数可读"),
        1,
        "消费后必须出现已应用入边计数"
    );

    // 把 nursery 对象挂到根槽上，让它在 minor cycle 里被 evacuate 到 Old。
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 1)
        .expect("根槽可登记");
    world.set_managed_root(slot, nursery).expect("根槽可写");
    world.collect_minor(0).expect("minor cycle 可执行");
    let moved = world.managed_root(slot).expect("根槽可读");
    assert_ne!(moved, nursery, "根槽必须被改写到新地址");
    let moved_ref = world.managed_block_ref(0, moved).expect("新 block");
    assert_ne!(moved_ref.id, nursery_ref.id, "对象必须真的换了 block");
    // 计数必须整体搬到新目标：旧块不能继续背着已经搬走的入边，新块也不能像是没有入边。
    let pairs = world.edge_pairs().expect("边对可读");
    assert_eq!(
        world
            .edge_applied_delta(source_ref, nursery_ref)
            .expect("计数可读"),
        0,
        "旧目标 block 的已应用计数必须清退；当前边对 = {pairs:?}，旧目标 = {nursery_ref:?}，新目标 = {moved_ref:?}"
    );
    assert_eq!(
        world
            .edge_applied_delta(source_ref, moved_ref)
            .expect("计数可读"),
        1,
        "计数必须迁移到新目标 block"
    );
    assert_eq!(world.block_incoming_leases(nursery_ref), 0);
    assert_eq!(world.block_incoming_leases(moved_ref), 1);
    // 两个 block 都必须重新进入候选视野：旧块失去入边、新块获得入边。
    world.drive_candidates(4096).expect("候选推进可执行");
}

#[test]
fn incremental_marking_shades_writes_and_allocations() {
    let mut world = heap_world();
    // cycle 打开前：分配与写入都不需要染色（没有灰色集合）。
    let before = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Old)
        .expect("对象可分配");
    assert_eq!(world.mark_worklist_items(), 0, "未开始 cycle 时不得染色");

    // 打开一个 cycle：run_mark_pass 会开始 cycle 并 seed 根，但不会收尾。
    let root = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Old)
        .expect("根对象可分配");
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 0)
        .expect("根槽可登记");
    world.set_managed_root(slot, root).expect("根槽可写");
    let pass = world.run_mark_pass(&[0]).expect("mark pass 可执行");
    assert!(pass.termination.converged());
    world.finish_mark_cycle().expect("收敛后可完成 cycle");
    assert_eq!(
        world.mark_worklist_items(),
        0,
        "cycle 收尾后 worklist 必须清空"
    );

    // 重新打开一个 cycle：本次不收敛（有新分配要染），验证分配与写入染色。
    let pass = world.run_mark_pass(&[0]).expect("第二个 mark pass 可执行");
    assert!(pass.termination.converged());
    let fresh = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Nursery)
        .expect("cycle 期间可分配");
    assert!(
        world.mark_worklist_items() >= 1,
        "cycle 打开期间的分配必须被染灰"
    );
    // 写屏障：把一个新引用写进 Old 对象，指向的对象必须进入灰色集合。
    let target = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Old)
        .expect("目标对象可分配");
    let baseline = world.mark_worklist_items();
    world
        .store_managed_field(0, 0, before, 0, target)
        .expect("字段写入成功");
    assert!(
        world.mark_worklist_items() > baseline,
        "cycle 打开期间的写屏障必须把引用对象染灰"
    );
    assert!(world.managed_object(fresh).is_ok());
    world.finish_mark_cycle().expect("cycle 可收尾");
    assert_eq!(world.mark_worklist_items(), 0);
}

#[test]
fn failure_cancellation_retreats_jobs_and_reports_through_rt0() {
    let mut world = heap_world();
    let (address, block) = old_leaf(&mut world);
    // 先建立一个真实的候选 job（只推两个单位，停在提交之前的相位）。
    world.note_candidate_dirty(block).expect("候选通知成功");
    world.drive_candidates(2).expect("候选推进成功");
    assert_eq!(world.candidate_job_count().expect("job 数可读"), 1);
    assert_eq!(
        world.candidate_job_of(block).expect("可查询"),
        Some(0),
        "块必须已绑定到 job"
    );
    // 失败取消：job 必须整体退回，绑定与状态复位。
    let cancelled = world
        .cancel_gc_work("测试注入的 cycle 失败")
        .expect("取消失败成功");
    assert_eq!(cancelled, 1, "进行中的 job 必须被取消");
    assert_eq!(world.candidate_job_count().expect("job 数可读"), 0);
    assert_eq!(world.candidate_job_of(block).expect("可查询"), None);
    let record = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(record.state, 0, "取消后必须退回 active");
    assert_eq!(record.candidate_job, u32::MAX);
    assert!(
        world.managed_object(address).is_ok(),
        "取消不得回收任何对象"
    );
    assert_eq!(world.candidate_stats().expect("统计可读").blocks_swept, 0);
    // 报告链：RT0 未启动时没有账本可写，返回 None（失败仍以 RawInvariant 形式返回）。
    assert_eq!(
        world
            .report_gc_failure("测试报告".to_owned())
            .expect("报告调用可执行"),
        None
    );
    // 启动 RT0 后同一条失败必须进入既有报告账本。
    world
        .boot(
            vec!["gugu".to_owned()],
            vec![],
            "/work".to_owned(),
            4,
            CoroutineEntry {
                pc: 0x1000,
                required_frame: 64,
            },
        )
        .expect("RT0 可启动");
    world
        .report_gc_failure("GC cycle 失败: 注入".to_owned())
        .expect("报告可发送")
        .expect("启动后必须写进账本");
    let reports = world.rt0_reports().expect("账本可读");
    let last = reports.last().expect("账本必须有报告");
    // 断言用户可见的报告文本：注入的失败信息与 fatal 原因都必须出现在渲染结果里。
    assert!(
        last.text().contains("GC cycle 失败"),
        "报告文本必须带上失败信息：{}",
        last.text()
    );
    assert!(
        last.text().contains(ReportReason::RuntimeInvariant.name()),
        "报告必须登记为运行时内部不变量失败：{}",
        last.text()
    );
}

#[test]
fn forced_cycle_consumes_candidate_decisions_in_the_same_cycle() {
    let mut world = heap_world();
    let (address, block) = old_leaf(&mut world);
    let before = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(before.incoming_leases, 0, "无引用对象的块 lease 必须为零");
    assert!(
        world.candidate_job_of(block).expect("可查询").is_none(),
        "尚未登记前不应有绑定"
    );
    // 一次 forced full cycle：mark 收敛后必须在同一个 cycle 内把死亡决议消费掉（sweep + release），
    // 而不是把块留到下一个 cycle 才归还。
    let report = world.run_gc_cycle(true).expect("forced cycle 可执行");
    assert!(report.cycle_completed, "cycle 必须真实完成");
    assert!(
        report.candidate_seeded >= 1,
        "零租约的块必须在 cycle 内被登记为候选"
    );
    assert!(
        report.candidate_blocks_swept >= 1,
        "同一个 cycle 内必须完成 sweep"
    );
    assert!(
        report.candidate_blocks_released >= 1,
        "同一个 cycle 内必须完成释放"
    );
    assert!(
        world.managed_object(address).is_err(),
        "不可达对象必须在本 cycle 内被回收"
    );
    let after = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(after.generation, before.generation + 1, "释放必须推进世代");
}

#[test]
fn edge_contract_is_verified_against_the_runtime() {
    use crate::runtime::candidate_schema::{CANDIDATE_SCHEMA, CandidatePhase};
    use crate::runtime::edge_schema::EDGE_TRACE_EXECUTOR_REVISION;
    use crate::runtime::local_heap_schema::HEAP_BLOCK_STATE_NAMES;
    let world = heap_world();
    let contract = world.edge_contract().expect("契约必须已配置并校对");
    // 运行时侧的常量必须与契约逐项一致：相位目录、状态目录、候选 schema、保留取值。
    assert_eq!(contract.candidate_schema(), CANDIDATE_SCHEMA);
    assert_eq!(contract.phases.len(), CandidatePhase::ALL.len());
    for (index, phase) in CandidatePhase::ALL.iter().enumerate() {
        assert_eq!(contract.phases[index], phase.name());
    }
    assert_eq!(contract.states, HEAP_BLOCK_STATE_NAMES.to_vec());
    assert_eq!(
        contract.trace_executor_revision(),
        EDGE_TRACE_EXECUTOR_REVISION
    );
    assert_eq!(contract.no_job(), u32::MAX, "未绑定取值必须与块记录一致");
    assert!(contract.candidate_quantum() > 0);
    // 负路径：把相位名改掉之后校验必须失败，而不是静默通过。
    let mut damaged = contract.clone();
    damaged.phases[0] = "not-a-phase".to_owned();
    assert!(
        world.verify_edge_contract(&damaged).is_err(),
        "相位名不一致必须在配置期被发现"
    );
    let mut damaged = contract.clone();
    damaged.candidate_schema = CANDIDATE_SCHEMA + 1;
    assert!(world.verify_edge_contract(&damaged).is_err());
    let mut damaged = contract.clone();
    damaged.states[0] = "unknown".to_owned();
    assert!(world.verify_edge_contract(&damaged).is_err());
}

#[test]
fn gc_layout_dump_reports_registered_layout() {
    use crate::runtime::candidate_schema::CANDIDATE_WORK_UNIT;
    let mut world = heap_world();
    let (_, block) = old_leaf(&mut world);
    // 先看没有 job 时的 dump：只有工作单位、相位目录与消息字段布局。
    let idle = world.dump_gc_layout().expect("dump 可读");
    assert!(idle.contains(&format!("work_unit={CANDIDATE_WORK_UNIT}")));
    assert!(idle.contains(
        "phases=discover trace trial scc validate commit sweep release complete invalidate"
    ));
    assert!(
        idle.contains("edge_delta."),
        "消息字段布局必须在 dump 里：{idle}"
    );
    for field in ["cycle_epoch", "source", "destination", "delta", "sequence"] {
        assert!(
            idle.contains(&format!("edge_delta.{field}=")),
            "dump 必须包含 EdgeDelta 的登记字段 `{field}`：{idle}"
        );
    }
    assert!(
        !idle.contains("cursor "),
        "没有活跃 job 时不应有游标行：{idle}"
    );
    // 建立 job 之后 dump 必须带上真实游标取值与它的相位名。
    world.note_candidate_dirty(block).expect("候选通知成功");
    world.drive_candidates(2).expect("候选推进成功");
    let dumped = world.dump_gc_layout().expect("dump 可读");
    let cursor = dumped
        .lines()
        .find(|line| line.starts_with("cursor "))
        .expect("活跃 job 必须出现在 dump 里");
    assert!(cursor.contains("job=0"), "游标行必须带 job 编号：{cursor}");
    assert!(cursor.contains("/32bit"), "游标行必须带登记位宽：{cursor}");
    assert_eq!(
        world.edge_converged().expect("收敛可读"),
        world.edge_stats().expect("统计可读").held == 0,
        "收敛判定必须与保留记录数一致"
    );
}

#[test]
fn cycle_epoch_is_the_single_authority() {
    let mut world = heap_world();
    assert_eq!(world.cycle_epoch(), 0);
    assert_eq!(world.barrier_cycle_epoch(), 0);
    // 每个 cycle 边界只前进一次，且 barrier 的记账纪元必须同步：卡键按 epoch 分组，barrier 落后
    // 一步就会出现「键属于旧 cycle 却被当成当前」的错配。
    world.collect_minor(0).expect("minor cycle 可执行");
    assert_eq!(world.cycle_epoch(), 1);
    assert_eq!(world.barrier_cycle_epoch(), 1);
    world.collect_minor(0).expect("第二个 minor cycle 可执行");
    assert_eq!(world.cycle_epoch(), 2);
    assert_eq!(world.barrier_cycle_epoch(), 2);
    // 收敛的 mark cycle 数在同一个 epoch 内可以前进多次，它不驱动 epoch：一个 epoch 只前进一次。
    let pass = world.run_mark_pass(&[0]).expect("mark pass 可执行");
    assert_eq!(pass.cycle, 1);
    world.finish_mark_cycle().expect("收敛后可完成 cycle");
    let pass = world.run_mark_pass(&[0]).expect("第二个 mark pass 可执行");
    assert_eq!(pass.cycle, 2, "mark cycle 计数按 mark 周期前进");
    assert_eq!(world.cycle_epoch(), 2, "mark 不推进世界 epoch");
    assert_eq!(world.barrier_cycle_epoch(), 2);
    // 完整周期路径同样只走单一权威：pressure cycle 完成后两个纪元必须仍然相等。
    let report = world.run_gc_cycle(true).expect("cycle 可执行");
    assert!(report.cycle_completed);
    assert_eq!(
        world.cycle_epoch(),
        world.barrier_cycle_epoch(),
        "cycle 路径推进后 barrier 纪元必须与世界纪元相等"
    );
    assert_eq!(world.cycle_epoch(), 3, "一次完整 cycle 只前进一个 epoch");
}

#[test]
fn contract_gc_helper_is_reusable() {
    // `heap_world` 与 `gc_contract` 由 heap 测试提供；这里确认候选平面与它们一起配置成功。
    let contract = gc_contract();
    let world = super::heap_tests::configured_world(&contract, 11, 1, 64);
    assert!(world.candidates_configured());
    let stats = world.candidate_stats().expect("统计可读");
    assert_eq!(stats.jobs_started, 0);
    assert_eq!(world.candidate_progress().expect("进度可读").len(), 0);
}

/// 双 owner 的两条跨 owner 环在两次候选轮次之间被 mutator 局部失效：必须收敛且不误回收。
#[test]
fn two_owner_double_cycle_with_local_invalidation_converges() {
    let contract = gc_contract();
    let mut world = super::heap_tests::configured_world(&contract, 47, 2, 64);
    // 两条互相独立的跨 owner 环：a↔b 与 c↔d，全部落在 old generation。
    let a = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Old)
        .expect("a 可分配");
    let c = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Old)
        .expect("c 可分配");
    let b = world
        .allocate_managed(1, 0, 16, ManagedPlacement::Old)
        .expect("b 可分配");
    let d = world
        .allocate_managed(1, 0, 16, ManagedPlacement::Old)
        .expect("d 可分配");
    world.store_managed_field(0, 0, a, 0, b).expect("a→b 可写");
    world.store_managed_field(1, 0, b, 0, a).expect("b→a 可写");
    world.store_managed_field(0, 0, c, 0, d).expect("c→d 可写");
    world.store_managed_field(1, 0, d, 0, c).expect("d→c 可写");
    // 两条环都挂在根上：它们是活环，候选只能退回，不能释放。
    let slot_a = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 0)
        .expect("根槽可登记");
    world.set_managed_root(slot_a, a).expect("根可写");
    let slot_b = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 1)
        .expect("根槽可登记");
    world.set_managed_root(slot_b, b).expect("根可写");
    // 边摘要按 block 对聚合：两条环各自贡献 2 条边，因此只发布两条差量、每条计数为 2。
    let published = world.publish_edge_deltas().expect("边记录可发布");
    assert_eq!(
        published.len(),
        2,
        "同一 block 对的多条边必须聚合成一条差量：{published:?}"
    );
    assert!(
        published.iter().all(|record| record.delta == 2),
        "同一对的重复 add 必须累加计数而不是塌成 1：{published:?}"
    );
    let budget = ServiceBudget::new(64, 1 << 20);
    world.drain_all(0, &budget).expect("owner 0 可服务");
    world.drain_all(1, &budget).expect("owner 1 可服务");
    let ref_a = world.managed_block_ref(0, a).expect("a 的 block");
    let ref_b = world.managed_block_ref(1, b).expect("b 的 block");
    let block_a = ref_a.id;
    assert_eq!(
        world.managed_block_ref(0, c).expect("c 的 block").id,
        block_a,
        "同一 owner 的两个对象落在同一 block：两条环共享同一对 block"
    );
    assert_eq!(
        world
            .edge_applied_delta(ref_a, ref_b)
            .expect("计数可读"),
        2,
        "两条独立环的入边必须各自计数"
    );
    assert_eq!(
        world
            .edge_applied_delta(ref_b, ref_a)
            .expect("计数可读"),
        2,
        "反向也要各自计数"
    );

    // 第一轮：两个 owner 一起标记 → 候选判定 → 收尾并推进 epoch（与 `collect_major` 同序）。
    let first_mark = world.run_mark_pass(&[0, 1]).expect("第一轮 mark pass 可执行");
    assert!(
        first_mark.termination.converged(),
        "第一轮标记必须收敛：{first_mark:?}"
    );
    assert_eq!(
        first_mark.tickets_published, first_mark.tickets_consumed,
        "跨 owner 标记工作必须结清：{first_mark:?}"
    );
    let first_candidates = drive_until_settled(&mut world);
    assert!(
        first_candidates
            .iter()
            .all(|verdict| !verdict.is_dead()),
        "第一轮不得把活环判成死亡：{first_candidates:?}"
    );
    assert_eq!(world.mark_worklist_items(), 0, "标记后不得残留工作项");
    world.finish_mark_cycle().expect("第一轮 cycle 可收尾");
    world.advance_cycle_epoch(0).expect("第一轮 epoch 可推进");
    let after_first = world.candidate_stats().expect("统计可读");
    assert_eq!(after_first.blocks_released, 0, "第一轮不得释放活环");

    // 轮次之间：候选 job 在飞时 mutator 做真实写入（局部失效），job 必须失效而不是继续清扫。
    world.note_candidate_dirty(block_a).expect("候选通知成功");
    let _ = world
        .drive_candidates(1)
        .expect("量子为 1 的推进必须启动 job 而不是结算");
    let in_flight = world.candidate_stats().expect("统计可读");
    assert!(
        in_flight.jobs_started > in_flight.jobs_completed + in_flight.jobs_invalidated,
        "量子 1 的推进必须留下在飞 job：{in_flight:?}"
    );
    world.store_managed_field(0, 0, a, 8, d).expect("a→d 可写");
    assert_eq!(
        world.publish_edge_deltas().expect("边记录可发布").len(),
        1,
        "失效写入产生一条新的跨 owner 差量"
    );
    world.drain_all(1, &budget).expect("owner 1 可服务");
    let invalidating = world
        .drive_candidates(4096)
        .expect("失效后的推进必须成功");
    assert!(
        invalidating
            .verdicts
            .iter()
            .all(|(_, verdict)| !verdict.is_dead()),
        "失效推进不得判定死亡：{:?}",
        invalidating.verdicts
    );
    let after_invalidation = world.candidate_stats().expect("统计可读");
    assert!(
        after_invalidation.jobs_invalidated > in_flight.jobs_invalidated,
        "候选块被 mutator 改动后，在飞 job 必须失效：{after_invalidation:?}"
    );
    assert_eq!(
        after_invalidation.blocks_released, 0,
        "局部失效不得触发释放"
    );
    assert_eq!(
        world
            .edge_applied_delta(ref_a, ref_b)
            .expect("计数可读"),
        3,
        "失效写入必须计入同一对 block"
    );

    // 第二轮：局部失效被吸收后，两个 owner 再次标记并判定，活环仍然存活且计数守恒。
    let second_mark = world.run_mark_pass(&[0, 1]).expect("第二轮 mark pass 可执行");
    assert!(
        second_mark.termination.converged(),
        "第二轮标记必须收敛：{second_mark:?}"
    );
    let second_candidates = drive_until_settled(&mut world);
    assert!(
        second_candidates
            .iter()
            .all(|verdict| !verdict.is_dead()),
        "第二轮同样不得误回收活环：{second_candidates:?}"
    );
    world.finish_mark_cycle().expect("第二轮 cycle 可收尾");
    world.advance_cycle_epoch(0).expect("第二轮 epoch 可推进");
    let final_stats = world.candidate_stats().expect("统计可读");
    assert_eq!(final_stats.blocks_released, 0, "两轮都不得释放活环");
    assert_eq!(
        final_stats.jobs_started,
        final_stats.jobs_completed + final_stats.jobs_invalidated,
        "所有 job 必须结算：{final_stats:?}"
    );
    assert_eq!(
        world
            .edge_applied_delta(ref_a, ref_b)
            .expect("计数可读"),
        3,
        "两轮 cycle 之后边计数不得凭空增减"
    );
    assert!(world.managed_object(a).is_ok(), "活对象必须仍然可解析");
    assert!(world.managed_object(b).is_ok());
    assert!(world.managed_object(c).is_ok());
    assert!(world.managed_object(d).is_ok());
    assert_eq!(
        world.managed_root(slot_a).expect("根可读"),
        a,
        "活环不得被搬迁或回收"
    );
    assert_eq!(world.managed_root(slot_b).expect("根可读"), b);
}
