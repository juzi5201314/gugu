//! 固定共享槽执行真实身份/guard/GC 逻辑；平台只返回预先登记的映射。
use super::*;

#[test]
fn shared_trim_resumes_bounded_scan_and_new_work_revokes_confirmation() {
    let output = compile();
    let mut harness = ResourceHarness::new(&output);
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    let base = 131072;
    memory.resize(base + 18 * 8192, 0);
    for index in 0..18 {
        let table = (base + index * 8192) as u64;
        harness.reserve_script.push_back(table);
        assert_eq!(
            harness.run("shared", "shared_new_table", &[], &mut memory),
            [table]
        );
        // 只保留链尾空表，让第一轮必须跨过 16 个 live 表才能找到它。
        if index != 0 {
            put(&mut memory, table + 16, 1);
            harness.run("counters", "remove", &[8, 8192], &mut memory);
        }
    }
    assert_eq!(harness.run("shared", "shared_trim", &[], &mut memory), [0]);
    assert!(harness.released.is_empty());
    assert_ne!(word(&memory, COORDINATOR + 200), 0);
    assert_eq!(harness.run("shared", "shared_trim", &[], &mut memory), [1]);
    assert_eq!(harness.released, [(base as u64, 8192)]);
    assert_eq!(word(&memory, COORDINATOR + 200), 0);
    for (index, bit) in [(7, 1), (8, 4), (28, 2)] {
        put(&mut memory, PROCESSOR + 3952, 100);
        put(&mut memory, PROCESSOR + 3928, 60);
        put(&mut memory, PROCESSOR + 3888, 2);
        put(&mut memory, PROCESSOR + 3832, 7);
        harness.run("counters", "add", &[index, 4096], &mut memory);
        assert_eq!(word(&memory, PROCESSOR + 3832), 7 & !bit);
        assert_eq!(word(&memory, PROCESSOR + 3888), 2);
        harness.run("counters", "remove", &[index, 4096], &mut memory);
        harness.run(
            "native_memory",
            "pressure_drained",
            &[PROCESSOR, bit],
            &mut memory,
        );
        assert_eq!(word(&memory, PROCESSOR + 3888), 0);
    }
}

#[test]
fn pressure_service_waits_for_resource_return_and_drains_both_caches() {
    let output = compile();
    let mut harness = ResourceHarness::new(&output);
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    harness.reserve_script.push_back(73728);
    harness.run("shared", "shared_new_table", &[], &mut memory);
    let base = 90112;
    put(&mut memory, PROCESSOR + 4048, base + 16);
    put(&mut memory, PROCESSOR + 4056, base + 12288);
    put(&mut memory, PROCESSOR + 3840, base + 12288);
    for offset in [3920, 3928] {
        put(&mut memory, PROCESSOR + offset, 28672);
    }
    put(&mut memory, PROCESSOR + 3936, 4);
    put(&mut memory, PROCESSOR + 3952, 30000);
    put(&mut memory, PROCESSOR + 3888, 2);
    harness.run("resource", "release_control", &[CONTROL], &mut memory);
    assert_eq!(
        harness.run("counters", "pending_returns", &[PROCESSOR], &mut memory),
        [4096]
    );
    harness.run("pressure", "pressure_service", &[PROCESSOR], &mut memory);
    assert_eq!(word(&memory, PROCESSOR + 3928), 12288);
    assert_eq!(word(&memory, PROCESSOR + 3832), 6);
    assert_eq!(
        word(&memory, PROCESSOR + 3888),
        2,
        "尚有 pending resource，低水位不能结束 episode"
    );
    assert_eq!(word(&memory, PROCESSOR + 3688), 0);
    assert_eq!(word(&memory, COORDINATOR + 184), 0);
    harness.run(
        "resource_state",
        "complete_close",
        &[CONTROL, 0],
        &mut memory,
    );
    harness.run("pressure", "pressure_service", &[PROCESSOR], &mut memory);
    assert_eq!(word(&memory, PROCESSOR + 3888), 0);
    assert_eq!(word(&memory, PROCESSOR + 3832), 7);
    assert_eq!(
        harness.run("counters", "pending_returns", &[PROCESSOR], &mut memory),
        [0]
    );
    assert_eq!(harness.decommitted, [(base + 4096, 8192)]);
    assert_eq!(harness.released, [(73728, 8192), (CONTROL, 4096)]);
    let before = memory.clone();
    harness.run("pressure", "pressure_service", &[PROCESSOR], &mut memory);
    assert_eq!(memory, before);
}

#[test]
fn pressure_reclaims_empty_shared_table_and_clears_reclaimable_bytes() {
    let output = compile();
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    let arena = 2097152;
    memory.resize((arena * 2 + 65536) as usize, 0);
    put(&mut memory, PROCESSOR + 3520, arena + 16);
    put(&mut memory, PROCESSOR + 3528, arena + 32768);
    put(&mut memory, PROCESSOR + 3592, arena);
    harness.reserve_script.extend([73728, 262144]);
    let payload = harness.run("shared", "gc_shared_alloc", &[1, 16, 8, 4], &mut memory)[0];
    harness.run(
        "shared",
        "gc_resolve_shared_handle",
        &[payload],
        &mut memory,
    );
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 1],
        &mut memory,
    );
    assert_eq!(word(&memory, PROCESSOR + 3688), 8192);
    assert_eq!(word(&memory, COORDINATOR + 184), 73728);
    put(&mut memory, PROCESSOR + 3952, 1);
    put(&mut memory, PROCESSOR + 3888, 2);
    for _ in 0..2 {
        harness.run("shared", "shared_sweep", &[], &mut memory);
    }
    assert_eq!(word(&memory, PROCESSOR + 3688), 0);
    assert_eq!(word(&memory, COORDINATOR + 184), 0);
    assert_eq!(harness.released, [(262144, 8192), (73728, 8192)]);
    assert_eq!(word(&memory, PROCESSOR + 3928), 8192);
    assert_eq!(word(&memory, PROCESSOR + 3960), 0);
    assert_eq!(word(&memory, PROCESSOR + 3976), 8192);
}

#[test]
fn shared_access_and_root_keep_payload_then_generation_rejects_stale_handle() {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../../../gugu-compiler/tests/fixtures/shared_pin.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    let arena = 2097152;
    let table = 73728;
    let mapping = 262144;
    memory.resize((arena * 2 + 65536) as usize, 0);
    put(&mut memory, PROCESSOR + 3520, arena + 16);
    put(&mut memory, PROCESSOR + 3528, arena + 32768);
    put(&mut memory, PROCESSOR + 3592, arena);
    harness.reserve_script.extend([table, mapping]);
    let payload = harness.run("shared", "gc_shared_alloc", &[1, 16, 8, 4], &mut memory)[0];
    let handle = harness.run(
        "shared",
        "gc_resolve_shared_handle",
        &[payload],
        &mut memory,
    )[0];
    assert_eq!(handle, 0xa001000000000001);
    assert!(harness.reserve_script.is_empty());
    assert_eq!(
        harness.run("shared", "gc_shared_access_begin", &[handle], &mut memory),
        [payload]
    );
    put(&mut memory, payload, ARENA + 16);
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 1],
        &mut memory,
    );
    assert!(
        !harness.released.contains(&(mapping, 8192)),
        "活动 guard 保活载荷"
    );
    harness.run("shared", "gc_shared_access_end", &[handle], &mut memory);
    put(&mut memory, ROOTS + 8, 1);
    put(&mut memory, ROOTS + 96, 2048);
    put(&mut memory, ROOTS + 104, 2);
    put(&mut memory, ROOTS + 112, handle);
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 1],
        &mut memory,
    );
    assert!(
        !harness.released.contains(&(mapping, 8192)),
        "handle 根保活载荷"
    );
    harness.reserve_result = 98304;
    let lease = harness.run("pinning", "pin_acquire", &[payload], &mut memory)[0];
    assert_ne!(lease, 0);
    assert_eq!(word(&memory, table + 64 + 32) >> 32, 1);
    put(&mut memory, ROOTS + 8, 0);
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 1],
        &mut memory,
    );
    assert!(
        !harness.released.contains(&(mapping, 8192)),
        "共享 pin 保活载荷"
    );
    harness.run("pinning", "pin_release", &[lease, payload], &mut memory);
    assert_eq!(word(&memory, table + 64 + 32) >> 32, 0);
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 1],
        &mut memory,
    );
    assert_eq!(
        harness
            .released
            .iter()
            .filter(|entry| **entry == (mapping, 8192))
            .count(),
        1
    );
    assert_eq!(word(&memory, table + 64) >> 32, 5);
    harness.reserve_result = mapping + 16384;
    let next = harness.run("shared", "gc_shared_alloc", &[1, 16, 8, 4], &mut memory)[0];
    let second = harness.run("shared", "gc_resolve_shared_handle", &[next], &mut memory)[0];
    assert_eq!(second, handle + 1);
    assert_eq!(
        harness.run("shared", "shared_payload", &[second], &mut memory),
        [next]
    );
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness.run("shared", "shared_payload", &[handle], &mut memory)
    }))
    .expect_err("旧代际不能命中新载荷");
    assert_eq!(
        failure
            .downcast_ref::<RuntimeExit>()
            .expect("必须是 runtime fatal")
            .0,
        2
    );
    assert_eq!(
        word(&memory, PROCESSOR + 3744),
        1,
        "成功 access 更新同一 resolve 计数"
    );
}
