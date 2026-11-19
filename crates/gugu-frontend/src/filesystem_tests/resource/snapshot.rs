//! 采样执行真实 LIR，只替换既有平台提交/线程完成边界。
use super::*;

fn compile_stats() -> Compilation {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../../gugu-compiler/tests/fixtures/runtime_stats.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}

const OUT: u64 = 3072;

fn snapshot(
    harness: &mut ResourceHarness<'_>,
    module: &str,
    memory: &mut Vec<u8>,
    words: usize,
) -> Vec<u64> {
    let body = harness.body(module, "snapshot");
    assert!(body.signature.sret.is_some());
    let before = memory.clone();
    harness.run(module, "snapshot", &[OUT, PROCESSOR], memory);
    assert_eq!(
        &memory[..OUT as usize],
        &before[..OUT as usize],
        "采样不得修改运行状态"
    );
    let end = OUT as usize + words * 8;
    assert_eq!(
        &memory[end..before.len()],
        &before[end..],
        "采样只能写结果区"
    );
    (0..words)
        .map(|index| word(memory, OUT + index as u64 * 8))
        .collect()
}

#[test]
fn memory_snapshot_separates_uncommitted_reservation_and_file_subtotal() {
    let (hir, bodies) = runtime_bodies();
    let mut harness = ResourceHarness::from_bodies(&hir, &bodies);
    let mut memory = vec![0; 16384];
    put(&mut memory, HOT + 16, PROCESSOR);
    harness.run(
        "native_memory",
        "adopt_boot",
        &[PROCESSOR, 1 << 30, 65536, 0],
        &mut memory,
    );
    let initial = snapshot(&mut harness, "native_memory", &mut memory, 11);
    assert_eq!(
        initial,
        [
            (1 << 30) + 8192,
            73728,
            (1 << 30) - 65536,
            3,
            73728,
            0,
            0,
            0,
            0,
            0,
            73728
        ]
    );
    harness.run("native_memory", "allocate", &[1], &mut memory);
    let allocated = snapshot(&mut harness, "native_memory", &mut memory, 11);
    assert_eq!(
        allocated,
        [
            initial[0] + 4096,
            77824,
            initial[2],
            4,
            77824,
            4096,
            4096,
            1,
            4096,
            0,
            77824
        ]
    );
    harness.run("native_memory", "release", &[STACK_BASE, 1], &mut memory);
    let released = snapshot(&mut harness, "native_memory", &mut memory, 11);
    assert_eq!(&released[..4], &initial[..4]);
    assert_eq!(
        &released[4..],
        &[77824, 0, 0, 0, 4096, 0, 73728],
        "释放保留高水位"
    );
    harness.reserve_result = 8192;
    harness.run(
        "native_memory",
        "reserve_aligned",
        &[4096, 4096],
        &mut memory,
    );
    put(&mut memory, 2048, 8192);
    put(&mut memory, 2056, 4096);
    harness.commit_error = 12;
    harness.run("native_memory", "commit_heap", &[2048], &mut memory);
    let failed = snapshot(&mut harness, "native_memory", &mut memory, 11);
    assert_eq!(failed[0], initial[0] + 4096);
    assert_eq!(failed[1], initial[1]);
    assert_eq!(failed[2], initial[2] + 4096);
    harness.commit_error = 0;
    harness.run("native_memory", "commit_heap", &[2048], &mut memory);
    harness.decommit_error = 12;
    harness.run("native_memory", "decommit_heap", &[2048], &mut memory);
    let committed = snapshot(&mut harness, "native_memory", &mut memory, 11);
    assert_eq!(committed[1], initial[1] + 4096);
    assert_eq!(committed[2], initial[2]);
    assert_eq!(committed[9], 4096);
    assert_eq!(committed[10], initial[1]);
    assert_eq!(committed[9] + committed[10], committed[1]);
    harness.decommit_error = 0;
    harness.run("native_memory", "decommit_heap", &[2048], &mut memory);
    assert_eq!(
        snapshot(&mut harness, "native_memory", &mut memory, 11),
        failed
    );
    harness.run(
        "native_memory",
        "release_mapping",
        &[8192, 4096, 0],
        &mut memory,
    );
    assert_eq!(
        snapshot(&mut harness, "native_memory", &mut memory, 11),
        released
    );
}

#[test]
fn windows_aligned_reservations_release_original_blocks_and_account_metadata() {
    let (hir, bodies) = super::super::harness::runtime_bodies_for(TargetName::X86_64Windows);
    let mut harness = ResourceHarness::from_bodies(&hir, &bodies);
    let mut memory = vec![0; 524288];
    put(&mut memory, HOT + 16, PROCESSOR);
    harness.reserve_result = 65536;
    let first = harness.run(
        "native_memory",
        "reserve_aligned",
        &[4096, 131072],
        &mut memory,
    )[0];
    assert_eq!(first, 131072);
    harness.reserve_result = 262144;
    let second = harness.run(
        "native_memory",
        "reserve_aligned",
        &[4096, 131072],
        &mut memory,
    )[0];
    assert_eq!(second, 393216);
    assert_eq!(word(&memory, PROCESSOR + 3920), 2 * 139264);
    assert_eq!(word(&memory, PROCESSOR + 3928), 8192);
    assert_eq!(word(&memory, PROCESSOR + 3936), 2);
    put(&mut memory, 2048, first);
    put(&mut memory, 2056, 4096);
    assert_eq!(
        harness.run("native_memory", "commit_heap", &[2048], &mut memory),
        [0]
    );
    assert_eq!(word(&memory, PROCESSOR + 3960), 4096);
    // 先释放非链首，不能误删后一个预约或只释放对齐后的内部地址。
    harness.run(
        "native_memory",
        "release_heap_mapping",
        &[first, 4096, 4096],
        &mut memory,
    );
    assert_eq!(harness.released, [(65536, 139264)]);
    assert_eq!(word(&memory, PROCESSOR + 3920), 139264);
    assert_eq!(word(&memory, PROCESSOR + 3928), 4096);
    assert_eq!(word(&memory, PROCESSOR + 3960), 0);
    harness.run(
        "native_memory",
        "release_mapping",
        &[second, 4096, 0],
        &mut memory,
    );
    assert_eq!(harness.released, [(65536, 139264), (262144, 139264)]);
    assert_eq!(word(&memory, PROCESSOR + 3920), 0);
    assert_eq!(word(&memory, PROCESSOR + 3928), 0);
    assert_eq!(word(&memory, PROCESSOR + 3936), 0);
    assert_eq!(
        word(&memory, PROCESSOR + 3944),
        12288,
        "实际 metadata 提交计入高水位"
    );
}

#[test]
fn windows_aligned_metadata_commit_failure_releases_unpublished_reservation() {
    let (hir, bodies) = super::super::harness::runtime_bodies_for(TargetName::X86_64Windows);
    let mut harness = ResourceHarness::from_bodies(&hir, &bodies);
    let mut memory = vec![0; 262144];
    put(&mut memory, HOT + 16, PROCESSOR);
    harness.reserve_result = 65536;
    harness.commit_error = 12;
    assert_eq!(
        harness.run(
            "native_memory",
            "reserve_aligned",
            &[4096, 131072],
            &mut memory
        ),
        [0]
    );
    assert_eq!(harness.released, [(65536, 139264)]);
    assert_eq!(word(&memory, PROCESSOR + 3920), 0);
    assert_eq!(word(&memory, PROCESSOR + 3928), 0);
    assert_eq!(word(&memory, PROCESSOR + 3936), 0);
}

#[test]
fn coroutine_stack_ledger_preserves_failed_commit_and_retires_only_live_bytes() {
    let output = compile_stats();
    let mut harness = ResourceHarness::new(&output);
    let mut memory = vec![0; 16384];
    put(&mut memory, HOT + 16, PROCESSOR);
    harness.run(
        "native_memory",
        "adopt_boot",
        &[PROCESSOR, 1 << 30, 65536, 0],
        &mut memory,
    );
    harness.run("native_memory", "reserve_stack", &[1 << 30], &mut memory);
    assert_eq!(word(&memory, PROCESSOR + 3864), 2 << 30);
    put(&mut memory, 2048, 196608);
    put(&mut memory, 2056, 65536);
    harness.commit_error = 12;
    assert_eq!(
        harness.run("native_memory", "commit_stack", &[2048], &mut memory),
        [12]
    );
    assert_eq!(word(&memory, PROCESSOR + 3872), 65536);
    assert_eq!(word(&memory, PROCESSOR + 3880), 65536);
    harness.commit_error = 0;
    harness.run("native_memory", "commit_stack", &[2048], &mut memory);
    // 随后的向下增长提交另一段，不重复提交相同页。
    put(&mut memory, 2048, 131072);
    harness.run("native_memory", "commit_stack", &[2048], &mut memory);
    assert_eq!(word(&memory, PROCESSOR + 3872), 3 * 65536);
    assert_eq!(word(&memory, PROCESSOR + 3880), 3 * 65536);
    harness.run(
        "native_memory",
        "retire_stack",
        &[PROCESSOR, 2 * 65536],
        &mut memory,
    );
    assert_eq!(word(&memory, PROCESSOR + 3880), 65536);
    assert_eq!(word(&memory, PROCESSOR + 3872), 3 * 65536);
    assert_eq!(word(&memory, PROCESSOR + 3864), 2 << 30);
    assert!(harness.released.is_empty());
}

#[test]
fn shutdown_releases_all_cached_worker_stacks() {
    let (hir, bodies) = runtime_bodies();
    let mut harness = ResourceHarness::from_bodies(&hir, &bodies);
    let mut memory = request_memory();
    put(&mut memory, REQUEST + 104, 0);
    let bytes = 9 * 65536 + 4096;
    for offset in [3920, 3928, 3968, 3976] {
        put(&mut memory, PROCESSOR + offset, bytes);
    }
    for offset in [3936, 3984] {
        put(&mut memory, PROCESSOR + offset, 10);
    }
    let mut expected = Vec::new();
    for index in 0..9 {
        let base = 131072 + index * 65536;
        let slot = if index < 8 {
            POOL + 1088 + index * 8
        } else {
            POOL + 2048 + 1088
        };
        put(&mut memory, slot, base + 65536);
        expected.push((base, 65536));
    }
    expected.push((POOL, 4096));
    harness.run("fs_platform", "shutdown", &[PROCESSOR], &mut memory);
    assert_eq!(harness.released, expected);
    for offset in [3920, 3928, 3936, 3968, 3976, 3984, 4024] {
        assert_eq!(word(&memory, PROCESSOR + offset), 0);
    }
}

#[test]
fn retired_stack_trim_respects_byte_budget_across_rounds() {
    let (hir, bodies) = runtime_bodies();
    let mut harness = ResourceHarness::from_bodies(&hir, &bodies);
    let mut memory = vec![0; 196608];
    let node = 16384;
    let dead = 17408;
    let high = (1u64 << 30) + 262144;
    put(&mut memory, HOT + 16, PROCESSOR);
    put(&mut memory, PROCESSOR + 4008, node);
    put(&mut memory, node + 8, dead);
    put(&mut memory, dead, 16);
    put(&mut memory, dead + 48, 18432);
    put(&mut memory, dead + 80, high);
    put(&mut memory, dead + 88, 131072);
    for (offset, value) in [
        (3864, 1 << 30),
        (3872, 131072),
        (3920, 1 << 30),
        (3928, 131072),
        (3936, 1),
        (3888, 2),
    ] {
        put(&mut memory, PROCESSOR + offset, value);
    }
    harness.decommit_alias = Some((high - 131072, 65536));
    harness.run(
        "scheduler",
        "trim_retired_stacks",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(harness.decommitted, [(high - 131072, 65536)]);
    assert!(harness.released.is_empty());
    assert_eq!(word(&memory, dead + 88), 65536);
    assert_eq!(word(&memory, PROCESSOR + 4008), node);
    assert_eq!(word(&memory, PROCESSOR + 3872), 65536);
    assert_eq!(word(&memory, PROCESSOR + 3928), 65536);
    harness.decommit_alias = Some((high - 65536, 131072));
    put(&mut memory, PROCESSOR + 3888, 2);
    harness.run(
        "scheduler",
        "trim_retired_stacks",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(
        harness.decommitted,
        [(high - 131072, 65536), (high - 65536, 65536)]
    );
    assert_eq!(harness.released, [(262144, 1 << 30)]);
    for offset in [3864, 3872, 3920, 3928, 3936, 4008] {
        assert_eq!(word(&memory, PROCESSOR + offset), 0);
    }
}

#[test]
fn pressure_retires_dead_stack_without_losing_join_result() {
    let output = compile_stats();
    let mut harness = ResourceHarness::new(&output);
    let mut memory = vec![0; 131072];
    let node = 16384;
    let dead = 17408;
    let cold = 18432;
    let stack_base = 262144;
    let reserve = 1u64 << 30;
    let capacity = 65536;
    let high = stack_base + reserve;
    harness.decommit_alias = Some((high - capacity, 65536));
    memory[65536..].fill(0xa5);
    put(&mut memory, HOT + 16, PROCESSOR);
    put(&mut memory, PROCESSOR + 4008, node);
    put(&mut memory, node + 8, dead);
    put(&mut memory, dead, 16);
    put(&mut memory, dead + 48, cold);
    put(&mut memory, dead + 64, high - capacity + 512);
    put(&mut memory, dead + 72, high - capacity);
    put(&mut memory, dead + 80, high);
    put(&mut memory, dead + 88, capacity);
    put(&mut memory, cold + 368, 1);
    put(&mut memory, cold + 376, 42);
    for (offset, value) in [
        (3864, reserve * 2),
        (3872, capacity * 2),
        (3880, capacity),
        (3920, reserve * 2 + 8192),
        (3928, capacity * 2 + 8192),
        (3936, 4),
    ] {
        put(&mut memory, PROCESSOR + offset, value);
    }
    let before = memory.clone();
    harness.run(
        "scheduler",
        "trim_retired_stacks",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(memory, before, "无压力不回收");
    put(&mut memory, PROCESSOR + 3888, 2);
    harness.decommit_error = 12;
    let before = memory.clone();
    harness.run(
        "scheduler",
        "trim_retired_stacks",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(memory, before, "平台失败保留节点与所有账目");
    assert!(harness.released.is_empty());
    harness.decommit_error = 0;
    harness.run(
        "scheduler",
        "trim_retired_stacks",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(harness.released, [(stack_base, reserve)]);
    assert_eq!(word(&memory, PROCESSOR + 4008), 0);
    for (offset, value) in [
        (3864, reserve),
        (3872, capacity),
        (3880, capacity),
        (3920, reserve + 8192),
        (3928, capacity + 8192),
        (3936, 3),
    ] {
        assert_eq!(word(&memory, PROCESSOR + offset), value, "offset {offset}");
    }
    assert_eq!(word(&memory, dead + 88), 0);
    assert_eq!(
        [word(&memory, cold + 368), word(&memory, cold + 376)],
        [1, 42]
    );
    assert!(memory[65536..].iter().all(|byte| *byte == 0));
    put(&mut memory, PROCESSOR + 3888, 2);
    harness.run(
        "scheduler",
        "trim_retired_stacks",
        &[PROCESSOR],
        &mut memory,
    );
    assert_eq!(harness.decommitted.len(), 2);
    assert_eq!(harness.released.len(), 1);
}

#[test]
fn public_stats_reads_runtime_state_without_scheduling_or_allocation() {
    let output = compile_stats();
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = 16384;
    harness.block_budget = 32768;
    let mut memory = vec![0; 81920];
    put(&mut memory, HOT + 16, PROCESSOR);
    for (index, value) in crate::runtime::GcImageConfig::build()
        .words()
        .into_iter()
        .enumerate()
    {
        put(&mut memory, 16384 + index as u64 * 8, value);
    }
    put(&mut memory, PROCESSOR + 4080, 12288);
    put(
        &mut memory,
        12288 + crate::runtime::entry::state::COORDINATOR,
        65536,
    );
    put(&mut memory, PROCESSOR + 4032, 2);
    harness.run(
        "native_memory",
        "adopt_boot",
        &[PROCESSOR, 1 << 30, 65536, 0],
        &mut memory,
    );
    put(&mut memory, PROCESSOR + 3928, 73728 + 32768 + 65536);
    put(&mut memory, PROCESSOR + 3960, 32768);
    put(&mut memory, PROCESSOR + 3872, 131072);
    harness.run(
        "collect",
        "gc_record_pause",
        &[PROCESSOR, 100, 125],
        &mut memory,
    );
    harness.run(
        "collect",
        "gc_record_pause",
        &[PROCESSOR, 1000, 1100],
        &mut memory,
    );
    harness.run("counters", "add", &[17, 4], &mut memory);
    put(&mut memory, 768 + 32, 17);
    put(&mut memory, 768 + 40, 3);
    put(&mut memory, PROCESSOR + 3624 + 19 * 8, 99);
    put(&mut memory, PROCESSOR + 3624 + 20 * 8, 99);
    let before = memory.clone();
    let stats = super::super::named(&output, "stats");
    assert!(stats.signature.sret.is_some());
    harness.evaluate(stats, &[OUT], &mut memory);
    for (field, value) in [
        (0, 2),
        (1, 1),
        (2, 1),
        (3, 32768),
        (4, 0),
        (5, 1 << 30),
        (6, 131072),
        (7, 65536),
        (18, 0),
        (19, 125),
        (22, 139264),
        (24, 65536),
        (34, 4),
        (36, 17),
        (37, 3),
    ] {
        assert_eq!(word(&memory, OUT + field * 8), value, "field {field}");
    }
    assert_eq!(&memory[..OUT as usize], &before[..OUT as usize]);
    assert_eq!(
        &memory[OUT as usize + 43 * 8..],
        &before[OUT as usize + 43 * 8..]
    );
    assert!(harness.started.is_empty() && harness.reserved.is_empty() && harness.woken.is_empty());
    put(&mut memory, PROCESSOR + 4024, POOL);
    put(&mut memory, REQUEST + 104, HOT);
    put(&mut memory, REQUEST + 2048 + 104, HOT);
    put(&mut memory, POOL + 2048, HOT);
    put(&mut memory, POOL + 2048 + 8, HOT);
    put(&mut memory, POOL + 2048 + 32, 1);
    put(&mut memory, HOT + 48, COLD);
    put(&mut memory, COLD + 480, 4096);
    // 已完成的宿主状态不由采样轮询；owner 未消费前依然计入 active。
    harness.poll_complete = true;
    put(
        &mut memory,
        64 + crate::runtime::callback::EXTERNAL_THREADS as u64,
        2,
    );
    let before = memory.clone();
    harness.evaluate(stats, &[OUT], &mut memory);
    for (field, value) in [(1, 5), (8, 1), (9, 1), (10, 1), (11, 0), (12, 1), (13, 0)] {
        assert_eq!(
            word(&memory, OUT + field * 8),
            value,
            "bridge field {field}"
        );
    }
    assert_eq!(&memory[..OUT as usize], &before[..OUT as usize]);
    assert_eq!(
        &memory[OUT as usize + 43 * 8..],
        &before[OUT as usize + 43 * 8..]
    );
    // 嵌套请求继承原 worker credit；采样同时覆盖执行中及等待 owner 消费的返回。
    let borrowed = 8192;
    put(
        &mut memory,
        64 + crate::runtime::callback::ACTIVE as u64,
        borrowed,
    );
    for state in [4, 5] {
        put(&mut memory, borrowed + 8, state);
        let before = memory.clone();
        harness.evaluate(stats, &[OUT], &mut memory);
        for (field, value) in [(1, 5), (10, 2), (11, 0), (12, 1)] {
            assert_eq!(
                word(&memory, OUT + field * 8),
                value,
                "borrowed field {field}"
            );
        }
        assert_eq!(&memory[..OUT as usize], &before[..OUT as usize]);
        assert_eq!(
            &memory[OUT as usize + 43 * 8..],
            &before[OUT as usize + 43 * 8..]
        );
    }
    assert!(harness.started.is_empty() && harness.reserved.is_empty() && harness.woken.is_empty());
}

#[test]
fn control_cache_trim_recommits_only_required_pages() {
    let output = compile_stats();
    let mut harness = ResourceHarness::new(&output);
    let mut memory = vec![0; 196608];
    let base = 65536;
    put(&mut memory, HOT + 16, PROCESSOR);
    put(&mut memory, PROCESSOR + 4048, base + 17);
    put(&mut memory, PROCESSOR + 4056, base + 131072);
    put(&mut memory, PROCESSOR + 3840, base + 131072);
    put(&mut memory, PROCESSOR + 3920, 131072);
    put(&mut memory, PROCESSOR + 3928, 131072);
    put(&mut memory, PROCESSOR + 3936, 1);
    harness.decommit_error = 12;
    assert_eq!(
        harness.run("scheduler", "trim_control_cache", &[PROCESSOR], &mut memory),
        [0]
    );
    assert_eq!(word(&memory, PROCESSOR + 3840), base + 131072);
    assert_eq!(word(&memory, PROCESSOR + 3928), 131072);
    harness.decommit_error = 0;
    assert_eq!(
        harness.run("scheduler", "trim_control_cache", &[PROCESSOR], &mut memory),
        [0]
    );
    assert_eq!(word(&memory, PROCESSOR + 3840), base + 65536);
    assert_eq!(
        harness.run("scheduler", "trim_control_cache", &[PROCESSOR], &mut memory),
        [1]
    );
    assert_eq!(word(&memory, PROCESSOR + 3840), base + 4096);
    assert_eq!(word(&memory, PROCESSOR + 3928), 4096);
    assert_eq!(
        harness.decommitted,
        [
            (base + 65536, 65536),
            (base + 65536, 65536),
            (base + 4096, 61440)
        ]
    );
    assert_eq!(
        harness.run("scheduler", "heap_alloc", &[PROCESSOR, 4096], &mut memory),
        [base + 64]
    );
    assert_eq!(harness.committed, [(base + 4096, 4096)]);
    assert_eq!(word(&memory, PROCESSOR + 3928), 8192);
    assert_eq!(word(&memory, PROCESSOR + 3920), 131072);
    assert_eq!(word(&memory, PROCESSOR + 3840), base + 8192);
    assert_eq!(
        harness.run("scheduler", "heap_alloc", &[PROCESSOR, 64], &mut memory),
        [base + 4160]
    );
    assert_eq!(harness.committed.len(), 1);
}

#[test]
fn blocking_snapshot_tracks_waiting_inflight_completion_and_stack_cache() {
    let output = compile();
    let mut harness = ResourceHarness::new(&output);
    let mut memory = request_memory();
    put(&mut memory, PROCESSOR + 4024, 0);
    assert_eq!(
        snapshot(&mut harness, "fs_platform", &mut memory, 7),
        [0; 7]
    );
    assert!(harness.reserved.is_empty(), "无 pool 时观察不能建立 pool");
    put(&mut memory, PROCESSOR + 4024, POOL);
    put(&mut memory, REQUEST + 104, 0);
    put(&mut memory, CANCEL, 0);
    put(&mut memory, POOL, HOT);
    put(&mut memory, POOL + 8, HOT);
    put(&mut memory, POOL + 32, 1);
    harness.run("fs_platform", "raw_allocate", &[12], &mut memory);
    assert_eq!(word(&memory, COLD + 480), 4096);
    assert_eq!(
        snapshot(&mut harness, "fs_platform", &mut memory, 7),
        [0, 1, 64, 0, 0, 0, 4096]
    );
    harness.reserve_result = 16384;
    assert!(harness.started.is_empty() && harness.woken.is_empty());
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(
        snapshot(&mut harness, "fs_platform", &mut memory, 7),
        [1, 0, 0, STACK_BYTES, 0, 0, 0]
    );
    harness.poll_complete = true;
    // 原生已经可报告完成但 service 尚未确认，采样不隐式调用 fs_poll。
    assert_eq!(snapshot(&mut harness, "fs_platform", &mut memory, 7)[0], 1);
    assert!(harness.woken.is_empty());
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(
        snapshot(&mut harness, "fs_platform", &mut memory, 7),
        [0, 0, 0, STACK_BYTES, STACK_BYTES, 0, 0]
    );
    assert_eq!(
        harness.run("fs_platform", "pending", &[PROCESSOR], &mut memory),
        [0]
    );
    put(&mut memory, PROCESSOR + 3888, 2);
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(
        snapshot(&mut harness, "fs_platform", &mut memory, 7),
        [0; 7]
    );
    put(&mut memory, POOL + 16, CONTROL);
    assert_eq!(
        snapshot(&mut harness, "fs_platform", &mut memory, 7),
        [0, 0, 0, 0, 0, 1, 0]
    );
    assert_eq!(
        harness.run("fs_platform", "pending", &[PROCESSOR], &mut memory),
        [1]
    );
    assert_eq!(harness.woken, [HOT]);
    assert_eq!(word(&memory, COLD + 480), 4096);
    harness.run("fs_platform", "raw_free", &[STACK_BASE, 12], &mut memory);
    assert_eq!(word(&memory, COLD + 480), 0);
}

#[test]
fn verified_morestack_entry_commits_and_publishes_larger_stack() {
    let (hir, bodies) = super::super::harness::runtime_bodies();
    let mut harness = ResourceHarness::from_bodies(&hir, &bodies);
    let mut memory = vec![0; 16384];
    const SECTION: u64 = 512;
    const STATE: u64 = 1024;
    const COLD: u64 = 1152;
    const FRAGMENT: u64 = 1048576;
    const HIGH: u64 = 1073807360;
    memory[512..520].copy_from_slice(b"GUGUSM01");
    memory[520..524].copy_from_slice(&0x01080004u32.to_le_bytes());
    memory[524..528].copy_from_slice(&2u32.to_le_bytes());
    memory[528..532].copy_from_slice(&1u32.to_le_bytes());
    put(&mut memory, SECTION + 32, 72);
    put(&mut memory, SECTION + 40, 136);
    put(&mut memory, SECTION + 72, 100);
    memory[592..596].copy_from_slice(&100u32.to_le_bytes());
    memory[604..608].copy_from_slice(&1u32.to_le_bytes());
    put(&mut memory, SECTION + 104, 300);
    memory[624..628].copy_from_slice(&100u32.to_le_bytes());
    memory[632..636].copy_from_slice(&1u32.to_le_bytes());
    memory[648..652].copy_from_slice(&7u32.to_le_bytes());
    memory[656] = 4;
    put(&mut memory, HOT + 16, PROCESSOR);
    put(&mut memory, HOT + 80, HIGH);
    put(&mut memory, HOT + 88, 65536);
    put(&mut memory, PROCESSOR + 3928, 65536);
    put(&mut memory, PROCESSOR + 3872, 65536);
    put(&mut memory, PROCESSOR + 3880, 65536);
    put(&mut memory, STATE, HOT);
    put(&mut memory, STATE + 8, HIGH - 65536 + 2048);
    put(&mut memory, STATE + 24, 65536);
    put(&mut memory, STATE + 32, COLD);
    put(&mut memory, COLD + 64, FRAGMENT + 107);
    harness.run(
        "core",
        "morestack_handle",
        &[SECTION, 160, FRAGMENT, STATE, HIGH - 1073741824],
        &mut memory,
    );
    assert_eq!(word(&memory, HOT + 88), 131072);
    assert_eq!(word(&memory, HOT + 64), HIGH - 131072 + 512);
    assert_eq!(word(&memory, PROCESSOR + 3928), 131072);
    assert_eq!(word(&memory, PROCESSOR + 3872), 131072);
    assert_eq!(word(&memory, PROCESSOR + 3880), 131072);
}
