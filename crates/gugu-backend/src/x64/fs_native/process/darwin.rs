use super::super::darwin::{c_call, dispatch, errno, finish_result, signed_result};
use super::*;

pub(super) fn call() -> Builder {
    let mut b = Builder::new();
    push(&mut b, R12);
    rr(&mut b, R12, Rbx);
    clear_result(&mut b);
    let done = b.label();
    let next = b.label();
    compare(&mut b, Rax, 0);
    jump(&mut b, "jne", next);
    observe(&mut b);
    jump(&mut b, "jmp", done);
    b.define(next);
    let other = b.label();
    compare(&mut b, Rax, 1);
    jump(&mut b, "jne", other);
    load(&mut b, Rdi, R12, A);
    number(&mut b, Rsi, 9);
    c_call(&mut b, "kill");
    signed_result(&mut b);
    finish_result(&mut b, V, E, -1);
    jump(&mut b, "jmp", done);
    b.define(other);
    dispatch(
        &mut b,
        &[(2, "close", -1), (3, "read", -1), (4, "write", -1)],
        A,
        V,
        E,
        done,
    );
    b.define(done);
    number(&mut b, Rax, 0);
    pop(&mut b, R12);
    ret(&mut b);
    b
}

fn observe(b: &mut Builder) {
    let done = b.label();
    let failed = b.label();
    let signaled = b.label();
    set(b, R12, A + 8, 0);
    load(b, Rdi, R12, A);
    address(b, Rsi, R12, A + 8);
    number(b, Rdx, 1); // WNOHANG
    c_call(b, "waitpid");
    signed_result(b);
    compare(b, Rax, 0);
    jump(b, "je", done);
    jump(b, "jl", failed);
    dword_load(b, Rax, R12, A + 8);
    rr(b, R10, Rax);
    adjust(b, "and", R10, 127);
    compare(b, R10, 0);
    jump(b, "jne", signaled);
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
    b.define(signaled);
    store(b, R12, A + 8, R10);
    set(b, R12, V, 2);
    jump(b, "jmp", done);
    b.define(failed);
    errno(b);
    store(b, R12, E, Rax);
    b.define(done);
}

pub(super) fn worker(b: &mut Builder) {
    for r in [R12, R13, R14] {
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
            3 => {
                load(b, Rdi, R12, A);
                number(b, Rsi, 67); // F_DUPFD_CLOEXEC
                number(b, Rdx, 3);
                number(b, Rax, 0);
                c_call(b, "fcntl");
                signed_result(b);
                finish_result(b, V, E, -1);
            }
            _ => unreachable!(),
        }
        jump(b, "jmp", done);
        b.define(next);
    }
    trap(b);
    b.define(done);
    number(b, Rax, 0);
    for r in [R14, R13, R12] {
        pop(b, r);
    }
    ret(b);
}

fn spawn(b: &mut Builder) {
    load(b, R13, R12, A);
    adjust(b, "sub", Rsp, 16);
    let done = b.label();
    let cleanup = b.label();
    let failed = b.label();
    rr(b, Rdi, Rsp);
    c_call(b, "posix_spawn_file_actions_init");
    compare(b, Rax, 0);
    jump(b, "jne", failed);
    for (fd, offset) in [(0, 32), (1, 40), (2, 48)] {
        let next = b.label();
        load(b, Rsi, R13, offset);
        compare(b, Rsi, -1);
        jump(b, "je", next);
        rr(b, Rdi, Rsp);
        number(b, Rdx, fd);
        c_call(b, "posix_spawn_file_actions_adddup2");
        compare(b, Rax, 0);
        jump(b, "jne", cleanup);
        b.define(next);
    }
    let no_cwd = b.label();
    load(b, Rsi, R13, 24);
    compare(b, Rsi, 0);
    jump(b, "je", no_cwd);
    rr(b, Rdi, Rsp);
    c_call(b, "posix_spawn_file_actions_addchdir_np");
    compare(b, Rax, 0);
    jump(b, "jne", cleanup);
    b.define(no_cwd);
    address(b, Rdi, Rsp, 8);
    load(b, Rsi, R13, 0);
    rr(b, Rdx, Rsp);
    number(b, Rcx, 0);
    load(b, R8, R13, 8);
    load(b, R9, R13, 16);
    c_call(b, "posix_spawn");
    compare(b, Rax, 0);
    jump(b, "jne", cleanup);
    dword_load(b, R14, Rsp, 8);
    store(b, R13, 56, R14);
    store(b, R13, 64, R14);
    store(b, R12, V, R14);
    b.define(cleanup);
    store(b, R12, E, Rax);
    rr(b, Rdi, Rsp);
    c_call(b, "posix_spawn_file_actions_destroy");
    jump(b, "jmp", done);
    b.define(failed);
    store(b, R12, E, Rax);
    b.define(done);
    adjust(b, "add", Rsp, 16);
}

fn pipe(b: &mut Builder) {
    adjust(b, "sub", Rsp, 16);
    rr(b, Rdi, Rsp);
    c_call(b, "pipe");
    signed_result(b);
    let failed = b.label();
    let cleanup = b.label();
    let pair = b.label();
    let done = b.label();
    compare(b, Rax, -1);
    jump(b, "je", failed);
    dword_load(b, R13, Rsp, 0);
    dword_load(b, R14, Rsp, 4);
    load(b, Rax, R12, A);
    compare(b, Rax, 0);
    jump(b, "jne", pair);
    rr(b, Rax, R13);
    rr(b, R13, R14);
    rr(b, R14, Rax);
    b.define(pair);
    for (fd, cmd, value) in [(R13, 2, 1), (R14, 2, 1), (R13, 4, 4), (R13, 73, 1)] {
        rr(b, Rdi, fd);
        number(b, Rsi, cmd);
        number(b, Rdx, value);
        number(b, Rax, 0);
        c_call(b, "fcntl");
        signed_result(b);
        compare(b, Rax, -1);
        jump(b, "je", cleanup);
    }
    store(b, R12, V, R13);
    store(b, R12, A + 16, R14);
    jump(b, "jmp", done);
    b.define(cleanup);
    errno(b);
    store(b, R12, E, Rax);
    for r in [R13, R14] {
        rr(b, Rdi, r);
        c_call(b, "close");
    }
    jump(b, "jmp", done);
    b.define(failed);
    errno(b);
    store(b, R12, E, Rax);
    b.define(done);
    adjust(b, "add", Rsp, 16);
}

fn null(b: &mut Builder) {
    adjust(b, "sub", Rsp, 16);
    b.emit(
        "mov",
        &[K::R64, K::Imm64],
        Access::Write,
        vec![reg(gpr(Rax)), imm(u64::from_le_bytes(*b"/dev/nul"))],
    );
    store(b, Rsp, 0, Rax);
    set(b, Rsp, 8, 108);
    rr(b, Rdi, Rsp);
    number(b, Rsi, 0x1000002); // O_CLOEXEC | O_RDWR
    number(b, Rax, 0);
    c_call(b, "open");
    signed_result(b);
    finish_result(b, V, E, -1);
    adjust(b, "add", Rsp, 16);
}
