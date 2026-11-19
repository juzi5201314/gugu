//! Monotone local branch relaxation above the single-word encoder.
use super::{
    Error,
    inst::{Inst, Operand, Target},
    machine,
    table::{self, FormId, Kind},
};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Item {
    Label(u32),
    Inst(Inst),
    Branch {
        form: FormId,
        operands: Vec<Operand>,
        target: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plan {
    pub offsets: Vec<u64>,
    pub labels: BTreeMap<u32, u64>,
    pub expanded: Vec<bool>,
    pub size: u64,
}

pub fn fits(displacement: i128, bits: u8) -> bool {
    if !(1..=26).contains(&bits) || displacement % 4 != 0 {
        return false;
    }
    let limit = (1_i128 << (bits - 1)) * 4;
    (-limit..limit).contains(&displacement)
}

fn branch_bits(form: FormId) -> Result<u8, Error> {
    let key = table::get(form)?.key;
    if key == "b" {
        Ok(26)
    } else if key == "b.cond" || key.starts_with("cbz.") || key.starts_with("cbnz.") {
        Ok(19)
    } else if key.starts_with("tbz.") || key.starts_with("tbnz.") {
        Ok(14)
    } else {
        Err(super::error("unsupported local branch form"))
    }
}

/// Base sizes may describe synthetic instruction spans for range tests, without allocating padding.
pub fn plan(items: &[Item], base_sizes: &[u64]) -> Result<Plan, Error> {
    if items.len() != base_sizes.len()
        || base_sizes.iter().any(|s| s % 4 != 0)
        || items.iter().zip(base_sizes).any(|(item, size)| match item {
            Item::Label(_) => *size != 0,
            Item::Branch { .. } => *size != 4,
            Item::Inst(_) => *size == 0,
        })
    {
        return Err(super::error("invalid branch-layout sizes/alignment"));
    }
    let mut expanded = vec![false; items.len()];
    loop {
        let mut labels = BTreeMap::new();
        let mut offsets = Vec::new();
        let mut size = 0_u64;
        for (index, item) in items.iter().enumerate() {
            offsets.push(size);
            if let Item::Label(label) = item
                && labels.insert(*label, size).is_some()
            {
                return Err(super::error("duplicate A64 label"));
            }
            size = size
                .checked_add(base_sizes[index])
                .and_then(|s| s.checked_add(if expanded[index] { 4 } else { 0 }))
                .ok_or_else(|| super::error("A64 layout offset overflow"))?;
        }
        let mut changed = false;
        for (index, item) in items.iter().enumerate() {
            let Item::Branch { form, target, .. } = item else {
                continue;
            };
            let target = *labels
                .get(target)
                .ok_or_else(|| super::error("unknown A64 label"))?;
            let bits = branch_bits(*form)?;
            let pc = offsets[index] + if expanded[index] { 4 } else { 0 };
            let displacement = i128::from(target) - i128::from(pc);
            if !fits(displacement, if expanded[index] { 26 } else { bits }) {
                if bits == 26 || expanded[index] {
                    return Err(super::error(
                        "A64 BranchOutOfRange: unconditional branch needs a veneer",
                    ));
                }
                expanded[index] = true;
                changed = true;
            }
        }
        if !changed {
            return Ok(Plan {
                offsets,
                labels,
                expanded,
                size,
            });
        }
    }
}

fn targeted(form: FormId, operands: &[Operand], displacement: i64) -> Result<Inst, Error> {
    let mut operands = operands.iter().cloned();
    let mut final_operands = Vec::new();
    for slot in table::get(form)?.operands {
        final_operands.push(if matches!(slot.kind, Kind::Target { .. }) {
            Operand::Target(Target::Offset(displacement))
        } else {
            operands
                .next()
                .ok_or_else(|| super::error("missing branch operand"))?
        });
    }
    if operands.next().is_some() {
        return Err(super::error("extra branch operand"));
    }
    Ok(Inst::new(form, final_operands))
}

fn inverted(form: FormId, operands: &[Operand]) -> Result<(FormId, Vec<Operand>), Error> {
    let key = table::get(form)?.key;
    let mut operands = operands.to_vec();
    if key == "b.cond" {
        let [Operand::Condition(condition)] = operands.as_mut_slice() else {
            return Err(super::error("invalid conditional branch operands"));
        };
        if *condition >= 14 {
            return Err(super::error("cannot invert AL/NV branch"));
        }
        *condition ^= 1;
        return Ok((form, operands));
    }
    for (a, b) in [("cbz.", "cbnz."), ("tbz.", "tbnz.")] {
        if let Some(width) = key.strip_prefix(a) {
            return Ok((machine::form(&format!("{b}{width}"))?, operands));
        }
        if let Some(width) = key.strip_prefix(b) {
            return Ok((machine::form(&format!("{a}{width}"))?, operands));
        }
    }
    Err(super::error("branch has no inverse"))
}

pub fn assemble(items: &[Item]) -> Result<(Vec<u8>, Plan), Error> {
    let sizes = items
        .iter()
        .map(|item| if matches!(item, Item::Label(_)) { 0 } else { 4 })
        .collect::<Vec<_>>();
    let plan = plan(items, &sizes)?;
    let mut bytes = Vec::new();
    let mut emit = |inst: Inst| -> Result<(), Error> {
        let encoded = super::encode::encode(&inst)?;
        if !encoded.relocations.is_empty() {
            return Err(super::error("unresolved symbol in integer leaf"));
        }
        bytes.extend_from_slice(&encoded.word.to_le_bytes());
        Ok(())
    };
    for (index, item) in items.iter().enumerate() {
        match item {
            Item::Label(_) => {}
            Item::Inst(inst) => emit(inst.clone())?,
            Item::Branch {
                form,
                operands,
                target,
            } => {
                let pc = plan.offsets[index];
                let target = plan.labels[target];
                let displacement = i64::try_from(i128::from(target) - i128::from(pc))
                    .map_err(|_| super::error("branch displacement overflow"))?;
                for inst in resolve_branch(*form, operands, displacement, plan.expanded[index])? {
                    emit(inst)?;
                }
            }
        }
    }
    Ok((bytes, plan))
}

pub(crate) fn resolve_branch(
    form: FormId,
    operands: &[Operand],
    displacement: i64,
    expanded: bool,
) -> Result<Vec<Inst>, Error> {
    if expanded {
        let (inverse, operands) = inverted(form, operands)?;
        Ok(vec![
            targeted(inverse, &operands, 8)?,
            targeted(
                machine::form("b")?,
                &[],
                displacement
                    .checked_sub(4)
                    .ok_or_else(|| super::error("branch displacement overflow"))?,
            )?,
        ])
    } else {
        Ok(vec![targeted(form, operands, displacement)?])
    }
}
