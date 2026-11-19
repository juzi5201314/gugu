use std::mem::offset_of;

use super::{Builder, Fragment, LR, Operand, SP, Target, add_imm, condition, imm, movz, next, reg};
use crate::{
    NativeImport,
    a64::{
        macho::import_stub_symbol,
        reg::{Reg, Width},
    },
    macho::LIB_SYSTEM,
    runtime::process_schema::{ProcessRequest, SpawnPacket},
};

const A: usize = offset_of!(ProcessRequest, arguments);
const V: usize = offset_of!(ProcessRequest, value);
const E: usize = offset_of!(ProcessRequest, error);
const X0: Reg = Reg::x(0);
const W0: Reg = Reg::w(0);
const XZR: Reg = Reg::zr(Width::X);
const REQ: Reg = Reg::x(19);
const PACKET: Reg = Reg::x(20);
const CHILD: Reg = Reg::x(21);
const FP: Reg = Reg::x(29);
const EQ: u8 = 0;
const NE: u8 = 1;
const EINVAL: u16 = 22;
const ACTIONS: usize = 8;
const SCRATCH: usize = 16;
const CALL_SAVED: [(usize, Reg); 3] = [(8, REQ), (16, FP), (24, LR)];
const WORKER_SAVED: [(usize, Reg); 5] = [(24, REQ), (32, PACKET), (40, CHILD), (48, FP), (56, LR)];

pub fn darwin_process_imports() -> Vec<NativeImport> {
    [
        "waitpid",
        "kill",
        "close",
        "read",
        "write",
        "__error",
        "pipe",
        "open",
        "fcntl",
        "posix_spawn_file_actions_init",
        "posix_spawn_file_actions_adddup2",
        "posix_spawn_file_actions_addchdir_np",
        "posix_spawn",
        "posix_spawn_file_actions_destroy",
    ]
    .map(|name| NativeImport::function(LIB_SYSTEM, name))
    .into()
}

/// x0 = 操作号，x1 = ProcessRequest；调用方须已进入原生栈。
pub fn darwin_process_call() -> Fragment {
    let mut b = Builder::default();
    enter(&mut b, 32, &CALL_SAVED, Reg::x(1));
    let op = Reg::x(9);
    b.add(op, X0, 0);
    let mut exits = Vec::new();
    for (number, name, argc) in [
        (0, "waitpid", 0),
        (1, "kill", 1),
        (2, "close", 1),
        (3, "read", 3),
        (4, "write", 3),
    ] {
        let skip = skip_op(&mut b, op, number);
        if number == 0 {
            observe(&mut b);
        } else {
            for i in 0..argc {
                b.memory("ldr.X.Unsigned", Reg::x(i), REQ, A + usize::from(i) * 8);
            }
            if number == 1 {
                number_reg(&mut b, Reg::x(1), 9);
            }
            call(&mut b, name);
            unix_result(&mut b, number >= 3);
        }
        exits.push(jump(&mut b));
        patch_cond(&mut b, skip, NE);
    }
    number_reg(&mut b, X0, EINVAL);
    b.memory("str.X.Unsigned", X0, REQ, E);
    for at in exits {
        b.patch_branch(at, "b", &[], &[]);
    }
    leave(&mut b, 32, &CALL_SAVED);
    b.finish()
}

/// 首入口返回同一片段内的 C ABI worker 地址；worker 的 x0 为 ProcessRequest。
pub fn darwin_process_worker() -> Fragment {
    let mut b = Builder::default();
    b.emit("adr", [reg(X0), Operand::Target(Target::Offset(8))]);
    b.ret();
    enter(&mut b, 64, &WORKER_SAVED, X0);
    let op = Reg::x(9);
    b.memory(
        "ldr.X.Unsigned",
        op,
        REQ,
        offset_of!(ProcessRequest, operation),
    );
    let mut exits = Vec::new();
    for (number, handler) in [spawn, pipe, open_null, duplicate_stdio]
        .into_iter()
        .enumerate()
    {
        let skip = skip_op(&mut b, op, number);
        handler(&mut b);
        exits.push(jump(&mut b));
        patch_cond(&mut b, skip, NE);
    }
    b.emit("brk", [imm(0)]);
    for at in exits {
        b.patch_branch(at, "b", &[], &[]);
    }
    leave(&mut b, 64, &WORKER_SAVED);
    b.finish()
}

fn observe(b: &mut Builder) {
    b.memory("str.X.Unsigned", XZR, REQ, A + 8);
    b.memory("ldr.X.Unsigned", X0, REQ, A);
    b.add(Reg::x(1), REQ, A + 8);
    number_reg(b, Reg::x(2), 1);
    call(b, "waitpid");
    let pending = forward(b, "cbz.W", [reg(W0), next()]);
    let failed = minus_one(b, false);
    let status = Reg::x(9);
    let code = Reg::x(10);
    b.memory("ldr.W.Unsigned", Reg::w(9), REQ, A + 8);
    b.emit("ubfm.X", [reg(code), reg(status), imm(0), imm(6)]);
    let signaled = forward(b, "cbnz.X", [reg(code), next()]);
    b.emit("ubfm.X", [reg(code), reg(status), imm(8), imm(15)]);
    number_reg(b, X0, 1);
    let decoded = jump(b);
    b.patch_branch(signaled, "cbnz.X", &[reg(code)], &[]);
    number_reg(b, X0, 2);
    b.patch_branch(decoded, "b", &[], &[]);
    b.memory("str.X.Unsigned", code, REQ, A + 8);
    b.memory("str.X.Unsigned", X0, REQ, V);
    let done = jump(b);
    patch_cond(b, failed, EQ);
    store_errno(b);
    b.patch_branch(pending, "cbz.W", &[reg(W0)], &[]);
    b.patch_branch(done, "b", &[], &[]);
}

fn spawn(b: &mut Builder) {
    b.memory("ldr.X.Unsigned", PACKET, REQ, A);
    b.add(X0, SP, ACTIONS);
    call(b, "posix_spawn_file_actions_init");
    let init_failed = forward(b, "cbnz.W", [reg(W0), next()]);
    let mut cleanup = Vec::new();
    for (fd, offset) in [
        offset_of!(SpawnPacket, stdin_handle),
        offset_of!(SpawnPacket, stdout_handle),
        offset_of!(SpawnPacket, stderr_handle),
    ]
    .into_iter()
    .enumerate()
    {
        let source = Reg::x(1);
        b.memory("ldr.X.Unsigned", source, PACKET, offset);
        b.emit("adds.X.imm", [reg(XZR), reg(source), add_imm(1)]);
        let skip = jump_cond(b, EQ);
        b.add(X0, SP, ACTIONS);
        number_reg(b, Reg::x(2), fd as u16);
        call(b, "posix_spawn_file_actions_adddup2");
        cleanup.push(forward(b, "cbnz.W", [reg(W0), next()]));
        patch_cond(b, skip, EQ);
    }
    let cwd = Reg::x(1);
    b.memory("ldr.X.Unsigned", cwd, PACKET, offset_of!(SpawnPacket, cwd));
    let skip_cwd = forward(b, "cbz.X", [reg(cwd), next()]);
    b.add(X0, SP, ACTIONS);
    call(b, "posix_spawn_file_actions_addchdir_np");
    cleanup.push(forward(b, "cbnz.W", [reg(W0), next()]));
    b.patch_branch(skip_cwd, "cbz.X", &[reg(cwd)], &[]);
    spawn_call(b);
    cleanup.push(forward(b, "cbnz.W", [reg(W0), next()]));
    let pid = Reg::x(9);
    b.memory("ldr.W.Unsigned", Reg::w(9), SP, SCRATCH);
    b.memory(
        "str.X.Unsigned",
        pid,
        PACKET,
        offset_of!(SpawnPacket, process_handle),
    );
    b.memory(
        "str.X.Unsigned",
        pid,
        PACKET,
        offset_of!(SpawnPacket, process_id),
    );
    b.memory("str.X.Unsigned", pid, REQ, V);
    for at in cleanup {
        b.patch_branch(at, "cbnz.W", &[reg(W0)], &[]);
    }
    b.memory("str.W.Unsigned", W0, REQ, E);
    b.add(X0, SP, ACTIONS);
    call(b, "posix_spawn_file_actions_destroy");
    let done = jump(b);
    b.patch_branch(init_failed, "cbnz.W", &[reg(W0)], &[]);
    b.memory("str.W.Unsigned", W0, REQ, E);
    b.patch_branch(done, "b", &[], &[]);
}

fn spawn_call(b: &mut Builder) {
    b.add(X0, SP, SCRATCH);
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(1),
        PACKET,
        offset_of!(SpawnPacket, executable),
    );
    b.add(Reg::x(2), SP, ACTIONS);
    number_reg(b, Reg::x(3), 0);
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(4),
        PACKET,
        offset_of!(SpawnPacket, argv),
    );
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(5),
        PACKET,
        offset_of!(SpawnPacket, environment),
    );
    call(b, "posix_spawn");
}

fn pipe(b: &mut Builder) {
    b.add(X0, SP, SCRATCH);
    call(b, "pipe");
    let failed = minus_one(b, false);
    b.memory("ldr.W.Unsigned", W0, SP, SCRATCH);
    b.memory("ldr.W.Unsigned", Reg::w(1), SP, SCRATCH + 4);
    b.memory("ldr.X.Unsigned", Reg::x(9), REQ, A);
    b.emit("subs.X.imm", [reg(XZR), reg(Reg::x(9)), add_imm(0)]);
    b.emit(
        "csel.X",
        [reg(PACKET), reg(Reg::x(1)), reg(X0), condition(EQ)],
    );
    b.emit(
        "csel.X",
        [reg(CHILD), reg(X0), reg(Reg::x(1)), condition(EQ)],
    );
    let mut cleanup = Vec::new();
    for (fd, command, value) in [
        (PACKET, 2, 1),
        (CHILD, 2, 1),
        (PACKET, 4, 4),
        (PACKET, 73, 1),
    ] {
        b.add(X0, fd, 0);
        fcntl(b, command, value);
        cleanup.push(minus_one(b, false));
    }
    b.memory("str.X.Unsigned", PACKET, REQ, V);
    b.memory("str.X.Unsigned", CHILD, REQ, A + 16);
    let done = jump(b);
    for at in cleanup {
        patch_cond(b, at, EQ);
    }
    store_errno(b);
    for fd in [PACKET, CHILD] {
        b.add(X0, fd, 0);
        call(b, "close");
    }
    let cleaned = jump(b);
    patch_cond(b, failed, EQ);
    store_errno(b);
    b.patch_branch(done, "b", &[], &[]);
    b.patch_branch(cleaned, "b", &[], &[]);
}

fn open_null(b: &mut Builder) {
    for (i, pair) in b"/dev/nul".as_chunks::<2>().0.iter().enumerate() {
        b.emit(
            if i == 0 { "movz.X" } else { "movk.X" },
            [
                reg(X0),
                Operand::Wide {
                    imm16: u16::from_le_bytes(*pair),
                    shift: i as u8 * 16,
                },
            ],
        );
    }
    b.memory("str.X.Unsigned", X0, SP, ACTIONS);
    number_reg(b, X0, u16::from(b'l'));
    b.memory("str.X.Unsigned", X0, SP, SCRATCH);
    b.add(X0, SP, ACTIONS);
    b.emit(
        "movz.X",
        [
            reg(Reg::x(1)),
            Operand::Wide {
                imm16: 0x100,
                shift: 16,
            },
        ],
    );
    b.add(Reg::x(1), Reg::x(1), 2);
    call(b, "open");
    unix_result(b, false);
}

fn duplicate_stdio(b: &mut Builder) {
    b.memory("ldr.X.Unsigned", X0, REQ, A);
    fcntl(b, 67, 3);
    unix_result(b, false);
}

fn fcntl(b: &mut Builder, command: u16, value: u16) {
    number_reg(b, Reg::x(1), command);
    number_reg(b, Reg::x(9), value);
    b.memory("str.W.Unsigned", Reg::w(9), SP, 0);
    call(b, "fcntl");
}

fn unix_result(b: &mut Builder, wide: bool) {
    let failed = minus_one(b, wide);
    b.memory("str.X.Unsigned", X0, REQ, V);
    let done = jump(b);
    patch_cond(b, failed, EQ);
    store_errno(b);
    b.patch_branch(done, "b", &[], &[]);
}

fn minus_one(b: &mut Builder, wide: bool) -> u32 {
    if !wide {
        b.emit("sbfm.X", [reg(X0), reg(X0), imm(0), imm(31)]);
    }
    b.emit("adds.X.imm", [reg(XZR), reg(X0), add_imm(1)]);
    jump_cond(b, EQ)
}

fn store_errno(b: &mut Builder) {
    call(b, "__error");
    b.memory("ldr.W.Unsigned", W0, X0, 0);
    b.memory("str.X.Unsigned", X0, REQ, E);
}

fn enter(b: &mut Builder, frame: usize, saved: &[(usize, Reg)], request: Reg) {
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(frame)]);
    for &(offset, register) in saved {
        b.memory("str.X.Unsigned", register, SP, offset);
    }
    b.add(FP, SP, frame - 16);
    b.add(REQ, request, 0);
    b.memory("str.X.Unsigned", XZR, REQ, V);
    b.memory("str.X.Unsigned", XZR, REQ, E);
}

fn leave(b: &mut Builder, frame: usize, saved: &[(usize, Reg)]) {
    for &(offset, register) in saved {
        b.memory("ldr.X.Unsigned", register, SP, offset);
    }
    number_reg(b, X0, 0);
    b.add(SP, SP, frame);
    b.ret();
}

fn call(b: &mut Builder, name: &str) {
    b.external_branch("bl", &import_stub_symbol(name));
}

fn number_reg(b: &mut Builder, register: Reg, value: u16) {
    b.emit("movz.X", [reg(register), movz(value)]);
}

fn skip_op(b: &mut Builder, op: Reg, number: usize) -> u32 {
    b.emit("subs.X.imm", [reg(XZR), reg(op), add_imm(number)]);
    jump_cond(b, NE)
}

fn jump_cond(b: &mut Builder, cc: u8) -> u32 {
    forward(b, "b.cond", [next(), condition(cc)])
}

fn patch_cond(b: &mut Builder, at: u32, cc: u8) {
    b.patch_branch(at, "b.cond", &[], &[condition(cc)]);
}

fn jump(b: &mut Builder) -> u32 {
    forward(b, "b", [next()])
}

fn forward<const N: usize>(b: &mut Builder, form: &str, operands: [Operand; N]) -> u32 {
    let at = b.offset();
    b.emit(form, operands);
    at
}
