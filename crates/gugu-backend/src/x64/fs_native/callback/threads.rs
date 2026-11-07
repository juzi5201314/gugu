//! 回调登记按 OS 线程身份去重；在队列锁内发布和撤销。
use super::*;

pub(super) fn register(b: &mut Builder, invalid: LabelId) {
    load(b, R8, Rbx, q::THREAD_ID);
    compare(b, R8, 0);
    jump(b, "je", invalid);
    load(b, Rdx, R10, q::MANAGED_THREAD);
    b.emit("cmp", MOV, Access::Read, vec![reg(gpr(R8)), reg(gpr(Rdx))]);
    jump(b, "je", invalid);
    let existing = b.label();
    let worker = b.label();
    let publish = b.label();
    load(b, Rdx, R10, q::PROCESSOR);
    load(b, Rdx, Rdx, 4024);
    let registry = b.label();
    compare(b, Rdx, 0);
    jump(b, "je", registry);
    // 池上界由 foreign_schema 固定；只匹配已发布的线程身份。
    for i in 0..crate::runtime::foreign_schema::MAX_DIRTY_WORKERS {
        match_worker(b, 2048 + 64 + i as i32 * 128, invalid);
    }
    for i in 0..crate::runtime::foreign_schema::MAX_BLOCKING_WORKERS {
        match_worker(b, 64 + i as i32 * 128, worker);
    }
    b.define(registry);
    load(b, Rdx, R10, q::REGISTRY);
    let scan = b.label();
    let new_thread = b.label();
    b.define(scan);
    compare(b, Rdx, 0);
    jump(b, "je", new_thread);
    load(b, Rax, Rdx, q::THREAD_ID);
    b.emit("cmp", MOV, Access::Read, vec![reg(gpr(R8)), reg(gpr(Rax))]);
    jump(b, "je", existing);
    load(b, Rdx, Rdx, q::REGISTRY_NEXT);
    jump(b, "jmp", scan);
    b.define(new_thread);
    set(b, Rbx, q::BORROW_CREDIT, 0);
    set(b, Rbx, q::EXTERNAL_LEASE, 1);
    load(b, Rax, R10, q::EXTERNAL_THREADS);
    adjust(b, "add", Rax, 1);
    store(b, R10, q::EXTERNAL_THREADS, Rax);
    jump(b, "jmp", publish);
    b.define(existing);
    load(b, Rax, Rdx, q::BORROW_CREDIT);
    store(b, Rbx, q::BORROW_CREDIT, Rax);
    set(b, Rbx, q::EXTERNAL_LEASE, 0);
    jump(b, "jmp", publish);
    b.define(worker);
    set(b, Rbx, q::BORROW_CREDIT, 1);
    set(b, Rbx, q::EXTERNAL_LEASE, 0);
    b.define(publish);
    load(b, Rax, R10, q::REGISTRY);
    store(b, Rbx, q::REGISTRY_NEXT, Rax);
    store(b, R10, q::REGISTRY, Rbx);
}

fn match_worker(b: &mut Builder, offset: i32, matched: LabelId) {
    let next = b.label();
    load(
        b,
        Rax,
        Rdx,
        offset + crate::runtime::foreign_schema::NATIVE_REQUEST_OWNER as i32,
    );
    compare(b, Rax, 0);
    jump(b, "je", next);
    load(b, Rax, Rdx, offset);
    b.emit("cmp", MOV, Access::Read, vec![reg(gpr(R8)), reg(gpr(Rax))]);
    jump(b, "je", matched);
    b.define(next);
}

pub(super) fn detach(b: &mut Builder, invalid: LabelId) {
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![mem_base(gpr(R10), q::REGISTRY), reg(gpr(Rdx))],
    );
    let scan = b.label();
    let found = b.label();
    b.define(scan);
    load(b, Rax, Rdx, 0);
    compare(b, Rax, 0);
    jump(b, "je", invalid);
    b.emit("cmp", MOV, Access::Read, vec![reg(gpr(Rax)), reg(gpr(Rbx))]);
    jump(b, "je", found);
    b.emit(
        "lea",
        &[K::Mem, K::R64],
        Access::Address,
        vec![mem_base(gpr(Rax), q::REGISTRY_NEXT), reg(gpr(Rdx))],
    );
    jump(b, "jmp", scan);
    b.define(found);
    load(b, Rax, Rbx, q::REGISTRY_NEXT);
    store(b, Rdx, 0, Rax);
    set(b, Rbx, q::REGISTRY_NEXT, 0);
    load(b, Rax, Rbx, q::EXTERNAL_LEASE);
    let done = b.label();
    compare(b, Rax, 0);
    jump(b, "je", done);
    set(b, Rbx, q::EXTERNAL_LEASE, 0);
    transfer_or_release(b);
    b.define(done);
}

fn transfer_or_release(b: &mut Builder) {
    load(b, R8, Rbx, q::THREAD_ID);
    load(b, Rdx, R10, q::REGISTRY);
    let scan = b.label();
    let release = b.label();
    let found = b.label();
    let done = b.label();
    b.define(scan);
    compare(b, Rdx, 0);
    jump(b, "je", release);
    load(b, Rax, Rdx, q::THREAD_ID);
    b.emit("cmp", MOV, Access::Read, vec![reg(gpr(R8)), reg(gpr(Rax))]);
    jump(b, "je", found);
    load(b, Rdx, Rdx, q::REGISTRY_NEXT);
    jump(b, "jmp", scan);
    b.define(found);
    set(b, Rdx, q::EXTERNAL_LEASE, 1);
    jump(b, "jmp", done);
    b.define(release);
    load(b, Rax, R10, q::EXTERNAL_THREADS);
    adjust(b, "sub", Rax, 1);
    store(b, R10, q::EXTERNAL_THREADS, Rax);
    b.define(done);
}
