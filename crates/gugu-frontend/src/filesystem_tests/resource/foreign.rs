//! 真正执行 foreign_dispatch → native_call → service，固定平台钩子只模拟线程完成。
use super::*;
const SOURCE: u64 = 2048;
const RAW: u64 = 12288;
const INPUT: u64 = 3072;
const OUTPUT: u64 = 3136;
const FRAME_BYTES: u64 = 272; // header176 + shadow32 + 两个32字节 fixup。
const FIXUPS: u64 = 208;
const RAW_BYTES: u64 = 336; // 两份24字节数据各自取整到16字节。

fn native_work(harness: &mut ResourceHarness<'_>, memory: &mut Vec<u8>) {
    assert_eq!(word(memory, POOL + 32), 1);
    assert_eq!(word(memory, COLD + 480), 4096, "排队 raw payload 真实计费");
    // 八个文件请求未完成，C 调用不能绕过共同额度。
    harness.run("fs_platform", "service", &[PROCESSOR], memory);
    assert!(harness.started.is_empty());
    assert_eq!(word(memory, POOL + 32), 1);
    // 固定脚本让七个请求退场，留下一个文件请求，C 调用取得唯一空位。
    for index in 1..8 {
        put(memory, REQUEST + index * 128 + 104, 0);
    }
    harness.run("fs_platform", "service", &[PROCESSOR], memory);
    let request = REQUEST + 128;
    assert_eq!(harness.started, [request]);
    assert_eq!(word(memory, request + 8), u64::MAX);
    assert_eq!(word(memory, request + 16), RAW);
    assert_eq!(word(memory, request + 104), HOT);
    let input = word(memory, RAW + 16);
    let output = word(memory, RAW + 24);
    assert_eq!(input, RAW + FRAME_BYTES);
    assert_eq!(output, RAW + FRAME_BYTES + 32);
    assert_eq!(
        &memory[input as usize..input as usize + 24],
        &memory[INPUT as usize..INPUT as usize + 24]
    );
    assert_eq!(
        word(memory, RAW + FIXUPS + 8),
        0,
        "native 包不保留协程栈输入地址"
    );
    assert_eq!(
        word(memory, RAW + FIXUPS + 40),
        0,
        "native 包不保留协程栈输出地址"
    );
    for index in 0..3 {
        put(memory, output + index * 8, 91 + index);
    }
    for (index, value) in [17, 18, 19, 20, 28].into_iter().enumerate() {
        put(memory, RAW + 128 + index as u64 * 8, value);
    }
    // 返回值已经写好，但尚未 join；额度、源结果和 raw 分配不能提前释放。
    harness.run("fs_platform", "service", &[PROCESSOR], memory);
    assert_eq!(word(memory, request + 104), HOT);
    assert_eq!(word(memory, OUTPUT), 0);
    assert!(harness.released.is_empty());
    // 模拟挂起栈整体迁移；native raw 包保持原地址。
    memory.copy_within(
        SOURCE as usize..(OUTPUT + 24) as usize,
        (SOURCE + 512) as usize,
    );
    put(memory, HOT + 80, SOURCE * 2 + 512);
    put(memory, REQUEST + 104, 0);
    harness.poll_complete = true;
    harness.run("fs_platform", "service", &[PROCESSOR], memory);
    assert_eq!(word(memory, request + 104), 0);
    assert_eq!(harness.woken, [HOT]);
}

#[test]
fn dirty_admission_is_independent_of_blocking_credit_and_observes_completion() {
    let mut imports: Vec<_> = crate::NativeLink::THREAD_SYMBOLS
        .into_iter()
        .map(|name| crate::NativeImport::function("libc.so.6", name))
        .collect();
    imports.extend(
        ["native_probe", "dirty_probe"]
            .map(|name| crate::NativeImport::function("libprobe.so", name)),
    );
    let output = Compiler::shared().compile(
        CompileRequest::single_file(
            "main.gg",
            include_str!("../../../../gugu-compiler/tests/fixtures/dirty_bridge.gg"),
            TargetName::X86_64Linux,
        )
        .with_native_link(crate::NativeLink {
            imports,
            interpreter: Some("/target/ld.so".into()),
        }),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = ResourceHarness::new(&output);
    harness.block_budget = 2048;
    harness.on_park = Some(dirty_work);
    let mut memory = request_memory();
    put(&mut memory, HOT + 80, SOURCE * 2);
    put(&mut memory, CANCEL, 0);
    put(&mut memory, SOURCE + 8, 0);
    for index in 0..8 {
        put(&mut memory, REQUEST + index * 128 + 104, HOT);
        put(&mut memory, REQUEST + index * 128 + 112, 0);
    }
    // 预留另一个 dirty 请求，新的请求只能排队；普通池即使满额也不影响 dirty 回收。
    put(&mut memory, REQUEST + 2048 + 104, HOT);
    put(&mut memory, POOL + 2048 + 1088, 131072);
    harness.run(
        "foreign",
        "foreign_dirty_dispatch",
        &[SOURCE, 176, 176, 0],
        &mut memory,
    );
    assert_eq!(word(&memory, SOURCE + 128), 77);
    assert_eq!(word(&memory, COLD + 480), 0);
    assert_eq!(harness.released, [(RAW, 4096)]);
}

fn dirty_work(harness: &mut ResourceHarness<'_>, memory: &mut Vec<u8>) {
    const DIRTY: u64 = REQUEST + 2048;
    harness.run("fs_platform", "service", &[PROCESSOR], memory);
    assert!(harness.started.is_empty());
    let waiting = harness.run("fs_platform", "dirty_snapshot", &[PROCESSOR], memory);
    assert_eq!(&waiting[..3], &[1, 1, 64]);
    assert_eq!(waiting[6], 4096);
    assert_eq!(
        harness.run("fs_platform", "snapshot", &[PROCESSOR], memory)[0],
        8
    );
    put(memory, DIRTY + 104, 0);
    harness.run("fs_platform", "service", &[PROCESSOR], memory);
    assert_eq!(harness.started, [DIRTY]);
    assert_eq!(word(memory, DIRTY + 24), 1);
    assert_eq!(word(memory, DIRTY + 16), RAW);
    assert_eq!(
        harness.run("fs_platform", "dirty_snapshot", &[PROCESSOR], memory)[..2],
        [1, 0]
    );
    put(memory, RAW + 128, 77);
    harness.run("fs_platform", "service", &[PROCESSOR], memory);
    assert_eq!(word(memory, DIRTY + 104), HOT);
    for index in 0..8 {
        put(memory, REQUEST + index * 128 + 104, 0);
    }
    harness.poll_complete = true;
    harness.run("fs_platform", "service", &[PROCESSOR], memory);
    assert_eq!(word(memory, DIRTY + 104), 0);
    assert_eq!(harness.woken, [HOT]);
    assert_eq!(
        harness.run("fs_platform", "dirty_snapshot", &[PROCESSOR], memory)[..2],
        [0, 0]
    );
}

#[test]
fn ordinary_c_and_files_share_admission_and_release_packet_after_completion() {
    let mut imports: Vec<_> = crate::NativeLink::THREAD_SYMBOLS
        .into_iter()
        .map(|name| crate::NativeImport::function("libc.so.6", name))
        .collect();
    imports.push(crate::NativeImport::function("libprobe.so", "native_probe"));
    let output = Compiler::shared().compile(
        CompileRequest::single_file(
            "main.gg",
            include_str!("../../../../gugu-compiler/tests/fixtures/foreign_bridge.gg"),
            TargetName::X86_64Linux,
        )
        .with_native_link(crate::NativeLink {
            imports,
            interpreter: Some("/target/ld.so".into()),
        }),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = ResourceHarness::new(&output);
    harness.reserve_result = RAW;
    harness.block_budget = 2048;
    harness.on_park = Some(native_work);
    let mut memory = request_memory();
    put(&mut memory, HOT + 80, SOURCE * 2);
    put(&mut memory, CANCEL, 0);
    for index in 0..8 {
        put(&mut memory, REQUEST + index * 128 + 104, HOT);
        put(&mut memory, REQUEST + index * 128 + 112, 0);
        put(&mut memory, POOL + 1088 + index * 8, 131072 + index * 65536);
    }
    put(&mut memory, SOURCE, 0xfeed);
    put(&mut memory, SOURCE + 8, 32);
    for (index, value) in [
        16,
        SOURCE * 2 - INPUT,
        24,
        0,
        24,
        SOURCE * 2 - OUTPUT,
        24,
        1,
    ]
    .into_iter()
    .enumerate()
    {
        put(&mut memory, SOURCE + FIXUPS + index as u64 * 8, value);
    }
    for index in 0..3 {
        put(&mut memory, INPUT + index * 8, 41 + index);
    }
    harness.run(
        "foreign",
        "foreign_dispatch",
        &[SOURCE, FRAME_BYTES, FIXUPS, 2],
        &mut memory,
    );
    assert_eq!(
        &memory[(OUTPUT + 512) as usize..(OUTPUT + 512 + 24) as usize],
        &[91u64, 92, 93]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect::<Vec<_>>()
    );
    for (index, value) in [17, 18, 19, 20, 28].into_iter().enumerate() {
        assert_eq!(word(&memory, SOURCE + 512 + 128 + index as u64 * 8), value);
    }
    assert_eq!(word(&memory, COLD + 480), 0);
    assert_eq!(harness.reserved, [(RAW, 4096)]);
    assert_eq!(harness.released, [(RAW, 4096)]);
    assert_eq!(
        word(&memory, COLD + 360),
        28,
        "C 错误状态保存在协程记录，不能被线程完成状态覆盖"
    );
    assert_eq!(RAW_BYTES, FRAME_BYTES + 64);
    harness.run("native_memory", "verify_drained", &[PROCESSOR], &mut memory);
}
