use super::*;

fn setup(h: &mut Harness<'_>, m: &mut Vec<u8>) -> (u64, u64) {
    let control = h.invoke(PIPE, "adopt", &[71, 0], m)[0];
    let node = h.allocate(m, 256);
    store(m, node, &[3, 71, node + 192, 16, node + 96]);
    put(m, node + 128, control);
    put(m, node + 136, 101);
    put(m, node + 144, 1536);
    put(m, control + 208, node);
    put(m, control + 216, node);
    put(m, control + 80, 1);
    (control, node)
}

#[test]
fn linux_pipe_consumes_short_completion_before_cancellation() {
    let output = protocol(TargetName::X86_64Linux);
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    let (control, node) = setup(&mut h, &mut m);
    h.script.extend([(3, 0, 11), (3, 3, 0)]);
    h.invoke(PIPE, "poll", &[0], &mut m);
    assert!(h.woken.is_empty());
    put(&mut m, 1536, 1);
    h.invoke(PIPE, "poll", &[0], &mut m);
    assert_eq!(word(&m, node + 72), 3);
    assert_eq!(word(&m, node + 80), 0);
    assert_eq!(word(&m, control + 208), 0);
    h.invoke(PIPE, "poll", &[0], &mut m);
    assert_eq!(h.woken, [101]);
    assert!(h.script.is_empty());
}

#[test]
fn windows_cancel_keeps_overlapped_request_until_terminal_completion() {
    let output = protocol(TargetName::X86_64Windows);
    for closing in [false, true] {
        let mut h = output.harness();
        let mut m = vec![0; 1048576];
        let (control, node) = setup(&mut h, &mut m);
        h.script.extend([
            (3, 0, 997),
            (5, 0, 996),
            (6, 0, 1168),
            (5, 0, 996),
            (5, 4, 0),
        ]);
        h.invoke(PIPE, "poll", &[0], &mut m);
        if closing {
            put(&mut m, control + 8, 6);
            h.invoke(PIPE, "request_close", &[control], &mut m);
        } else {
            put(&mut m, 1536, 1);
        }
        for _ in 0..2 {
            h.invoke(PIPE, "poll", &[0], &mut m);
            assert!(h.woken.is_empty());
            assert_eq!(word(&m, control + 208), node);
            assert_eq!(word(&m, control + 80), 1);
            assert!(h.freed.is_empty());
        }
        h.invoke(PIPE, "poll", &[0], &mut m);
        assert_eq!(h.woken, [101]);
        assert_eq!(word(&m, node + 72), 4);
        assert_eq!(h.calls.iter().filter(|(op, _)| *op == 6).count(), 1);
        if closing {
            put(&mut m, control + 80, 0);
            h.script.push_back((2, 0, 0));
            h.invoke(PIPE, "poll", &[0], &mut m);
            h.invoke(PIPE, "poll", &[0], &mut m);
            assert_eq!(h.calls.iter().filter(|(op, _)| *op == 2).count(), 1);
            h.invoke(
                "std/runtime/resource.gg",
                "release_control",
                &[control],
                &mut m,
            );
            h.collect_unreachable_resources(&mut m);
            assert!(
                h.allocations.is_empty(),
                "残留 {:?}，租约 {}",
                h.allocations,
                word(&m, control)
            );
        }
        assert!(h.script.is_empty());
    }
}

#[test]
fn pipe_round_robin_reaches_cancelled_tail_beyond_service_budget() {
    let output = protocol(TargetName::X86_64Linux);
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    let (_, tail) = setup(&mut h, &mut m);
    for _ in 1..129 {
        setup(&mut h, &mut m);
    }
    put(&mut m, 1536, 1);
    for _ in 0..3 {
        h.invoke(PIPE, "poll", &[0], &mut m);
    }
    assert_eq!(word(&m, tail + 80), u64::MAX);
    assert_eq!(word(&m, tail + 160), 1);
    assert_eq!(h.woken.len(), 129);
    assert!(h.calls.is_empty(), "未提交取消不占 blocking credit");
}

#[test]
fn pipe_eof_short_write_and_broken_pipe_have_distinct_terminal_results() {
    let output = protocol(TargetName::X86_64Linux);
    for (operation, count, error) in [(3, 0, 0), (4, 3, 0), (4, 0, 32)] {
        let mut h = output.harness();
        let mut m = vec![0; 1048576];
        let (control, node) = setup(&mut h, &mut m);
        put(&mut m, node, operation);
        h.script.push_back((operation, count, error));
        h.invoke(PIPE, "poll", &[0], &mut m);
        assert_eq!((word(&m, node + 72), word(&m, node + 80)), (count, error));
        assert_eq!(word(&m, control + 208), 0);
        h.invoke(PIPE, "poll", &[0], &mut m);
        assert_eq!(h.woken, [101]);
        assert!(h.script.is_empty());
    }
}
