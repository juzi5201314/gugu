//! fs/环境/时钟/启动包等平台原生边界片段；值口径与 x64 fs_native 一致。
//! Linux 全部走 aarch64 syscall 号；Darwin 经 libSystem 桩调用。

use super::linux_syscall::{compare, enter, leave, negate, patch_cond, syscall};
use super::{
    Builder, Fragment, HOT, LR, SP, add_imm, condition, imm, inst_target, mem, movz, next, reg,
    unshifted,
};
use crate::{
    NativeImport,
    a64::{
        foreign::Platform,
        inst::{AddressMode, Operand},
        macho::import_stub_symbol,
        reg::{Reg, Width},
    },
    macho::LIB_SYSTEM,
    runtime::foreign_schema as bridge,
    x64::mangle,
};

const XZR: Reg = Reg::zr(Width::X);
const EQ: u8 = 0;
const NE: u8 = 1;
const LT: u8 = 11;
const GE: u8 = 10;
const A: Reg = Reg::x(9);
const B: Reg = Reg::x(10);
const C: Reg = Reg::x(11);
const OPERATION: usize = bridge::NATIVE_REQUEST_OPERATION;
const ERROR: usize = bridge::NATIVE_REQUEST_ERROR;
const THREAD_HANDLE: usize = bridge::NATIVE_REQUEST_THREAD_HANDLE;

fn rt(name: &str) -> String {
    mangle::mangle_runtime(name)
}

fn jump(b: &mut Builder) -> u32 {
    let at = b.offset();
    b.emit("b", [next()]);
    at
}

fn cond(b: &mut Builder, code: u8) -> u32 {
    let at = b.offset();
    b.emit("b.cond", [next(), condition(code)]);
    at
}

fn patch(b: &mut Builder, at: u32, code: u8) {
    patch_cond(b, at, code);
}

fn link(b: &mut Builder, at: u32) {
    b.patch_branch(at, "b", &[], &[]);
}

fn compare_regs(b: &mut Builder, left: Reg, right: Reg) {
    b.emit(
        "subs.X.shift",
        [reg(XZR), reg(left), reg(right), unshifted()],
    );
}

fn load(b: &mut Builder, dst: Reg, base: Reg, offset: usize) {
    b.memory("ldr.X.Unsigned", dst, base, offset);
}

fn store(b: &mut Builder, base: Reg, offset: usize, src: Reg) {
    b.memory("str.X.Unsigned", src, base, offset);
}

/// `process_native_worker`：`process_worker` 的 +8 副入口（x64 双入口布局）。
pub fn process_native_worker() -> Fragment {
    let mut b = Builder::default();
    b.external_branch_at("b", &rt("process_worker"), 8);
    b.finish()
}

/// `callback_file_worker`：request x0；op==-1 转 foreign worker，其余走不发布
/// DONE 的文件分派变体（对应 x64 `worker(false)`；完成由 callback 协议自身发布）。
pub fn callback_file_worker() -> Fragment {
    let mut b = Builder::default();
    load(&mut b, A, Reg::x(0), OPERATION);
    b.emit("adds.X.imm", [reg(XZR), reg(A), add_imm(1)]);
    let foreign = cond(&mut b, EQ);
    b.external_branch("b", &rt("fd_dispatch"));
    patch(&mut b, foreign, EQ);
    b.external_branch("b", &rt("foreign_native_worker"));
    b.finish()
}

// ---- fs_start / fs_poll ----

/// `fs_start`：x0 = packet（op==−1 走 foreign 线程），x1 = 工作线程栈顶。
/// clone 标志同 x64：VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|PARENT_SETTID|CHILD_CLEARTID。
pub fn linux_fs_start() -> Fragment {
    let mut b = Builder::default();
    load(&mut b, A, Reg::x(0), OPERATION);
    b.emit("adds.X.imm", [reg(XZR), reg(A), add_imm(1)]);
    let foreign = cond(&mut b, EQ);
    b.add(B, Reg::x(0), 0);
    b.emit("sub.X.imm", [reg(C), reg(Reg::x(1)), add_imm(16)]);
    store(&mut b, C, 0, B);
    b.add(Reg::x(1), C, 0);
    b.add(Reg::x(2), B, 0);
    b.add(Reg::x(4), B, 0);
    b.emit("movz.X", [reg(Reg::x(3)), movz(0)]);
    b.emit("movz.X", [reg(Reg::x(0)), movz(0x0f00)]);
    b.emit(
        "movk.X",
        [
            reg(Reg::x(0)),
            Operand::Wide {
                imm16: 0x35,
                shift: 16,
            },
        ],
    );
    syscall(&mut b, 220);
    compare(&mut b, Reg::x(0), 0);
    let child = cond(&mut b, EQ);
    let failed = cond(&mut b, LT);
    b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
    b.ret();
    patch(&mut b, failed, LT);
    negate(&mut b, Reg::x(0), Reg::x(0));
    store(&mut b, B, ERROR, Reg::x(0));
    b.ret();
    patch(&mut b, child, EQ);
    load(&mut b, Reg::x(0), SP, 0);
    b.external_branch("b", &rt("fs_native_worker"));
    patch(&mut b, foreign, EQ);
    linux_foreign_start(&mut b);
    b.finish()
}

/// foreign 线程的 pthread 启动；attr 用 72 字节栈空间（glibc 56 字节 + 对齐）。
/// x1 仍是调用方的工作线程栈顶，栈底 = 顶 − BLOCKING_WORKER_STACK_BYTES。
fn linux_foreign_start(b: &mut Builder) {
    enter(b, 72);
    b.add(Reg::x(19), Reg::x(0), 0);
    b.add(Reg::x(20), Reg::x(1), 0);
    b.add(Reg::x(0), SP, 0);
    linux_call(b, "pthread_attr_init");
    compare(b, Reg::x(0), 0);
    let done = cond(b, NE);
    b.add(Reg::x(0), SP, 0);
    // 栈底 = 顶 − 64KiB；65536 = 1 << 16 走 Wide 移位，立即数车道放不进 imm12。
    b.emit(
        "movz.X",
        [
            reg(A),
            Operand::Wide {
                imm16: 1,
                shift: 16,
            },
        ],
    );
    b.emit(
        "sub.X.shift",
        [reg(Reg::x(1)), reg(Reg::x(20)), reg(A), unshifted()],
    );
    b.emit(
        "movz.X",
        [
            reg(Reg::x(2)),
            Operand::Wide {
                imm16: 1,
                shift: 16,
            },
        ],
    );
    linux_call(b, "pthread_attr_setstack");
    compare(b, Reg::x(0), 0);
    let destroy = cond(b, NE);
    b.add(Reg::x(0), Reg::x(19), THREAD_HANDLE);
    b.add(Reg::x(1), SP, 0);
    let worker = inst_target(&rt("foreign_native_worker"), 0);
    b.emit("adrp", [reg(Reg::x(2)), worker.clone()]);
    b.emit("add.X.imm", [reg(Reg::x(2)), reg(Reg::x(2)), worker]);
    b.add(Reg::x(3), Reg::x(19), 0);
    linux_call(b, "pthread_create");
    patch(b, destroy, NE);
    b.add(Reg::x(20), Reg::x(0), 0);
    b.add(Reg::x(0), SP, 0);
    linux_call(b, "pthread_attr_destroy");
    compare(b, Reg::x(0), 0);
    let failed = cond(b, NE);
    b.add(Reg::x(0), Reg::x(20), 0);
    patch(b, done, NE);
    store(b, Reg::x(19), ERROR, Reg::x(0));
    leave(b, 72);
    patch(b, failed, NE);
    fatal(b);
}

/// `fs_poll`：x0 = packet；op==−1 用 pthread_tryjoin_np，否则查 tid 字。
pub fn linux_fs_poll() -> Fragment {
    let mut b = Builder::default();
    load(&mut b, B, Reg::x(0), OPERATION);
    b.emit("adds.X.imm", [reg(XZR), reg(B), add_imm(1)]);
    let foreign = cond(&mut b, EQ);
    // tid 是内核 32 位字；提交前高 32 位已清零。
    load(&mut b, A, Reg::x(0), 0);
    compare(&mut b, A, 0);
    let pending = cond(&mut b, NE);
    b.emit("movz.X", [reg(Reg::x(0)), movz(1)]);
    b.ret();
    patch(&mut b, pending, NE);
    b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
    b.ret();
    patch(&mut b, foreign, EQ);
    linux_foreign_poll(&mut b);
    b.finish()
}

fn linux_foreign_poll(b: &mut Builder) {
    enter(b, 0);
    b.add(Reg::x(19), Reg::x(0), 0);
    load(b, Reg::x(0), Reg::x(19), THREAD_HANDLE);
    b.emit("movz.X", [reg(Reg::x(1)), movz(0)]);
    linux_call(b, "pthread_tryjoin_np");
    compare(b, Reg::x(0), 0);
    let pending = cond(b, NE);
    store(b, Reg::x(19), THREAD_HANDLE, XZR);
    b.emit("movz.X", [reg(Reg::x(0)), movz(1)]);
    leave(b, 0);
    patch(b, pending, NE);
    compare(b, Reg::x(0), 16); // EBUSY：线程或 TLS 析构仍在持 credit。
    let failed = cond(b, NE);
    b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
    leave(b, 0);
    patch(b, failed, NE);
    fatal(b);
}

/// Linux 致命错：exit_group(94) 以码 2 退出。
fn fatal(b: &mut Builder) {
    b.emit("movz.X", [reg(Reg::x(0)), movz(2)]);
    syscall(b, 94);
    b.emit("brk", [imm(0)]);
}

/// Linux C 调用统一经 GOT 桩符号。
fn linux_call(b: &mut Builder, name: &str) {
    b.external_branch("bl", &super::linux_import_stub_symbol(name));
}

/// `fs_start`：x0 = packet；pthread_create + detach 工作线程。
pub fn darwin_fs_start() -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(32)]);
    for (offset, saved) in [(0, Reg::x(19)), (16, Reg::x(29)), (24, LR)] {
        b.memory("str.X.Unsigned", saved, SP, offset);
    }
    b.add(Reg::x(29), SP, 16);
    b.add(Reg::x(19), Reg::x(0), 0);
    b.add(Reg::x(0), SP, 8);
    b.emit("movz.X", [reg(Reg::x(1)), movz(0)]);
    let worker = inst_target(&rt("fs_native_worker"), 0);
    b.emit("adrp", [reg(Reg::x(2)), worker.clone()]);
    b.emit("add.X.imm", [reg(Reg::x(2)), reg(Reg::x(2)), worker]);
    b.add(Reg::x(3), Reg::x(19), 0);
    darwin_call(&mut b, "pthread_create");
    compare(&mut b, Reg::x(0), 0);
    let done = cond(&mut b, NE);
    load(&mut b, Reg::x(0), SP, 8);
    darwin_call(&mut b, "pthread_detach");
    compare(&mut b, Reg::x(0), 0);
    let detached = cond(&mut b, EQ);
    b.emit("brk", [imm(0)]);
    patch(&mut b, detached, EQ);
    patch(&mut b, done, NE);
    for (offset, saved) in [(0, Reg::x(19)), (16, Reg::x(29)), (24, LR)] {
        b.memory("ldr.X.Unsigned", saved, SP, offset);
    }
    b.add(SP, SP, 32);
    b.ret();
    b.finish()
}

/// `fs_poll`：x0 = packet；返回 [THREAD_HANDLE] 完成标志。
pub fn darwin_fs_poll() -> Fragment {
    let mut b = Builder::default();
    load(&mut b, Reg::x(0), Reg::x(0), THREAD_HANDLE);
    b.ret();
    b.finish()
}

fn darwin_call(b: &mut Builder, name: &str) {
    b.external_branch("bl", &import_stub_symbol(name));
}

// ---- 时钟 / 处理器数 ----

/// `clock_id`：单调源 7（CLOCK_BOOTTIME，含挂起），墙钟 1（CLOCK_REALTIME）。
fn linux_clock(clock_id: u16) -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
    b.emit("movz.X", [reg(Reg::x(0)), movz(clock_id)]);
    b.add(Reg::x(1), SP, 0);
    syscall(&mut b, 113);
    compare(&mut b, Reg::x(0), 0);
    let failed = cond(&mut b, NE);
    load(&mut b, Reg::x(0), SP, 0);
    load(&mut b, C, SP, 8);
    b.emit("movz.W", [reg(Reg::w(10)), movz(0xca00)]);
    b.emit(
        "movk.W",
        [
            reg(Reg::w(10)),
            Operand::Wide {
                imm16: 0x3b9a,
                shift: 16,
            },
        ],
    );
    b.emit(
        "madd.X",
        [reg(Reg::x(0)), reg(Reg::x(0)), reg(Reg::x(10)), reg(C)],
    );
    b.add(SP, SP, 16);
    b.ret();
    patch(&mut b, failed, NE);
    fatal(&mut b);
    b.finish()
}

pub fn linux_monotonic_nanos() -> Fragment {
    linux_clock(7)
}

pub fn linux_wall_nanos() -> Fragment {
    linux_clock(1)
}

/// Darwin `clock_gettime_nsec_np(id)` 直接返回纳秒；单调 6（MONOTONIC），墙钟 0。
fn darwin_clock(clock_id: u16) -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
    b.memory("str.X.Unsigned", LR, SP, 0);
    b.emit("movz.X", [reg(Reg::x(0)), movz(clock_id)]);
    darwin_call(&mut b, "clock_gettime_nsec_np");
    b.memory("ldr.X.Unsigned", LR, SP, 0);
    b.add(SP, SP, 16);
    b.ret();
    b.finish()
}

pub fn darwin_monotonic_nanos() -> Fragment {
    darwin_clock(6)
}

pub fn darwin_wall_nanos() -> Fragment {
    darwin_clock(0)
}

/// `sched_getaffinity(0, 8, mask)` 位计数；失败返回 0，调用方回落到 1。
pub fn linux_parallelism_probe() -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
    b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
    b.emit("movz.X", [reg(Reg::x(1)), movz(8)]);
    b.add(Reg::x(2), SP, 0);
    syscall(&mut b, 123);
    compare(&mut b, Reg::x(0), 0);
    let failed = cond(&mut b, LT);
    load(&mut b, B, SP, 0);
    b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
    let bits = b.offset();
    b.emit("ands.X.imm", [reg(XZR), reg(B), Operand::Bits(1)]);
    let clear = cond(&mut b, EQ);
    b.emit("add.X.imm", [reg(Reg::x(0)), reg(Reg::x(0)), add_imm(1)]);
    patch(&mut b, clear, EQ);
    b.emit("ubfm.X", [reg(B), reg(B), imm(1), imm(63)]);
    compare_regs(&mut b, B, XZR);
    let zero = cond(&mut b, EQ);
    b.branch("b", &[], bits);
    patch(&mut b, zero, EQ);
    let done = jump(&mut b);
    patch(&mut b, failed, LT);
    b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
    link(&mut b, done);
    b.add(SP, SP, 16);
    b.ret();
    b.finish()
}

/// `sysconf(_SC_NPROCESSORS_ONLN)`；< 1 时按 x64 口径回落 1。
pub fn darwin_parallelism_probe() -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
    b.memory("str.X.Unsigned", LR, SP, 0);
    b.emit("movz.X", [reg(Reg::x(0)), movz(58)]);
    darwin_call(&mut b, "sysconf");
    compare(&mut b, Reg::x(0), 1);
    let valid = cond(&mut b, GE);
    b.emit("movz.X", [reg(Reg::x(0)), movz(1)]);
    patch(&mut b, valid, GE);
    b.memory("ldr.X.Unsigned", LR, SP, 0);
    b.add(SP, SP, 16);
    b.ret();
    b.finish()
}

// ---- startup_call / environment_symbol ----

/// `startup_call`：x0 = 操作，x1 = packet（[0]=op 镜像、[8]=缓冲、[16]=长度、[72]=value、[80]=error）。
/// op0 = getcwd；linux op1 报告 95（ENOTSUP→ENOSYS），darwin op1 报告 45。
pub fn linux_startup_call() -> Fragment {
    let mut b = Builder::default();
    let done = startup_prelude(&mut b, 95);
    load(&mut b, Reg::x(0), Reg::x(1), 8);
    load(&mut b, Reg::x(1), Reg::x(1), 16);
    syscall(&mut b, 17);
    compare(&mut b, Reg::x(0), 0);
    let success = cond(&mut b, GE);
    negate(&mut b, Reg::x(0), Reg::x(0));
    store(&mut b, Reg::x(1), 80, Reg::x(0));
    let end = jump(&mut b);
    patch(&mut b, success, GE);
    store(&mut b, Reg::x(1), 72, Reg::x(0));
    link(&mut b, end);
    startup_epilogue(&mut b, done);
    b.finish()
}

pub fn darwin_startup_call() -> Fragment {
    let mut b = Builder::default();
    let done = startup_prelude(&mut b, 45);
    b.memory("str.X.Unsigned", LR, SP, 0);
    load(&mut b, Reg::x(0), Reg::x(1), 8);
    load(&mut b, Reg::x(1), Reg::x(1), 16);
    darwin_call(&mut b, "getcwd");
    b.memory("ldr.X.Unsigned", LR, SP, 0);
    // getcwd 成功返回 buf 指针，失败返回 NULL 并置 errno。
    compare(&mut b, Reg::x(0), 0);
    let failed = cond(&mut b, EQ);
    store(&mut b, Reg::x(1), 72, Reg::x(0));
    let end = jump(&mut b);
    patch(&mut b, failed, EQ);
    darwin_errno(&mut b);
    store(&mut b, Reg::x(1), 80, Reg::x(0));
    link(&mut b, end);
    startup_epilogue(&mut b, done);
    b.finish()
}

/// 校验 op 镜像并清零 value/error；op0 落到调用方位置，op1 走 `done` 出口。
fn startup_prelude(b: &mut Builder, unsupported: u16) -> u32 {
    load(b, A, Reg::x(1), 0);
    compare_regs(b, Reg::x(0), A);
    let valid = cond(b, EQ);
    b.emit("brk", [imm(0)]);
    patch(b, valid, EQ);
    store(b, Reg::x(1), 72, XZR);
    store(b, Reg::x(1), 80, XZR);
    compare(b, Reg::x(0), 0);
    let cwd = cond(b, EQ);
    compare(b, Reg::x(0), 1);
    let known = cond(b, EQ);
    b.emit("brk", [imm(0)]);
    patch(b, known, EQ);
    b.emit("movz.X", [reg(A), movz(unsupported)]);
    store(b, Reg::x(1), 80, A);
    let done = jump(b);
    patch(b, cwd, EQ);
    done
}

/// 汇合：返回 packet[80]。
fn startup_epilogue(b: &mut Builder, done: u32) {
    load(b, Reg::x(0), Reg::x(1), 80);
    b.ret();
    link(b, done);
    load(b, Reg::x(0), Reg::x(1), 80);
    b.ret();
}

fn darwin_errno(b: &mut Builder) {
    b.memory("str.X.Unsigned", LR, SP, 0);
    darwin_call(b, "__error");
    b.memory("ldr.X.Unsigned", LR, SP, 0);
    b.memory("ldr.W.Unsigned", Reg::w(0), Reg::x(0), 0);
}

/// `environment_symbol`：x0 = 操作号，返回该环境原语的符号地址。
/// linux：op0 → GOT 槽中的 `environ` 值；op1..3 → setenv/unsetenv/getenv 桩。
/// darwin：op0 → `_NSGetEnviron()` 调用结果；op1..3 → setenv/unsetenv/getenv 桩地址。
pub fn linux_environment_symbol() -> Fragment {
    let mut b = Builder::default();
    for (op, name) in ["environ", "setenv", "unsetenv", "getenv"]
        .iter()
        .enumerate()
    {
        compare(&mut b, Reg::x(0), op);
        let next_op = cond(&mut b, NE);
        let name = if op == 0 {
            super::linux_import_slot_symbol(name)
        } else {
            super::linux_import_stub_symbol(name)
        };
        let target = inst_target(&name, 0);
        b.emit("adrp", [reg(Reg::x(0)), target.clone()]);
        b.emit("add.X.imm", [reg(Reg::x(0)), reg(Reg::x(0)), target]);
        if op == 0 {
            b.memory("ldr.X.Unsigned", Reg::x(0), Reg::x(0), 0);
        }
        b.ret();
        patch(&mut b, next_op, NE);
    }
    b.emit("brk", [imm(0)]);
    b.finish()
}

pub fn darwin_environment_symbol() -> Fragment {
    let mut b = Builder::default();
    for (op, name) in ["_NSGetEnviron", "setenv", "unsetenv", "getenv"]
        .iter()
        .enumerate()
    {
        compare(&mut b, Reg::x(0), op);
        let next_op = cond(&mut b, NE);
        if op == 0 {
            b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
            b.memory("str.X.Unsigned", LR, SP, 0);
            darwin_call(&mut b, name);
            b.memory("ldr.X.Unsigned", LR, SP, 0);
            b.add(SP, SP, 16);
        } else {
            let stub = inst_target(&import_stub_symbol(name), 0);
            b.emit("adrp", [reg(Reg::x(0)), stub.clone()]);
            b.emit("add.X.imm", [reg(Reg::x(0)), reg(Reg::x(0)), stub]);
        }
        b.ret();
        patch(&mut b, next_op, NE);
    }
    b.emit("brk", [imm(0)]);
    b.finish()
}

/// `net_close`：C ABI close(fd) → 0 / errno；linux 直接 syscall，darwin 经 __error。
pub fn linux_net_close() -> Fragment {
    let mut b = Builder::default();
    syscall(&mut b, 57);
    negate(&mut b, Reg::x(0), Reg::x(0));
    b.ret();
    b.finish()
}

pub fn darwin_net_close() -> Fragment {
    let mut b = Builder::default();
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
    b.memory("str.X.Unsigned", LR, SP, 0);
    darwin_call(&mut b, "close");
    compare(&mut b, Reg::x(0), 0);
    let done = cond(&mut b, EQ);
    darwin_errno(&mut b);
    patch(&mut b, done, EQ);
    b.memory("ldr.X.Unsigned", LR, SP, 0);
    b.add(SP, SP, 16);
    b.ret();
    b.finish()
}

// ---- 栈原语 ----

/// `stack_read_word`：x0 = 深度；返回协程栈锚 [HOT+80] − depth 处的字。
/// `__gugu_cage_control` 返回 cage 控制记录地址；不依赖 managed 寄存器。
pub fn cage_control() -> Fragment {
    let mut b = Builder::default();
    let target = inst_target(crate::a64::program::CAGE_CONTROL, 0);
    b.emit("adrp", [reg(Reg::x(0)), target.clone()]);
    b.emit("add.X.imm", [reg(Reg::x(0)), reg(Reg::x(0)), target]);
    b.ret();
    b.finish()
}

pub fn stack_read_word() -> Fragment {
    let mut b = Builder::default();
    load(&mut b, A, HOT, 80);
    b.emit("sub.X.shift", [reg(A), reg(A), reg(Reg::x(0)), unshifted()]);
    load(&mut b, Reg::x(0), A, 0);
    b.ret();
    b.finish()
}

/// `stack_copy_raw`：x0 = 深度、x1 = 指针、x2 = 字节数、x3 = 方向；内联字节拷贝。
/// 方向 0：copy(dst=x1, src=锚)；非零：copy(dst=锚, src=x1)。dst/src 固定到 B/A 两车道。
pub fn stack_copy_raw() -> Fragment {
    let mut b = Builder::default();
    load(&mut b, A, HOT, 80);
    b.emit("sub.X.shift", [reg(A), reg(A), reg(Reg::x(0)), unshifted()]);
    compare(&mut b, Reg::x(3), 0);
    let to_stack = cond(&mut b, NE);
    // 方向 0：src=A=锚 已在位，dst=B=x1。
    b.add(B, Reg::x(1), 0);
    let copy = jump(&mut b);
    patch(&mut b, to_stack, NE);
    // 方向非零：dst=B=锚，src=A=x1。
    b.add(B, A, 0);
    b.add(A, Reg::x(1), 0);
    link(&mut b, copy);
    // len 在 C 车道；ldrb/strb Post(1) 递减循环，len==0 直接返回。
    b.add(C, Reg::x(2), 0);
    let body = b.offset();
    b.emit("cbz.X", [reg(C), next()]);
    let done = b.offset();
    b.emit(
        "ldrb.W.Post",
        [reg(Reg::w(12)), mem(A, AddressMode::Post(1))],
    );
    b.emit(
        "strb.W.Post",
        [reg(Reg::w(12)), mem(B, AddressMode::Post(1))],
    );
    b.emit("sub.X.imm", [reg(C), reg(C), add_imm(1)]);
    b.branch("b", &[], body);
    patch(&mut b, done, EQ);
    b.ret();
    b.finish()
}

/// 新增片段声明的原生导入：Darwin 全经 libSystem；Linux 经 libc GOT 桩（见 LINUX_C_IMPORTS）。
pub fn native_imports(platform: Platform) -> Vec<NativeImport> {
    match platform {
        Platform::Linux => Vec::new(),
        Platform::Darwin => [
            "pthread_create",
            "pthread_detach",
            "clock_gettime_nsec_np",
            "sysconf",
            "getcwd",
            "_NSGetEnviron",
            "setenv",
            "unsetenv",
            "getenv",
            "close",
            "__error",
        ]
        .map(|name| NativeImport::function(LIB_SYSTEM, name))
        .into(),
    }
}
