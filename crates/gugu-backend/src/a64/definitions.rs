//! Canonical glue name → machine fragment definitions for a supported platform.
//!
//! Every runtime glue resolves to the shared `__gugu_runtime_*` symbol namespace:
//! fragment implementations are emitted here, and source-bound (Item) glues get an
//! alias on the same name in `World::symbols`, so call sites never distinguish.
use super::{foreign::Platform, fragments, world::Definition};
use crate::{
    NativeImport,
    macho::{LIB_SYSTEM, SectionContent, Segment},
    runtime::{GcImageConfig, platform_schema::PlatformOp},
    x64::mangle,
};

fn rt(name: &str) -> String {
    mangle::mangle_runtime(name)
}

/// glibc SONAME for the Linux resolver's C imports; matches every glibc aarch64 system.
pub const LINUX_LIBC: &str = "libc.so.6";

/// Darwin 侧经 libSystem 调用的额外入口（VM/FD/NET/进程/resolver 导入各自申报）。
const DARWIN_EXTRA_IMPORTS: [&str; 3] = ["exit", "write", "pthread_threadid_np"];

/// 平台已实现的 `platform_*` VM 原语；guard/unprotect 有片段覆盖，其余保持未绑定。
const PLATFORM_OPS: [PlatformOp; 6] = [
    PlatformOp::ReserveAligned,
    PlatformOp::Commit,
    PlatformOp::Decommit,
    PlatformOp::Release,
    PlatformOp::ProtectGuard,
    PlatformOp::Unprotect,
];

fn linux_vm_fragment(op: PlatformOp) -> Option<fragments::Fragment> {
    fragments::linux_vm(op)
}

fn darwin_vm_fragment(op: PlatformOp) -> Option<fragments::Fragment> {
    fragments::darwin_vm(op)
}

/// 哨兵/调度/平台 glue 的片段定义；调用点统一经 `__gugu_runtime_*` 符号到达。
/// `coroutine_enter` 不是 glue：rt0 目录按地址引用它，符号名仍走同一命名空间便于镜像消费。
pub fn runtime_definitions(platform: Platform) -> Vec<Definition> {
    let switch = rt("context_switch");
    let context = fragments::context_switch();
    let mut definitions = vec![
        Definition::fragment(rt("context_switch"), context.fragment.clone()),
        Definition::fragment(
            rt("context_restore"),
            fragments::context_restore(&switch, context.restore_offset),
        ),
        Definition::fragment(
            rt("morestack_or_poll"),
            fragments::morestack_or_poll(&switch).fragment,
        ),
        Definition::fragment(
            rt("safepoint_slow"),
            fragments::safepoint_slow(&rt("callback_poll"), &rt("gc_enter")),
        ),
        Definition::fragment(rt("current_slot"), fragments::current_slot()),
        Definition::fragment(rt("runtime_poll_defer"), fragments::runtime_poll_defer()),
        Definition::fragment(rt("process_exit"), fragments::process_exit(platform)),
        Definition::fragment(
            rt("main_return"),
            fragments::main_return(&switch, context.restore_offset),
        ),
        Definition::fragment(rt("report_write"), fragments::report_write(platform)),
        Definition::fragment(
            rt("coroutine_enter"),
            fragments::coroutine_enter(&switch, context.restore_offset),
        ),
        Definition::fragment(rt("gc_enter"), fragments::gc_enter(&rt("gc_dispatch"))),
        Definition::fragment(
            rt("gc_config"),
            fragments::gc_config(&rt("gc_config_record")),
        ),
        Definition {
            symbol: rt("gc_config_record"),
            align: 8,
            segment: Segment::DataConst,
            content: SectionContent::Data(GcImageConfig::build().canonical_bytes()),
            relocations: vec![],
            pointers: vec![],
        },
        Definition::fragment(
            rt("foreign_native_worker"),
            fragments::foreign_native_worker(platform),
        ),
        Definition::fragment(rt("thread_id"), fragments::thread_id(platform)),
    ];
    let vm = match platform {
        Platform::Linux => linux_vm_fragment,
        Platform::Darwin => darwin_vm_fragment,
    };
    for op in PLATFORM_OPS {
        if let Some(fragment) = vm(op) {
            definitions.push(Definition::fragment(
                rt(&format!("platform_{}", op.name())),
                fragment,
            ));
        }
    }
    match platform {
        Platform::Linux => definitions.extend(
            [
                Definition::fragment(rt("fs_native_worker"), fragments::linux_fd_dispatch()),
                Definition::fragment(rt("net_call"), fragments::linux_net_dispatch()),
                Definition::fragment(rt("resolver_symbol"), fragments::linux_resolver_symbol()),
                Definition::fragment(rt("process_call"), fragments::linux_process_call()),
                Definition::fragment(rt("process_worker"), fragments::linux_process_worker()),
            ]
            .into_iter()
            .chain(linux_import_definitions()),
        ),
        Platform::Darwin => definitions.extend([
            Definition::fragment(rt("fs_native_worker"), fragments::darwin_fd_dispatch()),
            Definition::fragment(rt("net_call"), fragments::darwin_net_dispatch()),
            Definition::fragment(rt("resolver_symbol"), fragments::darwin_resolver_symbol()),
            Definition::fragment(rt("process_call"), fragments::darwin_process_call()),
            Definition::fragment(rt("process_worker"), fragments::darwin_process_worker()),
        ]),
    }
    definitions
}

/// Linux 动态链接的 eager-import 槽与桩：resolver 返回的地址指向桩符号，
/// 桩经 GOT 槽跳到 libSystem/libc 实现。槽定义以 import 名命名，供 ELF dynamic 填充。
pub fn linux_import_definitions() -> Vec<Definition> {
    ["getaddrinfo", "freeaddrinfo"]
        .into_iter()
        .flat_map(|name| {
            [
                Definition::fragment(name, fragments::linux_import_stub(name)),
                Definition {
                    symbol: fragments::linux_import_slot_symbol(name),
                    align: 8,
                    segment: Segment::Data,
                    content: SectionContent::ZeroFill(8),
                    relocations: vec![],
                    pointers: vec![],
                },
            ]
        })
        .collect()
}

/// 片段声明的原生导入；Darwin 全部经 libSystem，Linux 仅 resolver 的 libc 函数。
pub fn runtime_imports(platform: Platform) -> Vec<NativeImport> {
    match platform {
        Platform::Linux => fragments::linux_resolver_imports(LINUX_LIBC),
        Platform::Darwin => {
            let mut imports = [
                fragments::darwin_vm_imports(),
                fragments::darwin_fd_imports(),
                fragments::darwin_net_imports(),
                fragments::darwin_process_imports(),
                fragments::darwin_resolver_imports(),
            ]
            .concat();
            imports
                .extend(DARWIN_EXTRA_IMPORTS.map(|name| NativeImport::function(LIB_SYSTEM, name)));
            imports
        }
    }
}
