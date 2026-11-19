//! 固定非零 cage 基址；实际 runtime 执行启动账本、checked 解码与 GC 字段回写。
use super::*;
const CAGE: u64 = 768;

#[test]
fn cage_capture_compilation_encodes_relative_to_runtime_base() {
    use crate::lir::body::{IntOp, Op};
    let output = Compiler::shared().compile(
        CompileRequest::single_file(
            "main.gg",
            include_str!("../../../../../gugu-compiler/tests/fixtures/runtime_cage.gg"),
            TargetName::X86_64Linux,
        )
        .with_compression_policy(
            crate::runtime::compression_schema::CompressionPolicyV1::cage(8388608),
        ),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let lir = output.lir.as_ref().unwrap();
    let make = super::super::super::named(&output, "make");
    assert!(make.instructions.iter().any(|i| matches!(
        i.op,
        Op::RuntimeMachineCall(crate::runtime::RuntimeMachineOp::CageControl)
    )));
    assert!(
        make.instructions
            .iter()
            .any(|i| matches!(i.op, Op::Integer(IntOp::Sub)))
    );
    assert!(lir.bodies().iter().any(|b| {
        b.instructions
            .iter()
            .any(|i| matches!(i.op, Op::DecodeCompressedRef))
    }));
    assert_eq!(
        output
            .raw_contract
            .as_ref()
            .unwrap()
            .compression()
            .cage_bytes(),
        8388608
    );
    assert!(output.image_plan().is_some());
}

#[test]
fn large_capture_uses_full_pointer_cells_with_cage_enabled() {
    let output = Compiler::shared().compile(
        CompileRequest::single_file(
            "main.gg",
            include_str!("../../../../../gugu-compiler/tests/fixtures/runtime_cage_large.gg"),
            TargetName::X86_64Linux,
        )
        .with_compression_policy(
            crate::runtime::compression_schema::CompressionPolicyV1::cage(8388608),
        ),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    assert!(output.lir.as_ref().unwrap().bodies().iter().all(|b| {
        b.instructions
            .iter()
            .all(|i| !matches!(i.op, crate::lir::body::Op::DecodeCompressedRef))
    }));
}

#[test]
fn cage_boot_slices_reservation_and_rejects_invalid_words() {
    let (hir, bodies) = runtime_bodies();
    let mut harness = ResourceHarness::from_bodies(&hir, &bodies);
    let mut memory = vec![0; 16384];
    put(&mut memory, HOT + 16, PROCESSOR);
    harness.run("compression", "cage_boot", &[], &mut memory);
    assert!(harness.reserved.is_empty());
    let base = 2097152;
    let length = 8388608;
    put(&mut memory, CAGE + 56, length);
    harness.reserve_result = base;
    harness.run("compression", "cage_boot", &[], &mut memory);
    assert_eq!(harness.reserved, [(base, length)]);
    assert_eq!(word(&memory, PROCESSOR + 3920), length);
    assert_eq!(word(&memory, PROCESSOR + 3928), 0);
    for expected in [base, base + 4194304] {
        assert_eq!(
            harness.run(
                "compression",
                "reserve_arena",
                &[2162688, 2097152],
                &mut memory
            ),
            [expected]
        );
    }
    assert_eq!(
        harness.run(
            "compression",
            "reserve_arena",
            &[2162688, 2097152],
            &mut memory
        ),
        [0]
    );
    assert_eq!(harness.reserved.len(), 1);
    let encoded = (1 << 32) | 96;
    assert_eq!(
        harness.run("compression", "encode", &[base + 96], &mut memory),
        [encoded]
    );
    assert_eq!(
        harness.run("compression", "decode", &[encoded], &mut memory),
        [base + 96]
    );
    assert_eq!(harness.run("compression", "decode", &[0], &mut memory), [0]);
    assert_eq!(word(&memory, CAGE + 32), 1);
    for invalid in [(2 << 32) | 96, (1 << 56) | encoded, (1 << 32) | length] {
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            harness.run("compression", "decode", &[invalid], &mut memory)
        }))
        .expect_err("失效压缩字必须拒绝");
        assert_eq!(failed.downcast_ref::<RuntimeExit>().unwrap().0, 2);
    }
    assert_eq!(word(&memory, CAGE + 40), 3);
}

#[test]
fn compressed_fields_and_roots_survive_minor_and_major() {
    const OLD: u64 = 2097152;
    const YOUNG: u64 = 6291456;
    const BARRIER: u64 = 98304;
    let (hir, bodies) = runtime_bodies();
    let mut harness = ResourceHarness::from_bodies(&hir, &bodies);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    let (mut memory, _) = graph_memory();
    memory.resize((YOUNG + 2097152 + 65536) as usize, 0);
    memory.copy_within(
        ARENA as usize..(ARENA + 2097152 + 65536) as usize,
        OLD as usize,
    );
    put(&mut memory, CAGE, 1);
    put(&mut memory, CAGE + 8, OLD);
    put(&mut memory, CAGE + 16, 8388608);
    put(&mut memory, CAGE + 24, (1 << 47) - OLD);
    let encode = |address| (1 << 32) | (address - OLD);
    put(&mut memory, COORDINATOR + 48, OLD);
    put(&mut memory, PROCESSOR + 3592, YOUNG);
    // cursor 恰好到达 nursery 末端仍属于原 arena，不能按 cursor 对齐到下一 arena。
    put(&mut memory, PROCESSOR + 3520, YOUNG + 2097152);
    put(&mut memory, PROCESSOR + 3528, YOUNG + 2097152);
    put(&mut memory, PROCESSOR + 3576, BARRIER);
    put(&mut memory, OLD, 1 | (2 << 36) | (3 << 43));
    put(&mut memory, OLD + 16, encode(YOUNG + 16));
    put(&mut memory, OLD + 24, encode(OLD + 16));
    put(&mut memory, OLD + 2097152 + 24, 1);
    put(&mut memory, YOUNG, 0);
    put(&mut memory, YOUNG + 8, 8);
    put(&mut memory, YOUNG + 16, 73);
    let meta = YOUNG + 2097152;
    put(&mut memory, meta + 24, 1);
    memory[(meta + 16) as usize..(meta + 20) as usize].copy_from_slice(&1u32.to_le_bytes());
    for block in 1..64 {
        let state = meta + 55576 + block * 64 + 56;
        memory[state as usize..state as usize + 4].copy_from_slice(&6u32.to_le_bytes());
    }
    harness.run(
        "barrier",
        "gc_write_barrier",
        &[OLD + 16, 0, encode(YOUNG + 16)],
        &mut memory,
    );
    assert_eq!(word(&memory, BARRIER), 1);
    put(&mut memory, ROOTS + 8, 1);
    put(&mut memory, ROOTS + 96, 90000);
    put(&mut memory, ROOTS + 104, 3);
    put(&mut memory, ROOTS + 112, encode(YOUNG + 16));
    put(&mut memory, 90000, encode(YOUNG + 16));
    assert_eq!(
        harness.run(
            "collect",
            "gc_collect_minor",
            &[PROCESSOR, ROOTS, 1],
            &mut memory
        ),
        [0]
    );
    assert_eq!(word(&memory, COORDINATOR + 40), YOUNG);
    let moved_word = word(&memory, OLD + 16);
    assert_eq!(word(&memory, 90000), moved_word);
    let moved = harness.run("compression", "decode", &[moved_word], &mut memory)[0];
    assert!((OLD..OLD + 2097152).contains(&moved));
    assert_eq!(word(&memory, moved), 73);
    put(&mut memory, ROOTS + 112, encode(OLD + 16));
    harness.run(
        "collect",
        "gc_collect_major",
        &[PROCESSOR, ROOTS, 1],
        &mut memory,
    );
    assert_eq!(word(&memory, moved), 73);
    assert_eq!(word(&memory, OLD + 16), moved_word);
    assert_eq!(word(&memory, CAGE + 40), 0);
}
