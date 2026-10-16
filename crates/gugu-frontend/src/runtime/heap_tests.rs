//! LocalHeap 契约段与 Immix arena 布局的确定性回归。
//!
//! 只消费进程内的契约对象与推导器，不读镜像、不启动子进程、不做重负载分配。

use super::barrier_schema::CARD_GRANULARITY_BYTES;
use super::gc_metadata_contract::{GC_ARENA_BYTES, GC_BLOCK_BYTES, GC_LINE_BYTES};
use super::local_heap::HEADER_ENCODING_MIRROR;
use super::local_heap_schema::{
    HEAP_BLOCKS_PER_ARENA, HEAP_GRANULE_BYTES, HEAP_LINES_PER_BLOCK, HEAP_OBJECT_HEADER_BYTES,
    HEAP_TLAB_SPAN_BLOCKS, HeapTriggerProfile, LOCAL_HEAP_SCHEMA, LocalHeapDemand,
    LocalHeapRuntimeContract,
};
use super::platform::PlatformProfile;
use super::tests::parse_uint_literal;
use crate::TargetName;

fn contract() -> LocalHeapRuntimeContract {
    LocalHeapRuntimeContract::build(LocalHeapDemand::default(), PlatformProfile::Linux)
        .expect("默认需求可构建 LocalHeap 契约")
}

#[test]
fn local_heap_contract_derives_immix_sizes_from_the_gc_arena() {
    let contract = contract();
    assert_eq!(contract.schema(), LOCAL_HEAP_SCHEMA);
    assert_eq!(contract.arena_bytes(), GC_ARENA_BYTES);
    assert_eq!(contract.block_bytes(), GC_BLOCK_BYTES);
    assert_eq!(contract.line_bytes(), GC_LINE_BYTES);
    // block/line 参数与 GC metadata 契约同源，派生规模由同一组参数唯一确定。
    assert_eq!(contract.blocks_per_arena, HEAP_BLOCKS_PER_ARENA);
    assert_eq!(contract.lines_per_block, HEAP_LINES_PER_BLOCK);
    assert_eq!(
        contract.tlab_span_bytes(),
        u64::from(HEAP_TLAB_SPAN_BLOCKS) * u64::from(GC_BLOCK_BYTES)
    );
    assert_eq!(
        contract.object_start_bits,
        u32::try_from(GC_ARENA_BYTES / u64::from(HEAP_GRANULE_BYTES)).expect("granule 位数")
    );
    assert_eq!(contract.mark_bits, contract.object_start_bits);
    assert_eq!(contract.bitmap_bytes, contract.object_start_bits / 8);
    assert_eq!(contract.page_cover_entries, 512);
    assert_eq!(contract.card_bytes, 4096);
    assert_eq!(contract.card_bytes % contract.blocks_per_arena, 0);
    // 记录规格 5 条：ObjectHeader 2 + HeapArenaMetadata 11 + HeapPinEntry 4 + HeapBlockRecord 13
    // + LargeMappingRecord 5 = 35 个字段；数字变化必须同时改这里与 `HEAP_RECORD_SPECS`。
    assert_eq!(contract.record_field_count(), 35);
    // arena 内每 granule 一位，且 page-cover 覆盖整 arena。
    assert_eq!(
        u64::from(contract.object_start_bits) * u64::from(HEAP_GRANULE_BYTES),
        GC_ARENA_BYTES
    );
    assert_eq!(
        u64::from(contract.page_cover_entries) * u64::from(contract.page_bytes),
        GC_ARENA_BYTES
    );
    assert_eq!(
        u64::from(contract.card_bytes) * u64::from(CARD_GRANULARITY_BYTES),
        GC_ARENA_BYTES
    );
}

#[test]
fn local_heap_trigger_profile_bounds_nursery_growth() {
    let contract = contract();
    let trigger = contract.trigger();
    assert_eq!(trigger.revision, 1);
    assert_eq!(trigger.minor_trigger_bytes, 256 * 1024);
    assert_eq!(trigger.tenure_age, 2);
    assert_eq!(trigger.max_age, 15);
    assert!(trigger.minor_trigger_bytes <= GC_ARENA_BYTES / 2);
}

#[test]
fn local_heap_contract_is_deterministic_across_builds() {
    let first = contract();
    let second = contract();
    assert_eq!(first.fingerprint, second.fingerprint);
    assert_eq!(first.canonical_bytes(), second.canonical_bytes());
    assert_eq!(first.dump(), second.dump());
    assert_ne!(first.fingerprint, [0_u8; 32]);
}

#[test]
fn local_heap_contract_dump_reports_records_bitmaps_and_trigger() {
    let dump = contract().dump();
    assert!(dump.contains("local-heap schema=5 arena=2097152 block=32768 line=128"));
    assert!(dump.contains("tlab-span=262144 blocks=64 lines-per-block=256"));
    assert!(dump.contains("local-heap-bitmaps object-start-bits=131072 mark-bits=131072"));
    assert!(dump.contains("page-cover=512 cards=4096"));
    assert!(
        dump.contains("local-heap-arena-states free,nursery,aging,old,resource,pinned,evacuating")
    );
    assert!(dump.contains("local-heap-generations nursery,aging,old,immortal"));
    assert!(dump.contains(
        "local-heap-representations local-direct,turn-region,shared-handle,compressed-ref"
    ));
    assert!(dump.contains("local-heap-object-flags forwarded,pinned,release-queued"));
    assert!(dump.contains(
        "local-heap-minor-phases stop-mutator,flush-remembered-set,copy-nursery,promote-aged,rebuild-summary,resume"
    ));
    assert!(dump.contains("local-heap-major-phases snapshot-roots,mark,remark,select-evacuation"));
    assert!(dump.contains("local-heap-record ObjectHeader bytes=16 align=8 fields=control@0"));
    assert!(dump.contains(
        "local-heap-record HeapArenaMetadata bytes=55576 align=8 fields=state@0,generation@4"
    ));
    assert!(dump.contains("mark@16408"));
    assert!(dump.contains("local-heap-record HeapPinEntry bytes=16 align=8 fields=arena@0"));
    assert!(dump.contains(
        "local-heap-record HeapBlockRecord bytes=64 align=64 fields=block_id@0,generation@4"
    ));
    assert!(dump.contains(
        "local-heap-block-states allocating,candidate,sweeping,evacuating,return-pending,owned-free,free"
    ));
    assert!(dump.contains("local-heap-trigger revision=1 minor-trigger-bytes=262144"));
    assert!(dump.contains("local-heap-fingerprint"));
}

#[test]
fn object_header_layout_keeps_payload_size_next_to_control() {
    let contract = contract();
    let header = &contract.records[0];
    assert_eq!(header.name, "ObjectHeader");
    assert_eq!(header.bytes, HEAP_OBJECT_HEADER_BYTES);
    assert_eq!(header.fields[0].name, "control");
    assert_eq!(header.fields[0].offset, 0);
    assert_eq!(header.fields[1].name, "payload_size_or_forward");
    assert_eq!(header.fields[1].offset, 8);
}

#[test]
fn arena_metadata_layout_places_bitmaps_before_page_cover() {
    let contract = contract();
    let arena = &contract.records[1];
    assert_eq!(arena.name, "HeapArenaMetadata");
    let offset = |name: &str| {
        arena
            .fields
            .iter()
            .find(|field| field.name == name)
            .expect("字段已登记")
            .offset
    };
    assert_eq!(offset("state"), 0);
    assert_eq!(offset("generation"), 4);
    assert_eq!(offset("mark_epoch"), 8);
    assert_eq!(offset("alloc_cursor"), 16);
    assert_eq!(offset("pin_count"), 20);
    assert_eq!(offset("object_start"), 24);
    assert_eq!(
        offset("object_start") + contract.bitmap_bytes,
        offset("mark")
    );
    assert_eq!(offset("mark") + contract.bitmap_bytes, offset("page_cover"));
    assert_eq!(
        offset("page_cover") + contract.page_cover_entries * 4,
        offset("cards")
    );
    assert_eq!(offset("cards") + contract.card_bytes, offset("block_live"));
    assert_eq!(
        offset("block_live") + contract.blocks_per_arena * 4,
        offset("line_live")
    );
    assert_eq!(
        offset("line_live") + contract.blocks_per_arena * contract.lines_per_block,
        arena.bytes
    );
}

#[test]
fn local_heap_contract_rejects_tampered_arena_and_tlab() {
    let mut tampered = contract();
    tampered.arena_bytes = GC_ARENA_BYTES / 2;
    tampered.fingerprint = tampered.compute_fingerprint();
    assert!(tampered.verify().is_err(), "arena 与登记常量不一致必须拒绝");

    let mut tampered = contract();
    tampered.tlab_span_blocks = HEAP_BLOCKS_PER_ARENA + 1;
    tampered.fingerprint = tampered.compute_fingerprint();
    assert!(tampered.verify().is_err(), "TLAB span 超过 arena 必须拒绝");

    let mut tampered = contract();
    tampered.bitmap_bytes = contract().bitmap_bytes - 8;
    tampered.fingerprint = tampered.compute_fingerprint();
    assert!(
        tampered.verify().is_err(),
        "位图字节数与 granule 位数不符必须拒绝"
    );
}

#[test]
fn local_heap_contract_rejects_tampered_record_and_directory() {
    let mut tampered = contract();
    tampered.records[1].fields[5].offset += 8;
    tampered.fingerprint = tampered.compute_fingerprint();
    assert!(tampered.verify().is_err(), "字段偏移漂移必须拒绝");

    let mut tampered = contract();
    tampered.generations[3] = "pinned".to_owned();
    tampered.fingerprint = tampered.compute_fingerprint();
    assert!(tampered.verify().is_err(), "generation 目录漂移必须拒绝");

    let mut tampered = contract();
    tampered.fingerprint = [7; 32];
    assert!(tampered.verify().is_err(), "指纹与内容不符必须拒绝");
}

#[test]
fn local_heap_trigger_profile_rejects_impossible_ages() {
    // 逐项推翻默认值：先把 tenure_age 收到 0，再逐步越过其余上界。
    let mut trigger = HeapTriggerProfile {
        tenure_age: 0,
        ..HeapTriggerProfile::default()
    };
    assert!(trigger.verify(GC_ARENA_BYTES).is_err());
    trigger.tenure_age = 3;
    trigger.max_age = 2;
    assert!(trigger.verify(GC_ARENA_BYTES).is_err());
    trigger.max_age = 16;
    assert!(trigger.verify(GC_ARENA_BYTES).is_err());
    trigger.max_age = 15;
    assert!(trigger.verify(GC_ARENA_BYTES).is_ok());
    trigger.minor_trigger_bytes = GC_ARENA_BYTES;
    assert!(trigger.verify(GC_ARENA_BYTES).is_err());
}

#[test]
fn local_heap_demand_rejects_impossible_placement_and_large_counts() {
    let manifest = LocalHeapDemand {
        alloc_sites: 3,
        pinned_sites: 1,
        resource_sites: 1,
        promote_sites: 1,
        pin_sites: 1,
        unpin_sites: 1,
        barrier_sites: 2,
        managed_types: 4,
        large_types: 1,
        max_object_bytes: u64::from(GC_BLOCK_BYTES),
    };
    assert!(manifest.verify(GC_BLOCK_BYTES).is_ok());
    let mut oversized = manifest;
    oversized.alloc_sites = 1;
    assert!(oversized.verify(GC_BLOCK_BYTES).is_err());
    let mut wrong_large = manifest;
    wrong_large.large_types = 0;
    assert!(wrong_large.verify(GC_BLOCK_BYTES).is_err());
    let mut small_type = manifest;
    small_type.max_object_bytes = 8;
    assert!(small_type.verify(GC_BLOCK_BYTES).is_err());
    let mut too_many = manifest;
    too_many.large_types = 5;
    assert!(too_many.verify(GC_BLOCK_BYTES).is_err());
    assert_ne!(
        manifest.fingerprint(),
        LocalHeapDemand::default().fingerprint()
    );
}

#[test]
fn local_heap_contract_uses_the_platform_page_size() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let profile = PlatformProfile::from(target);
        let contract = LocalHeapRuntimeContract::build(LocalHeapDemand::default(), profile)
            .expect("契约可构建");
        assert_eq!(
            u64::from(contract.page_bytes),
            profile.constants().page_bytes,
            "page-cover 粒度必须跟随平台页大小"
        );
    }
}

/// `heap.gg` 的对象头编码必须与 compiler 侧定义逐项一致。
///
/// 两处并存期间任何位级漂移都会让 Gugu 分配器/收集器写出的 `control` 字被 Rust 侧误读
/// （反向亦然）：类型下标、generation、representation 的错位既会漏扫根，也会把标量当指针。
/// 因此这些常量不允许各自漂移，只能逐项相等。
#[test]
fn heap_source_mirrors_the_object_header_encoding() {
    for (name, expected) in HEADER_ENCODING_MIRROR {
        let prefix = format!("pub const {name}: uint = ");
        let literal = super::RUNTIME_HEAP_SOURCE
            .lines()
            .find_map(|line| line.trim().strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("heap.gg 缺少常量 {name}"));
        let parsed = parse_uint_literal(literal)
            .unwrap_or_else(|| panic!("heap.gg 的 {name} 不是可解析的无符号字面量：{literal}"));
        assert_eq!(
            parsed, expected,
            "heap.gg 的 {name} 与 compiler 常量不一致（结果会是对象头误读）"
        );
    }
    // 位域必须互不重叠：相邻字段的掩码错位会让写方与读方各自解释同一批位。
    let field = |name: &str| {
        HEADER_ENCODING_MIRROR
            .iter()
            .find(|(field, _)| *field == name)
            .unwrap_or_else(|| panic!("表里有 {name}"))
            .1
    };
    let age = field("CONTROL_AGE_MASK");
    let generation = field("CONTROL_GENERATION_MASK");
    assert_eq!(age & generation, 0, "age 与 generation 位域不得重叠");
}

/// processor 页的机器侧槽位必须与 `processor.rs` 的固定偏移一致。
///
/// 屏障状态页指针、栈扫描 epoch 与 arena 基址都按这些常量读写：错位会写到相邻字段上，
/// 运行期不会报错，只会读到无效地址或让 grey 判断反转成丢根。
#[test]
fn heap_source_mirrors_the_processor_slots() {
    let expected = [
        (
            "PROCESSOR_EDGE_SCRATCH_OFFSET",
            super::processor::PROCESSOR_EDGE_SCRATCH_OFFSET,
        ),
        (
            "PROCESSOR_BARRIER_SCRATCH_OFFSET",
            super::processor::PROCESSOR_BARRIER_SCRATCH_OFFSET,
        ),
        (
            "PROCESSOR_STACK_EPOCH_OFFSET",
            super::processor::PROCESSOR_STACK_EPOCH_OFFSET,
        ),
        (
            "PROCESSOR_ARENA_OFFSET",
            super::processor::PROCESSOR_ARENA_OFFSET,
        ),
        (
            "PROCESSOR_MARK_ACTIVE_OFFSET",
            super::processor::PROCESSOR_MARK_ACTIVE_OFFSET,
        ),
        (
            "PROCESSOR_TRACE_CURSOR_OFFSET",
            super::processor::PROCESSOR_TRACE_CURSOR_OFFSET,
        ),
    ];
    for (name, want) in expected {
        assert_eq!(
            heap_source_constant(name),
            u64::try_from(want).expect("槽位偏移适配 u64"),
            "heap.gg 的 {name} 与 processor 布局不一致"
        );
    }
    // edge staging 页必须放得下 schema 容量规定的记录区。
    let contract = super::barrier_schema::BarrierRuntimeContract::build(Default::default())
        .expect("屏障契约可构建");
    let records = u64::from(contract.edge_buffer_entries)
        * heap_source_constant("EDGE_RECORD_BYTES")
        + heap_source_constant("EDGE_RECORDS_OFFSET");
    assert!(
        heap_source_constant("EDGE_SCRATCH_BYTES") >= records,
        "edge staging 页容不下 schema 规定的记录数"
    );
}

/// 读取 `heap.gg` 里的无符号常量。
fn heap_source_constant(name: &str) -> u64 {
    super::tests::source_uint_constant(super::RUNTIME_HEAP_SOURCE, name)
}

/// arena 侧元数据布局必须与 schema 几何推出的值逐项一致。
///
/// 分配器按这些常量写 object_start / block_live / line_live，偏移或尺寸漂移会让账本落到
/// 别的字段上，而运行期不会报任何错——收集器只会看到一批宣称空闲或已死的数据。
#[test]
fn heap_source_mirrors_the_arena_side_metadata_layout() {
    let contract = LocalHeapRuntimeContract::build(
        LocalHeapDemand::default(),
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    let blocks = u64::from(contract.blocks_per_arena);
    let lines = u64::from(super::local_heap_schema::HEAP_LINES_PER_BLOCK);
    // 状态头 24 字节：state/generation 各 4、mark_epoch 8、alloc_cursor/pin_count 各 4。
    let side = 24
        + 2 * u64::from(contract.bitmap_bytes)
        + u64::from(contract.page_cover_entries) * 4
        + u64::from(contract.card_bytes)
        + blocks * 4
        + blocks * lines;
    let expected = [
        ("ARENA_SIDE_METADATA_BYTES", side),
        ("ARENA_STATE_OFFSET", 0),
        ("ARENA_GENERATION_OFFSET", 4),
        ("ARENA_MARK_EPOCH_OFFSET", 8),
        ("ARENA_MARK_WORDS", u64::from(contract.bitmap_bytes) / 8),
        (
            "BLOCK_RECORD_BYTES",
            std::mem::size_of::<super::local_heap_schema::HeapBlockRecord>() as u64,
        ),
        ("ARENA_MARK_OFFSET", 24 + u64::from(contract.bitmap_bytes)),
        (
            "ARENA_PAGE_COVER_OFFSET",
            24 + 2 * u64::from(contract.bitmap_bytes),
        ),
        ("ARENA_OBJECT_START_OFFSET", 24),
        // 区域顺序：状态头 24 / object_start / mark / page_cover / cards / block_live / line_live。
        (
            "ARENA_CARDS_OFFSET",
            side - u64::from(contract.card_bytes) - blocks * 4 - blocks * lines,
        ),
        (
            "ARENA_BLOCK_LIVE_OFFSET",
            side - blocks * lines - blocks * 4,
        ),
        ("ARENA_LINE_LIVE_OFFSET", side - blocks * lines),
        ("ARENA_NO_BLOCK", u64::from(u32::MAX)),
        ("BLOCK_STATE_ALLOCATING", 0),
    ];
    for (name, want) in expected {
        assert_eq!(
            heap_source_constant(name),
            want,
            "heap.gg 的 {name} 与 schema 几何推出的值不一致"
        );
    }
    // 头 16 字节是对象布局的前提：payload 地址减 16 是 control 字。
    assert_eq!(super::local_heap_schema::HEAP_OBJECT_HEADER_BYTES, 16);
}

/// 大对象登记表必须位于块记录之后，且映射粒度与平台页相同；偏移错误会把首次大对象写入
/// block record 或把跨页范围交给未提交的尾部。
#[test]
fn heap_source_mirrors_large_mapping_layout() {
    use super::local_heap_schema::LargeMappingRecord;

    let profile = PlatformProfile::from(TargetName::X86_64Linux);
    assert_eq!(
        heap_source_constant("LARGE_MAPPING_RECORD_BYTES"),
        u64::try_from(std::mem::size_of::<LargeMappingRecord>()).expect("记录大小适配")
    );
    assert_eq!(
        heap_source_constant("LARGE_MAPPING_TABLE_BYTES"),
        heap_source_constant("LARGE_MAPPING_RECORD_BYTES")
            * heap_source_constant("LARGE_MAPPING_CAPACITY")
    );
    assert_eq!(
        heap_source_constant("LARGE_MAPPING_PAGE_BYTES"),
        u64::from(profile.constants().page_bytes)
    );
    assert!(heap_source_constant("LARGE_MAPPING_CAPACITY") > 0);
}

/// TurnRegion 的机器侧常量必须与 `region_schema` 的状态机逐项一致。
///
/// 状态判别值与导出位集合是判据而不是别名：漂移会把「summary 闭合才允许 reset」变成永远
/// 放行，从而静默复用仍有外部引用的区域空间。
#[test]
fn heap_source_mirrors_the_turn_region_state_machine() {
    use super::region_schema::{
        REGION_CAPACITY_CLASSES, REGION_EXPORT_ALL, REGION_EXPORT_BITS, REGION_MAX_ACTIVE,
        REGION_OBJECT_LIMIT, REGION_STATE_NAMES, REGION_TRANSFER_LEASE_LIMIT,
    };
    let contract = LocalHeapRuntimeContract::build(
        LocalHeapDemand::default(),
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    assert_eq!(
        heap_source_constant("REGION_OBJECT_LIMIT"),
        u64::from(REGION_OBJECT_LIMIT)
    );
    assert_eq!(
        heap_source_constant("REGION_MAX_ACTIVE"),
        u64::from(REGION_MAX_ACTIVE)
    );
    assert_eq!(
        heap_source_constant("REGION_TRANSFER_LEASE_LIMIT"),
        u64::from(REGION_TRANSFER_LEASE_LIMIT)
    );
    assert_eq!(
        heap_source_constant("REGION_EXPORT_ALL"),
        u64::from(REGION_EXPORT_ALL)
    );
    assert_eq!(
        u64::from(REGION_EXPORT_ALL),
        (1 << REGION_EXPORT_BITS.len()) - 1,
        "export 掩码必须恰好覆盖全部导出位"
    );
    // 区域容量上界就是块字节数：越界请求由分配慢路显式拒绝，专属 LargeMapping 另立路径。
    assert_eq!(
        u64::from(*REGION_CAPACITY_CLASSES.last().expect("容量阶梯非空")),
        u64::from(contract.block_bytes)
    );
    for (index, name) in REGION_STATE_NAMES.iter().enumerate() {
        let constant = format!("REGION_STATE_{}", name.replace('-', "_").to_uppercase());
        assert_eq!(
            heap_source_constant(&constant),
            u64::try_from(index).expect("状态下标适配 u64"),
            "heap.gg 的 {constant} 与 region_schema 的状态序号不一致"
        );
    }
    // 计划编号哨兵必须落在 u32 之外：它是「还没有 publish 认领」的标记，不能与真实计划撞号。
    assert_eq!(
        heap_source_constant("REGION_PLAN_UNCLAIMED"),
        u64::from(u32::MAX)
    );
}
