//! A64 normalized-event adapter; all allocation algorithms live in regalloc.
use super::{
    Error, abi,
    machine::{Access, Function, MReg},
};
use crate::regalloc::{self, RegSet, RegisterFile};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Allocation {
    pub types: Vec<crate::lir::body::Type>,
    pub values: Vec<regalloc::ValueAllocation>,
    pub slots: regalloc::slots::Slots,
    pub stats: regalloc::scan::Statistics,
    pub site_positions: std::collections::BTreeMap<(u32, u32), u32>,
}

pub fn allocate(function: &Function, registers: &impl RegisterFile) -> Result<Allocation, Error> {
    allocate_roots(function, registers, None)
}

pub(crate) fn allocate_roots(
    function: &Function,
    registers: &impl RegisterFile,
    body: Option<&crate::lir::body::Body>,
) -> Result<Allocation, Error> {
    let mut site_positions = std::collections::BTreeMap::new();
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
            let source = match site.item {
                super::machine::Item::Poll { block, instruction } => Some((block, instruction)),
                super::machine::Item::Call { safepoint, .. } => safepoint,
                super::machine::Item::ForeignCall { safepoint, .. } => Some(safepoint),
                _ => None,
            };
            if let Some(source) = source {
                site_positions.insert(source, position);
            }
            let mut fixed = if matches!(site.item, super::machine::Item::Poll { .. }) {
                abi::CALLER_SAVED.union(RegSet(
                    abi::POOL
                        .iter()
                        .fold(0, |mask, reg| mask | (1_u64 << reg.0)),
                ))
            } else if call {
                abi::CALLER_SAVED
            } else {
                RegSet(0)
            };
            for (reg, access) in site.item.events() {
                let MReg::Virtual(id) = reg else {
                    if let MReg::Physical(id) = reg {
                        if id == 31 || id > 63 {
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
                pointer_spill: body.is_some() && call,
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
            .enumerate()
            .map(|(index, v)| regalloc::Value {
                class: abi::class(function.types[index]),
                root: match body
                    .and_then(|body| body.values.get(index))
                    .and_then(|v| v.kind.provenance)
                    .and_then(|p| p.root_kind())
                {
                    Some(4) => regalloc::RootClass::StackPointer,
                    Some(_) => regalloc::RootClass::HeapPointer,
                    None => regalloc::RootClass::NonPointer,
                },
                allowed: if matches!(function.types[index], crate::lir::body::Type::V128(_)) {
                    RegSet(!(0xff_u64 << 40))
                } else {
                    RegSet(u64::MAX)
                },
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
        types: function.types.clone(),
        values: scanned.values,
        slots,
        stats: scanned.stats,
        site_positions,
    })
}

pub fn shared(error: regalloc::Error) -> Error {
    Error(error.to_string())
}
