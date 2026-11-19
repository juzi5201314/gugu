use super::*;

#[path = "linux_fd_vm_tests.rs"]
mod vm;
use vm::{REQUEST, Vm};

fn code() -> Vec<Inst> {
    let fragment = linux_fd_dispatch();
    assert!(fragment.relocations.is_empty());
    instructions(&fragment)
}

#[test]
fn file_syscalls_use_linux_request_ids_and_asm_generic_arguments() {
    let arguments = [11, 22, 33, 44, 55, 66];
    let cwd = (-100_i64) as u64;
    for (op, syscall, expected) in [
        (0, 63, vec![11, 22, 33]),
        (1, 64, vec![11, 22, 33]),
        (2, 56, vec![cwd, 11, 22, 33]),
        (3, 57, vec![11]),
        (8, 62, vec![11, 22, 33]),
        (72, 25, vec![11, 22, 33]),
        (74, 82, vec![11]),
        (75, 83, vec![11]),
        (77, 46, vec![11, 22]),
        (82, 38, vec![cwd, 11, cwd, 22]),
        (83, 34, vec![cwd, 11, 22]),
        (84, 35, vec![cwd, 11, 512]),
        (86, 37, vec![cwd, 11, cwd, 22, 0]),
        (87, 35, vec![cwd, 11, 0]),
        (88, 36, vec![11, cwd, 22]),
        (89, 78, vec![cwd, 11, 22, 33]),
        (217, 61, vec![11, 22, 33]),
    ] {
        let mut vm = Vm::new(op, arguments);
        let mut calls = 0;
        vm.run(&code(), |vm| {
            calls += 1;
            assert_eq!(vm.registers[8], syscall, "op {op}");
            assert_eq!(&vm.registers[..expected.len()], expected, "op {op}");
            vm.registers[0] = 123;
        });
        assert_eq!(calls, 1);
        assert_eq!(vm.read(REQUEST + 64, 8), 123);
        assert_eq!(vm.read(REQUEST + 72, 8), 0);
    }
}

#[test]
fn unsupported_operations_do_not_make_a_syscall() {
    for operation in [7, 14, 73, 218, 289, 4096, u64::MAX] {
        let mut vm = Vm::new(operation, [0; 6]);
        vm.run(&code(), |_| panic!("unsupported operation reached kernel"));
        assert_eq!(vm.read(REQUEST + 64, 8), 0);
        assert_eq!(vm.read(REQUEST + 72, 8), 22);
    }
}

#[test]
fn negative_returns_preserve_value_and_publish_positive_errno() {
    for operation in [
        0, 1, 2, 3, 4, 5, 6, 8, 72, 74, 75, 77, 82, 83, 84, 86, 87, 88, 89, 217, 262,
    ] {
        for errno in [2, 9, 13, 22] {
            let mut vm = Vm::new(operation, [0; 6]);
            vm.run(&code(), |vm| vm.registers[0] = (-errno as i64) as u64);
            assert_eq!(vm.read(REQUEST + 64, 8), (-errno as i64) as u64);
            assert_eq!(vm.read(REQUEST + 72, 8), errno as u64);
        }
    }
}

#[test]
fn zero_byte_read_and_directory_eof_are_success() {
    for operation in [0, 217] {
        let mut vm = Vm::new(operation, [0; 6]);
        vm.run(&code(), |vm| vm.registers[0] = 0);
        assert_eq!(vm.read(REQUEST + 64, 8), 0);
        assert_eq!(vm.read(REQUEST + 72, 8), 0);
    }
}

#[test]
fn open_and_fcntl_translate_each_architecture_specific_flag() {
    for (x86, arm) in [
        (1 << 14, 1 << 16),
        (1 << 15, 1 << 17),
        (1 << 16, 1 << 14),
        (1 << 17, 1 << 15),
    ] {
        let common = 0x280802;
        let mut open = Vm::new(2, [0x3000, x86 | common, 0o600, 0, 0, 0]);
        open.run(&code(), |vm| {
            assert_eq!(vm.registers[2], arm | common);
            vm.registers[0] = 7;
        });
        let mut set = Vm::new(72, [7, 4, x86 | common, 0, 0, 0]);
        set.run(&code(), |vm| {
            assert_eq!(vm.registers[2], arm | common);
            vm.registers[0] = 0;
        });
        let mut get = Vm::new(72, [7, 3, 0, 0, 0, 0]);
        get.run(&code(), |vm| vm.registers[0] = arm | common);
        assert_eq!(get.read(REQUEST + 64, 8), x86 | common);
    }
}

fn native_stat(vm: &mut Vm, at: u64) {
    for (offset, value, size) in [
        (0, 101, 8),
        (8, 102, 8),
        (16, 0o100640, 4),
        (20, 0xffff_ffff, 4),
        (24, 103, 4),
        (28, 104, 4),
        (32, 105, 8),
        (48, 106, 8),
        (56, 0xffff_ffff, 4),
        (64, 107, 8),
        (72, 108, 8),
        (80, 109, 8),
        (88, 110, 8),
        (96, 111, 8),
        (104, 112, 8),
        (112, 113, 8),
    ] {
        vm.put(at + offset, value, size);
    }
}

#[test]
fn stat_variants_normalize_the_full_x86_kernel_layout() {
    for (operation, expected, pointer_index) in [
        (4, vec![(-100_i64) as u64, 0x3000, 0, 0], 2),
        (5, vec![0x3000, 0], 1),
        (6, vec![(-100_i64) as u64, 0x3000, 0, 256], 2),
        (262, vec![0x3000, 0x4000, 0, 256], 2),
    ] {
        let arguments = if operation == 262 {
            [0x3000, 0x4000, 0x5000, 256, 0, 0]
        } else {
            [0x3000, 0x5000, 0, 0, 0, 0]
        };
        let mut vm = Vm::new(operation, arguments);
        for offset in (0..144).step_by(8) {
            vm.put(0x5000 + offset, u64::MAX, 8);
        }
        vm.run(&code(), |vm| {
            assert_eq!(vm.registers[8], if operation == 5 { 80 } else { 79 });
            for (i, value) in expected.iter().enumerate() {
                if i != pointer_index {
                    assert_eq!(vm.registers[i], *value);
                }
            }
            native_stat(vm, vm.registers[pointer_index]);
            vm.registers[0] = 0;
        });
        assert_stat(&vm);
    }
}

fn assert_stat(vm: &Vm) {
    for (offset, value) in [
        (0, 101),
        (8, 102),
        (16, 0xffff_ffff),
        (40, 105),
        (48, 106),
        (56, u64::MAX),
        (64, 107),
        (72, 108),
        (80, 109),
        (88, 110),
        (96, 111),
        (104, 112),
        (112, 113),
        (120, 0),
        (128, 0),
        (136, 0),
    ] {
        assert_eq!(vm.read(0x5000 + offset, 8), value, "offset {offset}");
    }
    assert_eq!(vm.read(0x5000 + 24, 4), 0o100640);
    assert_eq!(vm.read(0x5000 + 28, 4), 103);
    assert_eq!(vm.read(0x5000 + 32, 4), 104);
    assert_eq!(vm.read(0x5000 + 36, 4), 0);
}

#[test]
fn failed_stat_leaves_the_destination_unchanged() {
    let mut vm = Vm::new(5, [u64::MAX, 0x5000, 0, 0, 0, 0]);
    vm.put(0x5000, 1234, 8);
    vm.run(&code(), |vm| vm.registers[0] = (-9_i64) as u64);
    assert_eq!(vm.read(0x5000, 8), 1234);
}
