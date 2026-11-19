//! x64 event and constraint adapter for the shared weighted scan.
use super::super::reg::Reg;
pub use super::registers::{GPR_POOL, XMM_POOL};
use super::{AllocError, AllocationStats, Hint, LiveInfo, ValueAllocation, registers};
use crate::regalloc::{self, Class, PhysReg, RegSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scanned {
    pub values: Vec<ValueAllocation>,
    pub stats: AllocationStats,
}

pub fn scan(live: &LiveInfo) -> Result<Scanned, AllocError> {
    let input = regalloc::LiveInfo {
        values: live.values.iter().map(|value| regalloc::Value {
            class: Class(value.class.code()), root: value.root,
            allowed: RegSet((0..32).filter(|id| !value.byte_operand || *id >= 16 || {
                let reg = registers::location(regalloc::Location::Reg(PhysReg(*id))).ok();
                matches!(reg, Some(super::Location::Gpr(gpr)) if gpr.is_byte_encodable())
            }).fold(0, |bits, id| bits | (1_u64 << id))),
            rematerializable: value.rematerializable, range: (value.start, value.end),
            forced: value.forced, weight: value.weight,
            hints: value.hints.iter().filter_map(|hint| match hint {
                Hint::Register(Reg::Gpr(reg), _) => Some(regalloc::Hint::Register(registers::gpr(*reg))),
                Hint::Register(Reg::Xmm(reg), _) => Some(regalloc::Hint::Register(registers::xmm(*reg))),
                Hint::Register(Reg::Virtual(_), _) => None,
                Hint::SameAs(source, _) => Some(regalloc::Hint::SameAs(*source)),
            }).collect(),
        }).collect(),
        points: live.points.iter().map(|point| regalloc::Point {
            use_slot: point.use_slot, def_slot: point.def_slot, mask: registers::mask(point.mask),
            pointer_spill: point.pointer_spill, call: point.kind.is_call_or_bridge(),
        }).collect(),
    };
    let scanned = regalloc::scan::scan(&input, &registers::X64Registers)
        .map_err(|error| AllocError::new(error.to_string()))?;
    let peak = |class| {
        scanned
            .stats
            .peaks
            .iter()
            .find(|(key, _)| *key == Class(class))
            .map_or(0, |(_, peak)| *peak)
    };
    let stats = AllocationStats {
        peak_live_gpr: peak(0),
        peak_live_xmm: peak(1),
        call_sites: scanned.stats.call_sites,
        safepoint_spills: scanned.stats.safepoint_spills,
        allocated_values: scanned.stats.allocated_values,
        ..AllocationStats::default()
    };
    Ok(Scanned {
        values: scanned
            .values
            .into_iter()
            .map(registers::from_core)
            .collect::<Result<_, _>>()?,
        stats,
    })
}
