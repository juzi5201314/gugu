//! Linux 仅编码 syscall；fork 子分支不进入 libc 或 Gugu。
use super::*;

pub(super) fn call() -> Builder {
    let mut b = Builder::new();
    push(&mut b, R12);
    push(&mut b, R13);
    rr(&mut b, R12, Rbx);
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
                load(&mut b, Rdi, R12, A);
                number(&mut b, Rsi, 9);
                number(&mut b, Rax, 62);
                syscall(&mut b);
                linux_result(&mut b);
            }
            2 => {
                load(&mut b, Rdi, R12, A);
                number(&mut b, Rax, 3);
                syscall(&mut b);
                linux_result(&mut b);
            }
            3 => {
                load(&mut b, Rdi, R12, A);
                load(&mut b, Rsi, R12, A + 8);
                load(&mut b, Rdx, R12, A + 16);
                number(&mut b, Rax, 0);
                syscall(&mut b);
                linux_result(&mut b);
            }
            4 => pipe_write(&mut b),
            // Linux 请求不离开当前 native 调用，无 OVERLAPPED 在途存储。
            5 | 6 => set(&mut b, R12, E, 95),
            _ => unreachable!(),
        }
        jump(&mut b, "jmp", done);
        b.define(next);
    }
    b.define(invalid);
    trap(&mut b);
    b.define(done);
    number(&mut b, Rax, 0);
    pop(&mut b, R13);
    pop(&mut b, R12);
    ret(&mut b);
    b
}
fn observe(b: &mut Builder) {
    set(b, R12, A + 8, 0);
    load(b, Rdi, R12, A);
    address(b, Rsi, R12, A + 8);
    number(b, Rdx, 1); // WNOHANG。
    number(b, R10, 0);
    number(b, Rax, 61);
    syscall(b);
    let done = b.label();
    let exited = b.label();
    compare(b, Rax, 0);
    jump(b, "je", done);
    jump(b, "jg", exited);
    linux_result(b);
    jump(b, "jmp", done);
    b.define(exited);
    dword_load(b, Rax, R12, A + 8);
    rr(b, R10, Rax);
    adjust(b, "and", R10, 127);
    let signal = b.label();
    compare(b, R10, 0);
    jump(b, "jne", signal);
    b.emit(
        "shr",
        &[K::Rm64, K::Imm8],
        Access::ReadWrite,
        vec![reg(gpr(Rax)), imm(8)],
    );
    adjust(b, "and", Rax, 255);
    store(b, R12, A + 8, Rax);
    set(b, R12, V, 1);
    jump(b, "jmp", done);
    b.define(signal);
    store(b, R12, A + 8, R10);
    set(b, R12, V, 2);
    b.define(done);
}
fn pipe_write(b: &mut Builder) {
    // 内核 sigset 为一个 u64：只屏蔽本线程 SIGPIPE，不改全进程 disposition。
    adjust(b, "sub", Rsp, 48);
    set(b, Rsp, 0, 4096);
    set(b, Rsp, 8, 0);
    set(b, Rsp, 16, 0);
    set(b, Rsp, 24, 0);
    set(b, Rsp, 32, 0);
    number(b, Rdi, 0); // SIG_BLOCK。
    rr(b, Rsi, Rsp);
    address(b, Rdx, Rsp, 8);
    number(b, R10, 8);
    number(b, Rax, 14);
    syscall(b);
    let failed = b.label();
    let done = b.label();
    compare(b, Rax, 0);
    jump(b, "jne", failed);
    address(b, Rdi, Rsp, 16);
    number(b, Rsi, 8);
    number(b, Rax, 127);
    syscall(b);
    // sigpending 参数固定有效，内核失败属于协议不变量。
    compare(b, Rax, 0);
    let pending_ok = b.label();
    jump(b, "je", pending_ok);
    trap(b);
    b.define(pending_ok);
    load(b, Rdi, R12, A);
    load(b, Rsi, R12, A + 8);
    load(b, Rdx, R12, A + 16);
    number(b, Rax, 1);
    syscall(b);
    rr(b, R13, Rax);
    let restore = b.label();
    compare(b, R13, -32);
    jump(b, "jne", restore);
    load(b, Rax, Rsp, 16);
    adjust(b, "and", Rax, 4096);
    compare(b, Rax, 0);
    jump(b, "jne", restore);
    // 本次 EPIPE 新产生的 pending signal 才消费；旧 pending 保留。
    let consume = b.label();
    b.define(consume);
    rr(b, Rdi, Rsp);
    number(b, Rsi, 0);
    address(b, Rdx, Rsp, 24);
    number(b, R10, 8);
    number(b, Rax, 128);
    syscall(b);
    compare(b, Rax, -4);
    jump(b, "je", consume);
    b.define(restore);
    number(b, Rdi, 2); // SIG_SETMASK。
    address(b, Rsi, Rsp, 8);
    number(b, Rdx, 0);
    number(b, R10, 8);
    number(b, Rax, 14);
    syscall(b);
    compare(b, Rax, 0);
    let restored = b.label();
    jump(b, "je", restored);
    trap(b);
    b.define(restored);
    rr(b, Rax, R13);
    b.define(failed);
    linux_result(b);
    b.define(done);
    adjust(b, "add", Rsp, 48);
}

pub(super) fn worker(b: &mut Builder) {
    for r in [R12, R13, R14, R15] {
        push(b, r);
    }
    rr(b, R12, Rdi);
    clear_result(b);
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
    for r in [R15, R14, R13, R12] {
        pop(b, r);
    }
    ret(b);
}
fn duplicate(b: &mut Builder) {
    load(b, Rdi, R12, A);
    number(b, Rsi, 1030); // F_DUPFD_CLOEXEC；所有临时端至少为 3。
    number(b, Rdx, 3);
    number(b, Rax, 72);
    syscall(b);
    linux_result(b);
}
fn null(b: &mut Builder) {
    adjust(b, "sub", Rsp, 16);
    // /dev/null 的原始 native 字节；不访问宿主编译环境。
    b.emit(
        "mov",
        &[K::R64, K::Imm64],
        Access::Write,
        vec![reg(gpr(Rax)), imm(u64::from_le_bytes(*b"/dev/nul"))],
    );
    store(b, Rsp, 0, Rax);
    set(b, Rsp, 8, 108);
    rr(b, Rdi, Rsp);
    number(b, Rsi, 524290); // O_RDWR | O_CLOEXEC。
    number(b, Rdx, 0);
    number(b, Rax, 2);
    syscall(b);
    linux_result(b);
    adjust(b, "add", Rsp, 16);
}
fn pipe(b: &mut Builder) {
    adjust(b, "sub", Rsp, 16);
    rr(b, Rdi, Rsp);
    number(b, Rsi, 524288);
    number(b, Rax, 293);
    syscall(b);
    let error = b.label();
    let done = b.label();
    compare(b, Rax, 0);
    jump(b, "jl", error);
    dword_load(b, R13, Rsp, 0);
    dword_load(b, R14, Rsp, 4);
    load(b, Rax, R12, A);
    compare(b, Rax, 0);
    let output = b.label();
    jump(b, "jne", output);
    rr(b, Rax, R13);
    rr(b, R13, R14);
    rr(b, R14, Rax);
    b.define(output);
    rr(b, Rdi, R13);
    number(b, Rsi, 4); // F_SETFL；仅 parent 端为 nonblocking。
    number(b, Rdx, 2048);
    number(b, Rax, 72);
    syscall(b);
    compare(b, Rax, 0);
    let failed_flags = b.label();
    jump(b, "jl", failed_flags);
    store(b, R12, V, R13);
    store(b, R12, A + 16, R14);
    jump(b, "jmp", done);
    b.define(failed_flags);
    rr(b, R15, Rax);
    for r in [R13, R14] {
        rr(b, Rdi, r);
        number(b, Rax, 3);
        syscall(b);
    }
    rr(b, Rax, R15);
    b.define(error);
    linux_result(b);
    b.define(done);
    adjust(b, "add", Rsp, 16);
}
fn spawn(b: &mut Builder) {
    load(b, R13, R12, A);
    adjust(b, "sub", Rsp, 32);
    rr(b, Rdi, Rsp);
    number(b, Rsi, 524288);
    number(b, Rax, 293);
    syscall(b);
    let error = b.label();
    let done = b.label();
    compare(b, Rax, 0);
    jump(b, "jl", error);
    number(b, Rax, 57);
    syscall(b);
    let child = b.label();
    let fork_failed = b.label();
    compare(b, Rax, 0);
    jump(b, "je", child);
    jump(b, "jl", fork_failed);
    rr(b, R14, Rax);
    dword_load(b, Rdi, Rsp, 4);
    number(b, Rax, 3);
    syscall(b);
    let read = b.label();
    b.define(read);
    dword_load(b, Rdi, Rsp, 0);
    address(b, Rsi, Rsp, 8);
    number(b, Rdx, 4);
    number(b, Rax, 0);
    syscall(b);
    compare(b, Rax, -4);
    jump(b, "je", read);
    rr(b, R15, Rax);
    dword_load(b, Rdi, Rsp, 0);
    number(b, Rax, 3);
    syscall(b);
    let executed = b.label();
    compare(b, R15, 0);
    jump(b, "je", executed);
    let collected = b.label();
    compare(b, R15, 0);
    jump(b, "jg", collected);
    // 错误管道自身失败时终止本次子进程并回收，不交付未知创建结果。
    rr(b, Rdi, R14);
    number(b, Rsi, 9);
    number(b, Rax, 62);
    syscall(b);
    b.define(collected);
    let reap = b.label();
    b.define(reap);
    rr(b, Rdi, R14);
    number(b, Rsi, 0);
    number(b, Rdx, 0);
    number(b, R10, 0);
    number(b, Rax, 61);
    syscall(b);
    compare(b, Rax, -4);
    jump(b, "je", reap);
    let native_error = b.label();
    compare(b, R15, 0);
    jump(b, "jl", native_error);
    dword_load(b, Rax, Rsp, 8);
    store(b, R12, E, Rax);
    store(b, R13, 72, Rax);
    jump(b, "jmp", done);
    b.define(native_error);
    rr(b, Rax, R15);
    jump(b, "jmp", error);
    b.define(executed);
    store(b, R13, 56, R14);
    store(b, R13, 64, R14);
    store(b, R12, V, R14);
    jump(b, "jmp", done);
    b.define(fork_failed);
    rr(b, R15, Rax);
    for offset in [0, 4] {
        dword_load(b, Rdi, Rsp, offset);
        number(b, Rax, 3);
        syscall(b);
    }
    rr(b, Rax, R15);
    jump(b, "jmp", error);
    b.define(child);
    child_exec(b);
    b.define(error);
    linux_result(b);
    b.define(done);
    adjust(b, "add", Rsp, 32);
}
fn child_exec(b: &mut Builder) {
    let failed = b.label();
    dword_load(b, Rdi, Rsp, 0);
    number(b, Rax, 3);
    syscall(b);
    load(b, Rdi, R13, 24);
    number(b, Rax, 80);
    syscall(b);
    compare(b, Rax, 0);
    jump(b, "jl", failed);
    for (index, offset) in [(0, 32), (1, 40), (2, 48)] {
        load(b, Rdi, R13, offset);
        number(b, Rsi, index);
        number(b, Rax, 33);
        syscall(b);
        compare(b, Rax, 0);
        jump(b, "jl", failed);
    }
    for offset in [32, 40, 48] {
        load(b, Rdi, R13, offset);
        number(b, Rax, 3);
        syscall(b);
    }
    load(b, Rdi, R13, 0);
    load(b, Rsi, R13, 8);
    load(b, Rdx, R13, 16);
    number(b, Rax, 59);
    syscall(b);
    b.define(failed);
    b.emit("neg", &[K::Rm64], Access::ReadWrite, vec![reg(gpr(Rax))]);
    store(b, Rsp, 8, Rax);
    let retry = b.label();
    b.define(retry);
    dword_load(b, Rdi, Rsp, 4);
    address(b, Rsi, Rsp, 8);
    number(b, Rdx, 4);
    number(b, Rax, 1);
    syscall(b);
    compare(b, Rax, -4);
    jump(b, "je", retry);
    number(b, Rdi, 127);
    number(b, Rax, 60);
    syscall(b);
    trap(b);
}
