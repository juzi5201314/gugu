//! Checked, 16-byte aligned private leaf frames, no red zone or runtime entry.
use super::{Error, alloc::Allocation};
use crate::{
    lir::body::Body,
    regalloc::{Location, Placement},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub size: u32,
    pub required_frame: u32,
    pub max_leaf_reserve: u32,
    pub unchecked_leaf: bool,
    pub outgoing: u32,
    pub record: u32,
    pub copy_scratch: u32,
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
    let calls = body
        .instructions
        .iter()
        .filter_map(|inst| match &inst.op {
            crate::lir::body::Op::Call(call) => Some(call),
            _ => None,
        })
        .collect::<Vec<_>>();
    let words = calls
        .iter()
        .map(|call| call.parameters.len().saturating_sub(8))
        .max()
        .unwrap_or(0);
    let outgoing = align(
        u32::try_from(words)
            .ok()
            .and_then(|n| n.checked_mul(8))
            .ok_or_else(|| super::error("outgoing argument area overflow"))?,
        16,
    )?;
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
    let polls = body
        .instructions
        .iter()
        .any(|inst| matches!(inst.op, crate::lir::body::Op::SafepointPoll { .. }));
    if polls {
        saved.extend(super::abi::CALLEE_SAVED);
    }
    saved.sort_unstable();
    saved.dedup();
    if calls.is_empty()
        && saved.is_empty()
        && allocation.slots.slots.is_empty()
        && body.stack_slots.is_empty()
        && !polls
    {
        return Ok(Frame {
            size: 0,
            required_frame: 0,
            max_leaf_reserve: 0,
            unchecked_leaf: false,
            outgoing: 0,
            record: 0,
            copy_scratch: 0,
            saves: vec![],
            spills: vec![],
            locals: vec![],
        });
    }
    let copy_scratch = outgoing
        .checked_add(16)
        .ok_or_else(|| super::error("frame record overflow"))?;
    let mut offset = copy_scratch
        .checked_add(8)
        .ok_or_else(|| super::error("frame scratch overflow"))?;
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
    Ok(Frame {
        size,
        required_frame: size,
        max_leaf_reserve: 0,
        unchecked_leaf: false,
        outgoing,
        record: outgoing,
        copy_scratch,
        saves,
        spills,
        locals,
    })
}

pub const POLL_FREE_LEAF_RESERVE: u32 = 4096;

pub(crate) fn runtime_policy(body: &Body, frame: &mut Frame) -> Result<(), Error> {
    if body.poll_interface.leaf {
        if body.instructions.iter().any(|inst| matches!(inst.op, crate::lir::body::Op::StackCheck | crate::lir::body::Op::SafepointPoll { .. } | crate::lir::body::Op::Call(_) | crate::lir::body::Op::ForeignCall(_))) || !body.safepoints.is_empty() {
            return Err(super::error("poll-free leaf interface contains a checked entry or runtime call/poll"));
        }
        if frame.size > POLL_FREE_LEAF_RESERVE {
            return Err(super::error("poll-free leaf frame exceeds the 4096-byte caller reserve"));
        }
        frame.unchecked_leaf = true;
    } else if body.instructions.iter().any(|inst| matches!(&inst.op, crate::lir::body::Op::Call(call) if call.kind == crate::gir::body::CallKind::Managed && call.poll_free_leaf)) {
        frame.max_leaf_reserve = POLL_FREE_LEAF_RESERVE;
    }
    frame.required_frame = frame
        .size
        .checked_add(frame.max_leaf_reserve)
        .ok_or_else(|| super::error("required frame plus leaf reserve overflow"))?;
    Ok(())
}
