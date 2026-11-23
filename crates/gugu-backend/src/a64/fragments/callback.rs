//! 回调控制区与无分配、无 safepoint 的跨线程队列；锁内完成所有权转换。
//! `ldaxr`/`stlxr` 取锁，`stlr` 释放；锁忙立即返回 BUSY，不持锁切入 managed runtime。

use super::{Builder, Fragment, add_imm, condition, mem, movz, next, reg, unshifted};
use crate::a64::{
    inst::AddressMode,
    reg::{Reg, Width},
};
use crate::runtime::{callback as q, entry::state, foreign_schema, processor};

const XZR: Reg = Reg::zr(Width::X);
const EQ: u8 = 0;
const NE: u8 = 1;
const GE: u8 = 10;
const AL: u8 = 14;
// 内部 ABI (control: x0, node: x1, action: x2) -> status/node: x0。
const CTL: Reg = Reg::x(10);
const NODE: Reg = Reg::x(11);
const ACTION: Reg = Reg::x(12);
const A: Reg = Reg::x(9);
const B: Reg = Reg::x(13);
const C: Reg = Reg::x(14);
const D: Reg = Reg::x(15);

/// 待回填的分支桩：`at` 为指令偏移，`code` 为条件码（无条件用 AL）。
#[derive(Clone, Copy)]
struct Exit {
    at: u32,
    code: u8,
}

fn jump(b: &mut Builder) -> Exit {
    let at = b.offset();
    b.emit("b", [next()]);
    Exit { at, code: AL }
}

fn when(b: &mut Builder, code: u8) -> Exit {
    let at = b.offset();
    b.emit("b.cond", [next(), condition(code)]);
    Exit { at, code }
}

fn land(b: &mut Builder, exit: Exit) {
    if exit.code == AL {
        b.patch_branch(exit.at, "b", &[], &[]);
    } else {
        b.patch_branch(exit.at, "b.cond", &[], &[condition(exit.code)]);
    }
}

fn land_all(b: &mut Builder, exits: Vec<Exit>) {
    for exit in exits {
        land(b, exit);
    }
}

fn load(b: &mut Builder, dst: Reg, base: Reg, offset: i32) {
    b.memory("ldr.X.Unsigned", dst, base, offset as usize);
}

fn store(b: &mut Builder, base: Reg, offset: i32, src: Reg) {
    b.memory("str.X.Unsigned", src, base, offset as usize);
}

fn set(b: &mut Builder, base: Reg, offset: i32, value: i32) {
    b.emit("movz.X", [reg(D), movz(value as u16)]);
    store(b, base, offset, D);
}

fn compare_regs(b: &mut Builder, left: Reg, right: Reg) {
    b.emit(
        "subs.X.shift",
        [reg(XZR), reg(left), reg(right), unshifted()],
    );
}

fn compare(b: &mut Builder, left: Reg, value: i32) {
    b.emit(
        "subs.X.imm",
        [reg(XZR), reg(left), add_imm(value as usize)],
    );
}

/// `__gugu_callback_control` 返回运行时保留区中的回调控制块；不依赖 managed 寄存器。
pub fn callback_control() -> Fragment {
    let mut b = Builder::default();
    let target = super::inst_target(
        crate::a64::program::RUNTIME_STATE,
        i64::try_from(state::CALLBACK).expect("CALLBACK 偏移适配 i64"),
    );
    b.emit("adrp", [reg(Reg::x(0)), target.clone()]);
    b.emit("add.X.imm", [reg(Reg::x(0)), reg(Reg::x(0)), target]);
    b.ret();
    b.finish()
}

/// (control: x0, node: x1, action: x2) -> status/node: x0。
/// 动作：OPEN/SUBMIT/TAKE/FINISH/DETACH/CLOSE；TAKE 成功返回节点地址。
pub fn callback_queue() -> Fragment {
    let mut b = Builder::default();
    b.add(CTL, Reg::x(0), 0);
    b.add(NODE, Reg::x(1), 0);
    b.add(ACTION, Reg::x(2), 0);
    let retry = b.offset();
    b.emit("ldaxr.X", [reg(A), mem(CTL, AddressMode::Base)]);
    let busy = b.offset();
    b.emit("cbnz.X", [reg(A), next()]);
    b.emit("movz.X", [reg(B), movz(1)]);
    b.emit("stlxr.X", [reg(Reg::w(14)), reg(B), mem(CTL, AddressMode::Base)]);
    b.branch("cbnz.X", &[reg(C)], retry);
    let mut cases = Vec::new();
    for action in 0..6i32 {
        compare(&mut b, ACTION, action);
        cases.push(when(&mut b, EQ));
    }
    let unknown = jump(&mut b);
    let mut done = Vec::new();
    let mut invalid: Vec<Exit> = vec![unknown];
    // OPEN：只允许一个 processor 登记。
    land(&mut b, cases[q::OPEN as usize]);
    load(&mut b, A, CTL, q::REGISTERED);
    compare(&mut b, A, 0);
    invalid.push(when(&mut b, NE));
    store(&mut b, CTL, q::PROCESSOR, NODE);
    set(&mut b, CTL, q::ACCEPTING, 1);
    done.push(jump(&mut b));
    // SUBMIT。
    land(&mut b, cases[q::SUBMIT as usize]);
    submit(&mut b, &mut invalid, &mut done);
    // TAKE。
    land(&mut b, cases[q::TAKE as usize]);
    take(&mut b, &mut invalid, &mut done);
    // FINISH。
    land(&mut b, cases[q::FINISH as usize]);
    node_state(&mut b, q::RUNNING, &mut invalid);
    unlink_active(&mut b, &mut invalid);
    set(&mut b, NODE, q::STATE, q::COMPLETE);
    b.emit("movz.X", [reg(Reg::x(0)), movz(q::OK as u16)]);
    done.push(jump(&mut b));
    // DETACH。
    land(&mut b, cases[q::DETACH as usize]);
    node_state(&mut b, q::COMPLETE, &mut invalid);
    detach(&mut b, &mut invalid);
    load(&mut b, A, CTL, q::REGISTERED);
    compare(&mut b, A, 0);
    invalid.push(when(&mut b, EQ));
    b.emit("sub.X.imm", [reg(A), reg(A), add_imm(1)]);
    store(&mut b, CTL, q::REGISTERED, A);
    set(&mut b, NODE, q::STATE, q::IDLE);
    b.emit("movz.X", [reg(Reg::x(0)), movz(q::OK as u16)]);
    done.push(jump(&mut b));
    // CLOSE。
    land(&mut b, cases[q::CLOSE as usize]);
    set(&mut b, CTL, q::ACCEPTING, 0);
    b.emit("movz.X", [reg(Reg::x(0)), movz(q::OK as u16)]);
    done.push(jump(&mut b));
    // 全部非法路径汇合为 INVALID。
    land_all(&mut b, invalid);
    b.emit("movz.X", [reg(Reg::x(0)), movz(q::INVALID as u16)]);
    // done：stlr 解锁后返回。
    land_all(&mut b, done);
    b.emit("stlr.X", [reg(XZR), mem(CTL, AddressMode::Base)]);
    b.ret();
    // 锁忙 → BUSY。
    b.patch_branch(busy, "cbnz.X", &[reg(A)], &[]);
    b.emit("movz.X", [reg(Reg::x(0)), movz(q::BUSY as u16)]);
    b.ret();
    b.finish()
}

fn node_state(b: &mut Builder, expected: i32, invalid: &mut Vec<Exit>) {
    load(b, A, NODE, q::STATE);
    compare(b, A, expected);
    invalid.push(when(b, NE));
    if expected != q::IDLE {
        load(b, A, NODE, q::OWNER);
        compare_regs(b, A, CTL);
        invalid.push(when(b, NE));
    }
}

fn unlink_active(b: &mut Builder, invalid: &mut Vec<Exit>) {
    b.add(D, CTL, q::ACTIVE as usize);
    let search = b.offset();
    load(b, A, D, 0);
    compare(b, A, 0);
    invalid.push(when(b, EQ));
    compare_regs(b, A, NODE);
    let found = when(b, EQ);
    b.add(D, A, 0);
    b.branch("b", &[], search);
    land(b, found);
    load(b, A, NODE, q::NEXT);
    store(b, D, 0, A);
    set(b, NODE, q::NEXT, 0);
}

fn submit(b: &mut Builder, invalid: &mut Vec<Exit>, done: &mut Vec<Exit>) {
    load(b, A, CTL, q::ACCEPTING);
    compare(b, A, 1);
    let open = when(b, EQ);
    b.emit("movz.X", [reg(Reg::x(0)), movz(q::CLOSED as u16)]);
    done.push(jump(b));
    land(b, open);
    node_state(b, q::IDLE, invalid);
    load(b, A, CTL, q::REGISTERED);
    compare(b, A, q::MAX_REGISTERED);
    invalid.push(when(b, GE));
    register(b, invalid);
    store(b, NODE, q::OWNER, CTL);
    set(b, NODE, q::NEXT, 0);
    set(b, NODE, q::STATE, q::QUEUED);
    load(b, A, CTL, q::TAIL);
    compare(b, A, 0);
    let empty = when(b, EQ);
    store(b, A, q::NEXT, NODE);
    let linked = jump(b);
    land(b, empty);
    store(b, CTL, q::HEAD, NODE);
    land(b, linked);
    store(b, CTL, q::TAIL, NODE);
    load(b, A, CTL, q::REGISTERED);
    b.emit("add.X.imm", [reg(A), reg(A), add_imm(1)]);
    store(b, CTL, q::REGISTERED, A);
    notify_processor(b);
    b.emit("movz.X", [reg(Reg::x(0)), movz(q::OK as u16)]);
    done.push(jump(b));
}

/// `processor` 的 poll 标志位置位 POLL_BIT：无原子 RMW 指令就用 ldaxr/stlxr 循环。
fn notify_processor(b: &mut Builder) {
    load(b, A, CTL, q::PROCESSOR);
    b.add(A, A, processor::poll_flags_offset() as usize);
    let retry = b.offset();
    b.emit("ldaxr.W", [reg(Reg::w(9)), mem(A, AddressMode::Base)]);
    b.emit(
        "orr.W.imm",
        [
            reg(Reg::w(13)),
            reg(Reg::w(9)),
            super::Operand::Bits(u64::from(q::POLL_BIT)),
        ],
    );
    b.emit(
        "stlxr.W",
        [reg(Reg::w(14)), reg(Reg::w(13)), mem(A, AddressMode::Base)],
    );
    b.branch("cbnz.W", &[reg(Reg::w(14))], retry);
}

fn take(b: &mut Builder, invalid: &mut Vec<Exit>, done: &mut Vec<Exit>) {
    load(b, NODE, CTL, q::HEAD);
    b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
    compare(b, NODE, 0);
    done.push(when(b, EQ));
    node_state(b, q::QUEUED, invalid);
    load(b, A, NODE, q::NEXT);
    store(b, CTL, q::HEAD, A);
    compare(b, A, 0);
    let linked = when(b, NE);
    set(b, CTL, q::TAIL, 0);
    land(b, linked);
    load(b, A, CTL, q::ACTIVE);
    store(b, NODE, q::NEXT, A);
    store(b, CTL, q::ACTIVE, NODE);
    set(b, NODE, q::STATE, q::RUNNING);
    b.add(Reg::x(0), NODE, 0);
    done.push(jump(b));
}

/// 回调登记按 OS 线程身份去重；登记与撤销都在队列锁内发布。
fn register(b: &mut Builder, invalid: &mut Vec<Exit>) {
    load(b, D, NODE, q::THREAD_ID);
    compare(b, D, 0);
    invalid.push(when(b, EQ));
    load(b, B, CTL, q::MANAGED_THREAD);
    compare_regs(b, D, B);
    invalid.push(when(b, EQ));
    let mut existing = Vec::new();
    let mut worker = Vec::new();
    // 池上界由 foreign_schema 固定；只匹配已发布的线程身份。
    load(b, B, CTL, q::PROCESSOR);
    load(b, B, B, 4024);
    compare(b, B, 0);
    let registry = when(b, EQ);
    for i in 0..foreign_schema::MAX_DIRTY_WORKERS {
        match_worker(b, 2048 + 64 + i as i32 * 128, invalid);
    }
    for i in 0..foreign_schema::MAX_BLOCKING_WORKERS {
        match_worker(b, 64 + i as i32 * 128, &mut worker);
    }
    land(b, registry);
    load(b, B, CTL, q::REGISTRY);
    let scan = b.offset();
    compare(b, B, 0);
    let new_thread = when(b, EQ);
    load(b, A, B, q::THREAD_ID);
    compare_regs(b, D, A);
    existing.push(when(b, EQ));
    load(b, B, B, q::REGISTRY_NEXT);
    b.branch("b", &[], scan);
    land(b, new_thread);
    set(b, NODE, q::BORROW_CREDIT, 0);
    set(b, NODE, q::EXTERNAL_LEASE, 1);
    load(b, A, CTL, q::EXTERNAL_THREADS);
    b.emit("add.X.imm", [reg(A), reg(A), add_imm(1)]);
    store(b, CTL, q::EXTERNAL_THREADS, A);
    let publish_new = jump(b);
    // existing：已登记线程复用其借用额度。
    land_all(b, existing);
    load(b, A, B, q::BORROW_CREDIT);
    store(b, NODE, q::BORROW_CREDIT, A);
    set(b, NODE, q::EXTERNAL_LEASE, 0);
    let publish_existing = jump(b);
    // worker：阻塞池内线程只领借用额度，不进 registry。
    land_all(b, worker);
    set(b, NODE, q::BORROW_CREDIT, 1);
    set(b, NODE, q::EXTERNAL_LEASE, 0);
    land(b, publish_new);
    land(b, publish_existing);
    load(b, A, CTL, q::REGISTRY);
    store(b, NODE, q::REGISTRY_NEXT, A);
    store(b, CTL, q::REGISTRY, NODE);
}

/// 匹配 worker 块（`B` = 池基址，`offset` = 槽位）：owner 非零且线程 id 相同则计入 `hit`。
fn match_worker(b: &mut Builder, offset: i32, hit: &mut Vec<Exit>) {
    load(
        b,
        A,
        B,
        offset + crate::runtime::foreign_schema::NATIVE_REQUEST_OWNER as i32,
    );
    compare(b, A, 0);
    let next_slot = when(b, EQ);
    load(b, A, B, offset);
    compare_regs(b, D, A);
    hit.push(when(b, EQ));
    land(b, next_slot);
}

fn detach(b: &mut Builder, invalid: &mut Vec<Exit>) {
    b.add(D, CTL, q::REGISTRY as usize);
    let scan = b.offset();
    load(b, A, D, 0);
    compare(b, A, 0);
    invalid.push(when(b, EQ));
    compare_regs(b, A, NODE);
    let found = when(b, EQ);
    b.add(D, A, q::REGISTRY_NEXT as usize);
    b.branch("b", &[], scan);
    land(b, found);
    load(b, A, NODE, q::REGISTRY_NEXT);
    store(b, D, 0, A);
    set(b, NODE, q::REGISTRY_NEXT, 0);
    load(b, A, NODE, q::EXTERNAL_LEASE);
    compare(b, A, 0);
    let done = when(b, EQ);
    set(b, NODE, q::EXTERNAL_LEASE, 0);
    transfer_or_release(b);
    land(b, done);
}

fn transfer_or_release(b: &mut Builder) {
    load(b, D, NODE, q::THREAD_ID);
    load(b, B, CTL, q::REGISTRY);
    let scan = b.offset();
    compare(b, B, 0);
    let release = when(b, EQ);
    load(b, A, B, q::THREAD_ID);
    compare_regs(b, D, A);
    let found = when(b, EQ);
    load(b, B, B, q::REGISTRY_NEXT);
    b.branch("b", &[], scan);
    land(b, found);
    set(b, B, q::EXTERNAL_LEASE, 1);
    let done = jump(b);
    land(b, release);
    load(b, A, CTL, q::EXTERNAL_THREADS);
    b.emit("sub.X.imm", [reg(A), reg(A), add_imm(1)]);
    store(b, CTL, q::EXTERNAL_THREADS, A);
    land(b, done);
}
