//! 固定三对象图执行真实 major 标记、trace 和 sweep；不执行平台映射或原生线程。
use super::*;
use crate::runtime::local_heap_schema::{HeapBlockRecord, HeapBlockState};

mod compression;
mod foreign_cage;
mod large;
mod pin;
mod poll;
mod region;
mod shared;
mod stack;
mod trim;

const CONFIG: u64 = 1024;
const STATE: u64 = 12288;
const COORDINATOR: u64 = 65536;
const ROOTS: u64 = 24576;
const CURSOR: u64 = 28672;
const SECOND_CONTROL: u64 = 32768;
const ARENA: u64 = 131072;

fn graph_memory() -> (Vec<u8>, u64) {
    let config = crate::runtime::GcImageConfig::build();
    let mut memory = resource_object_memory();
    memory.resize((ARENA + config.arena_bytes + 65536) as usize, 0);
    for (index, value) in config.words().into_iter().enumerate() {
        put(&mut memory, CONFIG + index as u64 * 8, value);
    }
    put(&mut memory, PROCESSOR + 4080, STATE);
    put(
        &mut memory,
        STATE + crate::runtime::entry::state::COORDINATOR,
        COORDINATOR,
    );
    put(&mut memory, COORDINATOR + 48, ARENA);
    put(&mut memory, PROCESSOR + 3608, CURSOR);
    let meta = ARENA + config.arena_bytes;
    // 只有首块含三个对象；其余块是空闲块，保持真实 2 MiB arena 几何。
    for block in 1..config.arena_bytes / config.block_bytes {
        let state = meta
            + 55576
            + block * size_of::<HeapBlockRecord>() as u64
            + std::mem::offset_of!(HeapBlockRecord, state) as u64;
        memory[state as usize..state as usize + 4]
            .copy_from_slice(&HeapBlockState::Free.raw().to_le_bytes());
    }
    put(&mut memory, meta + 24, 0b1_0101);
    // type0 是 ResourceCell 叶；type1 是包含两个直接指针的容器。
    put(&mut memory, TYPE_SECTION + 32, 512);
    let leaf = TYPE_SECTION + 128;
    memory[leaf as usize + 28..leaf as usize + 32].copy_from_slice(&1u32.to_le_bytes());
    let container = leaf + 80;
    put(&mut memory, container, 16);
    memory[container as usize + 24..container as usize + 28].copy_from_slice(&16u32.to_le_bytes());
    memory[container as usize + 28..container as usize + 32].copy_from_slice(&10u32.to_le_bytes());
    let trace = TYPE_SECTION + 512 + 16;
    memory[trace as usize] = 1;
    memory[trace as usize + 4..trace as usize + 8].copy_from_slice(&2u32.to_le_bytes());
    memory[trace as usize + 8] = 3;
    // 容器 -> live resource，同时自环；另一个 resource 不可达。
    put(&mut memory, ARENA, 1);
    put(&mut memory, ARENA + 8, 16);
    put(&mut memory, ARENA + 16, ARENA + 48);
    put(&mut memory, ARENA + 24, ARENA + 16);
    for (header, control) in [(ARENA + 32, CONTROL), (ARENA + 64, SECOND_CONTROL)] {
        put(&mut memory, header, RESOURCE_FLAG);
        put(&mut memory, header + 8, 8);
        put(&mut memory, header + 16, control);
    }
    put(&mut memory, CONTROL + 184, SECOND_CONTROL);
    put(&mut memory, SECOND_CONTROL, 1);
    put(&mut memory, SECOND_CONTROL + 64, 43);
    put(&mut memory, SECOND_CONTROL + 72, 1);
    put(&mut memory, SECOND_CONTROL + 176, CONTROL);
    put(&mut memory, SECOND_CONTROL + 192, PROCESSOR);
    for offset in [3920, 3928, 3968, 3976] {
        put(&mut memory, PROCESSOR + offset, 8192);
    }
    for offset in [3936, 3984] {
        put(&mut memory, PROCESSOR + offset, 2);
    }
    stack::install(&mut memory);
    (memory, meta)
}

#[test]
fn gc_runtime_bodies_contracts() {
    edge_staging_counts_batches_and_major_consumes_overflow();
    large::large_field_barrier_uses_registered_range_without_arena_probe();
    pin::pinned_old_field_survives_minor_after_young_store();
    region::region_field_barrier_and_minor_preserve_young_reference();
    trim::run_all();
    compression::run_runtime_contracts();
    poll::run_all();
    shared::run_runtime_contracts();
    stack::run_all();
}

fn edge_staging_counts_batches_and_major_consumes_overflow() {
    let mut harness = runtime_harness();
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    let scratch = 73728;
    put(&mut memory, PROCESSOR + 3560, scratch);
    for target in [4, 5] {
        harness.run(
            "barrier",
            "barrier_edge_append",
            &[PROCESSOR, 3, target, 1],
            &mut memory,
        );
    }
    assert_eq!(word(&memory, scratch), 2);
    assert_eq!(
        harness.run("counters", "read", &[PROCESSOR, 13], &mut memory),
        [1]
    );
    put(
        &mut memory,
        scratch,
        crate::runtime::GcImageConfig::build().edge_entries,
    );
    harness.run(
        "barrier",
        "barrier_edge_append",
        &[PROCESSOR, 3, 6, 1],
        &mut memory,
    );
    assert_eq!(word(&memory, scratch + 8), 1);
    assert_eq!(
        harness.run("counters", "read", &[PROCESSOR, 13], &mut memory),
        [1]
    );
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 3],
        &mut memory,
    );
    assert_eq!([word(&memory, scratch), word(&memory, scratch + 8)], [0, 0]);
    harness.run(
        "barrier",
        "barrier_edge_append",
        &[PROCESSOR, 3, 4, 1],
        &mut memory,
    );
    assert_eq!(
        harness.run("counters", "read", &[PROCESSOR, 13], &mut memory),
        [2]
    );
}

#[test]
fn major_collection_preserves_reachable_resource_then_releases_cycle_once() {
    let output = Compiler::shared().compile_full_for_test(fs_api_request());
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    stack::verify_compiled_spills(&output);
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    // 固定 arena 的 2048 位图字和单活跃块的 2048 granule 扫描，每步数个基本块。
    harness.block_budget = 32768;
    let (mut memory, meta) = graph_memory();
    // 该夹具共同账本包含一个已提交堆块和两个文件控制页。
    put(&mut memory, PROCESSOR + 3920, 2097152 + 8192);
    put(&mut memory, PROCESSOR + 3928, 32768 + 8192);
    put(&mut memory, PROCESSOR + 3960, 32768);
    put(&mut memory, PROCESSOR + 3952, 32768);
    put(&mut memory, PROCESSOR + 3888, 2);
    memory[(meta + 55576 + 60) as usize..(meta + 55576 + 64) as usize]
        .copy_from_slice(&1u32.to_le_bytes());
    put(&mut memory, COORDINATOR + 56, ARENA + 96);
    put(&mut memory, COORDINATOR + 64, ARENA + 32768);
    stack::verify_live(&mut harness, &mut memory);
    put(&mut memory, ARENA, 1 | FORWARDED_FLAG);
    assert_eq!(
        harness.run("collect", "gc_heap_live_bytes", &[PROCESSOR], &mut memory),
        [16]
    );
    put(&mut memory, ARENA, 1);
    put(&mut memory, PROCESSOR + 3592, ARENA);
    assert_eq!(
        harness.run("collect", "gc_heap_live_bytes", &[PROCESSOR], &mut memory),
        [32]
    );
    assert_eq!(
        harness.run(
            "collect",
            "gc_collect_major",
            &[PROCESSOR, ROOTS, 3],
            &mut memory
        ),
        vec![0]
    );
    assert!(harness.decommitted.is_empty(), "仍有可达文件的块不能归还");
    assert_eq!(word(&memory, COORDINATOR + 56), ARENA + 96);
    assert_eq!(word(&memory, COORDINATOR + 64), ARENA + 32768);
    assert_eq!(word(&memory, CONTROL), 1, "从容器可达的文件不能终结");
    assert_eq!(word(&memory, ARENA + 48), CONTROL);
    assert_eq!(word(&memory, SECOND_CONTROL), 0);
    assert_eq!(harness.submitted, vec![vec![SECOND_CONTROL, 43, 1]]);
    assert_eq!(word(&memory, meta + 24), 0b101, "清扫只移除不可达对象");
    assert_eq!(word(&memory, COORDINATOR + 152), 24);
    assert_eq!(
        harness.run("collect", "gc_heap_live_bytes", &[PROCESSOR], &mut memory),
        [24]
    );
    harness.run(
        "resource_state",
        "complete_close",
        &[SECOND_CONTROL, 0],
        &mut memory,
    );
    // 撤销栈和寄存器的强根，旧槽内容与容器自环都不能保活。
    stack::remove_roots(&mut harness, &mut memory);
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 3],
        &mut memory,
    );
    assert_eq!(word(&memory, CONTROL), 0);
    assert_eq!(
        harness.submitted,
        vec![vec![SECOND_CONTROL, 43, 1], vec![CONTROL, 42, 1]]
    );
    harness.run(
        "resource_state",
        "complete_close",
        &[CONTROL, 0],
        &mut memory,
    );
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 3],
        &mut memory,
    );
    assert_eq!(harness.submitted.len(), 2, "空堆重复收集不得重新终结");
    assert_eq!(word(&memory, meta + 24), 0);
    assert_eq!(word(&memory, COORDINATOR + 144), 3);
    assert_eq!(
        harness.released,
        vec![(SECOND_CONTROL, 4096), (CONTROL, 4096)]
    );
    assert_eq!(harness.decommitted, vec![(ARENA, 32768)]);
    assert_eq!(word(&memory, COORDINATOR + 56), 0);
    assert_eq!(word(&memory, COORDINATOR + 64), 0);
    assert_eq!(word(&memory, PROCESSOR + 3928), 0);
    assert_eq!(
        harness.run("collect", "gc_heap_live_bytes", &[PROCESSOR], &mut memory),
        [0]
    );
    harness.run("native_memory", "verify_drained", &[PROCESSOR], &mut memory);
}
