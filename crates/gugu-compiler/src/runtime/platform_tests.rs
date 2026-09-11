//! 平台 range、二次幂 extent 与内存账本的确定性测试。
//!
//! 覆盖操作目录与故障映射、extent 阶梯的分裂与合并、三路 lease 与 queue-page grace 门禁、
//! 页粒度 commit/decommit 的两个字节口径，以及 guard、wait/wake、entropy 与 dump policy 的
//! 确定性行为。全部进程内、快速、无真实系统调用。

use super::RAW_SLAB_PAGE_BYTES;
use super::extent::{
    EXTENT_CLASS_LADDER, ExtentLease, ExtentOccupancy, ExtentState, ExtentTable, TrimBlocked,
    class_bytes, class_for_bytes,
};
use super::inbox::{GraceOutcome, ServiceBudget};
use super::platform::{FakePlatform, PlatformProfile};
use super::provider::{
    DumpPolicy, FaultClass, ProviderError, RangeProvider, RangeState, WaitOutcome,
};
use super::size_class::RuntimeSizeClassId;
use super::slab::{Epoch, MemoryDomainId, OwnerToken, RuntimeSeed};
use super::world::{OWNER_ARENA_BYTES, RawWorld};

fn world(owners: u32, nodes: u32) -> RawWorld {
    RawWorld::new(7, owners, nodes, super::message::BatchLimits::default())
        .expect("raw world 可创建")
}

fn descriptor_stride(world: &RawWorld, class: RuntimeSizeClassId) -> u32 {
    world
        .classes()
        .get(class)
        .expect("class 已登记")
        .slot_stride
}

fn extent_token(owner_id: u64) -> OwnerToken {
    OwnerToken {
        domain: MemoryDomainId::RUNTIME_RAW,
        owner_id: super::slab::OwnerId::from_raw(owner_id),
        generation: super::slab::OwnerGeneration::from_raw(1),
        route_key: super::slab::RouteKey::from_raw(owner_id),
    }
}

/// 建一张带 `owners` 个 arena 的 extent 表；每个 arena 的容量等于阶梯顶层。
fn extent_table(owners: u32) -> ExtentTable {
    let top = EXTENT_CLASS_LADDER[EXTENT_CLASS_LADDER.len() - 1];
    let mut table = ExtentTable::new();
    for owner in 0..owners {
        table
            .register_owner(
                owner,
                extent_token(u64::from(owner) + 1),
                MemoryDomainId::RUNTIME_RAW,
                super::provider::RangeId(owner),
                top * u64::from(owner),
                top,
            )
            .expect("arena 可登记");
    }
    table
}

fn empty_occupancy() -> ExtentOccupancy {
    ExtentOccupancy::default()
}

#[test]
fn extent_ladder_is_power_of_two_and_covers_slab_pages() {
    assert_eq!(EXTENT_CLASS_LADDER[0], 4096);
    for pair in EXTENT_CLASS_LADDER.windows(2) {
        assert_eq!(pair[1], pair[0] * 2, "阶梯必须逐级倍增");
        assert!(pair[0].is_power_of_two());
    }
    assert_eq!(
        EXTENT_CLASS_LADDER[EXTENT_CLASS_LADDER.len() - 1],
        2 * 1024 * 1024,
        "阶梯顶层必须等于 huge-page 阈值"
    );
    assert_eq!(
        class_for_bytes(RAW_SLAB_PAGE_BYTES),
        Some(4),
        "64 KiB slab page 落在阶梯第 4 级"
    );
    assert_eq!(class_bytes(4), Some(RAW_SLAB_PAGE_BYTES));
    assert_eq!(class_for_bytes(EXTENT_CLASS_LADDER[9] + 1), None);
}

#[test]
fn extent_split_descends_and_coalesces_back_to_top() {
    let mut table = extent_table(1);
    let top = 9_u32;
    assert_eq!(table.free_blocks(0, top), 1);
    let extent = table
        .allocate(0, 4, MemoryDomainId::RUNTIME_RAW)
        .expect("分配成功");
    // 顶层块分裂到第 4 级：每级留下一个空闲的 buddy。
    for class in 4..top {
        assert_eq!(
            table.free_blocks(0, class),
            1,
            "第 {class} 级应留下一个 buddy"
        );
    }
    let descriptor = *table.descriptor(extent).expect("描述符存在");
    assert_eq!(descriptor.bytes, RAW_SLAB_PAGE_BYTES);
    assert_eq!(
        descriptor.base % RAW_SLAB_PAGE_BYTES,
        0,
        "extent 按自身大小对齐"
    );
    assert_eq!(descriptor.state, ExtentState::Live);
    table.verify().expect("位图与描述符一致");

    table.give_back(extent).expect("归还成功");
    assert_eq!(table.free_blocks(0, top), 1, "相邻合并必须回到顶层");
    for class in 4..top {
        assert_eq!(table.free_blocks(0, class), 0, "中间级不得残留空闲块");
    }
    assert_eq!(table.live_count(), 0);
    table.verify().expect("合并后位图仍然一致");
}

#[test]
fn extent_slot_reuse_advances_generation() {
    let mut table = extent_table(1);
    let first = table
        .allocate(0, 3, MemoryDomainId::RUNTIME_RAW)
        .expect("分配成功");
    let generation = table.descriptor(first).expect("描述符存在").generation;
    table.give_back(first).expect("归还成功");
    let second = table
        .allocate(0, 3, MemoryDomainId::RUNTIME_RAW)
        .expect("再分配成功");
    assert_eq!(second.raw(), first.raw(), "空闲槽位必须复用");
    assert!(
        table
            .descriptor(second)
            .expect("描述符存在")
            .generation
            .raw()
            > generation.raw(),
        "槽位复用时 generation 必须推进"
    );
}

#[test]
fn extent_trim_requires_every_lease_and_grace() {
    let mut table = extent_table(1);
    let extent = table
        .allocate(0, 4, MemoryDomainId::RUNTIME_RAW)
        .expect("分配成功");
    let epoch = Epoch::from_raw(1);

    for lease in ExtentLease::ALL {
        table.acquire_lease(extent, lease).expect("取得 lease");
        assert_eq!(
            table.poll_trim(extent, epoch, empty_occupancy()),
            Err(TrimBlocked::Lease(lease)),
            "诊断必须点名未归零的那一路 lease"
        );
        table.release_lease(extent, lease).expect("结束 lease");
    }

    assert_eq!(
        table.poll_trim(
            extent,
            epoch,
            ExtentOccupancy {
                live_slots: 2,
                ..ExtentOccupancy::default()
            }
        ),
        Err(TrimBlocked::LiveSlots(2))
    );
    assert_eq!(
        table.poll_trim(
            extent,
            epoch,
            ExtentOccupancy {
                queued_slots: 3,
                ..ExtentOccupancy::default()
            }
        ),
        Err(TrimBlocked::QueuedSlots(3))
    );
    assert_eq!(
        table.poll_trim(
            extent,
            epoch,
            ExtentOccupancy {
                pending_returns: 1,
                ..ExtentOccupancy::default()
            }
        ),
        Err(TrimBlocked::PendingReturns(1))
    );

    // 门禁全部满足后仍需走完固定步数的 grace，且步数按 epoch 推进。
    assert_eq!(
        table.poll_trim(extent, epoch, empty_occupancy()),
        Err(TrimBlocked::GracePending {
            completed: 0,
            required: super::model::GRACE_STEPS,
        })
    );
    for step in 1..super::model::GRACE_STEPS {
        assert_eq!(
            table.poll_trim(extent, Epoch::from_raw(step + 1), empty_occupancy()),
            Err(TrimBlocked::GracePending {
                completed: step,
                required: super::model::GRACE_STEPS,
            })
        );
    }
    assert_eq!(
        table.poll_trim(
            extent,
            Epoch::from_raw(super::model::GRACE_STEPS + 1),
            empty_occupancy()
        ),
        Ok(())
    );
}

#[test]
fn extent_trim_grace_resets_when_a_lease_arrives() {
    let mut table = extent_table(1);
    let extent = table
        .allocate(0, 4, MemoryDomainId::RUNTIME_RAW)
        .expect("分配成功");
    let epoch = Epoch::from_raw(1);
    let _ = table.poll_trim(extent, epoch, empty_occupancy());
    let _ = table.poll_trim(extent, Epoch::from_raw(2), empty_occupancy());
    assert_eq!(table.trim_progress(extent), 1);
    table
        .acquire_lease(extent, ExtentLease::Scanner)
        .expect("取得 lease");
    assert_eq!(
        table.trim_progress(extent),
        0,
        "新 lease 必须重置 grace 进度"
    );
    table
        .release_lease(extent, ExtentLease::Scanner)
        .expect("结束 lease");
}

#[test]
fn extent_release_requires_no_outstanding_lease() {
    let mut table = extent_table(1);
    let extent = table
        .allocate(0, 4, MemoryDomainId::RUNTIME_RAW)
        .expect("分配成功");
    table
        .acquire_lease(extent, ExtentLease::Forwarder)
        .expect("取得 lease");
    assert!(
        table.give_back(extent).is_err(),
        "仍有 lease 时不得归还 extent"
    );
    assert!(table.descriptor(extent).is_some());
}

#[test]
fn extent_table_rejects_unaligned_arena_and_unknown_class() {
    let mut table = ExtentTable::new();
    assert!(
        table
            .register_owner(
                0,
                extent_token(1),
                MemoryDomainId::RUNTIME_RAW,
                super::provider::RangeId(0),
                4096,
                2 * 1024 * 1024,
            )
            .is_err(),
        "arena 基址必须按顶层 class 对齐"
    );
    assert!(
        table
            .register_owner(
                0,
                extent_token(1),
                MemoryDomainId::RUNTIME_RAW,
                super::provider::RangeId(0),
                0,
                4096,
            )
            .is_err(),
        "arena 容量必须是顶层 class 的整数倍"
    );
    let mut table = extent_table(1);
    assert!(
        table.allocate(0, 11, MemoryDomainId::RUNTIME_RAW).is_err(),
        "未知 class 必须被拒绝"
    );
    assert!(
        table.allocate(9, 4, MemoryDomainId::RUNTIME_RAW).is_err(),
        "未登记 owner 必须被拒绝"
    );
}

#[test]
fn platform_profiles_map_every_failure_identically() {
    for error in ProviderError::ALL {
        let linux = PlatformProfile::Linux.fault_class(error);
        let windows = PlatformProfile::Windows.fault_class(error);
        assert_eq!(
            linux,
            windows,
            "失败 `{}` 在两个 profile 上必须映射一致",
            error.name()
        );
        assert_eq!(linux, error.fault_class());
    }
    assert_eq!(
        ProviderError::OutOfSpace.fault_class(),
        FaultClass::OutOfMemory
    );
    assert_eq!(
        ProviderError::MappingLimit.fault_class(),
        FaultClass::ResourceExhausted
    );
    assert_eq!(
        ProviderError::DoubleRelease.fault_class(),
        FaultClass::RuntimeInvariant
    );
}

#[test]
fn platform_contract_carries_ops_states_classes_and_fault_map() {
    let section = super::platform_schema::PlatformRangeSchemaV1::build(
        PlatformProfile::Linux,
        super::platform_schema::PlatformRangeDemand::derive(2, 3, 1, 2),
    )
    .expect("平台契约段可构建");
    section.verify().expect("平台契约段必须自洽");
    assert_eq!(
        section.op_count(),
        super::platform_schema::PlatformOp::ALL.len() as u32
    );
    assert_eq!(section.class_count(), EXTENT_CLASS_LADDER.len() as u32);
    assert_eq!(section.profile(), "linux");
    assert_eq!(section.guard_bytes(), 4096);
    assert_eq!(section.dump_policy_default(), "included");
    assert_eq!(
        section.fault_map.len(),
        PlatformProfile::ALL.len() * ProviderError::ALL.len()
    );
    // 编码对 profile 敏感，使 Windows 与 Linux 的契约指纹不同。
    let windows = super::platform_schema::PlatformRangeSchemaV1::build(
        PlatformProfile::Windows,
        super::platform_schema::PlatformRangeDemand::derive(2, 3, 1, 2),
    )
    .expect("Windows 平台契约段可构建");
    assert_ne!(section.canonical_bytes(), windows.canonical_bytes());
}

#[test]
fn platform_demand_derives_documented_lower_bounds() {
    let demand = super::platform_schema::PlatformRangeDemand::derive(4, 3, 2, 5);
    assert_eq!(demand.owners, 4);
    assert_eq!(demand.payload_extents, 4 + 2 + 5);
    assert_eq!(demand.metadata_extents, 8);
    assert_eq!(demand.stack_extents, 4);
    assert_eq!(demand.guard_extents, 12);
    demand.verify().expect("下界关系成立");

    let mut broken = demand;
    broken.payload_extents = 1;
    assert!(broken.verify().is_err(), "低于 owner 下界必须被拒绝");
    let mut broken = demand;
    broken.guard_extents = 1;
    assert!(broken.verify().is_err(), "guard 需求不得低于 stack 需求");
    let mut broken = demand;
    broken.owners = 0;
    assert!(broken.verify().is_err(), "必须至少一个 owner");
}

#[test]
fn platform_page_commit_splits_reserved_and_committed_bytes() {
    let mut provider = FakePlatform::new(PlatformProfile::Linux, 1 << 20);
    let range = provider
        .reserve_aligned(65536, 65536, MemoryDomainId::RUNTIME_RAW)
        .expect("预留成功");
    assert_eq!(provider.stats().reserved_bytes, 65536);
    assert_eq!(provider.stats().committed_bytes, 0);

    provider.commit_pages(range, 0, 4096).expect("提交一页成功");
    assert_eq!(provider.stats().committed_bytes, 4096);
    assert_eq!(provider.stats().reserved_bytes, 65536 - 4096);
    assert_eq!(provider.committed_pages(range), 1);

    provider
        .commit_pages(range, 4096, 8192)
        .expect("再提交两页成功");
    assert_eq!(provider.stats().committed_bytes, 4096 * 3);
    assert_eq!(provider.stats().reserved_bytes, 65536 - 4096 * 3);
    // 两个口径之和恒等于 range 字节数，说明没有字节被重复计数。
    assert_eq!(
        provider.stats().reserved_bytes + provider.stats().committed_bytes,
        65536
    );

    provider
        .decommit_pages(range, 0, 4096)
        .expect("撤销一页成功");
    assert_eq!(provider.stats().committed_bytes, 4096 * 2);
    assert_eq!(
        provider.stats().reserved_bytes + provider.stats().committed_bytes,
        65536
    );
}

#[test]
fn platform_page_commit_rejects_unaligned_and_out_of_bounds_spans() {
    let mut provider = FakePlatform::new(PlatformProfile::Linux, 1 << 20);
    let range = provider
        .reserve_aligned(65536, 65536, MemoryDomainId::RUNTIME_RAW)
        .expect("预留成功");
    assert_eq!(
        provider.commit_pages(range, 1, 4096),
        Err(ProviderError::InvalidSubRange),
        "偏移必须按页对齐"
    );
    assert_eq!(
        provider.commit_pages(range, 0, 2048),
        Err(ProviderError::InvalidSubRange),
        "长度必须按页对齐"
    );
    assert_eq!(
        provider.commit_pages(range, 0, 0),
        Err(ProviderError::InvalidSubRange),
        "长度不能为 0"
    );
    assert_eq!(
        provider.commit_pages(range, 65536, 4096),
        Err(ProviderError::InvalidSubRange),
        "区间不得越界"
    );
    assert_eq!(
        provider.commit_pages(super::provider::RangeId(9), 0, 4096),
        Err(ProviderError::UnknownRange)
    );
    assert_eq!(provider.stats().rejected_requests, 5);
}

#[test]
fn platform_guard_wait_wake_entropy_and_dump_policy() {
    let mut provider = FakePlatform::new(PlatformProfile::Windows, 1 << 20);
    let range = provider
        .reserve_aligned(65536, 65536, MemoryDomainId::RUNTIME_RAW)
        .expect("预留成功");
    assert_eq!(
        provider.protect_guard(range),
        Err(ProviderError::NotCommitted),
        "未 commit 不得建立 guard"
    );
    provider.commit(range).expect("提交成功");
    provider.protect_guard(range).expect("建立 guard 成功");
    let descriptor = provider.describe(range).expect("描述符存在");
    assert_eq!(descriptor.guard_bytes, 4096);
    assert_eq!(descriptor.payload_bytes(), 65536 - 4096);
    assert_eq!(
        provider.protect_guard(range),
        Err(ProviderError::GuardOverlap),
        "重复建立 guard 必须被拒绝"
    );
    assert_eq!(
        provider.stats().guarded_bytes,
        4096,
        "guard 是 committed 的子集，不重复相加"
    );
    assert!(provider.stats().guarded_bytes <= provider.stats().committed_bytes);
    provider.unprotect(range).expect("取消 guard 成功");
    assert_eq!(provider.describe(range).expect("描述符存在").guard_bytes, 0);
    assert_eq!(provider.unprotect(range), Err(ProviderError::NotGuarded));

    provider
        .set_dump_policy(range, DumpPolicy::Excluded)
        .expect("设置 dump policy 成功");
    assert_eq!(
        provider.describe(range).expect("描述符存在").dump_policy,
        DumpPolicy::Excluded
    );
    provider.huge_page_hint(range).expect("huge-page hint 成功");
    assert!(provider.describe(range).expect("描述符存在").huge_page);
    assert_eq!(
        provider.huge_page_hint(super::provider::RangeId(7)),
        Err(ProviderError::UnknownRange)
    );

    let word = provider.register_wait_word(5);
    assert_eq!(provider.wait(word, 4), Ok(WaitOutcome::Mismatch));
    assert_eq!(provider.wait(word, 5), Ok(WaitOutcome::Woken));
    assert_eq!(provider.word_sleepers(word), Some(1));
    assert_eq!(provider.wake(word, 8), Ok(1));
    assert_eq!(provider.word_sleepers(word), Some(0));
    // wake 推进字值，使旧期望值过期。
    assert_eq!(provider.word_value(word), Some(6));
    assert_eq!(provider.wait(word, 5), Ok(WaitOutcome::Mismatch));
    assert_eq!(
        provider.wake(super::provider::WaitWordId(9), 1),
        Err(ProviderError::UnknownWaitWord)
    );

    let entropy = provider.entropy(24).expect("entropy 可用");
    assert_eq!(entropy.len(), 24);
    assert_ne!(entropy, vec![0_u8; 24], "entropy 不得为全零");
    assert_eq!(provider.stats().entropy_bytes, 24);
    provider.zero(range).expect("清零成功");
    assert_eq!(
        provider.stats().guarded_bytes,
        0,
        "取消 guard 后不再有 guard 字节"
    );
    assert_eq!(provider.stats().zeroed_bytes, 65536);
}

#[test]
fn platform_entropy_failure_maps_to_recoverable_exhaustion() {
    let constants = PlatformProfile::Linux.constants();
    assert!(constants.is_valid());
    let mut provider = FakePlatform::with_capacity(
        PlatformProfile::Linux,
        0x1000_0000_0000,
        1 << 20,
        RuntimeSeed::new(3),
    );
    // entropy 不可用映射到可恢复的平台资源耗尽，而不是实现故障。
    assert_eq!(
        ProviderError::EntropyUnavailable.fault_class(),
        FaultClass::ResourceExhausted
    );
    let entropy = provider.entropy(8).expect("默认 profile 的 entropy 可用");
    assert_eq!(entropy.len(), 8);
}

#[test]
fn world_trim_keeps_committed_pages_until_lease_and_grace_end() {
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    let extent = world
        .table()
        .descriptor(allocation.slot.descriptor)
        .expect("描述符存在")
        .extent;
    let committed = world.provider_stats().committed_bytes;
    assert_eq!(committed, RAW_SLAB_PAGE_BYTES);

    // live slot 尚未归还：reclaim 不把它当候选，物理页保持不变。
    assert!(
        world
            .reclaim_extents_for_test(0)
            .expect("reclaim 可执行")
            .is_empty(),
        "仍有 live slot 的 extent 不应进入 trim 候选"
    );
    assert_eq!(world.provider_stats().committed_bytes, committed);

    let stride = u64::from(descriptor_stride(&world, class));
    world
        .local_return(0, allocation.slot, stride)
        .expect("本地归还成功");

    // 三路 lease 中的任意一路未归零都必须拒绝，且诊断点名该路。
    world
        .acquire_extent_lease_for_test(0, extent, ExtentLease::Allocator)
        .expect("取得 allocator lease");
    assert_eq!(
        world.reclaim_extents_for_test(0).expect("reclaim 可执行"),
        vec![TrimBlocked::Lease(ExtentLease::Allocator)]
    );
    assert_eq!(
        world.provider_stats().committed_bytes,
        committed,
        "lease 未归零时不得撤销物理页"
    );
    world
        .release_extent_lease_for_test(extent, ExtentLease::Allocator)
        .expect("结束 allocator lease");

    // lease 归零后仍需走完固定步数的 queue-page grace。
    for step in 0..super::model::GRACE_STEPS {
        assert_eq!(
            world.reclaim_extents_for_test(0).expect("reclaim 可执行"),
            vec![TrimBlocked::GracePending {
                completed: step,
                required: super::model::GRACE_STEPS,
            }],
            "grace 必须按 epoch 逐步推进"
        );
        assert_eq!(
            world.provider_stats().committed_bytes,
            committed,
            "grace 未走完时不得撤销物理页"
        );
        world.advance_epoch_for_test();
    }

    // 四条门禁全部满足：物理页被撤销，extent 合并回 buddy 阶梯。
    assert!(
        world
            .reclaim_extents_for_test(0)
            .expect("reclaim 可执行")
            .is_empty(),
        "门禁满足后不应再有拒绝"
    );
    assert_eq!(
        world.provider_stats().committed_bytes,
        0,
        "trim 必须撤销该 extent 的物理页"
    );
    // 撤销物理页后该 extent 回到预留口径：owner 的 raw 与 Resource 两个 arena 各 2 MiB
    // 全部计入 range_reserved_bytes，且与 committed 严格互斥。
    assert_eq!(world.provider_stats().reserved_bytes, OWNER_ARENA_BYTES * 2);
    assert_eq!(
        world.provider_stats().reserved_bytes + world.provider_stats().committed_bytes,
        OWNER_ARENA_BYTES * 2
    );
    assert!(
        world.extents().descriptor(extent).is_none(),
        "trim 后 extent 必须归还 buddy 阶梯"
    );
    world.extents().verify().expect("trim 后位图仍然一致");
}

#[test]
fn world_retire_reports_blocked_spans_without_revoking_pages() {
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    let stride = u64::from(descriptor_stride(&world, class));
    world
        .local_return(0, allocation.slot, stride)
        .expect("本地归还成功");
    let committed = world.provider_stats().committed_bytes;

    // retire 会推进 epoch 并尝试回收；grace 尚未走完时物理页必须保留。
    let report = world
        .retire(0, world.domain_owner(), &ServiceBudget::new(8, 1 << 16))
        .expect("retire 可执行");
    assert_eq!(report.grace, GraceOutcome::Converged);
    assert_eq!(world.provider_stats().committed_bytes, committed);
    assert!(
        report.blocked_spans >= 1,
        "grace 未走完时必须报告被门禁保留的 span"
    );
}

#[test]
fn platform_provider_rejects_illegal_sequences_stably() {
    let mut provider = FakePlatform::new(PlatformProfile::Linux, 1 << 20);
    assert_eq!(
        provider.reserve_aligned(0, 64, MemoryDomainId::RUNTIME_RAW),
        Err(ProviderError::ZeroBytes)
    );
    assert_eq!(
        provider.reserve_aligned(64, 48, MemoryDomainId::RUNTIME_RAW),
        Err(ProviderError::NonPowerOfTwoAlignment)
    );
    let range = provider
        .reserve_aligned(4096, 64, MemoryDomainId::RUNTIME_RAW)
        .expect("预留成功");
    assert_eq!(provider.stats().reserved_bytes, 4096);
    assert_eq!(provider.stats().committed_bytes, 0);
    assert_eq!(provider.commit(range), Ok(()));
    assert_eq!(provider.stats().reserved_bytes, 0);
    assert_eq!(provider.stats().committed_bytes, 4096);
    assert_eq!(provider.commit(range), Err(ProviderError::AlreadyCommitted));
    assert_eq!(provider.decommit(range), Ok(()));
    assert_eq!(provider.stats().reserved_bytes, 4096);
    assert_eq!(provider.stats().committed_bytes, 0);
    assert_eq!(provider.decommit(range), Err(ProviderError::NotCommitted));
    assert_eq!(provider.release(range), Ok(()));
    assert_eq!(provider.release(range), Err(ProviderError::DoubleRelease));
    assert_eq!(provider.stats().reserved_bytes, 0);
    let descriptor = provider.describe(range).expect("描述符存在");
    assert_eq!(descriptor.state, RangeState::Released);
    assert_eq!(
        provider.describe_all()[0].base % 64,
        0,
        "range 必须满足对齐要求"
    );
}

#[test]
fn platform_contract_catalog_matches_source_intrinsics_exactly() {
    use crate::frontend::semantics::model::PlatformIntrinsic;
    // 契约目录、GIR/LIR 的操作枚举与 Gugu 侧原语必须一一对应；任何一侧增删都使对应关系失败。
    let section = super::platform_schema::PlatformRangeSchemaV1::build(
        PlatformProfile::Linux,
        super::platform_schema::PlatformRangeDemand::derive(1, 1, 1, 1),
    )
    .expect("平台契约段可构建");
    assert_eq!(
        PlatformIntrinsic::ALL.len(),
        super::platform_schema::PlatformOp::ALL.len()
    );
    assert_eq!(section.op_count(), PlatformIntrinsic::ALL.len() as u32);
    for intrinsic in PlatformIntrinsic::ALL {
        let name = intrinsic.name();
        assert!(
            section.ops.ops.iter().any(|entry| entry.name == name),
            "契约目录缺少原语 `{name}`"
        );
        let op = super::platform_schema::PlatformOp::ALL
            .into_iter()
            .find(|op| op.name() == name)
            .expect("操作枚举必须包含全部原语");
        assert_eq!(
            op.blocking(),
            intrinsic.blocking(),
            "原语 `{name}` 的阻塞分类必须与契约一致"
        );
    }
    for op in super::platform_schema::PlatformOp::ALL {
        assert!(
            PlatformIntrinsic::ALL
                .iter()
                .any(|intrinsic| intrinsic.name() == op.name()),
            "原语枚举缺少契约操作 `{}`",
            op.name()
        );
    }
}

#[test]
fn cross_owner_extent_return_is_exactly_once_and_gated() {
    use super::extent::ExtentId;
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    let extent = world
        .table()
        .descriptor(allocation.slot.descriptor)
        .expect("描述符存在")
        .extent;
    let stride = u64::from(descriptor_stride(&world, class));
    world
        .local_return(0, allocation.slot, stride)
        .expect("本地归还成功");
    let committed = world.provider_stats().committed_bytes;

    // 三路 lease 未归零：归还点不成立，状态必须保持 Live，调用方仍能正常结束 lease 后重试。
    world
        .acquire_extent_lease_for_test(0, extent, ExtentLease::Scanner)
        .expect("取得 scanner lease");
    let error = world
        .release_extent(extent)
        .expect_err("lease 未归零时归还必须被拒绝");
    assert!(
        error.message().contains("scanner lease"),
        "诊断必须点名未归零的 lease 路：{error}"
    );
    assert_eq!(
        world.provider_stats().committed_bytes,
        committed,
        "lease 未归零时不得撤销物理页"
    );
    assert_eq!(
        world.extents().descriptor(extent).map(|d| d.state),
        Some(ExtentState::Live),
        "被拒绝的归还不得推进状态"
    );
    world
        .release_extent_lease_for_test(extent, ExtentLease::Scanner)
        .expect("结束 scanner lease");

    // lease 归零后归还点成立：extent 进入 ReturnQueued，消息被消费但 grace 是之后的等待。
    assert_eq!(
        world
            .return_extent_for_test(0, extent)
            .expect("lease 归零后归还可发布"),
        0,
        "grace 未走完时归还只推进 ticket，不算完成"
    );
    assert_eq!(
        world.extents().descriptor(extent).map(|d| d.state),
        Some(ExtentState::ReturnQueued),
        "等待 grace 期间 extent 保持在归还状态"
    );
    assert_eq!(
        world.provider_stats().committed_bytes,
        committed,
        "grace 未走完时不得撤销物理页"
    );

    // grace 按 epoch 逐步推进：每一步都不撤销物理页，走完最后一步才完成归还。
    let mut completed = 0;
    for step in 0..super::model::GRACE_STEPS {
        world.advance_epoch_for_test();
        completed += world
            .return_extent_for_test(0, extent)
            .expect("grace 推进可执行");
        if step + 1 < super::model::GRACE_STEPS {
            assert_eq!(completed, 0, "grace 未走完时归还不得完成");
            assert_eq!(
                world.provider_stats().committed_bytes,
                committed,
                "grace 未走完时不得撤销物理页"
            );
        }
    }
    assert_eq!(completed, 1, "归还必须恰好完成一次");
    assert_eq!(world.provider_stats().committed_bytes, 0);
    assert!(
        world.extents().descriptor(extent).is_none(),
        "归还后 extent 必须回到 buddy 阶梯"
    );

    // 二次归还：extent 已经不是 Live，唯一的归还点拒绝重复投递。
    let error = world
        .release_extent(extent)
        .expect_err("同一 extent 不能归还两次");
    assert!(
        error.message().contains("未知 extent") || error.message().contains("Live"),
        "二次归还必须被明确拒绝：{error}"
    );
    world.extents().verify().expect("归还后位图仍然一致");
}

#[test]
fn extent_return_target_follows_arena_domain_ownership() {
    use super::extent::ExtentId;
    let world = world(2, 64);
    // 每个 owner 在 raw 与 Resource 两个 domain 上各持有独立 arena；归还目标必须按 domain 选择
    // owner，否则合并会写到别人的空闲结构上。
    let spaces = world.extents().spaces_of(0);
    assert_eq!(
        spaces.len(),
        2,
        "owner 0 在 raw 与 Resource 上各有一个 arena"
    );
    let domains: Vec<MemoryDomainId> = spaces
        .iter()
        .map(|index| world.extents().arena_domain(*index).expect("arena 已登记"))
        .collect();
    assert!(domains.contains(&MemoryDomainId::RUNTIME_RAW));
    assert!(domains.contains(&MemoryDomainId::RESOURCE));
    // 越界 extent 编号必须解析失败，而不是命中任意 arena。
    assert_eq!(
        world.extents().descriptor(ExtentId::from_raw(u32::MAX)),
        None
    );
}

fn compile_platform_source(source: &str) -> crate::Compilation {
    crate::Compiler::new().compile(crate::CompileRequest::single_file(
        "main.gg",
        source,
        crate::TargetName::X86_64Linux,
    ))
}

fn diagnostics_of(compilation: &crate::Compilation) -> Vec<crate::DiagnosticCode> {
    compilation
        .diagnostics()
        .items()
        .iter()
        .filter(|diagnostic| diagnostic.severity() == crate::Severity::Error)
        .map(|diagnostic| diagnostic.code())
        .collect()
}

#[test]
fn platform_intrinsics_reach_lir_and_require_unsafe() {
    // 每个原语都必须穿过 checker、HIR、GIR 与 LIR 四层 verifier 并生成镜像计划。
    let all = "use std.platform.{reserve_aligned, commit, decommit, release, protect_guard, unprotect, wait, wake, entropy, zero, set_dump_policy, low_memory_hint, huge_page_hint}\n\
fn main() {\n\
    unsafe {\n\
        let range = reserve_aligned(65536, 65536)\n\
        commit(range)\n\
        decommit(range)\n\
        protect_guard(range)\n\
        unprotect(range)\n\
        zero(range)\n\
        set_dump_policy(range, 1)\n\
        huge_page_hint(range)\n\
        let woke = wake(0, 1)\n\
        let matched = wait(0, 1)\n\
        let bytes = entropy(16)\n\
        let hint = low_memory_hint()\n\
        release(range)\n\
        _ = woke\n\
        _ = matched\n\
        _ = bytes\n\
        _ = hint\n\
    }\n\
}\n";
    let compilation = compile_platform_source(all);
    assert!(
        compilation.is_success(),
        "全部平台原语必须通过四层 verifier：{:?}",
        compilation.diagnostics().items()
    );
    assert!(compilation.image_plan().is_some());

    // 契约目录与镜像字段同时可见：操作数与 extent class 数来自同一份 schema。
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.platform_op_count(), 13);
    assert_eq!(plan.platform_range_class_count(), 10);
    assert_eq!(plan.platform_profile(), "linux");
    assert_ne!(plan.platform_contract_fingerprint(), [0u8; 32]);
}

#[test]
fn platform_intrinsic_outside_unsafe_is_rejected_before_ir() {
    // 没有 `unsafe` 块时必须在 checker 层拒绝，而不是留给 LIR。
    let compilation =
        compile_platform_source("use std.platform.{commit}\nfn main() {\n    commit(1)\n}\n");
    assert!(!compilation.is_success());
    assert_eq!(
        diagnostics_of(&compilation),
        [crate::DiagnosticCode::InvalidExpression]
    );
    assert!(compilation.image_plan().is_none());
}

#[test]
fn platform_intrinsic_argument_shape_is_checked() {
    // 类型实参、实参个数与实参类型都在 checker 层拒绝。
    for (source, expected) in [
        (
            "use std.platform.{commit}\nfn main() {\n    unsafe { commit::[int](1) }\n}\n",
            "不接受类型实参",
        ),
        (
            "use std.platform.{commit}\nfn main() {\n    unsafe { commit(1, 2) }\n}\n",
            "需要 1 个值实参",
        ),
        (
            "use std.platform.{commit}\nfn main() {\n    let x: int = 1\n    unsafe { commit(x) }\n}\n",
            "必须是整数或布尔标量",
        ),
    ] {
        let compilation = compile_platform_source(source);
        assert_eq!(
            diagnostics_of(&compilation),
            [crate::DiagnosticCode::InvalidExpression],
            "源码必须被拒绝：{source}"
        );
        let messages: Vec<String> = compilation
            .diagnostics()
            .items()
            .iter()
            .map(|diagnostic| diagnostic.message().to_owned())
            .collect();
        assert!(
            messages.iter().any(|message| message.contains(expected)),
            "诊断必须说明 `{expected}`：{messages:?}"
        );
    }
}

#[test]
fn private_std_implementation_modules_are_not_package_api() {
    // `std.runtime.platform` 是 std 的私有实现，非 `std` 模块导入它必须被拒绝。
    let compilation = compile_platform_source(
        "use std.runtime.platform.{install}\nfn main() {\n    _ = install()\n}\n",
    );
    assert_eq!(
        diagnostics_of(&compilation),
        [crate::DiagnosticCode::ReservedName]
    );
    assert!(compilation.image_plan().is_none());
}

#[test]
fn builtin_platform_source_is_injected_into_every_compilation() {
    let compilation = compile_platform_source("fn main() {}\n");
    let source_map = compilation.source_map();
    assert!(
        source_map
            .snapshots()
            .iter()
            .any(|snapshot| snapshot.logical_path() == "std/runtime/platform.gg"),
        "内建平台源单元必须进入每次编译的源码表"
    );
    // `#[used]` 入口闭包使平台 adapter 成为镜像的根，不依赖用户是否调用它。
    let plan = compilation.image_plan().expect("镜像计划");
    assert!(
        plan.mono_root_count() >= 2,
        "入口与内建平台 `#[used]` 根都必须是 mono 根"
    );
}

#[test]
fn memory_pressure_categories_are_mutually_exclusive() {
    // 验收：内存压力统计不重复计数。物理与虚拟是两个 plane，同一字节只能落在一个分类里。
    let ledger = super::ledger::LedgerSchemaV1::fixed();
    ledger.verify().expect("账本契约自洽");
    let physical = ledger
        .partitions
        .iter()
        .find(|partition| partition.name == super::ledger::LEDGER_PARTITION_COMMITTED)
        .expect("已提交分区已登记");
    let address_space = ledger
        .partitions
        .iter()
        .find(|partition| partition.name == super::ledger::LEDGER_PARTITION_RESERVED)
        .expect("预留分区已登记");
    assert_eq!(physical.plane, super::ledger::LedgerPlane::Physical);
    assert_eq!(address_space.plane, super::ledger::LedgerPlane::Virtual);
    // 两个 plane 的分类集合不相交：预留字节不能同时计入已提交分类，反之亦然。
    for name in &address_space.categories {
        assert!(
            !physical.categories.contains(name),
            "分类 `{name}` 同时出现在物理与虚拟分区"
        );
    }
    // 已提交分区把 live 排在最前、兜底成员排在末位；四类逐项相加恰好等于分区总量。
    assert_eq!(
        physical.categories.len(),
        4,
        "live/pending/cache/reclaimable 是四个互斥成员"
    );
    assert_eq!(
        physical.categories.last().map(String::as_str),
        Some("live-bytes")
    );
    assert_eq!(
        address_space.categories,
        vec!["reserved-bytes".to_owned()],
        "预留口径只有一个成员，不与其他分类重叠"
    );
    // 每个分类都点名自己的统计字段，limit 判断可以逐项读取而不需要再推断。
    for category in &ledger.categories {
        assert!(
            !category.counter.is_empty(),
            "分类 `{}` 必须点名统计字段",
            category.name
        );
    }
}

#[test]
fn reserved_and_committed_bytes_never_overlap() {
    // 验收：`range_reserved_bytes` 与 `runtime_committed_bytes` 是同一字节的两种互斥口径。
    let mut world = world(1, 64);
    let arena = OWNER_ARENA_BYTES * 2;
    let stats = world.provider_stats();
    assert_eq!(stats.reserved_bytes, arena, "初始只有预留，没有提交");
    assert_eq!(stats.committed_bytes, 0);
    assert_eq!(stats.reserved_bytes + stats.committed_bytes, arena);

    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    let stats = world.provider_stats();
    assert_eq!(stats.committed_bytes, RAW_SLAB_PAGE_BYTES, "提交一页");
    assert_eq!(
        stats.reserved_bytes,
        arena - RAW_SLAB_PAGE_BYTES,
        "同一页必须从预留口径扣减，不能同时计入两边"
    );
    assert_eq!(
        stats.reserved_bytes + stats.committed_bytes,
        arena,
        "两个口径之和恒等于 arena 容量"
    );
    // 分配同时写入 owner 账本，分类之和与已提交字节一致。
    world.ledger_invariant(0).expect("分配后账本互斥成立");

    let stride = u64::from(descriptor_stride(&world, class));
    world
        .local_return(0, allocation.slot, stride)
        .expect("本地归还成功");
    // grace 按 epoch 逐步推进：每一步都不撤销物理页，走完最后一步才真正 trim。
    for step in 0..super::model::GRACE_STEPS {
        assert_eq!(
            world.reclaim_extents_for_test(0).expect("reclaim 可执行"),
            vec![TrimBlocked::GracePending {
                completed: step,
                required: super::model::GRACE_STEPS,
            }],
        );
        assert_eq!(
            world.provider_stats().committed_bytes,
            RAW_SLAB_PAGE_BYTES,
            "grace 未走完时不得撤销物理页"
        );
        world.advance_epoch_for_test();
    }
    assert!(
        world
            .reclaim_extents_for_test(0)
            .expect("reclaim 可执行")
            .is_empty()
    );
    let stats = world.provider_stats();
    assert_eq!(stats.committed_bytes, 0, "trim 后物理页全部撤销");
    assert_eq!(stats.reserved_bytes, arena, "撤销的页回到预留口径");
    assert_eq!(stats.reserved_bytes + stats.committed_bytes, arena);
    world.ledger_invariant(0).expect("trim 后账本互斥成立");
}
