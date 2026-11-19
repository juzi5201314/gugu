//! 用固定 native 事件驱动实际 bind、epoll/IOCP 和 Resolver LIR。
use super::*;
const WAIT: &str = "std/runtime/net_wait.gg";
const NATIVE: &str = "std/runtime/net_platform.gg";
const H1: u64 = 128;
const H2: u64 = 256;
const W1: u64 = 1024 + 272;
const W2: u64 = 2048 + 272;

const DNS: &str = "std/runtime/net_resolver.gg";
fn resolver_call(h: &mut Harness<'_>, args: &[u64], memory: &mut Vec<u8>) -> Vec<u64> {
    let packet = args[1];
    let symbol = word(memory, packet);
    h.calls.push((symbol, args.to_vec()));
    if symbol == 100 {
        assert_eq!(word(memory, packet + 32), packet - 512 + 1536);
        let output = word(memory, packet + 40);
        put(memory, output, 3072);
        put(memory, packet + 128, 0);
        // 模拟原生解析已经产出结果、runtime 同时观察到取消。
        put(memory, 96, 1);
        h.invoke(DNS, "service", &[0], memory);
        vec![0, u64::MAX]
    } else {
        assert_eq!(symbol, 101);
        assert_eq!(word(memory, packet + 16), 3072);
        assert_eq!(args[9], 0, "释放迟到结果不得被原取消 token 再次取消");
        vec![0, 0]
    }
}

#[test]
fn resolver_cancellation_releases_late_result_and_wakes_only_once() {
    let output = compile(
        include_str!("../../../../gugu-compiler/tests/fixtures/net_resolve.gg"),
        TargetName::X86_64Linux,
    );
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    let node = h.allocate(&mut memory, 4096);
    put(&mut memory, node + 16, H1);
    put(&mut memory, node + 24, 96);
    h.invoke(DNS, "append", &[node], &mut memory);
    h.bridge_hook = Some(resolver_call);
    let result = h.invoke(DNS, "query", &[node], &mut memory);
    assert_eq!(result[0] & 0xff, 1, "取消应返回 Err");
    assert_eq!(
        h.calls.iter().map(|event| event.0).collect::<Vec<_>>(),
        [100, 101]
    );
    assert_eq!(h.woken, [H1]);
    assert_eq!(word(&memory, node + 16), 0);
    h.invoke(DNS, "service", &[0], &mut memory);
    assert_eq!(h.woken, [H1]);
    h.bridge_hook = Some(|_, args, memory| {
        put(memory, args[1] + 128, u64::from((-2i32) as u32));
        vec![0, 0]
    });
    let failed = h.allocate(&mut memory, 4096);
    put(&mut memory, failed + 16, H1);
    let missing = h.invoke(DNS, "query", &[failed], &mut memory);
    assert_eq!(missing[0] & 0xff, 1);
    assert_eq!(missing[1] & 0xff, 2, "系统 EAI_NONAME 必须返回 NotFound");
    for (code, tag) in [(-2i64, 2), (-3, 3), (-5, 2), (123456, 6)] {
        let result = h.invoke(DNS, "resolve_error", &[code as u64, 0], &mut memory);
        assert_eq!(result[0] & 0xff, tag);
        if tag == 6 {
            assert_eq!(result[1], code as u64);
        }
    }
}

fn compile(fixture: &str, target: TargetName) -> Compilation {
    let output =
        Compiler::shared().compile(CompileRequest::single_file("main.gg", fixture, target));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}
fn setup(h: &mut Harness<'_>, memory: &mut Vec<u8>) -> (u64, u64) {
    h.script.push_back((8, 10, 0));
    assert_eq!(h.invoke(WAIT, "initialize", &[], memory)[0], 0);
    let state = h.invoke(WAIT, "request", &[], memory)[0] - 2048;
    let control = h.allocate(memory, 4096);
    put(memory, H1 + 48, 1024);
    put(memory, H2 + 48, 2048);
    put(memory, control, 1);
    put(memory, control + 64, 42);
    put(memory, control + 72, 3);
    queue(h, memory, state, control);
    (state, control)
}
fn queue(h: &mut Harness<'_>, memory: &mut Vec<u8>, state: u64, control: u64) {
    put(memory, control + 80, 2);
    put(memory, control + 216, H1);
    put(memory, control + 224, H2);
    put(memory, control + 232, 1);
    put(memory, control + 240, 2);
    put(memory, W1, H2);
    put(memory, W1 + 8, 0);
    put(memory, W1 + 16, control);
    put(memory, W1 + 24, 96);
    put(memory, W2, 0);
    put(memory, W2 + 8, H1);
    put(memory, W2 + 16, control);
    put(memory, W2 + 24, 0);
    put(memory, state + 24, 2);
    h.invoke(WAIT, "append", &[control], memory);
}
#[expect(clippy::ptr_arg, reason = "签名需与 native_hook 一致")]
fn readiness(h: &mut Harness<'_>, op: u64, request: u64, memory: &mut Vec<u8>) {
    if op == 10 {
        let state = request - 2048;
        let event = word(memory, request + 8);
        let control = word(memory, state + 8);
        put(memory, event + 4, control);
    }
    if op == 4 {
        let addr = word(memory, request + 8);
        memory[addr as usize..addr as usize + 8].copy_from_slice(&[2, 0, 0x12, 0x34, 127, 0, 0, 1]);
        let length = word(memory, request + 16);
        put(memory, length, 16);
    }
    let _ = h;
}

#[test]
fn listener_fifo_cancel_and_close_execute_linux_state_machine() {
    let output = compile(
        include_str!("../../../../gugu-compiler/tests/fixtures/net_bind.gg"),
        TargetName::X86_64Linux,
    );
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    let (state, control) = setup(&mut h, &mut memory);
    put(&mut memory, 96, 1);
    h.invoke(WAIT, "cancel_waiters", &[], &mut memory);
    assert_eq!(h.woken, [H1]);
    assert_eq!(word(&memory, W1 + 40), u64::MAX);
    assert_eq!(word(&memory, control + 216), H2);
    assert_eq!(word(&memory, control + 80), 1);
    h.native_hook = Some(readiness);
    h.script.extend([(10, 1, 0), (4, 77, 0), (9, 0, 0)]);
    h.invoke(WAIT, "poll", &[state], &mut memory);
    assert_eq!(h.woken, [H1, H2]);
    assert_eq!(word(&memory, W2 + 32), 77);
    assert_eq!(word(&memory, control + 80), 0);
    assert_eq!(word(&memory, state + 24), 0);
    assert!(h.close_requests.is_empty());
    queue(&mut h, &mut memory, state, control);
    put(&mut memory, control + 8, 2);
    h.script.push_back((9, 0, 0));
    h.invoke(WAIT, "cancel_waiters", &[], &mut memory);
    assert_eq!(&h.woken[2..], [H1, H2]);
    assert_eq!(word(&memory, W1 + 40), u64::MAX - 1);
    assert_eq!(word(&memory, W2 + 40), u64::MAX - 1);
    assert_eq!(h.close_requests, [vec![control, 42, 3]]);
    assert!(h.script.is_empty());
}

#[test]
fn windows_accept_cancellation_retains_native_lease_until_completion() {
    let output = compile(
        include_str!("../../../../gugu-compiler/tests/fixtures/net_bind.gg"),
        TargetName::X86_64Windows,
    );
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    let (_, control) = setup(&mut h, &mut memory);
    h.script.extend([(0, 77, 0), (14, 0, 997)]);
    h.invoke(WAIT, "start_windows", &[control, H1], &mut memory);
    assert_eq!(word(&memory, control + 80), 3);
    put(&mut memory, 96, 1);
    h.script.push_back((17, 1, 0));
    h.invoke(WAIT, "cancel_waiters", &[], &mut memory);
    assert!(h.woken.is_empty());
    assert_eq!(word(&memory, control + 80), 3);
    h.script.push_back((7, 0, 0));
    h.invoke(WAIT, "complete_windows", &[control, 995], &mut memory);
    assert_eq!(h.woken, [H1]);
    assert_eq!(word(&memory, control + 80), 1);
    assert_eq!(word(&memory, control + 216), H2);
    assert_eq!(word(&memory, W1 + 40), u64::MAX);
    // close 先唤醒用户；已经提交的第二个 AcceptEx 必须收到完成包才能排入 OS close。
    h.script.extend([(0, 78, 0), (14, 0, 997)]);
    h.invoke(WAIT, "start_windows", &[control, H2], &mut memory);
    put(&mut memory, control + 8, 2);
    h.script.push_back((17, 1, 0));
    h.invoke(WAIT, "cancel_waiters", &[], &mut memory);
    assert_eq!(h.woken, [H1, H2]);
    assert!(h.close_requests.is_empty());
    assert_eq!(word(&memory, control + 80), 1);
    h.script.push_back((7, 0, 0));
    h.invoke(WAIT, "complete_windows", &[control, 995], &mut memory);
    assert_eq!(h.woken, [H1, H2]);
    assert_eq!(h.close_requests, [vec![control, 42, 3]]);
    assert!(h.script.is_empty());
}

#[test]
fn bind_options_configure_ipv6_and_reject_invalid_backlog_before_native() {
    let output = compile(
        include_str!("../../../../gugu-compiler/tests/fixtures/net_bind.gg"),
        TargetName::X86_64Linux,
    );
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    let words = h.invoke("main.gg", "numeric_v6", &[], &mut memory);
    let addr = 512;
    for (index, value) in words.into_iter().enumerate() {
        put(&mut memory, addr + index as u64 * 8, value);
    }
    let invalid = h.invoke(NATIVE, "bind", &[addr, 1, 0, 1, 0], &mut memory);
    assert_eq!(invalid[0] & 0xff, 1);
    assert_eq!(h.calls.len(), 0);
    h.native_hook = Some(|h, op, request, memory| {
        if op == 1 {
            let value = word(memory, word(memory, request + 24));
            h.calls.last_mut().unwrap().1.push(value);
        }
    });
    for dual in [0, 1] {
        h.calls.clear();
        h.script
            .extend([(0, 42, 0), (1, 0, 0), (1, 0, 0), (2, 0, 0), (3, 0, 0)]);
        let result = h.invoke(
            NATIVE,
            "bind",
            &[addr, 1, i64::MAX as u64, 1, dual],
            &mut memory,
        );
        assert_eq!(result[0] & 0xff, 0);
        assert_eq!(h.calls[0].1[..2], [10, 526337]);
        assert_eq!(h.calls[2].1[1..3], [41, 26]);
        assert_eq!(h.calls[2].1[9], 1 - dual);
        assert_eq!(h.calls[3].1[2], 28);
        assert_eq!(h.calls[4].1[..2], [42, i32::MAX as u64]);
    }
    h.calls.clear();
    h.script
        .extend([(0, 42, 0), (1, 0, 0), (1, 0, 92), (7, 0, 0)]);
    assert_eq!(
        h.invoke(NATIVE, "bind", &[addr, 1, 1024, 1, 1], &mut memory)[0] & 0xff,
        1
    );
    assert_eq!(h.calls.last().unwrap().0, 7);
    h.calls.clear();
    let scratch = 768;
    h.script.extend([(1, 0, 0), (1, 0, 0)]);
    h.invoke(NATIVE, "reuse", &[42, 1, 1, scratch], &mut memory);
    assert_eq!(word(&memory, scratch), 1);
    h.invoke(NATIVE, "reuse", &[42, 1, 0, scratch], &mut memory);
    assert_eq!(word(&memory, scratch), 0, "UDP 不允许活动地址共享");
    assert_eq!(h.calls[0].1[..5], [42, 1, 2, scratch, 4]);
    assert!(h.script.is_empty());
}
