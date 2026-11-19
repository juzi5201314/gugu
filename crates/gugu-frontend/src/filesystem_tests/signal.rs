use super::{
    CompileRequest, Compiler, TargetName,
    harness::{Harness, put, word},
};

const SIGNAL: &str = "std/runtime/signal.gg";
const USER1: u64 = 1 << 29;
const USER2: u64 = 1 << 30;
const HANDLER: u64 = 0x1234;

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
    let output = Compiler::shared().compile(
        CompileRequest::single_file(
            "main.gg",
            include_str!("../../../gugu-compiler/tests/fixtures/macos_signal.gg"),
            TargetName::X86_64Macos,
        )
        .with_native_link(crate::NativeLink {
            imports: ["getpid", "kill"]
                .map(|name| crate::NativeImport::function("/usr/lib/libSystem.B.dylib", name))
                .to_vec(),
            interpreter: None,
        }),
    );
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
    let subscriptions = [(256, 512, 768), (384, 640, 896)];
    for (sub, cell, ring) in subscriptions {
        put(&mut memory, sub, previous);
        put(&mut memory, sub + 8, USER1);
        put(&mut memory, sub + 24, ring);
        put(&mut memory, sub + 32, 1);
        put(&mut memory, cell + 64, sub);
        previous = sub;
    }
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

    put(&mut memory, subscriptions[0].0 + 64, 1);
    h.invoke(SIGNAL, "mark_disarm", &[], &mut memory);
    h.invoke(SIGNAL, "disarm_closed", &[], &mut memory);
    assert_eq!(word(&memory, page + 16), USER1);

    put(&mut memory, subscriptions[1].0 + 64, 1);
    h.invoke(SIGNAL, "mark_disarm", &[], &mut memory);
    h.script.extend([(29, 0, 0), (9, 0, 0), (7, 0, 0)]);
    h.invoke(SIGNAL, "service", &[], &mut memory);
    let tail = &h.calls[h.calls.len() - 3..];
    assert_eq!((tail[0].1[0], tail[0].1[2]), (30, 0));
    assert_eq!(tail[0].1[6..8], [1, 2]);
    assert_eq!(tail[1].1[6..8], [30, 0x2_fffa]);
    assert_eq!(tail[2].1[0], 0);
    assert_eq!(h.invoke(SIGNAL, "pending", &[], &mut memory), [0]);
    assert!(h.script.is_empty());
}
