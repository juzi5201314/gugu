//! 把逻辑节、运行时入口和相对重定位收成一张 ELF64 static PIE。

use std::collections::{BTreeMap, BTreeSet};

use super::super::inst::{RelocKind, RelocTarget};
use super::super::mangle::{mangle_glue, mangle_runtime, mangle_symbol};
use super::archive;
use super::runtime::{self, Rt0Info};
use super::{BootOffsets, ELF_DOMAIN, ELF_SCHEMA, ET_DYN, ElfError, ElfImage};

const PAGE: u64 = 4096;
const TEXT_VADDR: u64 = PAGE;
const PHDR_VADDR: u64 = 64;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_GNU_STACK: u32 = 0x6474_e551;
const PT_GNU_RELRO: u32 = 0x6474_e552;
const SHT_PROGBITS: u32 = 1;
const SHT_STRTAB: u32 = 3;
const SHT_NOBITS: u32 = 8;
const SHF_WRITE: u64 = 1;
const SHF_ALLOC: u64 = 2;
const SHF_EXEC: u64 = 4;

#[derive(Clone, Debug)]
pub(super) struct LinkCode {
    pub(super) symbol: String,
    pub(super) bytes: Vec<u8>,
    pub(super) included: bool,
    pub(super) relocs: Vec<LinkReloc>,
}

#[derive(Clone, Debug)]
pub(super) struct LinkReloc {
    pub(super) offset: u32,
    pub(super) kind: LinkRelocKind,
    pub(super) target: String,
    pub(super) addend: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LinkRelocKind {
    PcRel32,
    Abs64,
    Rva32,
}

impl LinkReloc {
    pub(super) fn from_fragment(reloc: &super::super::inst::Relocation) -> Self {
        let kind = match reloc.kind {
            RelocKind::PcRel32 => LinkRelocKind::PcRel32,
            RelocKind::Abs64 => LinkRelocKind::Abs64,
            RelocKind::Rva32 => LinkRelocKind::Rva32,
        };
        let target = match &reloc.target {
            RelocTarget::Lir(symbol) => mangle_symbol(symbol),
            RelocTarget::CageControl => "cage-control".to_owned(),
            RelocTarget::Cold(edge) => format!("cold:{}", edge.site),
        };
        Self {
            offset: reloc.offset,
            kind,
            target,
            addend: reloc.addend,
        }
    }
}

pub(super) struct LinkRequest<'a> {
    pub(super) codes: &'a [LinkCode],
    pub(super) consts: &'a [(String, Vec<u8>, u32)],
    pub(super) sections: &'a [(&'a str, &'a [u8])],
    pub(super) entry: &'a str,
    pub(super) interpreter: Option<&'a str>,
    pub(super) archives: &'a [&'a [u8]],
    pub(super) boot: BootOffsets,
    pub(super) emit_runtime: bool,
}

struct Section {
    name: String,
    addr: u64,
    offset: u64,
    size: u64,
    flags: u64,
    kind: u32,
}

pub(super) fn reject_writable_executable(flags: u32) -> Result<(), ElfError> {
    if flags & PF_X != 0 && flags & PF_W != 0 {
        return Err(ElfError::new("节权限不能同时可写可执行"));
    }
    Ok(())
}

pub(super) fn pc_disp(field: u64, target: i64) -> Result<i32, ElfError> {
    i32::try_from(target - (field as i64 + 4)).map_err(|_| ElfError::new("pc-rel32 超出范围"))
}

/// 把链接虚址上的相对重定位表应用到一份已装入的镜像副本。重复写同一槽是错误。
pub(super) fn apply_relative(base: u64, image: &mut [u8], table: usize) -> Result<(), ElfError> {
    let count = read_u64(image, table)?;
    for index in 0..count {
        let at = table + 8 + index as usize * 16;
        let offset = read_u64(image, at)?;
        let addend = read_u64(image, at + 8)?;
        write_slot(image, offset, base.wrapping_add(addend))?;
    }
    Ok(())
}

pub(super) fn link(request: &LinkRequest<'_>) -> Result<ElfImage, ElfError> {
    reject_codes(request)?;
    let archives = load_archives(request.archives)?;
    let mut symbols = BTreeMap::new();
    let mut text = Vec::new();
    place_user(&mut text, request.codes, &mut symbols)?;
    if request.emit_runtime {
        place_runtime(&mut text, request.boot, &mut symbols)?;
    }
    place_named(&mut text, "__gugu_trap", &runtime::trap(), &mut symbols)?;
    let pending = unresolved(request, &symbols);
    let mut dynamic = Vec::new();
    place_archives(&mut text, &pending, &archives, &mut symbols, &mut dynamic)?;
    if !dynamic.is_empty() && request.interpreter.is_none() {
        return Err(ElfError::new("动态导入缺少解释器"));
    }
    let rt0_len = usize::from(request.emit_runtime) * measure_rt0(request.boot);
    let prefix = text.len();
    let text_end = TEXT_VADDR + prefix as u64 + dynamic.len() as u64 * 6 + rt0_len as u64;
    let ro_vaddr = page_align(text_end);
    let (mut rodata, mut sections, ro_symbols, table, sentinel) =
        layout_ro(request, &dynamic, ro_vaddr)?;
    let relro_vaddr = page_align(ro_vaddr + rodata.len() as u64);
    patch_reloc_table(&mut rodata, table - ro_vaddr, relro_vaddr, sentinel)?;
    let (relro, slot) = layout_relro(&mut symbols, relro_vaddr, &dynamic);
    symbols.extend(ro_symbols);
    place_plt(&mut text, &dynamic, &mut symbols)?;
    patch_codes(&mut text, request.codes, &symbols)?;
    let start = emit_entry(
        &mut text,
        request,
        &symbols,
        relro_vaddr,
        relro.len() as u64,
        slot,
        table,
    )?;
    if text.len() != prefix + dynamic.len() * 6 + rt0_len {
        return Err(ElfError::new("运行时入口长度与布局不一致"));
    }
    sections.extend(image_sections(text.len() as u64, relro_vaddr, &relro));
    encode_elf(
        &text,
        &rodata,
        &relro,
        ro_vaddr,
        relro_vaddr,
        start,
        &sections,
        table,
        sentinel,
    )
}

fn reject_codes(request: &LinkRequest<'_>) -> Result<(), ElfError> {
    if request
        .codes
        .iter()
        .all(|code| code.symbol != request.entry)
    {
        return Err(ElfError::new("入口符号缺失"));
    }
    let mut seen = BTreeSet::new();
    for code in request.codes {
        if !seen.insert(code.symbol.clone()) {
            return Err(ElfError::new("符号重复"));
        }
        check_relocs(code)?;
    }
    Ok(())
}

fn check_relocs(code: &LinkCode) -> Result<(), ElfError> {
    let mut fields = BTreeSet::new();
    for reloc in &code.relocs {
        if !fields.insert(reloc.offset) {
            return Err(ElfError::new("重定位重复"));
        }
        let width = match reloc.kind {
            LinkRelocKind::PcRel32 | LinkRelocKind::Rva32 => 4,
            LinkRelocKind::Abs64 => 8,
        };
        if reloc.offset as usize + width > code.bytes.len() {
            return Err(ElfError::new("重定位偏移越界"));
        }
    }
    Ok(())
}

fn load_archives(archives: &[&[u8]]) -> Result<BTreeMap<String, Vec<u8>>, ElfError> {
    let mut all = BTreeMap::new();
    for archive in archives {
        for (name, bytes) in archive::extract(archive)? {
            if all.insert(name, bytes).is_some() {
                return Err(ElfError::new("静态归档成员重复"));
            }
        }
    }
    Ok(all)
}

fn place_user(
    text: &mut Vec<u8>,
    codes: &[LinkCode],
    symbols: &mut BTreeMap<String, u64>,
) -> Result<(), ElfError> {
    let mut included: Vec<&LinkCode> = codes.iter().filter(|code| code.included).collect();
    let mut rest: Vec<&LinkCode> = codes.iter().filter(|code| !code.included).collect();
    included.sort_by(|left, right| left.symbol.cmp(&right.symbol));
    rest.sort_by(|left, right| left.symbol.cmp(&right.symbol));
    for code in included.into_iter().chain(rest) {
        place_named(text, &code.symbol, &code.bytes, symbols)?;
    }
    Ok(())
}

fn place_runtime(
    text: &mut Vec<u8>,
    boot: BootOffsets,
    symbols: &mut BTreeMap<String, u64>,
) -> Result<(), ElfError> {
    let mut bodies = runtime::named_bodies(boot);
    bodies.sort_by(|left, right| left.0.cmp(right.0));
    for (name, bytes, is_runtime) in bodies {
        let symbol = if is_runtime {
            mangle_runtime(name)
        } else {
            mangle_glue(name)
        };
        place_named(text, &symbol, &bytes, symbols)?;
    }
    Ok(())
}

fn place_named(
    text: &mut Vec<u8>,
    name: &str,
    bytes: &[u8],
    symbols: &mut BTreeMap<String, u64>,
) -> Result<(), ElfError> {
    while text.len() % 16 != 0 {
        text.push(0x90);
    }
    let addr = TEXT_VADDR + text.len() as u64;
    if symbols.insert(name.to_owned(), addr).is_some() {
        return Err(ElfError::new("符号重复"));
    }
    text.extend_from_slice(bytes);
    Ok(())
}

fn unresolved(request: &LinkRequest<'_>, symbols: &BTreeMap<String, u64>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for code in request.codes {
        for reloc in &code.relocs {
            if reloc.target.starts_with("cold:") || symbols.contains_key(&reloc.target) {
                continue;
            }
            names.insert(reloc.target.clone());
        }
    }
    for (name, _, _) in request.consts {
        names.remove(name);
    }
    names.remove("cage-control");
    names
}

fn place_archives(
    text: &mut Vec<u8>,
    pending: &BTreeSet<String>,
    archives: &BTreeMap<String, Vec<u8>>,
    symbols: &mut BTreeMap<String, u64>,
    dynamic: &mut Vec<String>,
) -> Result<(), ElfError> {
    let mut unknown = Vec::new();
    for name in pending {
        if let Some(bytes) = archives.get(name) {
            place_named(text, name, bytes, symbols)?;
        } else if name.starts_with("__gugu_") {
            unknown.push(name.clone());
        } else {
            dynamic.push(name.clone());
        }
    }
    if !unknown.is_empty() {
        return Err(ElfError::new(format!(
            "未知运行时符号 {}",
            unknown.join(" ")
        )));
    }
    Ok(())
}

fn measure_rt0(boot: BootOffsets) -> usize {
    runtime::rt0(&Rt0Info {
        origin: 0x10_0000,
        phdr_vaddr: PHDR_VADDR,
        reloc_table: 0x20_0000,
        relro_vaddr: 0x30_0000,
        relro_size: PAGE,
        sentinel_slot: 0x30_0000,
        entry: 0x10_1000,
        boot,
    })
    .len()
}

fn layout_ro(
    request: &LinkRequest<'_>,
    dynamic: &[String],
    ro_vaddr: u64,
) -> Result<(Vec<u8>, Vec<Section>, BTreeMap<String, u64>, u64, u64), ElfError> {
    let mut bytes = Vec::new();
    let mut symbols = BTreeMap::new();
    let mut sections = Vec::new();
    push_interp(request, dynamic, ro_vaddr, &mut bytes, &mut sections);
    let rodata_at = bytes.len() as u64;
    if needs_cage(request) {
        align_vec(&mut bytes, 8);
        let addr = ro_vaddr + bytes.len() as u64;
        symbols.insert("cage-control".to_owned(), addr);
        bytes.extend(std::iter::repeat_n(0, 64));
    }
    align_vec(&mut bytes, 8);
    let sentinel = ro_vaddr + bytes.len() as u64;
    bytes.push(1);
    align_vec(&mut bytes, 8);
    let table = ro_vaddr + bytes.len() as u64;
    bytes.extend(std::iter::repeat_n(0, 24));
    symbols.insert("__gugu_reloc_table".to_owned(), table);
    symbols.insert("__gugu_sentinel".to_owned(), sentinel);
    place_consts(request, ro_vaddr, &mut bytes, &mut symbols)?;
    let ro_size = bytes.len() as u64 - rodata_at;
    sections.push(prog_section(
        ".rodata",
        ro_vaddr + rodata_at,
        ro_size,
        SHF_ALLOC,
        SHT_PROGBITS,
    ));
    for (name, blob) in request.sections {
        align_vec(&mut bytes, 16);
        let addr = ro_vaddr + bytes.len() as u64;
        sections.push(prog_section(
            name,
            addr,
            blob.len() as u64,
            SHF_ALLOC,
            SHT_PROGBITS,
        ));
        bytes.extend_from_slice(blob);
    }
    Ok((bytes, sections, symbols, table, sentinel))
}

fn needs_cage(request: &LinkRequest<'_>) -> bool {
    request.codes.iter().any(|code| {
        code.relocs
            .iter()
            .any(|reloc| reloc.target == "cage-control")
    })
}

fn push_interp(
    request: &LinkRequest<'_>,
    dynamic: &[String],
    ro_vaddr: u64,
    bytes: &mut Vec<u8>,
    sections: &mut Vec<Section>,
) {
    let Some(path) = request.interpreter else {
        return;
    };
    if dynamic.is_empty() {
        return;
    }
    let mut text = path.as_bytes().to_vec();
    text.push(0);
    let addr = ro_vaddr + bytes.len() as u64;
    sections.push(prog_section(
        ".interp",
        addr,
        text.len() as u64,
        SHF_ALLOC,
        SHT_PROGBITS,
    ));
    bytes.extend(text);
}

fn place_consts(
    request: &LinkRequest<'_>,
    ro_vaddr: u64,
    bytes: &mut Vec<u8>,
    symbols: &mut BTreeMap<String, u64>,
) -> Result<(), ElfError> {
    for (name, blob, align) in request.consts {
        align_vec(bytes, (*align).max(1));
        let addr = ro_vaddr + bytes.len() as u64;
        if symbols.insert(name.clone(), addr).is_some() {
            return Err(ElfError::new("符号重复"));
        }
        bytes.extend_from_slice(blob);
    }
    Ok(())
}

fn patch_reloc_table(
    rodata: &mut [u8],
    table_off: u64,
    slot: u64,
    sentinel: u64,
) -> Result<(), ElfError> {
    let at = usize::try_from(table_off).map_err(|_| ElfError::new("重定位表偏移越界"))?;
    rodata[at..at + 8].copy_from_slice(&1_u64.to_le_bytes());
    rodata[at + 8..at + 16].copy_from_slice(&slot.to_le_bytes());
    rodata[at + 16..at + 24].copy_from_slice(&sentinel.to_le_bytes());
    Ok(())
}

fn layout_relro(
    symbols: &mut BTreeMap<String, u64>,
    relro_vaddr: u64,
    dynamic: &[String],
) -> (Vec<u8>, u64) {
    symbols.insert("__gugu_reloc_slot".to_owned(), relro_vaddr);
    for (index, name) in dynamic.iter().enumerate() {
        let addr = relro_vaddr + 8 + index as u64 * 8;
        symbols.insert(format!("__gugu_got_{name}"), addr);
    }
    (vec![0_u8; PAGE as usize], relro_vaddr)
}

fn place_plt(
    text: &mut Vec<u8>,
    dynamic: &[String],
    symbols: &mut BTreeMap<String, u64>,
) -> Result<(), ElfError> {
    for name in dynamic {
        let addr = TEXT_VADDR + text.len() as u64;
        let got = *symbols
            .get(&format!("__gugu_got_{name}"))
            .ok_or_else(|| ElfError::new("GOT 缺失"))?;
        let disp = pc_disp(addr + 2, got as i64)?;
        text.extend_from_slice(&[0xff, 0x25]);
        text.extend_from_slice(&disp.to_le_bytes());
        if symbols.insert(name.clone(), addr).is_some() {
            return Err(ElfError::new("符号重复"));
        }
    }
    Ok(())
}

fn patch_codes(
    text: &mut [u8],
    codes: &[LinkCode],
    symbols: &BTreeMap<String, u64>,
) -> Result<(), ElfError> {
    for code in codes {
        let base = *symbols
            .get(&code.symbol)
            .ok_or_else(|| ElfError::new("符号缺失"))?;
        for reloc in &code.relocs {
            patch_one(text, base, reloc, symbols)?;
        }
    }
    Ok(())
}

fn patch_one(
    text: &mut [u8],
    base: u64,
    reloc: &LinkReloc,
    symbols: &BTreeMap<String, u64>,
) -> Result<(), ElfError> {
    let target = resolve(reloc, symbols)?;
    let field = base + u64::from(reloc.offset);
    let at = usize::try_from(field - TEXT_VADDR).map_err(|_| ElfError::new("重定位偏移越界"))?;
    match reloc.kind {
        LinkRelocKind::PcRel32 => {
            let disp = pc_disp(field, target as i64 + reloc.addend)?;
            text[at..at + 4].copy_from_slice(&disp.to_le_bytes());
        }
        LinkRelocKind::Rva32 => {
            let value = u32::try_from(target as i64 + reloc.addend)
                .map_err(|_| ElfError::new("rva32 超出范围"))?;
            text[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        LinkRelocKind::Abs64 => {
            return Err(ElfError::new("绝对重定位落在不可写节"));
        }
    }
    Ok(())
}

fn resolve(reloc: &LinkReloc, symbols: &BTreeMap<String, u64>) -> Result<u64, ElfError> {
    if let Some(addr) = symbols.get(&reloc.target) {
        return Ok(*addr);
    }
    if reloc.target.starts_with("cold:") {
        return symbols
            .get("__gugu_trap")
            .copied()
            .ok_or_else(|| ElfError::new("冷边没有陷阱入口"));
    }
    Err(ElfError::new(format!("未知符号 {}", reloc.target)))
}

fn emit_entry(
    text: &mut Vec<u8>,
    request: &LinkRequest<'_>,
    symbols: &BTreeMap<String, u64>,
    relro_vaddr: u64,
    relro_size: u64,
    slot: u64,
    table: u64,
) -> Result<u64, ElfError> {
    let user = *symbols
        .get(request.entry)
        .ok_or_else(|| ElfError::new("入口符号缺失"))?;
    if !request.emit_runtime {
        return Ok(user);
    }
    let origin = TEXT_VADDR + text.len() as u64;
    let bytes = runtime::rt0(&Rt0Info {
        origin,
        phdr_vaddr: PHDR_VADDR,
        reloc_table: table,
        relro_vaddr,
        relro_size,
        sentinel_slot: slot,
        entry: user,
        boot: request.boot,
    });
    text.extend(bytes);
    Ok(origin)
}

fn image_sections(text_len: u64, relro: u64, relro_bytes: &[u8]) -> Vec<Section> {
    let bss = page_align(relro + relro_bytes.len() as u64);
    vec![
        prog_section(
            ".text",
            TEXT_VADDR,
            text_len,
            SHF_ALLOC | SHF_EXEC,
            SHT_PROGBITS,
        ),
        prog_section(
            ".data.rel.ro",
            relro,
            relro_bytes.len() as u64,
            SHF_ALLOC | SHF_WRITE,
            SHT_PROGBITS,
        ),
        prog_section(".bss", bss, PAGE, SHF_ALLOC | SHF_WRITE, SHT_NOBITS),
    ]
}

fn encode_elf(
    text: &[u8],
    rodata: &[u8],
    relro: &[u8],
    ro_vaddr: u64,
    relro_vaddr: u64,
    entry: u64,
    sections: &[Section],
    table: u64,
    sentinel: u64,
) -> Result<ElfImage, ElfError> {
    let bss = page_align(relro_vaddr + relro.len() as u64);
    let mut file = vec![0_u8; bss as usize];
    file[TEXT_VADDR as usize..TEXT_VADDR as usize + text.len()].copy_from_slice(text);
    file[ro_vaddr as usize..ro_vaddr as usize + rodata.len()].copy_from_slice(rodata);
    file[relro_vaddr as usize..relro_vaddr as usize + relro.len()].copy_from_slice(relro);
    let interp = interp_range(sections);
    let phdrs = program_headers(ro_vaddr, relro_vaddr, bss, interp);
    for header in &phdrs {
        reject_writable_executable(header.flags)?;
    }
    if entry < TEXT_VADDR || entry >= ro_vaddr {
        return Err(ElfError::new("入口不在可执行段内"));
    }
    let (strtab, str_index) = string_table(sections);
    let str_off = file.len() as u64;
    file.extend_from_slice(&strtab);
    let shoff = file.len() as u64;
    write_sections(&mut file, sections, str_off, strtab.len() as u64, str_index);
    let shnum = u16::try_from(sections.len() + 2).expect("节数量");
    let shstrndx = u16::try_from(sections.len() + 1).expect("节名表序号");
    write_ehdr(&mut file, entry, phdrs.len(), shoff, shnum, shstrndx);
    write_phdrs(&mut file, &phdrs);
    verify_reloc_table(&file, table, sentinel)?;
    let fingerprint = crate::frontend::mono::keys::hash_domain(ELF_DOMAIN, &file);
    Ok(ElfImage {
        bytes: file,
        schema: ELF_SCHEMA,
        elf_type: ET_DYN,
        entry,
        load_segments: 4,
        relative_relocs: 1,
        interp: interp
            .map(|(addr, _)| interp_text(rodata, ro_vaddr, addr))
            .unwrap_or_default(),
        fingerprint,
        reloc_table: table,
        sentinel_vaddr: sentinel,
    })
}

fn verify_reloc_table(bytes: &[u8], table: u64, sentinel: u64) -> Result<(), ElfError> {
    let mut copy = bytes.to_vec();
    let at = usize::try_from(table).map_err(|_| ElfError::new("重定位表偏移越界"))?;
    apply_relative(0, &mut copy, at)?;
    let addend = read_u64(&copy, at + 16)?;
    if addend != sentinel {
        return Err(ElfError::new("哨兵重定位加数不一致"));
    }
    let offset = read_u64(&copy, at + 8)?;
    let value = read_u64(
        &copy,
        usize::try_from(offset).map_err(|_| ElfError::new("重定位偏移越界"))?,
    )?;
    if value != sentinel {
        return Err(ElfError::new("哨兵重定位结果不一致"));
    }
    Ok(())
}

fn interp_range(sections: &[Section]) -> Option<(u64, u64)> {
    sections
        .iter()
        .find(|section| section.name == ".interp")
        .map(|section| (section.addr, section.size))
}

fn interp_text(rodata: &[u8], ro_vaddr: u64, addr: u64) -> String {
    let at = (addr - ro_vaddr) as usize;
    let bytes = &rodata[at..];
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn program_headers(
    ro_vaddr: u64,
    relro_vaddr: u64,
    bss: u64,
    interp: Option<(u64, u64)>,
) -> Vec<Phdr> {
    let mut headers = vec![
        Phdr::new(PT_LOAD, 0, PF_R | PF_X, ro_vaddr, ro_vaddr, PAGE),
        Phdr::new(
            PT_LOAD,
            ro_vaddr,
            PF_R,
            relro_vaddr - ro_vaddr,
            relro_vaddr - ro_vaddr,
            PAGE,
        ),
        Phdr::new(PT_LOAD, relro_vaddr, PF_R | PF_W, PAGE, PAGE, PAGE),
        Phdr::new(PT_LOAD, bss, PF_R | PF_W, 0, PAGE, PAGE),
        Phdr::new(PT_GNU_RELRO, relro_vaddr, PF_R, PAGE, PAGE, PAGE),
        Phdr::new(PT_GNU_STACK, 0, PF_R | PF_W, 0, 0, 1),
    ];
    if let Some((addr, size)) = interp {
        headers.insert(0, Phdr::new(PT_INTERP, addr, PF_R, size, size, 1));
    }
    headers
}

struct Phdr {
    kind: u32,
    addr: u64,
    flags: u32,
    filesz: u64,
    memsz: u64,
    align: u64,
}

impl Phdr {
    fn new(kind: u32, addr: u64, flags: u32, filesz: u64, memsz: u64, align: u64) -> Self {
        Self {
            kind,
            addr,
            flags,
            filesz,
            memsz,
            align,
        }
    }
}

fn write_ehdr(file: &mut [u8], entry: u64, phnum: usize, shoff: u64, shnum: u16, shstrndx: u16) {
    file[..16].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    put_u16(file, 16, ET_DYN as u16);
    put_u16(file, 18, 62);
    put_u32(file, 20, 1);
    put_u64(file, 24, entry);
    put_u64(file, 32, PHDR_VADDR);
    put_u64(file, 40, shoff);
    put_u32(file, 48, 0);
    put_u16(file, 52, 64);
    put_u16(file, 54, 56);
    put_u16(file, 56, phnum as u16);
    put_u16(file, 58, 64);
    put_u16(file, 60, shnum);
    put_u16(file, 62, shstrndx);
}

fn write_phdrs(file: &mut [u8], headers: &[Phdr]) {
    for (index, header) in headers.iter().enumerate() {
        let at = 64 + index * 56;
        put_u32(file, at, header.kind);
        put_u32(file, at + 4, header.flags);
        put_u64(file, at + 8, header.addr);
        put_u64(file, at + 16, header.addr);
        put_u64(file, at + 24, header.addr);
        put_u64(file, at + 32, header.filesz);
        put_u64(file, at + 40, header.memsz);
        put_u64(file, at + 48, header.align);
    }
}

fn string_table(sections: &[Section]) -> (Vec<u8>, Vec<u32>) {
    let mut bytes = vec![0];
    let mut index = Vec::new();
    for section in sections {
        index.push(bytes.len() as u32);
        bytes.extend_from_slice(section.name.as_bytes());
        bytes.push(0);
    }
    index.push(bytes.len() as u32);
    bytes.extend_from_slice(b".shstrtab\0");
    (bytes, index)
}

fn write_sections(
    file: &mut Vec<u8>,
    sections: &[Section],
    str_off: u64,
    str_size: u64,
    names: Vec<u32>,
) {
    let shoff = file.len();
    file.extend(std::iter::repeat_n(0, 64 * (sections.len() + 2)));
    for (index, section) in sections.iter().enumerate() {
        write_shdr(file, shoff + 64 * (index + 1), names[index], section);
    }
    let str_section = Section {
        name: ".shstrtab".to_owned(),
        addr: 0,
        offset: str_off,
        size: str_size,
        flags: 0,
        kind: SHT_STRTAB,
    };
    write_shdr(
        file,
        shoff + 64 * (sections.len() + 1),
        names[sections.len()],
        &str_section,
    );
}

fn write_shdr(file: &mut [u8], at: usize, name: u32, section: &Section) {
    let stored = if section.kind == SHT_NOBITS {
        section.addr
    } else if section.addr == 0 {
        section.offset
    } else {
        section.addr
    };
    put_u32(file, at, name);
    put_u32(file, at + 4, section.kind);
    put_u64(file, at + 8, section.flags);
    put_u64(file, at + 16, section.addr);
    put_u64(file, at + 24, stored);
    put_u64(file, at + 32, section.size);
    put_u32(file, at + 40, 0);
    put_u32(file, at + 44, 0);
    put_u64(
        file,
        at + 48,
        if section.kind == SHT_NOBITS { PAGE } else { 1 },
    );
    put_u64(file, at + 56, 0);
}

fn prog_section(name: &str, addr: u64, size: u64, flags: u64, kind: u32) -> Section {
    Section {
        name: name.to_owned(),
        addr,
        offset: addr,
        size,
        flags,
        kind,
    }
}

fn align_vec(bytes: &mut Vec<u8>, align: u32) {
    let align = align.max(1) as usize;
    while bytes.len() % align != 0 {
        bytes.push(0);
    }
}

fn page_align(value: u64) -> u64 {
    (value + PAGE - 1) & !(PAGE - 1)
}

fn write_slot(image: &mut [u8], offset: u64, value: u64) -> Result<(), ElfError> {
    let slot = usize::try_from(offset).map_err(|_| ElfError::new("重定位偏移越界"))?;
    if slot
        .checked_add(8)
        .map(|end| end > image.len())
        .unwrap_or(true)
    {
        return Err(ElfError::new("重定位偏移越界"));
    }
    let current = u64::from_le_bytes(image[slot..slot + 8].try_into().expect("8 字节"));
    if current == value {
        return Err(ElfError::new("相对重定位被重复执行"));
    }
    image[slot..slot + 8].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn read_u64(image: &[u8], at: usize) -> Result<u64, ElfError> {
    let bytes = image
        .get(at..at + 8)
        .ok_or_else(|| ElfError::new("重定位表越界"))?;
    Ok(u64::from_le_bytes(bytes.try_into().expect("8 字节")))
}

fn put_u16(file: &mut [u8], at: usize, value: u16) {
    file[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(file: &mut [u8], at: usize, value: u32) {
    file[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(file: &mut [u8], at: usize, value: u64) {
    file[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
