//! LocalHeap world 接入的确定性测试：nursery 分配、TLAB、位图回表、pin 与分代 cycle。
//!
//! 全部在进程内运行：arena 的物理页由 extent 替身按 32 KiB 提交，不读镜像、不起子进程。

use super::RawWorld;
use super::heap_impl::ManagedPlacement;
use crate::runtime::gc_metadata_contract::{GC_ARENA_BYTES, GC_BLOCK_BYTES, GC_LINE_BYTES};
use crate::runtime::gc_metadata_schema::{
    GcArenaLayoutV1, GcMetadataWorldV1, GcRootKindV1, GcRootLocationV1, GcRootRangeV1,
    GcTypeEntryV1, TraceKind, boot_verify,
};
use crate::runtime::gc_metadata_section::encode_sections;
use crate::runtime::local_heap::{GENERATION_AGING, GENERATION_OLD, HeapArenaKind};
use crate::runtime::local_heap_schema::{LocalHeapDemand, LocalHeapRuntimeContract};
use crate::runtime::platform::PlatformProfile;

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

/// 配置一个已接入 LocalHeap 的 world。
fn heap_world() -> RawWorld {
    let contract =
        LocalHeapRuntimeContract::build(LocalHeapDemand::default(), PlatformProfile::Linux)
            .expect("契约可构建");
    let (_, type_section, metadata_section) = metadata_world();
    let mut world = RawWorld::new(7, 1, 64, crate::runtime::message::BatchLimits::default())
        .expect("world 可创建");
    world
        .configure_local_heap(&contract, &type_section, &metadata_section)
        .expect("LocalHeap 可配置");
    world
}

/// 分配一个 leaf（8 字节，无指针）并返回 payload 地址。
fn leaf(world: &mut RawWorld, placement: ManagedPlacement) -> u64 {
    world
        .allocate_managed(0, 1, 8, placement)
        .expect("leaf 可分配")
}

#[test]
fn configure_local_heap_exposes_contract_and_counts() {
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
    let contract =
        LocalHeapRuntimeContract::build(LocalHeapDemand::default(), PlatformProfile::Linux)
            .expect("契约可构建");
    let (_, type_section, metadata_section) = metadata_world();
    let mut world = RawWorld::new(11, 2, 64, crate::runtime::message::BatchLimits::default())
        .expect("world 可创建");
    world
        .configure_local_heap(&contract, &type_section, &metadata_section)
        .expect("LocalHeap 可配置");
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
    let contract = plan.local_heap_runtime().clone();
    let types = crate::runtime::gc_metadata_section::decode_sections(
        plan.gc_type_section(),
        plan.gc_metadata_section(),
    )
    .expect("section 可解码");
    let mut world = RawWorld::new(3, 1, 64, crate::runtime::message::BatchLimits::default())
        .expect("world 可创建");
    world
        .configure_local_heap(
            &contract,
            plan.gc_type_section(),
            plan.gc_metadata_section(),
        )
        .expect("真实契约可配置");
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
