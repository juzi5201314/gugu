//! 文件错误不能写成协程完成状态；Join 必须等真实 finish_hot。
use super::*;

const CHILD: u64 = 256;
const CHILD_COLD: u64 = 2560;
const OUT: u64 = 7000;
const DESC: u64 = 7100;

#[test]
fn native_error_one_does_not_complete_join() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../../gugu-compiler/tests/fixtures/fs_join_error.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = ResourceHarness::new(&output);
    let mut memory = request_memory();
    put(&mut memory, CHILD + 16, PROCESSOR);
    put(&mut memory, CHILD + 48, CHILD_COLD);
    put(&mut memory, REQUEST + 104, CHILD);
    put(&mut memory, REQUEST + 112, 0);
    put(&mut memory, REQUEST + 72, 1);
    put(&mut memory, DESC, 8);
    put(&mut memory, PROCESSOR + 4000, 6000);
    put(&mut memory, PROCESSOR + 4032, 1);
    put(&mut memory, 6000 + 8, CHILD);
    put(&mut memory, CHILD_COLD + 392, 7);
    harness.poll_complete = true;
    harness.run("fs_platform", "service", &[PROCESSOR], &mut memory);
    assert_eq!(word(&memory, CHILD_COLD + 360), 1);
    assert_eq!(word(&memory, CHILD_COLD + 368), 0);
    harness.on_park = Some(|harness, memory| {
        assert_eq!(word(memory, CHILD_COLD + 512), HOT);
        harness.run("scheduler", "finish_hot", &[CHILD, 42, 0], memory);
    });
    assert_eq!(
        harness.run("channel", "join_wait", &[CHILD, OUT, DESC], &mut memory),
        [1]
    );
    assert!(harness.on_park.is_none(), "Join 必须等待真实完成");
    assert_eq!(word(&memory, OUT), 42);
    assert_eq!(word(&memory, CHILD_COLD + 360), 1, "完成不覆盖外调错误");
    assert_eq!(word(&memory, CHILD_COLD + 392), 7, "Join 登记不覆盖租约");
    assert_eq!(harness.woken, [CHILD, HOT]);
}
