use super::*;

#[test]
fn child_exit_is_shared_and_cancellation_removes_only_its_waiter() {
    let output = protocol(TargetName::X86_64Linux);
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    let control = child(&mut h, &mut m);
    h.native_hook = Some(exit_code);
    h.script.extend([(0, 0, 0), (0, 1, 0)]);
    assert_eq!(h.invoke(STATE, "try_wait", &[512], &mut m)[..2], [0, 1]);
    store(&mut m, 1024, &[1088, 101, 1536]);
    store(&mut m, 1088, &[0, 102, 0]);
    put(&mut m, 1536, 1);
    put(&mut m, control + 256, 1024);
    put(&mut m, control + 264, 1088);
    h.invoke(STATE, "waiters", &[control], &mut m);
    assert_eq!(h.woken, [101]);
    assert_eq!(word(&m, 1024 + 48), u64::MAX);
    assert_eq!(word(&m, control + 256), 1088);
    h.invoke(STATE, "poll", &[0], &mut m);
    assert_eq!(h.woken, [101, 102]);
    assert_eq!((word(&m, 1088 + 32), word(&m, 1088 + 40)), (1, 7));
    assert_eq!(word(&m, control + 256), 0);
    let cached = h.invoke(STATE, "try_wait", &[512], &mut m);
    assert_eq!(&cached[..4], &[0, 0, 1, 7]);
    assert_eq!(h.invoke(STATE, "try_wait", &[512], &mut m), cached);
    assert_eq!(h.calls.len(), 2, "缓存状态不能再次 waitpid");
    put(&mut m, control + 256, 1024);
    put(&mut m, control + 264, 1024);
    put(&mut m, 1024, 0);
    h.invoke(STATE, "waiters", &[control], &mut m);
    assert_eq!(word(&m, 1024 + 48), 0, "已提交退出优先于同轮取消");
    assert_eq!(word(&m, 1024 + 40), 7);
}

#[test]
fn explicit_close_kills_once_and_detach_never_kills() {
    let output = protocol(TargetName::X86_64Linux);
    for detach in [false, true] {
        let mut h = output.harness();
        let mut m = vec![0; 1048576];
        let control = child(&mut h, &mut m);
        h.native_hook = Some(exit_code);
        h.park_hook = Some(|h, m| {
            h.invoke(STATE, "poll", &[0], m);
            if word(m, word(m, 512) + 8) & 8 == 0 {
                h.invoke(STATE, "poll", &[0], m);
            }
        });
        if detach {
            h.script.push_back((0, 0, 0));
        } else {
            h.script.extend([(0, 0, 0), (1, 0, 0), (0, 1, 0)]);
        }
        let operation = if detach { "detach" } else { "close" };
        assert_eq!(h.invoke(STATE, operation, &[512], &mut m)[0], 0);
        assert_eq!(h.invoke(STATE, operation, &[512], &mut m)[0], 0);
        assert_eq!(
            h.calls.iter().filter(|(op, _)| *op == 1).count(),
            usize::from(!detach)
        );
        assert_eq!(h.invoke(STATE, "try_wait", &[512], &mut m)[..2], [1, 10]);
        assert_eq!(h.invoke(STATE, "kill", &[512], &mut m)[..2], [1, 10]);
        assert_eq!(h.invoke(STATE, "pending", &[0], &mut m), [0]);
        if detach {
            assert_eq!(h.invoke(STATE, "close", &[512], &mut m)[..2], [1, 10]);
            h.script.push_back((0, 1, 0));
            h.invoke(STATE, "poll", &[0], &mut m);
        }
        h.invoke(
            "std/runtime/resource.gg",
            "release_control",
            &[control],
            &mut m,
        );
        h.collect_unreachable_resources(&mut m);
        assert!(
            h.allocations.is_empty(),
            "observer 与控制页均已归还：{:?}，租约 {}",
            h.allocations,
            word(&m, control)
        );
        assert!(h.script.is_empty());
    }
}

#[test]
fn child_wait_precancel_has_no_effect_and_supervisor_does_not_keep_runtime_alive() {
    let output = protocol(TargetName::X86_64Linux);
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    let control = child(&mut h, &mut m);
    put(&mut m, 1536, 1);
    store(&mut m, 1552, &[1536]);
    assert_eq!(
        h.invoke(STATE, "wait_cancel", &[512, 1552], &mut m)[..2],
        [1, 9]
    );
    assert_eq!(word(&m, control + 80), 0);
    assert_eq!(word(&m, control + 256), 0);
    assert!(h.calls.is_empty());
    h.invoke(STATE, "request_close", &[control], &mut m);
    put(&mut m, control + 8, 6);
    h.script.push_back((0, 0, 0));
    h.invoke(STATE, "poll", &[0], &mut m);
    assert_eq!(h.invoke(STATE, "pending", &[0], &mut m), [0]);
    h.invoke(STATE, "shutdown", &[0], &mut m);
    assert_eq!(h.calls.iter().filter(|(op, _)| *op == 1).count(), 0);
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

#[test]
fn nine_children_progress_without_blocking_worker_credits() {
    let output = protocol(TargetName::X86_64Linux);
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    let mut nodes = Vec::new();
    for index in 0..9 {
        let control = child(&mut h, &mut m);
        let node = h.allocate(&mut m, 64);
        store(&mut m, node, &[0, 100 + index, 0]);
        put(&mut m, control + 256, node);
        put(&mut m, control + 264, node);
        nodes.push(node);
        h.script.push_back((0, 0, 0));
    }
    h.invoke(STATE, "poll", &[0], &mut m);
    assert!(h.woken.is_empty());
    h.native_hook = Some(exit_code);
    h.script.extend((0..9).map(|_| (0, 1, 0)));
    h.invoke(STATE, "poll", &[0], &mut m);
    h.woken.sort_unstable();
    assert_eq!(h.woken, (100..109).collect::<Vec<_>>());
    for node in nodes {
        assert_eq!((word(&m, node + 32), word(&m, node + 40)), (1, 7));
    }
    assert!(h.script.is_empty());
    assert_eq!(h.invoke(STATE, "pending", &[0], &mut m), [0]);
}

#[test]
fn wait_closes_stdin_but_try_wait_does_not() {
    let output = protocol(TargetName::X86_64Linux);
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    store(&mut m, 512, &[71, 71, 72, u64::MAX, u64::MAX]);
    let child = h.invoke(STATE, "adopt", &[512], &mut m);
    store(&mut m, 512, &child);
    let input = word(&m, child[0] + 232);
    h.script.extend([(0, 0, 0), (2, 0, 0), (0, 1, 0)]);
    h.native_hook = Some(exit_code);
    h.park_hook = Some(|h, m| {
        h.invoke(PIPE, "poll", &[0], m);
        h.invoke(STATE, "poll", &[0], m);
    });
    assert_eq!(&h.invoke(STATE, "try_wait", &[512], &mut m)[..2], &[0, 1]);
    assert_eq!(word(&m, input + 8) & 6, 0);
    assert_eq!(&h.invoke(STATE, "wait", &[512], &mut m)[..3], &[0, 1, 7]);
    assert_ne!(word(&m, input + 8) & 8, 0);
    assert_eq!(h.calls.iter().filter(|(op, _)| *op == 2).count(), 1);
    assert!(h.script.is_empty());
}
