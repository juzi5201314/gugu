//! Explicit A64 image/data binding; no target registration or native bootstrap.
use crate::{
    coroutine::{A64Context, A64MorestackMailbox, A64MorestackScratch},
    coroutine_schema::{CoroutineFieldLayout, CoroutineRecordLayout},
    entry::Rt0Payload,
    model::RawModelError,
    stack::a64::{FrameMetadata, Snapshot},
    stackmap_codec::{self, CodeLayout, DecodedSection, SafepointLayout, a64_unwind},
    stackmap_schema::StackMapArchitecture,
};
use std::mem::offset_of;

pub const DIRECTORY_MAGIC: &[u8; 8] = b"GUGUAD01";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[repr(C, align(16))]
pub struct A64ControlBlock {
    pub context: A64Context,
    pub scratch: A64MorestackScratch,
    pub mailbox: A64MorestackMailbox,
}

impl A64ControlBlock {
    /// Addresses belong to nonmoving owner storage, not the copied coroutine stack.
    pub fn bind(&mut self, address: u64, hot: u64, system: u64) -> Result<(), RawModelError> {
        if address == 0
            || !address.is_multiple_of(16)
            || hot == 0
            || system == 0
            || !system.is_multiple_of(8)
            || address.checked_add(size_of::<Self>() as u64).is_none()
        {
            return Err(fail("invalid A64 control-block binding"));
        }
        self.mailbox.hot = hot;
        self.mailbox.scratch = address + offset_of!(Self, scratch) as u64;
        self.mailbox.mutator_context = address;
        self.mailbox.system_context = system;
        Ok(())
    }
}

/// Separate from the selectable x64 runtime contract and its fingerprint.
pub fn source_layouts() -> Vec<CoroutineRecordLayout> {
    macro_rules! record {
        ($name:literal, $ty:ty; $($field:ident),+ $(,)?) => {
            CoroutineRecordLayout {
                name: $name.into(), bytes: size_of::<$ty>() as u32,
                alignment: align_of::<$ty>() as u32,
                fields: vec![$(CoroutineFieldLayout {
                    name: stringify!($field).into(), offset: offset_of!($ty, $field) as u32,
                }),+],
            }
        };
    }
    vec![
        record!("A64Context", A64Context; sp, pc, x19_to_x26, x29, d8_to_d15),
        record!("A64MorestackMailbox", A64MorestackMailbox; hot, entry_sp, candidate, required, scratch, mutator_context, system_context, return_value, processor),
        record!("A64MorestackScratch", A64MorestackScratch; return_pc, caller_lr, vector, gpr),
        record!("A64ControlBlock", A64ControlBlock; context, scratch, mailbox),
        record!("A64Snapshot", Snapshot; context, return_pc, registers),
    ]
}

pub struct Function<'a> {
    /// Relative to the common fragment base; input fragment sections use RVA zero.
    pub code_rva: u64,
    pub maps: &'a [u8],
    pub unwind: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sections {
    maps: Vec<u8>,
    unwind: Vec<u8>,
}

fn fail(message: &str) -> RawModelError {
    RawModelError::new(message)
}

type Inputs = (Vec<CodeLayout>, Vec<(u32, SafepointLayout<u32>)>);

fn inputs(section: &DecodedSection) -> Inputs {
    let mut functions = Vec::new();
    let mut points = Vec::new();
    for (index, function) in section.functions.iter().enumerate() {
        functions.push(CodeLayout {
            code_rva: function.code_rva,
            code_size: function.code_size,
            frame_size: function.frame_size,
            unwind_index: function.unwind_index,
            runtime_bridge: function.flags & 1 != 0,
            panic_landing: function.flags & 2 != 0,
            has_stack_interior: function.flags & 4 != 0,
        });
        let start = function.safepoint_start as usize;
        let end = start + function.safepoint_count as usize;
        for point in &section.safepoints[start..end] {
            let map = &section.maps[point.map_index as usize];
            let count = if point.kind == 4 {
                0
            } else {
                function.frame_size / 8
            };
            let slots = std::array::from_fn(|class| {
                let mut bits = vec![0; (count as usize).div_ceil(64).max(1)];
                for &word in &map.slots[class] {
                    bits[word as usize / 64] |= 1 << (word % 64);
                }
                bits
            });
            points.push((
                index as u32,
                SafepointLayout {
                    pc_offset: point.pc_offset,
                    kind: point.kind,
                    dirty: point.flags & 8 != 0,
                    copy_allowed: point.flags & 1 != 0,
                    scan_allowed: point.flags & 2 != 0,
                    slots,
                    registers: map.registers,
                    typed: map.typed.clone(),
                    function: index as u32,
                    slot_count: count,
                },
            ));
        }
    }
    (functions, points)
}

fn frame_metadata(
    section: &DecodedSection,
    unwind: &[u8],
    index: usize,
    bias: u64,
) -> Result<FrameMetadata, RawModelError> {
    let function = &section.functions[index];
    if function.unwind_index as usize != index {
        return Err(fail("invalid A64 unwind index"));
    }
    let mut single = section.clone();
    single.functions = vec![function.clone()];
    single.functions[0].safepoint_start = 0;
    single.functions[0].unwind_index = 0;
    let start = function.safepoint_start as usize;
    single.safepoints =
        section.safepoints[start..start + function.safepoint_count as usize].to_vec();
    let (code, points) = inputs(&single);
    let bytes = stackmap_codec::encode_for(StackMapArchitecture::Aarch64, &code, &points)?;
    FrameMetadata::load(&bytes, unwind, bias)
}

impl Sections {
    pub fn encode(functions: &[Function<'_>]) -> Result<Self, RawModelError> {
        let mut ordered: Vec<_> = functions.iter().collect();
        ordered.sort_by_key(|function| function.code_rva);
        let mut layouts = Vec::new();
        let mut points = Vec::new();
        let mut tables = Vec::new();
        for function in ordered {
            FrameMetadata::decode(function.maps, function.unwind)?;
            let decoded = stackmap_codec::decode_tables(function.maps)?;
            let (mut code, mut sites) = inputs(&decoded);
            if code[0].code_rva != 0 {
                return Err(fail("A64 fragment input RVA must be zero"));
            }
            let index = layouts.len() as u32;
            code[0].code_rva = function.code_rva;
            code[0].unwind_index = index;
            for (owner, point) in &mut sites {
                *owner = index;
                point.function = index;
            }
            let table = a64_unwind::decode(function.unwind)?;
            tables.push(a64_unwind::encode(
                code[0],
                table.fp_offset,
                &table.sites,
                &table.landings,
            )?);
            layouts.extend(code);
            points.extend(sites);
        }
        let maps = stackmap_codec::encode_for(StackMapArchitecture::Aarch64, &layouts, &points)?;
        let count = u32::try_from(tables.len()).map_err(|_| fail("too many A64 functions"))?;
        let mut unwind = DIRECTORY_MAGIC.to_vec();
        unwind.extend_from_slice(&count.to_le_bytes());
        unwind.extend_from_slice(&183_u32.to_le_bytes());
        let mut offset = 16 + (u64::from(count) + 1) * 8;
        unwind.extend_from_slice(&offset.to_le_bytes());
        for table in &tables {
            offset += table.len() as u64;
            unwind.extend_from_slice(&offset.to_le_bytes());
        }
        for table in tables {
            unwind.extend(table);
        }
        Self::decode(&maps, &unwind)
    }

    pub fn decode(maps: &[u8], unwind: &[u8]) -> Result<Self, RawModelError> {
        let decoded = stackmap_codec::decode_tables(maps)?;
        let count = decoded.functions.len();
        let head = 16 + (count + 1) * 8;
        if decoded.architecture != StackMapArchitecture::Aarch64
            || unwind.len() < head
            || &unwind[..8] != DIRECTORY_MAGIC
            || u32::from_le_bytes(unwind[8..12].try_into().unwrap()) as usize != count
            || u32::from_le_bytes(unwind[12..16].try_into().unwrap()) != 183
        {
            return Err(fail("invalid A64 CFA directory"));
        }
        let offsets: Vec<_> = unwind[16..head]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|bytes| u64::from_le_bytes(*bytes))
            .collect();
        if offsets.first() != Some(&(head as u64)) || offsets.last() != Some(&(unwind.len() as u64))
        {
            return Err(fail("invalid A64 CFA directory extent"));
        }
        for (index, range) in offsets.windows(2).enumerate() {
            if range[0] >= range[1] || range[1] > unwind.len() as u64 || !range[0].is_multiple_of(8)
            {
                return Err(fail("invalid A64 CFA directory offsets"));
            }
            frame_metadata(
                &decoded,
                &unwind[range[0] as usize..range[1] as usize],
                index,
                0,
            )?;
        }
        Ok(Self {
            maps: maps.to_vec(),
            unwind: unwind.to_vec(),
        })
    }

    pub fn maps(&self) -> &[u8] {
        &self.maps
    }
    pub fn unwind(&self) -> &[u8] {
        &self.unwind
    }

    /// Feed the existing metadata-driven stack/context copier from the same image.
    pub fn frames(&self, code_bias: u64) -> Result<Vec<FrameMetadata>, RawModelError> {
        let maps = stackmap_codec::decode_tables(&self.maps)?;
        (0..maps.functions.len())
            .map(|index| {
                let at = 16 + index * 8;
                let first =
                    u64::from_le_bytes(self.unwind[at..at + 8].try_into().unwrap()) as usize;
                let end =
                    u64::from_le_bytes(self.unwind[at + 8..at + 16].try_into().unwrap()) as usize;
                frame_metadata(&maps, &self.unwind[first..end], index, code_bias)
            })
            .collect()
    }

    /// Publish link-time addresses only after both immutable sections validate jointly.
    pub fn bind(
        &self,
        payload: &mut Rt0Payload,
        maps: u64,
        unwind: u64,
        code: u64,
    ) -> Result<(), RawModelError> {
        if payload.revision != Rt0Payload::REVISION
            || maps == 0
            || unwind == 0
            || !maps.is_multiple_of(8)
            || !unwind.is_multiple_of(8)
            || !code.is_multiple_of(4)
            || maps.checked_add(self.maps.len() as u64).is_none()
            || unwind.checked_add(self.unwind.len() as u64).is_none()
        {
            return Err(fail("invalid A64 image placement"));
        }
        let map_end = maps + self.maps.len() as u64;
        let unwind_end = unwind + self.unwind.len() as u64;
        if maps < unwind_end && unwind < map_end {
            return Err(fail("overlapping A64 metadata sections"));
        }
        for function in stackmap_codec::decode_tables(&self.maps)?.functions {
            let start = code
                .checked_add(function.code_rva)
                .ok_or_else(|| fail("A64 loaded code range overflow"))?;
            let end = start
                .checked_add(u64::from(function.code_size))
                .ok_or_else(|| fail("A64 loaded code range overflow"))?;
            if start < map_end && maps < end || start < unwind_end && unwind < end {
                return Err(fail("A64 code overlaps metadata"));
            }
        }
        payload.stackmap = maps;
        payload.stackmap_len = self.maps.len() as u64;
        payload.fragment_base = code;
        payload.unwind = unwind;
        payload.unwind_len = self.unwind.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
