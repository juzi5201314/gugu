//! `LC_DYLD_CHAINED_FIXUPS`：指针格式 `DYLD_CHAINED_PTR_64_OFFSET`，导入格式
//! `DYLD_CHAINED_IMPORT`。每页一条链，链内按 4 字节 stride 记录到下一槽的距离。

use super::layout::{Layout, PlacedSegment, align};
use super::{Arch, Fixup, Location, MachORequest, SectionContent, Segment, dylibs};
use std::collections::BTreeMap;

const HEADER_SIZE: u64 = 28;
const DYLD_CHAINED_IMPORT: u32 = 1;
const DYLD_CHAINED_PTR_64_OFFSET: u16 = 6;
const DYLD_CHAINED_PTR_START_NONE: u16 = 0xffff;
const STRIDE: u64 = 4;
const TARGET_BITS: u32 = 36;
const MAX_DYLIBS: usize = 0xef;

pub(super) fn validate(request: &MachORequest) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for fixup in &request.fixups {
        let at = fixup.at();
        let section = super::section_at(request, at, false)?;
        if section.segment == Segment::Text || !matches!(section.content, SectionContent::Data(_)) {
            return Err(format!(
                "Mach-O fixup slot in {} must be in a __DATA_CONST/__DATA data section",
                section.name
            ));
        }
        if at.offset % 8 != 0 || at.offset + 8 > section.content.size() {
            return Err(format!(
                "Mach-O fixup slot {:#x} in {} must be an aligned 8-byte slot",
                at.offset, section.name
            ));
        }
        if section.align < 8 {
            return Err(format!(
                "Mach-O fixup section {} must be 8-aligned",
                section.name
            ));
        }
        if !seen.insert(at) {
            return Err(format!(
                "Mach-O fixup slot {:#x} in {} is duplicated",
                at.offset, section.name
            ));
        }
        match *fixup {
            Fixup::Rebase { target, .. } => {
                super::section_at(request, target, true)?;
            }
            Fixup::Bind { import, .. } if import >= request.imports.len() => {
                return Err(format!("Mach-O bind names missing import {import}"));
            }
            Fixup::Bind { .. } => {}
        }
    }
    // DYLD_CHAINED_IMPORT 的 8 位序号中 0xF0..=0xFF 是负数特殊序号。
    if dylibs(request).len() > MAX_DYLIBS {
        return Err(format!("Mach-O image links more than {MAX_DYLIBS} dylibs"));
    }
    Ok(())
}

/// load command 中全部段：`__PAGEZERO`、有节的段、`__LINKEDIT`。
fn segment_count(segments: &[PlacedSegment]) -> u64 {
    segments.len() as u64 + 2
}

fn fixup_segments(request: &MachORequest) -> Vec<Segment> {
    let mut out: Vec<Segment> = request
        .fixups
        .iter()
        .map(|fixup| request.sections[fixup.at().section].segment)
        .collect();
    out.sort();
    out.dedup();
    out
}

fn starts_in_segment_size(placed: &PlacedSegment, arch: Arch) -> u64 {
    22 + 2 * placed.vmsize.div_ceil(arch.page_size())
}

struct Offsets {
    starts: u64,
    segments: Vec<(Segment, u64)>,
    imports: u64,
    symbols: u64,
    end: u64,
}

fn offsets(request: &MachORequest, segments: &[PlacedSegment], arch: Arch) -> Offsets {
    let starts = align(HEADER_SIZE, 8);
    let mut cursor = starts + 4 + 4 * segment_count(segments);
    let mut placed = Vec::new();
    for segment in fixup_segments(request) {
        let Some(info) = segments.iter().find(|p| p.segment == segment) else {
            continue;
        };
        cursor = align(cursor, 8);
        placed.push((segment, cursor));
        cursor += starts_in_segment_size(info, arch);
    }
    let imports = align(cursor, 4);
    let symbols = imports + 4 * request.imports.len() as u64;
    let names: u64 = request
        .imports
        .iter()
        .map(|i| i.name.len() as u64 + 1)
        .sum();
    Offsets {
        starts,
        segments: placed,
        imports,
        symbols,
        end: align(symbols + 1 + names, 8),
    }
}

pub(super) fn blob_size(request: &MachORequest, segments: &[PlacedSegment], arch: Arch) -> u64 {
    offsets(request, segments, arch).end
}

pub(super) fn blob(request: &MachORequest, layout: &Layout) -> Result<Vec<u8>, String> {
    let offsets = offsets(request, &layout.segments, layout.arch);
    let mut out = vec![0u8; offsets.end as usize];
    let header = [
        0,
        offsets.starts as u32,
        offsets.imports as u32,
        offsets.symbols as u32,
        request.imports.len() as u32,
        DYLD_CHAINED_IMPORT,
        0,
    ];
    for (i, value) in header.into_iter().enumerate() {
        put32(&mut out, 4 * i as u64, value);
    }
    put32(
        &mut out,
        offsets.starts,
        segment_count(&layout.segments) as u32,
    );
    for &(segment, at) in &offsets.segments {
        let index = layout
            .segment_index(segment)
            .expect("fixup segment is placed");
        put32(
            &mut out,
            offsets.starts + 4 + 4 * index as u64,
            (at - offsets.starts) as u32,
        );
        write_starts(&mut out, at, request, layout, segment)?;
    }
    write_imports(&mut out, &offsets, request)?;
    Ok(out)
}

fn write_starts(
    out: &mut [u8],
    at: u64,
    request: &MachORequest,
    layout: &Layout,
    segment: Segment,
) -> Result<(), String> {
    let placed = layout
        .segments
        .iter()
        .find(|p| p.segment == segment)
        .expect("fixup segment is placed");
    let page = layout.arch.page_size();
    let pages = placed.vmsize.div_ceil(page);
    if pages > u64::from(u16::MAX) {
        return Err(format!(
            "Mach-O segment {} spans more than 65535 pages",
            segment.name()
        ));
    }
    put32(out, at, starts_in_segment_size(placed, layout.arch) as u32);
    put16(out, at + 4, page as u16);
    put16(out, at + 6, DYLD_CHAINED_PTR_64_OFFSET);
    put64(out, at + 8, placed.vmaddr - super::IMAGE_BASE);
    put32(out, at + 16, 0);
    put16(out, at + 20, pages as u16);
    let mut starts = vec![DYLD_CHAINED_PTR_START_NONE; pages as usize];
    for (offset, _) in chain(request, layout, placed)? {
        let page_index = (offset / page) as usize;
        if starts[page_index] == DYLD_CHAINED_PTR_START_NONE {
            starts[page_index] = (offset % page) as u16;
        }
    }
    for (i, start) in starts.into_iter().enumerate() {
        put16(out, at + 22 + 2 * i as u64, start);
    }
    Ok(())
}

fn write_imports(out: &mut [u8], offsets: &Offsets, request: &MachORequest) -> Result<(), String> {
    let libraries = dylibs(request);
    let mut name_at = 1u64;
    for (i, import) in request.imports.iter().enumerate() {
        let ordinal = libraries
            .iter()
            .position(|path| *path == import.dylib)
            .expect("import dylib is collected")
            + 1;
        if name_at >= 1 << 23 {
            return Err("Mach-O import names exceed 8 MiB".to_owned());
        }
        let value = ordinal as u32 | (u32::from(import.weak) << 8) | ((name_at as u32) << 9);
        put32(out, offsets.imports + 4 * i as u64, value);
        let at = (offsets.symbols + name_at) as usize;
        out[at..at + import.name.len()].copy_from_slice(import.name.as_bytes());
        name_at += import.name.len() as u64 + 1;
    }
    Ok(())
}

/// 段内按偏移排序的 `(segment offset, fixup)`。
fn chain<'a>(
    request: &'a MachORequest,
    layout: &Layout,
    placed: &PlacedSegment,
) -> Result<BTreeMap<u64, &'a Fixup>, String> {
    let mut out = BTreeMap::new();
    for fixup in &request.fixups {
        if request.sections[fixup.at().section].segment != placed.segment {
            continue;
        }
        let address = layout.address(fixup.at())?;
        out.insert(address - placed.vmaddr, fixup);
    }
    Ok(out)
}

/// 把每个槽写成链上的 64 位编码值。
pub(super) fn apply(
    file: &mut [u8],
    request: &MachORequest,
    layout: &Layout,
) -> Result<(), String> {
    let page = layout.arch.page_size();
    for placed in &layout.segments {
        let slots = chain(request, layout, placed)?;
        let offsets: Vec<u64> = slots.keys().copied().collect();
        for (i, (&offset, fixup)) in slots.iter().enumerate() {
            let next = offsets
                .get(i + 1)
                .filter(|&&n| n / page == offset / page)
                .map_or(0, |&n| (n - offset) / STRIDE);
            let value = encode(request, layout, fixup, next)?;
            let at = (placed.fileoff + offset) as usize;
            file[at..at + 8].copy_from_slice(&value.to_le_bytes());
        }
    }
    Ok(())
}

fn encode(
    request: &MachORequest,
    layout: &Layout,
    fixup: &Fixup,
    next: u64,
) -> Result<u64, String> {
    debug_assert!(next < 1 << 12);
    match *fixup {
        Fixup::Rebase { target, .. } => {
            let offset = target_offset(layout, target)?;
            Ok(offset | (next << 51))
        }
        Fixup::Bind { import, addend, .. } => {
            debug_assert!(import < request.imports.len());
            if import >= 1 << 24 {
                return Err("Mach-O bind import ordinal exceeds 24 bits".to_owned());
            }
            Ok(import as u64 | (u64::from(addend) << 24) | (next << 51) | (1 << 63))
        }
    }
}

fn target_offset(layout: &Layout, target: Location) -> Result<u64, String> {
    let offset = layout.address(target)? - super::IMAGE_BASE;
    if offset >= 1 << TARGET_BITS {
        return Err(format!("Mach-O rebase target {offset:#x} exceeds 36 bits"));
    }
    Ok(offset)
}

fn put16(out: &mut [u8], at: u64, value: u16) {
    out[at as usize..at as usize + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut [u8], at: u64, value: u32) {
    out[at as usize..at as usize + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(out: &mut [u8], at: u64, value: u64) {
    out[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
}
