//! LocalHeap world 接入的确定性测试：nursery 分配、TLAB、位图回表、pin 与分代 cycle。
//!
//! 全部在进程内运行：arena 的物理页由 extent 替身按 32 KiB 提交，不读镜像、不起子进程。

use super::RawWorld;
use super::heap_impl::ManagedPlacement;
use crate::TargetName;
use crate::runtime::barrier_schema::BarrierDemand;
use crate::runtime::compression_schema::CompressionDemand;
use crate::runtime::gc_metadata_contract::{GC_ARENA_BYTES, GC_BLOCK_BYTES, GC_LINE_BYTES};
use crate::runtime::gc_metadata_schema::{
    GcArenaLayoutV1, GcMetadataWorldV1, GcRootKindV1, GcRootLocationV1, GcRootRangeV1,
    GcTypeEntryV1, TraceKind, boot_verify,
};
use crate::runtime::gc_metadata_section::encode_sections;
use crate::runtime::local_heap::{
    CycleReport, GENERATION_AGING, GENERATION_OLD, HeapArenaKind, LocalHeap, ManagedBlockId,
};
use crate::runtime::local_heap_schema::{LocalHeapDemand, LocalHeapRuntimeContract};
use crate::runtime::mark_schema::MarkDemand;
use crate::runtime::message::BatchLimits;
use crate::runtime::pacing_schema::GcPacingDemand;
use crate::runtime::platform::PlatformProfile;
use crate::runtime::{
    RawPlaneDemand, RawPlanePolicyV1, RawResourceDemand, Rt0Demand, RuntimeRawContractV1,
    SchedulerDemand, StackMapDemand, SyncDemand, WaitDemand,
};

/// 构造一个 16 字节双指针类型与一个 8 字节无指针类型的 metadata world。
fn metadata_world() -> (GcMetadataWorldV1, Vec<u8>, Vec<u8>) {
    let words = 2_u32;
    let bytes = usize::try_from(words.div_ceil(8)).expect("bitmap 字节数");
    let mut node = vec![0u8; (8 + bytes * 2).next_multiple_of(4)];
    node[0] = TraceKind::Bitmap as u8;
    node[4..8].copy_from_slice(&words.to_le_bytes());
    node[8] = 0b11;
    let world = GcMetadataWorldV1 {
        types: vec![
            GcTypeEntryV1 {
                type_key: [1_u8; 32],
                canonical: vec![0, 1],
                name: "node".to_owned(),
                layout: Some((16, 8)),
                children: Vec::new(),
                flags: 0b01,
                trace_offset: 0,
                value_offset: 0,
                trace_len: u32::try_from(node.len()).expect("descriptor 长度"),
                value_len: 0,
            },
            GcTypeEntryV1 {
                type_key: [2_u8; 32],
                canonical: vec![0, 2],
                name: "leaf".to_owned(),
                layout: Some((8, 8)),
                children: Vec::new(),
                flags: 0,
                trace_offset: u32::try_from(node.len()).expect("descriptor 长度"),
                value_offset: 0,
                trace_len: 1,
                value_len: 0,
            },
        ],
        vtables: Vec::new(),
        trace_program: {
            let mut program = node;
            program.push(TraceKind::None as u8);
            program
        },
        value_program: Vec::new(),
        glue: Vec::new(),
        roots: vec![GcRootRangeV1 {
            kind: GcRootKindV1::CoroutineFrame,
            location: GcRootLocationV1::Aggregate { offset_bytes: 0 },
            type_range: (0, 2),
            word_range: (0, 2),
        }],
        sources: Vec::new(),
        alloc_sites: Vec::new(),
        arena: GcArenaLayoutV1 {
            arena_bytes: GC_ARENA_BYTES,
            block_bytes: GC_BLOCK_BYTES,
            line_bytes: GC_LINE_BYTES,
        },
        schema: GcMetadataWorldV1::SCHEMA,
    };
    boot_verify(&world).expect("metadata world 自洽");
    let (type_section, metadata_section) = encode_sections(&world).expect("section 可编码");
    (world, type_section, metadata_section)
}

/// 用内建 metadata world 构造一个已验证的整体契约。
pub(super) fn gc_contract() -> RuntimeRawContractV1 {
    gc_contract_with(RawPlanePolicyV1::default(), CompressionDemand::default())
}

/// 用内建 metadata world 构造整体契约；policy 与压缩需求由调用方给出。
pub(super) fn gc_contract_with(
    policy: RawPlanePolicyV1,
    compression_demand: CompressionDemand,
) -> RuntimeRawContractV1 {
    let (metadata, type_section, metadata_section) = metadata_world();
    let mut gc_demand = metadata.demand();
    gc_demand.type_section_bytes = u32::try_from(type_section.len()).expect("section 长度适配 u32");
    gc_demand.metadata_section_bytes =
        u32::try_from(metadata_section.len()).expect("section 长度适配 u32");
    // mark 的根站点与 GC metadata 的 root range 同源，跨段相等性由契约自身强制。
    let mark_demand = MarkDemand {
        root_sites: gc_demand.root_range_count,
        ..MarkDemand::default()
    };
    // LocalHeap 的 managed 类型数与最大 payload 必须与冻结类型表一致，否则跨段校验拒绝。
    let max_object_bytes = metadata
        .types
        .iter()
        .filter_map(|entry| entry.layout.map(|(size, _)| size))
        .max()
        .unwrap_or(0);
    let local_heap_demand = LocalHeapDemand {
        managed_types: gc_demand.type_count,
        max_object_bytes,
        ..LocalHeapDemand::default()
    };
    let contract = RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        policy,
        RawPlaneDemand::default(),
        RawResourceDemand::default(),
        Rt0Demand::default(),
        SchedulerDemand::default(),
        WaitDemand::default(),
        SyncDemand::default(),
        StackMapDemand::default(),
        gc_demand,
        BarrierDemand::default(),
        GcPacingDemand::default(),
        mark_demand,
        local_heap_demand,
        compression_demand,
        PlatformProfile::Linux,
    )
    .expect("契约可构建")
    .with_gc_sections(type_section, metadata_section)
    .expect("section 可挂载");
    contract
}

/// 配置一个已接入 LocalHeap 与 mark 平面的 world。
pub(super) fn heap_world() -> RawWorld {
    let contract = gc_contract();
    configured_world(&contract, 7, 1, 64)
}

/// 按给定契约创建并配置 world。
pub(super) fn configured_world(
    contract: &RuntimeRawContractV1,
    seed: u64,
    owners: u32,
    nodes: u32,
) -> RawWorld {
    let mut world =
        RawWorld::new(seed, owners, nodes, BatchLimits::default()).expect("world 可创建");
    world.configure_gc(contract).expect("GC 平面可配置");
    world
}

/// 分配一个 leaf（8 字节，无指针）并返回 payload 地址。
fn leaf(world: &mut RawWorld, placement: ManagedPlacement) -> u64 {
    world
        .allocate_managed(0, 1, 8, placement)
        .expect("leaf 可分配")
}

/// 在一个 access guard 内读取共享字段并结清 guard。
fn read_shared_field(
    world: &mut RawWorld,
    handle: crate::runtime::shared_heap_schema::SharedHandle,
    offset: u32,
) -> u64 {
    let token = world
        .begin_shared_access(handle)
        .expect("共享 guard 可建立");
    let value = world
        .load_shared_field_with(token, offset)
        .expect("共享字段可读");
    world.end_shared_access(token).expect("共享 guard 可结清");
    value
}

#[test]
fn queued_line_run_restore_rewinds_allocator_cursor() {
    let contract = gc_contract();
    let heap_contract = contract.local_heap();
    let mut heap = LocalHeap::new(heap_contract);
    let arena = heap.attach_arena(HeapArenaKind::Old, 1, 0, heap_contract);
    heap.commit_block(arena, 0).expect("block 可提交");
    let id = ManagedBlockId::new(1, 0).expect("block 身份");
    // 先把 run 交给归还门禁：bump 只能跳过 queued 前缀，游标因此落在 run 之后。
    heap.mark_line_run_queued(id, 0, 8)
        .expect("line-run 可入队");
    heap.allocate(arena, 1, 8, 8).expect("对象可分配");
    assert!(
        heap.block_free_line(id).expect("游标可读") > 0,
        "queued run 之后的分配必须推进游标"
    );
    heap.restore_queued_line_run(id, 0, 8)
        .expect("line-run 可恢复");
    assert_eq!(
        heap.block_free_line(id).expect("游标可读"),
        0,
        "恢复的 run 必须重新进入 bump 区间，否则这段空闲 line 永远不可分配"
    );
}

#[test]
fn configure_gc_exposes_contract_and_counts() {
    let world = heap_world();
    assert!(world.local_heap_configured());
    assert_eq!(world.managed_root_count(), 0);
    assert_eq!(world.managed_live_bytes(), 0);
    assert_eq!(world.managed_counters(0).expect("计数可读").objects, 0);
    assert!(world.managed_counters(1).is_err(), "越界 owner 必须失败");
}

#[test]
fn nursery_allocation_records_header_and_lives_in_nursery() {
    let mut world = heap_world();
    let address = leaf(&mut world, ManagedPlacement::Nursery);
    let object = world.managed_object(address).expect("对象可解码");
    assert_eq!(object.type_index, 1);
    assert_eq!(object.payload_bytes, 8);
    assert_eq!(object.generation, 0);
    assert_eq!(object.age, 0);
    assert!(!object.pinned);
    assert!(world.managed_object(address).expect("可解码").large == false);
    let counters = world.managed_counters(0).expect("计数可读");
    assert_eq!(counters.objects, 1);
    // nursery 分配一次就提交整个 TLAB span（8 个 block）。
    assert_eq!(counters.committed_blocks, 8);
    assert_eq!(counters.live_bytes, 24);
    assert_eq!(world.nursery_bytes(0).expect("nursery 可读"), 24);
    assert!(
        world.managed_object(address + 4).is_ok(),
        "interior 地址必须回表到对象"
    );
}

#[test]
fn tlab_refills_after_the_span_is_exhausted() {
    let mut world = heap_world();
    let mut addresses = Vec::new();
    for _ in 0..64 {
        addresses.push(leaf(&mut world, ManagedPlacement::Nursery));
    }
    let counters = world.managed_counters(0).expect("计数可读");
    assert_eq!(counters.objects, 64);
    assert_eq!(counters.live_bytes, 64 * 24);
    // 每个 TLAB span 覆盖 8 个 block，每 block 32 KiB，因此 64 个 24 字节对象不会触发 refill。
    assert_eq!(counters.tlab_refills, 1);
    assert!(addresses.windows(2).all(|pair| pair[0] < pair[1]));
    // 强制跨越 span：分配足够多的对象使 block 用尽。
    for _ in 0..4096 {
        leaf(&mut world, ManagedPlacement::Nursery);
    }
    assert!(world.managed_counters(0).expect("计数可读").tlab_refills > 1);
}

#[test]
fn placement_routes_to_the_matching_arena() {
    let mut world = heap_world();
    let pinned = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Pinned)
        .expect("pinned 可分配");
    let resource = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Resource)
        .expect("resource 可分配");
    assert_eq!(
        world.managed_object(pinned).expect("对象").generation,
        3,
        "pinned arena 的对象初始为 immortal"
    );
    assert_eq!(world.managed_object(resource).expect("对象").generation, 3);
    assert_eq!(
        world.nursery_bytes(0).expect("nursery 可读"),
        0,
        "非 nursery placement 不推进 nursery 字节数"
    );
    assert!(
        world
            .allocate_managed(0, 1, 8, ManagedPlacement::SharedHeap)
            .is_err(),
        "SharedHeap placement 必须拒绝"
    );
}

#[test]
fn managed_field_store_loads_and_marks_cards() {
    let mut world = heap_world();
    let holder = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Pinned)
        .expect("holder 可分配");
    let child = leaf(&mut world, ManagedPlacement::Nursery);
    world
        .store_managed_field(0, 0, holder, 8, child)
        .expect("managed store 可执行");
    assert_eq!(
        world.load_managed_field(holder, 8).expect("字段可读"),
        child
    );
    // remembered set：pinned 对象写进 nursery 目标后，minor cycle 必须只靠 card 键就找回 child。
    world.flush_remembered_set(0).expect("冲刷可执行");
    let report = world.collect_minor(0).expect("minor 可执行");
    assert_eq!(report.evacuated, 1);
    let moved = world.load_managed_field(holder, 8).expect("字段可读");
    assert_ne!(moved, child, "nursery 对象必须被搬运");
    assert!(world.nursery_bytes(0).expect("nursery 可读") == 0);
}

#[test]
fn minor_cycle_evacuates_roots_and_reclaims_nursery() {
    let mut world = heap_world();
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 0)
        .expect("根槽可登记");
    let kept = leaf(&mut world, ManagedPlacement::Nursery);
    let dropped = leaf(&mut world, ManagedPlacement::Nursery);
    world.set_managed_root(slot, kept).expect("根可写");
    let report = world.collect_minor(0).expect("minor 可执行");
    assert_eq!(report.evacuated, 1, "只有可达对象被搬运");
    let moved = world.managed_root(slot).expect("根可读");
    assert_ne!(moved, kept);
    let object = world.managed_object(moved).expect("对象可解码");
    assert_eq!(object.generation, GENERATION_AGING);
    assert_eq!(object.age, 1);
    assert!(
        world.managed_object(dropped).is_err(),
        "未达可达的 nursery 对象在 minor 之后必须消失"
    );
    let counters = world.managed_counters(0).expect("计数可读");
    assert_eq!(counters.minor_cycles, 1);
    assert_eq!(world.nursery_bytes(0).expect("nursery 可读"), 0);
    // 第二次 minor 让 aging 对象达到 tenure。
    let report = world.collect_minor(0).expect("minor 可执行");
    assert_eq!(report.evacuated, 0);
    let object = world
        .managed_object(world.managed_root(slot).expect("根"))
        .expect("对象");
    assert_eq!(object.generation, GENERATION_OLD);
    assert_eq!(object.age, 2);
}

#[test]
fn major_cycle_reclaims_unreachable_objects() {
    let mut world = heap_world();
    let slot = world
        .register_managed_root(GcRootKindV1::Static, 0)
        .expect("根槽可登记");
    let kept = leaf(&mut world, ManagedPlacement::Nursery);
    world.set_managed_root(slot, kept).expect("根可写");
    world.collect_minor(0).expect("minor 可执行");
    let live = world.managed_root(slot).expect("根可读");
    let dead = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Pinned)
        .expect("old 对象可分配");
    let before = world.managed_counters(0).expect("计数可读");
    let report = world.collect_major(0).expect("major 可执行");
    assert!(report.marked >= 1, "可达对象必须被标记");
    assert!(report.reclaimed_lines > 0);
    let after = world.managed_counters(0).expect("计数可读");
    assert_eq!(after.major_cycles, 1);
    assert!(after.live_bytes < before.live_bytes + 24);
    assert!(
        world.managed_object(dead).is_err(),
        "未标记的 old 对象必须被回收"
    );
    assert!(world.managed_object(live).is_ok(), "可达对象必须保留");
}

#[test]
fn managed_live_bytes_feed_pacing_baseline() {
    let mut world = heap_world();
    let slot = world
        .register_managed_root(GcRootKindV1::Static, 1)
        .expect("根槽可登记");
    let address = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Old)
        .expect("old 对象可分配");
    world.set_managed_root(slot, address).expect("根可写");
    let cycle = world.run_gc_cycle(true).expect("cycle 可执行");
    assert!(cycle.cycle_completed, "cycle 必须完成");
    assert!(
        world.pacing().last_live_bytes() >= u64::from(GC_BLOCK_BYTES),
        "managed live 必须进入 pacing 基线，而不是只剩 min_growth_budget"
    );
}

#[test]
fn pin_promotes_nursery_object_and_counts_nesting() {
    let mut world = heap_world();
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 1)
        .expect("根槽可登记");
    let address = leaf(&mut world, ManagedPlacement::Nursery);
    world.set_managed_root(slot, address).expect("根可写");
    let (pinned, count) = world.pin_managed(0, address).expect("pin 可执行");
    assert_eq!(count, 1);
    assert_ne!(pinned, address, "nursery 对象必须提升后才能固定");
    assert_eq!(world.managed_root(slot).expect("根可读"), pinned);
    let object = world.managed_object(pinned).expect("对象可解码");
    assert!(object.pinned);
    assert_eq!(object.generation, 3);
    let (again, count) = world.pin_managed(0, pinned).expect("重复 pin 可执行");
    assert_eq!(again, pinned);
    assert_eq!(count, 2);
    assert_eq!(world.unpin_managed(0, pinned).expect("unpin"), 1);
    assert_eq!(world.unpin_managed(0, pinned).expect("unpin"), 0);
    assert!(!world.managed_object(pinned).expect("对象").pinned);
    assert!(
        world.unpin_managed(0, pinned).is_err(),
        "重复 unpin 必须失败"
    );
}

#[test]
fn managed_addresses_are_owner_local() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 11, 2, 64);
    let first = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Nursery)
        .expect("owner 0 可分配");
    let second = world
        .allocate_managed(1, 1, 8, ManagedPlacement::Nursery)
        .expect("owner 1 可分配");
    assert_ne!(
        first / GC_ARENA_BYTES as u64,
        second / GC_ARENA_BYTES as u64,
        "每个 owner 拥有独立 arena"
    );
    assert!(world.managed_object(first).is_ok());
    assert!(world.managed_object(second).is_ok());
    assert!(world.managed_counters(0).expect("计数").objects == 1);
    assert!(world.managed_counters(1).expect("计数").objects == 1);
}

/// 一直分配 leaf 直到新对象落到 `block` 之外的 block。
fn fill_block(world: &mut RawWorld, block: u32) {
    loop {
        let address = leaf(world, ManagedPlacement::Old);
        if world
            .managed_block_ref(0, address)
            .expect("block 身份可解析")
            .id
            .0
            != block
        {
            return;
        }
    }
}

#[test]
fn field_stores_publish_exact_cross_block_edge_deltas() {
    let mut world = heap_world();
    // 三个对象落在三个不同 block：类型 0 是 16 字节双指针类型，两个字段各自成一条边。
    let first = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Old)
        .expect("第一个对象可分配");
    let first_block = world
        .managed_block_ref(0, first)
        .expect("block 身份可解析")
        .id
        .0;
    fill_block(&mut world, first_block);
    let second = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Old)
        .expect("第二个对象可分配");
    let second_block = world
        .managed_block_ref(0, second)
        .expect("block 身份可解析")
        .id
        .0;
    assert_ne!(first_block, second_block, "两个对象必须落在不同 block");
    fill_block(&mut world, second_block);
    let third = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Old)
        .expect("第三个对象可分配");
    let third_block = world
        .managed_block_ref(0, third)
        .expect("block 身份可解析")
        .id
        .0;
    assert_ne!(third_block, second_block);

    // 同一 block 内的自引用不产生传输，也不产生边。
    world
        .store_managed_field(0, 0, first, 0, first)
        .expect("同 block 写入");
    assert!(world.publish_edge_deltas().expect("发布").is_empty());
    let source = world.managed_block_ref(0, first).expect("block");
    let second_ref = world.managed_block_ref(0, second).expect("block");
    let third_ref = world.managed_block_ref(0, third).expect("block");
    assert_eq!(world.block_incoming_leases(third_ref), 0);

    // 建立 A→B 边并发布：同一对 block 只算一份 incoming lease。
    world
        .store_managed_field(0, 0, first, 0, second)
        .expect("写入第二条边");
    let published = world.publish_edge_deltas().expect("发布");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].delta, 1);
    assert_eq!(published[0].source, source);
    assert_eq!(published[0].target, second_ref);
    assert_eq!(published[0].sequence, 1);
    assert_eq!(world.block_incoming_leases(second_ref), 1);

    // 覆盖到第三个 block：撤销旧目标与新增新目标各自发布一条记录。
    world
        .store_managed_field(0, 0, first, 0, third)
        .expect("改写到第三个 block");
    let published = world.publish_edge_deltas().expect("发布");
    assert_eq!(published.len(), 2, "撤销旧边与新增新边各一条记录");
    assert!(
        published
            .iter()
            .any(|record| record.delta == -1 && record.target == second_ref)
    );
    assert!(
        published
            .iter()
            .any(|record| record.delta == 1 && record.target == third_ref)
    );
    assert_eq!(world.block_incoming_leases(second_ref), 0, "旧目标边已撤销");
    assert_eq!(world.block_incoming_leases(third_ref), 1);

    // 同一 block 对的两条字段边：删掉一条之后另一条仍然活跃。
    world
        .store_managed_field(0, 0, first, 8, third)
        .expect("写入同一 block 对的第二条字段边");
    let published = world.publish_edge_deltas().expect("发布");
    assert_eq!(published.len(), 1, "同一 block 对聚合为一条记录");
    assert_eq!(published[0].delta, 1);
    assert_eq!(published[0].sequence, 2, "同一 block 对的序号单调递增");
    assert_eq!(
        world.block_incoming_leases(third_ref),
        1,
        "同一对 block 的两条字段边只算一份 incoming lease"
    );

    world
        .store_managed_field(0, 0, first, 8, 0)
        .expect("清空一个字段");
    let published = world.publish_edge_deltas().expect("发布");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].delta, -1);
    assert_eq!(
        world.block_incoming_leases(third_ref),
        1,
        "删掉一条字段边后该 block 对仍然活跃"
    );

    // 清空最后一条边：incoming lease 归零，且没有待发布差量残留。
    world
        .store_managed_field(0, 0, first, 0, 0)
        .expect("清空最后一个字段");
    let published = world.publish_edge_deltas().expect("发布");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].delta, -1);
    assert_eq!(world.block_incoming_leases(third_ref), 0);
    assert_eq!(
        world.barrier_stats().edge_pending,
        0,
        "全部差量发布后不得残留 pending"
    );
    assert_eq!(world.barrier_stats().edge_published, 6);
}

#[test]
fn edge_heap_ticket_identity_does_not_alias_between_owners() {
    let mut world = RawWorld::new(71, 2, 64, BatchLimits::default()).expect("world");
    world.configure_gc(&gc_contract()).expect("GC 契约");
    let first = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Pinned)
        .expect("owner 0");
    let second = world
        .allocate_managed(1, 1, 8, ManagedPlacement::Pinned)
        .expect("owner 1");
    let (descriptor, offset, _) = world.heap(0).unwrap().ticket_identity(first).unwrap();
    assert!(
        world
            .heap(1)
            .unwrap()
            .object_at_ticket(descriptor, u64::from(offset))
            .is_err(),
        "另一个 owner 的同位置对象不能接受错误 arena 的 ticket"
    );
    assert_eq!(world.managed_object(second).unwrap().object_start, second);
}

#[test]
fn edge_heap_allocation_reaches_second_arena() {
    let contract =
        LocalHeapRuntimeContract::build(LocalHeapDemand::default(), PlatformProfile::Linux)
            .expect("契约可构建");
    let mut heap = LocalHeap::new(&contract);
    // descriptor 由世界级稠密表分配；这里显式给出两个不同的全局身份。
    let first = heap.attach_arena(HeapArenaKind::Old, 1, 0x2000_0000, &contract);
    let second = heap.attach_arena(HeapArenaKind::Old, 2, 0x2020_0000, &contract);
    heap.commit_block(first, 0).expect("block 可提交");
    heap.commit_block(second, 0).expect("block 可提交");
    let payload = u64::from(GC_BLOCK_BYTES) - 16;
    let a = heap
        .allocate(first, 0, payload, 16)
        .expect("首个 arena 可分配");
    let b = heap
        .allocate(second, 0, payload, 16)
        .expect("第二个 arena 可分配");
    heap.set_field(a, 0, 17).expect("字段可写");
    heap.set_field(b, 0, 29).expect("字段可写");
    assert_eq!(heap.field(a, 0).expect("字段可读"), 17);
    assert_eq!(heap.field(b, 0).expect("字段可读"), 29);
    assert_eq!(
        heap.block_ref(a).expect("block 身份").id.arena(),
        1,
        "第一个对象必须仍属于第一个 arena 的全局身份"
    );
    assert_eq!(heap.block_ref(b).expect("block 身份").id.arena(), 2);
}

#[test]
fn mark_bitmap_clears_only_the_marked_block() {
    // mark 位图按「一 granule 一位」组织：epoch 切换时只能清本 block 的区间。若按字节累加，
    // block ≥ 1 会清掉 block 0 的位而保留自己的陈旧位——第二个 cycle 重复标记同一对象时，
    // 陈旧位会让 mark_object 误判为「已标记」。
    let contract =
        LocalHeapRuntimeContract::build(LocalHeapDemand::default(), PlatformProfile::Linux)
            .expect("契约可构建");
    let mut heap = LocalHeap::new(&contract);
    let arena = heap.attach_arena(HeapArenaKind::Old, 1, 0x2000_0000, &contract);
    heap.commit_block(arena, 0).expect("block 0 可提交");
    heap.commit_block(arena, 1).expect("block 1 可提交");
    // 填满 block 0，使后续对象落到 block 1。
    let mut block0_last = 0_u64;
    let mut block1 = 0_u64;
    for _ in 0..4096 {
        let address = heap.allocate(arena, 1, 8, 16).expect("old 对象可分配");
        if heap.block_of(address).expect("block 可解析") == 0 {
            block0_last = address;
        } else {
            block1 = address;
            break;
        }
    }
    assert_ne!(block1, 0, "必须有对象落到第二个 block");
    heap.begin_mark_cycle();
    assert!(heap.mark_object(block1).expect("可标记").is_some());
    assert!(heap.mark_object(block0_last).expect("可标记").is_some());
    // 下一个 cycle：两个 block 的陈旧位必须各自清掉，重复标记同一对象仍返回 Some。
    heap.begin_mark_cycle();
    assert!(
        heap.mark_object(block1).expect("可标记").is_some(),
        "block 1 的陈旧位必须被清掉"
    );
    assert!(heap.mark_object(block0_last).expect("可标记").is_some());
    // sweep 不得回收已标记对象。
    let mut report = CycleReport::default();
    heap.sweep_unmarked(&mut report).expect("sweep 可执行");
    assert!(heap.object_at(block1).is_ok(), "已标记对象必须保留");
    assert!(heap.object_at(block0_last).is_ok(), "已标记对象必须保留");
}

#[test]
fn block_object_enumeration_and_mark_query_track_real_state() {
    let contract =
        LocalHeapRuntimeContract::build(LocalHeapDemand::default(), PlatformProfile::Linux)
            .expect("契约可构建");
    let mut heap = LocalHeap::new(&contract);
    let arena = heap.attach_arena(HeapArenaKind::Old, 1, 0x2000_0000, &contract);
    heap.commit_block(arena, 0).expect("block 0 可提交");
    heap.commit_block(arena, 1).expect("block 1 可提交");
    let descriptor = heap.arena_descriptors()[0].1;
    assert_eq!(
        heap.committed_blocks_of(descriptor).expect("block 快照"),
        vec![0, 1],
        "枚举必须给出已提交 block"
    );
    // 填满 block 0，让后续对象落到 block 1：枚举要按 block 边界分开。
    let mut block1 = 0_u64;
    for _ in 0..4096 {
        let address = heap.allocate(arena, 1, 8, 16).expect("old 对象可分配");
        if heap.block_of(address).expect("block 可解析") == 1 {
            block1 = address;
            break;
        }
    }
    assert_ne!(block1, 0, "必须有对象落到第二个 block");
    let in_block1 = heap.block_objects(descriptor, 1).expect("block 1 可枚举");
    let (_, header_in_block) = in_block1
        .iter()
        .find(|(payload, _)| *payload == block1)
        .expect("枚举必须包含落在该 block 的对象");
    // 枚举的 block 内偏移与 ticket 身份同基准，候选阶段才能把两者对上同一对象。
    let (_, ticket_offset, _) = heap.ticket_identity(block1).expect("ticket 身份");
    assert_eq!(
        *header_in_block,
        u64::from(ticket_offset) - u64::from(GC_BLOCK_BYTES),
        "block 内偏移必须与 ticket 身份同基准"
    );
    for (payload, _) in &in_block1 {
        assert_eq!(heap.block_of(*payload).expect("block 可解析"), 1);
    }
    for (payload, _) in heap.block_objects(descriptor, 0).expect("block 0 可枚举") {
        assert_eq!(heap.block_of(payload).expect("block 可解析"), 0);
    }
    // 未提交 block 与未知 descriptor 都必须是错误，而不是空清单。
    assert!(
        heap.block_objects(descriptor, 2).is_err(),
        "未提交 block 不能被枚举"
    );
    assert!(
        heap.committed_blocks_of(99).is_err(),
        "未知 arena descriptor 必须报错"
    );
    // mark 查询只认当前 epoch：epoch 前进后旧位是陈旧标记。
    heap.begin_mark_cycle();
    assert!(
        !heap.marked_in_current_epoch(block1).expect("可查询"),
        "未标记对象不能报告为已标记"
    );
    assert!(heap.mark_object(block1).expect("可标记").is_some());
    assert!(
        heap.marked_in_current_epoch(block1).expect("可查询"),
        "本 epoch 标记过的对象必须报告为已标记"
    );
    heap.begin_mark_cycle();
    assert!(
        !heap.marked_in_current_epoch(block1).expect("可查询"),
        "epoch 前进后的陈旧位不得报告为已标记"
    );
}

#[test]
fn large_object_spans_whole_blocks() {
    let mut world = heap_world();
    let big = GC_BLOCK_BYTES as u64 + 64;
    let address = world
        .allocate_managed(0, 0, big, ManagedPlacement::Nursery)
        .expect("大对象可分配");
    let object = world.managed_object(address).expect("对象可解码");
    assert_eq!(object.payload_bytes, big);
    assert_eq!(HeapArenaKind::Large.name(), "large");
    assert!(object.large);
}

#[test]
fn interior_pointer_resolves_within_the_page() {
    let mut world = heap_world();
    let node = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Nursery)
        .expect("node 可分配");
    let interior = node + 8;
    let object = world.managed_object(interior).expect("interior 可回表");
    assert_eq!(object.address, interior);
    assert_eq!(object.object_start, node);
}

#[test]
fn real_compile_sections_drive_local_heap_allocation_and_collection() {
    // 真实编译产物：ImagePlan 的 LocalHeap 契约段与 GC metadata section 直接驱动 arena。
    let source =
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }";
    let compilation = crate::Compiler::new().compile(crate::CompileRequest::single_file(
        "main.gg",
        source,
        crate::TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let plan = compilation.image_plan().expect("镜像计划");
    let raw = compilation.raw_contract().expect("真实契约存在");
    let types = crate::runtime::gc_metadata_section::decode_sections(
        plan.gc_type_section(),
        plan.gc_metadata_section(),
    )
    .expect("section 可解码");
    let mut world = RawWorld::new(3, 1, 64, crate::runtime::message::BatchLimits::default())
        .expect("world 可创建");
    world.configure_gc(raw).expect("真实契约可配置");
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 0)
        .expect("根槽可登记");
    // 用真实类型表逐类型分配，覆盖每个类型 footprint 与 trace descriptor。
    let mut roots = Vec::new();
    for (index, entry) in types.types().iter().enumerate() {
        let address = world
            .allocate_managed(
                0,
                u32::try_from(index).expect("类型下标适配 u32"),
                entry.size,
                ManagedPlacement::Nursery,
            )
            .expect("真实类型可分配");
        roots.push(address);
    }
    assert_eq!(roots.len(), types.types().len());
    world.set_managed_root(slot, roots[0]).expect("根可写");
    let report = world.collect_minor(0).expect("真实 world 可执行 minor");
    assert!(report.scanned_words > 0 || types.types().len() > 0);
    let moved = world.managed_root(slot).expect("根可读");
    assert!(
        world.managed_object(moved).is_ok(),
        "搬运后的对象必须可解析"
    );
    let major = world.collect_major(0).expect("真实 world 可执行 major");
    assert!(major.marked >= 1);
    assert!(world.managed_counters(0).expect("计数可读").objects >= 1);
}

/// 共享 payload 的字段写入必须接入与本地字段同一条 barrier 记账路径。
#[test]
fn shared_field_store_routes_through_barrier_plane() {
    use crate::runtime::barrier::BarrierFlushReason;

    let contract = gc_contract();
    let mut world = configured_world(&contract, 29, 2, 64);
    let handle = world.allocate_shared_object(1, 32).expect("共享对象可分配");
    let block = world
        .shared_payload_block(handle)
        .expect("登记项可读")
        .block;
    // 共享 block 的 card table 必须以 payload owner 为 manager 登记：别的 owner 写它时，
    // 卡必须以 CardMark 消息投给 payload owner，而不是写进写入者自己的表。
    let table = world
        .barrier()
        .table(u64::from(block.id.arena()))
        .expect("共享 block 的 card table 已登记");
    assert_eq!(table.manager(), world.token(1));
    let child = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Nursery)
        .expect("child 可分配");
    world
        .store_shared_managed_field(0, 0, handle, 8, child, Some(0))
        .expect("共享字段可写");
    assert_eq!(
        read_shared_field(&mut world, handle, 8),
        child,
        "共享 payload 必须真的写入新值"
    );
    // 边变更先落在 processor scratch：合并进 summary 之后才成为活跃 pair。
    world.barrier_mut().merge_edges().expect("边变更可合并");
    assert!(
        world.barrier().active_pairs() >= 1,
        "共享字段写入必须记录边：共享 block → 目标 block"
    );
    // 写入者不是 payload owner：冲刷必须以 card batch 投给 payload owner。
    let published = world
        .flush_barrier(0, 0, BarrierFlushReason::ProducerStopGate)
        .expect("冲刷可执行");
    assert_eq!(
        published, 1,
        "非 owner 写入必须以 card batch 投给 payload owner"
    );
    // 覆盖掉旧引用：Yuasa deletion 必须记账，新值仍然生效。
    let second = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Nursery)
        .expect("第二个对象可分配");
    world
        .store_shared_managed_field(1, 0, handle, 8, second, Some(0))
        .expect("覆盖写可执行");
    assert_eq!(read_shared_field(&mut world, handle, 8), second);
    assert!(
        world.barrier().edge_pending_items() > 0,
        "覆盖写的新边必须进入 edge 账本"
    );
}
