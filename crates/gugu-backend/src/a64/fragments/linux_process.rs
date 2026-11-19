use std::mem::offset_of;

use super::linux_syscall::*;
use super::{Builder, Fragment, Operand, SP, Target, imm, reg, unshifted};
use crate::{a64::reg::Reg, runtime::process_schema::ProcessRequest};

#[path = "linux_process_spawn.rs"]
mod spawn;

const REQ: Reg = Reg::x(19);
const PACKET: Reg = Reg::x(20);
const CHILD: Reg = Reg::x(21);
const RESULT: Reg = Reg::x(22);
const PARENT: Reg = Reg::x(23);
const OTHER: Reg = Reg::x(24);
const A: usize = offset_of!(ProcessRequest, arguments);
const V: usize = offset_of!(ProcessRequest, value);
const E: usize = offset_of!(ProcessRequest, error);

/// x0 = operation, x1 = ProcessRequest; native stack, no managed calls.
pub fn linux_process_call() -> Fragment {
    let mut b = Builder::default();
    frame(&mut b, Reg::x(1));
    b.memory("ldr.X.Unsigned", Reg::x(9), REQ, 0);
    b.emit(
        "subs.X.shift",
        [reg(ZERO), reg(Reg::x(9)), reg(Reg::x(0)), unshifted()],
    );
    let valid = branch_cond(&mut b, 0);
    b.emit("brk", [imm(0)]);
    patch_cond(&mut b, valid, 0);
    let mut exits = Vec::new();
    for op in 0..7 {
        let next = skip(&mut b, Reg::x(9), op);
        match op {
            0 => observe(&mut b),
            4 => pipe_write(&mut b),
            5 | 6 => {
                n(&mut b, 0, 95);
                b.memory("str.X.Unsigned", Reg::x(0), REQ, E);
            }
            _ => {
                args(&mut b, if op == 3 { 3 } else { 1 });
                if op == 1 {
                    n(&mut b, 1, 9);
                }
                syscall(
                    &mut b,
                    match op {
                        1 => 129,
                        2 => 57,
                        3 => 63,
                        _ => unreachable!(),
                    },
                );
                result(&mut b);
            }
        }
        exits.push(branch(&mut b));
        patch_cond(&mut b, next, 1);
    }
    b.emit("brk", [imm(0)]);
    for at in exits {
        b.patch_branch(at, "b", &[], &[]);
    }
    finish(&mut b);
    b.finish()
}

/// First entry returns a C ABI worker pointer; worker receives x0 = ProcessRequest.
pub fn linux_process_worker() -> Fragment {
    let mut b = Builder::default();
    b.emit("adr", [reg(Reg::x(0)), Operand::Target(Target::Offset(8))]);
    b.ret();
    frame(&mut b, Reg::x(0));
    let mut exits = Vec::new();
    for (op, handler) in [spawn::spawn, pipe, open_null, duplicate]
        .into_iter()
        .enumerate()
    {
        b.memory("ldr.X.Unsigned", Reg::x(9), REQ, 0);
        let next = skip(&mut b, Reg::x(9), op);
        handler(&mut b);
        exits.push(branch(&mut b));
        patch_cond(&mut b, next, 1);
    }
    b.emit("brk", [imm(0)]);
    for at in exits {
        b.patch_branch(at, "b", &[], &[]);
    }
    finish(&mut b);
    b.finish()
}

fn frame(b: &mut Builder, request: Reg) {
    enter(b, 128);
    for (i, r) in [CHILD, RESULT, PARENT, OTHER].into_iter().enumerate() {
        b.memory("str.X.Unsigned", r, SP, 96 + i * 8);
    }
    b.add(REQ, request, 0);
    b.memory("str.X.Unsigned", ZERO, REQ, V);
    b.memory("str.X.Unsigned", ZERO, REQ, E);
}

fn finish(b: &mut Builder) {
    for (i, r) in [CHILD, RESULT, PARENT, OTHER].into_iter().enumerate() {
        b.memory("ldr.X.Unsigned", r, SP, 96 + i * 8);
    }
    n(b, 0, 0);
    leave(b, 128);
}

fn args(b: &mut Builder, count: u8) {
    for i in 0..count {
        b.memory("ldr.X.Unsigned", Reg::x(i), REQ, A + usize::from(i) * 8);
    }
}

fn n(b: &mut Builder, r: u8, value: u64) {
    for shift in [0, 16, 32, 48] {
        let imm16 = (value >> shift) as u16;
        if shift == 0 || imm16 != 0 {
            b.emit(
                if shift == 0 { "movz.X" } else { "movk.X" },
                [reg(Reg::x(r)), Operand::Wide { imm16, shift }],
            );
        }
    }
}

fn negative(b: &mut Builder, errno: u16) -> u32 {
    n(b, 10, u64::from(errno));
    b.emit(
        "adds.X.shift",
        [reg(ZERO), reg(Reg::x(0)), reg(Reg::x(10)), unshifted()],
    );
    branch_cond(b, 0)
}

fn result(b: &mut Builder) {
    compare(b, Reg::x(0), 0);
    let success = branch_cond(b, 10);
    negate(b, Reg::x(10), Reg::x(0));
    b.memory("str.X.Unsigned", Reg::x(10), REQ, E);
    let done = branch(b);
    patch_cond(b, success, 10);
    b.memory("str.X.Unsigned", Reg::x(0), REQ, V);
    b.patch_branch(done, "b", &[], &[]);
}

fn observe(b: &mut Builder) {
    b.memory("str.X.Unsigned", ZERO, REQ, A + 8);
    args(b, 1);
    b.add(Reg::x(1), REQ, A + 8);
    n(b, 2, 1);
    n(b, 3, 0);
    syscall(b, 260);
    compare(b, Reg::x(0), 0);
    let pending = branch_cond(b, 0);
    let failed = branch_cond(b, 11);
    b.memory("ldr.W.Unsigned", Reg::w(9), REQ, A + 8);
    b.emit("ubfm.X", [reg(Reg::x(10)), reg(Reg::x(9)), imm(0), imm(6)]);
    compare(b, Reg::x(10), 0);
    let signaled = branch_cond(b, 1);
    b.emit("ubfm.X", [reg(Reg::x(10)), reg(Reg::x(9)), imm(8), imm(15)]);
    n(b, 0, 1);
    let decoded = branch(b);
    patch_cond(b, signaled, 1);
    n(b, 0, 2);
    b.patch_branch(decoded, "b", &[], &[]);
    b.memory("str.X.Unsigned", Reg::x(10), REQ, A + 8);
    b.memory("str.X.Unsigned", Reg::x(0), REQ, V);
    let done = branch(b);
    patch_cond(b, failed, 11);
    result(b);
    b.patch_branch(done, "b", &[], &[]);
    patch_cond(b, pending, 0);
}

fn duplicate(b: &mut Builder) {
    args(b, 1);
    n(b, 1, 1030);
    n(b, 2, 3);
    syscall(b, 25);
    result(b);
}

fn open_null(b: &mut Builder) {
    n(b, 9, u64::from_le_bytes(*b"/dev/nul"));
    b.memory("str.X.Unsigned", Reg::x(9), SP, 0);
    n(b, 9, 108);
    b.memory("str.X.Unsigned", Reg::x(9), SP, 8);
    n(b, 0, (-100_i64) as u64);
    b.add(Reg::x(1), SP, 0);
    n(b, 2, 524290);
    n(b, 3, 0);
    syscall(b, 56);
    result(b);
}

fn pipe(b: &mut Builder) {
    b.add(Reg::x(0), SP, 0);
    n(b, 1, 524288);
    syscall(b, 59);
    compare(b, Reg::x(0), 0);
    let failed = branch_cond(b, 11);
    b.memory("ldr.W.Unsigned", Reg::w(23), SP, 0);
    b.memory("ldr.W.Unsigned", Reg::w(24), SP, 4);
    b.memory("ldr.X.Unsigned", Reg::x(9), REQ, A);
    let output = skip(b, Reg::x(9), 0);
    b.add(Reg::x(9), PARENT, 0);
    b.add(PARENT, OTHER, 0);
    b.add(OTHER, Reg::x(9), 0);
    patch_cond(b, output, 1);
    b.add(Reg::x(0), PARENT, 0);
    n(b, 1, 4);
    n(b, 2, 2048);
    syscall(b, 25);
    compare(b, Reg::x(0), 0);
    let flags_failed = branch_cond(b, 11);
    b.memory("str.X.Unsigned", PARENT, REQ, V);
    b.memory("str.X.Unsigned", OTHER, REQ, A + 16);
    let done = branch(b);
    patch_cond(b, flags_failed, 11);
    b.add(RESULT, Reg::x(0), 0);
    for r in [PARENT, OTHER] {
        b.add(Reg::x(0), r, 0);
        syscall(b, 57);
    }
    b.add(Reg::x(0), RESULT, 0);
    patch_cond(b, failed, 11);
    result(b);
    b.patch_branch(done, "b", &[], &[]);
}

fn pipe_write(b: &mut Builder) {
    n(b, 9, 4096);
    b.memory("str.X.Unsigned", Reg::x(9), SP, 32);
    for at in [40, 48, 56, 64, 72] {
        b.memory("str.X.Unsigned", ZERO, SP, at);
    }
    n(b, 0, 0);
    b.add(Reg::x(1), SP, 32);
    b.add(Reg::x(2), SP, 40);
    n(b, 3, 8);
    syscall(b, 135);
    compare(b, Reg::x(0), 0);
    let failed = branch_cond(b, 1);
    b.add(Reg::x(0), SP, 48);
    n(b, 1, 8);
    syscall(b, 136);
    invariant(b);
    args(b, 3);
    syscall(b, 64);
    b.add(RESULT, Reg::x(0), 0);
    let not_epipe = negative(b, 32);
    let skip_consume = branch(b);
    patch_cond(b, not_epipe, 0);
    b.memory("ldr.X.Unsigned", Reg::x(9), SP, 48);
    b.emit("ubfm.X", [reg(Reg::x(9)), reg(Reg::x(9)), imm(12), imm(12)]);
    compare(b, Reg::x(9), 0);
    let old_pending = branch_cond(b, 1);
    let retry = b.offset();
    b.add(Reg::x(0), SP, 32);
    n(b, 1, 0);
    b.add(Reg::x(2), SP, 56);
    n(b, 3, 8);
    syscall(b, 137);
    retry_if_interrupted(b, retry);
    b.patch_branch(skip_consume, "b", &[], &[]);
    patch_cond(b, old_pending, 1);
    n(b, 0, 2);
    b.add(Reg::x(1), SP, 40);
    n(b, 2, 0);
    n(b, 3, 8);
    syscall(b, 135);
    invariant(b);
    b.add(Reg::x(0), RESULT, 0);
    patch_cond(b, failed, 1);
    result(b);
}

fn invariant(b: &mut Builder) {
    compare(b, Reg::x(0), 0);
    let okay = branch_cond(b, 0);
    b.emit("brk", [imm(0)]);
    patch_cond(b, okay, 0);
}

fn retry_if_interrupted(b: &mut Builder, target: u32) {
    b.emit("adds.X.imm", [reg(ZERO), reg(Reg::x(0)), super::add_imm(4)]);
    let at = b.offset();
    b.emit(
        "b.cond",
        [
            Operand::Target(Target::Offset(i64::from(target) - i64::from(at))),
            super::condition(0),
        ],
    );
}
