use super::linux_vm::{REQUEST, Vm};
use super::*;

fn net_vm(operation: u64, arguments: &[u64]) -> Vm {
    let mut vm = Vm::new(0, [0; 6]);
    vm.registers[0] = operation;
    vm.registers[1] = REQUEST;
    for (i, value) in arguments.iter().enumerate() {
        vm.put(REQUEST + i as u64 * 8, *value, 8);
    }
    vm.put(REQUEST + 80, 999, 8);
    vm
}

fn net_code() -> Vec<Inst> {
    let fragment = linux_net_dispatch();
    assert!(fragment.relocations.is_empty());
    instructions(&fragment)
}

#[test]
fn net_operations_translate_arguments_and_result_protocol() {
    for (op, nr, count) in [
        (0, 198, 3),
        (1, 208, 5),
        (2, 200, 3),
        (3, 201, 2),
        (4, 242, 4),
        (5, 204, 3),
        (6, 205, 3),
        (7, 57, 1),
        (8, 20, 1),
        (21, 203, 3),
        (22, 210, 2),
        (23, 209, 5),
        (24, 207, 6),
        (25, 206, 6),
    ] {
        let arguments = [11, 22, 33, 44, 55, 66];
        let mut vm = net_vm(op, &arguments);
        let mut calls = 0;
        assert_eq!(
            vm.run_result(&net_code(), |vm| {
                calls += 1;
                assert_eq!(vm.registers[8], nr);
                assert_eq!(vm.registers[..count], arguments[..count]);
                vm.registers[0] = 123;
            }),
            123
        );
        assert_eq!(calls, 1);
        assert_eq!(vm.read(REQUEST + 72, 8), 123);
        assert_eq!(vm.read(REQUEST + 80, 8), 0);
    }
}

#[test]
fn net_errors_and_unsupported_requests_zero_value() {
    for operation in [0, 4, 8, 21, 24, 25] {
        let mut vm = net_vm(operation, &[0; 6]);
        assert_eq!(
            vm.run_result(&net_code(), |vm| vm.registers[0] = (-11_i64) as u64),
            0
        );
        assert_eq!(vm.read(REQUEST + 72, 8), 0);
        assert_eq!(vm.read(REQUEST + 80, 8), 11);
    }
    for operation in [11, 12, 13, 26, 29, u64::MAX] {
        let mut vm = net_vm(operation, &[]);
        vm.run_result(&net_code(), |_| panic!("unsupported syscall"));
        assert_eq!(vm.read(REQUEST + 72, 8), 0);
        assert_eq!(vm.read(REQUEST + 80, 8), 22);
    }
}

#[test]
fn epoll_ctl_expands_packed_event_and_delete_ignores_pointer() {
    let mut vm = net_vm(9, &[3, 1, 4, 0x3000]);
    vm.put(0x3000, 0x80000001, 4);
    vm.put(0x3004, 0x123456789abcdef0, 8);
    vm.run_result(&net_code(), |vm| {
        assert_eq!(vm.registers[8], 21);
        let at = vm.registers[3];
        assert_eq!(vm.read(at, 8), 0x80000001);
        assert_eq!(vm.read(at + 8, 8), 0x123456789abcdef0);
        vm.registers[0] = 0;
    });
    let mut vm = net_vm(9, &[3, 2, 4, 0]);
    vm.run_result(&net_code(), |vm| {
        assert_eq!(vm.registers[3], 0);
        vm.registers[0] = 0;
    });
}

#[test]
fn epoll_wait_packs_multiple_events_and_limits_native_batch() {
    for maximum in [2, 128, 129, 10000] {
        let mut vm = net_vm(10, &[3, 0x3000, maximum, 55]);
        assert_eq!(
            vm.run_result(&net_code(), |vm| {
                assert_eq!(vm.registers[8], 22);
                assert_eq!(vm.registers[2], maximum.min(128));
                assert_eq!(&vm.registers[3..6], &[55, 0, 8]);
                let at = vm.registers[1];
                vm.put(at, 1, 4);
                vm.put(at + 8, 0xfedcba9876543210, 8);
                vm.put(at + 16, 4, 4);
                vm.put(at + 24, 0x123456789abcdef0, 8);
                vm.registers[0] = 2;
            }),
            2
        );
        assert_eq!(vm.read(0x3000, 4), 1);
        assert_eq!(vm.read(0x3004, 8), 0xfedcba9876543210);
        assert_eq!(vm.read(0x300c, 4), 4);
        assert_eq!(vm.read(0x3010, 8), 0x123456789abcdef0);
        assert_eq!(vm.read(0x3018, 8), 0);
    }
}

#[test]
fn epoll_wait_empty_error_and_invalid_count_preserve_destination() {
    for result in [0, (-4_i64) as u64] {
        let mut vm = net_vm(10, &[3, 0x3000, 1, 0]);
        vm.put(0x3000, 123, 8);
        vm.run_result(&net_code(), |vm| vm.registers[0] = result);
        assert_eq!(vm.read(0x3000, 8), 123);
        assert_eq!(vm.read(REQUEST + 80, 8), if result == 0 { 0 } else { 4 });
    }
    for count in [0, u64::MAX, 0x80000000] {
        let mut vm = net_vm(10, &[3, 0x3000, count, 0]);
        vm.run_result(&net_code(), |_| panic!("invalid count reached kernel"));
        assert_eq!(vm.read(REQUEST + 80, 8), 22);
    }
}

#[test]
fn net_close_returns_a_local_c_abi_callback() {
    let mut vm = net_vm(19, &[]);
    let code = net_code();
    let address = vm.run_result(&code, |_| panic!("callback lookup made syscall"));
    assert_eq!(form(&code[address as usize / 4]), "movz.X");
    assert_eq!(
        code[address as usize / 4].operands,
        [reg(Reg::x(8)), movz(57)]
    );
    assert_eq!(form(&code[address as usize / 4 + 1]), "svc");
}

#[test]
fn signal_requests_work_standalone_and_through_file_worker() {
    for fragment in [linux_signal_dispatch(), linux_fd_dispatch(false)] {
        let code = instructions(&fragment);
        for (op, nr, count) in [
            (14, 135, 4),
            (39, 172, 0),
            (62, 129, 2),
            (186, 178, 0),
            (200, 130, 2),
            (234, 131, 3),
            (289, 74, 4),
        ] {
            for result in [7, (-22_i64) as u64] {
                let arguments = [11, 22, 8, 524288, 0, 0];
                let mut vm = Vm::new(op, arguments);
                vm.run(&code, |vm| {
                    assert_eq!(vm.registers[8], nr);
                    assert_eq!(vm.registers[..count], arguments[..count]);
                    vm.registers[0] = result;
                });
                assert_eq!(vm.read(REQUEST + 64, 8), result);
                assert_eq!(vm.read(REQUEST + 72, 8), if result == 7 { 0 } else { 22 });
            }
        }
    }
}

#[test]
fn sigaction_clears_x86_restorer_and_copies_old_action_only_on_success() {
    for result in [0, (-22_i64) as u64] {
        let mut vm = Vm::new(13, [10, 0x3000, 0x4000, 8, 0, 0]);
        for (i, value) in [0x1234, 0x04000004, 0x5678, 0x123456]
            .into_iter()
            .enumerate()
        {
            vm.put(0x3000 + i as u64 * 8, value, 8);
        }
        vm.put(0x4000, 999, 8);
        vm.run(&instructions(&linux_signal_dispatch()), |vm| {
            assert_eq!(vm.registers[8], 134);
            let at = vm.registers[1];
            assert_eq!(
                [
                    vm.read(at, 8),
                    vm.read(at + 8, 8),
                    vm.read(at + 16, 8),
                    vm.read(at + 24, 8)
                ],
                [0x1234, 4, 0, 0x123456]
            );
            for (i, value) in [1, 2, 3, 4].into_iter().enumerate() {
                vm.put(vm.registers[2] + i as u64 * 8, value, 8);
            }
            vm.registers[0] = result;
        });
        assert_eq!(vm.read(0x4000, 8), if result == 0 { 1 } else { 999 });
        assert_eq!(vm.read(0x3008, 8), 0x04000004);
    }
}

#[test]
fn signal_query_null_pointers_and_unsupported_operations() {
    let code = instructions(&linux_signal_dispatch());
    let mut vm = Vm::new(13, [10, 0, 0, 8, 0, 0]);
    vm.run(&code, |vm| {
        assert_eq!(&vm.registers[1..4], &[0, 0, 8]);
        vm.registers[0] = 0;
    });
    let mut vm = Vm::new(290, [0; 6]);
    vm.run(&code, |_| panic!("unsupported operation reached kernel"));
    assert_eq!(vm.read(REQUEST + 64, 8), 0);
    assert_eq!(vm.read(REQUEST + 72, 8), 22);
}
