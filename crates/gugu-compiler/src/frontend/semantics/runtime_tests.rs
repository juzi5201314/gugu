use super::tests::accepts;

#[test]
fn runtime_publish_resolves_imports_and_canonical_paths() {
    assert!(accepts(
        "use std.runtime.{ownership_publish, root_publish}\nfn main() { let a = chan[int](0)\n let b = chan[int](0)\n ownership_publish(a, b)\n root_publish(b, a) }"
    ));
    assert!(accepts(
        "fn main() { let a = chan[int](0)\n let b = chan[int](0)\n std.runtime.ownership_publish(a, b) }"
    ));
}

#[test]
fn runtime_publish_requires_destination_and_value() {
    assert!(!accepts(
        "use std.runtime.{ownership_publish}\nfn main() { let a = chan[int](0)\n ownership_publish(a) }"
    ));
}

#[test]
fn runtime_publish_rejects_temporary_destination() {
    assert!(!accepts(
        "use std.runtime.{ownership_publish}\nfn main() { let a = chan[int](0)\n ownership_publish(chan[int](0), a) }"
    ));
}

#[test]
fn runtime_publish_rejects_aggregate_values() {
    assert!(!accepts(
        "use std.runtime.{ownership_publish}\nstruct Holder { value: int }\nfn main() { let a = Holder { value: 1 }\n let b = Holder { value: 2 }\n ownership_publish(a, b) }"
    ));
}

#[test]
fn runtime_publish_checks_explicit_handle_type() {
    assert!(accepts(
        "use std.runtime.{root_publish as publish}\nfn main() { let a = chan[int](0)\n let b = chan[int](0)\n publish::[chan[int]](a, b) }"
    ));
    assert!(!accepts(
        "use std.runtime.{root_publish}\nfn main() { let a = chan[int](0)\n let b = chan[int](0)\n root_publish::[chan[int], chan[int]](a, b) }"
    ));
    assert!(!accepts(
        "use std.runtime.{root_publish}\nfn main() { let a = chan[int](0)\n let b = chan[bool](0)\n root_publish(a, b) }"
    ));
}

#[test]
fn runtime_machine_intrinsics_accept_fixed_shapes() {
    // `current_slot` 无实参、返回机器字；`process_exit` 接受一个机器字。
    assert!(accepts(
        "fn main() { unsafe { _ = std.runtime.current_slot()\n let code: uint = 0\n std.runtime.process_exit(code) } }"
    ));
    // `report_write` 形状 `(uint, *byte, uint) uint`。
    assert!(accepts(
        "use std.ptr\nfn main() { let value: uint = 0\n unsafe { let data = ptr.addr_of(value)\n let fd: uint = 2\n let len: uint = 8\n _ = std.runtime.report_write(fd, data, len) } }"
    ));
    // `context_switch`/`context_restore` 形状为四/三个原始指针。
    assert!(accepts(
        "use std.ptr\nfn main() { let value: uint = 0\n unsafe { let p = ptr.addr_of(value)\n std.runtime.context_switch(p, p, p, p)\n std.runtime.context_restore(p, p, p) } }"
    ));
}

/// GC 控制原语按注入来源授权：普通源码即使写 `unsafe` 也不能调用，而既有机器原语的契约
/// （按规范路径识别）保持不变。
#[test]
fn gc_control_intrinsics_require_runtime_source_identity() {
    assert!(!accepts(
        "fn main() { unsafe { _ = std.runtime.gc_config() } }"
    ));
    assert!(accepts(
        "fn main() { unsafe { _ = std.runtime.current_slot() } }"
    ));
}

#[test]
fn runtime_machine_intrinsics_require_unsafe_and_fixed_shapes() {
    // 非 unsafe 调用被拒。
    assert!(!accepts("fn main() { _ = std.runtime.current_slot() }"));
    // 指针位置给有符号机器字被拒。
    assert!(!accepts(
        "fn main() { unsafe { let n: int = 0\n std.runtime.context_switch(n, n, n, n) } }"
    ));
    // 机器字位置给原始指针被拒。
    assert!(!accepts(
        "use std.ptr\nfn main() { let value: uint = 0\n unsafe { std.runtime.process_exit(ptr.addr_of(value)) } }"
    ));
    // 错误返回类型被拒。
    assert!(!accepts(
        "fn main() { unsafe { let slot: bool = std.runtime.current_slot()\n _ = slot } }"
    ));
    // 实参数量错误被拒。
    assert!(!accepts(
        "fn main() { unsafe { std.runtime.process_exit() } }"
    ));
    // 类型实参被拒。
    assert!(!accepts(
        "fn main() { unsafe { _ = std.runtime.current_slot::[uint]() } }"
    ));
}

#[test]
fn gc_atomic_authorization_rejects_aliases_and_counterfeit_runtime_paths() {
    for path in ["main.gg", "std/runtime/counterfeit.gg"] {
        for call in [
            "std.runtime.atomic_load_acquire(p)",
            "std.runtime.atomic_store_release[uint](p, uint(1))",
            "std.runtime.atomic_fetch_or_acq_rel(p, uint(1))",
            "and_mask(p, uint(1))",
        ] {
            let source = format!(
                "use std.runtime.{{atomic_fetch_and_acq_rel as and_mask}}\nfn main() {{ let value: uint = 0\n unsafe {{ let p = std.ptr.addr_of(value)\n _ = {call} }} }}"
            );
            let compilation = crate::Compiler::new().compile(crate::CompileRequest::single_file(
                path,
                &source,
                crate::TargetName::X86_64Linux,
            ));
            if path.starts_with("std/runtime/") {
                assert!(
                    compilation.diagnostics().items().iter().any(|diagnostic| {
                        diagnostic.code() == crate::DiagnosticCode::ReservedName
                    }),
                    "{path}: {call}: {:?}",
                    compilation.diagnostics().items()
                );
            } else {
                assert!(
                    !compilation.is_success(),
                    "用户入口意外获得 runtime 原子权限：{call}"
                );
            }
            assert!(compilation.image_plan().is_none());
        }
    }
}
