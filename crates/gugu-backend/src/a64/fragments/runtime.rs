//! 哨兵与调度机器片段；与 `Definition` 的一符号一入口模型配套的胶水代码。
use std::mem::offset_of;

use crate::a64::reg::{Reg, Width};
use crate::a64::{foreign::Platform, macho::import_stub_symbol};
use crate::runtime::collect_layout::{
    COORDINATOR_ENTRY_RSP_OFFSET, COORDINATOR_SAVED_R14_OFFSET, COORDINATOR_SAVED_RSP_OFFSET,
    COORDINATOR_SYSTEM_DESCRIPTOR_OFFSET, COORDINATOR_SYSTEM_TOP_OFFSET,
};
use crate::runtime::coroutine::A64MorestackMailbox as MorestackMailbox;
use crate::runtime::coroutine_pack::{
    COROUTINE_PACK_FLOAT_LANES, COROUTINE_PACK_INTEGER_LANES, coroutine_pack_float_offset,
    coroutine_pack_integer_offset,
};
use crate::runtime::entry::state;
use crate::runtime::processor::{PROCESSOR_STATE_OFFSET, PROCESSOR_SYSTEM_SLOT_OFFSET};

use super::linux_syscall::syscall;
use super::{
    AddressMode, Builder, Fragment, HOT, LR, PROCESSOR, SP, add_imm, condition, imm, mem, movz,
    next, reg,
};

/// main/协程返回登记的调度原因：2 = 协程返回（与 x64 哨兵同值）。
const REASON_COROUTINE_RETURN: u16 = 2;

fn mailbox(b: &mut Builder, scratch: Reg) {
    b.memory("ldr.X.Unsigned", scratch, PROCESSOR, PROCESSOR_STATE_OFFSET);
}

/// 写完邮箱后经 `context_switch + restore_offset` 恢复系统上下文；
/// restore 入口契约 x0=restore, x1=hot, x2=processor。
fn restore_system(b: &mut Builder, mailbox_reg: Reg, switch: &str, restore_offset: u32) {
    b.emit("movz.X", [reg(Reg::x(10)), movz(REASON_COROUTINE_RETURN)]);
    b.memory(
        "str.X.Unsigned",
        Reg::x(10),
        mailbox_reg,
        state::REASON as usize,
    );
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(0),
        mailbox_reg,
        offset_of!(MorestackMailbox, system_context),
    );
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(1),
        PROCESSOR,
        PROCESSOR_SYSTEM_SLOT_OFFSET,
    );
    b.add(Reg::x(2), PROCESSOR, 0);
    b.external_branch_at("b", switch, i64::from(restore_offset));
}

/// 单向恢复跳板；x0=restore, x1=hot, x2=processor，由调用方按 ABI 备好。
pub fn context_restore(context_switch: &str, restore_offset: u32) -> Fragment {
    let mut b = Builder::default();
    b.external_branch_at("b", context_switch, i64::from(restore_offset));
    b.finish()
}

/// x27 = CoroutineSlot*；返回当前槽地址。
pub fn current_slot() -> Fragment {
    let mut b = Builder::default();
    b.add(Reg::x(0), HOT, 0);
    b.ret();
    b.finish()
}

/// x0 = 退出码，不返回。Linux 走 exit_group syscall，Darwin 尾调用 libSystem exit。
pub fn process_exit(platform: Platform) -> Fragment {
    let mut b = Builder::default();
    match platform {
        Platform::Linux => {
            syscall(&mut b, 94);
            b.emit("brk", [imm(0)]);
        }
        Platform::Darwin => {
            b.external_branch("b", &import_stub_symbol("exit"));
        }
    }
    b.finish()
}

/// panic 入口（invoke 调用点）：x0-x5 = panic 记录六车道，LR = 原调用返回 PC
/// （即 CallReturn safepoint pc）。装 pc=x6、state=x7 后 `bl panic_dispatch`
/// → x0 = landing PC，`br x0` 交给调用方帧内 landing。a64 `bl` 不压栈，
/// 没有 x64 片段的返回槽搬运。
pub fn panic(panic_dispatch: &str) -> Fragment {
    let mut b = Builder::default();
    b.add(Reg::x(6), LR, 0);
    mailbox(&mut b, Reg::x(7));
    b.external_branch("bl", panic_dispatch);
    b.emit("br", [reg(Reg::x(0))]);
    b.finish()
}

/// panic 挂起恢复（4c.2 invoke/unwind）：校验 state+104 挂起标记后按
/// `panic_unwind(message, len, path, path_len, line, column, state)` 的内部整数
/// 通道装参并 bl 过去；标记无效时 `process_exit(2)`，与 x64 panic_resume 片段同契约。
pub fn panic_resume(panic_unwind: &str, process_exit: &str) -> Fragment {
    const NE: u8 = 1;
    let mut b = Builder::default();
    mailbox(&mut b, Reg::x(9));
    b.memory("ldr.W.Unsigned", Reg::w(10), Reg::x(9), 104);
    b.emit(
        "subs.W.imm",
        [reg(Reg::zr(Width::W)), reg(Reg::w(10)), add_imm(1)],
    );
    let invalid = b.offset();
    b.emit("b.cond", [next(), condition(NE)]);
    for index in 0..6u8 {
        b.memory(
            "ldr.X.Unsigned",
            Reg::x(index),
            Reg::x(9),
            state::PANIC_MESSAGE as usize + usize::from(index) * 8,
        );
    }
    b.add(Reg::x(6), Reg::x(9), 0);
    b.external_branch("bl", panic_unwind);
    b.patch_branch(invalid, "b.cond", &[], &[condition(NE)]);
    b.emit("movz.X", [reg(Reg::x(0)), movz(2)]);
    b.external_branch("b", process_exit);
    b.finish()
}

/// x0 = fd, x1 = buf, x2 = len → x0 = 已写字节数。
pub fn report_write(platform: Platform) -> Fragment {
    let mut b = Builder::default();
    match platform {
        Platform::Linux => {
            syscall(&mut b, 64);
            b.ret();
        }
        Platform::Darwin => {
            b.external_branch("b", &import_stub_symbol("write"));
        }
    }
    b.finish()
}

/// x0 = main 返回值；登记邮箱后恢复系统上下文，不返回。
pub fn main_return(context_switch: &str, restore_offset: u32) -> Fragment {
    let mut b = Builder::default();
    let scratch = Reg::x(9);
    mailbox(&mut b, scratch);
    b.memory(
        "str.X.Unsigned",
        Reg::x(0),
        scratch,
        offset_of!(MorestackMailbox, return_value),
    );
    b.memory(
        "str.X.Unsigned",
        PROCESSOR,
        scratch,
        offset_of!(MorestackMailbox, processor),
    );
    restore_system(&mut b, scratch, context_switch, restore_offset);
    b.finish()
}

/// 栈上入参 `[sp] = code, [sp+8] = pack`；pack 九条整数道入 x0-x8，八条浮点道入 v0-v7。
/// 协程返回后 x0 是完成值，登记邮箱并恢复系统上下文。
pub fn coroutine_enter(context_switch: &str, restore_offset: u32) -> Fragment {
    let mut b = Builder::default();
    let code = Reg::x(9);
    let pack = Reg::x(10);
    b.memory("ldr.X.Unsigned", code, SP, 0);
    b.memory("ldr.X.Unsigned", pack, SP, 8);
    for lane in 0..COROUTINE_PACK_FLOAT_LANES {
        // pack 浮点道基址 72 只按 8 对齐，Unsigned 缩放编码放不下，用 Unscaled。
        b.emit(
            "ldr.Q.Unscaled",
            [
                reg(Reg::v(lane as u8, Width::Q)),
                mem(
                    pack,
                    AddressMode::Unscaled(coroutine_pack_float_offset(lane) as i64),
                ),
            ],
        );
    }
    for lane in 0..COROUTINE_PACK_INTEGER_LANES {
        b.memory(
            "ldr.X.Unsigned",
            Reg::x(lane as u8),
            pack,
            coroutine_pack_integer_offset(lane),
        );
    }
    b.emit("blr", [reg(code)]);
    let scratch = Reg::x(9);
    mailbox(&mut b, scratch);
    b.memory(
        "str.X.Unsigned",
        Reg::x(0),
        scratch,
        offset_of!(MorestackMailbox, return_value),
    );
    b.memory(
        "str.X.Unsigned",
        PROCESSOR,
        scratch,
        offset_of!(MorestackMailbox, processor),
    );
    b.add(Reg::x(10), SP, 0);
    b.memory(
        "str.X.Unsigned",
        Reg::x(10),
        scratch,
        offset_of!(MorestackMailbox, candidate),
    );
    restore_system(&mut b, scratch, context_switch, restore_offset);
    b.finish()
}

/// x0 = GC 请求；切到系统栈调用 `gc_dispatch(request, entry_rsp)`，恢复后返回 x0。
/// 切换面与 x64 `gc_enter` 相同：HOT 存 coordinator 槽，SP 走 ENTRY_RSP/SAVED_RSP。
/// a64 的 `bl` 不落栈：入口在 mutator 栈上开辟 16 字节槽并把 LR 写到高 8 字节，
/// `entry_rsp` 取 LR 所在地址（SP+8），使 poll 慢路的 `heap_word(entry_rsp)`/
/// `entry_rsp+8` 与 x64 `call` 压栈后的布局一致；返回前再弹回 LR。
pub fn gc_enter(gc_dispatch: &str) -> Fragment {
    let mut b = Builder::default();
    let coordinator = Reg::x(9);
    let temp = Reg::x(10);
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
    b.memory("str.X.Unsigned", LR, SP, 8);
    mailbox(&mut b, coordinator);
    b.memory(
        "ldr.X.Unsigned",
        coordinator,
        coordinator,
        state::COORDINATOR as usize,
    );
    b.memory(
        "str.X.Unsigned",
        HOT,
        coordinator,
        COORDINATOR_SAVED_R14_OFFSET as usize,
    );
    b.memory(
        "ldr.X.Unsigned",
        HOT,
        coordinator,
        COORDINATOR_SYSTEM_DESCRIPTOR_OFFSET as usize,
    );
    b.add(temp, SP, 8);
    b.memory(
        "str.X.Unsigned",
        temp,
        coordinator,
        COORDINATOR_ENTRY_RSP_OFFSET as usize,
    );
    b.memory(
        "str.X.Unsigned",
        temp,
        coordinator,
        COORDINATOR_SAVED_RSP_OFFSET as usize,
    );
    b.memory(
        "ldr.X.Unsigned",
        temp,
        coordinator,
        COORDINATOR_SYSTEM_TOP_OFFSET as usize,
    );
    b.add(SP, temp, 0);
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(1),
        coordinator,
        COORDINATOR_ENTRY_RSP_OFFSET as usize,
    );
    b.external_branch("bl", gc_dispatch);
    mailbox(&mut b, coordinator);
    b.memory(
        "ldr.X.Unsigned",
        coordinator,
        coordinator,
        state::COORDINATOR as usize,
    );
    b.memory(
        "ldr.X.Unsigned",
        HOT,
        coordinator,
        COORDINATOR_SAVED_R14_OFFSET as usize,
    );
    b.memory(
        "ldr.X.Unsigned",
        temp,
        coordinator,
        COORDINATOR_SAVED_RSP_OFFSET as usize,
    );
    // SP 必须保持 16 字节对齐：用 temp（=SAVED_RSP=entry_rsp）作基址取回 LR，
    // 再把 SP 一步恢复到 entry_rsp+8（即调用方帧顶）。
    b.memory("ldr.X.Unsigned", LR, temp, 0);
    b.add(SP, temp, 8);
    b.ret();
    b.finish()
}

/// 返回 `gc_config_record` 数据定义的地址；数据内容见 `GcImageConfig::canonical_bytes`。
pub fn gc_config(record: &str) -> Fragment {
    let mut b = Builder::default();
    let target = super::inst_target(record, 0);
    b.emit("adrp", [reg(Reg::x(0)), target.clone()]);
    b.emit("add.X.imm", [reg(Reg::x(0)), reg(Reg::x(0)), target]);
    b.ret();
    b.finish()
}

/// `runtime_poll_defer`：无法在现场处理时推迟到下一个 safepoint；片段侧直接返回。
pub fn runtime_poll_defer() -> Fragment {
    let mut b = Builder::default();
    b.ret();
    b.finish()
}

/// → x0 = 当前 OS 线程 id；失败返回 0。Linux 走 gettid syscall，
/// Darwin 调 `pthread_threadid_np(NULL, &tid)`。
pub fn thread_id(platform: Platform) -> Fragment {
    let mut b = Builder::default();
    match platform {
        Platform::Linux => {
            syscall(&mut b, 178);
            b.ret();
        }
        Platform::Darwin => {
            b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(16)]);
            b.memory("str.X.Unsigned", LR, SP, 8);
            b.emit("movz.X", [reg(Reg::x(0)), movz(0)]);
            b.add(Reg::x(1), SP, 0);
            b.external_branch("bl", &import_stub_symbol("pthread_threadid_np"));
            b.memory("ldr.X.Unsigned", LR, SP, 8);
            b.memory("ldr.X.Unsigned", Reg::x(0), SP, 0);
            b.emit("add.X.imm", [reg(SP), reg(SP), add_imm(16)]);
            b.ret();
        }
    }
    b.finish()
}
