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

/// Linux 侧经 GOT 桩调用的全部 C 入口：DNS resolver 两个 + 外调工作线程的 gettid/errno。
const LINUX_C_IMPORTS: [&str; 13] = [
    "getaddrinfo",
    "freeaddrinfo",
    "gettid",
    "__errno_location",
    "pthread_attr_init",
    "pthread_attr_setstack",
    "pthread_create",
    "pthread_attr_destroy",
    "pthread_tryjoin_np",
    "environ",
    "setenv",
    "unsetenv",
    "getenv",
];

/// Darwin 侧经 libSystem 调用的额外入口（VM/FD/NET/进程/resolver 导入各自申报）。
pub(crate) const DARWIN_EXTRA_IMPORTS: [&str; 3] = ["exit", "write", "pthread_threadid_np"];

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
        Definition::fragment(rt("panic"), fragments::panic(&rt("panic_dispatch"))),
        Definition::fragment(
            rt("panic_resume"),
            fragments::panic_resume(&rt("panic_unwind"), &rt("process_exit")),
        ),
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
    let shared = [
        // 镜像私有退出桩：未绑定 runtime glue 的死路径引用兜底到这里（与 x64
        // `resolve_target` 的 `__gugu_stub_exit` 语义一致）；符号不走 glue 命名空间。
        Definition::fragment("__gugu_stub_exit", fragments::process_exit(platform)),
        Definition::fragment(rt("cage_control"), fragments::cage_control()),
        Definition::fragment(rt("callback_control"), fragments::callback_control()),
        Definition::fragment(rt("callback_queue"), fragments::callback_queue()),
        Definition::fragment(
            rt("callback_file_worker"),
            fragments::callback_file_worker(),
        ),
        Definition::fragment(
            rt("process_native_worker"),
            fragments::process_native_worker(),
        ),
        Definition::fragment(rt("stack_read_word"), fragments::stack_read_word()),
        Definition::fragment(rt("stack_copy_raw"), fragments::stack_copy_raw()),
    ];
    definitions.extend(shared);
    match platform {
        Platform::Linux => definitions.extend(
            [
                Definition {
                    symbol: fragments::LINUX_RESERVE_CURSOR.into(),
                    align: 8,
                    segment: Segment::Data,
                    content: SectionContent::Data(
                        fragments::LINUX_RESERVE_START.to_le_bytes().to_vec(),
                    ),
                    relocations: vec![],
                    pointers: vec![],
                },
                Definition::fragment(rt("fs_native_worker"), fragments::linux_fd_dispatch(true)),
                Definition::fragment(rt("fd_dispatch"), fragments::linux_fd_dispatch(false)),
                Definition::fragment(rt("net_call"), fragments::linux_net_dispatch()),
                Definition::fragment(rt("resolver_symbol"), fragments::linux_resolver_symbol()),
                Definition::fragment(rt("process_call"), fragments::linux_process_call()),
                Definition::fragment(rt("process_worker"), fragments::linux_process_worker()),
                Definition::fragment(rt("fs_start"), fragments::linux_fs_start()),
                Definition::fragment(rt("fs_poll"), fragments::linux_fs_poll()),
                Definition::fragment(rt("monotonic_nanos"), fragments::linux_monotonic_nanos()),
                Definition::fragment(rt("wall_nanos"), fragments::linux_wall_nanos()),
                Definition::fragment(
                    rt("parallelism_probe"),
                    fragments::linux_parallelism_probe(),
                ),
                Definition::fragment(rt("startup_call"), fragments::linux_startup_call()),
                Definition::fragment(
                    rt("environment_symbol"),
                    fragments::linux_environment_symbol(),
                ),
                Definition::fragment(rt("net_close"), fragments::linux_net_close()),
                Definition::fragment(rt("page_size"), fragments::linux_page_size()),
            ]
            .into_iter()
            .chain(linux_import_definitions()),
        ),
        Platform::Darwin => definitions.extend([
            Definition::fragment(rt("fs_native_worker"), fragments::darwin_fd_dispatch(true)),
            Definition::fragment(rt("fd_dispatch"), fragments::darwin_fd_dispatch(false)),
            Definition::fragment(rt("net_call"), fragments::darwin_net_dispatch()),
            Definition::fragment(rt("resolver_symbol"), fragments::darwin_resolver_symbol()),
            Definition::fragment(rt("process_call"), fragments::darwin_process_call()),
            Definition::fragment(rt("process_worker"), fragments::darwin_process_worker()),
            Definition::fragment(rt("fs_start"), fragments::darwin_fs_start()),
            Definition::fragment(rt("fs_poll"), fragments::darwin_fs_poll()),
            Definition::fragment(rt("monotonic_nanos"), fragments::darwin_monotonic_nanos()),
            Definition::fragment(rt("wall_nanos"), fragments::darwin_wall_nanos()),
            Definition::fragment(
                rt("parallelism_probe"),
                fragments::darwin_parallelism_probe(),
            ),
            Definition::fragment(rt("startup_call"), fragments::darwin_startup_call()),
            Definition::fragment(
                rt("environment_symbol"),
                fragments::darwin_environment_symbol(),
            ),
            Definition::fragment(rt("net_close"), fragments::darwin_net_close()),
        ]),
    }
    definitions
}

/// Linux 动态链接的 eager-import 槽与桩：resolver 返回的地址指向桩符号，
/// 桩经 GOT 槽跳到 libc 实现。桩符号以 `__gugu_linux_stub_` 命名（与 import 真名错开），
/// 槽是文件回填的 8 字节 Data，供 ELF dynamic 写 GLOB_DAT。
pub fn linux_import_definitions() -> Vec<Definition> {
    LINUX_C_IMPORTS
        .into_iter()
        .flat_map(|name| {
            [
                Definition::fragment(
                    fragments::linux_import_stub_symbol(name),
                    fragments::linux_import_stub(name),
                ),
                Definition {
                    symbol: fragments::linux_import_slot_symbol(name),
                    align: 8,
                    segment: Segment::Data,
                    content: SectionContent::Data(vec![0; 8]),
                    relocations: vec![],
                    pointers: vec![],
                },
            ]
        })
        .collect()
}

/// 片段声明的原生导入；Darwin 全部经 libSystem（去重），Linux 是 resolver/工作线程的 libc 面。
pub fn runtime_imports(platform: Platform) -> Vec<NativeImport> {
    match platform {
        Platform::Linux => LINUX_C_IMPORTS
            .map(|name| NativeImport::function(LINUX_LIBC, name))
            .into(),
        Platform::Darwin => {
            let mut imports = [
                fragments::darwin_vm_imports(),
                fragments::darwin_fd_imports(),
                fragments::darwin_net_imports(),
                fragments::darwin_process_imports(),
                fragments::darwin_resolver_imports(),
                fragments::native_imports(Platform::Darwin),
            ]
            .concat();
            imports
                .extend(DARWIN_EXTRA_IMPORTS.map(|name| NativeImport::function(LIB_SYSTEM, name)));
            let mut seen = std::collections::BTreeSet::new();
            imports.retain(|import| seen.insert(import.name.clone()));
            imports
        }
    }
}
