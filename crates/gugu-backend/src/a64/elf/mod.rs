//! Internal AArch64 Linux static-PIE writer; not a registered target.
//!
//! Section containers are shared with the Mach-O writer; `DataConst` is read-only
//! here, so self-relocated pointer slots must live in `Data`.

mod dynamic;
mod entry;
mod headers;
mod layout;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

use super::{fragments::Fragment, inst::RelocKind, relocation};
pub use crate::macho::{Location, Section, SectionContent, Segment};
pub use dynamic::{DynamicLink, Import, link_dynamic};
pub use entry::{LinuxEntry, link_entry, link_entry_dynamic};
pub use layout::{Layout, PlacedSection};

pub const SEGMENT_ALIGN: u64 = 0x10000;
pub const R_AARCH64_RELATIVE: u64 = 1027;
const PHNUM: u16 = 5;
#[cfg(test)]
const HEADER_BYTES: u64 = 64 + PHNUM as u64 * 56;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Relative {
    pub at: Location,
    pub target: Location,
    pub addend: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Relocation {
    pub at: Location,
    pub target: Location,
    pub addend: i64,
    pub kind: RelocKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElfRequest {
    pub sections: Vec<Section>,
    pub entry: Location,
    pub relatives: Vec<Relative>,
    pub relocations: Vec<Relocation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElfImage {
    pub bytes: Vec<u8>,
    pub fingerprint: [u8; 32],
}

pub fn layout(request: &ElfRequest) -> Result<Layout, String> {
    validate(request)?;
    Layout::compute(request, 0, false)
}

pub fn link(request: &ElfRequest) -> Result<ElfImage, String> {
    link_prepared(request, None)
}

fn prepared_layout(
    request: &ElfRequest,
    tables: Option<&dynamic::Tables>,
) -> Result<Layout, String> {
    validate(request)?;
    Layout::compute(
        request,
        tables.map_or(0, |t| t.imports.len()),
        tables.is_some(),
    )
}

fn link_prepared(
    request: &ElfRequest,
    tables: Option<&dynamic::Tables>,
) -> Result<ElfImage, String> {
    let layout = prepared_layout(request, tables)?;
    let size = usize::try_from(layout.file_size).map_err(|_| "ELF image too large")?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| "ELF image too large")?;
    bytes.resize(size, 0);
    headers::write(&mut bytes, request, &layout)?;
    for (section, placed) in request.sections.iter().zip(&layout.sections) {
        if let SectionContent::Code(data) | SectionContent::Data(data) = &section.content {
            let at = placed.addr as usize;
            bytes[at..at + data.len()].copy_from_slice(data);
        }
    }
    write_relocations(&mut bytes, request, &layout)?;
    if let Some(tables) = tables {
        tables.write(&mut bytes, request, &layout)?;
    }
    let fingerprint = *blake3::hash(&bytes).as_bytes();
    Ok(ElfImage { bytes, fingerprint })
}

fn write_relocations(
    bytes: &mut [u8],
    request: &ElfRequest,
    layout: &Layout,
) -> Result<(), String> {
    for rel in &request.relocations {
        let pc = layout.address(rel.at)?;
        let target = target_address(layout, rel.target, rel.addend)?;
        let field = &mut bytes[pc as usize..pc as usize + 4];
        let word = u32::from_le_bytes(field.try_into().unwrap());
        field.copy_from_slice(
            &relocation::patch(word, rel.kind, pc, target).map_err(|e| e.to_string())?,
        );
    }
    for (index, rel) in request.relatives.iter().enumerate() {
        let slot = layout.address(rel.at)?;
        let target = target_address(layout, rel.target, rel.addend)?;
        if target > i64::MAX as u64 {
            return Err("ELF RELA addend exceeds i64".into());
        }
        put(bytes, slot as usize, target);
        let at = layout.rela as usize + index * 24;
        for (i, value) in [slot, R_AARCH64_RELATIVE, target].into_iter().enumerate() {
            put(bytes, at + i * 8, value);
        }
    }
    Ok(())
}

fn target_address(layout: &Layout, target: Location, addend: i64) -> Result<u64, String> {
    layout
        .address(target)?
        .checked_add_signed(addend)
        .ok_or_else(|| "ELF relocation target overflow".into())
}

fn validate(request: &ElfRequest) -> Result<(), String> {
    for section in &request.sections {
        if !section.align.is_power_of_two() || section.align > SEGMENT_ALIGN {
            return Err("ELF section alignment must be a power of two within 64 KiB".into());
        }
        match (&section.content, section.segment) {
            (SectionContent::Code(_), Segment::Text)
            | (SectionContent::Data(_), _)
            | (SectionContent::ZeroFill(_), Segment::Data) => {}
            _ => return Err("ELF code must be RX and zero-fill must be RW".into()),
        }
    }
    let entry = section_at(request, request.entry, 4)?;
    if !matches!(entry.content, SectionContent::Code(_))
        || !request.entry.offset.is_multiple_of(4)
        || entry.align < 4
    {
        return Err("ELF entry must be an aligned instruction".into());
    }
    validate_relocations(request)
}

fn validate_relocations(request: &ElfRequest) -> Result<(), String> {
    let mut occupied = BTreeSet::new();
    for rel in &request.relatives {
        let section = section_at(request, rel.at, 8)?;
        section_at(request, rel.target, 0)?;
        if section.segment != Segment::Data
            || section.align < 8
            || rel.at.offset % 8 != 0
            || !matches!(section.content, SectionContent::Data(_))
        {
            return Err("ELF relative slot must be aligned file-backed RW data".into());
        }
        if !occupied.insert(rel.at) {
            return Err("duplicate ELF relative slot".into());
        }
    }
    occupied.clear();
    for rel in &request.relocations {
        let section = section_at(request, rel.at, 4)?;
        section_at(request, rel.target, 0)?;
        if !matches!(section.content, SectionContent::Code(_))
            || section.align < 4
            || rel.at.offset % 4 != 0
        {
            return Err("ELF instruction relocation must address aligned code".into());
        }
        if !occupied.insert(rel.at) {
            return Err("duplicate ELF instruction relocation".into());
        }
    }
    Ok(())
}

fn section_at(request: &ElfRequest, at: Location, width: u64) -> Result<&Section, String> {
    request
        .sections
        .get(at.section)
        .filter(|s| {
            at.offset
                .checked_add(width)
                .is_some_and(|end| end <= s.content.size())
        })
        .ok_or_else(|| format!("invalid ELF location {at:?} (width {width})"))
}

fn put(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn relocate_fragment(
    request: &mut ElfRequest,
    at: Location,
    fragment: Fragment,
    layout: &Layout,
    symbols: &std::collections::BTreeMap<&str, Location>,
) -> Result<(), String> {
    let bytes = relocation::relocate(fragment, layout.address(at)?, |name| {
        layout.address(
            *symbols
                .get(name)
                .ok_or_else(|| format!("unresolved ELF startup symbol {name}"))?,
        )
    })?;
    request.sections[at.section].content = SectionContent::Code(bytes);
    Ok(())
}
