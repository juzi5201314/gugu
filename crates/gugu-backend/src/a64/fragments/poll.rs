use std::mem::offset_of;

use crate::a64::{
    inst::{AddressMode, Operand},
    reg::Reg,
};
use crate::runtime::{
    callback::POLL_BIT,
    foreign_schema::{
        RUNTIME_MAPPING_LEDGER_OFFSET, RUNTIME_PRESSURE_OFFSET, RuntimeMappingLedger,
        RuntimePressureState,
    },
    processor::{PROCESSOR_GC_REQUEST_OFFSET, poll_flags_offset},
};

use super::{
    Builder, Fragment, PROCESSOR, add_imm, condition, imm, mem, movz, next, reg, unshifted,
};

/// 保留 mutator 的 SP 和 LR；callback/GC 入口负责切栈。
pub fn safepoint_slow(callback_poll: &str, gc_enter: &str) -> Fragment {
    let mut b = Builder::default();
    let flags = Reg::x(9);
    let value = Reg::w(10);
    let status = Reg::w(11);
    b.add(flags, PROCESSOR, poll_flags_offset() as usize);
    let retry = b.offset();
    b.emit("ldaxr.W", [reg(value), mem(flags, AddressMode::Base)]);
    let callback = b.offset();
    let test = [reg(value), imm(i64::from(POLL_BIT.trailing_zeros()))];
    b.emit("tbnz.W", [test[0].clone(), test[1].clone(), next()]);
    b.emit(
        "and.W.imm",
        [reg(value), reg(value), Operand::Bits(u64::from(!1_u32))],
    );
    b.emit(
        "stlxr.W",
        [reg(status), reg(value), mem(flags, AddressMode::Base)],
    );
    b.branch("cbnz.W", &[reg(status)], retry);
    pressure_dispatch(&mut b, gc_enter);
    b.patch_branch(callback, "tbnz.W", &test, &[]);
    b.emit("clrex", [imm(15)]);
    b.external_branch("b", callback_poll);
    b.finish()
}

fn pressure_dispatch(b: &mut Builder, gc_enter: &str) {
    let tmp = Reg::x(10);
    let zero = Reg::zr(crate::a64::reg::Width::X);
    let mut done = Vec::new();
    for offset in [
        offset_of!(RuntimePressureState, state),
        offset_of!(RuntimePressureState, ready),
    ] {
        b.memory(
            "ldr.X.Unsigned",
            tmp,
            PROCESSOR,
            RUNTIME_PRESSURE_OFFSET + offset,
        );
        b.emit("subs.X.imm", [reg(zero), reg(tmp), add_imm(1)]);
        done.push((b.offset(), 1));
        b.emit("b.cond", [next(), condition(1)]);
    }
    let limit = Reg::x(11);
    for (register, offset) in [
        (tmp, offset_of!(RuntimeMappingLedger, committed)),
        (limit, offset_of!(RuntimeMappingLedger, memory_limit)),
    ] {
        b.memory(
            "ldr.X.Unsigned",
            register,
            PROCESSOR,
            RUNTIME_MAPPING_LEDGER_OFFSET + offset,
        );
    }
    b.emit(
        "subs.X.shift",
        [reg(zero), reg(tmp), reg(limit), unshifted()],
    );
    done.push((b.offset(), 2));
    b.emit("b.cond", [next(), condition(2)]);
    let request = Reg::x(0);
    b.add(request, PROCESSOR, PROCESSOR_GC_REQUEST_OFFSET);
    b.emit("movz.X", [reg(tmp), movz(1)]);
    b.memory("str.X.Unsigned", tmp, request, 0);
    b.external_branch("b", gc_enter);
    for (branch, cond) in done {
        b.patch_branch(branch, "b.cond", &[], &[condition(cond)]);
    }
    b.ret();
}
