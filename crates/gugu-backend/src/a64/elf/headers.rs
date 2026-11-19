use super::{ElfRequest, Layout, PHNUM, SEGMENT_ALIGN, put};

pub(super) const SHSTRTAB: &[u8] = b"\0.rela.dyn\0.shstrtab\0";

pub(super) fn write(bytes: &mut [u8], request: &ElfRequest, layout: &Layout) -> Result<(), String> {
    bytes[..16].copy_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    half(bytes, 16, 3);
    half(bytes, 18, 183);
    word(bytes, 20, 1);
    put(bytes, 24, layout.address(request.entry)?);
    put(bytes, 32, 64);
    put(bytes, 40, layout.shoff);
    for (at, value) in [(52, 64), (54, 56), (56, PHNUM), (58, 64), (60, 3), (62, 2)] {
        half(bytes, at, value);
    }
    phdr(
        bytes,
        0,
        6,
        4,
        (64, PHNUM as u64 * 56, PHNUM as u64 * 56),
        8,
    );
    for (index, (segment, flags)) in layout.segments.into_iter().zip([5, 4, 6]).enumerate() {
        phdr(bytes, index + 1, 1, flags, segment, SEGMENT_ALIGN);
    }
    phdr(bytes, 4, 0x6474_e551, 6, (0, 0, 0), 16);
    section_headers(bytes, request, layout);
    Ok(())
}

fn phdr(bytes: &mut [u8], index: usize, kind: u32, flags: u32, range: (u64, u64, u64), align: u64) {
    let at = 64 + index * 56;
    let (start, file_size, memory_size) = range;
    word(bytes, at, kind);
    word(bytes, at + 4, flags);
    for (offset, value) in [
        (8, start),
        (16, start),
        (24, start),
        (32, file_size),
        (40, memory_size),
        (48, align),
    ] {
        put(bytes, at + offset, value);
    }
}

fn section_headers(bytes: &mut [u8], request: &ElfRequest, layout: &Layout) {
    let at = layout.shoff as usize + 64;
    word(bytes, at, 1);
    word(bytes, at + 4, 4);
    for (offset, value) in [
        (8, 2),
        (16, layout.rela),
        (24, layout.rela),
        (32, request.relatives.len() as u64 * 24),
        (48, 8),
        (56, 24),
    ] {
        put(bytes, at + offset, value);
    }
    let at = at + 64;
    word(bytes, at, 11);
    word(bytes, at + 4, 3);
    put(bytes, at + 24, layout.shstrtab);
    put(bytes, at + 32, SHSTRTAB.len() as u64);
    put(bytes, at + 48, 1);
    let at = layout.shstrtab as usize;
    bytes[at..at + SHSTRTAB.len()].copy_from_slice(SHSTRTAB);
}

fn half(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn word(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
