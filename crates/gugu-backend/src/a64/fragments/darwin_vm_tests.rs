use super::linux_vm::{REQUEST, Vm};
use super::*;
use crate::runtime::platform_schema::PlatformOp;
use std::collections::BTreeMap;

const ERRNO_AT: u64 = 0x6000;

#[derive(Default)]
struct LibSystem {
    calls: Vec<(String, u64, u64, u64)>,
    mmap_result: u64,
    failing: BTreeMap<String, u64>,
    errno: u64,
}

impl LibSystem {
    fn call(&mut self, vm: &mut Vm, name: &str) {
        let short = name.strip_prefix("__gugu_darwin_c_").unwrap();
        let fail = self.failing.contains_key(short);
        if fail {
            vm.put(ERRNO_AT, self.errno, 4);
        }
        self.calls.push((
            short.to_owned(),
            vm.registers[0],
            vm.registers[1],
            vm.registers[2],
        ));
        vm.registers[0] = match short {
            "__error" => ERRNO_AT,
            "mmap" if fail => u64::MAX,
            "mmap" => self.mmap_result,
            _ if fail => u64::MAX,
            _ => 0,
        };
    }
}

fn vm_code(op: PlatformOp) -> (Vec<Inst>, BTreeMap<u64, String>) {
    let fragment = darwin_vm(op).unwrap();
    let calls = fragment
        .relocations
        .iter()
        .map(|site| (u64::from(site.offset), site.relocation.target.clone()))
        .collect();
    (instructions(&fragment), calls)
}

fn request_vm() -> Vm {
    let mut vm = Vm::new(0, [0; 6]);
    vm.put(REQUEST, 0x100000, 8);
    vm.put(REQUEST + 8, 0x4000, 8);
    vm
}

#[test]
fn imports_cover_vm_calls_page_size_and_errno() {
    use crate::{NativeImport, macho::LIB_SYSTEM};

    assert_eq!(
        darwin_vm_imports(),
        ["getpagesize", "mmap", "mprotect", "munmap", "__error"]
            .map(|name| NativeImport::function(LIB_SYSTEM, name))
    );
}

#[test]
fn descriptor_operations_call_libsystem_and_translate_errno() {
    for (op, name, argument) in [
        (PlatformOp::Commit, "mprotect", Some(3)),
        (PlatformOp::Unprotect, "mprotect", Some(3)),
        (PlatformOp::ProtectGuard, "mprotect", Some(0)),
        (PlatformOp::Release, "munmap", None),
    ] {
        let (code, calls) = vm_code(op);
        let mut libsystem = LibSystem::default();
        let mut vm = request_vm();
        assert_eq!(
            vm.run_calls(&code, |vm, pc| libsystem.call(vm, &calls[&pc])),
            0,
            "{name} success"
        );
        let [(called, base, bytes, arg)] = libsystem.calls.as_slice() else {
            panic!("expected one libSystem call for {name}")
        };
        assert_eq!(*called, name);
        assert_eq!(*base, 0x100000);
        assert_eq!(*bytes, 0x4000);
        if let Some(argument) = argument {
            assert_eq!(*arg, argument, "{name} argument");
        }

        let mut libsystem = LibSystem {
            failing: BTreeMap::from([(name.to_owned(), 1)]),
            errno: 22,
            ..LibSystem::default()
        };
        let mut vm = request_vm();
        assert_eq!(
            vm.run_calls(&code, |vm, pc| libsystem.call(vm, &calls[&pc])),
            (-22_i64) as u64,
            "{name} failure"
        );
        assert_eq!(libsystem.calls[1].0, "__error");
    }
}

/// Darwin 的 decommit 用 mmap(MAP_FIXED) 覆盖清零：madvise 不保证零页。
#[test]
fn decommit_recovers_the_extent_with_a_fixed_anonymous_mapping() {
    let (code, calls) = vm_code(PlatformOp::Decommit);
    let mut libsystem = LibSystem {
        mmap_result: 0x100000,
        ..LibSystem::default()
    };
    let mut vm = request_vm();
    assert_eq!(
        vm.run_calls(&code, |vm, pc| libsystem.call(vm, &calls[&pc])),
        0
    );
    let [(called, base, bytes, prot)] = libsystem.calls.as_slice() else {
        panic!("expected the covering mmap")
    };
    assert_eq!(*called, "mmap");
    assert_eq!(*base, 0x100000);
    assert_eq!(*bytes, 0x4000);
    assert_eq!(*prot, 0);

    let mut libsystem = LibSystem {
        failing: BTreeMap::from([("mmap".to_owned(), 1)]),
        errno: 12,
        ..LibSystem::default()
    };
    let mut vm = request_vm();
    assert_eq!(
        vm.run_calls(&code, |vm, pc| libsystem.call(vm, &calls[&pc])),
        (-12_i64) as u64
    );
    assert_eq!(libsystem.calls[1].0, "__error");
}

#[test]
fn page_size_returns_getpagesize_zero_extended() {
    let fragment = darwin_page_size();
    assert_eq!(fragment.relocations.len(), 1);
    let calls = BTreeMap::from([(
        u64::from(fragment.relocations[0].offset),
        fragment.relocations[0].relocation.target.clone(),
    )]);
    let code = instructions(&fragment);
    let mut libsystem = LibSystem::default();
    let mut vm = Vm::new(0, [0; 6]);
    vm.registers[0] = 0;
    assert_eq!(
        vm.run_calls(&code, |vm, pc| {
            assert_eq!(calls[&pc], "__gugu_darwin_c_getpagesize");
            libsystem.call(vm, &calls[&pc]);
            vm.registers[0] = 16384;
        }),
        16384
    );
}

#[test]
fn reserve_trims_prefix_and_tail() {
    let (code, calls) = vm_code(PlatformOp::ReserveAligned);
    let mut libsystem = LibSystem {
        mmap_result: 0x1_0000_8000,
        ..LibSystem::default()
    };
    let mut vm = request_vm();
    vm.registers[0] = 0x1_0000;
    vm.registers[1] = 0x1_0000;
    assert_eq!(
        vm.run_calls(&code, |vm, pc| libsystem.call(vm, &calls[&pc])),
        0x1_0001_0000
    );
    assert_eq!(
        libsystem.calls,
        [
            ("mmap".to_owned(), 0, 0x2_0000, 0),
            ("munmap".to_owned(), 0x1_0000_8000, 0x8000, 0),
            ("munmap".to_owned(), 0x1_0002_0000, 0x8000, 0),
        ]
    );
}

#[test]
fn reserve_skips_prefix_when_already_aligned() {
    let (code, calls) = vm_code(PlatformOp::ReserveAligned);
    let mut libsystem = LibSystem {
        mmap_result: 0x1_0000_0000,
        ..LibSystem::default()
    };
    let mut vm = request_vm();
    vm.registers[0] = 0x1_0000;
    vm.registers[1] = 0x1_0000;
    assert_eq!(
        vm.run_calls(&code, |vm, pc| libsystem.call(vm, &calls[&pc])),
        0x1_0000_0000
    );
    assert_eq!(
        libsystem.calls,
        [
            ("mmap".to_owned(), 0, 0x2_0000, 0),
            ("munmap".to_owned(), 0x1_0001_0000, 0x1_0000, 0),
        ]
    );
}

#[test]
fn reserve_releases_owned_range_on_trim_failure() {
    let (code, calls) = vm_code(PlatformOp::ReserveAligned);
    // 前缀 munmap 失败：释放整个原始预留。
    let mut libsystem = LibSystem {
        mmap_result: 0x1_0000_8000,
        failing: BTreeMap::from([("munmap".to_owned(), 1)]),
        errno: 12,
        ..LibSystem::default()
    };
    let mut vm = request_vm();
    vm.registers[0] = 0x1_0000;
    vm.registers[1] = 0x1_0000;
    assert_eq!(
        vm.run_calls(&code, |vm, pc| libsystem.call(vm, &calls[&pc])),
        (-12_i64) as u64
    );
    assert_eq!(
        libsystem.calls,
        [
            ("mmap".to_owned(), 0, 0x2_0000, 0),
            ("munmap".to_owned(), 0x1_0000_8000, 0x8000, 0),
            ("__error".to_owned(), u64::MAX, 0x8000, 0),
            ("munmap".to_owned(), 0x1_0000_8000, 0x2_0000, 0),
        ]
    );
}

#[test]
fn reserve_releases_remaining_range_on_tail_failure() {
    let (code, calls) = vm_code(PlatformOp::ReserveAligned);
    // 第二次 munmap 失败：释放对齐后剩余的范围。
    let mut libsystem = LibSystem {
        mmap_result: 0x1_0000_8000,
        ..LibSystem::default()
    };
    let mut count = 0;
    let mut vm = request_vm();
    vm.registers[0] = 0x1_0000;
    vm.registers[1] = 0x1_0000;
    assert_eq!(
        vm.run_calls(&code, |vm, pc| {
            let name = calls[&pc].clone();
            if name == "__gugu_darwin_c_munmap" && count == 1 {
                libsystem.failing.insert("munmap".to_owned(), 1);
                libsystem.errno = 12;
            }
            if name == "__gugu_darwin_c_munmap" {
                count += 1;
            }
            libsystem.call(vm, &name)
        }),
        (-12_i64) as u64
    );
    assert_eq!(
        libsystem.calls,
        [
            ("mmap".to_owned(), 0, 0x2_0000, 0),
            ("munmap".to_owned(), 0x1_0000_8000, 0x8000, 0),
            ("munmap".to_owned(), 0x1_0002_0000, 0x8000, 0),
            ("__error".to_owned(), u64::MAX, 0x8000, 0),
            ("munmap".to_owned(), 0x1_0001_0000, 0x1_8000, 0),
        ]
    );
}

#[test]
fn reserve_maps_mmap_failure_to_negative_errno() {
    let (code, calls) = vm_code(PlatformOp::ReserveAligned);
    let mut libsystem = LibSystem {
        failing: BTreeMap::from([("mmap".to_owned(), 1)]),
        errno: 12,
        ..LibSystem::default()
    };
    let mut vm = request_vm();
    vm.registers[0] = 0x1_0000;
    vm.registers[1] = 0x1_0000;
    assert_eq!(
        vm.run_calls(&code, |vm, pc| libsystem.call(vm, &calls[&pc])),
        (-12_i64) as u64
    );
    assert_eq!(
        libsystem.calls,
        [
            ("mmap".to_owned(), 0, 0x2_0000, 0),
            ("__error".to_owned(), u64::MAX, 0x2_0000, 0),
        ]
    );
}

#[test]
fn reserve_rejects_invalid_arguments() {
    let (code, calls) = vm_code(PlatformOp::ReserveAligned);
    for (bytes, align) in [
        (0, 0x1_0000),
        (0x1_0000, 0),
        (0x1_0000, 3),
        (u64::MAX - 8, 0x20),
    ] {
        let mut libsystem = LibSystem::default();
        let mut vm = request_vm();
        vm.registers[0] = bytes;
        vm.registers[1] = align;
        assert_eq!(
            vm.run_calls(&code, |vm, pc| libsystem.call(vm, &calls[&pc])),
            (-22_i64) as u64,
            "bytes={bytes} align={align}"
        );
        assert!(libsystem.calls.is_empty());
    }
}
