use super::linux_syscall::*;
use super::{Builder, Fragment, SP, imm, movz, reg};
use crate::{
    a64::reg::Reg,
    runtime::foreign_schema::{
        NATIVE_REQUEST_ARGUMENTS as ARGS, NATIVE_REQUEST_ERROR as ERROR,
        NATIVE_REQUEST_OPERATION as OP, NATIVE_REQUEST_VALUE as VALUE,
    },
};

const METHODS: &[(usize, u16, u8)] = &[
    (13, 134, 4),
    (14, 135, 4),
    (39, 172, 0),
    (62, 129, 2),
    (186, 178, 0),
    (200, 130, 2),
    (234, 131, 3),
    (289, 74, 4),
];

/// x0 = NativeRequest; signal operations use x86_64-numbered Linux kernel ABI.
pub fn linux_signal_dispatch() -> Fragment {
    let mut b = Builder::default();
    enter(&mut b, 64);
    b.add(Reg::x(19), Reg::x(0), 0);
    b.memory("str.X.Unsigned", ZERO, Reg::x(19), ERROR);
    b.memory("ldr.X.Unsigned", Reg::x(9), Reg::x(19), OP);
    let mut exits = Vec::new();
    dispatch(&mut b, &mut exits);
    b.memory("str.X.Unsigned", ZERO, Reg::x(19), VALUE);
    b.emit("movz.X", [reg(Reg::x(0)), movz(22)]);
    b.memory("str.X.Unsigned", Reg::x(0), Reg::x(19), ERROR);
    for at in exits {
        b.patch_branch(at, "b", &[], &[]);
    }
    b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
    leave(&mut b, 64);
    b.finish()
}

pub(super) fn dispatch(b: &mut Builder, exits: &mut Vec<u32>) {
    for &(operation, number, count) in METHODS {
        let other = skip(b, Reg::x(9), operation);
        for index in 0..count {
            b.memory(
                "ldr.X.Unsigned",
                Reg::x(index),
                Reg::x(19),
                ARGS + index as usize * 8,
            );
        }
        if operation == 13 {
            prepare_action(b);
        }
        syscall(b, number);
        b.memory("str.X.Unsigned", Reg::x(0), Reg::x(19), VALUE);
        compare(b, Reg::x(0), 0);
        let success = branch_cond(b, 10);
        negate(b, Reg::x(10), Reg::x(0));
        b.memory("str.X.Unsigned", Reg::x(10), Reg::x(19), ERROR);
        let failed = branch(b);
        patch_cond(b, success, 10);
        if operation == 13 {
            copy_old_action(b);
        }
        b.patch_branch(failed, "b", &[], &[]);
        exits.push(branch(b));
        patch_cond(b, other, 1);
    }
}

fn prepare_action(b: &mut Builder) {
    compare(b, Reg::x(1), 0);
    let query = branch_cond(b, 0);
    b.memory("ldr.X.Unsigned", Reg::x(10), Reg::x(1), 0);
    b.memory("ldr.X.Unsigned", Reg::x(11), Reg::x(1), 8);
    b.memory("ldr.X.Unsigned", Reg::x(12), Reg::x(1), 24);
    b.emit("bfm.X", [reg(Reg::x(11)), reg(ZERO), imm(38), imm(0)]);
    b.memory("str.X.Unsigned", Reg::x(10), SP, 0);
    b.memory("str.X.Unsigned", Reg::x(11), SP, 8);
    b.memory("str.X.Unsigned", ZERO, SP, 16);
    b.memory("str.X.Unsigned", Reg::x(12), SP, 24);
    b.add(Reg::x(1), SP, 0);
    patch_cond(b, query, 0);
    b.add(Reg::x(20), Reg::x(2), 0);
    compare(b, Reg::x(2), 0);
    let no_output = branch_cond(b, 0);
    b.add(Reg::x(2), SP, 32);
    patch_cond(b, no_output, 0);
}

fn copy_old_action(b: &mut Builder) {
    compare(b, Reg::x(20), 0);
    let no_output = branch_cond(b, 0);
    for offset in [0, 8, 16, 24] {
        b.memory("ldr.X.Unsigned", Reg::x(10), SP, 32 + offset);
        b.memory("str.X.Unsigned", Reg::x(10), Reg::x(20), offset);
    }
    patch_cond(b, no_output, 0);
}
