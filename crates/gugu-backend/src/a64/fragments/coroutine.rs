use std::mem::{offset_of, size_of};

use crate::a64::reg::{Reg, Width};
use crate::runtime::processor::{PROCESSOR_STATE_OFFSET, PROCESSOR_SYSTEM_SLOT_OFFSET};

use super::{Builder, Fragment, HOT, LR, PROCESSOR, SP, reg, unshifted};

pub use crate::runtime::coroutine::{
    A64Context as Context, A64MorestackMailbox as MorestackMailbox,
    A64MorestackScratch as MorestackScratch,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCode {
    pub fragment: Fragment,
    /// 单向恢复入口；换栈入口位于 0。
    pub restore_offset: u32,
}

/// switch 接收 x0=save, x1=restore, x2=hot, x3=processor；
/// restore 接收 x0=restore, x1=hot, x2=processor。调用点先完成 root spill。
pub fn context_switch() -> ContextCode {
    let mut b = Builder::default();
    let save = Reg::x(0);
    let restore = Reg::x(1);
    let temp = Reg::x(9);
    b.add(temp, SP, 0);
    b.memory("str.X.Unsigned", temp, save, offset_of!(Context, sp));
    b.memory("str.X.Unsigned", LR, save, offset_of!(Context, pc));
    transfer_context(&mut b, save, "str.X.Unsigned", "str.D.Unsigned");
    let restore_body = b.offset();
    b.add(HOT, Reg::x(2), 0);
    b.add(PROCESSOR, Reg::x(3), 0);
    transfer_context(&mut b, restore, "ldr.X.Unsigned", "ldr.D.Unsigned");
    b.memory("ldr.X.Unsigned", temp, restore, offset_of!(Context, sp));
    b.memory("ldr.X.Unsigned", LR, restore, offset_of!(Context, pc));
    b.add(SP, temp, 0);
    b.ret();
    let restore_offset = b.offset();
    b.add(Reg::x(3), Reg::x(2), 0);
    b.add(Reg::x(2), Reg::x(1), 0);
    b.add(restore, Reg::x(0), 0);
    b.branch("b", &[], restore_body);
    ContextCode {
        fragment: b.finish(),
        restore_offset,
    }
}

fn transfer_context(b: &mut Builder, base: Reg, gpr: &str, fp: &str) {
    for lane in 0..8 {
        let offset = usize::from(lane) * size_of::<u64>();
        b.memory(
            gpr,
            Reg::x(19 + lane),
            base,
            offset_of!(Context, x19_to_x26) + offset,
        );
        b.memory(
            fp,
            Reg::v(8 + lane, Width::D),
            base,
            offset_of!(Context, d8_to_d15) + offset,
        );
    }
    b.memory(gpr, Reg::x(29), base, offset_of!(Context, x29));
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MorestackCode {
    pub fragment: Fragment,
    /// 系统侧恢复到此处，重新加载入口寄存器。
    pub resume_offset: u32,
}

/// prologue 用 BL 进入；x10 保存 prologue 原始 LR，x11 是候选低水位。
/// 返回到 prologue 后由其从 x10 恢复原始 LR，并重新检查入口。
pub fn morestack_or_poll(context_switch: &str) -> MorestackCode {
    let mut b = Builder::default();
    let mailbox = Reg::x(12);
    let scratch = Reg::x(13);
    let candidate = Reg::x(11);
    let temp = Reg::x(14);
    b.memory("ldr.X.Unsigned", mailbox, PROCESSOR, PROCESSOR_STATE_OFFSET);
    b.add(temp, SP, 0);
    for (value, field) in [
        (HOT, offset_of!(MorestackMailbox, hot)),
        (temp, offset_of!(MorestackMailbox, entry_sp)),
        (candidate, offset_of!(MorestackMailbox, candidate)),
    ] {
        b.memory("str.X.Unsigned", value, mailbox, field);
    }
    b.emit(
        "sub.X.shift",
        [reg(temp), reg(temp), reg(candidate), unshifted()],
    );
    b.memory(
        "str.X.Unsigned",
        temp,
        mailbox,
        offset_of!(MorestackMailbox, required),
    );
    transfer_scratch(&mut b, mailbox, scratch, "str.X.Unsigned", "str.Q.Unsigned");
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(0),
        mailbox,
        offset_of!(MorestackMailbox, mutator_context),
    );
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(1),
        mailbox,
        offset_of!(MorestackMailbox, system_context),
    );
    b.memory(
        "ldr.X.Unsigned",
        Reg::x(2),
        PROCESSOR,
        PROCESSOR_SYSTEM_SLOT_OFFSET,
    );
    b.add(Reg::x(3), PROCESSOR, 0);
    b.external_branch("bl", context_switch);
    let resume_offset = b.offset();
    b.memory("ldr.X.Unsigned", mailbox, PROCESSOR, PROCESSOR_STATE_OFFSET);
    transfer_scratch(&mut b, mailbox, scratch, "ldr.X.Unsigned", "ldr.Q.Unsigned");
    b.ret();
    MorestackCode {
        fragment: b.finish(),
        resume_offset,
    }
}

fn transfer_scratch(b: &mut Builder, mailbox: Reg, scratch: Reg, gpr: &str, vector: &str) {
    b.memory(
        "ldr.X.Unsigned",
        scratch,
        mailbox,
        offset_of!(MorestackMailbox, scratch),
    );
    b.memory(gpr, LR, scratch, offset_of!(MorestackScratch, return_pc));
    b.memory(
        gpr,
        Reg::x(10),
        scratch,
        offset_of!(MorestackScratch, caller_lr),
    );
    for lane in 0..9 {
        let offset = offset_of!(MorestackScratch, gpr) + usize::from(lane) * 8;
        b.memory(gpr, Reg::x(lane), scratch, offset);
    }
    for lane in 0..8 {
        let offset = offset_of!(MorestackScratch, vector) + usize::from(lane) * 16;
        b.memory(vector, Reg::v(lane, Width::Q), scratch, offset);
    }
}
