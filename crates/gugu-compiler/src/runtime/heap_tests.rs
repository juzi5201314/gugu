//! LocalHeap 契约段与 Immix arena 布局的确定性回归。
//!
//! 只消费进程内的契约对象与推导器，不读镜像、不启动子进程、不做重负载分配。

use super::barrier_schema::CARD_GRANULARITY_BYTES;
use super::gc_metadata_contract::{GC_ARENA_BYTES, GC_BLOCK_BYTES, GC_LINE_BYTES};
use super::local_heap_schema::{
    HEAP_BLOCKS_PER_ARENA, HEAP_GRANULE_BYTES, HEAP_LINES_PER_BLOCK, HEAP_OBJECT_HEADER_BYTES,
    HEAP_TLAB_SPAN_BLOCKS, HeapTriggerProfile, LOCAL_HEAP_SCHEMA, LocalHeapDemand,
    LocalHeapRuntimeContract,
};
use super::platform::PlatformProfile;
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
    assert_eq!(contract.record_field_count(), 30);
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
    assert!(dump.contains("local-heap schema=3 arena=2097152 block=32768 line=128"));
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
    assert!(dump.contains("local-heap-block-states active,candidate,reclaiming,free"));
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
    let mut trigger = HeapTriggerProfile::default();
    trigger.tenure_age = 0;
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
