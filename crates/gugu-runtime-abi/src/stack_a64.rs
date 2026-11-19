//! Exact metadata-driven copies at stackless A64 runtime handoff boundaries.
use super::{Relocation, StackError, StackImage};
use crate::{
    cage::CompressionPlane,
    coroutine::{
        A64Context, A64MorestackMailbox, A64MorestackScratch, POLL_SENTINEL, StackDescriptor,
    },
    model::RawModelError,
    stackmap::{self, HandleSlot, ScannedRoot},
    stackmap_codec::{self, DecodedSection, a64_unwind},
};

#[derive(Clone, Debug)]
pub struct FrameMetadata {
    maps: DecodedSection,
    unwind: a64_unwind::Table,
}

impl FrameMetadata {
    pub fn decode(maps: &[u8], unwind: &[u8]) -> Result<Self, RawModelError> {
        Self::load(maps, unwind, 0)
    }

    pub fn load(maps: &[u8], unwind: &[u8], code_bias: u64) -> Result<Self, RawModelError> {
        let mut maps = stackmap_codec::decode_tables(maps)?;
        let mut unwind = a64_unwind::decode(unwind)?;
        unwind.verify_joint(&maps)?;
        for (point, site) in maps.safepoints.iter().zip(&unwind.sites) {
            if site.entry {
                continue;
            }
            let map = &maps.maps[point.map_index as usize];
            let record = unwind.fp_offset / 8..unwind.fp_offset / 8 + 2;
            if map
                .slots
                .iter()
                .flatten()
                .chain(map.typed.iter().map(|root| &root.word))
                .any(|word| record.contains(word))
                || map
                    .typed
                    .iter()
                    .any(|root| (unwind.fp_offset..unwind.fp_offset + 16).contains(&root.init_byte))
            {
                return Err(RawModelError::new("A64 root overlaps FP/LR record"));
            }
        }
        let rva = unwind
            .code_rva
            .checked_add(code_bias)
            .filter(|rva| {
                rva.is_multiple_of(4) && rva.checked_add(u64::from(unwind.code_size)).is_some()
            })
            .ok_or_else(|| RawModelError::new("invalid A64 image code bias"))?;
        maps.functions[0].code_rva = rva;
        unwind.code_rva = rva;
        Ok(Self { maps, unwind })
    }
}

/// `return_pc`/registers describe the user boundary, not the switch fragment PC.
/// Morestack supplies these from its nonmoving scratch before restoring it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Snapshot {
    pub context: A64Context,
    pub return_pc: u64,
    pub registers: [u64; 31],
}

impl Snapshot {
    pub fn morestack(
        context: A64Context,
        mailbox: &A64MorestackMailbox,
        scratch: &A64MorestackScratch,
        resume_pc: u64,
    ) -> Result<Self, StackError> {
        if context.pc != resume_pc
            || !resume_pc.is_multiple_of(4)
            || context.sp != mailbox.entry_sp
            || !context.sp.is_multiple_of(16)
            || mailbox.required > u64::from(u32::MAX)
            || mailbox.entry_sp.wrapping_sub(mailbox.candidate) != mailbox.required
        {
            return Err(bad("invalid A64 morestack handoff"));
        }
        let mut registers = [0; 31];
        registers[..9].copy_from_slice(&scratch.gpr);
        registers[10] = scratch.caller_lr;
        Ok(Self {
            context,
            return_pc: scratch.return_pc,
            registers,
        })
    }
}

#[derive(Debug)]
pub struct Image {
    source: Snapshot,
    image: StackImage,
    frame_links: Vec<u32>,
    entry_stack_registers: Vec<u32>,
}

fn bad(reason: &'static str) -> StackError {
    StackError::Invariant(reason)
}

fn word(bytes: &[u8], offset: usize) -> Result<u64, StackError> {
    let end = offset.checked_add(8).ok_or(StackError::Overflow)?;
    let bytes = bytes
        .get(offset..end)
        .ok_or_else(|| bad("A64 frame word outside image"))?;
    Ok(u64::from_le_bytes(bytes.try_into().expect("checked word")))
}

impl Image {
    pub fn relocate_morestack(
        &self,
        context: A64Context,
        mailbox: &mut A64MorestackMailbox,
        scratch: &mut A64MorestackScratch,
        old: &StackDescriptor,
        destination: (usize, usize),
    ) -> Result<(Self, A64Context), StackError> {
        let snapshot = Snapshot::morestack(context, mailbox, scratch, self.source.context.pc)?;
        let (image, snapshot) = self.relocate(snapshot, old, destination.0, destination.1)?;
        scratch.gpr.copy_from_slice(&snapshot.registers[..9]);
        mailbox.entry_sp = snapshot.context.sp;
        mailbox.candidate = mailbox.entry_sp.wrapping_sub(mailbox.required);
        Ok((image, snapshot.context))
    }

    pub fn bytes(&self) -> &[u8] {
        &self.image.bytes
    }

    pub fn stack_roots(&self) -> &[u32] {
        &self.image.stack_roots
    }

    pub fn frame_links(&self) -> &[u32] {
        &self.frame_links
    }

    pub fn capture(
        bytes: Vec<u8>,
        snapshot: &Snapshot,
        terminal_pc: u64,
        metadata: &[FrameMetadata],
        handles: &[HandleSlot],
        compression: &mut CompressionPlane,
    ) -> Result<Self, StackError> {
        let start = snapshot.context.sp;
        let high = start
            .checked_add(bytes.len() as u64)
            .ok_or(StackError::Overflow)?;
        if high >= POLL_SENTINEL as u64
            || !start.is_multiple_of(16)
            || !bytes.len().is_multiple_of(16)
            || !snapshot.context.pc.is_multiple_of(4)
        {
            return Err(bad("unaligned A64 suspension"));
        }
        let mut previous_end = 0;
        for function in metadata {
            let table = &function.unwind;
            if table.code_rva < previous_end {
                return Err(bad("overlapping or unordered A64 functions"));
            }
            previous_end = table.code_rva + u64::from(table.code_size);
        }
        let mut image = Self {
            source: *snapshot,
            image: StackImage {
                bytes,
                ..Default::default()
            },
            frame_links: vec![],
            entry_stack_registers: vec![],
        };
        let mut sp = start;
        let mut pc = snapshot.return_pc;
        let mut entry_seen = false;
        while sp != high || pc != terminal_pc {
            let function = metadata
                .iter()
                .find(|function| {
                    let table = &function.unwind;
                    pc >= table.code_rva && pc - table.code_rva < u64::from(table.code_size)
                })
                .ok_or_else(|| bad("A64 caller has no metadata"))?;
            let table = &function.unwind;
            let index = table
                .sites
                .binary_search_by_key(&(pc - table.code_rva), |s| u64::from(s.pc_offset))
                .map_err(|_| bad("A64 copy requires exact return PC"))?;
            let entry = table.sites[index].entry;
            if entry && (entry_seen || sp != start || !image.frame_links.is_empty()) {
                return Err(bad("A64 entry scratch is only valid at the top boundary"));
            }
            entry_seen |= entry;
            if function.maps.safepoints[index].flags & 2 == 0 {
                return Err(bad("A64 safepoint forbids root scanning"));
            }
            if !entry && function.maps.safepoints[index].flags & 1 == 0 {
                return Err(bad("A64 safepoint forbids stack copying"));
            }
            let offset = usize::try_from(
                sp.checked_sub(start)
                    .ok_or_else(|| bad("A64 CFA moved backwards"))?,
            )
            .map_err(|_| StackError::Overflow)?;
            let size = if entry { 0 } else { table.frame_size as usize };
            let end = offset.checked_add(size).ok_or(StackError::Overflow)?;
            let frame = image
                .image
                .bytes
                .get(offset..end)
                .ok_or_else(|| bad("A64 CFA outside image"))?;
            let words = frame
                .as_chunks::<8>()
                .0
                .iter()
                .map(|word| u64::from_le_bytes(*word))
                .collect::<Vec<_>>();
            let roots = stackmap::scan_frame(
                &function.maps,
                pc,
                &words,
                &snapshot.registers,
                handles,
                compression,
            )
            .map_err(|_| bad("invalid A64 root snapshot"))?;
            for root in roots.slots {
                if let ScannedRoot::Stack(slot) = root {
                    let at = offset
                        .checked_add(slot as usize * 8)
                        .ok_or(StackError::Overflow)?;
                    image
                        .image
                        .stack_roots
                        .push(u32::try_from(at).map_err(|_| StackError::Overflow)?);
                }
            }
            for root in roots.registers {
                if let ScannedRoot::Stack(register) = root {
                    image.entry_stack_registers.push(register);
                }
            }
            if !entry {
                image.frame_links.push(
                    u32::try_from(offset + table.fp_offset as usize)
                        .map_err(|_| StackError::Overflow)?,
                );
            }
            let caller = table
                .caller(pc, sp, &words, &snapshot.registers)
                .map_err(|_| bad("invalid A64 caller recovery"))?;
            if !entry && caller.sp <= sp {
                return Err(bad("A64 CFA did not advance"));
            }
            sp = caller.sp;
            pc = caller.pc;
        }
        image.image.stack_roots.sort_unstable();
        image.image.stack_roots.dedup();
        if image
            .frame_links
            .iter()
            .any(|at| image.image.stack_roots.binary_search(at).is_ok())
        {
            return Err(bad("A64 root overlaps structural frame link"));
        }
        let expected = image
            .frame_links
            .first()
            .map_or(0, |at| start + u64::from(*at));
        if snapshot.context.x29 != expected {
            return Err(bad("A64 Context FP disagrees with serialized frame chain"));
        }
        for (index, at) in image.frame_links.iter().enumerate() {
            let expected = image
                .frame_links
                .get(index + 1)
                .map_or(0, |at| start + u64::from(*at));
            if word(&image.image.bytes, *at as usize)? != expected {
                return Err(bad("A64 saved FP disagrees with serialized frame chain"));
            }
        }
        Ok(image)
    }

    pub fn relocate(
        &self,
        snapshot: Snapshot,
        old: &StackDescriptor,
        new_low: usize,
        new_capacity: usize,
    ) -> Result<(Self, Snapshot), StackError> {
        if snapshot != self.source {
            return Err(bad("A64 copy snapshot changed after capture"));
        }
        if new_capacity < 512
            || !new_capacity.is_power_of_two()
            || !new_low.is_multiple_of(16)
            || new_low
                .checked_add(new_capacity)
                .is_none_or(|high| high >= POLL_SENTINEL)
            || old.stack_high.checked_sub(old.stack_low) != Some(old.capacity)
        {
            return Err(bad("invalid A64 stack allocation"));
        }
        let relocation = Relocation::new(
            old,
            usize::try_from(snapshot.context.sp).map_err(|_| StackError::Overflow)?,
            new_low,
            new_capacity,
        )?;
        let mut offsets = self.image.stack_roots.clone();
        offsets.extend(&self.frame_links);
        offsets.sort_unstable();
        let bytes = relocation.copy(&self.image.bytes, &offsets)?;
        let mut restored = snapshot;
        restored.context.sp = relocation.sp as u64;
        restored.context.x29 = relocation
            .pointer(usize::try_from(snapshot.context.x29).map_err(|_| StackError::Overflow)?)?
            as u64;
        for register in &self.entry_stack_registers {
            let value = &mut restored.registers[*register as usize];
            *value = relocation
                .pointer(usize::try_from(*value).map_err(|_| StackError::Overflow)?)?
                as u64;
        }
        Ok((
            Self {
                source: restored,
                image: StackImage {
                    bytes,
                    stack_roots: self.image.stack_roots.clone(),
                    stack_registers: 0,
                },
                frame_links: self.frame_links.clone(),
                entry_stack_registers: self.entry_stack_registers.clone(),
            },
            restored,
        ))
    }
}

#[cfg(test)]
#[path = "stack_a64_tests.rs"]
mod tests;
