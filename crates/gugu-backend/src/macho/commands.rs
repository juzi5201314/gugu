//! Mach header、load command、符号表与 UUID 的字节编码（小端）。

use super::layout::{EXPORTS_TRIE, HEADER_SIZE, Layout, PlacedSegment};
use super::{MIN_MACOS, MachORequest, Section, SectionContent, Segment, dylibs};

const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_EXECUTE: u32 = 2;
/// `MH_NOUNDEFS | MH_DYLDLINK | MH_TWOLEVEL | MH_PIE`
const MH_FLAGS: u32 = 0x0020_0085;
const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const LC_LOAD_DYLIB: u32 = 0xc;
const LC_LOAD_DYLINKER: u32 = 0xe;
const LC_UUID: u32 = 0x1b;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const LC_BUILD_VERSION: u32 = 0x32;
const LC_MAIN: u32 = 0x8000_0028;
const LC_DYLD_EXPORTS_TRIE: u32 = 0x8000_0033;
const LC_DYLD_CHAINED_FIXUPS: u32 = 0x8000_0034;
const PLATFORM_MACOS: u32 = 1;
const SG_READ_ONLY: u32 = 0x10;
const S_ZEROFILL: u32 = 0x1;
const S_CODE: u32 = 0x8000_0400;
const N_SECT: u8 = 0x0e;
const DYLD_PATH: &str = "/usr/lib/dyld";
const VM_PROT_READ: u32 = 1;
const VM_PROT_WRITE: u32 = 2;
const VM_PROT_EXECUTE: u32 = 4;

/// 固定大小的 load command：两个 linkedit_data、symtab、dysymtab、uuid、build_version、
/// main 与 code_signature。
const FIXED_COMMANDS: [u32; 8] = [16, 16, 24, 80, 24, 24, 24, 16];

pub(super) fn size_of_commands(
    request: &MachORequest,
    groups: &[(Segment, Vec<usize>)],
) -> (u32, u32) {
    let segments = groups
        .iter()
        .map(|(_, members)| 72 + 80 * members.len() as u32)
        .sum::<u32>()
        + 2 * 72;
    let dylibs = dylibs(request);
    let dylib_bytes: u32 = dylibs.iter().map(|path| path_command_size(24, path)).sum();
    let size = segments
        + FIXED_COMMANDS.iter().sum::<u32>()
        + path_command_size(12, DYLD_PATH)
        + dylib_bytes;
    let count = groups.len() + 2 + FIXED_COMMANDS.len() + 1 + dylibs.len();
    (count as u32, size)
}

fn path_command_size(header: u32, path: &str) -> u32 {
    (header + path.len() as u32 + 1).div_ceil(8) * 8
}

pub(super) fn string_table_size(request: &MachORequest) -> u64 {
    1 + request
        .symbols
        .iter()
        .map(|symbol| symbol.name.len() as u64 + 1)
        .sum::<u64>()
}

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn name16(&mut self, name: &str) {
        let mut field = [0u8; 16];
        field[..name.len()].copy_from_slice(name.as_bytes());
        self.0.extend_from_slice(&field);
    }
    fn path(&mut self, path: &str, command_size: u32, header: u32) {
        self.0.extend_from_slice(path.as_bytes());
        let pad = command_size - header - path.len() as u32;
        self.0.extend(std::iter::repeat_n(0, pad as usize));
    }
}

pub(super) fn write_header(
    file: &mut [u8],
    request: &MachORequest,
    layout: &Layout,
) -> Result<(), String> {
    let (cputype, cpusubtype) = request.arch.cpu();
    let mut out = Out::default();
    for value in [MH_MAGIC_64, cputype, cpusubtype, MH_EXECUTE] {
        out.u32(value);
    }
    for value in [layout.ncmds, layout.sizeofcmds, MH_FLAGS, 0] {
        out.u32(value);
    }
    write_pagezero(&mut out);
    for segment in &layout.segments {
        write_segment(&mut out, request, layout, segment);
    }
    write_linkedit_segment(&mut out, layout);
    write_linkedit_commands(&mut out, request, layout);
    write_loader_commands(&mut out, request, layout)?;
    let expected = HEADER_SIZE + u64::from(layout.sizeofcmds);
    if out.0.len() as u64 != expected {
        return Err(format!(
            "Mach-O load commands are {} bytes, layout reserved {expected}",
            out.0.len()
        ));
    }
    file[..out.0.len()].copy_from_slice(&out.0);
    Ok(())
}

struct SegmentHeader<'a> {
    name: &'a str,
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    prot: u32,
    nsects: u32,
    flags: u32,
}

fn segment_header(out: &mut Out, header: SegmentHeader<'_>) {
    out.u32(LC_SEGMENT_64);
    out.u32(72 + 80 * header.nsects);
    out.name16(header.name);
    for value in [
        header.vmaddr,
        header.vmsize,
        header.fileoff,
        header.filesize,
    ] {
        out.u64(value);
    }
    for value in [header.prot, header.prot, header.nsects, header.flags] {
        out.u32(value);
    }
}

fn write_pagezero(out: &mut Out) {
    segment_header(
        out,
        SegmentHeader {
            name: "__PAGEZERO",
            vmaddr: 0,
            vmsize: super::IMAGE_BASE,
            fileoff: 0,
            filesize: 0,
            prot: 0,
            nsects: 0,
            flags: 0,
        },
    );
}

fn write_segment(out: &mut Out, request: &MachORequest, layout: &Layout, placed: &PlacedSegment) {
    let (prot, flags) = match placed.segment {
        Segment::Text => (VM_PROT_READ | VM_PROT_EXECUTE, 0),
        Segment::DataConst => (VM_PROT_READ | VM_PROT_WRITE, SG_READ_ONLY),
        Segment::Data => (VM_PROT_READ | VM_PROT_WRITE, 0),
    };
    segment_header(
        out,
        SegmentHeader {
            name: placed.segment.name(),
            vmaddr: placed.vmaddr,
            vmsize: placed.vmsize,
            fileoff: placed.fileoff,
            filesize: placed.filesize,
            prot,
            nsects: placed.sections.len() as u32,
            flags,
        },
    );
    for &index in &placed.sections {
        write_section(out, &request.sections[index], layout.sections[index]);
    }
}

fn write_section(out: &mut Out, section: &Section, placed: super::PlacedSection) {
    out.name16(&section.name);
    out.name16(section.segment.name());
    out.u64(placed.addr);
    out.u64(placed.size);
    out.u32(placed.fileoff as u32);
    out.u32(section.align.trailing_zeros());
    let flags = match section.content {
        SectionContent::Code(_) => S_CODE,
        SectionContent::Data(_) => 0,
        SectionContent::ZeroFill(_) => S_ZEROFILL,
    };
    for value in [0, 0, flags, 0, 0, 0] {
        out.u32(value);
    }
}

fn write_linkedit_segment(out: &mut Out, layout: &Layout) {
    let link = &layout.linkedit;
    segment_header(
        out,
        SegmentHeader {
            name: "__LINKEDIT",
            vmaddr: link.vmaddr,
            vmsize: link.vmsize,
            fileoff: link.fileoff,
            filesize: layout.file_size - link.fileoff,
            prot: VM_PROT_READ,
            nsects: 0,
            flags: 0,
        },
    );
}

fn linkedit_data(out: &mut Out, cmd: u32, offset: u64, size: u64) {
    for value in [cmd, 16, offset as u32, size as u32] {
        out.u32(value);
    }
}

fn write_linkedit_commands(out: &mut Out, request: &MachORequest, layout: &Layout) {
    let link = &layout.linkedit;
    linkedit_data(
        out,
        LC_DYLD_CHAINED_FIXUPS,
        link.fixups_off,
        link.fixups_size,
    );
    linkedit_data(
        out,
        LC_DYLD_EXPORTS_TRIE,
        link.exports_off,
        link.exports_size,
    );
    let nsyms = request.symbols.len() as u32;
    for value in [LC_SYMTAB, 24, link.symoff as u32, nsyms] {
        out.u32(value);
    }
    out.u32(link.stroff as u32);
    out.u32(link.strsize as u32);
    // dysymtab：全部符号都是本地符号，其余表为空。
    for value in [LC_DYSYMTAB, 80, 0, nsyms, nsyms, 0, nsyms, 0] {
        out.u32(value);
    }
    for _ in 0..12 {
        out.u32(0);
    }
}

fn write_loader_commands(
    out: &mut Out,
    request: &MachORequest,
    layout: &Layout,
) -> Result<(), String> {
    let size = path_command_size(12, DYLD_PATH);
    for value in [LC_LOAD_DYLINKER, size, 12] {
        out.u32(value);
    }
    out.path(DYLD_PATH, size, 12);
    for value in [LC_UUID, 24, 0, 0, 0, 0] {
        out.u32(value);
    }
    for value in [
        LC_BUILD_VERSION,
        24,
        PLATFORM_MACOS,
        MIN_MACOS,
        MIN_MACOS,
        0,
    ] {
        out.u32(value);
    }
    let entry = layout.address(request.entry)? - layout.text().vmaddr;
    out.u32(LC_MAIN);
    out.u32(24);
    out.u64(entry);
    out.u64(0);
    for path in dylibs(request) {
        let size = path_command_size(24, path);
        // 名字偏移、时间戳 2、current/compatibility 版本 1.0.0。
        for value in [LC_LOAD_DYLIB, size, 24, 2, 0x1_0000, 0x1_0000] {
            out.u32(value);
        }
        out.path(path, size, 24);
    }
    let link = &layout.linkedit;
    linkedit_data(
        out,
        LC_CODE_SIGNATURE,
        link.signature_off,
        link.signature_size,
    );
    Ok(())
}

pub(super) fn write_linkedit(
    file: &mut [u8],
    request: &MachORequest,
    layout: &Layout,
) -> Result<(), String> {
    let link = &layout.linkedit;
    let blob = super::fixups::blob(request, layout)?;
    if blob.len() as u64 != link.fixups_size {
        return Err("Mach-O chained fixup blob size drifted from layout".to_owned());
    }
    put(file, link.fixups_off, &blob);
    put(file, link.exports_off, &EXPORTS_TRIE);
    let mut symbols = Out::default();
    let mut strings = Out::default();
    strings.u8(0);
    for symbol in &request.symbols {
        let placed = layout.sections[symbol.at.section];
        symbols.u32(strings.0.len() as u32);
        symbols.u8(N_SECT);
        symbols.u8(placed.ordinal);
        symbols.u16(0);
        symbols.u64(placed.addr + symbol.at.offset);
        strings.0.extend_from_slice(symbol.name.as_bytes());
        strings.u8(0);
    }
    put(file, link.symoff, &symbols.0);
    put(file, link.stroff, &strings.0);
    Ok(())
}

/// UUID 取签名区之前全部字节（UUID 字段为 0 时）的 BLAKE3 前 16 字节，镜像因而可复现。
pub(super) fn write_uuid(file: &mut [u8], layout: &Layout) {
    let Some(at) = uuid_offset(file, layout.ncmds) else {
        return;
    };
    let digest = blake3::hash(&file[..layout.linkedit.signature_off as usize]);
    file[at..at + 16].copy_from_slice(&digest.as_bytes()[..16]);
}

fn uuid_offset(file: &[u8], ncmds: u32) -> Option<usize> {
    let mut at = HEADER_SIZE as usize;
    for _ in 0..ncmds {
        let cmd = u32::from_le_bytes(file[at..at + 4].try_into().ok()?);
        if cmd == LC_UUID {
            return Some(at + 8);
        }
        at += u32::from_le_bytes(file[at + 4..at + 8].try_into().ok()?) as usize;
    }
    None
}

fn put(file: &mut [u8], at: u64, bytes: &[u8]) {
    let at = at as usize;
    file[at..at + bytes.len()].copy_from_slice(bytes);
}
