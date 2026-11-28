use super::{
    CompileRequest, Compiler, TargetName,
    harness::{Harness, put, word},
};

const SIGNAL: &str = "std/runtime/signal.gg";
const RESOURCE: &str = "std/runtime/resource.gg";
const USER1: u64 = 1 << 29;
const USER2: u64 = 1 << 30;
const HANDLER: u64 = 0x1234;

#[expect(clippy::ptr_arg, reason = "签名需与 bridge_hook 一致")]
fn linux_bridge(h: &mut Harness<'_>, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
    let (operation, value, error) = h.script.pop_front().unwrap();
    assert_eq!(args[0], operation);
    if operation == 14 {
        assert_eq!((args[1], args[3], args[4]), (0, 0, 8));
    } else {
        assert_eq!(operation, 289);
        assert_eq!((args[1], args[3], args[4]), (u64::MAX, 8, 524288));
    }
    assert_eq!(word(memory, args[2]), 1 << 9);
    vec![value, error]
}

#[test]
fn linux_signal_arm_passes_kernel_sigset_size_before_signalfd_flags() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../gugu-compiler/tests/fixtures/linux_signal.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    for (script, expected) in [
        (vec![(14, 0, 0), (289, 7, 0)], 0),
        (vec![(14, 0, 0), (289, 0, 22)], 22),
        (vec![(14, 0, 13)], 13),
    ] {
        let mut h = Harness::new(&output);
        let mut memory = vec![0; 262144];
        h.bridge_hook = Some(linux_bridge);
        assert_eq!(h.invoke(SIGNAL, "arm", &[0], &mut memory), [0]);
        h.script.extend(script);
        assert_eq!(h.invoke(SIGNAL, "arm", &[1 << 9], &mut memory), [expected]);
        assert!(h.script.is_empty());
    }
}

#[expect(clippy::ptr_arg, reason = "签名需与 native_hook 一致")]
fn native(h: &mut Harness<'_>, op: u64, request: u64, memory: &mut Vec<u8>) {
    let args = &mut h.calls.last_mut().unwrap().1;
    if op == 9 && args[2] == 1 {
        let change = word(memory, request + 8);
        args[6] = word(memory, change);
        args[7] = word(memory, change + 8);
        assert_eq!(word(memory, change + 16), 0);
    } else if op == 9 {
        assert_eq!(args[..3], [0, 0, 0]);
        assert_eq!(args[4], 5);
        assert_eq!((word(memory, args[5]), word(memory, args[5] + 8)), (0, 0));
        put(memory, args[3], 30);
        put(memory, args[3] + 16, 3);
    } else if op == 29 {
        let action = args[1];
        args[6] = word(memory, action);
        args[7] = word(memory, action + 8);
        if args[2] != 0 {
            assert_eq!((args[6], args[7]), (HANDLER, 2 << 32));
            put(memory, args[2], 1);
            put(memory, args[2] + 8, 2);
        }
    }
}

#[test]
fn darwin_signals_arm_deliver_counts_and_restore_disposition() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../gugu-compiler/tests/fixtures/macos_signal.gg"),
        TargetName::X86_64Macos,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    h.native_hook = Some(native);
    h.script.extend([
        (8, 0, 0),
        (26, 0, 0),
        (30, HANDLER, 0),
        (9, 0, 0),
        (29, 0, 0),
    ]);
    assert_eq!(h.invoke(SIGNAL, "arm", &[USER1], &mut memory), [0]);
    assert_eq!(h.calls[1].1[..3], [0, 2, 1]);
    assert_eq!(h.calls[3].1[6..8], [30, 0x21_fffa]);
    assert_eq!(h.invoke(SIGNAL, "arm", &[USER1], &mut memory), [0]);

    let page = h.invoke(SIGNAL, "control", &[], &mut memory)[0];
    let mut previous = 0;
    put(&mut memory, h.current + 16, 131072);
    let subscriptions = [(256, 768), (384, 896)].map(|(sub, ring)| {
        let cell = h.allocate(&mut memory, 4096);
        h.allocations.insert(cell, 4096);
        put(&mut memory, cell, 1);
        put(&mut memory, cell + 64, sub);
        put(&mut memory, cell + 72, 6);
        h.invoke(
            "std/runtime/resource_state.gg",
            "register",
            &[cell],
            &mut memory,
        );
        put(&mut memory, sub, previous);
        put(&mut memory, sub + 8, USER1);
        put(&mut memory, sub + 24, ring);
        put(&mut memory, sub + 32, 1);
        previous = sub;
        (sub, cell, ring)
    });
    put(&mut memory, page, previous);
    h.script.push_back((9, 1, 0));
    h.invoke(SIGNAL, "service", &[], &mut memory);
    for (_, cell, _) in subscriptions {
        assert_eq!(
            h.invoke(SIGNAL, "try_recv", &[cell], &mut memory),
            [30, 3, 0]
        );
    }

    h.script
        .extend([(30, HANDLER, 0), (9, 0, 0), (29, 0, 22), (9, 0, 0)]);
    assert_eq!(h.invoke(SIGNAL, "arm", &[USER2], &mut memory), [22]);
    assert_eq!(word(&memory, page + 16), USER1);
    assert_eq!(h.calls.last().unwrap().1[6..8], [31, 0x2_fffa]);

    let calls = h.calls.len();
    h.invoke(
        RESOURCE,
        "release_control",
        &[subscriptions[0].1],
        &mut memory,
    );
    assert_eq!(h.calls.len(), calls);
    assert_eq!(word(&memory, page + 16), USER1);

    h.script.extend([(29, 0, 0), (9, 0, 0), (7, 0, 0)]);
    h.invoke(
        RESOURCE,
        "release_control",
        &[subscriptions[1].1],
        &mut memory,
    );
    assert_eq!(h.calls.len(), calls + 3);
    let tail = &h.calls[h.calls.len() - 3..];
    assert_eq!((tail[0].1[0], tail[0].1[2]), (30, 0));
    assert_eq!(tail[0].1[6..8], [1, 2]);
    assert_eq!(tail[1].1[6..8], [30, 0x2_fffa]);
    assert_eq!(tail[2].1[0], 0);
    assert_eq!(h.invoke(SIGNAL, "pending", &[], &mut memory), [0]);
    assert!(h.script.is_empty());
}
