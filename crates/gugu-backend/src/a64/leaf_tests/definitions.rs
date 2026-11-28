//! `runtime_definitions`/`runtime_imports` producer coverage: glue namespace, import surface.
use std::collections::BTreeSet;

use crate::{
    NativeImport,
    a64::{
        definitions::{
            DARWIN_EXTRA_IMPORTS, LINUX_LIBC, LinuxFeatures, linux_import_definitions,
            runtime_definitions, runtime_imports,
        },
        foreign::Platform,
        fragments::{
            darwin_fd_imports, darwin_net_imports, darwin_process_imports, darwin_resolver_imports,
            darwin_vm_imports, linux_import_slot_symbol, linux_import_stub_symbol, native_imports,
        },
        world::Definition,
    },
    macho::{LIB_SYSTEM, SectionContent, Segment},
    runtime::{GcImageConfig, platform_schema::PlatformOp},
    x64::mangle::mangle_runtime,
};

const ALL: LinuxFeatures = LinuxFeatures {
    foreign: true,
    resolver: true,
    environment: true,
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
    let definitions = runtime_definitions(Platform::Darwin, LinuxFeatures::default());
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
    let definitions = runtime_definitions(Platform::Linux, ALL);
    let names = symbols(&definitions);
    let cursor = definitions
        .iter()
        .find(|definition| definition.symbol == crate::a64::fragments::LINUX_RESERVE_CURSOR)
        .unwrap();
    assert_eq!(cursor.segment, Segment::Data);
    assert_eq!(cursor.align, 8);
    assert_eq!(
        cursor.content,
        SectionContent::Data(
            crate::a64::fragments::LINUX_RESERVE_START
                .to_le_bytes()
                .to_vec()
        )
    );
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
    let definitions = linux_import_definitions(&runtime_imports(Platform::Linux, ALL));
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
    assert!(names.contains(linux_import_slot_symbol("environ").as_str()));
    assert!(!names.contains(linux_import_stub_symbol("environ").as_str()));
}

#[test]
fn runtime_imports_match_platform_import_surfaces() {
    let linux = runtime_imports(Platform::Linux, ALL);
    assert_eq!(
        linux,
        [
            "getaddrinfo",
            "freeaddrinfo",
            "gettid",
            "pthread_create",
            "pthread_tryjoin_np",
            "pthread_attr_init",
            "pthread_attr_setstack",
            "pthread_attr_destroy",
            "__errno_location",
            "environ",
            "setenv",
            "unsetenv",
            "getenv",
        ]
        .map(|name| if name == "environ" {
            NativeImport::data(LINUX_LIBC, name)
        } else {
            NativeImport::function(LINUX_LIBC, name)
        }),
        "Linux imports are the resolver plus worker/environment libc surface"
    );
    let darwin = runtime_imports(Platform::Darwin, LinuxFeatures::default());
    // Darwin 面必须是各片段申报导入的完整去重并集：缺名立即失败。
    let mut expected: Vec<NativeImport> = [
        darwin_vm_imports(),
        darwin_fd_imports(),
        darwin_net_imports(),
        darwin_process_imports(),
        darwin_resolver_imports(),
        native_imports(Platform::Darwin),
    ]
    .concat();
    expected.extend(DARWIN_EXTRA_IMPORTS.map(|name| NativeImport::function(LIB_SYSTEM, name)));
    let mut seen = BTreeSet::new();
    expected.retain(|import| seen.insert(import.name.clone()));
    // 与生产共用的常量若漏掉必需入口，两边会一致地错：再独立核对必需名。
    for name in ["exit", "write", "pthread_threadid_np"] {
        assert!(
            darwin.iter().any(|import| import.name == name),
            "Darwin imports must include {name}"
        );
    }
    assert_eq!(darwin, expected);
}

#[test]
fn static_linux_runtime_has_no_libc_references() {
    let features = LinuxFeatures::default();
    assert!(runtime_imports(Platform::Linux, features).is_empty());
    let definitions = runtime_definitions(Platform::Linux, features);
    let names = symbols(&definitions);
    for glue in [
        "resolver_symbol",
        "environment_symbol",
        "foreign_native_worker",
    ] {
        assert!(!names.contains(mangle_runtime(glue).as_str()));
    }
    for definition in definitions {
        for relocation in definition.relocations {
            assert!(
                !relocation
                    .relocation
                    .target
                    .starts_with("__gugu_linux_stub_")
            );
            assert!(
                !relocation
                    .relocation
                    .target
                    .starts_with("__gugu_linux_import_")
            );
        }
    }
}
