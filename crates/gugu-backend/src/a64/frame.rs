//! Checked, 16-byte aligned private leaf frames, no red zone or runtime entry.
use super::{Error, alloc::Allocation};
use crate::{
    lir::body::Body,
    regalloc::{Location, Placement},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub size: u32,
    pub saves: Vec<(u8, u32)>,
    pub spills: Vec<u32>,
    pub locals: Vec<u32>,
}

fn align(value: u32, alignment: u32) -> Result<u32, Error> {
    if !alignment.is_power_of_two() || alignment > 16 {
        return Err(super::error("invalid leaf frame alignment"));
    }
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
        .ok_or_else(|| super::error("leaf frame alignment overflow"))
}

pub fn layout(body: &Body, allocation: &Allocation) -> Result<Frame, Error> {
    let mut saved = allocation
        .values
        .iter()
        .filter_map(|v| match v.placement {
            Placement::Location(Location::Reg(reg))
                if super::abi::CALLEE_SAVED.contains(&reg.0) =>
            {
                Some(reg.0)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    saved.sort_unstable();
    saved.dedup();
    if saved.is_empty() && allocation.slots.slots.is_empty() && body.stack_slots.is_empty() {
        return Ok(Frame {
            size: 0,
            saves: vec![],
            spills: vec![],
            locals: vec![],
        });
    }
    let mut offset = 16_u32;
    let mut saves = Vec::new();
    for reg in saved {
        saves.push((reg, offset));
        offset = offset
            .checked_add(8)
            .ok_or_else(|| super::error("leaf save area overflow"))?;
    }
    let mut spills = Vec::new();
    for slot in &allocation.slots.slots {
        offset = align(offset, slot.align)?;
        spills.push(offset);
        offset = offset
            .checked_add(slot.size)
            .ok_or_else(|| super::error("leaf spill area overflow"))?;
    }
    let mut locals = Vec::new();
    for slot in &body.stack_slots {
        offset = align(offset, slot.align)?;
        locals.push(offset);
        offset = offset
            .checked_add(
                u32::try_from(slot.bytes).map_err(|_| super::error("leaf local too large"))?,
            )
            .ok_or_else(|| super::error("leaf local area overflow"))?;
    }
    let size = align(offset, 16)?;
    if size > 32752 {
        return Err(super::error(
            "leaf frame exceeds initial 32752-byte legalization limit",
        ));
    }
    Ok(Frame {
        size,
        saves,
        spills,
        locals,
    })
}
