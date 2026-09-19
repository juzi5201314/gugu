//! typed combining 世界接入的确定性回归。
//!
//! 全部在进程内运行：direct 与 combined 两个世界走同一串 extent 冷路径，结果必须逐项
//! 一致；争用时记录挂在 owner 的 combiner 链上并在平台 wait 字上登记等待；分配、本地
//! 返还、远程归还与 owner drain 这些热路径不进入平面。

use super::super::combining::{CombiningStats, OperationOutcome, OperationTag};
use super::super::combining_schema::{CombiningMode, CombiningPolicyV1};
use super::super::extent::{
    EXTENT_CLASS_LADDER, ExtentId, ExtentOccupancy, ExtentState, TrimBlocked, TrimReport,
};
use super::super::inbox::{ServiceBudget, ShardIndex};
use super::super::message::{BatchLimits, FlushTrigger, ReturnKind};
use super::super::model::GRACE_STEPS;
use super::super::slab::{MemoryDomainId, OwnerToken, RouteKey};
use super::RawWorld;
use super::combining_impl::combining_outcome;
use crate::TargetName;
use crate::runtime::OWNER_INBOX_SHARDS;
use crate::runtime::combining_schema::CombiningRuntimeContract;
use crate::runtime::size_class::RuntimeSizeClassId;

/// 由真实编译得到 combining 契约；世界只按契约模式配置平面。
fn contract(mode: CombiningMode) -> CombiningRuntimeContract {
    let compilation = crate::Compiler::new().compile(
        crate::CompileRequest::single_file("main.gg", "fn main() {\n}\n", TargetName::X86_64Linux)
            .with_combining_policy(CombiningPolicyV1 { mode }),
    );
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    compilation
        .image_plan()
        .expect("镜像计划")
        .combining_runtime()
        .clone()
}

/// 构建一个已配置 combining 的世界。
fn world(mode: CombiningMode, owners: u32, seed: u64) -> RawWorld {
    let contract = contract(mode);
    let mut world = RawWorld::new(seed, owners, 64, BatchLimits::default()).expect("world 可创建");
    world
        .configure_combining(&contract)
        .expect("combining 平面可配置");
    world
}

/// 取一个 RUNTIME_RAW extent 并提交它的物理页；extent 上没有任何 slab descriptor。
fn take_extent(world: &mut RawWorld, owner: u32, class: u32) -> ExtentId {
    world
        .take_extent(owner, class, MemoryDomainId::RUNTIME_RAW)
        .expect("extent 可发放")
}

/// 服务预算：pressure trim 不设上限。
fn budget() -> ServiceBudget {
    ServiceBudget::pressure(u32::MAX, u64::MAX)
}

/// 排空一个 owner 的全部 shard。
fn service_all(world: &mut RawWorld, owner: u32) {
    for shard in 0..OWNER_INBOX_SHARDS {
        let shard = ShardIndex::from_raw(shard).expect("shard 合法");
        world
            .service(owner, shard, &budget())
            .expect("service 可运行");
    }
}

/// 推进 grace 并执行一次 pressure trim。
///
/// 候选集、门禁与动作都走 pressure trim 的同一条路径：空载 extent 需要 `GRACE_STEPS`
/// 次 epoch 前进才允许撤销物理页，因此这里按 epoch 逐步推进到收敛。
fn pressure_trim(world: &mut RawWorld) -> TrimReport {
    let mut report = {
        let candidates = world.trim_candidates();
        world
            .trim_extents(&candidates)
            .expect("pressure trim 可执行")
    };
    for _ in 0..GRACE_STEPS {
        if report.blocked.is_empty() {
            break;
        }
        world.advance_epoch_for_test();
        let candidates = world.trim_candidates();
        report = world
            .trim_extents(&candidates)
            .expect("pressure trim 可执行");
    }
    report
}

/// 把某个 extent 的门禁推到通过。
fn drive_grace(world: &mut RawWorld, extent: ExtentId) -> Result<(), TrimBlocked> {
    let occupancy = ExtentOccupancy::default();
    for _ in 0..GRACE_STEPS {
        let gated = world
            .gate_extent_trim(extent, occupancy)
            .expect("门禁可执行");
        if gated.is_ok() {
            return Ok(());
        }
        world.advance_epoch_for_test();
    }
    world
        .gate_extent_trim(extent, occupancy)
        .expect("门禁可执行")
}

/// 九项统计的规范顺序；差值与不变量都按它比较。
fn stats_array(stats: CombiningStats) -> [u64; 9] {
    [
        stats.requests,
        stats.fast_path_claims,
        stats.contended_parkings,
        stats.merged_requests,
        stats.rounds,
        stats.executions,
        stats.cancellations,
        stats.timeouts,
        stats.refills,
    ]
}

/// 该 owner 的 raw arena 下标。
fn raw_arena(world: &RawWorld, owner: u32) -> u32 {
    world
        .extents()
        .spaces_of(owner)
        .iter()
        .copied()
        .find(|index| world.extents().arena_domain(*index) == Some(MemoryDomainId::RUNTIME_RAW))
        .expect("每个 owner 都有 raw arena")
}

/// direct 与 combined 走同一串 trim 序列必须给出逐项一致的结果。
#[test]
fn direct_and_combined_modes_return_identical_trim_results() {
    let classes = [0_u32, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4];
    let mut reports = Vec::new();
    let mut committed = Vec::new();
    let mut stats = Vec::new();
    for mode in [CombiningMode::Direct, CombiningMode::Combined] {
        let mut world = world(mode, 1, 11);
        for class in classes {
            take_extent(&mut world, 0, class);
        }
        let before = world.combining_stats();
        let report = pressure_trim(&mut world);
        stats.push((before, world.combining_stats()));
        reports.push(report);
        committed.push(world.provider_stats().committed_bytes);
    }
    assert_eq!(reports[0].trimmed, classes.len() as u32);
    assert_eq!(reports[0].trimmed, reports[1].trimmed);
    assert_eq!(reports[0].blocked, reports[1].blocked);
    assert_eq!(committed[0], committed[1]);
    // direct 模式不创建任何记录：九项统计在 trim 前后完全不变。
    assert_eq!(stats_array(stats[0].0), stats_array(stats[0].1));
    // combined 模式把同一条冷路径记录进池并合并执行。
    let combined = stats[1];
    assert!(combined.1.requests > combined.0.requests);
    assert!(combined.1.executions > combined.0.executions);
    assert!(combined.1.merged_requests > combined.0.merged_requests);
    assert!(combined.1.rounds > combined.0.rounds);
    assert_eq!(combined.1.timeouts, 0);
    assert_eq!(combined.1.cancellations, 0);
}

/// combined 模式下的批量 trim 会把同类请求合并进同一次执行。
#[test]
fn combined_bulk_trim_merges_same_class_requests() {
    let extents = 8_u32;
    let mut combined_world = world(CombiningMode::Combined, 1, 21);
    let mut direct_world = world(CombiningMode::Direct, 1, 21);
    for _ in 0..extents {
        take_extent(&mut combined_world, 0, 0);
        take_extent(&mut direct_world, 0, 0);
    }
    let before = combined_world.combining_stats();
    let combined_report = pressure_trim(&mut combined_world);
    let direct_report = pressure_trim(&mut direct_world);
    let after = combined_world.combining_stats();
    assert_eq!(combined_report.trimmed, extents);
    assert_eq!(combined_report.blocked, direct_report.blocked);
    assert_eq!(
        combined_world.provider_stats().committed_bytes,
        direct_world.provider_stats().committed_bytes
    );
    // 每个 extent 产生两条冷操作（platform-trim 与 extent-coalesce），两条都进记录池。
    let requests = after.requests - before.requests;
    let executions = after.executions - before.executions;
    let merged = after.merged_requests - before.merged_requests;
    assert_eq!(requests, u64::from(extents) * 2);
    assert_eq!(
        combined_world.combining_tag_requests(OperationTag::PlatformTrim),
        u64::from(extents)
    );
    assert_eq!(
        combined_world.combining_tag_requests(OperationTag::ExtentCoalesce),
        u64::from(extents)
    );
    assert!(executions < requests);
    assert_eq!(executions + merged, requests);
    assert!(merged > 0);
    assert!(after.rounds > before.rounds);
    assert_eq!(combined_world.combining_pending_records(), 0);
    assert_eq!(combined_world.combining_pending_bytes(), 0);
}

/// 分配、本地返还、远程归还与 owner drain 都不进入 combining 平面。
#[test]
fn hot_paths_never_enter_the_combining_plane() {
    let mut world = world(CombiningMode::Combined, 2, 31);
    let before = stats_array(world.combining_stats());
    // 分配 + 本地返还：owner 上下文的唯一分配入口与本地 free structure。
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    world
        .local_return(0, allocation.slot, 64)
        .expect("本地返还成功");
    // 远程归还：owner 1 的 slot 归还给 owner 0，走 return message 与 inbox service。
    let remote = world.allocate(1, class).expect("分配成功");
    world.queue_return(1, remote.slot, 64).expect("归还线性化");
    let target = world.token(0);
    let message = world
        .message(target, ReturnKind::RawSlot, remote.slot, 64)
        .expect("消息可构造");
    let published = world
        .publish_message(
            1,
            &message,
            ShardIndex::from_raw(0).expect("shard 合法"),
            Some(FlushTrigger::OwnerPressure),
        )
        .expect("发布成功");
    assert!(!published.is_empty());
    service_all(&mut world, 0);
    // owner drain：排空 owner 1 并把本 owner 与 domain 的 combiner 队列一并检查。
    world.drain_all(1, &budget()).expect("drain 可运行");
    // pressure trim 的门禁：grace 还没走完，候选因此只被门禁拒绝、不进入平面。
    let blocked = world.reclaim_extents_for_test(0).expect("reclaim 可运行");
    assert!(
        blocked
            .iter()
            .all(|reason| matches!(reason, TrimBlocked::GracePending { .. })),
        "{blocked:?}"
    );
    // 九项统计全部不变，且平面里没有遗留记录。
    assert_eq!(before, stats_array(world.combining_stats()));
    assert_eq!(world.combining_pending_records(), 0);
}

/// extent coalescing 的结果可重复：同 seed 的 direct/combined 与重复的 combined 逐项一致。
#[test]
fn extent_coalescing_results_are_reproducible() {
    let classes = [0_u32, 1, 1, 2, 3, 3, 4, 0];
    let run = |mode: CombiningMode| {
        let mut world = world(mode, 1, 41);
        for class in classes {
            take_extent(&mut world, 0, class);
        }
        let report = pressure_trim(&mut world);
        let arena = raw_arena(&world, 0);
        let free: Vec<u32> = (0..EXTENT_CLASS_LADDER.len())
            .map(|class| {
                world
                    .extents()
                    .free_blocks(arena, u32::try_from(class).expect("class 下标适配 u32"))
            })
            .collect();
        let descriptors: Vec<(u32, ExtentState, u32)> = world
            .extents()
            .descriptors()
            .iter()
            .map(|descriptor| {
                (
                    descriptor.id.raw(),
                    descriptor.state,
                    descriptor.generation.raw(),
                )
            })
            .collect();
        (
            report.trimmed,
            report.blocked,
            world.extents().live_count(),
            free,
            descriptors,
        )
    };
    let direct = run(CombiningMode::Direct);
    let combined = run(CombiningMode::Combined);
    let repeated = run(CombiningMode::Combined);
    assert_eq!(direct.0, classes.len() as u32);
    assert_eq!(direct, combined);
    assert_eq!(combined, repeated);
}

/// topology 重建经平面执行，索引查询与线性扫描等价。
#[test]
fn topology_rebuild_routes_through_the_plane_and_keeps_lookups_equivalent() {
    let mut world = world(CombiningMode::Combined, 2, 51);
    let before = world.combining_tag_requests(OperationTag::TopologyRebuild);
    // 转发：owner 0 的目标改为 owner 1，目录 epoch 前进并带上一次 typed cold operation。
    let target = world.token(1);
    world.begin_forwarding(0, target).expect("转发目标可发布");
    assert!(world.combining_tag_requests(OperationTag::TopologyRebuild) > before);
    assert_eq!(
        world.combining_topology_epoch().raw(),
        world.directory.epoch().raw()
    );
    // 逐 token 比对：索引给出的槽位必须与测试内实现的线性扫描完全一致。
    let mut tokens: Vec<OwnerToken> = Vec::new();
    for owner in 0..world.owner_count() {
        tokens.push(world.token(owner));
        tokens.push(world.resource_token(owner));
    }
    tokens.push(world.domain_owner);
    for token in &tokens {
        let expected = world
            .owners
            .iter()
            .position(|owner| owner.token() == *token)
            .or_else(|| {
                world
                    .resource_owners
                    .iter()
                    .position(|owner| owner.token() == *token)
            });
        match expected {
            Some(index) => assert_eq!(world.owner_slot(token).expect("已登记 token"), index),
            None => assert!(world.owner_slot(token).is_err()),
        }
    }
    // 伪造 route key 的 token 必须被同一错误拒绝，而不是命中同 owner 的槽位。
    let forged = OwnerToken {
        route_key: RouteKey::from_raw(world.token(1).route_key.raw() ^ 1),
        ..world.token(1)
    };
    assert_eq!(
        world.owner_slot(&forged).expect_err("伪造 token").message(),
        "目标 owner 没有登记的 inbox 槽位"
    );
    assert_eq!(
        world
            .combining_slot_for_test(&forged)
            .expect_err("伪造 token")
            .message(),
        "目标 owner 没有登记的 inbox 槽位"
    );
    // domain owner 独占最后一个 combiner 队列。
    assert_eq!(
        world
            .combining_slot_for_test(&world.domain_owner)
            .expect("domain 槽位"),
        world.owner_count() * 2
    );
}

/// 争用请求在平台 wait 字上登记等待，并由下一轮执行后唤醒。
#[test]
fn parked_request_uses_platform_wait_and_wake() {
    let mut world = world(CombiningMode::Combined, 1, 61);
    let extent = take_extent(&mut world, 0, 0);
    let token = world.token(0);
    let slot = world
        .combining_slot_for_test(&token)
        .expect("raw owner 槽位");
    let word = world.combining_words[usize::try_from(slot).expect("槽位适配 usize")];
    // 门禁先走完 grace，使下一条冷操作只剩「争用挂链」这一件事。
    drive_grace(&mut world, extent).expect("grace 走完");
    assert_eq!(world.provider.word_sleepers(word), Some(0));
    world.begin_combining_round_for_test(slot).expect("开轮");
    let before_stats = world.combining_stats();
    let before_decommitted = world.provider_stats().decommitted_total;
    let error = world
        .poll_trim_extent(extent, ExtentOccupancy::default())
        .expect_err("轮次打开时请求必须挂链等待");
    assert_eq!(error.message(), "combiner 未在固定轮次内发布 response");
    let parked = world.combining_stats();
    assert_eq!(
        parked.contended_parkings - before_stats.contended_parkings,
        1
    );
    assert_eq!(parked.fast_path_claims, before_stats.fast_path_claims);
    assert_eq!(parked.executions, before_stats.executions);
    assert_eq!(world.provider.word_sleepers(word), Some(1));
    assert_eq!(world.provider_stats().decommitted_total, before_decommitted);
    // 结束争用轮次后，下一轮认领挂链记录：物理页只被撤销一次，等待者被唤醒。
    world
        .end_combining_round_for_test(slot)
        .expect("结束空轮次");
    let report = world.drain_combining_for_test(slot).expect("排空");
    assert_eq!(report.executions, 1);
    assert_eq!(report.woken, 1);
    assert_eq!(world.provider.word_sleepers(word), Some(0));
    assert!(world.provider.word_wakes(word).expect("wait 字已登记") >= 1);
    assert_eq!(
        world.provider_stats().decommitted_total - before_decommitted,
        EXTENT_CLASS_LADDER[0]
    );
    // 物理页只被撤销一次。extent 仍留在阶梯里：`extent-coalesce` 是第二条冷操作，而
    // parked 失败让 `apply_extent_trim` 在第一步之后就返回——范围动作没有被静默跳过，
    // 调用方必须自己看到这次失败。
    assert_eq!(
        world
            .extents()
            .descriptor(extent)
            .expect("extent 仍在阶梯中")
            .state,
        ExtentState::Live
    );
    // 记录已被执行并发布 response，但请求者已经带着不变量错误返回，因此槽位保持在
    // 「已完成、未回收」状态：`configure_combining` 在下一轮配置时正是拒绝这种在飞状态。
    assert_eq!(world.combining_pending_records(), 1);
}

/// 记录池通过 typed cold operation 补充 chunk，算术不变量同时成立。
#[test]
fn operation_pool_refills_through_typed_cold_operation() {
    let extents = 31_u32;
    let mut world = world(CombiningMode::Combined, 1, 71);
    for _ in 0..extents {
        take_extent(&mut world, 0, 0);
    }
    let before = world.combining_stats();
    assert_eq!(world.combining_pool_chunks(), 1);
    assert_eq!(before.refills, 1);
    assert_eq!(
        world.combining_tag_requests(OperationTag::GlobalRangeRefill),
        0
    );
    let report = pressure_trim(&mut world);
    let after = world.combining_stats();
    assert_eq!(report.trimmed, extents);
    assert!(world.combining_pool_chunks() >= 2);
    // 每次补充恰好来自一条 refill 请求，而构造时的首个 chunk 也计入 refills。
    assert_eq!(
        world.combining_tag_requests(OperationTag::GlobalRangeRefill),
        after.refills - before.refills
    );
    assert!(after.refills > before.refills);
    // 每条请求要么被执行要么被合并进一次执行：没有超时也没有取消。
    assert_eq!(after.timeouts, 0);
    assert_eq!(after.cancellations, 0);
    assert_eq!(after.executions + after.merged_requests, after.requests);
    assert!(after.executions > before.executions);
    assert_eq!(world.combining_pending_records(), 0);
}

/// 在飞记录存在时配置平面必须失败，且平面状态保持原样。
#[test]
fn configure_refuses_while_records_are_in_flight() {
    let mut world = world(CombiningMode::Combined, 1, 81);
    let extent = take_extent(&mut world, 0, 0);
    let token = world.token(0);
    let slot = world
        .combining_slot_for_test(&token)
        .expect("raw owner 槽位");
    drive_grace(&mut world, extent).expect("grace 走完");
    world.begin_combining_round_for_test(slot).expect("开轮");
    let error = world
        .poll_trim_extent(extent, ExtentOccupancy::default())
        .expect_err("轮次打开时请求必须挂链等待");
    assert_eq!(error.message(), "combiner 未在固定轮次内发布 response");
    assert_eq!(world.combining_pending_records(), 1);
    let before = stats_array(world.combining_stats());
    let contract = contract(CombiningMode::Combined);
    let refused = world
        .configure_combining(&contract)
        .expect_err("在飞记录必须拒绝");
    assert_eq!(
        refused.message(),
        "combining 平面配置前不得存在在飞 operation record"
    );
    // 平面状态与在飞记录都没有被改动。
    assert_eq!(before, stats_array(world.combining_stats()));
    assert_eq!(world.combining_pending_records(), 1);
    // 收尾：结束轮次并排空，确认挂链记录仍能被正常服务。
    world
        .end_combining_round_for_test(slot)
        .expect("结束空轮次");
    let report = world.drain_combining_for_test(slot).expect("排空");
    assert_eq!(report.executions, 1);
    // 请求者已经返回，因此这次排空留下的记录只有「已完成、未回收」这一种状态；它正是
    // 上面被拒绝的那种在飞记录，不会因为配置失败就被静默丢弃。
    assert_eq!(world.combining_pending_records(), 1);
}

/// 已取消的冷操作永远不会应用范围动作。
#[test]
fn cancelled_operation_never_applies_range_work() {
    let mut world = world(CombiningMode::Combined, 1, 91);
    let extent = take_extent(&mut world, 0, 0);
    let token = world.token(0);
    let slot = world
        .combining_slot_for_test(&token)
        .expect("raw owner 槽位");
    drive_grace(&mut world, extent).expect("grace 走完");
    world.begin_combining_round_for_test(slot).expect("开轮");
    let before_committed = world.provider_stats().committed_bytes;
    let before_decommitted = world.provider_stats().decommitted_total;
    world
        .poll_trim_extent(extent, ExtentOccupancy::default())
        .expect_err("轮次打开时请求必须挂链等待");
    assert_eq!(
        world
            .cancel_parked_combining_for_test(slot)
            .expect("取消挂链记录"),
        OperationOutcome::Cancelled
    );
    world
        .end_combining_round_for_test(slot)
        .expect("结束空轮次");
    let report = world.drain_combining_for_test(slot).expect("排空");
    assert_eq!(report.cancellations, 1);
    assert_eq!(report.executions, 0);
    assert_eq!(report.woken, 0);
    // extent 仍然 Live，物理页一次都没有被撤销。
    assert_eq!(
        world
            .extents()
            .descriptor(extent)
            .expect("extent 仍存在")
            .state,
        ExtentState::Live
    );
    assert_eq!(world.provider_stats().committed_bytes, before_committed);
    assert_eq!(world.provider_stats().decommitted_total, before_decommitted);
    assert_eq!(world.combining_pending_records(), 0);
    // park 路径读回 response 时的分支：取消与超时都不是「被执行」。
    assert_eq!(
        combining_outcome(Some(OperationOutcome::Cancelled))
            .expect_err("取消的请求没有被执行")
            .message(),
        "combining 请求未被执行"
    );
    assert_eq!(
        combining_outcome(Some(OperationOutcome::TimedOut))
            .expect_err("超时的请求没有被执行")
            .message(),
        "combining 请求未被执行"
    );
    assert_eq!(
        combining_outcome(None)
            .expect_err("没有 response")
            .message(),
        "combiner 未在固定轮次内发布 response"
    );
    assert_eq!(
        combining_outcome(Some(OperationOutcome::Applied { bytes: 4096 })).expect("已执行的请求"),
        OperationOutcome::Applied { bytes: 4096 }
    );
}
