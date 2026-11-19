//! 退役 TurnRegion 的根、pin 和字段图必须保持可达，最后解除后整段归还。
use super::*;

#[test]
fn region_field_barrier_and_minor_preserve_young_reference() {
    let (hir, bodies) = runtime_bodies();
    let mut harness = ResourceHarness::from_bodies(&hir, &bodies);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    let old = 2097152;
    let young = 4194304;
    let base = 7340032;
    memory.resize((base + 65536) as usize, 0);
    memory.copy_within(
        ARENA as usize..(ARENA + 2097152 + 65536) as usize,
        old as usize,
    );
    put(&mut memory, COORDINATOR + 48, old);
    put(&mut memory, PROCESSOR + 3592, young);
    put(&mut memory, PROCESSOR + 3520, young + 32);
    put(&mut memory, PROCESSOR + 3528, young + 32768);
    put(&mut memory, PROCESSOR + 3576, 98304);
    put(&mut memory, young + 8, 8);
    put(&mut memory, young + 16, 73);
    let meta = young + 2097152;
    put(&mut memory, meta + 24, 1);
    put(&mut memory, meta + 16, 1);
    for block in 1..64 {
        let state = (meta + 55576 + block * 64 + 56) as usize;
        memory[state..state + 4].copy_from_slice(&6u32.to_le_bytes());
    }
    harness.reserve_result = base;
    let payload = harness.run("heap", "gc_region_slow", &[1, 16, 16, 0], &mut memory)[0];
    harness.run(
        "barrier",
        "gc_write_barrier",
        &[payload, 0, young + 16],
        &mut memory,
    );
    put(&mut memory, payload, young + 16);
    put(&mut memory, ROOTS + 8, 0);
    harness.run(
        "collect",
        "gc_collect_minor",
        &[PROCESSOR, ROOTS, 1],
        &mut memory,
    );
    let moved = word(&memory, payload);
    assert!((old..old + 2097152).contains(&moved));
    assert_eq!(word(&memory, moved), 73);
    assert_eq!(word(&memory, 98304), 0, "区域不产生普通 arena 卡片");
}

#[test]
fn retired_region_keeps_fields_and_pin_until_last_root_then_releases_once() {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../../../gugu-compiler/tests/fixtures/pin.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, meta) = graph_memory();
    let base = 4194304;
    let second = base + 65536;
    memory.resize((second + 65536) as usize, 0);
    harness.reserve_result = base;
    let payload = harness.run("heap", "gc_region_slow", &[1, 16, 16, 0], &mut memory)[0];
    put(&mut memory, payload, ARENA + 16);
    harness.run("heap", "gc_region_publish", &[7, 1], &mut memory);
    harness.run("heap", "gc_region_promote", &[7], &mut memory);
    harness.reserve_result = second;
    harness.run("heap", "gc_region_slow", &[1, 16, 16, 0], &mut memory);
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
        "退役 region 的字段必须参与追踪"
    );
    harness.reserve_result = 73728;
    let lease = harness.run("pinning", "pin_acquire", &[payload + 8], &mut memory)[0];
    assert_ne!(lease, 0, "region pin 必须登记可达性租约");
    put(&mut memory, ROOTS + 8, 0);
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 1],
        &mut memory,
    );
    assert_eq!(word(&memory, meta + 24), 0b101);
    assert!(!harness.released.iter().any(|entry| entry.0 == base));
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
            .filter(|entry| entry.0 == base)
            .copied()
            .collect::<Vec<_>>(),
        [(base, 36864)]
    );
    assert!(
        !harness.released.iter().any(|entry| entry.0 == second),
        "活动分配区不能释放"
    );
    assert_eq!(word(&memory, PROCESSOR + 3960), 32768);
    assert_eq!(word(&memory, PROCESSOR + 3928), 8192 + 36864);
    assert_eq!(word(&memory, PROCESSOR + 3824), 16);
    assert_eq!(word(&memory, PROCESSOR + 3976), 8192);
}
