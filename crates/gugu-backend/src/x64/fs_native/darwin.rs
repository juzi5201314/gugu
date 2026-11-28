use super::*;
use crate::runtime::PlatformOp;

pub(crate) const C_PREFIX: &str = "__gugu_darwin_c_";
const C_ARGUMENTS: [Gpr; 6] = [Rdi, Rsi, Rdx, Rcx, R8, R9];
const DONE: i32 = THREAD_HANDLE;

fn c_symbol(name: &str) -> Symbol {
    Symbol::External {
        key: hash_domain("gugu-darwin-c-symbol", name.as_bytes()),
        name: format!("{C_PREFIX}{name}"),
    }
}

pub(super) fn c_call(b: &mut Builder, name: &str) {
    call_symbol(b, c_symbol(name));
}

pub(super) fn c_address(b: &mut Builder, r: Gpr, name: &str) {
    symbol_address(b, r, c_symbol(name));
}

fn call_symbol(b: &mut Builder, symbol: Symbol) {
    b.emit(
        "call",
        &[K::Rel32],
        Access::Read,
        vec![Operand::Reloc(RelocTarget::Lir(symbol), RelocKind::PcRel32)],
    );
}

fn symbol_address(b: &mut Builder, r: Gpr, symbol: Symbol) {
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![Operand::Rip(RelocTarget::Lir(symbol), 0), reg(gpr(r))],
    );
}

pub(super) fn errno(b: &mut Builder) {
    c_call(b, "__error");
    b.emit(
        "mov",
        &[K::Rm32, K::R32],
        Access::Read,
        vec![mem_base(gpr(Rax), 0), reg(gpr(Rax))],
    );
}

pub(super) fn signed_result(b: &mut Builder) {
    b.emit(
        "movsxd",
        &[K::Rm32, K::R64],
        Access::Read,
        vec![reg(gpr(Rax)), reg(gpr(Rax))],
    );
}

pub(super) fn finish_result(b: &mut Builder, value: i32, error: i32, failure: i32) {
    let failed = b.label();
    let end = b.label();
    compare(b, Rax, failure);
    jump(b, "je", failed);
    store(b, R12, value, Rax);
    jump(b, "jmp", end);
    b.define(failed);
    errno(b);
    store(b, R12, error, Rax);
    b.define(end);
}

pub(super) fn start() -> Builder {
    let mut b = Builder::new();
    adjust(&mut b, "sub", Rsp, 24);
    rr(&mut b, Rcx, Rax);
    rr(&mut b, Rdi, Rsp);
    number(&mut b, Rsi, 0);
    symbol_address(&mut b, Rdx, mangle::runtime_symbol("fs_native_worker"));
    c_call(&mut b, "pthread_create");
    dword_result(&mut b);
    let done = b.label();
    compare(&mut b, Rax, 0);
    jump(&mut b, "jne", done);
    load(&mut b, Rdi, Rsp, 0);
    c_call(&mut b, "pthread_detach");
    let detached = b.label();
    compare(&mut b, Rax, 0);
    jump(&mut b, "je", detached);
    b.emit("ud2", &[], Access::Read, vec![]);
    b.define(detached);
    b.define(done);
    adjust(&mut b, "add", Rsp, 24);
    ret(&mut b);
    b
}

pub(super) fn poll() -> Builder {
    let mut b = Builder::new();
    load(&mut b, Rax, Rax, DONE);
    ret(&mut b);
    b
}

// Packet operation numbers stay independent of the Darwin syscall-number space.
const FILES: &[(i32, &str, i32)] = &[
    (0, "read", -1),
    (1, "write", -1),
    (2, "open", -1),
    (3, "close", -1),
    (4, "lseek", -1),
    (5, "stat$INODE64", -1),
    (6, "lstat$INODE64", -1),
    (7, "fstat$INODE64", -1),
    (8, "ftruncate", -1),
    (9, "fsync", -1),
    (10, "fcntl", -1),
    (11, "opendir$INODE64", 0),
    (12, "readdir$INODE64", 0),
    (13, "closedir", -1),
    (14, "mkdir", -1),
    (15, "unlink", -1),
    (16, "rmdir", -1),
    (17, "rename", -1),
    (18, "link", -1),
    (19, "symlink", -1),
    (20, "readlink", -1),
    (21, "realpath", 0),
    (22, "getentropy", -1),
];

pub(super) fn worker(thread: bool) -> Builder {
    let mut b = Builder::new();
    push(&mut b, R12);
    rr(&mut b, R12, Rdi);
    load(&mut b, Rax, R12, OPERATION);
    let done = b.label();
    let file = b.label();
    compare(&mut b, Rax, -1);
    jump(&mut b, "jne", file);
    call_symbol(&mut b, mangle::runtime_symbol("foreign_native_worker"));
    jump(&mut b, "jmp", done);
    b.define(file);
    dispatch(&mut b, FILES, ARGUMENTS, VALUE, ERROR, done);
    b.define(done);
    if thread {
        set(&mut b, R12, DONE, 1);
    }
    number(&mut b, Rax, 0);
    pop(&mut b, R12);
    ret(&mut b);
    b
}

pub(super) fn dispatch(
    b: &mut Builder,
    methods: &[(i32, &str, i32)],
    args: i32,
    value: i32,
    error: i32,
    done: LabelId,
) {
    for &(op, name, failure) in methods {
        let next = b.label();
        compare(b, Rax, op);
        jump(b, "jne", next);
        if name == "readdir$INODE64" {
            c_call(b, "__error");
            b.emit(
                "mov",
                &[K::Rm32, K::Imm32],
                Access::Write,
                vec![mem_base(gpr(Rax), 0), imm(0)],
            );
        }
        for (i, r) in C_ARGUMENTS.into_iter().enumerate() {
            load(b, r, R12, args + i as i32 * 8);
        }
        number(b, Rax, 0); // variadic open/fcntl: no vector arguments
        c_call(b, name);
        if failure == -1
            && !matches!(
                name,
                "read" | "write" | "lseek" | "readlink" | "recvfrom" | "recvmsg" | "sendto"
            )
        {
            signed_result(b);
        }
        finish_result(b, value, error, failure);
        jump(b, "jmp", done);
        b.define(next);
    }
    set(b, R12, error, 22);
}

pub(super) fn thread_id() -> Builder {
    let mut b = Builder::new();
    adjust(&mut b, "sub", Rsp, 24);
    number(&mut b, Rdi, 0);
    rr(&mut b, Rsi, Rsp);
    c_call(&mut b, "pthread_threadid_np");
    load(&mut b, Rax, Rsp, 0);
    adjust(&mut b, "add", Rsp, 24);
    ret(&mut b);
    b
}

pub(super) fn parallelism() -> Builder {
    let mut b = Builder::new();
    adjust(&mut b, "sub", Rsp, 8);
    number(&mut b, Rdi, 58); // _SC_NPROCESSORS_ONLN
    c_call(&mut b, "sysconf");
    let valid = b.label();
    compare(&mut b, Rax, 1);
    jump(&mut b, "jge", valid);
    number(&mut b, Rax, 1);
    b.define(valid);
    adjust(&mut b, "add", Rsp, 8);
    ret(&mut b);
    b
}

pub(super) fn clock(id: i32) -> Builder {
    let mut b = Builder::new();
    number(&mut b, Rdi, id);
    adjust(&mut b, "sub", Rsp, 8);
    c_call(&mut b, "clock_gettime_nsec_np");
    adjust(&mut b, "add", Rsp, 8);
    ret(&mut b);
    b
}

pub(crate) fn exit(b: &mut Builder) {
    rr(b, Rdi, Rax);
    adjust(b, "sub", Rsp, 8);
    c_call(b, "exit");
    b.emit("ud2", &[], Access::Read, vec![]);
}

pub(crate) fn report(b: &mut Builder) {
    rr(b, Rdi, Rax);
    rr(b, Rsi, Rbx);
    rr(b, Rdx, Rcx);
    adjust(b, "sub", Rsp, 8);
    c_call(b, "write");
    adjust(b, "add", Rsp, 8);
    ret(b);
}

pub(crate) fn range(b: &mut Builder, op: PlatformOp) {
    if op == PlatformOp::ReserveAligned {
        return reserve(b);
    }
    load(b, Rdi, Rax, 0);
    load(b, Rsi, Rax, 8);
    adjust(b, "sub", Rsp, 8);
    match op {
        PlatformOp::Commit => {
            number(b, Rdx, 3);
            c_call(b, "mprotect");
            signed_result(b);
        }
        PlatformOp::Decommit => {
            number(b, Rdx, 0);
            number(b, Rcx, 0x1012); // MAP_ANON | MAP_PRIVATE | MAP_FIXED
            number(b, R8, -1);
            number(b, R9, 0);
            c_call(b, "mmap");
            let fail = b.label();
            compare(b, Rax, -1);
            jump(b, "je", fail);
            number(b, Rax, 0);
            b.define(fail);
        }
        PlatformOp::Release => {
            c_call(b, "munmap");
            signed_result(b);
        }
        _ => unreachable!(),
    }
    adjust(b, "add", Rsp, 8);
    ret(b);
}

fn reserve(b: &mut Builder) {
    for r in [R12, R13, Rbp] {
        push(b, r);
    }
    adjust(b, "sub", Rsp, 16);
    let failed = b.label();
    let release = b.label();
    let done = b.label();
    rr(b, R12, Rax);
    adjust(b, "add", R12, 4095);
    jump(b, "jb", failed);
    adjust(b, "and", R12, -4096);
    rr(b, R13, Rbx);
    number(b, Rdi, 0);
    rr(b, Rsi, R12);
    binary(b, "add", Rsi, R13);
    jump(b, "jb", failed);
    store(b, Rsp, 8, Rsi);
    number(b, Rdx, 0);
    number(b, Rcx, 0x1002); // MAP_ANON | MAP_PRIVATE
    number(b, R8, -1);
    number(b, R9, 0);
    c_call(b, "mmap");
    compare(b, Rax, -1);
    jump(b, "je", failed);
    store(b, Rsp, 0, Rax);
    rr(b, Rdi, Rax);
    rr(b, Rbp, Rax);
    binary(b, "add", Rbp, R13);
    adjust(b, "sub", Rbp, 1);
    rr(b, Rsi, R13);
    b.emit("neg", &[K::Rm64], Access::ReadWrite, vec![reg(gpr(Rsi))]);
    binary(b, "and", Rbp, Rsi);
    rr(b, Rsi, Rbp);
    binary(b, "sub", Rsi, Rdi);
    binary(b, "sub", R13, Rsi);
    let trim_tail = b.label();
    compare(b, Rsi, 0);
    jump(b, "je", trim_tail);
    c_call(b, "munmap");
    compare(b, Rax, 0);
    jump(b, "jne", release);
    b.define(trim_tail);
    rr(b, Rdi, Rbp);
    binary(b, "add", Rdi, R12);
    rr(b, Rsi, R13);
    c_call(b, "munmap");
    compare(b, Rax, 0);
    jump(b, "jne", release);
    rr(b, Rax, Rbp);
    jump(b, "jmp", done);
    b.define(release);
    load(b, Rdi, Rsp, 0);
    load(b, Rsi, Rsp, 8);
    c_call(b, "munmap");
    b.define(failed);
    number(b, Rax, 0);
    b.define(done);
    adjust(b, "add", Rsp, 16);
    for r in [Rbp, R13, R12] {
        pop(b, r);
    }
    ret(b);
}

fn binary(b: &mut Builder, op: &'static str, dst: Gpr, src: Gpr) {
    b.emit(
        op,
        MOV,
        Access::ReadWrite,
        vec![reg(gpr(dst)), reg(gpr(src))],
    );
}
