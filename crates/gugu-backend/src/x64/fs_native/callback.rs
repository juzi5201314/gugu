//! 无分配、无 safepoint 的跨线程队列；xchg 取锁，普通 mov 在 x64 上 release 解锁。
use super::*;
use crate::runtime::callback as q;
mod threads;

/// 返回 writer 放置的可写控制区；不依赖外部线程的 managed 寄存器。
pub(super) fn control() -> Builder {
    let mut b = Builder::new();
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![
            Operand::Rip(
                RelocTarget::Lir(Symbol::External {
                    key: hash_domain("gugu-callback-control-v1", b"callback-control"),
                    name: "__gugu_callback_control".into(),
                }),
                0,
            ),
            reg(gpr(Rax)),
        ],
    );
    ret(&mut b);
    b
}

/// 内部 ABI `(control: rax, node: rbx, action: rcx) -> status/node: rax`。
/// 所有权转换都在同一锁内；锁忙立即返回，不能持锁切入 managed runtime。
pub(super) fn queue() -> Builder {
    let mut b = Builder::new();
    rr(&mut b, R10, Rax);
    number(&mut b, Rax, 1);
    b.emit(
        "xchg",
        MOV,
        Access::ReadWrite,
        vec![mem_base(gpr(R10), q::LOCK), reg(gpr(Rax))],
    );
    let busy = b.label();
    compare(&mut b, Rax, 0);
    jump(&mut b, "jne", busy);
    let invalid = b.label();
    let done = b.label();
    let cases: Vec<_> = (0..6).map(|_| b.label()).collect();
    for (action, &label) in cases.iter().enumerate() {
        compare(&mut b, Rcx, action as i32);
        jump(&mut b, "je", label);
    }
    jump(&mut b, "jmp", invalid);
    b.define(cases[q::OPEN as usize]);
    load(&mut b, Rax, R10, q::REGISTERED);
    compare(&mut b, Rax, 0);
    jump(&mut b, "jne", invalid);
    store(&mut b, R10, q::PROCESSOR, Rbx);
    set(&mut b, R10, q::ACCEPTING, 1);
    jump(&mut b, "jmp", done);
    b.define(cases[q::SUBMIT as usize]);
    submit(&mut b, invalid, done);
    b.define(cases[q::TAKE as usize]);
    take(&mut b, invalid, done);
    b.define(cases[q::FINISH as usize]);
    node_state(&mut b, q::RUNNING, invalid);
    unlink_active(&mut b, invalid);
    set(&mut b, Rbx, q::STATE, q::COMPLETE);
    number(&mut b, Rax, q::OK);
    jump(&mut b, "jmp", done);
    b.define(cases[q::DETACH as usize]);
    node_state(&mut b, q::COMPLETE, invalid);
    threads::detach(&mut b, invalid);
    load(&mut b, Rax, R10, q::REGISTERED);
    compare(&mut b, Rax, 0);
    jump(&mut b, "je", invalid);
    adjust(&mut b, "sub", Rax, 1);
    store(&mut b, R10, q::REGISTERED, Rax);
    set(&mut b, Rbx, q::STATE, q::IDLE);
    number(&mut b, Rax, q::OK);
    jump(&mut b, "jmp", done);
    b.define(cases[q::CLOSE as usize]);
    set(&mut b, R10, q::ACCEPTING, 0);
    number(&mut b, Rax, q::OK);
    jump(&mut b, "jmp", done);
    b.define(invalid);
    number(&mut b, Rax, q::INVALID);
    b.define(done);
    set(&mut b, R10, q::LOCK, 0);
    ret(&mut b);
    b.define(busy);
    number(&mut b, Rax, q::BUSY);
    ret(&mut b);
    b
}

fn unlink_active(b: &mut Builder, invalid: LabelId) {
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![mem_base(gpr(R10), q::ACTIVE), reg(gpr(Rdx))],
    );
    let search = b.label();
    let found = b.label();
    b.define(search);
    load(b, Rax, Rdx, 0);
    compare(b, Rax, 0);
    jump(b, "je", invalid);
    b.emit("cmp", MOV, Access::Read, vec![reg(gpr(Rax)), reg(gpr(Rbx))]);
    jump(b, "je", found);
    rr(b, Rdx, Rax);
    jump(b, "jmp", search);
    b.define(found);
    load(b, Rax, Rbx, q::NEXT);
    store(b, Rdx, 0, Rax);
    set(b, Rbx, q::NEXT, 0);
}

fn node_state(b: &mut Builder, expected: i32, invalid: LabelId) {
    load(b, Rax, Rbx, q::STATE);
    compare(b, Rax, expected);
    jump(b, "jne", invalid);
    if expected != q::IDLE {
        load(b, Rax, Rbx, q::OWNER);
        b.emit("cmp", MOV, Access::Read, vec![reg(gpr(Rax)), reg(gpr(R10))]);
        jump(b, "jne", invalid);
    }
}

fn submit(b: &mut Builder, invalid: LabelId, done: LabelId) {
    load(b, Rax, R10, q::ACCEPTING);
    compare(b, Rax, 1);
    let open = b.label();
    jump(b, "je", open);
    number(b, Rax, q::CLOSED);
    jump(b, "jmp", done);
    b.define(open);
    node_state(b, q::IDLE, invalid);
    load(b, Rax, R10, q::REGISTERED);
    compare(b, Rax, q::MAX_REGISTERED);
    jump(b, "jge", invalid);
    threads::register(b, invalid);
    store(b, Rbx, q::OWNER, R10);
    set(b, Rbx, q::NEXT, 0);
    set(b, Rbx, q::STATE, q::QUEUED);
    load(b, Rax, R10, q::TAIL);
    let empty = b.label();
    let linked = b.label();
    compare(b, Rax, 0);
    jump(b, "je", empty);
    store(b, Rax, q::NEXT, Rbx);
    jump(b, "jmp", linked);
    b.define(empty);
    store(b, R10, q::HEAD, Rbx);
    b.define(linked);
    store(b, R10, q::TAIL, Rbx);
    load(b, Rax, R10, q::REGISTERED);
    adjust(b, "add", Rax, 1);
    store(b, R10, q::REGISTERED, Rax);
    load(b, Rax, R10, q::PROCESSOR);
    b.emit_locked(
        "or",
        &[K::Rm32, K::Imm8],
        Access::ReadWrite,
        vec![
            mem_base(
                gpr(Rax),
                crate::runtime::processor::poll_flags_offset() as i32,
            ),
            imm(u64::from(q::POLL_BIT)),
        ],
    );
    number(b, Rax, q::OK);
    jump(b, "jmp", done);
}

fn take(b: &mut Builder, invalid: LabelId, done: LabelId) {
    load(b, Rbx, R10, q::HEAD);
    number(b, Rax, 0);
    compare(b, Rbx, 0);
    jump(b, "je", done);
    node_state(b, q::QUEUED, invalid);
    load(b, Rax, Rbx, q::NEXT);
    store(b, R10, q::HEAD, Rax);
    let linked = b.label();
    compare(b, Rax, 0);
    jump(b, "jne", linked);
    set(b, R10, q::TAIL, 0);
    b.define(linked);
    load(b, Rax, R10, q::ACTIVE);
    store(b, Rbx, q::NEXT, Rax);
    store(b, R10, q::ACTIVE, Rbx);
    set(b, Rbx, q::STATE, q::RUNNING);
    rr(b, Rax, Rbx);
    jump(b, "jmp", done);
}
