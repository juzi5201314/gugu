use super::{Builder, Fragment, SP, imm, movz, next, reg, unshifted};
use crate::{
    a64::{
        inst::{Operand, Shift, Target},
        reg::Reg,
    },
    runtime::entry::rt0_params,
};

pub(crate) fn linux_main(params: &str, rt0: &str) -> Fragment {
    let mut b = Builder::default();
    b.memory("ldr.X.Unsigned", Reg::x(3), SP, 0);
    b.add(Reg::x(4), SP, 8);
    b.emit(
        "add.X.shift",
        [
            reg(Reg::x(2)),
            reg(Reg::x(4)),
            reg(Reg::x(3)),
            Operand::Shift(Shift::Lsl, 3),
        ],
    );
    b.add(Reg::x(2), Reg::x(2), 8);
    let target = Operand::Target(Target::Symbol {
        name: params.into(),
        addend: 0,
    });
    b.emit("adrp", [reg(Reg::x(0)), target.clone()]);
    b.emit("add.X.imm", [reg(Reg::x(0)), reg(Reg::x(0)), target]);
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(1),
        Reg::x(0),
        rt0_params::BIAS_BASELINE * 8,
    );
    b.emit(
        "sub.X.shift",
        [reg(Reg::x(1)), reg(Reg::x(0)), reg(Reg::x(1)), unshifted()],
    );
    relocate(&mut b);
    b.external_branch("bl", rt0);
    b.emit("movz.X", [reg(Reg::x(8)), movz(94)]);
    b.emit("svc", [imm(0)]);
    b.emit("brk", [imm(0)]);
    b.finish()
}

fn relocate(b: &mut Builder) {
    let table = Reg::x(9);
    let count = Reg::x(10);
    let slot = Reg::x(11);
    let target = Reg::x(12);
    b.memory(
        "ldr.X.Unsigned",
        table,
        Reg::x(0),
        rt0_params::RELOC_TABLE * 8,
    );
    b.memory(
        "ldr.X.Unsigned",
        count,
        Reg::x(0),
        rt0_params::RELOC_COUNT * 8,
    );
    b.emit(
        "add.X.shift",
        [reg(table), reg(table), reg(Reg::x(1)), unshifted()],
    );
    let empty = b.offset();
    b.emit("cbz.X", [reg(count), next()]);
    let loop_at = b.offset();
    b.memory("ldr.X.Unsigned", slot, table, 0);
    b.memory("ldr.X.Unsigned", target, table, 16);
    for register in [slot, target] {
        b.emit(
            "add.X.shift",
            [reg(register), reg(register), reg(Reg::x(1)), unshifted()],
        );
    }
    b.memory("str.X.Unsigned", target, slot, 0);
    b.add(table, table, 24);
    b.emit("sub.X.imm", [reg(count), reg(count), super::add_imm(1)]);
    b.branch("cbnz.X", &[reg(count)], loop_at);
    b.patch_branch(empty, "cbz.X", &[reg(count)], &[]);
}
