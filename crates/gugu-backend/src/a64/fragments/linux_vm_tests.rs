use super::linux_vm::{REQUEST, Vm};
use super::*;
use crate::{
    a64::relocation,
    runtime::{cage_control::CAGE_CANONICAL_LIMIT, platform_schema::PlatformOp},
};
use std::collections::VecDeque;

const CURSOR: u64 = 0x2000;
const HINT_GRANULE: u64 = 65536;

fn vm_code(op: PlatformOp) -> Vec<Inst> {
    let mut fragment = linux_vm(op).unwrap();
    for relocation in &fragment.relocations {
        assert_eq!(relocation.relocation.target, LINUX_RESERVE_CURSOR);
        assert_eq!(relocation.relocation.addend, 0);
        let offset = relocation.offset as usize;
        let word = u32::from_le_bytes(fragment.bytes[offset..offset + 4].try_into().unwrap());
        let patched = relocation::patch(
            word,
            relocation.relocation.kind,
            u64::from(relocation.offset),
            CURSOR,
        )
        .unwrap();
        fragment.bytes[offset..offset + 4].copy_from_slice(&patched);
    }
    instructions(&fragment)
}

fn reservation_vm(size: u64, alignment: u64) -> Vm {
    let mut vm = Vm::new(0, [0; 6]);
    vm.put(CURSOR, LINUX_RESERVE_START, 8);
    vm.registers[0] = size;
    vm.registers[1] = alignment;
    vm
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
                vec![LINUX_RESERVE_START, size + alignment, 0, 34, u64::MAX, 0],
                base,
            )]);
            if aligned != base {
                script.push_back((215, vec![base, aligned - base], 0));
            }
            script.push_back((215, vec![aligned + size, alignment - (aligned - base)], 0));
            let mut vm = reservation_vm(size, alignment);
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
    for (size, align) in [
        (0, 4096),
        (4096, 0),
        (4096, 3),
        (u64::MAX, 4096),
        (CAGE_CANONICAL_LIMIT, 4096),
    ] {
        let mut vm = reservation_vm(size, align);
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
        let mut vm = reservation_vm(4096, 65536);
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

#[test]
fn reservation_releases_out_of_range_results_before_retrying_and_accepts_boundary() {
    let code = vm_code(PlatformOp::ReserveAligned);
    let limit = CAGE_CANONICAL_LIMIT;
    for rejected in [0, limit, limit - 4096] {
        let mut script = VecDeque::from([
            (222, LINUX_RESERVE_START, 8192, rejected),
            (215, rejected, 8192, 0),
            (222, LINUX_RESERVE_START + HINT_GRANULE, 8192, limit - 8192),
            (215, limit - 4096, 4096, 0),
        ]);
        let mut vm = reservation_vm(4096, 4096);
        assert_eq!(
            vm.run_result(&code, |vm| {
                let (nr, base, length, result) = script.pop_front().unwrap();
                assert_eq!(vm.registers[8], nr);
                assert_eq!(&vm.registers[..2], &[base, length]);
                if nr == 222 {
                    assert_eq!(vm.registers[3], 34, "hint 不得覆盖已有映射");
                }
                vm.registers[0] = result;
            }),
            limit - 8192
        );
        assert!(script.is_empty());
    }
}

#[test]
fn reservation_bounds_retry_count_and_preserves_cleanup_errors() {
    let code = vm_code(PlatformOp::ReserveAligned);
    for release_result in [0, (-5_i64) as u64] {
        let mut vm = reservation_vm(4096, 4096);
        let mut maps = 0;
        let mut releases = 0;
        let status = vm.run_result(&code, |vm| match vm.registers[8] {
            222 => {
                assert_eq!(maps, releases);
                assert_eq!(vm.registers[0], LINUX_RESERVE_START + maps * HINT_GRANULE);
                maps += 1;
                vm.registers[0] = CAGE_CANONICAL_LIMIT;
            }
            215 => {
                assert_eq!(&vm.registers[..2], &[CAGE_CANONICAL_LIMIT, 8192]);
                releases += 1;
                vm.registers[0] = release_result;
            }
            number => panic!("未预期的系统调用 {number}"),
        });
        assert_eq!(maps, releases);
        assert_eq!(
            (maps, status),
            if release_result == 0 {
                (16, (-12_i64) as u64)
            } else {
                (1, release_result)
            }
        );
    }
}

#[test]
fn reservation_cursor_wraps_and_advances_across_calls() {
    let code = vm_code(PlatformOp::ReserveAligned);
    let mut vm = reservation_vm(4096, 4096);
    vm.put(CURSOR, CAGE_CANONICAL_LIMIT - HINT_GRANULE, 8);
    for expected in [
        CAGE_CANONICAL_LIMIT - HINT_GRANULE,
        LINUX_RESERVE_START,
        LINUX_RESERVE_START + HINT_GRANULE,
    ] {
        vm.registers[0] = 4096;
        vm.registers[1] = 4096;
        let mut mapped = false;
        assert_eq!(
            vm.run_result(&code, |vm| {
                if !mapped {
                    assert_eq!(vm.registers[8], 222);
                    assert_eq!(&vm.registers[..2], &[expected, 8192]);
                    vm.registers[0] = expected;
                    mapped = true;
                } else {
                    assert_eq!(vm.registers[8], 215);
                    assert_eq!(&vm.registers[..2], &[expected + 4096, 4096]);
                    vm.registers[0] = 0;
                }
            }),
            expected
        );
        assert!(mapped);
    }
    assert_eq!(vm.read(CURSOR, 8), LINUX_RESERVE_START + 2 * HINT_GRANULE);
}

#[test]
fn reservation_bounds_exclusive_contention_without_advancing_unclaimed_hints() {
    let code = vm_code(PlatformOp::ReserveAligned);
    for failures in [2, 16] {
        let mut vm = reservation_vm(4096, 4096);
        vm.exclusive_failures = failures;
        let mut maps = 0;
        let status = vm.run_result(&code, |vm| {
            assert_eq!(vm.registers[8], 222);
            assert_eq!(vm.registers[0], LINUX_RESERVE_START);
            assert_eq!(vm.exclusive_failures, 0);
            maps += 1;
            vm.registers[0] = (-5_i64) as u64;
        });
        assert_eq!(
            (maps, status, vm.read(CURSOR, 8)),
            if failures == 16 {
                (0, (-12_i64) as u64, LINUX_RESERVE_START)
            } else {
                (1, (-5_i64) as u64, LINUX_RESERVE_START + HINT_GRANULE)
            }
        );
    }
}
