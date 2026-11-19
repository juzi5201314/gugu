use std::mem::offset_of;

use crate::runtime::entry::{Rt0Params, Rt0Payload, rt0_params};

use super::{Builder, Fragment, HOT, LR, PROCESSOR, SP, add_imm, imm, reg, unshifted};
use crate::a64::inst::{Operand, Target};
use crate::a64::reg::Reg;

pub(crate) fn darwin_main(params: &str, rt0: &str, exit: &str) -> Fragment {
    let mut b = Builder::default();
    b.add(Reg::x(4), Reg::x(1), 0);
    b.emit("add.W.imm", [reg(Reg::w(3)), reg(Reg::w(0)), add_imm(0)]);
    let params_addr = reg(Reg::x(0));
    let target = Operand::Target(Target::Symbol {
        name: params.into(),
        addend: 0,
    });
    b.emit("adrp", [params_addr.clone(), target.clone()]);
    b.emit("add.X.imm", [params_addr.clone(), params_addr, target]);
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
    b.external_branch("bl", rt0);
    b.external_branch("bl", exit);
    b.emit("brk", [imm(0)]);
    b.finish()
}

/// x0–x4 = params、bias、envp、argc、argv；由 OS 入口先完成加载修正。
/// 此片段调用 `rt0_start`，并以 x0 中的退出状态返回给 OS 入口。
pub fn rt0(rt0_start: &str) -> Fragment {
    let mut b = Builder::default();
    let params = Reg::x(0);
    let bias = Reg::x(1);
    let payload = Reg::x(9);
    b.emit("sub.X.imm", [reg(SP), reg(SP), add_imm(32)]);
    let host_saved = [HOT, PROCESSOR, Reg::x(29), LR];
    for (offset, register) in host_saved.into_iter().enumerate() {
        b.memory("str.X.Unsigned", register, SP, offset * 8);
    }
    b.add(Reg::x(29), SP, 16);
    for (register, field) in [
        (payload, offset_of!(Rt0Params, payload_va)),
        (HOT, offset_of!(Rt0Params, initial_slot)),
        (PROCESSOR, offset_of!(Rt0Params, processor)),
    ] {
        b.memory("ldr.X.Unsigned", register, params, field);
        b.emit(
            "add.X.shift",
            [reg(register), reg(register), reg(bias), unshifted()],
        );
    }
    for (register, field) in [
        (Reg::x(2), offset_of!(Rt0Payload, boot_environment)),
        (Reg::x(3), offset_of!(Rt0Payload, boot_argc)),
        (Reg::x(4), offset_of!(Rt0Payload, boot_argv)),
    ] {
        b.memory("str.X.Unsigned", register, payload, field);
    }
    b.external_branch("bl", rt0_start);
    for (offset, register) in host_saved.into_iter().enumerate() {
        b.memory("ldr.X.Unsigned", register, SP, offset * 8);
    }
    b.add(SP, SP, 32);
    b.ret();
    b.finish()
}
