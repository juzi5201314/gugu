//! A64 normalized-event adapter; all allocation algorithms live in regalloc.
use super::{
    Error, abi,
    machine::{Access, Function, MReg},
};
use crate::regalloc::{self, RegSet, RegisterFile};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Allocation {
    pub values: Vec<regalloc::ValueAllocation>,
    pub slots: regalloc::slots::Slots,
    pub stats: regalloc::scan::Statistics,
}

pub fn allocate(function: &Function, registers: &impl RegisterFile) -> Result<Allocation, Error> {
    let count = function.types.len();
    let words = count.div_ceil(64);
    let blocks = function.blocks.len();
    let mut values = vec![
        regalloc::live::Interval {
            start: u32::MAX,
            end: 0,
            uses: vec![],
            rematerializable: false,
            weight: 0
        };
        count
    ];
    let mut uses = vec![vec![0_u64; words]; blocks];
    let mut defs = uses.clone();
    let mut points = Vec::new();
    let mut scan_points = Vec::new();
    let mut predecessors = vec![vec![]; blocks];
    for (block, selected) in function.blocks.iter().enumerate() {
        for successor in &selected.successors {
            predecessors
                .get_mut(*successor)
                .ok_or_else(|| super::error("invalid selected successor"))?
                .push(block);
        }
        for site in &selected.items {
            let position = u32::try_from(points.len())
                .ok()
                .and_then(|p| p.checked_mul(2))
                .ok_or_else(|| super::error("A64 allocation position overflow"))?;
            let call = site.item.is_call();
            let mut fixed = if call { abi::CALLER_SAVED } else { RegSet(0) };
            for (reg, access) in site.item.events() {
                let MReg::Virtual(id) = reg else {
                    if let MReg::Physical(id) = reg {
                        if id > 30 {
                            return Err(super::error("invalid selected physical register"));
                        }
                        if !matches!(site.item, super::machine::Item::Copies(_)) {
                            fixed =
                                fixed.union(RegSet::single(regalloc::PhysReg(id)).map_err(shared)?);
                        }
                    }
                    continue;
                };
                let value = values
                    .get_mut(id as usize)
                    .ok_or_else(|| super::error("unknown selected virtual register"))?;
                if !matches!(access, Access::Write) {
                    value.start = value.start.min(position);
                    value.end = value.end.max(position);
                    value.uses.push(position);
                    uses[block][id as usize / 64] |= 1 << (id % 64);
                }
                if !matches!(access, Access::Read) {
                    value.start = value.start.min(position + 1);
                    value.end = value.end.max(position + 1);
                    defs[block][id as usize / 64] |= 1 << (id % 64);
                }
            }
            points.push(regalloc::live::BlockPoint {
                block: block as u32,
                use_slot: position,
                def_slot: position + 1,
                fixed_physical: fixed.0 != 0,
                call,
            });
            scan_points.push(regalloc::Point {
                use_slot: position,
                def_slot: position + 1,
                mask: fixed,
                pointer_spill: false,
                call,
            });
        }
    }
    let values = regalloc::live::analyze(regalloc::live::Program {
        successors: function
            .blocks
            .iter()
            .map(|b| b.successors.clone())
            .collect(),
        predecessors,
        entry: function.entry,
        uses,
        defs,
        points,
        values,
    })
    .map_err(shared)?;
    let input = regalloc::LiveInfo {
        points: scan_points,
        values: values
            .into_iter()
            .map(|v| regalloc::Value {
                class: abi::GPR,
                root: regalloc::RootClass::NonPointer,
                allowed: RegSet(u64::MAX),
                rematerializable: false,
                range: (v.start, v.end),
                forced: false,
                weight: v.weight,
                hints: vec![],
            })
            .collect(),
    };
    let mut scanned = regalloc::scan::scan(&input, registers).map_err(shared)?;
    let slots = regalloc::slots::assign(&mut scanned.values, registers).map_err(shared)?;
    Ok(Allocation {
        values: scanned.values,
        slots,
        stats: scanned.stats,
    })
}

pub fn shared(error: regalloc::Error) -> Error {
    Error(error.to_string())
}
