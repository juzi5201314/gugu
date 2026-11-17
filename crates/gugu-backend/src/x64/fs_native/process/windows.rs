//! Windows x64 C ABI 薄封装；所有 OVERLAPPED 请求缓冲由 Gugu 持有。
use super::*;

pub(crate) const SYMBOLS: &[&str] = &[
    "CancelIoEx",
    "CloseHandle",
    "ConnectNamedPipe",
    "CreateFileW",
    "CreateNamedPipeW",
    "CreateProcessW",
    "DeleteProcThreadAttributeList",
    "DuplicateHandle",
    "GetCurrentProcess",
    "GetCurrentProcessId",
    "GetExitCodeProcess",
    "GetLastError",
    "GetOverlappedResult",
    "GetProcessHeap",
    "GetStdHandle",
    "HeapAlloc",
    "HeapFree",
    "InitializeProcThreadAttributeList",
    "ReadFile",
    "TerminateProcess",
    "UpdateProcThreadAttribute",
    "WaitForSingleObject",
    "WriteFile",
];
fn error(b: &mut Builder) {
    win_call(b, "GetLastError");
    dword_result(b);
    store(b, R12, E, Rax);
}
fn bool_result(b: &mut Builder) {
    let ok = b.label();
    compare(b, Rax, 0);
    jump(b, "jne", ok);
    error(b);
    b.define(ok);
}
fn stack_arg(b: &mut Builder, offset: i32, value: Gpr) {
    store(b, Rsp, offset, value);
}

pub(super) fn call() -> Builder {
    let mut b = Builder::new();
    push(&mut b, R12);
    rr(&mut b, R12, Rbx);
    adjust(&mut b, "sub", Rsp, 48);
    clear_result(&mut b);
    load(&mut b, R10, R12, 0);
    b.emit("cmp", MOV, Access::Read, vec![reg(gpr(Rax)), reg(gpr(R10))]);
    let invalid = b.label();
    jump(&mut b, "jne", invalid);
    let done = b.label();
    for op in 0..7 {
        let next = b.label();
        compare(&mut b, Rax, op);
        jump(&mut b, "jne", next);
        match op {
            0 => observe(&mut b),
            1 => {
                load(&mut b, Rcx, R12, A);
                number(&mut b, Rdx, 1);
                win_call(&mut b, "TerminateProcess");
                bool_result(&mut b);
            }
            2 => {
                load(&mut b, Rcx, R12, A);
                win_call(&mut b, "CloseHandle");
                bool_result(&mut b);
            }
            3 | 4 => {
                load(&mut b, Rcx, R12, A);
                load(&mut b, Rdx, R12, A + 8);
                load(&mut b, R8, R12, A + 16);
                address(&mut b, R9, R12, V);
                load(&mut b, R10, R12, A + 24);
                stack_arg(&mut b, 32, R10);
                win_call(&mut b, if op == 3 { "ReadFile" } else { "WriteFile" });
                bool_result(&mut b);
            }
            5 => {
                load(&mut b, Rcx, R12, A);
                load(&mut b, Rdx, R12, A + 24);
                address(&mut b, R8, R12, V);
                number(&mut b, R9, 0);
                win_call(&mut b, "GetOverlappedResult");
                bool_result(&mut b);
            }
            6 => {
                load(&mut b, Rcx, R12, A);
                load(&mut b, Rdx, R12, A + 24);
                win_call(&mut b, "CancelIoEx");
                bool_result(&mut b);
            }
            _ => unreachable!(),
        }
        jump(&mut b, "jmp", done);
        b.define(next);
    }
    b.define(invalid);
    trap(&mut b);
    b.define(done);
    number(&mut b, Rax, 0);
    adjust(&mut b, "add", Rsp, 48);
    pop(&mut b, R12);
    ret(&mut b);
    b
}
fn observe(b: &mut Builder) {
    load(b, Rcx, R12, A);
    number(b, Rdx, 0);
    win_call(b, "WaitForSingleObject");
    dword_result(b);
    let done = b.label();
    let exited = b.label();
    compare(b, Rax, 258);
    jump(b, "je", done);
    compare(b, Rax, 0);
    jump(b, "je", exited);
    error(b);
    jump(b, "jmp", done);
    b.define(exited);
    set(b, R12, A + 8, 0);
    load(b, Rcx, R12, A);
    address(b, Rdx, R12, A + 8);
    win_call(b, "GetExitCodeProcess");
    bool_result(b);
    load(b, Rax, R12, E);
    compare(b, Rax, 0);
    jump(b, "jne", done);
    // WaitForSingleObject 已确认终态，259 与其它 u32 退出码同样可收集。
    set(b, R12, V, 1);
    b.define(done);
}

pub(super) fn worker(b: &mut Builder) {
    for r in [R12, R13, R14, R15, Rbx] {
        push(b, r);
    }
    adjust(b, "sub", Rsp, 384);
    rr(b, R12, Rcx);
    clear_result(b);
    // scratch 同时存放 STARTUPINFOEX、PROCESS_INFORMATION 或 pipe OVERLAPPED。
    for offset in (80..384).step_by(8) {
        set(b, Rsp, offset, 0);
    }
    let done = b.label();
    for op in 0..4 {
        let next = b.label();
        load(b, Rax, R12, 0);
        compare(b, Rax, op);
        jump(b, "jne", next);
        match op {
            0 => spawn(b),
            1 => pipe(b),
            2 => null(b),
            3 => duplicate(b),
            _ => unreachable!(),
        }
        jump(b, "jmp", done);
        b.define(next);
    }
    trap(b);
    b.define(done);
    number(b, Rax, 0);
    adjust(b, "add", Rsp, 384);
    for r in [Rbx, R15, R14, R13, R12] {
        pop(b, r);
    }
    ret(b);
}
fn duplicate(b: &mut Builder) {
    let supplied = b.label();
    load(b, R13, R12, A);
    load(b, Rax, R12, A + 8);
    compare(b, Rax, 0);
    jump(b, "je", supplied);
    number(b, Rcx, -10);
    b.emit(
        "sub",
        MOV,
        Access::ReadWrite,
        vec![reg(gpr(Rcx)), reg(gpr(R13))],
    );
    win_call(b, "GetStdHandle");
    rr(b, R13, Rax);
    b.define(supplied);
    win_call(b, "GetCurrentProcess");
    rr(b, Rcx, Rax);
    rr(b, R8, Rax);
    rr(b, Rdx, R13);
    address(b, R9, R12, V);
    set(b, Rsp, 32, 0);
    set(b, Rsp, 40, 1);
    set(b, Rsp, 48, 2); // DUPLICATE_SAME_ACCESS。
    win_call(b, "DuplicateHandle");
    bool_result(b);
}
fn security(b: &mut Builder) {
    set(b, Rsp, 240, 24);
    set(b, Rsp, 248, 0);
    set(b, Rsp, 256, 1);
}
fn null(b: &mut Builder) {
    security(b);
    set(b, Rsp, 272, 0x0055004e);
    set(b, Rsp, 280, 0);
    b.emit(
        "mov",
        &[K::Rm32, K::Imm32],
        Access::Write,
        vec![mem_base(gpr(Rsp), 276), imm(76)],
    );
    address(b, Rcx, Rsp, 272);
    number(b, Rdx, i32::MIN); // GENERIC_READ。
    load(b, Rax, R12, A);
    compare(b, Rax, 0);
    let access = b.label();
    jump(b, "je", access);
    number(b, Rdx, 0x40000000);
    b.define(access);
    number(b, R8, 3);
    address(b, R9, Rsp, 240);
    set(b, Rsp, 32, 3);
    set(b, Rsp, 40, 0x80);
    set(b, Rsp, 48, 0);
    win_call(b, "CreateFileW");
    handle_result(b);
}
fn handle_result(b: &mut Builder) {
    let ok = b.label();
    let done = b.label();
    compare(b, Rax, -1);
    jump(b, "jne", ok);
    error(b);
    jump(b, "jmp", done);
    b.define(ok);
    store(b, R12, V, Rax);
    b.define(done);
}
fn pipe(b: &mut Builder) {
    security(b);
    load(b, Rcx, R12, A + 8);
    number(b, Rdx, 0x40080002); // OVERLAPPED | FIRST_PIPE_INSTANCE | OUTBOUND。
    load(b, Rax, R12, A);
    compare(b, Rax, 0);
    let mode = b.label();
    jump(b, "je", mode);
    number(b, Rdx, 0x40080001);
    b.define(mode);
    number(b, R8, 8); // byte mode | PIPE_REJECT_REMOTE_CLIENTS。
    number(b, R9, 1);
    set(b, Rsp, 32, 65536);
    set(b, Rsp, 40, 65536);
    set(b, Rsp, 48, 0);
    set(b, Rsp, 56, 0); // parent 不继承。
    win_call(b, "CreateNamedPipeW");
    let failed = b.label();
    let already_connected = b.label();
    let done = b.label();
    compare(b, Rax, -1);
    jump(b, "je", failed);
    rr(b, R13, Rax);
    rr(b, Rcx, R13);
    address(b, Rdx, Rsp, 80);
    win_call(b, "ConnectNamedPipe");
    let connecting = b.label();
    let close_parent = b.label();
    compare(b, Rax, 0);
    jump(b, "jne", already_connected);
    win_call(b, "GetLastError");
    dword_result(b);
    compare(b, Rax, 997);
    jump(b, "je", connecting);
    compare(b, Rax, 535); // ERROR_PIPE_CONNECTED。
    jump(b, "je", already_connected);
    store(b, R12, E, Rax);
    jump(b, "jmp", close_parent);
    b.define(already_connected);
    set(b, Rsp, 120, 1);
    b.define(connecting);
    load(b, Rcx, R12, A + 8);
    number(b, Rdx, i32::MIN);
    load(b, Rax, R12, A);
    compare(b, Rax, 0);
    let access = b.label();
    jump(b, "je", access);
    number(b, Rdx, 0x40000000);
    b.define(access);
    number(b, R8, 0);
    address(b, R9, Rsp, 240);
    set(b, Rsp, 32, 3);
    set(b, Rsp, 40, 0x80); // child 同步句柄。
    set(b, Rsp, 48, 0);
    win_call(b, "CreateFileW");
    let child_open = b.label();
    compare(b, Rax, -1);
    jump(b, "jne", child_open);
    error(b);
    rr(b, Rcx, R13);
    address(b, Rdx, Rsp, 80);
    win_call(b, "CancelIoEx");
    // 取消后仍收集终态，不能让栈 OVERLAPPED 提前失效。
    rr(b, Rcx, R13);
    address(b, Rdx, Rsp, 80);
    address(b, R8, Rsp, 112);
    number(b, R9, 1);
    win_call(b, "GetOverlappedResult");
    jump(b, "jmp", close_parent);
    b.define(child_open);
    rr(b, R14, Rax);
    load(b, Rax, Rsp, 120);
    let connected = b.label();
    compare(b, Rax, 0);
    jump(b, "jne", connected);
    rr(b, Rcx, R13);
    address(b, Rdx, Rsp, 80);
    address(b, R8, Rsp, 112);
    number(b, R9, 1);
    win_call(b, "GetOverlappedResult");
    compare(b, Rax, 0);
    jump(b, "jne", connected);
    error(b);
    rr(b, Rcx, R14);
    win_call(b, "CloseHandle");
    jump(b, "jmp", close_parent);
    b.define(connected);
    store(b, R12, V, R13);
    store(b, R12, A + 16, R14);
    jump(b, "jmp", done);
    b.define(close_parent);
    rr(b, Rcx, R13);
    win_call(b, "CloseHandle");
    jump(b, "jmp", done);
    b.define(failed);
    error(b);
    b.define(done);
}
fn spawn(b: &mut Builder) {
    load(b, R13, R12, A);
    // 只含一个 HANDLE_LIST 属性，系统报告其不透明存储大小。
    number(b, Rcx, 0);
    number(b, Rdx, 1);
    number(b, R8, 0);
    address(b, R9, Rsp, 232);
    win_call(b, "InitializeProcThreadAttributeList");
    win_call(b, "GetProcessHeap");
    rr(b, R15, Rax);
    rr(b, Rcx, R15);
    number(b, Rdx, 0);
    load(b, R8, Rsp, 232);
    win_call(b, "HeapAlloc");
    rr(b, R14, Rax);
    let allocated = b.label();
    let done = b.label();
    compare(b, R14, 0);
    jump(b, "jne", allocated);
    set(b, R12, E, 8); // HeapAlloc 不保证设置 LastError。
    jump(b, "jmp", done);
    b.define(allocated);
    rr(b, Rcx, R14);
    number(b, Rdx, 1);
    number(b, R8, 0);
    address(b, R9, Rsp, 232);
    win_call(b, "InitializeProcThreadAttributeList");
    let initialized = b.label();
    let free = b.label();
    compare(b, Rax, 0);
    jump(b, "jne", initialized);
    error(b);
    jump(b, "jmp", free);
    b.define(initialized);
    for (source, destination) in [(32, 200), (40, 208), (48, 216)] {
        load(b, Rax, R13, source);
        store(b, Rsp, destination, Rax);
    }
    rr(b, Rcx, R14);
    number(b, Rdx, 0);
    number(b, R8, 0x20002); // PROC_THREAD_ATTRIBUTE_HANDLE_LIST。
    address(b, R9, Rsp, 200);
    set(b, Rsp, 32, 24);
    set(b, Rsp, 40, 0);
    set(b, Rsp, 48, 0);
    win_call(b, "UpdateProcThreadAttribute");
    let configured = b.label();
    let delete = b.label();
    compare(b, Rax, 0);
    jump(b, "jne", configured);
    error(b);
    jump(b, "jmp", delete);
    b.define(configured);
    set(b, Rsp, 80, 112); // STARTUPINFOEXW.cb。
    set(b, Rsp, 140, 0x100); // STARTF_USESTDHANDLES。
    for (source, destination) in [(32, 160), (40, 168), (48, 176)] {
        load(b, Rax, R13, source);
        store(b, Rsp, destination, Rax);
    }
    store(b, Rsp, 184, R14);
    load(b, Rcx, R13, 0);
    load(b, Rdx, R13, 8);
    number(b, R8, 0);
    number(b, R9, 0);
    set(b, Rsp, 32, 1);
    set(b, Rsp, 40, 0x80400); // EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT。
    load(b, Rax, R13, 16);
    stack_arg(b, 48, Rax);
    load(b, Rax, R13, 24);
    stack_arg(b, 56, Rax);
    address(b, Rax, Rsp, 80);
    stack_arg(b, 64, Rax);
    address(b, Rax, Rsp, 288);
    stack_arg(b, 72, Rax);
    win_call(b, "CreateProcessW");
    let created = b.label();
    compare(b, Rax, 0);
    jump(b, "jne", created);
    error(b);
    jump(b, "jmp", delete);
    b.define(created);
    load(b, Rax, Rsp, 288);
    store(b, R13, 56, Rax);
    store(b, R12, V, Rax);
    dword_load(b, Rax, Rsp, 304);
    store(b, R13, 64, Rax);
    load(b, Rcx, Rsp, 296);
    win_call(b, "CloseHandle");
    b.define(delete);
    rr(b, Rcx, R14);
    win_call(b, "DeleteProcThreadAttributeList");
    b.define(free);
    rr(b, Rcx, R15);
    number(b, Rdx, 0);
    rr(b, R8, R14);
    win_call(b, "HeapFree");
    b.define(done);
    load(b, Rax, R12, E);
    store(b, R13, 72, Rax);
}
