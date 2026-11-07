//! 普通 C worker：pthread 提供独立 TLS，确认 join 后才允许 owner 复用请求。
use super::*;
use crate::runtime::foreign_packet as packet;
use crate::x64::reg::{Reg, Xmm};

fn c_call(b: &mut Builder, name: &str) {
    b.emit(
        "call",
        &[K::Rel32],
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(Symbol::External {
                key: hash_domain("gugu-native-c-symbol-v1", name.as_bytes()),
                name: name.into(),
            }),
            RelocKind::PcRel32,
        )],
    );
}

pub(super) fn linux_start(b: &mut Builder) {
    push(b, R12);
    push(b, R13);
    adjust(b, "sub", Rsp, 72); // x86_64 glibc pthread_attr_t 为 56 字节，留出对齐空间。
    rr(b, R12, Rax);
    rr(b, R13, Rbx);
    rr(b, Rdi, Rsp);
    c_call(b, "pthread_attr_init");
    dword_result(b);
    let done = b.label();
    let destroy = b.label();
    let fatal = b.label();
    compare(b, Rax, 0);
    jump(b, "jne", done);
    rr(b, Rdi, Rsp);
    rr(b, Rsi, R13);
    adjust(b, "sub", Rsi, bridge::BLOCKING_WORKER_STACK_BYTES as i32);
    number(b, Rdx, bridge::BLOCKING_WORKER_STACK_BYTES as i32);
    c_call(b, "pthread_attr_setstack");
    dword_result(b);
    compare(b, Rax, 0);
    jump(b, "jne", destroy);
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![mem_base(gpr(R12), THREAD_HANDLE), reg(gpr(Rdi))],
    );
    rr(b, Rsi, Rsp);
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![
            Operand::Rip(
                RelocTarget::Lir(mangle::runtime_symbol("foreign_native_worker")),
                0,
            ),
            reg(gpr(Rdx)),
        ],
    );
    rr(b, Rcx, R12);
    c_call(b, "pthread_create");
    dword_result(b);
    b.define(destroy);
    rr(b, R13, Rax);
    rr(b, Rdi, Rsp);
    c_call(b, "pthread_attr_destroy");
    dword_result(b);
    compare(b, Rax, 0);
    jump(b, "jne", fatal);
    rr(b, Rax, R13);
    b.define(done);
    store(b, R12, ERROR, Rax);
    adjust(b, "add", Rsp, 72);
    pop(b, R13);
    pop(b, R12);
    ret(b);
    b.define(fatal);
    number(b, Rax, 231);
    number(b, Rdi, 2);
    syscall(b);
    b.emit("ud2", &[], Access::Read, vec![]);
}

pub(super) fn linux_poll(b: &mut Builder) {
    push(b, R12);
    rr(b, R12, Rax);
    load(b, Rdi, R12, THREAD_HANDLE);
    number(b, Rsi, 0);
    c_call(b, "pthread_tryjoin_np");
    dword_result(b);
    let pending = b.label();
    let failed = b.label();
    let end = b.label();
    compare(b, Rax, 16); // EBUSY：线程或 TLS 析构尚未结束，仍持有同一个 credit。
    jump(b, "je", pending);
    compare(b, Rax, 0);
    jump(b, "jne", failed);
    set(b, R12, THREAD_HANDLE, 0);
    number(b, Rax, 1);
    jump(b, "jmp", end);
    b.define(pending);
    number(b, Rax, 0);
    b.define(end);
    pop(b, R12);
    ret(b);
    b.define(failed);
    number(b, Rax, 231);
    number(b, Rdi, 2);
    syscall(b);
    b.emit("ud2", &[], Access::Read, vec![]);
}

fn xmm(b: &mut Builder, register: Xmm, base: Gpr, offset: i32, access: Access) {
    b.emit(
        "movsd",
        &[K::XmmRm, K::Xmm],
        access,
        vec![mem_base(gpr(base), offset), reg(Reg::Xmm(register))],
    );
}

/// native 栈先逐页触碰再复制参数，Windows guard page 不可一步跨过。
fn stack_arguments(b: &mut Builder) {
    load(b, R10, R14, packet::FOREIGN_PACKET_STACK_BYTES as i32);
    let pages = b.label();
    let tail = b.label();
    b.define(pages);
    compare(b, R10, 4096);
    jump(b, "jl", tail);
    adjust(b, "sub", Rsp, 4096);
    set(b, Rsp, 0, 0);
    adjust(b, "sub", R10, 4096);
    jump(b, "jmp", pages);
    b.define(tail);
    let copied_base = b.label();
    compare(b, R10, 0);
    jump(b, "je", copied_base);
    b.emit(
        "sub",
        MOV,
        Access::ReadWrite,
        vec![reg(gpr(Rsp)), reg(gpr(R10))],
    );
    set(b, Rsp, 0, 0);
    b.define(copied_base);
    rr(b, R10, R14);
    adjust(b, "add", R10, packet::FOREIGN_PACKET_BYTES as i32);
    rr(b, R11, Rsp);
    load(b, Rcx, R14, packet::FOREIGN_PACKET_STACK_BYTES as i32);
    let copy = b.label();
    let done = b.label();
    b.define(copy);
    compare(b, Rcx, 0);
    jump(b, "je", done);
    load(b, Rax, R10, 0);
    store(b, R11, 0, Rax);
    adjust(b, "add", R10, 8);
    adjust(b, "add", R11, 8);
    adjust(b, "sub", Rcx, 8);
    jump(b, "jmp", copy);
    b.define(done);
}

pub(super) fn worker(target: TargetName) -> Builder {
    let mut b = Builder::new();
    // 五个 nonvolatile 保存槽使 call 前 rsp 按 16 对齐。
    for r in [R12, R13, R14, R15, Rbp] {
        push(&mut b, r);
    }
    rr(
        &mut b,
        R12,
        if target == TargetName::X86_64Linux {
            Rdi
        } else {
            Rcx
        },
    );
    load(&mut b, R14, R12, ARGUMENTS);
    rr(&mut b, R13, Rsp);
    if target == TargetName::X86_64Linux {
        number(&mut b, Rax, 186);
        syscall(&mut b);
    } else {
        adjust(&mut b, "sub", Rsp, 32);
        win_call(&mut b, "GetCurrentThreadId");
        dword_result(&mut b);
        adjust(&mut b, "add", Rsp, 32);
    }
    store(&mut b, R12, 0, Rax);
    if target == TargetName::X86_64Linux {
        c_call(&mut b, "__errno_location");
        rr(&mut b, R15, Rax);
    }
    stack_arguments(&mut b);
    let registers: &[Gpr] = if target == TargetName::X86_64Linux {
        &[Rdi, Rsi, Rdx, Rcx, R8, R9]
    } else {
        &[Rcx, Rdx, R8, R9]
    };
    for (index, &r) in registers.iter().enumerate() {
        load(
            &mut b,
            r,
            R14,
            packet::FOREIGN_PACKET_INTEGERS as i32 + index as i32 * 8,
        );
    }
    let floats = [
        Xmm::Xmm0,
        Xmm::Xmm1,
        Xmm::Xmm2,
        Xmm::Xmm3,
        Xmm::Xmm4,
        Xmm::Xmm5,
        Xmm::Xmm6,
        Xmm::Xmm7,
    ];
    let count = if target == TargetName::X86_64Linux {
        8
    } else {
        4
    };
    for (index, &r) in floats[..count].iter().enumerate() {
        xmm(
            &mut b,
            r,
            R14,
            packet::FOREIGN_PACKET_FLOATS as i32 + index as i32 * 8,
            Access::Read,
        );
    }
    b.emit(
        "call",
        &[K::Mem],
        Access::Read,
        vec![mem_base(gpr(R14), packet::FOREIGN_PACKET_TARGET as i32)],
    );
    for (index, r) in [Rax, Rdx].into_iter().enumerate() {
        store(
            &mut b,
            R14,
            packet::FOREIGN_PACKET_RESULTS as i32 + index as i32 * 8,
            r,
        );
    }
    for (index, r) in [Xmm::Xmm0, Xmm::Xmm1].into_iter().enumerate() {
        xmm(
            &mut b,
            r,
            R14,
            packet::FOREIGN_PACKET_RESULTS as i32 + 16 + index as i32 * 8,
            Access::Write,
        );
    }
    if target == TargetName::X86_64Linux {
        b.emit(
            "mov",
            &[K::Rm32, K::R32],
            Access::Read,
            vec![mem_base(gpr(R15), 0), reg(gpr(Rax))],
        );
    } else {
        // 栈包包含 shadow space；先保存全部返回车道，再读取线程本地错误。
        win_call(&mut b, "GetLastError");
        dword_result(&mut b);
    }
    store(&mut b, R14, packet::FOREIGN_PACKET_ERROR as i32, Rax);
    set(&mut b, R12, ERROR, 0);
    set(&mut b, R12, 0, 0); // 先撤销线程身份，再允许 native 线程退出并重用 OS ID。
    rr(&mut b, Rsp, R13);
    number(&mut b, Rax, 0);
    for r in [Rbp, R15, R14, R13, R12] {
        pop(&mut b, r);
    }
    ret(&mut b);
    b
}
