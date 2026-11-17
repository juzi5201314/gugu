use super::*;

#[test]
fn output_drains_stderr_while_stdout_is_blocked() {
    let output = protocol_source(
        TargetName::X86_64Linux,
        include_str!("../../../../gugu-compiler/tests/fixtures/process_output.gg"),
    );
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    super::snapshot::setup(&mut h, &mut m);
    h.bridge_hook = Some(|_, args, m| {
        let request = word(m, args[1] + 16);
        let op = word(m, request);
        let result = match op {
            2 => 71,
            1 => {
                let first = if word(m, 2500) == 0 { 73 } else { 76 };
                put(m, 2500, 1);
                put(m, request + 24, first + 1);
                first
            }
            3 => word(m, request + 8) + 1,
            0 => {
                let config = word(m, request + 8);
                put(m, config + 56, 79);
                put(m, config + 64, 79);
                0
            }
            _ => panic!("未登记 worker {op}"),
        };
        put(m, request + 72, result);
        vec![0, 0]
    });
    h.script.extend((0..6).map(|_| (2, 0, 0)));
    h.script.extend([
        (3, 3, 0),
        (3, 0, 11),
        (3, 0, 0),
        (3, 3, 0),
        (3, 0, 0),
        (0, 1, 0),
        (2, 0, 0),
        (2, 0, 0),
    ]);
    h.native_hook = Some(|_, op, request, m| {
        if op == 3 && word(m, request + 72) == 3 {
            let handle = word(m, request + 8);
            let at = usize::try_from(word(m, request + 16)).unwrap();
            if handle == 76 {
                m[at..at + 3].copy_from_slice(b"err");
                put(m, 2512, 1);
            } else {
                assert_eq!(handle, 73);
                assert_eq!(word(m, 2512), 1, "stdout 必须等 stderr 先被读取");
                m[at..at + 3].copy_from_slice(b"out");
            }
        }
    });
    h.park_hook = Some(|h, m| {
        if h.now == 1000 {
            h.now = 1001;
            h.run_scheduled(1, m);
        }
        if h.script.front().is_some_and(|(op, _, _)| *op == 2) {
            h.invoke(STATE, "poll", &[0], m);
        }
        h.invoke(PIPE, "poll", &[0], m);
        if h.script.front().is_some_and(|(op, _, _)| *op == 0) || h.script.is_empty() {
            h.invoke(STATE, "poll", &[0], m);
        }
    });
    assert_eq!(h.invoke("main.gg", "captured", &[], &mut m), [1]);
    assert!(h.script.is_empty());
    h.invoke(CONTEXT, "shutdown", &[], &mut m);
    h.collect_unreachable_resources(&mut m);
    assert!(
        h.allocations.is_empty(),
        "请求、观察者及控制页必须全部归还：{:?}",
        h.allocations
    );
}
