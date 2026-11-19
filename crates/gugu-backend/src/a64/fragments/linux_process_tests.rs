use super::linux_vm::{REQUEST, Vm};
use super::*;
use std::collections::VecDeque;

fn request(op: u64, arguments: &[u64]) -> Vm {
    let mut vm = Vm::new(0, [0; 6]);
    vm.put(REQUEST, op, 8);
    vm.put(REQUEST + 80, 999, 8);
    for (i, a) in arguments.iter().enumerate() {
        vm.put(REQUEST + 8 + i as u64 * 8, *a, 8);
    }
    vm
}

fn call_code() -> Vec<Inst> {
    instructions(&linux_process_call())
}
fn worker_code() -> Vec<Inst> {
    let f = linux_process_worker();
    assert!(f.relocations.is_empty());
    instructions(&f)[2..].to_vec()
}

fn call(vm: &mut Vm, op: u64, syscall: impl FnMut(&mut Vm)) {
    vm.registers[0] = op;
    vm.registers[1] = REQUEST;
    vm.run(&call_code(), syscall);
}

#[test]
fn process_calls_translate_linux_arguments_and_clear_stale_results() {
    for (op, nr, args) in [
        (1, 129, vec![91, 9]),
        (2, 57, vec![13]),
        (3, 63, vec![13, 0x3000, 11]),
    ] {
        for result in [0, 7, (-11_i64) as u64] {
            let mut vm = request(op, &[91, 0x3000, 11]);
            if op != 1 {
                vm.put(REQUEST + 8, 13, 8);
            }
            call(&mut vm, op, |vm| {
                assert_eq!(vm.registers[8], nr);
                assert_eq!(vm.registers[..args.len()], args);
                vm.registers[0] = result;
            });
            assert_eq!(
                vm.read(REQUEST + 72, 8),
                if result >> 63 == 0 { result } else { 0 }
            );
            assert_eq!(
                vm.read(REQUEST + 80, 8),
                if result >> 63 == 0 { 0 } else { 11 }
            );
        }
    }
}

#[test]
fn wait_decodes_pending_exit_signal_and_error() {
    for (ret, status, kind, value, error) in [
        (0, 0, 0, 0, 0),
        (91, 7 << 8, 1, 7, 0),
        (91, 137, 2, 9, 0),
        ((-10_i64) as u64, 0, 0, 0, 10),
    ] {
        let mut vm = request(0, &[91, 999]);
        call(&mut vm, 0, |vm| {
            assert_eq!(vm.registers[8], 260);
            assert_eq!(&vm.registers[..4], &[91, REQUEST + 16, 1, 0]);
            vm.put(REQUEST + 16, status, 4);
            vm.registers[0] = ret;
        });
        assert_eq!(vm.read(REQUEST + 72, 8), kind);
        assert_eq!(vm.read(REQUEST + 16, 8), value);
        assert_eq!(vm.read(REQUEST + 80, 8), error);
    }
}

#[test]
fn completion_is_unsupported_and_invalid_protocol_traps() {
    for op in [5, 6] {
        let mut vm = request(op, &[]);
        call(&mut vm, op, |_| panic!("unexpected syscall"));
        assert_eq!(vm.read(REQUEST + 80, 8), 95);
    }
    let mut vm = request(2, &[]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        call(&mut vm, 3, |_| panic!("unexpected syscall"))
    }));
    assert!(result.is_err());
}

#[test]
fn worker_returns_local_entry_and_creates_directional_pipes() {
    let mut vm = request(0, &[]);
    assert_eq!(
        vm.run_result(&instructions(&linux_process_worker()), |_| panic!()),
        8
    );
    for direction in [0, 1, 2] {
        let mut vm = request(1, &[direction]);
        let mut calls = 0;
        vm.run(&worker_code(), |vm| {
            calls += 1;
            if calls == 1 {
                assert_eq!(vm.registers[8], 59);
                assert_eq!(vm.registers[1], 524288);
                let p = vm.registers[0];
                vm.put(p, 10, 4);
                vm.put(p + 4, 11, 4);
            } else {
                assert_eq!(vm.registers[8], 25);
                assert_eq!(
                    &vm.registers[..3],
                    &[if direction == 0 { 11 } else { 10 }, 4, 2048]
                );
            }
            vm.registers[0] = 0;
        });
        assert_eq!(calls, 2);
        assert_eq!(
            vm.read(REQUEST + 72, 8),
            if direction == 0 { 11 } else { 10 }
        );
        assert_eq!(
            vm.read(REQUEST + 24, 8),
            if direction == 0 { 10 } else { 11 }
        );
    }
}

#[test]
fn pipe_flag_failure_closes_both_fds_and_preserves_errno() {
    let mut vm = request(1, &[1]);
    let mut script = VecDeque::from([(59, 0), (25, (-5_i64) as u64), (57, 0), (57, 0)]);
    let mut closed = vec![];
    vm.run(&worker_code(), |vm| {
        let (nr, result) = script.pop_front().unwrap();
        assert_eq!(vm.registers[8], nr);
        if nr == 59 {
            let p = vm.registers[0];
            vm.put(p, 10, 4);
            vm.put(p + 4, 11, 4);
        }
        if nr == 57 {
            closed.push(vm.registers[0]);
        }
        vm.registers[0] = result;
    });
    assert!(script.is_empty());
    assert_eq!(closed, [10, 11]);
    assert_eq!(vm.read(REQUEST + 80, 8), 5);
}

fn spawn_vm() -> Vm {
    let mut vm = request(0, &[0x3000]);
    for (i, value) in [0x4000, 0x5000, 0x6000, 0x7000, 3, 4, 5, 0, 0, 0]
        .into_iter()
        .enumerate()
    {
        vm.put(0x3000 + i as u64 * 8, value, 8);
    }
    vm
}

#[test]
fn spawn_parent_observes_cloexec_eof_and_retries_interrupted_read() {
    let mut vm = spawn_vm();
    let mut script = VecDeque::from([
        (59, 0),
        (220, 91),
        (57, 0),
        (63, (-4_i64) as u64),
        (63, 0),
        (57, 0),
    ]);
    vm.run(&worker_code(), |vm| {
        let (nr, result) = script.pop_front().unwrap();
        assert_eq!(vm.registers[8], nr);
        if nr == 59 {
            let p = vm.registers[0];
            vm.put(p, 10, 4);
            vm.put(p + 4, 11, 4);
        }
        if nr == 220 {
            assert_eq!(&vm.registers[..5], &[17, 0, 0, 0, 0]);
        }
        vm.registers[0] = result;
    });
    assert!(script.is_empty());
    assert_eq!(vm.read(REQUEST + 72, 8), 91);
    assert_eq!(vm.read(0x3038, 8), 91);
    assert_eq!(vm.read(0x3040, 8), 91);
}

#[test]
fn spawn_failures_report_errno_and_reap_or_close_as_needed() {
    for (script, error) in [
        (vec![(59, -24)], 24),
        (vec![(59, 0), (220, -11), (57, 0), (57, 0)], 11),
        (
            vec![(59, 0), (220, 91), (57, 0), (63, 4), (57, 0), (260, 91)],
            2,
        ),
        (
            vec![
                (59, 0),
                (220, 91),
                (57, 0),
                (63, -5),
                (57, 0),
                (129, 0),
                (260, -4),
                (260, 91),
            ],
            5,
        ),
    ] {
        let mut vm = spawn_vm();
        let mut script = VecDeque::from(script);
        vm.run(&worker_code(), |vm| {
            let (nr, result) = script.pop_front().unwrap();
            assert_eq!(vm.registers[8], nr);
            if nr == 59 && result == 0 {
                let p = vm.registers[0];
                vm.put(p, 10, 4);
                vm.put(p + 4, 11, 4);
            }
            if nr == 63 && result == 4 {
                vm.put(vm.registers[1], 2, 4);
            }
            vm.registers[0] = result as u64;
        });
        assert!(script.is_empty());
        assert_eq!(vm.read(REQUEST + 80, 8), error);
        assert_eq!(vm.read(REQUEST + 72, 8), 0);
    }
}

#[test]
fn spawn_child_only_uses_raw_syscalls_and_reports_exec_errno() {
    let mut vm = spawn_vm();
    let mut script = VecDeque::from([59, 220, 57, 49, 24, 24, 24, 57, 57, 57, 134, 221, 64, 93]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.run(&worker_code(), |vm| {
            let nr = script.pop_front().unwrap();
            assert_eq!(vm.registers[8], nr);
            if nr == 59 {
                let p = vm.registers[0];
                vm.put(p, 10, 4);
                vm.put(p + 4, 11, 4);
            }
            if nr == 221 {
                assert_eq!(&vm.registers[..3], &[0x4000, 0x5000, 0x6000]);
            }
            if nr == 64 {
                assert_eq!(vm.registers[0], 11);
                assert_eq!(vm.read(vm.registers[1], 4), 2);
            }
            if nr == 93 {
                assert_eq!(vm.registers[0], 127);
                panic!("child exited 127");
            }
            vm.registers[0] = if nr == 221 { (-2_i64) as u64 } else { 0 };
        })
    }));
    assert_eq!(
        result.unwrap_err().downcast_ref::<&str>(),
        Some(&"child exited 127")
    );
    assert!(script.is_empty());
}

#[test]
fn pipe_write_masks_sigpipe_consumes_only_new_pending_and_restores() {
    for pending in [0, 4096] {
        let mut vm = request(4, &[10, 0x3000, 4]);
        let mut script = VecDeque::from(if pending == 0 {
            vec![135, 136, 64, 137, 135]
        } else {
            vec![135, 136, 64, 135]
        });
        call(&mut vm, 4, |vm| {
            let nr = script.pop_front().unwrap();
            assert_eq!(vm.registers[8], nr);
            if nr == 136 {
                vm.put(vm.registers[0], pending, 8);
            }
            if nr == 135 {
                assert_eq!(vm.registers[3], 8);
                assert!([0, 2].contains(&vm.registers[0]));
            }
            vm.registers[0] = if nr == 64 { (-32_i64) as u64 } else { 0 };
        });
        assert!(script.is_empty());
        assert_eq!(vm.read(REQUEST + 80, 8), 32);
    }
}

#[test]
fn open_null_and_duplicate_stdio_use_cloexec_and_fd_floor_three() {
    for op in [2, 3] {
        let mut vm = request(op, &[1]);
        vm.run(&worker_code(), |vm| {
            if op == 2 {
                assert_eq!(vm.registers[8], 56);
                assert_eq!(vm.registers[0], (-100_i64) as u64);
                assert_eq!(vm.registers[2], 524290);
                assert_eq!(
                    vm.read(vm.registers[1], 8),
                    u64::from_le_bytes(*b"/dev/nul")
                );
            } else {
                assert_eq!(vm.registers[8], 25);
                assert_eq!(&vm.registers[..3], &[1, 1030, 3]);
            }
            vm.registers[0] = 17;
        });
        assert_eq!(vm.read(REQUEST + 72, 8), 17);
    }
}

#[test]
fn pipe_write_returns_counts_empty_eagain_and_mask_errors() {
    for (write_result, error) in [(0, 0), (4, 0), (-11, 11)] {
        let mut vm = request(4, &[10, 0x3000, 4]);
        let mut script = VecDeque::from([135, 136, 64, 135]);
        call(&mut vm, 4, |vm| {
            let nr = script.pop_front().unwrap();
            assert_eq!(vm.registers[8], nr);
            if nr == 135 && vm.registers[0] == 0 {
                vm.put(vm.registers[2], 0x400, 8);
            }
            if nr == 135 && vm.registers[0] == 2 {
                assert_eq!(vm.read(vm.registers[1], 8), 0x400);
            }
            vm.registers[0] = if nr == 64 { write_result as u64 } else { 0 };
        });
        assert!(script.is_empty());
        assert_eq!(vm.read(REQUEST + 80, 8), error);
        assert_eq!(
            vm.read(REQUEST + 72, 8),
            if error == 0 { write_result as u64 } else { 0 }
        );
    }
    let mut vm = request(4, &[10, 0x3000, 0]);
    call(&mut vm, 4, |vm| {
        assert_eq!(vm.registers[8], 135);
        vm.registers[0] = (-13_i64) as u64;
    });
    assert_eq!(vm.read(REQUEST + 80, 8), 13);
}
