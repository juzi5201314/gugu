//! x64 conversion for shared root-separated spill-slot assignment.
use super::{AllocError, AllocationStats, ValueAllocation, registers};
pub use crate::regalloc::slots::Slots;

pub fn assign(values: &mut [ValueAllocation]) -> Result<Slots, AllocError> {
    let mut normalized = values.iter().map(registers::to_core).collect::<Vec<_>>();
    let slots = crate::regalloc::slots::assign(&mut normalized, &registers::X64Registers)
        .map_err(|error| AllocError::new(error.to_string()))?;
    for (value, normalized) in values.iter_mut().zip(normalized) {
        *value = registers::from_core(normalized)?;
    }
    Ok(slots)
}

pub fn statistics(slots: &Slots, stats: &mut AllocationStats) {
    stats.spill_slot_count = u32::try_from(slots.slots.len()).expect("spill slot 数适配 u32");
    stats.spill_bytes = slots.spill_bytes;
}
