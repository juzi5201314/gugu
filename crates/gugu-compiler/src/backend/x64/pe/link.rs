//! 把逻辑节、运行时入口和 IAT 收成一张 PE32+。

use std::collections::{BTreeMap, BTreeSet};

use super::super::elf::{BootOffsets, LinkCode, LinkRelocKind};
use super::super::mangle::{mangle_glue, mangle_runtime};
use super::runtime::{self, CallSite, ImportUse, Routine};
use super::{IMAGE_BASE, ImageKind, PE_DOMAIN, PE_SCHEMA, PeError, PeImage};

const PAGE: u32 = 4096;
const FILE_ALIGN: u32 = 512;
const TEXT_RVA: u32 = 0x1000;
const SCN_CODE: u32 = 0x6000_0020;
const SCN_RDATA: u32 = 0x4000_0040;
const SCN_RELOC: u32 = 0x4200_0040;
const SCN_BSS: u32 = 0xC000_0080;

const IMPORTS: &[(&str, &str)] = &[
    ("kernel32.dll", "GetStdHandle"),
    ("kernel32.dll", "VirtualAlloc"),
    ("kernel32.dll", "VirtualFree"),
    ("kernel32.dll", "VirtualProtect"),
    ("kernel32.dll", "WriteFile"),
    ("ntdll.dll", "RtlExitUserProcess"),
    ("ntdll.dll", "RtlRandomEx"),
];

struct Func {
    rva: u32,
    bytes_at: usize,
    len: usize,
    imports: Vec<ImportUse>,
    calls: Vec<CallSite>,
    frame: u32,
}

struct ImportDir {
    bytes: Vec<u8>,
    slots: BTreeMap<String, u32>,
}

struct Section {
    name: [u8; 8],
    bytes: Vec<u8>,
    rva: u32,
    characteristics: u32,
    raw: bool,
}

pub(super) fn link(
    codes: &[LinkCode],
    consts: &[(String, Vec<u8>, u32)],
    sections: &[(&str, &[u8])],
    entry: &str,
    boot: BootOffsets,
    frames: &BTreeMap<String, u32>,
    kind: ImageKind,
) -> Result<PeImage, PeError> {
    reject_entry(codes, entry)?;
    let mut text = Vec::new();
    let mut symbols = BTreeMap::new();
    let mut funcs = Vec::new();
    place_users(&mut text, codes, frames, &mut symbols, &mut funcs)?;
    place_runtime(&mut text, boot, entry, &mut symbols, &mut funcs)?;
    place_raw(
        &mut text,
        "__gugu_trap",
        &[0x0f, 0x0b],
        0,
        &mut symbols,
        &mut funcs,
    )?;
    let rdata_rva = page_align(TEXT_RVA + text.len() as u32);
    let imports = build_imports(rdata_rva)?;
    let (rdata, ro_symbols) = layout_rdata(&imports.bytes, consts, codes, rdata_rva)?;
    symbols.extend(ro_symbols);
    let mut abs = Vec::new();
    patch_text(&mut text, codes, &symbols, &mut abs)?;
    patch_sites(&mut text, &funcs, &symbols, &imports.slots)?;
    let (file, count, start) = encode_file(
        &text, &rdata, rdata_rva, sections, &funcs, &abs, &symbols, kind,
    )?;
    if file.windows(2).any(|window| window == [0x0f, 0x05]) {
        return Err(PeError::new("镜像含有 syscall 指令"));
    }
    let _ = entry;
    let archive = super::staticlib(&text);
    Ok(finish(
        file,
        u64::from(start),
        abs.len() as u32,
        count,
        archive,
    ))
}

fn reject_entry(codes: &[LinkCode], entry: &str) -> Result<(), PeError> {
    if codes.iter().any(|code| code.symbol == entry) {
        Ok(())
    } else {
        Err(PeError::new("入口符号缺失"))
    }
}

fn place_users(
    text: &mut Vec<u8>,
    codes: &[LinkCode],
    frames: &BTreeMap<String, u32>,
    symbols: &mut BTreeMap<String, u32>,
    funcs: &mut Vec<Func>,
) -> Result<(), PeError> {
    let mut included: Vec<&LinkCode> = codes.iter().filter(|code| code.included).collect();
    let mut rest: Vec<&LinkCode> = codes.iter().filter(|code| !code.included).collect();
    included.sort_by(|left, right| left.symbol.cmp(&right.symbol));
    rest.sort_by(|left, right| left.symbol.cmp(&right.symbol));
    for code in included.into_iter().chain(rest) {
        let frame = frames.get(&code.symbol).copied().unwrap_or(0);
        place_raw(text, &code.symbol, &code.bytes, frame, symbols, funcs)?;
    }
    Ok(())
}

fn place_runtime(
    text: &mut Vec<u8>,
    boot: BootOffsets,
    entry: &str,
    symbols: &mut BTreeMap<String, u32>,
    funcs: &mut Vec<Func>,
) -> Result<(), PeError> {
    let mut bodies = runtime::routines(boot, entry);
    bodies.sort_by(|left, right| left.name.cmp(right.name));
    for routine in bodies {
        place_routine(text, &routine, symbols, funcs)?;
    }
    Ok(())
}

fn place_routine(
    text: &mut Vec<u8>,
    routine: &Routine,
    symbols: &mut BTreeMap<String, u32>,
    funcs: &mut Vec<Func>,
) -> Result<(), PeError> {
    let name = if routine.name == "_start" {
        "_start".to_owned()
    } else if routine.runtime {
        mangle_runtime(routine.name)
    } else {
        mangle_glue(routine.name)
    };
    align16(text);
    let rva = TEXT_RVA + text.len() as u32;
    if symbols.insert(name, rva).is_some() {
        return Err(PeError::new("符号重复"));
    }
    let bytes_at = text.len();
    text.extend_from_slice(&routine.bytes);
    funcs.push(Func {
        rva,
        bytes_at,
        len: routine.bytes.len(),
        imports: routine.imports.clone(),
        calls: routine.calls.clone(),
        frame: 0,
    });
    Ok(())
}

fn place_raw(
    text: &mut Vec<u8>,
    name: &str,
    bytes: &[u8],
    frame: u32,
    symbols: &mut BTreeMap<String, u32>,
    funcs: &mut Vec<Func>,
) -> Result<(), PeError> {
    align16(text);
    let rva = TEXT_RVA + text.len() as u32;
    if symbols.insert(name.to_owned(), rva).is_some() {
        return Err(PeError::new("符号重复"));
    }
    let bytes_at = text.len();
    text.extend_from_slice(bytes);
    funcs.push(Func {
        rva,
        bytes_at,
        len: bytes.len(),
        imports: Vec::new(),
        calls: Vec::new(),
        frame,
    });
    Ok(())
}

fn align16(text: &mut Vec<u8>) {
    while text.len() % 16 != 0 {
        text.push(0x90);
    }
}

fn layout_rdata(
    imports: &[u8],
    consts: &[(String, Vec<u8>, u32)],
    codes: &[LinkCode],
    rva: u32,
) -> Result<(Vec<u8>, BTreeMap<String, u32>), PeError> {
    let mut bytes = imports.to_vec();
    let mut symbols = BTreeMap::new();
    if needs_cage(codes) {
        align_to(&mut bytes, 8);
        symbols.insert("cage-control".to_owned(), rva + bytes.len() as u32);
        bytes.extend(std::iter::repeat_n(0, 64));
    }
    for (name, data, align) in consts {
        let align = (*align).max(1);
        align_to(&mut bytes, align as usize);
        if symbols
            .insert(name.clone(), rva + bytes.len() as u32)
            .is_some()
        {
            return Err(PeError::new("符号重复"));
        }
        bytes.extend_from_slice(data);
    }
    if bytes.is_empty() {
        bytes.push(0);
    }
    Ok((bytes, symbols))
}

fn needs_cage(codes: &[LinkCode]) -> bool {
    codes.iter().any(|code| {
        code.relocs
            .iter()
            .any(|reloc| reloc.target == "cage-control")
    })
}

fn patch_text(
    text: &mut [u8],
    codes: &[LinkCode],
    symbols: &BTreeMap<String, u32>,
    abs: &mut Vec<u32>,
) -> Result<(), PeError> {
    let mut unknown = BTreeSet::new();
    let mut foreign = BTreeSet::new();
    for code in codes {
        let base = symbols
            .get(&code.symbol)
            .copied()
            .ok_or_else(|| PeError::new("符号缺失"))?;
        for reloc in &code.relocs {
            patch_one(text, base, reloc, symbols, abs, &mut unknown, &mut foreign)?;
        }
    }
    if !unknown.is_empty() {
        let names = unknown.into_iter().collect::<Vec<_>>().join(" ");
        return Err(PeError::new(format!("未知运行时符号 {names}")));
    }
    if !foreign.is_empty() {
        let names = foreign.into_iter().collect::<Vec<_>>().join(" ");
        return Err(PeError::new(format!("未登记导入 {names}")));
    }
    Ok(())
}

fn patch_one(
    text: &mut [u8],
    base: u32,
    reloc: &super::super::elf::LinkReloc,
    symbols: &BTreeMap<String, u32>,
    abs: &mut Vec<u32>,
    unknown: &mut BTreeSet<String>,
    foreign: &mut BTreeSet<String>,
) -> Result<(), PeError> {
    let target_name = if let Some(site) = reloc.target.strip_prefix("cold:") {
        let _ = site;
        "__gugu_trap"
    } else {
        reloc.target.as_str()
    };
    let Some(target) = symbols.get(target_name).copied() else {
        classify_missing(&reloc.target, unknown, foreign);
        return Ok(());
    };
    let field = base + reloc.offset;
    let at = (field - TEXT_RVA) as usize;
    match reloc.kind {
        LinkRelocKind::PcRel32 => write_disp(text, at, field, target, reloc.addend)?,
        LinkRelocKind::Rva32 => write_rva(text, at, target, reloc.addend)?,
        LinkRelocKind::Abs64 => write_abs(text, at, field, target, reloc.addend, abs)?,
    }
    Ok(())
}

fn classify_missing(name: &str, unknown: &mut BTreeSet<String>, foreign: &mut BTreeSet<String>) {
    if name.starts_with("__gugu_") || name.starts_with("cold:") {
        unknown.insert(name.to_owned());
    } else {
        foreign.insert(name.to_owned());
    }
}

fn write_disp(
    text: &mut [u8],
    at: usize,
    field: u32,
    target: u32,
    addend: i64,
) -> Result<(), PeError> {
    let disp = i64::from(target) + addend - (i64::from(field) + 4);
    let disp = i32::try_from(disp).map_err(|_| PeError::new("pc-rel32 超出范围"))?;
    text[at..at + 4].copy_from_slice(&disp.to_le_bytes());
    Ok(())
}

fn write_rva(text: &mut [u8], at: usize, target: u32, addend: i64) -> Result<(), PeError> {
    let value = i64::from(target) + addend;
    let value = u32::try_from(value).map_err(|_| PeError::new("rva32 超出范围"))?;
    text[at..at + 4].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_abs(
    text: &mut [u8],
    at: usize,
    field: u32,
    target: u32,
    addend: i64,
    abs: &mut Vec<u32>,
) -> Result<(), PeError> {
    let value = IMAGE_BASE
        .wrapping_add(u64::from(target))
        .wrapping_add(addend as u64);
    text[at..at + 8].copy_from_slice(&value.to_le_bytes());
    abs.push(field);
    Ok(())
}

fn patch_sites(
    text: &mut [u8],
    funcs: &[Func],
    symbols: &BTreeMap<String, u32>,
    slots: &BTreeMap<String, u32>,
) -> Result<(), PeError> {
    for func in funcs {
        for site in &func.imports {
            let iat = slots
                .get(site.symbol)
                .copied()
                .ok_or_else(|| PeError::new(format!("未登记导入 {}", site.symbol)))?;
            let field = func.rva + site.offset;
            write_disp(text, (field - TEXT_RVA) as usize, field, iat, 0)?;
        }
        for site in &func.calls {
            let target = symbols
                .get(&site.symbol)
                .copied()
                .ok_or_else(|| PeError::new("入口符号缺失"))?;
            let field = func.rva + site.offset;
            write_disp(text, (field - TEXT_RVA) as usize, field, target, 0)?;
        }
    }
    Ok(())
}

fn encode_file(
    text: &[u8],
    rdata: &[u8],
    rdata_rva: u32,
    meta: &[(&str, &[u8])],
    funcs: &[Func],
    abs: &[u32],
    symbols: &BTreeMap<String, u32>,
    kind: ImageKind,
) -> Result<(Vec<u8>, u32, u32), PeError> {
    let mut parts = vec![
        section(".text", text, TEXT_RVA, SCN_CODE),
        section(".rdata", rdata, rdata_rva, SCN_RDATA),
    ];
    let mut cursor = page_align(rdata_rva + rdata.len() as u32);
    cursor = push_named(&mut parts, meta, cursor)?;
    let infos = unwind_blob(text, funcs);
    let xdata = concat_xdata(meta, &infos);
    parts.push(section(".xdata", &xdata, cursor, SCN_RDATA));
    let pdata = pdata_bytes(funcs, &infos, cursor + xdata_prefix(meta));
    cursor = page_align(cursor + xdata.len() as u32);
    parts.push(section(".pdata", &pdata, cursor, SCN_RDATA));
    cursor = page_align(cursor + pdata.len() as u32);
    let reloc = reloc_bytes(abs);
    parts.push(section(".reloc", &reloc, cursor, SCN_RELOC));
    cursor = page_align(cursor + reloc.len() as u32);
    parts.push(Section {
        name: name_bytes(".bss"),
        bytes: Vec::new(),
        rva: cursor,
        characteristics: SCN_BSS,
        raw: false,
    });
    let start = symbols
        .get("_start")
        .copied()
        .ok_or_else(|| PeError::new("入口符号缺失"))?;
    let count = parts.len() as u32;
    Ok((assemble(&parts, start, kind, cursor)?, count, start))
}

fn push_named(
    parts: &mut Vec<Section>,
    meta: &[(&str, &[u8])],
    mut cursor: u32,
) -> Result<u32, PeError> {
    for (name, bytes) in meta {
        if *name == ".gugu.unwind" {
            continue;
        }
        let pe_name = match *name {
            ".gugu.meta" => ".ggmeta",
            other if other.len() <= 8 => other,
            _ => return Err(PeError::new("节名超出 8 字节")),
        };
        let payload = if bytes.is_empty() { &[0][..] } else { bytes };
        parts.push(section(pe_name, payload, cursor, SCN_RDATA));
        cursor = page_align(cursor + payload.len() as u32);
    }
    Ok(cursor)
}

fn xdata_prefix(meta: &[(&str, &[u8])]) -> u32 {
    let mut len = gugu_unwind_len(meta);
    if len == 0 {
        len = 1;
    }
    while len % 4 != 0 {
        len += 1;
    }
    len as u32
}

fn gugu_unwind_len(meta: &[(&str, &[u8])]) -> usize {
    meta.iter()
        .find(|(name, _)| *name == ".gugu.unwind")
        .map(|(_, bytes)| bytes.len())
        .unwrap_or(0)
}

fn concat_xdata(meta: &[(&str, &[u8])], infos: &[Vec<u8>]) -> Vec<u8> {
    let mut out = meta
        .iter()
        .find(|(name, _)| *name == ".gugu.unwind")
        .map(|(_, bytes)| bytes.to_vec())
        .unwrap_or_default();
    if out.is_empty() {
        out.push(0);
    }
    while out.len() % 4 != 0 {
        out.push(0);
    }
    for info in infos {
        out.extend_from_slice(info);
    }
    out
}

fn section(name: &str, bytes: &[u8], rva: u32, characteristics: u32) -> Section {
    Section {
        name: name_bytes(name),
        bytes: bytes.to_vec(),
        rva,
        characteristics,
        raw: true,
    }
}

fn name_bytes(name: &str) -> [u8; 8] {
    let mut out = [0; 8];
    let raw = name.as_bytes();
    out[..raw.len()].copy_from_slice(raw);
    out
}

fn assemble(
    parts: &[Section],
    entry: u32,
    kind: ImageKind,
    bss_rva: u32,
) -> Result<Vec<u8>, PeError> {
    let headers = align_up(0x80 + 4 + 20 + 240 + 40 * parts.len() as u32, FILE_ALIGN);
    let mut raw_at = headers;
    let mut file = vec![0; headers as usize];
    write_headers(&mut file, parts, entry, kind, headers, bss_rva, &mut raw_at)?;
    for part in parts {
        if !part.raw {
            continue;
        }
        let at = file.len();
        file.extend_from_slice(&part.bytes);
        while file.len() % FILE_ALIGN as usize != 0 {
            file.push(0);
        }
        let _ = at;
    }
    let _ = raw_at;
    Ok(file)
}

fn write_headers(
    file: &mut [u8],
    parts: &[Section],
    entry: u32,
    kind: ImageKind,
    headers: u32,
    bss_rva: u32,
    raw_at: &mut u32,
) -> Result<(), PeError> {
    file[0] = b'M';
    file[1] = b'Z';
    write_u32(file, 0x3c, 0x80);
    file[0x80..0x84].copy_from_slice(b"PE\0\0");
    write_coff(file, parts.len() as u16, kind);
    write_optional(file, parts, entry, headers, bss_rva)?;
    let mut cursor = 0x80 + 4 + 20 + 240;
    for part in parts {
        write_section_header(file, cursor, part, raw_at)?;
        cursor += 40;
    }
    Ok(())
}

fn write_coff(file: &mut [u8], count: u16, kind: ImageKind) {
    let coff = 0x84;
    write_u16(file, coff, 0x8664);
    write_u16(file, coff + 2, count);
    write_u16(file, coff + 16, 240);
    let mut flags = 0x0022u16;
    if kind == ImageKind::Dll {
        flags |= 0x2000;
    }
    write_u16(file, coff + 18, flags);
}

fn write_optional(
    file: &mut [u8],
    parts: &[Section],
    entry: u32,
    headers: u32,
    bss_rva: u32,
) -> Result<(), PeError> {
    let at = 0x84 + 20;
    write_u16(file, at, 0x20b);
    let code = parts
        .iter()
        .find(|part| part.name.starts_with(b".text"))
        .map(|part| align_up(part.bytes.len() as u32, FILE_ALIGN))
        .unwrap_or(0);
    let init = parts
        .iter()
        .filter(|part| part.raw && !part.name.starts_with(b".text"))
        .map(|part| align_up(part.bytes.len() as u32, FILE_ALIGN))
        .sum();
    write_u32(file, at + 4, code);
    write_u32(file, at + 8, init);
    write_u32(file, at + 12, 4096);
    write_u32(file, at + 16, entry);
    write_u32(file, at + 20, TEXT_RVA);
    write_u64(file, at + 24, IMAGE_BASE);
    write_u32(file, at + 32, PAGE);
    write_u32(file, at + 36, FILE_ALIGN);
    write_u16(file, at + 40, 6);
    write_u16(file, at + 48, 6);
    let image = page_align(bss_rva + 4096);
    write_u32(file, at + 56, image);
    write_u32(file, at + 60, headers);
    write_u16(file, at + 68, 3);
    write_u16(file, at + 70, 0x0160);
    write_u64(file, at + 72, 0x10_0000);
    write_u64(file, at + 80, 0x1000);
    write_u64(file, at + 88, 0x10_0000);
    write_u64(file, at + 96, 0x1000);
    write_u32(file, at + 108, 16);
    write_directories(file, at + 112, parts)?;
    Ok(())
}

fn write_directories(file: &mut [u8], at: usize, parts: &[Section]) -> Result<(), PeError> {
    let import = find_section(parts, b".rdata")?;
    let pdata = find_section(parts, b".pdata")?;
    let reloc = find_section(parts, b".reloc")?;
    let (import_rva, import_size, iat_rva, iat_size) = import_span(&import.bytes, import.rva);
    write_dir(file, at, 1, import_rva, import_size);
    write_dir(file, at, 3, pdata.rva, pdata.bytes.len() as u32);
    write_dir(file, at, 5, reloc.rva, reloc.bytes.len() as u32);
    write_dir(file, at, 12, iat_rva, iat_size);
    Ok(())
}

fn write_dir(file: &mut [u8], at: usize, index: usize, rva: u32, size: u32) {
    let slot = at + index * 8;
    write_u32(file, slot, rva);
    write_u32(file, slot + 4, size);
}

fn write_section_header(
    file: &mut [u8],
    at: usize,
    part: &Section,
    raw_at: &mut u32,
) -> Result<(), PeError> {
    if part.name.len() > 8 {
        return Err(PeError::new("节名超出 8 字节"));
    }
    file[at..at + 8].copy_from_slice(&part.name);
    let virtual_size = if part.raw {
        part.bytes.len().max(1) as u32
    } else {
        4096
    };
    write_u32(file, at + 8, virtual_size);
    write_u32(file, at + 12, part.rva);
    if part.raw {
        let raw_size = align_up(part.bytes.len() as u32, FILE_ALIGN);
        write_u32(file, at + 16, raw_size);
        write_u32(file, at + 20, *raw_at);
        *raw_at += raw_size;
    }
    write_u32(file, at + 36, part.characteristics);
    Ok(())
}

fn find_section<'a>(parts: &'a [Section], name: &[u8]) -> Result<&'a Section, PeError> {
    parts
        .iter()
        .find(|part| part.name.starts_with(name))
        .ok_or_else(|| PeError::new("节缺失"))
}

fn import_span(rdata: &[u8], rva: u32) -> (u32, u32, u32, u32) {
    let dlls = IMPORTS.iter().map(|(dll, _)| *dll).collect::<BTreeSet<_>>();
    let desc = (dlls.len() + 1) * 20;
    let thunks: usize = dlls
        .iter()
        .map(|dll| (IMPORTS.iter().filter(|(name, _)| name == dll).count() + 1) * 8)
        .sum();
    let _ = rdata;
    (
        rva,
        desc as u32,
        rva + desc as u32 + thunks as u32,
        thunks as u32,
    )
}

fn finish(bytes: Vec<u8>, entry: u64, relocs: u32, sections: u32, archive: Vec<u8>) -> PeImage {
    let fingerprint = crate::frontend::mono::keys::hash_domain(PE_DOMAIN, &bytes);
    PeImage {
        bytes,
        schema: PE_SCHEMA,
        entry,
        sections,
        imports: IMPORTS.len() as u32,
        relocs,
        fingerprint,
        archive,
    }
}

fn build_imports(rdata_rva: u32) -> Result<ImportDir, PeError> {
    let grouped = grouped_imports();
    let desc_len = (grouped.len() + 1) * 20;
    let thunks: usize = grouped.iter().map(|(_, names)| (names.len() + 1) * 8).sum();
    let mut bytes = vec![0; desc_len + thunks + thunks];
    let mut names = Vec::new();
    let mut slots = BTreeMap::new();
    let mut thunk_at = 0;
    for (index, (dll, symbols)) in grouped.iter().enumerate() {
        let dll_off = append_cstr(&mut names, dll);
        let ilt = rdata_rva + desc_len as u32 + thunk_at as u32;
        let iat = rdata_rva + (desc_len + thunks) as u32 + thunk_at as u32;
        let dll_rva = rdata_rva + (desc_len + thunks * 2) as u32 + dll_off as u32;
        write_u32(&mut bytes, index * 20, ilt);
        write_u32(&mut bytes, index * 20 + 12, dll_rva);
        write_u32(&mut bytes, index * 20 + 16, iat);
        thunk_at = write_thunks(
            &mut bytes, &mut names, symbols, dll, rdata_rva, desc_len, thunks, thunk_at, &mut slots,
        )?;
    }
    bytes.extend(names);
    Ok(ImportDir { bytes, slots })
}

fn write_thunks(
    bytes: &mut [u8],
    names: &mut Vec<u8>,
    symbols: &[&str],
    dll: &str,
    rdata_rva: u32,
    desc_len: usize,
    thunks: usize,
    mut thunk_at: usize,
    slots: &mut BTreeMap<String, u32>,
) -> Result<usize, PeError> {
    for symbol in symbols {
        align_to(names, 2);
        let hint = names.len();
        names.extend_from_slice(&0u16.to_le_bytes());
        names.extend(symbol.as_bytes());
        names.push(0);
        let hint_rva = rdata_rva + (desc_len + thunks * 2) as u32 + hint as u32;
        write_u64(bytes, desc_len + thunk_at, u64::from(hint_rva));
        write_u64(bytes, desc_len + thunks + thunk_at, u64::from(hint_rva));
        let iat = rdata_rva + (desc_len + thunks) as u32 + thunk_at as u32;
        slots.insert(format!("{dll}!{symbol}"), iat);
        thunk_at += 8;
    }
    Ok(thunk_at + 8)
}

fn grouped_imports() -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut grouped = BTreeMap::new();
    for (dll, name) in IMPORTS {
        grouped.entry(*dll).or_insert_with(Vec::new).push(*name);
    }
    grouped
}

fn append_cstr(names: &mut Vec<u8>, text: &str) -> usize {
    let at = names.len();
    names.extend(text.as_bytes());
    names.push(0);
    at
}

fn unwind_blob(text: &[u8], funcs: &[Func]) -> Vec<Vec<u8>> {
    funcs
        .iter()
        .map(|func| unwind_info(&text[func.bytes_at..func.bytes_at + func.len], func.frame))
        .collect()
}

fn unwind_info(bytes: &[u8], frame: u32) -> Vec<u8> {
    let (end, alloc) = detect_alloc(bytes, frame);
    if alloc == 0 || alloc % 8 != 0 {
        return vec![1, 0, 0, 0];
    }
    let prolog = u8::try_from(end).unwrap_or(u8::MAX);
    if (8..=128).contains(&alloc) {
        let info = ((alloc - 8) / 8) << 4;
        let mut bytes = vec![1, prolog, 1, 0, prolog, (info as u8) | 2];
        pad4(&mut bytes);
        return bytes;
    }
    let mut bytes = vec![1, prolog, 1, 0, prolog, 1];
    bytes.extend_from_slice(&((alloc / 8) as u16).to_le_bytes());
    pad4(&mut bytes);
    bytes
}

fn detect_alloc(bytes: &[u8], frame: u32) -> (usize, u32) {
    if frame == 0 {
        return (0, 0);
    }
    let Some((end, _)) = find_sub_rsp(bytes) else {
        return (0, 0);
    };
    (end, frame)
}

fn find_sub_rsp(bytes: &[u8]) -> Option<(usize, u32)> {
    let mut index = 0;
    while index + 4 <= bytes.len() {
        if bytes[index] == 0x48 && bytes[index + 1] == 0x83 && bytes[index + 2] == 0xec {
            return Some((index + 4, u32::from(bytes[index + 3])));
        }
        if index + 7 <= bytes.len()
            && bytes[index] == 0x48
            && bytes[index + 1] == 0x81
            && bytes[index + 2] == 0xec
        {
            let imm = u32::from_le_bytes(bytes[index + 3..index + 7].try_into().expect("imm32"));
            return Some((index + 7, imm));
        }
        index += 1;
    }
    None
}

fn pdata_bytes(funcs: &[Func], infos: &[Vec<u8>], info_base: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cursor = info_base;
    for (func, info) in funcs.iter().zip(infos) {
        out.extend_from_slice(&func.rva.to_le_bytes());
        let end = func.rva + func.len as u32;
        out.extend_from_slice(&end.to_le_bytes());
        out.extend_from_slice(&cursor.to_le_bytes());
        cursor += info.len() as u32;
        while cursor % 4 != 0 {
            cursor += 1;
        }
    }
    if out.is_empty() {
        out.push(0);
    }
    out
}

fn reloc_bytes(sites: &[u32]) -> Vec<u8> {
    if sites.is_empty() {
        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&12u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        return out;
    }
    let mut pages: BTreeMap<u32, Vec<u16>> = BTreeMap::new();
    for site in sites {
        pages
            .entry(site & !0xFFF)
            .or_default()
            .push((site & 0xFFF) as u16);
    }
    let mut out = Vec::new();
    for (page, offsets) in pages {
        let mut entries = Vec::new();
        for offset in offsets {
            entries.extend_from_slice(&(0xA000 | offset).to_le_bytes());
        }
        if entries.len() % 4 != 0 {
            entries.extend_from_slice(&0u16.to_le_bytes());
        }
        out.extend_from_slice(&page.to_le_bytes());
        out.extend_from_slice(&(8 + entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&entries);
    }
    out
}

fn pad4(bytes: &mut Vec<u8>) {
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
}

fn align_to(bytes: &mut Vec<u8>, align: usize) {
    let align = align.max(1);
    while bytes.len() % align != 0 {
        bytes.push(0);
    }
}

fn page_align(value: u32) -> u32 {
    value.div_ceil(PAGE) * PAGE
}

fn align_up(value: u32, align: u32) -> u32 {
    value.div_ceil(align) * align
}

fn write_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
