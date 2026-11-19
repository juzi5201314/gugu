use super::linux_vm::{REQUEST, Vm};
use super::*;
use crate::runtime::platform_schema::PlatformOp;
use std::collections::VecDeque;

fn vm_code(op: PlatformOp) -> Vec<Inst> {
    let fragment = linux_vm(op).unwrap();
    assert!(fragment.relocations.is_empty());
    instructions(&fragment)
}

#[test]
fn page_size_walks_envp_and_auxv_for_each_supported_kernel_size() {
    let code = instructions(&linux_page_size());
    for page in [4096, 16384, 65536] {
        let mut vm = Vm::new(0, [0; 6]);
        for (i, value) in [0x2000, 0x2100, 0, 3, 0x4000, 6, page, 0, 0]
            .into_iter()
            .enumerate()
        {
            vm.put(REQUEST + i as u64 * 8, value, 8);
        }
        assert_eq!(
            vm.run_result(&code, |_| panic!("page probe made a syscall")),
            page
        );
    }
}

#[test]
fn missing_auxv_page_size_returns_zero_without_a_fixed_fallback() {
    for pointer in [0, REQUEST] {
        let mut vm = Vm::new(0, [0; 6]);
        vm.registers[0] = pointer;
        for i in 0..32 {
            vm.put(REQUEST + i * 8, 0, 8);
        }
        assert_eq!(
            vm.run_result(&instructions(&linux_page_size()), |_| panic!()),
            0
        );
    }
}

#[test]
fn descriptor_operations_preserve_raw_results_and_native_lengths() {
    for (op, nr, arg) in [
        (PlatformOp::Commit, 226, Some(3)),
        (PlatformOp::Unprotect, 226, Some(3)),
        (PlatformOp::ProtectGuard, 226, Some(0)),
        (PlatformOp::Decommit, 233, Some(4)),
        (PlatformOp::Release, 215, None),
    ] {
        for page in [4096, 16384, 65536] {
            for result in [0, (-22_i64) as u64] {
                let mut vm = Vm::new(0, [0; 6]);
                vm.put(REQUEST, 0x100000, 8);
                vm.put(REQUEST + 8, page, 8);
                assert_eq!(
                    vm.run_result(&vm_code(op), |vm| {
                        assert_eq!(vm.registers[8], nr);
                        assert_eq!(&vm.registers[..2], &[0x100000, page]);
                        if let Some(arg) = arg {
                            assert_eq!(vm.registers[2], arg);
                        }
                        vm.registers[0] = result;
                    }),
                    result
                );
            }
        }
    }
}

#[test]
fn aligned_reservation_trims_prefix_and_tail_for_all_native_page_sizes() {
    for page in [4096_u64, 16384, 65536] {
        for base in [page * 17, page * 32] {
            let size = page * 3;
            let alignment = page * 16;
            let aligned = (base + alignment - 1) & !(alignment - 1);
            let mut script = VecDeque::from(vec![(
                222,
                vec![0, size + alignment, 0, 34, u64::MAX, 0],
                base,
            )]);
            if aligned != base {
                script.push_back((215, vec![base, aligned - base], 0));
            }
            script.push_back((215, vec![aligned + size, alignment - (aligned - base)], 0));
            let mut vm = Vm::new(0, [0; 6]);
            vm.registers[0] = size;
            vm.registers[1] = alignment;
            assert_eq!(
                vm.run_result(&vm_code(PlatformOp::ReserveAligned), |vm| {
                    let (nr, args, result) = script.pop_front().unwrap();
                    assert_eq!(vm.registers[8], nr);
                    assert_eq!(vm.registers[..args.len()], args);
                    vm.registers[0] = result;
                }),
                aligned
            );
            assert!(script.is_empty());
        }
    }
}

#[test]
fn reservation_rejects_empty_invalid_alignment_and_overflow_without_syscalls() {
    for (size, align) in [(0, 4096), (4096, 0), (4096, 3), (u64::MAX, 4096)] {
        let mut vm = Vm::new(0, [0; 6]);
        vm.registers[0] = size;
        vm.registers[1] = align;
        assert_eq!(
            vm.run_result(&vm_code(PlatformOp::ReserveAligned), |_| panic!()),
            (-22_i64) as u64
        );
    }
}

#[test]
fn reservation_failure_returns_errno_and_trim_failures_release_only_owned_mapping() {
    let code = vm_code(PlatformOp::ReserveAligned);
    for failure in [0, 1, 2] {
        let mut script = match failure {
            0 => VecDeque::from(vec![(222, 0, (-12_i64) as u64)]),
            1 => VecDeque::from(vec![
                (222, 0, 0x11000),
                (215, 0x11000, (-5_i64) as u64),
                (215, 0x11000, 0),
            ]),
            _ => VecDeque::from(vec![
                (222, 0, 0x11000),
                (215, 0x11000, 0),
                (215, 0x21000, (-5_i64) as u64),
                (215, 0x20000, 0),
            ]),
        };
        let mut vm = Vm::new(0, [0; 6]);
        vm.registers[0] = 4096;
        vm.registers[1] = 65536;
        assert_eq!(
            vm.run_result(&code, |vm| {
                let (nr, address, result) = script.pop_front().unwrap();
                assert_eq!(vm.registers[8], nr);
                if nr == 215 {
                    assert_eq!(vm.registers[0], address);
                    if script.is_empty() {
                        assert_eq!(vm.registers[1], if failure == 1 { 0x11000 } else { 0x2000 });
                    }
                }
                vm.registers[0] = result;
            }),
            if failure == 0 {
                (-12_i64) as u64
            } else {
                (-5_i64) as u64
            }
        );
        assert!(script.is_empty());
    }
}

#[test]
fn non_range_platform_operations_are_explicitly_unavailable() {
    for op in [
        PlatformOp::Wait,
        PlatformOp::Wake,
        PlatformOp::Entropy,
        PlatformOp::Zero,
        PlatformOp::SetDumpPolicy,
        PlatformOp::LowMemoryHint,
        PlatformOp::HugePageHint,
    ] {
        assert!(linux_vm(op).is_none());
    }
}
