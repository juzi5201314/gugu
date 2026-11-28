use super::{CompileRequest, Compiler, TargetName, harness::Harness};
use crate::lir::body::{CallTarget, RuntimeCall};

fn channel_calls(
    harness: &mut Harness<'_>,
    target: &CallTarget,
    args: &[u64],
    memory: &mut Vec<u8>,
) -> Option<Vec<u64>> {
    let CallTarget::Runtime(call) = target else {
        return None;
    };
    let name = match call {
        RuntimeCall::ChannelNew => "channel_new",
        RuntimeCall::ChannelSend => "channel_send",
        RuntimeCall::ChannelReceive => "channel_receive",
        RuntimeCall::ChannelClose => "channel_close",
        RuntimeCall::ChannelTryRecv => "channel_try_recv",
        RuntimeCall::SelectCommit { .. } => "select_commit",
        _ => return None,
    };
    Some(harness.invoke("std/runtime/channel.gg", name, args, memory))
}

#[test]
fn successive_selects_bind_their_own_payloads() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/successive_selects.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = Harness::new(&output);
    let (hir, bodies) = super::harness::runtime_bodies();
    let runtime = Harness::from_bodies(hir, bodies);
    harness.bodies.extend(runtime.bodies);
    harness.names.extend(runtime.names);
    harness.call_hook = Some(channel_calls);
    harness.current = 128;
    let mut memory = vec![0; 65536];
    // 此 fixture 只有 int 载荷；Harness 的描述符替身以大小为地址。
    super::harness::put(&mut memory, 8, 8);
    super::harness::put(&mut memory, 128 + 16, 512);
    super::harness::put(&mut memory, 512 + 4048, 32768);
    assert_eq!(
        harness.invoke("main.gg", "sequential", &[], &mut memory),
        [42]
    );
    assert_eq!(
        harness.invoke("main.gg", "repeated", &[], &mut memory),
        [123]
    );
    assert_eq!(harness.invoke("main.gg", "nested", &[], &mut memory), [23]);
    assert_eq!(
        harness.invoke("main.gg", "nested_operand", &[], &mut memory),
        [42]
    );
    super::harness::put(&mut memory, 256 + 48, 1024);
    super::harness::put(&mut memory, 1024 + 368, 1);
    super::harness::put(&mut memory, 1024 + 376, 77);
    assert_eq!(
        harness.invoke("main.gg", "waited", &[256], &mut memory),
        [77]
    );
}

#[test]
fn loop_bindings_have_distinct_capture_cells() {
    for target in [TargetName::X86_64Linux, TargetName::Aarch64Macos] {
        let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
            "main.gg",
            include_str!("fixtures/loop_captures.gg"),
            target,
        ));
        assert!(output.is_success(), "{:?}", output.diagnostics().items());
        let mut harness = Harness::new(&output);
        let mut memory = vec![0; 65536];
        for (entry, expected) in [
            ("closures", 33),
            ("async_slots", 3),
            ("shared_outer", 16),
            ("or_pattern", 1),
        ] {
            assert_eq!(
                harness.invoke("main.gg", entry, &[], &mut memory),
                [expected],
                "{target:?}: {entry}"
            );
        }
    }
}
