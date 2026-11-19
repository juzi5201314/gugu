//! Internal integer leaf convention, not an external C ABI.
use crate::regalloc::{Class, PhysReg, RegisterFile};

pub const GPR: Class = Class(0);
pub const CALLER_SAVED: crate::regalloc::RegSet = crate::regalloc::RegSet(0xffff);

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
    fn class(&self, _: PhysReg) -> Class {
        GPR
    }
    fn pool(&self, class: Class, crosses_call: bool) -> &[PhysReg] {
        if class != GPR {
            &[]
        } else if crosses_call {
            &POOL[16..]
        } else {
            &POOL
        }
    }
    fn spill_layout(&self, _: Class) -> (u32, u32) {
        (8, 8)
    }
    fn copy_scratch(&self, _: Class) -> Option<PhysReg> {
        Some(PhysReg(16))
    }
}
