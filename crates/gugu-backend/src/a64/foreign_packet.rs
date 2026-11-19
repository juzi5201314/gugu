//! Packet and coroutine-frame staging for canonical A64 C calls.
use super::{
    Error, error,
    foreign::{self, Platform, Slot},
};
use crate::{
    lir::body::{Body, Call, CallTarget, Provenance},
    runtime::foreign_packet as packet,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Group {
    pub first: u32,
    pub bytes: u32,
    pub data: u32,
    pub by_reference: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Plan {
    pub platform: Platform,
    pub abi: foreign::Layout,
    pub groups: Vec<Group>,
    pub packet: packet::A64Layout,
    pub bank: u32,
    pub extra: u32,
    pub result: u32,
    pub sret_depth: u32,
    pub bytes: u32,
    pub fixup_count: u32,
}

pub(crate) fn platform(body: &Body) -> Result<Platform, Error> {
    match body.target.as_str() {
        "aarch64-linux" | "x86_64-linux" => Ok(Platform::Linux),
        "aarch64-macos" | "x86_64-macos" => Ok(Platform::Darwin),
        _ => Err(error(
            "A64 C ABI requires an explicit Linux or Darwin LIR platform",
        )),
    }
}

fn reserve(cursor: &mut u32, bytes: u64) -> Result<u32, Error> {
    let start = *cursor;
    *cursor = u32::try_from(bytes)
        .ok()
        .and_then(|size| size.checked_add(15))
        .map(|size| size & !15)
        .and_then(|size| start.checked_add(size))
        .filter(|end| *end <= i32::MAX as u32)
        .ok_or_else(|| error("A64 foreign frame overflow"))?;
    Ok(start)
}

pub(crate) fn plan(call: &Call, platform: Platform) -> Result<Plan, Error> {
    let signature = call
        .foreign_signature
        .as_ref()
        .filter(|signature| signature.matches(call))
        .ok_or_else(|| error("missing/mismatched canonical A64 foreign signature"))?;
    let abi = foreign::classify(signature, platform)?;
    if call.results.len() as u64 > signature.result.bytes.div_ceil(8)
        || (call.sret.is_some() && !call.results.is_empty())
    {
        return Err(error("A64 foreign results exceed canonical return storage"));
    }
    if call.parameters.iter().chain(&call.results).any(|value| {
        value.ty.bytes().is_none_or(|bytes| bytes > 8)
            || value.provenance.is_some_and(|p| p.managed())
    }) {
        return Err(error(
            "A64 foreign lanes must be scalar raw C representations",
        ));
    }
    if !matches!(
        call.target,
        CallTarget::External { .. } | CallTarget::Instance(_) | CallTarget::Indirect
    ) {
        return Err(error("unsupported A64 foreign target"));
    }
    if matches!(call.target, CallTarget::Indirect)
        && call
            .parameters
            .iter()
            .filter(|p| p.provenance == Some(Provenance::Code))
            .count()
            != 1
    {
        return Err(error("indirect A64 foreign call requires one Code callee"));
    }
    if call.sret.is_some_and(|(index, bytes, _)| {
        bytes != signature.result.bytes
            || call
                .parameters
                .get(index as usize)
                .is_none_or(|p| p.provenance != Some(Provenance::Stack))
    }) {
        return Err(error(
            "A64 foreign sret requires a matching coroutine-stack destination",
        ));
    }
    if call.parameters.iter().enumerate().any(|(index, p)| {
        p.provenance == Some(Provenance::Stack)
            && call.sret.is_none_or(|(sret, _, _)| sret as usize != index)
            && !call
                .by_value
                .iter()
                .any(|(input, _, _)| *input as usize == index)
    }) {
        return Err(error(
            "A64 native cannot retain a coroutine-stack pointer argument",
        ));
    }
    let mut fixup_count = u32::from(abi.result.indirect) + u32::from(abi.result.pieces.len() > 2);
    let mut group_index = 0;
    for (ty, arg) in signature.arguments.iter().zip(&abi.arguments) {
        if ty.bytes == 0 {
            continue;
        }
        let group = &call.foreign_arguments[group_index];
        group_index += 1;
        if arg.indirect
            || call
                .parameters
                .get(group.first as usize)
                .is_some_and(|p| p.provenance == Some(Provenance::Raw))
        {
            fixup_count = fixup_count
                .checked_add(1)
                .ok_or_else(|| error("too many A64 foreign fixups"))?;
        }
    }
    let layout = packet::A64Layout::new(abi.stack_bytes, fixup_count)
        .ok_or_else(|| error("A64 packet overflow"))?;
    let mut cursor = layout.bytes;
    let bank = reserve(&mut cursor, call.parameters.len() as u64 * 8)?;
    let mut groups = Vec::new();
    for group in &call.foreign_arguments {
        let by_reference = call
            .by_value
            .iter()
            .any(|(index, _, bytes)| *index == group.first && *bytes == u64::from(group.bytes));
        let end = group
            .first
            .checked_add(group.lanes)
            .filter(|end| *end as usize <= call.parameters.len())
            .ok_or_else(|| error("invalid foreign argument lane range"))?;
        if group.lanes == 0
            || (!by_reference && u64::from(group.lanes) * 8 < u64::from(group.bytes))
            || (by_reference
                && (group.lanes != 1
                    || call.parameters[group.first as usize].provenance != Some(Provenance::Stack)))
            || end == 0
        {
            return Err(error("invalid A64 foreign aggregate source"));
        }
        let data = reserve(&mut cursor, u64::from(group.bytes))?;
        groups.push(Group {
            first: group.first,
            bytes: group.bytes,
            data,
            by_reference,
        });
    }
    let extra = reserve(&mut cursor, 16)?;
    let result = reserve(&mut cursor, signature.result.bytes)?;
    let sret_depth = reserve(&mut cursor, 8)?;
    if call.sret.is_none()
        && call
            .results
            .iter()
            .map(|p| p.ty.bytes().unwrap_or(0))
            .sum::<u64>()
            < signature.result.bytes
    {
        return Err(error(
            "A64 foreign result lanes do not cover canonical result",
        ));
    }
    Ok(Plan {
        platform,
        abi,
        groups,
        packet: layout,
        bank,
        extra,
        result,
        sret_depth,
        bytes: cursor,
        fixup_count,
    })
}

pub(crate) fn lane(slot: Slot) -> u32 {
    match slot {
        Slot::Integer(index) => packet::a64_integer_lane(index).expect("classified GPR"),
        Slot::Float(index) => packet::FOREIGN_PACKET_FLOATS as u32 + u32::from(index) * 8,
        Slot::Stack(offset) => packet::A64_PACKET_BYTES as u32 + offset,
    }
}
