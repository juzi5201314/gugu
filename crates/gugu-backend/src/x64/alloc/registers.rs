//! x64 register identities and allocation policy.
use super::super::reg::{Clobbers, Gpr, Xmm};
use super::{AllocError, Location, Placement, RegClass, ValueAllocation};
use crate::regalloc::{self, Class, PhysReg, RegSet, RegisterFile};

/// 通用 GPR 池，顺序即不跨 call 时的偏好序。
pub const GPR_POOL: [Gpr; 12] = [
    Gpr::Rax,
    Gpr::Rcx,
    Gpr::Rdx,
    Gpr::Rbx,
    Gpr::Rsi,
    Gpr::Rdi,
    Gpr::R8,
    Gpr::R9,
    Gpr::R10,
    Gpr::Rbp,
    Gpr::R12,
    Gpr::R13,
];

/// 跨 call/bridge 活跃的 GPR 偏好序前缀：内部 ABI 的 callee-saved 集合。
pub const CROSS_CALL_GPR: [Gpr; 3] = [Gpr::Rbp, Gpr::R12, Gpr::R13];

/// XMM 池，顺序即偏好序。
pub const XMM_POOL: [Xmm; 16] = [
    Xmm::Xmm0,
    Xmm::Xmm1,
    Xmm::Xmm2,
    Xmm::Xmm3,
    Xmm::Xmm4,
    Xmm::Xmm5,
    Xmm::Xmm6,
    Xmm::Xmm7,
    Xmm::Xmm8,
    Xmm::Xmm9,
    Xmm::Xmm10,
    Xmm::Xmm11,
    Xmm::Xmm12,
    Xmm::Xmm13,
    Xmm::Xmm14,
    Xmm::Xmm15,
];

pub struct X64Registers;

const GENERAL: [PhysReg; 12] = gpr_ids(GPR_POOL);
const CROSS: [PhysReg; 15] = {
    let prefix = gpr_ids(CROSS_CALL_GPR);
    let mut result = [PhysReg(0); 15];
    let mut i = 0;
    while i < 15 {
        result[i] = if i < 3 { prefix[i] } else { GENERAL[i - 3] };
        i += 1;
    }
    result
};
const VECTOR: [PhysReg; 16] = {
    let mut result = [PhysReg(0); 16];
    let mut i = 0;
    while i < 16 {
        result[i] = PhysReg(16 + XMM_POOL[i].code());
        i += 1;
    }
    result
};

const fn gpr_ids<const N: usize>(pool: [Gpr; N]) -> [PhysReg; N] {
    let mut result = [PhysReg(0); N];
    let mut i = 0;
    while i < N {
        result[i] = PhysReg(pool[i].code());
        i += 1;
    }
    result
}
const GPRS: [Gpr; 16] = [
    Gpr::Rax,
    Gpr::Rcx,
    Gpr::Rdx,
    Gpr::Rbx,
    Gpr::Rsp,
    Gpr::Rbp,
    Gpr::Rsi,
    Gpr::Rdi,
    Gpr::R8,
    Gpr::R9,
    Gpr::R10,
    Gpr::R11,
    Gpr::R12,
    Gpr::R13,
    Gpr::R14,
    Gpr::R15,
];

impl RegisterFile for X64Registers {
    fn class(&self, reg: PhysReg) -> Class {
        Class(u8::from(reg.0 >= 16))
    }
    fn pool(&self, class: Class, crosses_call: bool) -> &[PhysReg] {
        match (class.0, crosses_call) {
            (0, true) => &CROSS,
            (0, false) => &GENERAL,
            (1, _) => &VECTOR,
            _ => &[],
        }
    }
    fn spill_layout(&self, class: Class) -> (u32, u32) {
        if class.0 == 0 { (8, 8) } else { (16, 16) }
    }
    fn copy_scratch(&self, class: Class) -> Option<PhysReg> {
        (class.0 == 0).then_some(PhysReg(11))
    }
}

pub fn gpr(reg: Gpr) -> PhysReg {
    PhysReg(reg.code())
}
pub fn xmm(reg: Xmm) -> PhysReg {
    PhysReg(16 + reg.code())
}
pub fn mask(mask: Clobbers) -> RegSet {
    RegSet(u64::from(mask.gpr) | (u64::from(mask.xmm) << 16))
}
pub fn location(location: regalloc::Location) -> Result<Location, AllocError> {
    match location {
        regalloc::Location::Reg(PhysReg(id @ 0..=15)) => Ok(Location::Gpr(GPRS[usize::from(id)])),
        regalloc::Location::Reg(PhysReg(id @ 16..=31)) => {
            Ok(Location::Xmm(XMM_POOL[usize::from(id - 16)]))
        }
        regalloc::Location::Slot(slot) => Ok(Location::Slot(slot)),
        _ => Err(AllocError::new(
            "shared allocation returned an invalid x64 register",
        )),
    }
}

pub fn to_core(value: &ValueAllocation) -> regalloc::ValueAllocation {
    regalloc::ValueAllocation {
        value: value.value,
        class: Class(value.class.code()),
        root: value.root,
        range: value.range,
        placement: match value.placement {
            Placement::Dead => regalloc::Placement::Dead,
            Placement::Rematerialize => regalloc::Placement::Rematerialize,
            Placement::Location(location) => regalloc::Placement::Location(match location {
                Location::Gpr(reg) => regalloc::Location::Reg(gpr(reg)),
                Location::Xmm(reg) => regalloc::Location::Reg(xmm(reg)),
                Location::Slot(slot) => regalloc::Location::Slot(slot),
            }),
        },
    }
}

pub fn from_core(value: regalloc::ValueAllocation) -> Result<ValueAllocation, AllocError> {
    Ok(ValueAllocation {
        value: value.value,
        class: match value.class.0 {
            0 => RegClass::Gpr,
            1 => RegClass::Xmm,
            _ => return Err(AllocError::new("invalid x64 register class")),
        },
        root: value.root,
        range: value.range,
        placement: match value.placement {
            regalloc::Placement::Dead => Placement::Dead,
            regalloc::Placement::Rematerialize => Placement::Rematerialize,
            regalloc::Placement::Location(loc) => Placement::Location(location(loc)?),
        },
    })
}
