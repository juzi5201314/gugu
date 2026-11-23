//! `runtime_definitions`/`runtime_imports` producer coverage: glue namespace, import surface.
use std::collections::BTreeSet;

use crate::{
    NativeImport,
    a64::{
        definitions::{
            DARWIN_EXTRA_IMPORTS, LINUX_LIBC, linux_import_definitions, runtime_definitions,
            runtime_imports,
        },
        foreign::Platform,
        fragments::{
            darwin_fd_imports, darwin_net_imports, darwin_process_imports, darwin_resolver_imports,
            darwin_vm_imports, linux_import_slot_symbol, linux_import_stub_symbol,
        },
        world::Definition,
    },
    macho::{LIB_SYSTEM, SectionContent, Segment},
    runtime::{GcImageConfig, platform_schema::PlatformOp},
    x64::mangle::mangle_runtime,
};

fn symbols(definitions: &[Definition]) -> BTreeSet<&str> {
    definitions
        .iter()
        .map(|definition| definition.symbol.as_str())
        .collect()
}

/// 两平台共有的哨兵/调度 glue：命名空间一致、内容随平台分支的仅 VM 与 OS 分发。
fn shared() -> Vec<String> {
    [
        "context_switch",
        "context_restore",
        "morestack_or_poll",
        "safepoint_slow",
        "current_slot",
        "runtime_poll_defer",
        "process_exit",
        "main_return",
        "report_write",
        "coroutine_enter",
        "gc_enter",
        "gc_config",
        "gc_config_record",
        "foreign_native_worker",
        "thread_id",
    ]
    .into_iter()
    .map(mangle_runtime)
    .collect()
}

fn platform_ops() -> Vec<String> {
    [
        PlatformOp::ReserveAligned,
        PlatformOp::Commit,
        PlatformOp::Decommit,
        PlatformOp::Release,
        PlatformOp::ProtectGuard,
        PlatformOp::Unprotect,
    ]
    .into_iter()
    .map(|op| mangle_runtime(&format!("platform_{}", op.name())))
    .collect()
}

#[test]
fn darwin_definitions_cover_shared_glue_platform_vm_and_dispatch() {
    let definitions = runtime_definitions(Platform::Darwin);
    let names = symbols(&definitions);
    for expected in shared().into_iter().chain(platform_ops()) {
        assert!(names.contains(expected.as_str()), "missing {expected}");
    }
    for glue in [
        "fs_native_worker",
        "net_call",
        "resolver_symbol",
        "process_call",
        "process_worker",
    ] {
        assert!(names.contains(mangle_runtime(glue).as_str()));
    }
    // gc_config_record 是唯一的数据定义：GcImageConfig 的规范字节序快照。
    let record = definitions
        .iter()
        .find(|definition| definition.symbol == mangle_runtime("gc_config_record"))
        .unwrap();
    assert_eq!(record.segment, Segment::DataConst);
    assert_eq!(
        record.content,
        SectionContent::Data(GcImageConfig::build().canonical_bytes())
    );
}

#[test]
fn linux_definitions_mirror_darwin_with_linux_dispatch() {
    let definitions = runtime_definitions(Platform::Linux);
    let names = symbols(&definitions);
    for expected in shared().into_iter().chain(platform_ops()) {
        assert!(names.contains(expected.as_str()), "missing {expected}");
    }
    for glue in [
        "fs_native_worker",
        "net_call",
        "resolver_symbol",
        "process_call",
        "process_worker",
    ] {
        assert!(names.contains(mangle_runtime(glue).as_str()));
    }
}

#[test]
fn linux_import_definitions_pair_named_stub_and_file_backed_slot() {
    let definitions = linux_import_definitions();
    let names = symbols(&definitions);
    for name in ["getaddrinfo", "freeaddrinfo", "gettid", "__errno_location"] {
        // 桩符号与 import 真名错开：世界校验拒绝定义与导入重名。
        assert!(!names.contains(name), "import name must stay free {name}");
        assert!(
            names.contains(linux_import_stub_symbol(name).as_str()),
            "missing stub {name}"
        );
        assert!(
            names.contains(linux_import_slot_symbol(name).as_str()),
            "missing slot {name}"
        );
        let slot = definitions
            .iter()
            .find(|definition| definition.symbol == linux_import_slot_symbol(name))
            .unwrap();
        assert_eq!(slot.segment, Segment::Data);
        // ELF dynamic 要求槽是文件回填的 Data（写 GLOB_DAT），不收 ZeroFill。
        assert_eq!(slot.content, SectionContent::Data(vec![0; 8]));
        assert_eq!(slot.align, 8);
    }
}

#[test]
fn runtime_imports_match_platform_import_surfaces() {
    let linux = runtime_imports(Platform::Linux);
    assert_eq!(
        linux,
        ["getaddrinfo", "freeaddrinfo", "gettid", "__errno_location"]
            .map(|name| NativeImport::function(LINUX_LIBC, name)),
        "Linux imports are the resolver plus worker libc surface"
    );
    let darwin = runtime_imports(Platform::Darwin);
    // Darwin 面必须是各片段申报导入的完整去重并集：缺名立即失败。
    let mut expected: Vec<NativeImport> = [
        darwin_vm_imports(),
        darwin_fd_imports(),
        darwin_net_imports(),
        darwin_process_imports(),
        darwin_resolver_imports(),
    ]
    .concat();
    expected.extend(DARWIN_EXTRA_IMPORTS.map(|name| NativeImport::function(LIB_SYSTEM, name)));
    let mut seen = BTreeSet::new();
    expected.retain(|import| seen.insert(import.name.clone()));
    assert_eq!(darwin, expected);
}
