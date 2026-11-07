//! 普通外调留存 cage 地址前，消费真实 pin 登记；失败不能泄漏 raw 请求包。
use super::*;

#[test]
fn foreign_cage_address_requires_live_pin_before_submission() {
    let mut imports: Vec<_> = crate::NativeLink::THREAD_SYMBOLS
        .into_iter()
        .map(|name| ("libc.so.6".into(), name.into()))
        .collect();
    imports.push(("libprobe.so".into(), "native_probe".into()));
    let output = Compiler::shared().compile(
        CompileRequest::single_file(
            "main.gg",
            include_str!("../../../../../gugu-compiler/tests/fixtures/foreign_pointer.gg"),
            TargetName::X86_64Linux,
        )
        .with_compression_policy(
            crate::runtime::compression_schema::CompressionPolicyV1::cage(8388608),
        )
        .with_native_link(crate::NativeLink {
            imports,
            interpreter: Some("/target/ld.so".into()),
        }),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = ResourceHarness::new(&output);
    harness.gc_config = CONFIG;
    harness.block_budget = 32768;
    harness.reserve_result = 98304;
    let (mut memory, _) = graph_memory();
    put(&mut memory, 768, 1);
    put(&mut memory, 768 + 8, ARENA);
    put(&mut memory, 768 + 16, 2097152);
    put(&mut memory, HOT + 80, 4096);
    let pointer = ARENA + 16;
    put(&mut memory, 2048 + 16, pointer);
    for (index, value) in [16, pointer, 0, 2].into_iter().enumerate() {
        put(&mut memory, 2048 + 176 + index as u64 * 8, value);
    }
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness.run(
            "foreign",
            "foreign_dispatch",
            &[2048, 208, 176, 1],
            &mut memory,
        )
    }))
    .expect_err("无租约的 cage 地址不可发布到 native");
    assert_eq!(failed.downcast_ref::<RuntimeExit>().unwrap().0, 2);
    assert!(harness.started.is_empty());
    assert_eq!(harness.released, [(98304, 4096)]);
    assert_eq!(
        harness.run("counters", "read", &[PROCESSOR, 24], &mut memory),
        [1]
    );
    harness.reserve_result = 73728;
    let lease = harness.run("pinning", "pin_acquire", &[pointer], &mut memory)[0];
    assert_eq!(
        harness.run("pinning", "foreign_save", &[pointer + 8], &mut memory),
        [1]
    );
    assert_eq!(
        harness.run("counters", "read", &[PROCESSOR, 22], &mut memory),
        [1]
    );
    harness.run("pinning", "pin_release", &[lease, pointer], &mut memory);
    assert_eq!(
        harness.run("pinning", "foreign_save", &[pointer], &mut memory),
        [0]
    );
    assert_eq!(
        harness.run("counters", "read", &[PROCESSOR, 23], &mut memory),
        [0]
    );
    harness.run(
        "foreign",
        "foreign_copy",
        &[90000, pointer, 16],
        &mut memory,
    );
    assert_eq!(
        &memory[90000..90016],
        &memory[pointer as usize..pointer as usize + 16]
    );
    assert_eq!(
        harness.run("counters", "read", &[PROCESSOR, 23], &mut memory),
        [1]
    );
    let before = memory[90000..90016].to_vec();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness.run(
            "foreign",
            "foreign_copy",
            &[90000, ARENA + 2097152 - 8, 16],
            &mut memory,
        )
    }))
    .expect_err("跨 cage 边界复制必须拒绝");
    assert_eq!(failed.downcast_ref::<RuntimeExit>().unwrap().0, 2);
    assert_eq!(&memory[90000..90016], before);
    assert_eq!(
        harness.run("counters", "read", &[PROCESSOR, 23], &mut memory),
        [1]
    );
    assert_eq!(
        harness.run("counters", "read", &[PROCESSOR, 24], &mut memory),
        [3]
    );
}
