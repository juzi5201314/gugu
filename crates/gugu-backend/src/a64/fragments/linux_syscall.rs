use super::{Builder, LR, SP, add_imm, condition, imm, movz, next, reg, unshifted};
use crate::a64::reg::{Reg, Width};

pub(super) const ZERO: Reg = Reg::zr(Width::X);

pub(super) fn syscall(b: &mut Builder, number: u16) {
    b.emit("movz.X", [reg(Reg::x(8)), movz(number)]);
    b.emit("svc", [imm(0)]);
}

pub(super) fn branch(b: &mut Builder) -> u32 {
    let at = b.offset();
    b.emit("b", [next()]);
    at
}

pub(super) fn branch_cond(b: &mut Builder, code: u8) -> u32 {
    let at = b.offset();
    b.emit("b.cond", [next(), condition(code)]);
    at
}

pub(super) fn compare(b: &mut Builder, left: Reg, right: usize) {
    b.emit("subs.X.imm", [reg(ZERO), reg(left), add_imm(right)]);
}

pub(super) fn skip(b: &mut Builder, operation: Reg, number: usize) -> u32 {
    compare(b, operation, number);
    branch_cond(b, 1)
}

pub(super) fn patch_cond(b: &mut Builder, at: u32, code: u8) {
    b.patch_branch(at, "b.cond", &[], &[condition(code)]);
}

pub(super) fn negate(b: &mut Builder, destination: Reg, source: Reg) {
    b.emit(
        "sub.X.shift",
        [reg(destination), reg(ZERO), reg(source), unshifted()],
    );
}

pub(super) fn enter(b: &mut Builder, scratch: usize) {
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(scratch + 32)]);
    for (offset, saved) in [(0, Reg::x(19)), (8, Reg::x(20)), (16, Reg::x(29)), (24, LR)] {
        b.memory("str.X.Unsigned", saved, SP, scratch + offset);
    }
    b.add(Reg::x(29), SP, scratch + 16);
}

pub(super) fn leave(b: &mut Builder, scratch: usize) {
    for (offset, saved) in [(0, Reg::x(19)), (8, Reg::x(20)), (16, Reg::x(29)), (24, LR)] {
        b.memory("ldr.X.Unsigned", saved, SP, scratch + offset);
    }
    b.add(SP, SP, scratch + 32);
    b.ret();
}
