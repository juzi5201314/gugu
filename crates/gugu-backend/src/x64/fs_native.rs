//! 文件系统 native worker 边界；请求只含 non-moving raw 地址。
//! Linux 以 CHILD_CLEARTID、Windows 以 thread handle 确认退出后才允许复用请求与栈。
use super::encode;
use super::fragments::{FragmentBlob, FragmentDomain, FragmentEntry};
use super::inst::{LabelId, Operand, RelocKind, RelocTarget};
use super::lower::{Builder, gpr, imm, mem_base, reg};
use super::mangle;
use super::reg::Gpr::{self, *};
use super::table::{Access, OperandKind as K};
use crate::hash::hash_domain;
use crate::lir::body::Symbol;
use crate::runtime::foreign_schema as bridge;
use crate::target::TargetName;
mod callback;
pub(crate) mod darwin;
pub(crate) mod environment;
pub(crate) mod exports;
mod foreign;
pub(crate) mod net;
pub(crate) mod process;
mod stack;
const OPERATION: i32 = bridge::NATIVE_REQUEST_OPERATION as i32;
const ARGUMENTS: i32 = bridge::NATIVE_REQUEST_ARGUMENTS as i32;
const VALUE: i32 = bridge::NATIVE_REQUEST_VALUE as i32;
const ERROR: i32 = bridge::NATIVE_REQUEST_ERROR as i32;
const THREAD_HANDLE: i32 = bridge::NATIVE_REQUEST_THREAD_HANDLE as i32;
const EXTRA_ARGUMENTS: i32 = bridge::NATIVE_REQUEST_EXTRA_ARGUMENTS as i32;
const MOV: &[K] = &[K::Rm64, K::R64];
const IMM: &[K] = &[K::Rm64, K::Imm32];
fn rr(b: &mut Builder, dst: Gpr, src: Gpr) {
    b.emit(
        "mov",
        MOV,
        Access::Write,
        vec![reg(gpr(dst)), reg(gpr(src))],
    );
}
fn load(b: &mut Builder, dst: Gpr, base: Gpr, offset: i32) {
    b.emit(
        "mov",
        MOV,
        Access::Read,
        vec![mem_base(gpr(base), offset), reg(gpr(dst))],
    );
}
fn store(b: &mut Builder, base: Gpr, offset: i32, src: Gpr) {
    b.emit(
        "mov",
        MOV,
        Access::Write,
        vec![mem_base(gpr(base), offset), reg(gpr(src))],
    );
}
fn number(b: &mut Builder, dst: Gpr, value: i32) {
    b.emit(
        "mov",
        &[K::R32, K::Imm32],
        Access::Write,
        vec![reg(gpr(dst)), imm(value as i64 as u64)],
    );
}
fn set(b: &mut Builder, base: Gpr, offset: i32, value: i32) {
    b.emit(
        "mov",
        IMM,
        Access::Write,
        vec![mem_base(gpr(base), offset), imm(value as i64 as u64)],
    );
}
fn adjust(b: &mut Builder, op: &'static str, dst: Gpr, value: i32) {
    b.emit(
        op,
        IMM,
        Access::ReadWrite,
        vec![reg(gpr(dst)), imm(value as i64 as u64)],
    );
}
fn jump(b: &mut Builder, op: &'static str, target: LabelId) {
    b.emit(op, &[K::Rel32], Access::Read, vec![Operand::Label(target)]);
}
fn compare(b: &mut Builder, register: Gpr, value: i32) {
    b.emit(
        "cmp",
        IMM,
        Access::Read,
        vec![reg(gpr(register)), imm(value as i64 as u64)],
    );
}
fn ret(b: &mut Builder) {
    b.emit("ret", &[], Access::Read, vec![]);
}
fn syscall(b: &mut Builder) {
    b.emit("syscall", &[], Access::Read, vec![]);
}
fn push(b: &mut Builder, r: Gpr) {
    b.emit("push", &[K::R64], Access::Read, vec![reg(gpr(r))]);
}
fn pop(b: &mut Builder, r: Gpr) {
    b.emit("pop", &[K::R64], Access::Write, vec![reg(gpr(r))]);
}
fn iat(name: &str) -> Operand {
    Operand::Rip(
        RelocTarget::Lir(Symbol::External {
            key: hash_domain("gugu-pe-iat-symbol-v1", name.as_bytes()),
            name: format!("__gugu_iat_{name}"),
        }),
        0,
    )
}
/// `__gugu_signal_mailbox`：镜像保留可写单元（16B，见 pe.rs/elf.rs 保留区）。
/// +0 存信号 ctl 页地址；SetConsoleCtrlHandler 回调无上下文，靠它找到 ctl 页。
fn signal_mailbox() -> Operand {
    Operand::Rip(
        RelocTarget::Lir(Symbol::External {
            key: hash_domain("gugu-signal-mailbox-v1", b"signal_mailbox"),
            name: "__gugu_signal_mailbox".to_owned(),
        }),
        0,
    )
}
fn win_call(b: &mut Builder, name: &str) {
    b.emit("call", &[K::Mem], Access::Read, vec![iat(name)]);
}
fn dword_result(b: &mut Builder) {
    b.emit(
        "mov",
        &[K::Rm32, K::R32],
        Access::Write,
        vec![reg(gpr(Rax)), reg(gpr(Rax))],
    );
}
fn blob(name: &'static str, b: Builder) -> FragmentBlob {
    let encoded = encode::assemble(&b.finish().sequence).expect("文件系统 native 片段编码");
    FragmentBlob {
        alignment: 16,
        domain: FragmentDomain::Runtime,
        bytes: encoded.bytes,
        entries: vec![FragmentEntry {
            glue: name,
            offset: 0,
        }],
        relocations: encoded.relocations,
    }
}
/// 调度、额度与错误分类由 Gugu 实现；机器片段只负责 native 线程入口。
pub fn fragments(target: TargetName) -> Vec<FragmentBlob> {
    let builders = match target {
        TargetName::X86_64Linux => [
            linux_start(),
            linux_poll(),
            linux_worker(),
            linux_clock(7),
            linux_clock(1),
            linux_parallelism_probe(),
        ],
        TargetName::X86_64Windows => [
            windows_start(),
            windows_poll(),
            windows_worker(),
            windows_clock(),
            windows_wall_clock(),
            windows_parallelism_probe(),
        ],
        TargetName::X86_64Macos => [
            darwin::start(),
            darwin::poll(),
            darwin::worker(true),
            darwin::clock(6),
            darwin::clock(0),
            darwin::parallelism(),
        ],
    };
    [
        "fs_start",
        "fs_poll",
        "fs_native_worker",
        "monotonic_nanos",
        "wall_nanos",
        "parallelism_probe",
    ]
    .into_iter()
    .zip(builders)
    .map(|(name, builder)| blob(name, builder))
    .chain(std::iter::once(blob(
        "foreign_native_worker",
        foreign::worker(target),
    )))
    .chain([
        blob(
            "callback_file_worker",
            match target {
                TargetName::X86_64Linux => linux_file_worker(false),
                TargetName::X86_64Windows => windows_worker(),
                TargetName::X86_64Macos => darwin::worker(false),
            },
        ),
        blob("thread_id", thread_id(target)),
        blob("callback_control", callback::control()),
        blob("callback_queue", callback::queue()),
        blob("stack_read_word", stack::read_word()),
        blob("cage_control", stack::cage_control()),
        blob("stack_copy_raw", stack::copy_raw()),
    ])
    .chain([net::fragment(target), net::resolver_fragment(target)])
    .chain([environment::startup(target), environment::symbols(target)])
    .chain([process::call(target), process::worker(target)])
    .chain(
        (target == TargetName::X86_64Windows)
            .then(|| {
                [
                    blob("signal_install", windows_signal_install()),
                    blob("signal_handler", windows_signal_handler()),
                ]
            })
            .into_iter()
            .flatten(),
    )
    .collect()
}

/// 可选片段统一按已生成代码的重定位可达性选择，三个镜像消费者共用。
pub(crate) fn required(
    fragments: &[crate::x64::codegen::FragmentPayload],
    blob: &FragmentBlob,
) -> bool {
    let glue = blob.entries[0].glue;
    !matches!(
        glue,
        "net_call"
            | "resolver_symbol"
            | "environment_symbol"
            | "startup_call"
            | "process_call"
            | "process_worker"
    ) || net::required(fragments, glue)
}
fn thread_id(target: TargetName) -> Builder {
    if target == TargetName::X86_64Macos {
        return darwin::thread_id();
    }
    let mut b = Builder::new();
    match target {
        TargetName::X86_64Linux => {
            number(&mut b, Rax, 186);
            syscall(&mut b);
        }
        TargetName::X86_64Windows => {
            adjust(&mut b, "sub", Rsp, 40);
            win_call(&mut b, "GetCurrentThreadId");
            dword_result(&mut b);
            adjust(&mut b, "add", Rsp, 40);
        }
        TargetName::X86_64Macos => unreachable!(),
    }
    ret(&mut b);
    b
}

fn multiply(b: &mut Builder, scale: u64) {
    b.emit(
        "imul",
        &[K::Rm64, K::R64, K::Imm32],
        Access::Read,
        vec![reg(gpr(Rax)), reg(gpr(Rax)), imm(scale)],
    );
}
/// `clock_id`：单调源用 7（CLOCK_BOOTTIME，含系统挂起时间），墙钟用 1（CLOCK_REALTIME）。
fn linux_clock(clock_id: i32) -> Builder {
    let mut b = Builder::new();
    let failed = b.label();
    adjust(&mut b, "sub", Rsp, 16);
    number(&mut b, Rax, 228);
    number(&mut b, Rdi, clock_id);
    rr(&mut b, Rsi, Rsp);
    syscall(&mut b);
    compare(&mut b, Rax, 0);
    jump(&mut b, "jne", failed);
    load(&mut b, Rax, Rsp, 0);
    multiply(&mut b, 1_000_000_000);
    load(&mut b, R10, Rsp, 8);
    b.emit(
        "add",
        MOV,
        Access::ReadWrite,
        vec![reg(gpr(Rax)), reg(gpr(R10))],
    );
    adjust(&mut b, "add", Rsp, 16);
    ret(&mut b);
    b.define(failed);
    number(&mut b, Rax, 231);
    number(&mut b, Rdi, 2);
    syscall(&mut b);
    b.emit("ud2", &[], Access::Read, vec![]);
    b
}
fn windows_clock() -> Builder {
    let mut b = Builder::new();
    adjust(&mut b, "sub", Rsp, 40);
    win_call(&mut b, "GetTickCount64");
    multiply(&mut b, 1_000_000);
    adjust(&mut b, "add", Rsp, 40);
    ret(&mut b);
    b
}
/// FILETIME 是 1601-01-01 起 100 纳秒计数；减去 Unix epoch 偏移后乘 100 得到墙钟纳秒。
/// `signal_install(ctl, event)`：把 ctl 页登记进镜像邮箱（ctl+64 存事件句柄），
/// 然后 `SetConsoleCtrlHandler(signal_handler, TRUE)`。返回 BOOL（非 0 成功）。
fn windows_signal_install() -> Builder {
    let mut b = Builder::new();
    b.emit(
        "mov",
        MOV,
        Access::Write,
        vec![signal_mailbox(), reg(gpr(Rax))],
    );
    store(&mut b, Rax, 64, Rbx);
    adjust(&mut b, "sub", Rsp, 40);
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![
            Operand::Rip(
                RelocTarget::Lir(mangle::runtime_symbol("signal_handler")),
                0,
            ),
            reg(gpr(Rcx)),
        ],
    );
    number(&mut b, Rdx, 1);
    win_call(&mut b, "SetConsoleCtrlHandler");
    dword_result(&mut b);
    adjust(&mut b, "add", Rsp, 40);
    ret(&mut b);
    b
}
/// `signal_handler(ctrl)`：控制台 ctrl 回调（ecx = CTRL_* 事件码）。
/// 运行在 Windows 自起的 OS 线程上，只做两件原子小事：把 `1 << ctrl` 并入
/// ctl+72 的 signo 掩码、然后 SetEvent(ctl+64)；绝不触碰托管代码或堆。
fn windows_signal_handler() -> Builder {
    let mut b = Builder::new();
    b.emit(
        "mov",
        MOV,
        Access::Read,
        vec![signal_mailbox(), reg(gpr(Rax))],
    );
    number(&mut b, Rdx, 1);
    b.emit(
        "shl",
        &[K::Rm32, K::Cl],
        Access::ReadWrite,
        vec![reg(gpr(Rdx)), reg(gpr(Rcx))],
    );
    b.emit(
        "or",
        &[K::Rm32, K::R32],
        Access::ReadWrite,
        vec![mem_base(gpr(Rax), 72), reg(gpr(Rdx))],
    );
    load(&mut b, Rcx, Rax, 64);
    adjust(&mut b, "sub", Rsp, 40);
    win_call(&mut b, "SetEvent");
    adjust(&mut b, "add", Rsp, 40);
    number(&mut b, Rax, 1);
    ret(&mut b);
    b
}
fn windows_wall_clock() -> Builder {
    let mut b = Builder::new();
    adjust(&mut b, "sub", Rsp, 40);
    // 输出缓冲放在 shadow space 之上，callee 的寄存器回写不能覆盖它。
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![mem_base(gpr(Rsp), 32), reg(gpr(Rcx))],
    );
    win_call(&mut b, "GetSystemTimePreciseAsFileTime");
    load(&mut b, Rax, Rsp, 32);
    b.emit(
        "mov",
        &[K::R64, K::Imm64],
        Access::Write,
        vec![reg(gpr(R10)), imm(116_444_736_000_000_000)],
    );
    b.emit(
        "sub",
        MOV,
        Access::ReadWrite,
        vec![reg(gpr(Rax)), reg(gpr(R10))],
    );
    multiply(&mut b, 100);
    adjust(&mut b, "add", Rsp, 40);
    ret(&mut b);
    b
}

/// `sched_getaffinity(0, 8, mask)`：宿主可用 CPU 掩码位计数；失败返回 0，调用方回落到 1。
fn linux_parallelism_probe() -> Builder {
    let mut b = Builder::new();
    let failed = b.label();
    let bits = b.label();
    adjust(&mut b, "sub", Rsp, 16);
    number(&mut b, Rax, 204);
    number(&mut b, Rdi, 0);
    number(&mut b, Rsi, 8);
    rr(&mut b, Rdx, Rsp);
    syscall(&mut b);
    compare(&mut b, Rax, 0);
    jump(&mut b, "jl", failed);
    load(&mut b, Rdx, Rsp, 0);
    number(&mut b, Rax, 0);
    number(&mut b, Rcx, 64);
    b.define(bits);
    rr(&mut b, R11, Rdx);
    b.emit("and", IMM, Access::ReadWrite, vec![reg(gpr(R11)), imm(1)]);
    b.emit(
        "add",
        MOV,
        Access::ReadWrite,
        vec![reg(gpr(Rax)), reg(gpr(R11))],
    );
    b.emit(
        "shr",
        &[K::Rm64, K::Imm8],
        Access::ReadWrite,
        vec![reg(gpr(Rdx)), imm(1)],
    );
    adjust(&mut b, "sub", Rcx, 1);
    jump(&mut b, "jne", bits);
    adjust(&mut b, "add", Rsp, 16);
    ret(&mut b);
    b.define(failed);
    number(&mut b, Rax, 0);
    adjust(&mut b, "add", Rsp, 16);
    ret(&mut b);
    b
}
/// `GetSystemInfo` 的 `dwNumberOfProcessors` 在 SYSTEM_INFO +32；失败时 API 不写值，返回 0。
fn windows_parallelism_probe() -> Builder {
    let mut b = Builder::new();
    adjust(&mut b, "sub", Rsp, 48);
    rr(&mut b, Rcx, Rsp);
    win_call(&mut b, "GetSystemInfo");
    load(&mut b, Rax, Rsp, 32);
    dword_result(&mut b);
    adjust(&mut b, "add", Rsp, 48);
    ret(&mut b);
    b
}
fn linux_start() -> Builder {
    let mut b = Builder::new();
    let file = b.label();
    load(&mut b, R11, Rax, OPERATION);
    compare(&mut b, R11, -1);
    jump(&mut b, "jne", file);
    foreign::linux_start(&mut b);
    b.define(file);
    let child = b.label();
    let failed = b.label();
    rr(&mut b, Rdx, Rax);
    rr(&mut b, R10, Rax);
    rr(&mut b, Rsi, Rbx);
    adjust(&mut b, "sub", Rsi, 16);
    store(&mut b, Rsi, 0, Rax);
    number(&mut b, R8, 0);
    // VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|PARENT_SETTID|CHILD_CLEARTID。
    number(&mut b, Rdi, 0x350f00);
    number(&mut b, Rax, 56);
    syscall(&mut b);
    compare(&mut b, Rax, 0);
    jump(&mut b, "je", child);
    jump(&mut b, "jl", failed);
    number(&mut b, Rax, 0);
    ret(&mut b);
    b.define(failed);
    b.emit("neg", &[K::Rm64], Access::ReadWrite, vec![reg(gpr(Rax))]);
    store(&mut b, Rdx, ERROR, Rax);
    ret(&mut b);
    b.define(child);
    load(&mut b, Rdi, Rsp, 0);
    b.emit(
        "jmp",
        &[K::Rel32],
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(mangle::runtime_symbol("fs_native_worker")),
            RelocKind::PcRel32,
        )],
    );
    b
}
fn linux_poll() -> Builder {
    let mut b = Builder::new();
    let file = b.label();
    load(&mut b, R11, Rax, OPERATION);
    compare(&mut b, R11, -1);
    jump(&mut b, "jne", file);
    foreign::linux_poll(&mut b);
    b.define(file);
    let pending = b.label();
    // tid 为内核32位字，高32位在提交前清零。x86 load 提供 acquire。
    load(&mut b, Rax, Rax, 0);
    compare(&mut b, Rax, 0);
    jump(&mut b, "jne", pending);
    number(&mut b, Rax, 1);
    ret(&mut b);
    b.define(pending);
    number(&mut b, Rax, 0);
    ret(&mut b);
    b
}
fn linux_worker() -> Builder {
    linux_file_worker(true)
}
fn linux_file_worker(exit_thread: bool) -> Builder {
    let mut b = Builder::new();
    if !exit_thread {
        push(&mut b, R12);
    }
    let accepted = b.label();
    let done = b.label();
    rr(&mut b, R12, Rdi);
    load(&mut b, Rax, R12, OPERATION);
    for code in [
        0, 1, 2, 3, 5, 8, 14, 72, 74, 75, 77, 82, 83, 84, 86, 87, 88, 89, 217, 262, 289,
    ] {
        compare(&mut b, Rax, code);
        jump(&mut b, "je", accepted);
    }
    set(&mut b, R12, VALUE, 0);
    set(&mut b, R12, ERROR, 22);
    jump(&mut b, "jmp", done);
    b.define(accepted);
    for (index, r) in [Rdi, Rsi, Rdx, R10, R8, R9].into_iter().enumerate() {
        load(&mut b, r, R12, ARGUMENTS + index as i32 * 8);
    }
    syscall(&mut b);
    store(&mut b, R12, VALUE, Rax);
    compare(&mut b, Rax, 0);
    jump(&mut b, "jge", done);
    b.emit("neg", &[K::Rm64], Access::ReadWrite, vec![reg(gpr(Rax))]);
    store(&mut b, R12, ERROR, Rax);
    b.define(done);
    if exit_thread {
        number(&mut b, Rdi, 0);
        number(&mut b, Rax, 60);
        syscall(&mut b);
        b.emit("ud2", &[], Access::Read, vec![]);
    } else {
        pop(&mut b, R12);
        ret(&mut b);
    }
    b
}
fn windows_start() -> Builder {
    let mut b = Builder::new();
    push(&mut b, R12);
    adjust(&mut b, "sub", Rsp, 48);
    rr(&mut b, R12, Rax);
    rr(&mut b, R9, Rax);
    number(&mut b, Rcx, 0);
    number(
        &mut b,
        Rdx,
        crate::runtime::foreign_schema::BLOCKING_WORKER_STACK_BYTES as i32,
    );
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![
            Operand::Rip(
                RelocTarget::Lir(mangle::runtime_symbol("fs_native_worker")),
                0,
            ),
            reg(gpr(R8)),
        ],
    );
    let file_worker = b.label();
    load(&mut b, R11, R12, OPERATION);
    compare(&mut b, R11, -1);
    jump(&mut b, "jne", file_worker);
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![
            Operand::Rip(
                RelocTarget::Lir(mangle::runtime_symbol("foreign_native_worker")),
                0,
            ),
            reg(gpr(R8)),
        ],
    );
    b.define(file_worker);
    set(&mut b, Rsp, 32, 0);
    set(&mut b, Rsp, 40, 0);
    win_call(&mut b, "CreateThread");
    store(&mut b, R12, THREAD_HANDLE, Rax);
    let success = b.label();
    let end = b.label();
    compare(&mut b, Rax, 0);
    jump(&mut b, "jne", success);
    win_call(&mut b, "GetLastError");
    dword_result(&mut b);
    store(&mut b, R12, ERROR, Rax);
    jump(&mut b, "jmp", end);
    b.define(success);
    number(&mut b, Rax, 0);
    b.define(end);
    adjust(&mut b, "add", Rsp, 48);
    pop(&mut b, R12);
    ret(&mut b);
    b
}
fn windows_poll() -> Builder {
    let mut b = Builder::new();
    push(&mut b, R12);
    adjust(&mut b, "sub", Rsp, 32);
    rr(&mut b, R12, Rax);
    load(&mut b, Rcx, R12, THREAD_HANDLE);
    number(&mut b, Rdx, 0);
    win_call(&mut b, "WaitForSingleObject");
    dword_result(&mut b);
    let pending = b.label();
    let failed = b.label();
    let end = b.label();
    compare(&mut b, Rax, 258);
    jump(&mut b, "je", pending);
    compare(&mut b, Rax, 0);
    jump(&mut b, "jne", failed);
    load(&mut b, Rcx, R12, THREAD_HANDLE);
    win_call(&mut b, "CloseHandle");
    dword_result(&mut b);
    compare(&mut b, Rax, 0);
    jump(&mut b, "je", failed);
    set(&mut b, R12, THREAD_HANDLE, 0);
    number(&mut b, Rax, 1);
    jump(&mut b, "jmp", end);
    b.define(pending);
    number(&mut b, Rax, 0);
    b.define(end);
    adjust(&mut b, "add", Rsp, 32);
    pop(&mut b, R12);
    ret(&mut b);
    b.define(failed);
    number(&mut b, Rcx, 2);
    win_call(&mut b, "ExitProcess");
    b.emit("ud2", &[], Access::Read, vec![]);
    b
}
/// opcode、薄入口名、失败哨兵；参数按 Win64 ABI 传递。
const WINDOWS_CALLS: &[(i32, &str, i32)] = &[
    (0, "CreateFileW", -1),
    (1, "CloseHandle", 0),
    (2, "ReadFile", 0),
    (3, "WriteFile", 0),
    (4, "SetFilePointerEx", 0),
    (5, "GetFileInformationByHandle", 0),
    (6, "SetEndOfFile", 0),
    (7, "FlushFileBuffers", 0),
    (8, "DuplicateHandle", 0),
    (9, "FindFirstFileW", -1),
    (10, "FindNextFileW", 0),
    (11, "FindClose", 0),
    (12, "CreateDirectoryW", 0),
    (13, "DeleteFileW", 0),
    (14, "RemoveDirectoryW", 0),
    (15, "MoveFileExW", 0),
    (16, "CreateHardLinkW", 0),
    (17, "CreateSymbolicLinkW", 0),
    (18, "DeviceIoControl", 0),
    (19, "GetFinalPathNameByHandleW", 0),
    (20, "SetFileInformationByHandle", 0),
    (21, "CreateEventW", 0),
    (22, "WaitForSingleObject", -1),
];
fn windows_worker() -> Builder {
    let mut b = Builder::new();
    push(&mut b, R12);
    adjust(&mut b, "sub", Rsp, 64);
    rr(&mut b, R12, Rcx);
    let done = b.label();
    for &(code, name, failure) in WINDOWS_CALLS {
        let next = b.label();
        load(&mut b, Rax, R12, OPERATION);
        compare(&mut b, Rax, code);
        jump(&mut b, "jne", next);
        for (index, r) in [Rcx, Rdx, R8, R9].into_iter().enumerate() {
            load(&mut b, r, R12, ARGUMENTS + index as i32 * 8);
        }
        for index in 0..4 {
            load(
                &mut b,
                Rax,
                R12,
                if index < 2 {
                    ARGUMENTS + 32 + index * 8
                } else {
                    EXTRA_ARGUMENTS + (index - 2) * 8
                },
            );
            store(&mut b, Rsp, 32 + index * 8, Rax);
        }
        win_call(&mut b, name);
        if name == "CreateSymbolicLinkW" {
            b.emit("and", IMM, Access::ReadWrite, vec![reg(gpr(Rax)), imm(255)]);
        } else if name == "WaitForSingleObject" {
            // 失败哨兵是 WAIT_FAILED (0xFFFFFFFF) 而非 -1；先零扩展再按 dword 比较。
            dword_result(&mut b);
        } else if name == "CreateEventW" {
            // 返回 HANDLE 保持原生宽度；失败哨兵是 NULL，不能 dword 截断。
        } else if failure == 0 {
            dword_result(&mut b);
        }
        store(&mut b, R12, VALUE, Rax);
        if name == "WaitForSingleObject" {
            b.emit(
                "cmp",
                &[K::Rm32, K::Imm32],
                Access::Read,
                vec![reg(gpr(Rax)), imm(0xffff_ffff)],
            );
        } else {
            compare(&mut b, Rax, failure);
        }
        jump(&mut b, "jne", done);
        win_call(&mut b, "GetLastError");
        dword_result(&mut b);
        store(&mut b, R12, ERROR, Rax);
        jump(&mut b, "jmp", done);
        b.define(next);
    }
    set(&mut b, R12, ERROR, 87);
    b.define(done);
    number(&mut b, Rax, 0);
    adjust(&mut b, "add", Rsp, 64);
    pop(&mut b, R12);
    ret(&mut b);
    b
}
#[cfg(test)]
pub(crate) mod protocol_tests;
