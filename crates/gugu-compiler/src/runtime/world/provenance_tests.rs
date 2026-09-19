//! raw link provenance 与 release 安全 profile 的世界级确定性回归。
//!
//! 覆盖整体契约与 policy 的跨段校验、debug profile 的 poison/双返还标记/全链 verifier、
//! security profile 的随机复用与 checked copy/guard region、释放拒绝的稳定分类
//! （链损坏、伪造、重复返还、跨 owner/class、旧 generation）。全部进程内运行。

use super::RawWorld;
use super::heap_tests::{gc_contract, gc_contract_with};
use crate::runtime::inbox::{ServiceBudget, ShardIndex};
use crate::runtime::message::{
    BatchLimits, FlushTrigger, ProducerStaging, ReturnKind, stage_message,
};
use crate::runtime::model::RawPlanePolicyV1;
use crate::runtime::provenance::{GuardedBuffer, ReleaseRejection};
use crate::runtime::provenance_schema::{
    ProvenanceDemand, ProvenancePolicyV1, ProvenanceRuntimeContract, SafetyProfile,
};
use crate::runtime::size_class::RuntimeSizeClassId;
use crate::runtime::slab::{RawSlot, SlabGeneration, SlotState};
use crate::runtime::{CompressionDemand, RuntimeRawContractV1};

/// 构建带指定安全 profile 的整体契约。
fn provenance_contract(profile: SafetyProfile) -> RuntimeRawContractV1 {
    gc_contract_with(
        RawPlanePolicyV1 {
            provenance: ProvenancePolicyV1 { profile },
            ..RawPlanePolicyV1::default()
        },
        CompressionDemand::default(),
    )
}

/// 构建一个独立的 provenance 契约段；平面只消费 mode 与需求。
fn segment(profile: SafetyProfile) -> ProvenanceRuntimeContract {
    ProvenanceRuntimeContract::build(
        ProvenanceDemand::derive(2, 7, 5),
        ProvenancePolicyV1 { profile },
    )
    .expect("provenance 段可构建")
}

/// 构建一个按给定 profile 配置的单 owner world。
fn world(profile: SafetyProfile, nodes: u32) -> RawWorld {
    let mut world = RawWorld::new(11, 1, nodes, BatchLimits::default()).expect("world 可创建");
    world.configure_provenance(&segment(profile));
    world
}

fn budget() -> ServiceBudget {
    ServiceBudget::pressure(u32::MAX, u64::MAX)
}

fn shard(index: u32) -> ShardIndex {
    ShardIndex::from_raw(index).expect("shard 编号合法")
}

fn class() -> RuntimeSizeClassId {
    RuntimeSizeClassId::from_raw(0)
}

fn stride(world: &RawWorld) -> u64 {
    u64::from(
        world
            .classes()
            .get(class())
            .expect("class 已登记")
            .slot_stride,
    )
}

/// 测试用发布：单条消息立即冲入目标 inbox；与 runtime/tests.rs 的发布路径一致。
fn publish(world: &RawWorld, owner: u32, message: &super::super::message::ReturnMessage) {
    let inbox = world.inbox(owner);
    let pool = world.pool();
    let mut staging = ProducerStaging::new(BatchLimits::default());
    let outcome = stage_message(
        &pool,
        Some(&inbox),
        &mut staging,
        message,
        shard(0),
        Some(FlushTrigger::OwnerPressure),
    )
    .expect("发布成功");
    assert_eq!(outcome.map(|outcome| outcome.items), Some(1));
}

/// 整体契约的跨段校验：provenance 段与 raw policy 不一致、需求派生不一致均拒绝。
#[test]
fn raw_contract_cross_checks_provenance_segment() {
    let mut contract = gc_contract();
    contract.verify().expect("整体契约自洽");

    // 把 provenance 段换成 debug：子段自洽，但与 policy（release）对不上。
    *contract.provenance_mut() = segment(SafetyProfile::Debug);
    assert!(contract.verify().is_err());

    // 需求与 raw 平面派生值不一致。
    let mut contract = gc_contract();
    let drifted = ProvenanceRuntimeContract::build(
        ProvenanceDemand::derive(contract.provenance().demand().owners + 1, 0, 0),
        ProvenancePolicyV1::release(),
    )
    .expect("漂移需求段可构建");
    *contract.provenance_mut() = drifted;
    assert!(contract.verify().is_err());
}

/// debug profile：返还写入 poison 标记、标记识别重复返还、重新分配清除标记、全链走查计数。
#[test]
fn debug_profile_poisons_returns_and_flags_double_return() {
    let mut world = world(SafetyProfile::Debug, 64);
    assert_eq!(world.provenance().mode(), SafetyProfile::Debug);
    let allocation = world.allocate(0, class()).expect("分配成功");
    let slot = allocation.slot;
    world
        .local_return(0, slot, stride(&world))
        .expect("本地归还成功");
    assert_eq!(world.provenance_stats()[1], 1, "poison-writes");
    // poison 标记在进入状态机之前把重复返还按稳定分类记账；状态机仍按原语义拒绝。
    let error = world.queue_return(0, slot, stride(&world)).unwrap_err();
    assert!(error.message().contains("slot 状态迁移非法"));
    assert_eq!(world.provenance_stats()[2], 1, "double-return-markers");
    assert_eq!(
        world.provenance_rejection_count(ReleaseRejection::DoubleReturn),
        1
    );
    // poison 标记在重新分配时清除：同一 slot 可以正常复用。
    let reallocated = world.allocate(0, class()).expect("重新分配成功");
    assert_eq!(reallocated.slot.index, slot.index);
    assert_eq!(world.provenance_stats()[1], 1, "重新分配不新增 poison 写入");
    // debug profile 的全链走查计数。
    world.verify_links().expect("free 链完整");
    assert_eq!(world.provenance_stats()[3], 1, "full-chain-verifications");
}

/// debug profile：free 期间被写入的 slot 在分配时按 chain-corruption 拒绝。
#[test]
fn debug_stamp_detects_writes_to_freed_slots() {
    let mut world = world(SafetyProfile::Debug, 64);
    let allocation = world.allocate(0, class()).expect("分配成功");
    let slot = allocation.slot;
    world
        .local_return(0, slot, stride(&world))
        .expect("本地归还成功");
    world
        .table
        .write_stamp(slot.descriptor, slot.index, 0x9999_9999_9999_9999)
        .expect("标记可改写");
    let error = world.allocate(0, class()).unwrap_err();
    assert!(error.message().contains("poison"));
    assert_eq!(
        world.provenance_rejection_count(ReleaseRejection::ChainCorruption),
        1
    );
}

/// security profile：随机复用改变顺序但不改变集合与账本；release 保持 LIFO。
#[test]
fn security_profile_randomizes_reuse_order_with_identical_ledger() {
    let mut release = world(SafetyProfile::Release, 256);
    let mut security = world(SafetyProfile::Security, 256);
    let run = |world: &mut RawWorld| -> (Vec<u32>, Vec<u32>) {
        let mut live = Vec::new();
        for _ in 0..4 {
            let allocation = world.allocate(0, class()).expect("分配成功");
            live.push(allocation.slot);
        }
        for slot in &live {
            world
                .local_return(0, *slot, stride(world))
                .expect("归还成功");
        }
        let mut reused = Vec::new();
        for _ in 0..4 {
            let allocation = world.allocate(0, class()).expect("复用分配成功");
            reused.push(allocation.slot.index);
        }
        (live.iter().map(|slot| slot.index).collect(), reused)
    };
    let (release_live, release_reused) = run(&mut release);
    let (security_live, security_reused) = run(&mut security);
    assert_eq!(release_live, security_live, "初始 bump 顺序一致");
    assert_eq!(
        release_reused,
        release_live.iter().rev().copied().collect::<Vec<_>>(),
        "release 保持 LIFO"
    );
    let mut release_set = release_reused.clone();
    let mut security_set = security_reused.clone();
    release_set.sort_unstable();
    security_set.sort_unstable();
    assert_eq!(release_set, security_set, "复用的 slot 集合一致");
    assert_ne!(
        security_reused, release_reused,
        "security 复用顺序偏离 LIFO"
    );
    assert_eq!(release.provenance_stats()[4], 0, "release 不随机化");
    assert!(security.provenance_stats()[4] > 0, "randomized-pops");
    assert!(release.verify_links().is_ok());
    assert!(security.verify_links().is_ok());
    assert!(release.ledger_invariant(0).is_ok());
    assert!(security.ledger_invariant(0).is_ok());
    assert_eq!(
        release.provider_stats().committed_bytes,
        security.provider_stats().committed_bytes
    );
}

/// 释放拒绝按稳定分类记账；失败路径不改变 slot 状态、不静默丢弃消息。
#[test]
fn release_rejections_are_categorized_stably() {
    // chain-corruption：消息 checksum 被破坏。
    let mut corrupt_world = world(SafetyProfile::Release, 64);
    let allocation = corrupt_world.allocate(0, class()).expect("分配成功");
    corrupt_world
        .queue_return(0, allocation.slot, stride(&corrupt_world))
        .expect("归还成功");
    let mut message = corrupt_world
        .message(
            corrupt_world.token(0),
            ReturnKind::RawSlot,
            allocation.slot,
            stride(&corrupt_world) as u32,
        )
        .expect("消息可构造");
    message.integrity.checksum ^= 0xdead_beef;
    publish(&corrupt_world, 0, &message);
    let error = corrupt_world
        .service(0, shard(0), &budget())
        .expect_err("被破坏的消息必须进入不变量失败");
    assert!(error.message().contains("integrity"));
    assert_eq!(
        corrupt_world.provenance_rejection_count(ReleaseRejection::ChainCorruption),
        1
    );
    assert_eq!(
        corrupt_world
            .table()
            .state(allocation.slot.descriptor, allocation.slot.index),
        Ok(SlotState::ReturnQueued),
        "拒绝的消息不得丢弃 slot 状态"
    );

    // cross-owner：target=owner 0 的消息被投进 owner 1 的 inbox，owner 1 消费时拒绝。
    let mut cross_world = RawWorld::new(11, 2, 64, BatchLimits::default()).expect("world 可创建");
    cross_world.configure_provenance(&segment(SafetyProfile::Release));
    let allocation = cross_world.allocate(0, class()).expect("分配成功");
    cross_world
        .queue_return(0, allocation.slot, stride(&cross_world))
        .expect("归还成功");
    let message = cross_world
        .message(
            cross_world.token(0),
            ReturnKind::RawSlot,
            allocation.slot,
            stride(&cross_world) as u32,
        )
        .expect("消息可构造");
    publish(&cross_world, 1, &message);
    let error = cross_world
        .service(1, shard(0), &budget())
        .expect_err("投递到非目标 owner 的消息必须失败");
    assert!(error.message().contains("非目标 owner"));
    assert_eq!(
        cross_world.provenance_rejection_count(ReleaseRejection::CrossOwner),
        1
    );

    // cross-class：bytes 与 class stride 不一致。
    let mut class_world = world(SafetyProfile::Release, 64);
    let allocation = class_world.allocate(0, class()).expect("分配成功");
    class_world
        .queue_return(0, allocation.slot, stride(&class_world))
        .expect("归还成功");
    let message = class_world
        .message(
            class_world.token(0),
            ReturnKind::RawSlot,
            allocation.slot,
            (stride(&class_world) * 2) as u32,
        )
        .expect("消息可构造");
    publish(&class_world, 0, &message);
    let error = class_world
        .service(0, shard(0), &budget())
        .expect_err("bytes 与 stride 不一致必须失败");
    assert!(error.message().contains("bytes 与 class stride"));
    assert_eq!(
        class_world.provenance_rejection_count(ReleaseRejection::CrossClass),
        1
    );

    // stale-generation：返还请求引用过期 generation。
    let mut stale_world = world(SafetyProfile::Release, 64);
    let allocation = stale_world.allocate(0, class()).expect("分配成功");
    let stale = RawSlot {
        generation: SlabGeneration::from_raw(allocation.slot.generation.raw() + 1),
        ..allocation.slot
    };
    let error = stale_world
        .queue_return(0, stale, stride(&stale_world))
        .unwrap_err();
    assert!(error.message().contains("过期 generation"));
    assert_eq!(
        stale_world.provenance_rejection_count(ReleaseRejection::StaleGeneration),
        1
    );

    // double-return：slot 已返还后重放同一条消息。
    let mut replay_world = world(SafetyProfile::Release, 64);
    let allocation = replay_world.allocate(0, class()).expect("分配成功");
    replay_world
        .queue_return(0, allocation.slot, stride(&replay_world))
        .expect("归还成功");
    let message = replay_world
        .message(
            replay_world.token(0),
            ReturnKind::RawSlot,
            allocation.slot,
            stride(&replay_world) as u32,
        )
        .expect("消息可构造");
    publish(&replay_world, 0, &message);
    replay_world
        .service(0, shard(0), &budget())
        .expect("首次消费成功");
    publish(&replay_world, 0, &message);
    let error = replay_world
        .service(0, shard(0), &budget())
        .expect_err("重放消息必须失败");
    assert!(error.message().contains("ReturnQueued"));
    assert_eq!(
        replay_world.provenance_rejection_count(ReleaseRejection::DoubleReturn),
        1
    );
}

/// guard region 与 checked copy 不改变分配/归还语义；拒绝与统计各自正确。
#[test]
fn guard_regions_and_checked_copy_do_not_change_allocation_semantics() {
    let mut release = world(SafetyProfile::Release, 256);
    let mut security = world(SafetyProfile::Security, 256);
    security.register_guard_region(0x7000_0000, 4096);
    let source = GuardedBuffer::new(0x1000, true, false, vec![1, 2, 3, 4]);
    let mut target = GuardedBuffer::new(0x8000_0000, false, true, vec![0; 4]);
    security
        .checked_copy(&source, &mut target, 0, 0, 4)
        .expect("合法 checked copy 成功");
    assert_eq!(target.bytes(), &[1, 2, 3, 4]);
    let mut overlap = GuardedBuffer::new(0x7000_0000, false, true, vec![0; 4]);
    security
        .checked_copy(&source, &mut overlap, 0, 0, 4)
        .expect_err("guard region 重叠必须拒绝");
    assert_eq!(
        security.provenance_rejection_count(ReleaseRejection::GuardRegion),
        1
    );
    // 两个 profile 的分配/归还序列与账本完全一致。
    for world in [&mut release, &mut security] {
        let first = world.allocate(0, class()).expect("首次分配");
        let second = world.allocate(0, class()).expect("第二次分配");
        world
            .local_return(0, first.slot, stride(world))
            .expect("归还成功");
        let third = world.allocate(0, class()).expect("复用分配");
        assert_eq!(third.slot.index, first.slot.index);
        world
            .local_return(0, second.slot, stride(world))
            .expect("归还成功");
        world
            .local_return(0, third.slot, stride(world))
            .expect("归还成功");
        assert!(world.verify_links().is_ok());
        assert!(world.ledger_invariant(0).is_ok());
    }
    assert_eq!(security.provenance_stats()[5], 1, "checked-copies");
    assert_eq!(release.provenance_stats()[5], 0);
}
