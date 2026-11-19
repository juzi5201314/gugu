use super::linux_syscall::*;
use super::{Builder, Fragment, SP, add_imm, imm, mem, movz, next, reg};
use crate::a64::{inst::AddressMode, reg::Reg};

const REQUEST: Reg = Reg::x(19);
const OUTPUT: Reg = Reg::x(20);
const VALUE: usize = 72;
const ERROR: usize = 80;
const BATCH: usize = 128;
const SCRATCH: usize = BATCH * 16;
const METHODS: &[(usize, u16, u8)] = &[
    (0, 198, 3),
    (1, 208, 5),
    (2, 200, 3),
    (3, 201, 2),
    (4, 242, 4),
    (5, 204, 3),
    (6, 205, 3),
    (7, 57, 1),
    (8, 20, 1),
    (9, 21, 4),
    (10, 22, 4),
    (21, 203, 3),
    (22, 210, 2),
    (23, 209, 5),
    (24, 207, 6),
    (25, 206, 6),
];

/// x0 = net_call operation, x1 = net_platform packet; native stack required.
pub fn linux_net_dispatch() -> Fragment {
    let mut b = Builder::default();
    enter(&mut b, SCRATCH);
    b.add(REQUEST, Reg::x(1), 0);
    b.memory("str.X.Unsigned", ZERO, REQUEST, ERROR);
    let not_callback = skip(&mut b, Reg::x(0), 19);
    let callback = b.offset();
    b.emit("adr", [reg(Reg::x(0)), next()]);
    let mut exits = vec![branch(&mut b)];
    patch_cond(&mut b, not_callback, 1);
    for &(operation, number, count) in METHODS {
        let other = skip(&mut b, Reg::x(0), operation);
        for index in 0..count {
            b.memory("ldr.X.Unsigned", Reg::x(index), REQUEST, index as usize * 8);
        }
        if operation == 9 {
            epoll_control(&mut b);
        }
        if operation == 10 {
            epoll_wait(&mut b);
        } else {
            syscall(&mut b, number);
        }
        exits.push(branch(&mut b));
        patch_cond(&mut b, other, 1);
    }
    b.emit("movn.X", [reg(Reg::x(0)), movz(21)]);
    for at in exits {
        b.patch_branch(at, "b", &[], &[]);
    }
    result(&mut b);
    leave(&mut b, SCRATCH);
    b.patch_branch(callback, "adr", &[reg(Reg::x(0))], &[]);
    syscall(&mut b, 57);
    negate(&mut b, Reg::x(0), Reg::x(0));
    b.ret();
    b.finish()
}

fn epoll_control(b: &mut Builder) {
    compare(b, Reg::x(1), 2);
    let delete = branch_cond(b, 0);
    compare(b, Reg::x(3), 0);
    let null = branch_cond(b, 0);
    b.memory("ldr.W.Unsigned", Reg::w(10), Reg::x(3), 0);
    b.emit(
        "ldr.X.Unscaled",
        [reg(Reg::x(11)), mem(Reg::x(3), AddressMode::Unscaled(4))],
    );
    b.memory("str.X.Unsigned", ZERO, SP, 0);
    b.memory("str.W.Unsigned", Reg::w(10), SP, 0);
    b.memory("str.X.Unsigned", Reg::x(11), SP, 8);
    b.add(Reg::x(3), SP, 0);
    patch_cond(b, null, 0);
    patch_cond(b, delete, 0);
}

fn epoll_wait(b: &mut Builder) {
    b.emit("sbfm.X", [reg(Reg::x(2)), reg(Reg::x(2)), imm(0), imm(31)]);
    compare(b, Reg::x(2), 0);
    let invalid = branch_cond(b, 13);
    compare(b, Reg::x(2), BATCH);
    let small = branch_cond(b, 13);
    b.emit("movz.X", [reg(Reg::x(2)), movz(BATCH as u16)]);
    patch_cond(b, small, 13);
    b.add(OUTPUT, Reg::x(1), 0);
    b.add(Reg::x(1), SP, 0);
    b.emit("movz.X", [reg(Reg::x(4)), movz(0)]);
    b.emit("movz.X", [reg(Reg::x(5)), movz(8)]);
    syscall(b, 22);
    compare(b, Reg::x(0), 0);
    let empty_or_error = branch_cond(b, 13);
    copy_events(b);
    patch_cond(b, empty_or_error, 13);
    let done = branch(b);
    patch_cond(b, invalid, 13);
    b.emit("movn.X", [reg(Reg::x(0)), movz(21)]);
    b.patch_branch(done, "b", &[], &[]);
}

fn copy_events(b: &mut Builder) {
    b.add(Reg::x(9), SP, 0);
    b.add(Reg::x(10), Reg::x(0), 0);
    let again = b.offset();
    b.memory("ldr.W.Unsigned", Reg::w(11), Reg::x(9), 0);
    b.memory("ldr.X.Unsigned", Reg::x(12), Reg::x(9), 8);
    b.memory("str.W.Unsigned", Reg::w(11), OUTPUT, 0);
    b.emit(
        "str.X.Unscaled",
        [reg(Reg::x(12)), mem(OUTPUT, AddressMode::Unscaled(4))],
    );
    b.add(Reg::x(9), Reg::x(9), 16);
    b.add(OUTPUT, OUTPUT, 12);
    b.emit("subs.X.imm", [reg(Reg::x(10)), reg(Reg::x(10)), add_imm(1)]);
    let at = b.offset();
    b.emit(
        "b.cond",
        [
            super::Operand::Target(super::Target::Offset(i64::from(again) - i64::from(at))),
            super::condition(1),
        ],
    );
}

fn result(b: &mut Builder) {
    compare(b, Reg::x(0), 0);
    let success = branch_cond(b, 10);
    negate(b, Reg::x(10), Reg::x(0));
    b.memory("str.X.Unsigned", Reg::x(10), REQUEST, ERROR);
    b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
    patch_cond(b, success, 10);
    b.memory("str.X.Unsigned", Reg::x(0), REQUEST, VALUE);
}
