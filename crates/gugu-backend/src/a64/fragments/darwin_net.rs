use super::{Builder, Fragment, LR, SP, add_imm, condition, imm, movz, next, reg};
use crate::{
    NativeImport,
    a64::{
        macho::import_stub_symbol,
        reg::{Reg, Width},
    },
    macho::LIB_SYSTEM,
};

enum Abi {
    Int(u8),
    Ssize(u8),
    Fcntl,
}

use Abi::{Fcntl, Int, Ssize};

const METHODS: &[(usize, &str, Abi)] = &[
    (0, "socket", Int(3)),
    (1, "setsockopt", Int(5)),
    (2, "bind", Int(3)),
    (3, "listen", Int(2)),
    (4, "accept", Int(3)),
    (5, "getsockname", Int(3)),
    (6, "getpeername", Int(3)),
    (7, "close", Int(1)),
    (8, "kqueue", Int(0)),
    (9, "kevent", Int(6)),
    (10, "poll", Int(3)),
    (21, "connect", Int(3)),
    (22, "shutdown", Int(2)),
    (23, "getsockopt", Int(5)),
    (24, "recvfrom", Ssize(6)),
    (25, "sendto", Ssize(6)),
    (26, "fcntl", Fcntl),
    (27, "recvmsg", Ssize(3)),
    (28, "disconnectx", Int(3)),
];

const CLOSE_CALLBACK: usize = 19;
const VALUE: usize = 72;
const ERROR: usize = 80;
const EINVAL: u16 = 22;
const EQ: u8 = 0;
const NE: u8 = 1;
const OP: Reg = Reg::x(0);
const RESULT: Reg = Reg::x(0);
const REQUEST: Reg = Reg::x(19);
const FP: Reg = Reg::x(29);
const XZR: Reg = Reg::zr(Width::X);

pub fn darwin_net_imports() -> Vec<NativeImport> {
    METHODS
        .iter()
        .map(|&(_, name, _)| name)
        .chain(["__error"])
        .map(|name| NativeImport::function(LIB_SYSTEM, name))
        .collect()
}

/// x0 = 操作号，x1 = net_platform.gg 请求；调用方须已进入原生栈。
pub fn darwin_net_dispatch() -> Fragment {
    let mut b = Builder::default();
    let saved = [(8, REQUEST), (16, FP), (24, LR)];
    enter(&mut b, 32, &saved);
    b.add(REQUEST, Reg::x(1), 0);
    b.memory("str.X.Unsigned", XZR, REQUEST, VALUE);
    b.memory("str.X.Unsigned", XZR, REQUEST, ERROR);
    let skip_callback = compare_op(&mut b, CLOSE_CALLBACK);
    let callback_ref = b.offset();
    b.emit("adr", [reg(RESULT), next()]);
    let mut successes = vec![branch(&mut b, "b", [next()])];
    b.patch_branch(skip_callback, "b.cond", &[], &[condition(NE)]);
    let mut failures = Vec::new();
    for &(op, name, ref abi) in METHODS {
        let skip = compare_op(&mut b, op);
        call_method(&mut b, name, abi);
        failures.push(branch(&mut b, "b.cond", [next(), condition(EQ)]));
        successes.push(branch(&mut b, "b", [next()]));
        b.patch_branch(skip, "b.cond", &[], &[condition(NE)]);
    }
    b.emit("movz.X", [reg(RESULT), movz(EINVAL)]);
    let invalid = branch(&mut b, "b", [next()]);
    for at in failures {
        b.patch_branch(at, "b.cond", &[], &[condition(EQ)]);
    }
    read_errno(&mut b);
    b.patch_branch(invalid, "b", &[], &[]);
    b.memory("str.X.Unsigned", RESULT, REQUEST, ERROR);
    let done = branch(&mut b, "b", [next()]);
    for at in successes {
        b.patch_branch(at, "b", &[], &[]);
    }
    b.memory("str.X.Unsigned", RESULT, REQUEST, VALUE);
    b.patch_branch(done, "b", &[], &[]);
    b.memory("ldr.X.Unsigned", RESULT, REQUEST, VALUE);
    leave(&mut b, 32, &saved);
    b.patch_branch(callback_ref, "adr", &[reg(RESULT)], &[]);
    close_callback(&mut b);
    b.finish()
}

fn call_method(b: &mut Builder, name: &str, abi: &Abi) {
    let argc = match *abi {
        Int(argc) | Ssize(argc) => argc,
        Fcntl => 2,
    };
    for i in 0..argc {
        b.memory("ldr.X.Unsigned", Reg::x(i), REQUEST, usize::from(i) * 8);
    }
    if matches!(abi, Fcntl) {
        let arg = Reg::w(10);
        b.memory("ldr.W.Unsigned", arg, REQUEST, 16);
        b.memory("str.W.Unsigned", arg, SP, 0);
    }
    call(b, name);
    if !matches!(abi, Ssize(_)) {
        b.emit("sbfm.X", [reg(RESULT), reg(RESULT), imm(0), imm(31)]);
    }
    b.emit("adds.X.imm", [reg(XZR), reg(RESULT), add_imm(1)]);
}

fn close_callback(b: &mut Builder) {
    let saved = [(0, FP), (8, LR)];
    enter(b, 16, &saved);
    call(b, "close");
    let done = branch(b, "cbz.W", [reg(Reg::w(0)), next()]);
    read_errno(b);
    b.patch_branch(done, "cbz.W", &[reg(Reg::w(0))], &[]);
    leave(b, 16, &saved);
}

fn compare_op(b: &mut Builder, op: usize) -> u32 {
    b.emit("subs.X.imm", [reg(XZR), reg(OP), add_imm(op)]);
    branch(b, "b.cond", [next(), condition(NE)])
}

fn call(b: &mut Builder, name: &str) {
    b.external_branch("bl", &import_stub_symbol(name));
}

fn read_errno(b: &mut Builder) {
    call(b, "__error");
    b.memory("ldr.W.Unsigned", Reg::w(0), RESULT, 0);
}

fn enter(b: &mut Builder, frame: usize, saved: &[(usize, Reg)]) {
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(frame)]);
    for &(offset, register) in saved {
        b.memory("str.X.Unsigned", register, SP, offset);
    }
    b.add(FP, SP, frame - 16);
}

fn leave(b: &mut Builder, frame: usize, saved: &[(usize, Reg)]) {
    for &(offset, register) in saved {
        b.memory("ldr.X.Unsigned", register, SP, offset);
    }
    b.add(SP, SP, frame);
    b.ret();
}

fn branch<const N: usize>(b: &mut Builder, form: &str, operands: [super::Operand; N]) -> u32 {
    let at = b.offset();
    b.emit(form, operands);
    at
}
