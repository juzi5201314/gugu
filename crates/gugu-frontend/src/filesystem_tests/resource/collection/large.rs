use super::*;

#[test]
fn large_field_barrier_uses_registered_range_without_arena_probe() {
    let (hir, bodies) = runtime_bodies();
    let mut harness = ResourceHarness::from_bodies(&hir, &bodies);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, meta) = graph_memory();
    let payload = 6291456;
    memory.resize((payload + 4096) as usize, 0);
    let record = meta + 55576 + 64 * 64;
    put(&mut memory, record, payload - 4096);
    put(&mut memory, record + 8, payload);
    put(&mut memory, record + 16, 4096);
    put(&mut memory, record + 24, (1 << 32) | 1);
    put(&mut memory, payload - 16, 0x20000000000);
    put(&mut memory, payload - 8, 4096);
    put(&mut memory, PROCESSOR + 3592, ARENA);
    put(&mut memory, PROCESSOR + 3576, 98304);
    harness.run(
        "barrier",
        "gc_write_barrier",
        &[payload + 8, 0, ARENA + 16],
        &mut memory,
    );
    assert_eq!(
        word(&memory, 98304),
        0,
        "大对象由 minor 表扫描，不伪造 ordinary 卡片"
    );
}

#[test]
fn large_mapping_is_traced_then_released_once() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../../../gugu-compiler/tests/fixtures/pin.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, meta) = graph_memory();
    let base = 262144;
    let payload = base + 4096;
    let record = meta + 55576 + 64 * 64;
    let bytes = 45056;
    put(&mut memory, record, base);
    put(&mut memory, record + 8, payload);
    put(&mut memory, record + 16, 40000);
    put(&mut memory, record + 24, (1 << 32) | 1);
    put(&mut memory, record + 32, bytes);
    put(&mut memory, payload - 16, 1 | 0x20000000000);
    put(&mut memory, payload - 8, 40000);
    put(&mut memory, payload, ARENA + 16);
    put(&mut memory, payload + 8, 0);
    put(&mut memory, PROCESSOR + 3592, ARENA);
    for offset in [3920, 3928] {
        put(&mut memory, PROCESSOR + offset, 8192 + bytes);
    }
    put(&mut memory, PROCESSOR + 3960, bytes);
    put(&mut memory, PROCESSOR + 3936, 3);
    put(&mut memory, ROOTS + 8, 1);
    put(&mut memory, ROOTS + 96, 2048);
    put(&mut memory, ROOTS + 104, 1);
    put(&mut memory, ROOTS + 112, payload + 8);
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 1],
        &mut memory,
    );
    assert_eq!(
        word(&memory, meta + 24),
        0b101,
        "大对象内部根必须追踪到字段图"
    );
    assert!(!harness.released.contains(&(base, bytes)));
    harness.reserve_result = 73728;
    let lease = harness.run("pinning", "pin_acquire", &[payload + 8], &mut memory)[0];
    assert_ne!(lease, 0, "大对象必须登记 pin 租约");
    put(&mut memory, ROOTS + 8, 0);
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 1],
        &mut memory,
    );
    assert_eq!(word(&memory, meta + 24), 0b101);
    assert!(!harness.released.contains(&(base, bytes)));
    harness.run("pinning", "pin_release", &[lease, payload], &mut memory);
    for _ in 0..2 {
        harness.run(
            "collect",
            "gc_collect_major",
            &[PROCESSOR, ROOTS, 1],
            &mut memory,
        );
    }
    assert_eq!(
        harness
            .released
            .iter()
            .filter(|entry| **entry == (base, bytes))
            .count(),
        1
    );
    assert_eq!(word(&memory, record + 24) >> 32, 0);
    assert_eq!(word(&memory, PROCESSOR + 3960), 0);
    for offset in [3920, 3928, 3968, 3976] {
        assert_eq!(word(&memory, PROCESSOR + offset), 8192);
    }
    assert_eq!(word(&memory, PROCESSOR + 3936), 2);
}
