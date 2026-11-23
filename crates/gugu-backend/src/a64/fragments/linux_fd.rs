use super::{Builder, Fragment, LR, SP, add_imm, condition, imm, movz, next, reg};
use crate::{
    a64::reg::{Reg, Width},
    runtime::foreign_schema::{
        NATIVE_REQUEST_ARGUMENTS, NATIVE_REQUEST_ERROR, NATIVE_REQUEST_OPERATION,
        NATIVE_REQUEST_VALUE,
    },
};

#[derive(Clone, Copy)]
enum Argument {
    Request(u8),
    Constant(i16),
    StatBuffer,
}
use Argument::{Constant as C, Request as A, StatBuffer as S};

struct Method {
    operation: u16,
    syscall: u16,
    arguments: &'static [Argument],
    stat_output: Option<u8>,
}

const METHODS: &[Method] = &[
    method(0, 63, &[A(0), A(1), A(2)], None),
    method(1, 64, &[A(0), A(1), A(2)], None),
    method(2, 56, &[C(-100), A(0), A(1), A(2)], None),
    method(3, 57, &[A(0)], None),
    method(4, 79, &[C(-100), A(0), S, C(0)], Some(1)),
    method(5, 80, &[A(0), S], Some(1)),
    method(6, 79, &[C(-100), A(0), S, C(256)], Some(1)),
    method(8, 62, &[A(0), A(1), A(2)], None),
    method(72, 25, &[A(0), A(1), A(2)], None),
    method(74, 82, &[A(0)], None),
    method(75, 83, &[A(0)], None),
    method(77, 46, &[A(0), A(1)], None),
    method(82, 38, &[C(-100), A(0), C(-100), A(1)], None),
    method(83, 34, &[C(-100), A(0), A(1)], None),
    method(84, 35, &[C(-100), A(0), C(512)], None),
    method(86, 37, &[C(-100), A(0), C(-100), A(1), C(0)], None),
    method(87, 35, &[C(-100), A(0), C(0)], None),
    method(88, 36, &[A(0), C(-100), A(1)], None),
    method(89, 78, &[C(-100), A(0), A(1), A(2)], None),
    method(217, 61, &[A(0), A(1), A(2)], None),
    method(262, 79, &[A(0), A(1), S, A(3)], Some(2)),
];

const fn method(
    operation: u16,
    syscall: u16,
    arguments: &'static [Argument],
    stat_output: Option<u8>,
) -> Method {
    Method {
        operation,
        syscall,
        arguments,
        stat_output,
    }
}

const REQUEST: Reg = Reg::x(19);
const OUTPUT: Reg = Reg::x(20);
const ZERO: Reg = Reg::zr(Width::X);
const FRAME_BYTES: usize = 160;
const SAVED: [(usize, Reg); 4] = [(128, REQUEST), (136, OUTPUT), (144, Reg::x(29)), (152, LR)];

/// x0 = NativeRequest on a native stack; no imports.
/// Operation IDs and result buffers use the existing x86_64-numbered Linux request ABI.
///
/// `exit_thread` 对应 x64 `linux_file_worker(true)`：clone 子线程的入口变体，
/// 写出结果后以 `exit(93)` 终结本线程，内核经 `CLONE_CHILD_CLEARTID` 清 packet[0]
/// 唤醒 `fs_poll`；不恢复寄存器也不依赖 LR（clone 子的 LR 是父帧残留）。非退出
/// 变体注册为 `fd_dispatch`，是 `callback_file_worker` 的 `b` 尾巴，恢复寄存器后
/// `ret` 回 managed 调用方（对应 x64 `worker(false)`）。
pub fn linux_fd_dispatch(exit_thread: bool) -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(FRAME_BYTES)]);
    for (offset, saved) in SAVED {
        b.memory("str.X.Unsigned", saved, SP, offset);
    }
    b.add(Reg::x(29), SP, 144);
    b.add(REQUEST, Reg::x(0), 0);
    b.memory("str.X.Unsigned", ZERO, REQUEST, NATIVE_REQUEST_ERROR);
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(9),
        REQUEST,
        NATIVE_REQUEST_OPERATION,
    );
    let mut exits = Vec::new();
    for method in METHODS {
        b.emit(
            "subs.X.imm",
            [
                reg(ZERO),
                reg(Reg::x(9)),
                add_imm(method.operation as usize),
            ],
        );
        let skip = branch_cond(&mut b, 1);
        call(&mut b, method);
        exits.push(branch(&mut b));
        b.patch_branch(skip, "b.cond", &[], &[condition(1)]);
    }
    super::linux_signal::dispatch(&mut b, &mut exits);
    b.memory("str.X.Unsigned", ZERO, REQUEST, NATIVE_REQUEST_VALUE);
    b.emit("movz.X", [reg(Reg::x(0)), movz(22)]);
    b.memory("str.X.Unsigned", Reg::x(0), REQUEST, NATIVE_REQUEST_ERROR);
    for at in exits {
        b.patch_branch(at, "b", &[], &[]);
    }
    if exit_thread {
        b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
        b.emit("movz.X", [reg(Reg::x(8)), movz(93)]);
        b.emit("svc", [imm(0)]);
        b.emit("brk", [imm(0)]);
    } else {
        for (offset, saved) in SAVED {
            b.memory("ldr.X.Unsigned", saved, SP, offset);
        }
        b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
        b.add(SP, SP, FRAME_BYTES);
        b.ret();
    }
    b.finish()
}

fn call(b: &mut Builder, method: &Method) {
    for (index, argument) in method.arguments.iter().enumerate() {
        argument_value(b, Reg::x(index as u8), *argument);
    }
    if let Some(index) = method.stat_output {
        argument_value(b, OUTPUT, A(index));
    }
    if method.operation == 2 {
        swap_open_flags(b, Reg::x(2));
    }
    if method.operation == 72 {
        fcntl_flags(b, Reg::x(2), 4);
    }
    b.emit("movz.X", [reg(Reg::x(8)), movz(method.syscall)]);
    b.emit("svc", [imm(0)]);
    b.memory("str.X.Unsigned", Reg::x(0), REQUEST, NATIVE_REQUEST_VALUE);
    b.emit("subs.X.imm", [reg(ZERO), reg(Reg::x(0)), add_imm(0)]);
    let success = branch_cond(b, 10);
    b.emit(
        "sub.X.shift",
        [
            reg(Reg::x(10)),
            reg(ZERO),
            reg(Reg::x(0)),
            super::unshifted(),
        ],
    );
    b.memory("str.X.Unsigned", Reg::x(10), REQUEST, NATIVE_REQUEST_ERROR);
    let failed = branch(b);
    b.patch_branch(success, "b.cond", &[], &[condition(10)]);
    if method.stat_output.is_some() {
        normalize_stat(b);
    }
    if method.operation == 72 {
        fcntl_flags(b, Reg::x(0), 3);
        b.memory("str.X.Unsigned", Reg::x(0), REQUEST, NATIVE_REQUEST_VALUE);
    }
    b.patch_branch(failed, "b", &[], &[]);
}

fn argument_value(b: &mut Builder, destination: Reg, argument: Argument) {
    match argument {
        A(index) => b.memory(
            "ldr.X.Unsigned",
            destination,
            REQUEST,
            NATIVE_REQUEST_ARGUMENTS + index as usize * 8,
        ),
        C(value) if value < 0 => b.emit("movn.X", [reg(destination), movz(!value as u16)]),
        C(value) => b.emit("movz.X", [reg(destination), movz(value as u16)]),
        S => b.add(destination, SP, 0),
    }
}

fn fcntl_flags(b: &mut Builder, flags: Reg, command: usize) {
    b.emit("subs.X.imm", [reg(ZERO), reg(Reg::x(1)), add_imm(command)]);
    let skip = branch_cond(b, 1);
    swap_open_flags(b, flags);
    b.patch_branch(skip, "b.cond", &[], &[condition(1)]);
}

fn swap_open_flags(b: &mut Builder, flags: Reg) {
    b.emit("ubfm.X", [reg(Reg::x(10)), reg(flags), imm(14), imm(15)]);
    b.emit("ubfm.X", [reg(Reg::x(11)), reg(flags), imm(16), imm(17)]);
    b.emit("bfm.X", [reg(flags), reg(Reg::x(11)), imm(50), imm(1)]);
    b.emit("bfm.X", [reg(flags), reg(Reg::x(10)), imm(48), imm(1)]);
}

fn normalize_stat(b: &mut Builder) {
    for offset in (0..144).step_by(8) {
        b.memory("str.X.Unsigned", ZERO, OUTPUT, offset);
    }
    for offset in [0, 8, 48, 64, 72, 80, 88, 96, 104, 112] {
        b.memory("ldr.X.Unsigned", Reg::x(10), SP, offset);
        b.memory("str.X.Unsigned", Reg::x(10), OUTPUT, offset);
    }
    for (source, destination) in [(16, 24), (24, 28), (28, 32)] {
        b.memory("ldr.W.Unsigned", Reg::w(10), SP, source);
        b.memory("str.W.Unsigned", Reg::w(10), OUTPUT, destination);
    }
    b.memory("ldr.W.Unsigned", Reg::w(10), SP, 20);
    b.memory("str.X.Unsigned", Reg::x(10), OUTPUT, 16);
    b.memory("ldr.X.Unsigned", Reg::x(10), SP, 32);
    b.memory("str.X.Unsigned", Reg::x(10), OUTPUT, 40);
    b.memory("ldrsw.X.Unsigned", Reg::x(10), SP, 56);
    b.memory("str.X.Unsigned", Reg::x(10), OUTPUT, 56);
}

fn branch_cond(b: &mut Builder, cond: u8) -> u32 {
    let at = b.offset();
    b.emit("b.cond", [next(), condition(cond)]);
    at
}

fn branch(b: &mut Builder) -> u32 {
    let at = b.offset();
    b.emit("b", [next()]);
    at
}
