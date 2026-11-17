//! 只在平台 decommit 边界注入状态；释放门禁、账本和重新开块执行真实 LIR。
use super::*;

const BLOCK_BYTES: u64 = 32768;
const TOTAL: u64 = BLOCK_BYTES + 8192;
fn put32(memory: &mut [u8], at: u64, value: u32) {
    memory[at as usize..at as usize + 4].copy_from_slice(&value.to_le_bytes());
}
fn empty_block() -> (Vec<u8>, u64, u64) {
    let (mut memory, meta) = graph_memory();
    let record = meta + 55576;
    put(&mut memory, meta + 24, 0);
    put32(&mut memory, record + 56, HeapBlockState::Free.raw());
    put32(&mut memory, record + 60, 1);
    put(&mut memory, PROCESSOR + 3920, 8192 + 2097152);
    put(&mut memory, PROCESSOR + 3928, TOTAL);
    put(&mut memory, PROCESSOR + 3960, BLOCK_BYTES);
    put(&mut memory, PROCESSOR + 3944, TOTAL);
    put(&mut memory, PROCESSOR + 3936, 3);
    put(&mut memory, PROCESSOR + 3952, BLOCK_BYTES);
    put(&mut memory, PROCESSOR + 3888, 2);
    (memory, meta, record)
}
fn trim(harness: &mut ResourceHarness<'_>, memory: &mut Vec<u8>) -> u64 {
    harness.run(
        "heap",
        "heap_trim_free_blocks",
        &[PROCESSOR, ARENA, 2097152, BLOCK_BYTES],
        memory,
    )[0]
}

#[test]
fn pressure_decommit_preserves_failed_mapping_and_recommits_reused_block() {
    let output = compile();
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, _, record) = empty_block();
    harness.decommit_error = 12;
    assert_eq!(trim(&mut harness, &mut memory), 0);
    assert_eq!(word(&memory, PROCESSOR + 3928), TOTAL);
    assert_eq!(word(&memory, PROCESSOR + 3960), BLOCK_BYTES);
    assert_eq!(word(&memory, record + 56) >> 32, 1);
    harness.decommit_error = 0;
    assert_eq!(trim(&mut harness, &mut memory), BLOCK_BYTES);
    assert_eq!(harness.decommitted, vec![(ARENA, BLOCK_BYTES); 2]);
    assert_eq!(word(&memory, record + 56) >> 32, 0);
    assert_eq!(word(&memory, PROCESSOR + 3928), 8192);
    assert_eq!(word(&memory, PROCESSOR + 3960), 0);
    assert_eq!(word(&memory, PROCESSOR + 3920), 8192 + 2097152);
    assert_eq!(word(&memory, PROCESSOR + 3936), 3);
    assert_eq!(word(&memory, PROCESSOR + 3944), TOTAL);
    assert_eq!(
        word(&memory, PROCESSOR + 3888),
        2,
        "低水位仍等待三类 drain 确认"
    );
    put(&mut memory, PROCESSOR + 3888, 2);
    assert_eq!(trim(&mut harness, &mut memory), 0, "未提交块不能重复扣减");
    assert_eq!(harness.decommitted.len(), 2);
    harness.run(
        "heap",
        "heap_open_free_block_meta",
        &[ARENA, 0, 2097152, BLOCK_BYTES, 128],
        &mut memory,
    );
    assert_eq!(harness.committed, vec![(ARENA, BLOCK_BYTES)]);
    assert_eq!(word(&memory, PROCESSOR + 3928), TOTAL);
    assert_eq!(word(&memory, PROCESSOR + 3960), BLOCK_BYTES);
    assert_eq!(word(&memory, record + 56), 1 << 32);
    assert_eq!(trim(&mut harness, &mut memory), 0, "重新分配的块不能回收");
    put32(&mut memory, record + 56, HeapBlockState::Free.raw());
    harness.run(
        "heap",
        "heap_open_free_block_meta",
        &[ARENA, 0, 2097152, BLOCK_BYTES, 128],
        &mut memory,
    );
    assert_eq!(harness.committed.len(), 1, "已提交块复用不重复提交");
    assert_eq!(word(&memory, PROCESSOR + 3960), BLOCK_BYTES);
    assert_eq!(
        word(&memory, PROCESSOR + 3976),
        8192,
        "heap trim 不修改文件分类小计"
    );
}

#[test]
fn large_object_mapping_charges_heap_partition_without_file_subtotal() {
    let output = compile();
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    // 大对象入口按 2 MiB 反推当前 arena；使用对齐的固定地址与独立元数据。
    let arena = 2097152;
    memory.resize((arena * 2 + 65536) as usize, 0);
    put(&mut memory, PROCESSOR + 3520, arena + 16);
    put(&mut memory, PROCESSOR + 3528, arena + 32768);
    harness.reserve_result = 131072;
    let result = harness.run(
        "heap",
        "heap_large_allocate",
        &[PROCESSOR, arena, 16, 40000, 8, 0, 4096],
        &mut memory,
    );
    let charged = (16u64 + 4095 + 40000).div_ceil(4096) * 4096;
    assert_eq!(result, [131072 + 4096]);
    assert_eq!(harness.committed, [(131072, charged)]);
    assert_eq!(word(&memory, PROCESSOR + 3960), charged);
    assert_eq!(word(&memory, PROCESSOR + 3928), 8192 + charged);
    assert_eq!(word(&memory, PROCESSOR + 3976), 8192);
    put(&mut memory, PROCESSOR + 3592, arena);
    put(&mut memory, COORDINATOR + 48, arena);
    assert_eq!(
        harness.run("collect", "gc_heap_live_bytes", &[PROCESSOR], &mut memory),
        [40000]
    );
}

#[test]
fn region_allocation_commits_data_and_reset_removes_only_payload() {
    let output = compile();
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    harness.reserve_result = 131072;
    let (mut memory, _) = graph_memory();
    memory.resize(196608, 0);
    let committed_before = word(&memory, PROCESSOR + 3928);
    let heap_before = word(&memory, PROCESSOR + 3960);
    assert_eq!(
        harness.run("heap", "gc_region_slow", &[1, 16, 16, 0], &mut memory),
        [131088]
    );
    assert_eq!(
        harness.committed,
        [(131072, BLOCK_BYTES), (131072 + BLOCK_BYTES, 384)]
    );
    assert_eq!(
        word(&memory, PROCESSOR + 3928),
        committed_before + BLOCK_BYTES + 4096
    );
    assert_eq!(word(&memory, PROCESSOR + 3960), heap_before + BLOCK_BYTES);
    assert_eq!(word(&memory, PROCESSOR + 3824), 16);
    assert_eq!(word(&memory, 131072 + BLOCK_BYTES + 96), 16);
    harness.run("heap", "gc_region_publish", &[7, 0], &mut memory);
    harness.run("heap", "gc_region_reset", &[7], &mut memory);
    assert_eq!(
        [
            word(&memory, PROCESSOR + 3824),
            word(&memory, 131072 + BLOCK_BYTES + 96),
            word(&memory, 131072 + BLOCK_BYTES + 48)
        ],
        [0, 0, 1]
    );
    assert_eq!(word(&memory, PROCESSOR + 3960), heap_before + BLOCK_BYTES);
    assert!(harness.decommitted.is_empty());
}

#[test]
fn pressure_decommit_rejects_leases_candidates_objects_and_active_allocator() {
    let output = compile();
    let (original, meta, record) = empty_block();
    // incoming、allocator、scanner、evacuation、candidate、state、commit、live line、object、TLAB、pressure。
    for (address, value) in [
        (record + 24, 1),
        (record + 40, 1),
        (record + 44, 1),
        (record + 48, 1),
        (record + 52, 1),
        (record + 56, 0),
        (record + 60, 0),
        (meta + 38936, 1),
        (meta + 24, 1),
        (PROCESSOR + 3520, ARENA as u32),
        (PROCESSOR + 3888, 0),
    ] {
        let mut harness = ResourceHarness::new(&output);
        harness.gc_config = CONFIG;
        harness.block_budget = 32768;
        let mut memory = original.clone();
        put32(&mut memory, address, value);
        if address == PROCESSOR + 3520 {
            put(&mut memory, PROCESSOR + 3528, ARENA + BLOCK_BYTES);
        }
        assert_eq!(trim(&mut harness, &mut memory), 0, "gate {address}");
        assert!(harness.decommitted.is_empty(), "gate {address}");
        assert_eq!(word(&memory, PROCESSOR + 3928), TOTAL);
        assert_eq!(word(&memory, PROCESSOR + 3960), BLOCK_BYTES);
    }
}
