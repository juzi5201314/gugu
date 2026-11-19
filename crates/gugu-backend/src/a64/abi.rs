//! Internal scalar/SIMD convention, not an external C ABI.
use crate::regalloc::{Class, PhysReg, RegisterFile};

pub const GPR: Class = Class(0);
pub const FPR: Class = Class(1);
pub const CALLER_SAVED: crate::regalloc::RegSet = crate::regalloc::RegSet(0xffff_00ff_0000_ffff);
pub const FP_CALLEE_SAVED: std::ops::RangeInclusive<u8> = 40..=47;
pub const FP_POOL: [PhysReg; 29] = {
    let mut registers = [PhysReg(32); 29];
    let mut index = 0;
    while index < registers.len() {
        registers[index] = PhysReg(index as u8 + 32);
        index += 1;
    }
    registers
};

pub fn class(ty: crate::lir::body::Type) -> Class {
    use crate::lir::body::Type;
    if matches!(ty, Type::F32 | Type::F64 | Type::V128(_)) {
        FPR
    } else {
        GPR
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Argument {
    Register(u8),
    Stack(u32),
}

pub fn arguments(
    types: impl IntoIterator<Item = crate::lir::body::Type>,
) -> Result<(Vec<Argument>, u32), super::Error> {
    let mut counters = [0_u8; 2];
    let mut stack = 0_u32;
    let mut locations = Vec::new();
    for ty in types {
        let bank = usize::from(class(ty) == FPR);
        let counter = &mut counters[bank];
        if *counter < 8 {
            locations.push(Argument::Register(*counter + bank as u8 * 32));
            *counter += 1;
        } else {
            let bytes = if matches!(ty, crate::lir::body::Type::V128(_)) {
                16
            } else {
                8
            };
            stack = stack
                .checked_add(bytes - 1)
                .map(|v| v & !(bytes - 1))
                .ok_or_else(|| super::error("argument alignment overflow"))?;
            locations.push(Argument::Stack(stack));
            stack = stack
                .checked_add(bytes)
                .ok_or_else(|| super::error("argument area overflow"))?;
        }
    }
    Ok((locations, stack))
}

pub fn results(
    types: impl IntoIterator<Item = crate::lir::body::Type>,
) -> Result<Vec<u8>, super::Error> {
    let (locations, _) = arguments(types)?;
    locations
        .into_iter()
        .map(|location| match location {
            Argument::Register(id) if id < 2 || (32..40).contains(&id) => Ok(id),
            _ => Err(super::error(
                "A64 results exceed two integer or eight FP lanes",
            )),
        })
        .collect()
}

pub fn symbol(instance: &[u8; 32]) -> String {
    let key = instance
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("__gugu_fn_{key}")
}
pub const COROUTINE: u8 = 27;
pub const PROCESSOR: u8 = 28;
pub const CALLEE_SAVED: std::ops::RangeInclusive<u8> = 19..=26;
pub const POOL: [PhysReg; 24] = [
    PhysReg(0),
    PhysReg(1),
    PhysReg(2),
    PhysReg(3),
    PhysReg(4),
    PhysReg(5),
    PhysReg(6),
    PhysReg(7),
    PhysReg(8),
    PhysReg(9),
    PhysReg(10),
    PhysReg(11),
    PhysReg(12),
    PhysReg(13),
    PhysReg(14),
    PhysReg(15),
    PhysReg(19),
    PhysReg(20),
    PhysReg(21),
    PhysReg(22),
    PhysReg(23),
    PhysReg(24),
    PhysReg(25),
    PhysReg(26),
];

pub struct A64Registers;
impl RegisterFile for A64Registers {
    fn class(&self, reg: PhysReg) -> Class {
        if reg.0 >= 32 { FPR } else { GPR }
    }
    fn pool(&self, class: Class, crosses_call: bool) -> &[PhysReg] {
        if class == FPR {
            if crosses_call {
                &FP_POOL[8..16]
            } else {
                &FP_POOL
            }
        } else if class != GPR {
            &[]
        } else if crosses_call {
            &POOL[16..]
        } else {
            &POOL
        }
    }
    fn spill_layout(&self, class: Class) -> (u32, u32) {
        if class == FPR { (16, 16) } else { (8, 8) }
    }
    fn copy_scratch(&self, class: Class) -> Option<PhysReg> {
        Some(PhysReg(if class == FPR { 62 } else { 16 }))
    }
}
