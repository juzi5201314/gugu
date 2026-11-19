//! 固定原生事件解释真实 transport LIR，不建立 socket 或线程。
use super::*;
const TRANSPORT: &str = "std/runtime/net_transport.gg";
const CONTROL: u64 = 512;
const CELL: u64 = 64;
const BUFFER: u64 = 128;
const SLOT: u64 = 256;

fn compile(target: TargetName) -> Compilation {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../../gugu-compiler/tests/fixtures/net_io.gg"),
        target,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    output
}
fn setup(h: &mut Harness<'_>, memory: &mut [u8]) {
    put(memory, CELL, CONTROL);
    put(memory, CONTROL, 1);
    put(memory, CONTROL + 64, 42);
    put(memory, CONTROL + 72, 3);
    put(memory, CONTROL + 240, 2);
    h.current = SLOT;
    h.park_hook = Some(|h, memory| {
        h.invoke(TRANSPORT, "service", &[], memory);
    });
}
fn node(h: &mut Harness<'_>, memory: &mut Vec<u8>, op: u64, length: u64, cancel: u64) -> u64 {
    h.invoke(
        TRANSPORT,
        "allocate",
        &[CONTROL, op, BUFFER, length, cancel],
        memory,
    )[0]
}
fn submit(h: &mut Harness<'_>, memory: &mut Vec<u8>, node: u64) -> Vec<u64> {
    h.invoke(TRANSPORT, "submit", &[node], memory)
}

fn control_contract(target: TargetName) {
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        include_str!("../../../../gugu-compiler/tests/fixtures/net_control.gg"),
        target,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    setup(&mut h, &mut memory);
    let values = h.invoke("main.gg", "candidates", &[], &mut memory);
    put(&mut memory, 2048, values[0]);
    let ordered = h.invoke(
        "std/runtime/net_connect.gg",
        "ordered",
        &[2048],
        &mut memory,
    );
    for (index, expected) in [1, 3, 2, 4].into_iter().enumerate() {
        assert_eq!(
            h.invoke(
                "main.gg",
                "candidate_port",
                &[ordered[0], index as u64],
                &mut memory
            ),
            [expected]
        );
    }
    let options = "std/runtime/net_options.gg";
    for (option, value) in [(2, 0), (2, 256), (3, 0), (4, u64::MAX), (7, 256), (10, 0)] {
        let result = h.invoke(options, "set", &[CELL, option, value], &mut memory);
        assert_eq!(result[0] & 255, 1);
        assert_eq!(result[1] & 255, 3);
    }
    h.native_hook = Some(|h, op, request, memory| {
        if op == 1 {
            let scalar = word(memory, word(memory, request + 24)) & u64::from(u32::MAX);
            h.calls.last_mut().unwrap().1.push(scalar);
        } else if op == 23 {
            let address = word(memory, request + 24);
            put(memory, address, 16384);
        }
    });
    h.script.extend([(1, 0, 0), (23, 0, 0)]);
    assert_eq!(
        h.invoke(options, "set", &[CELL, 3, 8192], &mut memory)[0] & 255,
        0
    );
    assert_eq!(h.calls.last().unwrap().1[9], 8192);
    assert_eq!(
        h.invoke(options, "get", &[CELL, 3], &mut memory)[1],
        16384,
        "getter 返回内核值"
    );
    let unix = "std/runtime/net_unix.gg";
    let address = 3072;
    put(&mut memory, address, 1);
    assert_eq!(
        h.invoke(unix, "decode", &[address, 2], &mut memory)[0] & 255,
        0
    );
    memory[address as usize + 2..address as usize + 10].copy_from_slice(b"/tmp/abc");
    let decoded = h.invoke(unix, "decode", &[address, 11], &mut memory);
    assert_eq!(decoded[0] & 255, 0);
    for length in [1, 111] {
        assert_eq!(
            h.invoke(unix, "decode", &[address, length], &mut memory)[0] & 255,
            1
        );
    }
    assert!(h.script.is_empty());
    let connection = node(&mut h, &mut memory, 4, 0, 0);
    put(&mut memory, connection + 96, 16);
    h.native_hook = Some(|h, op, request, memory| {
        if op == 9 && h.calls.last().unwrap().1[1] == 3355443206 {
            assert_eq!(word(memory, word(memory, request + 16)), 0x4660ddf325a207b9);
            let out = word(memory, request + 32);
            put(memory, out, 100);
        }
    });
    h.park_hook = Some(|h, memory| {
        let node = word(memory, CONTROL + 1232);
        if word(memory, 96) == 0 {
            h.script.extend([(21, 0, 115), (9, 0, 0)]);
            h.invoke(TRANSPORT, "service", &[], memory);
            h.script.extend([(23, 0, 0), (6, 0, 0), (9, 0, 0)]);
            h.invoke(TRANSPORT, "ready", &[CONTROL], memory);
        } else {
            h.script
                .extend([(8, 10, 0), (2, 0, 0), (9, 0, 0), (31, 0, 997)]);
            h.invoke(TRANSPORT, "service", &[], memory);
            assert_eq!(word(memory, CONTROL + 80), 1);
            h.script.push_back((1, 0, 0));
            h.invoke(TRANSPORT, "complete_windows", &[node, 0, 0], memory);
            assert_eq!(&h.calls.last().unwrap().1[..3], &[42, 65535, 28688]);
        }
    });
    put(
        &mut memory,
        96,
        u64::from(target == TargetName::X86_64Windows),
    );
    assert_eq!(submit(&mut h, &mut memory, connection)[0] & 255, 0);
    assert_eq!(word(&memory, CONTROL + 80), 0);
    assert!(h.script.is_empty());
    h.woken.clear();
    let event = 3000;
    h.invoke("std/runtime/net_event.gg", "notify", &[event], &mut memory);
    assert_eq!(
        h.invoke(
            "std/runtime/net_event.gg",
            "wait",
            &[event, 0, 0, 0],
            &mut memory
        ),
        [0],
        "完成先于注册不能丢唤醒"
    );
    h.park_hook = Some(|h, memory| {
        h.invoke("std/runtime/net_event.gg", "notify", &[3000], memory);
        h.invoke("std/runtime/net_event.gg", "notify", &[3000], memory);
        assert_eq!(h.woken, [SLOT]);
    });
    assert_eq!(
        h.invoke(
            "std/runtime/net_event.gg",
            "wait",
            &[event, 1, 0, 0],
            &mut memory
        ),
        [0]
    );
    assert_eq!(
        h.invoke("std/runtime/net_event.gg", "pending", &[], &mut memory),
        [0]
    );
    h.park_hook = Some(|h, memory| {
        h.now = 2000;
        h.invoke("std/runtime/net_event.gg", "service", &[], memory);
    });
    assert_eq!(
        h.invoke(
            "std/runtime/net_event.gg",
            "wait",
            &[event, 3, 2000, 0],
            &mut memory
        ),
        [u64::MAX - 2]
    );
}

#[test]
fn linux_connection_order_options_and_unix_addresses_follow_contract() {
    control_contract(TargetName::X86_64Linux);
}
#[test]
fn windows_connection_order_options_and_unix_addresses_follow_contract() {
    control_contract(TargetName::X86_64Windows);
}

#[test]
fn transport_cancel_cursor_continues_past_one_service_budget() {
    let output = compile(TargetName::X86_64Linux);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    setup(&mut h, &mut memory);
    // 只建立128项预算边界两侧的固定请求头，缓冲和原生操作均为空。
    let mut last = 0;
    for index in 0..129 {
        let n = h.allocate(&mut memory, 384);
        put(&mut memory, n + 16, CONTROL);
        put(&mut memory, n + 104, 1);
        h.invoke(TRANSPORT, "enqueue", &[n, SLOT + index], &mut memory);
        last = n;
    }
    put(&mut memory, CONTROL + 80, 129);
    put(&mut memory, 96, 1);
    put(&mut memory, last + 32, 96);
    h.invoke(TRANSPORT, "service", &[], &mut memory);
    assert!(h.woken.is_empty());
    h.invoke(TRANSPORT, "service", &[], &mut memory);
    assert_eq!(h.woken, [SLOT + 128]);
    assert_eq!(word(&memory, CONTROL + 80), 128);
    put(&mut memory, CONTROL + 8, 2);
    h.invoke(TRANSPORT, "service", &[], &mut memory);
    assert_eq!(h.woken.len(), 129);
    assert_eq!(h.close_requests, [vec![CONTROL, 42, 3]]);
    assert_eq!(h.invoke(TRANSPORT, "pending", &[], &mut memory), [0]);
}

#[test]
fn linux_transport_short_io_eof_shutdown_and_closed_are_distinct() {
    let output = compile(TargetName::X86_64Linux);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    setup(&mut h, &mut memory);
    for (op, syscall, result) in [(0, 24, 3), (1, 25, 2), (0, 24, 0)] {
        let n = node(&mut h, &mut memory, op, 8, 0);
        h.script.push_back((syscall, result, 0));
        let value = submit(&mut h, &mut memory, n);
        assert_eq!(value[0] & 255, 0);
        assert_eq!(value[1], result);
        assert_eq!(word(&memory, CONTROL + 80), 0);
    }
    let waiting = node(&mut h, &mut memory, 0, 8, 0);
    h.invoke(TRANSPORT, "enqueue", &[waiting, SLOT], &mut memory);
    put(&mut memory, CONTROL + 80, 1);
    h.invoke(TRANSPORT, "close_read", &[CONTROL], &mut memory);
    assert_eq!(word(&memory, waiting + 72), 0);
    assert_eq!(word(&memory, waiting + 80), 0);
    put(&mut memory, CONTROL + 1256, 3);
    put(&mut memory, CONTROL + 1552, 1);
    let eof = h.invoke(
        "std/runtime/net_endpoint.gg",
        "read",
        &[CELL, BUFFER, 8, 0],
        &mut memory,
    );
    assert_eq!(&eof[..2], &[0, 0]);
    put(&mut memory, CONTROL + 1552, 0);
    let n = node(&mut h, &mut memory, 0, 8, 0);
    assert_eq!(submit(&mut h, &mut memory, n)[1], 0);
    let n = node(&mut h, &mut memory, 1, 8, 0);
    let broken = submit(&mut h, &mut memory, n);
    assert_eq!(broken[0] & 255, 1);
    assert_eq!(broken[1] & 255, 11);
    let n = node(&mut h, &mut memory, 5, 0, 0);
    assert_eq!(
        submit(&mut h, &mut memory, n)[0] & 255,
        0,
        "重复半关闭无系统调用"
    );
    put(&mut memory, CONTROL + 8, 2);
    let n = node(&mut h, &mut memory, 0, 0, 0);
    let closed = submit(&mut h, &mut memory, n);
    assert_eq!(closed[0] & 255, 1);
    assert_eq!(closed[1] & 255, 10);
    assert!(h.script.is_empty());
    assert_eq!(h.invoke(TRANSPORT, "pending", &[], &mut memory), [0]);
}

#[test]
fn linux_transport_fifo_readiness_and_opposite_direction_progress() {
    let output = compile(TargetName::X86_64Linux);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    setup(&mut h, &mut memory);
    let read = node(&mut h, &mut memory, 0, 8, 0);
    let write = node(&mut h, &mut memory, 1, 8, 0);
    put(&mut memory, 96, write);
    h.park_hook = Some(|h, memory| {
        h.script.extend([(24, 0, 11), (9, 0, 0)]);
        h.invoke(TRANSPORT, "service", &[], memory);
        let read = word(memory, CONTROL + 1216);
        assert!(h.woken.is_empty());
        h.invoke(TRANSPORT, "service", &[], memory);
        assert!(h.script.is_empty(), "等待 readiness 时不重复读取");
        h.current = SLOT + 1;
        h.park_hook = Some(|h, memory| {
            h.script.push_back((25, 2, 0));
            h.invoke(TRANSPORT, "service", &[], memory);
        });
        let write = word(memory, 96);
        assert_eq!(submit(h, memory, write)[0] & 255, 0);
        assert_eq!(h.woken, [SLOT + 1], "写方向独立推进");
        h.current = SLOT + 2;
        h.park_hook = Some(|h, memory| {
            h.script.push_back((24, 3, 0));
            h.invoke(TRANSPORT, "ready", &[CONTROL], memory);
            assert_eq!(h.woken, [SLOT + 1, SLOT]);
            h.script.extend([(24, 4, 0), (9, 0, 0)]);
            h.invoke(TRANSPORT, "service", &[], memory);
        });
        let second = node(h, memory, 0, 8, 0);
        assert_eq!(submit(h, memory, second)[1], 4);
        assert_eq!(word(memory, read + 80), 3);
    });
    assert_eq!(submit(&mut h, &mut memory, read)[1], 3);
    assert_eq!(h.woken, [SLOT + 1, SLOT, SLOT + 2]);
    assert_eq!(word(&memory, CONTROL + 80), 0);
    assert!(h.script.is_empty());
    h.woken.clear();
    let read = node(&mut h, &mut memory, 2, 8, 0);
    let send = node(&mut h, &mut memory, 3, 8, 0);
    let connect = node(&mut h, &mut memory, 7, 0, 0);
    let later = node(&mut h, &mut memory, 2, 8, 0);
    put(&mut memory, read + 104, 1);
    put(&mut memory, later + 320, 1);
    for (index, request) in [read, send, connect, later].into_iter().enumerate() {
        h.invoke(
            TRANSPORT,
            "enqueue",
            &[request, SLOT + index as u64],
            &mut memory,
        );
    }
    put(&mut memory, CONTROL + 80, 4);
    h.script.push_back((25, 8, 0));
    h.invoke(TRANSPORT, "service", &[], &mut memory);
    assert_eq!(h.woken, [SLOT + 1], "连接屏障等待此前读，但不阻塞此前写");
    h.script.extend([(24, 1, 0), (21, 0, 0)]);
    h.invoke(TRANSPORT, "ready", &[CONTROL], &mut memory);
    h.invoke(TRANSPORT, "service", &[], &mut memory);
    assert_eq!(h.woken, [SLOT + 1, SLOT, SLOT + 2, SLOT + 3]);
    assert_eq!(
        word(&memory, later + 72),
        u64::MAX - 3,
        "控制线性化之后复查模式"
    );
    assert_eq!(word(&memory, CONTROL + 1248), 1);
    assert_eq!(word(&memory, CONTROL + 1288), 0);
    let disconnect = node(&mut h, &mut memory, 8, 0, 0);
    h.park_hook = Some(|h, memory| {
        h.invoke(TRANSPORT, "service", &[], memory);
    });
    h.script.push_back((21, 0, 0));
    assert_eq!(submit(&mut h, &mut memory, disconnect)[0] & 255, 0);
    assert_eq!(word(&memory, CONTROL + 1248), 0);
    assert!(h.script.is_empty());
}

#[test]
fn transport_deadline_is_captured_and_cancelled_wait_does_not_close() {
    let output = compile(TargetName::X86_64Linux);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    setup(&mut h, &mut memory);
    put(&mut memory, CONTROL + 1536, 0);
    put(&mut memory, CONTROL + 1544, 10);
    put(&mut memory, CONTROL + 1552, 1);
    let n = node(&mut h, &mut memory, 0, 8, 96);
    assert_eq!(word(&memory, n + 40), 1010);
    put(&mut memory, CONTROL + 1544, 1000);
    h.park_hook = Some(|h, memory| {
        h.now = 1010;
        h.invoke(TRANSPORT, "service", &[], memory);
    });
    let result = submit(&mut h, &mut memory, n);
    assert_eq!(result[0] & 255, 1);
    assert_eq!(result[1] & 255, 8, "超时返回 TimedOut");
    assert_eq!(word(&memory, CONTROL + 8), 0);
    put(&mut memory, 96, 1);
    let n = node(&mut h, &mut memory, 0, 8, 96);
    let result = submit(&mut h, &mut memory, n);
    assert_eq!(result[1] & 255, 9);
    assert_eq!(h.woken, [SLOT], "预取消不入队");
    assert_eq!(word(&memory, CONTROL + 80), 0);
    put(&mut memory, CONTROL + 1536, u64::MAX);
    assert_eq!(
        h.invoke(
            "std/runtime/net_deadline.gg",
            "capture",
            &[CONTROL, 1536],
            &mut memory
        ),
        [u64::MAX]
    );
    put(&mut memory, CONTROL + 1552, 0);
    assert_eq!(
        h.invoke(
            "std/runtime/net_deadline.gg",
            "capture",
            &[CONTROL, 1536],
            &mut memory
        ),
        [0]
    );
}

#[test]
fn darwin_datagram_truncation_and_disconnect_follow_native_results() {
    let output = compile(TargetName::X86_64Macos);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    setup(&mut h, &mut memory);
    h.native_hook = Some(|_, op, request, memory| {
        if op == 27 {
            let message = word(memory, request + 8);
            let iovec = word(memory, message + 16);
            assert_eq!(word(memory, iovec + 8), 2);
            let data = word(memory, iovec) as usize;
            memory[data..data + 2].copy_from_slice(b"AA");
            let address = word(memory, message) as usize;
            memory[address..address + 8].copy_from_slice(&[16, 2, 0, 80, 127, 0, 0, 1]);
            put(memory, message + 8, 16);
            put(memory, message + 40, 16 << 32);
        } else if op == 21 {
            let address = word(memory, request + 8) as usize;
            let family = word(memory, request + 16);
            if family == 2 {
                assert_eq!(&memory[address..address + 2], &[2, 0]);
            } else {
                assert_eq!(&memory[address..address + 2], &[16, 2]);
            }
        } else if op == 28 {
            assert_eq!(word(memory, request), 42);
            assert_eq!(word(memory, request + 8), 0);
            assert_eq!(word(memory, request + 16), 0);
        }
    });
    h.script.push_back((27, 2, 0));
    let result = h.invoke(
        "std/runtime/net_datagram.gg",
        "receive",
        &[CELL, BUFFER, 2, 0, 0],
        &mut memory,
    );
    assert_eq!(result[0] & 255, 0);
    assert_eq!(result[1], 2);
    assert_ne!(result.last().unwrap() & 255, 0);
    assert_eq!(&memory[BUFFER as usize..BUFFER as usize + 2], b"AA");

    check_darwin_ip_disconnect(&mut h, &mut memory);
    check_darwin_peer_after_failure(&mut h, &mut memory);
    put(&mut memory, CONTROL + 240, 1);
    for (peer_error, connected) in [(0, 1), (57, 0)] {
        put(&mut memory, CONTROL + 1248, 1);
        let disconnect = node(&mut h, &mut memory, 8, 0, 0);
        put(&mut memory, disconnect + 96, 2);
        h.script.extend([(21, 0, 22), (6, 0, peer_error)]);
        submit(&mut h, &mut memory, disconnect);
        assert_eq!(word(&memory, CONTROL + 1248), connected);
        assert_eq!(word(&memory, disconnect + 72), connected * 22);
    }
    assert!(h.script.is_empty());
}

fn check_darwin_peer_after_failure(h: &mut Harness<'_>, memory: &mut Vec<u8>) {
    put(memory, CONTROL + 240, 2);
    put(memory, CONTROL + 1304, 1);
    for (peer_error, connected) in [(0, 1), (57, 0), (4, 1)] {
        put(memory, CONTROL + 1248, 1);
        let reconnect = node(h, memory, 7, 0, 0);
        put(memory, reconnect + 96, 16);
        put(memory, reconnect + 128, 0x0210);
        h.script.extend([(21, 0, 49), (6, 0, peer_error)]);
        submit(h, memory, reconnect);
        assert_eq!(word(memory, reconnect + 72), 49);
        assert_eq!(word(memory, CONTROL + 1248), connected);
        let unconnected = h.invoke(
            "std/runtime/net_endpoint.gg",
            "connected",
            &[CELL, 0],
            memory,
        );
        assert_eq!(unconnected[0] & 255, connected);
        if connected == 0 {
            let disconnect = node(h, memory, 8, 0, 0);
            submit(h, memory, disconnect);
            assert_eq!(word(memory, disconnect + 72), 0);
        }
    }
    put(memory, CONTROL + 1304, 0);
    let disconnect = node(h, memory, 8, 0, 0);
    h.script.extend([(28, 0, 22), (6, 0, 57)]);
    submit(h, memory, disconnect);
    assert_eq!(word(memory, disconnect + 72), 22);
    assert_eq!(word(memory, CONTROL + 1248), 0);
}

fn check_darwin_ip_disconnect(h: &mut Harness<'_>, memory: &mut Vec<u8>) {
    for (ipv6, family) in [(0, 2), (1, 30)] {
        put(memory, CONTROL + 240, family);
        for (explicit, initial, status, connected) in [(0, 1, 0, 0), (1, 1, 45, 1), (1, 0, 0, 0)] {
            put(memory, CONTROL + 1304, 0);
            let words = h.invoke("main.gg", "udp_binding", &[ipv6, explicit], memory);
            let address = h.allocate(memory, words.len() * 8);
            for (index, value) in words.into_iter().enumerate() {
                put(memory, address + index as u64 * 8, value);
            }
            h.invoke(
                TRANSPORT,
                "record_datagram_binding",
                &[CONTROL, address],
                memory,
            );
            put(memory, CONTROL + 1248, initial);
            let disconnect = node(h, memory, 8, 0, 0);
            if explicit == 0 {
                h.script.push_back((28, 0, 0));
            }
            submit(h, memory, disconnect);
            assert_eq!(word(memory, CONTROL + 1248), connected);
            assert_eq!(word(memory, disconnect + 72), status);
            assert!(h.script.is_empty());
        }
    }
}

#[test]
fn linux_datagram_consumes_once_and_rejects_short_sends_and_wrong_mode() {
    let output = compile(TargetName::X86_64Linux);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    setup(&mut h, &mut memory);
    h.native_hook = Some(|_, op, request, memory| {
        if op == 24 {
            let out = word(memory, request + 8);
            let capacity = word(memory, request + 16) as usize;
            memory[out as usize..out as usize + capacity].fill(0x41);
            let addr = word(memory, request + 32);
            memory[addr as usize..addr as usize + 8].copy_from_slice(&[2, 0, 0, 80, 127, 0, 0, 1]);
            let length = word(memory, request + 40);
            put(memory, length, 16);
        }
    });
    h.script.push_back((24, 5, 0));
    let result = h.invoke(
        "std/runtime/net_datagram.gg",
        "receive",
        &[CELL, BUFFER, 2, 0, 0],
        &mut memory,
    );
    assert_eq!(result[0] & 255, 0);
    assert_eq!(result[1], 2);
    assert!(result.last().unwrap() & 255 != 0, "截断标记保留");
    assert_eq!(&memory[BUFFER as usize..BUFFER as usize + 2], b"AA");
    assert_eq!(h.calls.iter().filter(|(op, _)| *op == 24).count(), 1);
    h.script.push_back((24, 0, 0));
    let result = h.invoke(
        "std/runtime/net_datagram.gg",
        "receive",
        &[CELL, BUFFER, 2, 0, 0],
        &mut memory,
    );
    assert_eq!(result[1], 0);
    assert_eq!(result.last().unwrap() & 255, 0);
    h.script.push_back((24, 5, 0));
    let too_large = h.invoke(
        "std/runtime/net_datagram.gg",
        "packet",
        &[CELL, 2, 0],
        &mut memory,
    );
    assert_eq!(too_large[0] & 255, 1);
    assert_eq!(
        too_large[1] & 255,
        24,
        "超上限报文消费一次后返回 MessageTooLarge"
    );
    put(&mut memory, CONTROL + 1248, 1);
    put(&mut memory, 80, BUFFER);
    put(&mut memory, 88, 4);
    let absent = 3072;
    put(&mut memory, absent, 1);
    for (sent, expected) in [(0, 6), (2, 4)] {
        h.script.push_back((25, sent, 0));
        let result = h.invoke(
            "std/runtime/net_datagram.gg",
            "send",
            &[CELL, 80, absent, 0],
            &mut memory,
        );
        assert_eq!(result[0] & 255, 1);
        assert_eq!(result[1] & 255, expected);
    }
    put(&mut memory, CONTROL + 1248, 0);
    let n = node(&mut h, &mut memory, 2, 2, 0);
    put(&mut memory, n + 320, 2);
    assert_eq!(submit(&mut h, &mut memory, n)[1] & 255, 12);
    put(&mut memory, CONTROL + 1248, 1);
    let n = node(&mut h, &mut memory, 2, 2, 0);
    put(&mut memory, n + 320, 1);
    assert_eq!(submit(&mut h, &mut memory, n)[1] & 255, 23);
    assert!(h.script.is_empty());
}

#[test]
fn windows_transport_holds_native_request_until_completion() {
    let output = compile(TargetName::X86_64Windows);
    let mut h = Harness::new(&output);
    let mut memory = vec![0; 262144];
    setup(&mut h, &mut memory);
    h.park_hook = Some(|h, memory| {
        let n = word(memory, CONTROL + 1216);
        h.script.extend([(8, 10, 0), (28, 0, 997)]);
        h.invoke(TRANSPORT, "service", &[], memory);
        assert_eq!(word(memory, n + 104), 1);
        assert_eq!(word(memory, CONTROL + 80), 1);
        put(memory, 96, 1);
        h.script.push_back((17, 1, 0));
        h.invoke(TRANSPORT, "service", &[], memory);
        assert!(h.woken.is_empty());
        assert!(h.close_requests.is_empty());
        h.invoke(TRANSPORT, "service", &[], memory);
        assert!(h.script.is_empty(), "不重复 CancelIoEx");
        h.invoke(TRANSPORT, "complete_windows", &[n, 0, 995], memory);
        assert_eq!(h.woken, [SLOT]);
        assert_eq!(word(memory, CONTROL + 80), 0);
    });
    let n = node(&mut h, &mut memory, 0, 8, 96);
    assert_eq!(submit(&mut h, &mut memory, n)[1] & 255, 9);
    assert_eq!(h.invoke(TRANSPORT, "pending", &[], &mut memory), [0]);
    put(&mut memory, 96, 0);
    h.woken.clear();
    h.park_hook = Some(|h, memory| {
        let n = word(memory, CONTROL + 1216);
        h.script.push_back((28, 0, 0));
        h.invoke(TRANSPORT, "service", &[], memory);
        assert!(h.woken.is_empty(), "同步成功也等待 IOCP");
        put(memory, CONTROL + 8, 2);
        h.script.push_back((17, 1, 0));
        h.invoke(TRANSPORT, "service", &[], memory);
        assert!(h.close_requests.is_empty());
        h.invoke(TRANSPORT, "complete_windows", &[n, 3, 0], memory);
        assert_eq!(word(memory, n + 80), 3, "已提交前缀优先于后观察 close");
        assert_eq!(h.close_requests, [vec![CONTROL, 42, 3]]);
    });
    let n = node(&mut h, &mut memory, 0, 8, 96);
    assert_eq!(submit(&mut h, &mut memory, n)[1], 3);
    put(&mut memory, CONTROL + 8, 0);
    for (bytes, status, expected, truncated) in [(0, 234, 4, 1), (0, 0, 0, 0), (3, 0, 3, 0)] {
        let n = node(&mut h, &mut memory, 2, 4, 0);
        put(&mut memory, 3000, bytes);
        put(&mut memory, 3008, status);
        h.park_hook = Some(|h, memory| {
            let n = word(memory, CONTROL + 1216);
            h.script.push_back((26, 0, 997));
            h.invoke(TRANSPORT, "service", &[], memory);
            h.invoke(
                TRANSPORT,
                "complete_windows",
                &[n, word(memory, 3000), word(memory, 3008)],
                memory,
            );
        });
        assert_eq!(submit(&mut h, &mut memory, n)[1], expected);
        assert_eq!(word(&memory, n + 88), truncated);
    }
    h.park_hook = Some(|h, memory| {
        let n = word(memory, CONTROL + 1216);
        h.script.push_back((28, 0, 997));
        h.invoke(TRANSPORT, "service", &[], memory);
        h.script.push_back((17, 1, 0));
        h.invoke(TRANSPORT, "close_read", &[CONTROL], memory);
        assert_eq!(word(memory, CONTROL + 80), 1);
        h.invoke(TRANSPORT, "complete_windows", &[n, 0, 995], memory);
    });
    let n = node(&mut h, &mut memory, 0, 8, 0);
    let eof = submit(&mut h, &mut memory, n);
    assert_eq!(eof[0] & 255, 0);
    assert_eq!(eof[1], 0);
    assert!(h.script.is_empty());
}
