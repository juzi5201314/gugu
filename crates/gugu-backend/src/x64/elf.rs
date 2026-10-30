//! Linux ELF64 static PIE 与显式动态 FFI 镜像。
//!
//! 无动态导入时写出 `ET_DYN`、不带 `PT_INTERP`。运行时只消费
//! `R_X86_64_RELATIVE`：`bias = AT_PHDR - phdr_vaddr`，`*slot = bias + addend`。
//! PC 相对与 RVA 在写出前修完。需要自重定位的槽落在 `.data.rel.ro`，由 rt0
//! `mprotect` 成只读。

use crate::runtime::entry;

use super::codegen::{BlobRelocation, FragmentPayload, X64World};
use super::inst::{RelocKind, RelocTarget};
use super::{fragments, mangle};
use std::collections::{BTreeMap, BTreeSet};
use std::mem::offset_of;

#[path = "elf_codec.rs"]
mod codec;

#[cfg(any(test, feature = "test-support"))]
pub use codec::{extract_archive, pack_archive, rt0_bytes};

const PAGE: u64 = 4096;
/// 类型描述符记录的宿主 section；`Symbol::TypeDescriptor` 的地址由它派生。
const TYPE_SECTION_NAME: &str = ".gugu.types";
const GC_METADATA_NAME: &str = ".gugu.meta";
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PT_GNU_RELRO: u32 = 0x6474_e552;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_STRTAB: i64 = 5;
const DT_STRSZ: i64 = 10;
const PLT_LEN: u64 = 6;

/// 链接一个已编码片段世界。Windows 调用方不要走这里。
pub fn link_world(world: &X64World, types: &[u8], meta: &[u8]) -> Result<LinuxImage, String> {
    let mut symbols = Vec::new();
    for fragment in &world.fragments {
        symbols.push(symbol_from_fragment(fragment)?);
    }
    for blob in fragments::registered_fragments(world.target) {
        symbols.push(symbol_from_machine_fragment(&blob)?);
    }
    let mut readonly = vec![
        Blob {
            name: world.metadata.stackmap_name.clone(),
            bytes: world.metadata.stackmap_section.clone(),
        },
        Blob {
            name: world.metadata.unwind_name.clone(),
            bytes: world.metadata.unwind_section.clone(),
        },
        Blob {
            name: world.metadata.source_name.clone(),
            bytes: world.metadata.source_section.clone(),
        },
        Blob {
            name: ".gugu.types".to_owned(),
            bytes: types.to_vec(),
        },
        Blob {
            name: ".gugu.meta".to_owned(),
            bytes: meta.to_vec(),
        },
    ];
    readonly.retain(|blob| !blob.bytes.is_empty() || blob.name.starts_with(".gugu"));
    readonly.extend(world.constants.iter().map(|(name, bytes)| Blob {
        name: name.clone(),
        bytes: bytes.clone(),
    }));
    link(&LinkRequest {
        entry: world.entry_symbol.clone(),
        symbols,
        readonly,
        imports: Vec::new(),
        interpreter: None,
        archive: Vec::new(),
        bound_glues: world.glues().bound_symbols(),
        type_descriptors: world.type_descriptors.iter().cloned().collect(),
        globals: world.globals.iter().cloned().collect(),
        blob_relocations: world.blob_relocations.clone(),
    })
}

fn symbol_from_fragment(fragment: &FragmentPayload) -> Result<ObjectSymbol, String> {
    let mut relocs = Vec::new();
    for reloc in &fragment.relocations {
        relocs.push(ObjectReloc {
            offset: reloc.offset,
            kind: match reloc.kind {
                RelocKind::PcRel32 => RelocKindCode::PcRel32,
                RelocKind::Abs64 => RelocKindCode::Abs64,
                RelocKind::Rva32 => RelocKindCode::Rva32,
            },
            target: reloc_name(&reloc.target),
            addend: reloc.addend,
        });
    }
    Ok(ObjectSymbol {
        name: fragment.symbol.clone(),
        bytes: fragment.bytes.clone(),
        relocs,
        aliases: match mangle::mangle_instance(&fragment.instance) {
            instance if instance != fragment.symbol => vec![(instance, 0)],
            _ => Vec::new(),
        },
    })
}

/// 登记机器片段作为一个代码符号；入口名与别名指向同一字节串。
fn symbol_from_machine_fragment(blob: &fragments::FragmentBlob) -> Result<ObjectSymbol, String> {
    let (first, rest) = blob
        .entries
        .split_first()
        .ok_or_else(|| "登记片段缺少入口符号".to_owned())?;
    let mut relocs = Vec::new();
    for reloc in &blob.relocations {
        if reloc.kind != RelocKind::PcRel32 {
            return Err(format!("登记片段 `{}` 只允许 PC 相对修补点", first.glue));
        }
        relocs.push(ObjectReloc {
            offset: reloc.offset,
            kind: RelocKindCode::PcRel32,
            target: reloc_name(&reloc.target),
            addend: reloc.addend,
        });
    }
    let aliases = rest
        .iter()
        .map(|entry| (blob.domain.mangle(entry.glue), entry.offset))
        .collect();
    Ok(ObjectSymbol {
        name: blob.domain.mangle(first.glue),
        bytes: blob.bytes.clone(),
        relocs,
        aliases,
    })
}

fn reloc_name(target: &RelocTarget) -> String {
    match target {
        RelocTarget::Lir(symbol) => mangle::mangle_symbol(symbol),
        RelocTarget::Cold(_) => "__gugu_cold_trap".to_owned(),
        RelocTarget::CageControl => "__gugu_cage_control".to_owned(),
    }
}

/// 只读逻辑节。
#[derive(Clone, Debug)]
pub struct Blob {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// 一个待链接的代码符号。
#[derive(Clone, Debug)]
pub struct ObjectSymbol {
    pub name: String,
    pub bytes: Vec<u8>,
    pub relocs: Vec<ObjectReloc>,
    /// 同一字节串上的别名入口：名字与相对该符号起点的偏移。
    pub aliases: Vec<(String, u32)>,
}

/// 一条尚未解析的重定位。
#[derive(Clone, Debug)]
pub struct ObjectReloc {
    pub offset: u32,
    pub kind: RelocKindCode,
    pub target: String,
    pub addend: i64,
}

/// 写出前可见的重定位种类。`Unknown` 只用于拒绝路径。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelocKindCode {
    PcRel32,
    Abs64,
    Rva32,
    #[allow(dead_code)]
    Unknown(u8),
}

/// 链接请求。`imports` 非空时才写 `PT_INTERP`。
#[derive(Clone, Debug)]
pub struct LinkRequest {
    pub entry: String,
    pub symbols: Vec<ObjectSymbol>,
    pub readonly: Vec<Blob>,
    pub imports: Vec<(String, String)>,
    pub interpreter: Option<String>,
    /// 非空时按未解析的 C 符号名抽取 SysV ar 成员。
    pub archive: Vec<u8>,
    /// 已绑定 glue 的 mangled 符号名；这些名字缺失定义时按 `E0062` 语义失败，不再兜底。
    pub bound_glues: BTreeSet<String>,
    /// 类型描述符符号 → `.gugu.types` 内偏移；这些符号的地址由 type section 派生。
    pub type_descriptors: BTreeMap<String, u64>,
    /// 静态存储块：符号名 → 块字节数（`initialized` 字 + 值槽），按符号名升序。
    pub globals: BTreeMap<String, u64>,
    /// `.gugu.meta` 内待回填的符号地址：`(blob 内偏移, 符号名)`，按偏移升序。
    pub blob_relocations: Vec<BlobRelocation>,
}

/// 写出的 Linux 镜像。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinuxImage {
    pub bytes: Vec<u8>,
    pub kind: String,
    pub entry: u64,
    pub reloc_count: u32,
    pub reloc_vaddr: u64,
    pub load_count: u32,
    pub interpreter: String,
    pub fingerprint: [u8; 32],
}

impl LinuxImage {
    pub fn absent() -> Self {
        Self {
            bytes: Vec::new(),
            kind: String::new(),
            entry: 0,
            reloc_count: 0,
            reloc_vaddr: 0,
            load_count: 0,
            interpreter: String::new(),
            fingerprint: [0; 32],
        }
    }
}

/// 把代码、只读节和可选动态导入收成 ELF64。
pub fn link(request: &LinkRequest) -> Result<LinuxImage, String> {
    let mut request = request.clone();
    codec::absorb_archive(&mut request)?;
    let mut processor = vec![0xff; 4096];
    processor[..64].fill(0);
    request.readonly.push(Blob {
        name: "__gugu_processor".to_owned(),
        bytes: processor,
    });
    if request.imports.is_empty() {
        link_static(&mut request)
    } else {
        let interpreter = request
            .interpreter
            .clone()
            .ok_or_else(|| "动态导入缺少解释器路径".to_owned())?;
        link_dynamic(&mut request, &interpreter)
    }
}

/// 把元数据 blob 内的符号引用回填成链接地址。
///
/// 布局完成后才能解析符号；偏移必须 8 字节对齐且落在 blob 内。缺失符号按 `E0062` 语义失败，
/// 不留零值让运行期读到无效地址。
fn apply_blob_relocations(
    request: &mut LinkRequest,
    defined: &BTreeMap<String, u64>,
) -> Result<(), String> {
    if request.blob_relocations.is_empty() {
        return Ok(());
    }
    let metadata = request
        .readonly
        .iter_mut()
        .find(|blob| blob.name == ".gugu.meta")
        .ok_or_else(|| "元数据重定位缺少 .gugu.meta 段".to_owned())?;
    for relocation in &request.blob_relocations {
        if !relocation.offset.is_multiple_of(8) {
            return Err("元数据重定位偏移未对齐到 8 字节".to_owned());
        }
        let at = usize::try_from(relocation.offset).map_err(|_| "元数据偏移超出宿主")?;
        let slot = metadata
            .bytes
            .get_mut(at..at + 8)
            .ok_or_else(|| "元数据重定位偏移越过 blob".to_owned())?;
        let address = *defined
            .get(&relocation.symbol)
            .ok_or_else(|| format!("元数据重定位符号 {} 没有地址", relocation.symbol))?;
        slot.copy_from_slice(&address.to_le_bytes());
    }
    Ok(())
}

fn link_static(request: &mut LinkRequest) -> Result<LinuxImage, String> {
    finish(request, None, "static-pie")
}

fn link_dynamic(request: &mut LinkRequest, interpreter: &str) -> Result<LinuxImage, String> {
    let mut path = interpreter.as_bytes().to_vec();
    path.push(0);
    request.readonly.insert(
        0,
        Blob {
            name: ".interp".to_owned(),
            bytes: path,
        },
    );
    let (dynstr, dynamic) = codec::dynamic_blobs(request);
    request.readonly.push(Blob {
        name: ".dynstr".to_owned(),
        bytes: dynstr,
    });
    request.readonly.push(Blob {
        name: ".dynamic".to_owned(),
        bytes: dynamic,
    });
    finish(request, Some(interpreter.to_owned()), "dynamic-pie")
}

fn finish(
    request: &mut LinkRequest,
    interpreter: Option<String>,
    kind: &str,
) -> Result<LinuxImage, String> {
    let dynamic = interpreter.is_some();
    let phnum: u16 = if dynamic { 7 } else { 5 };
    let header_end = 64 + u64::from(phnum) * 56;
    let defined = defined_layout(request, header_end)?;
    if !defined.contains_key(&request.entry) {
        return Err(format!("入口符号 {} 不存在", request.entry));
    }
    let start = *defined
        .get("_start")
        .ok_or_else(|| "rt0 入口缺失".to_owned())?;
    resolve_relocs(request, &defined)?;
    let relocs = relative_relocs(request, &defined)?;
    let limit_va = defined["__gugu_stack_limit"];
    let reloc_va = reloc_table_va(limit_va, request)?;
    let relro_end = reloc_va + relocs.len() as u64 * 16;
    let rw_va = align_down(limit_va, PAGE);
    // 静态块排在启动载荷之后，可写区必须覆盖到最后一个块，否则它们落在映射之外。
    let payload_end = defined["__gugu_rt0_payload"] + entry::Rt0Payload::bytes();
    let mut rw_end = payload_end;
    for (name, bytes) in &request.globals {
        let address = defined
            .get(name)
            .ok_or_else(|| format!("静态块 {name} 没有地址"))?;
        rw_end = rw_end.max(address.saturating_add(*bytes));
    }
    let rw_end = align_up(rw_end, PAGE)?;
    apply_blob_relocations(request, &defined)?;
    let image = assemble(
        request,
        &defined,
        interpreter.as_deref(),
        phnum,
        &relocs,
        rw_va,
        rw_end,
        relro_end,
    )?;
    let mut out = LinuxImage {
        bytes: image,
        kind: kind.to_owned(),
        entry: start,
        reloc_count: u32::try_from(relocs.len()).unwrap_or(u32::MAX),
        reloc_vaddr: reloc_va,
        load_count: 3,
        interpreter: interpreter.unwrap_or_default(),
        fingerprint: [0; 32],
    };
    out.fingerprint = fingerprint(&out.bytes);
    confirm_bias_zero(&out.bytes, out.reloc_vaddr, out.reloc_count)?;
    Ok(out)
}

fn confirm_bias_zero(file: &[u8], reloc_vaddr: u64, count: u32) -> Result<(), String> {
    if count == 0 {
        return Ok(());
    }
    let mut copy = file.to_vec();
    apply_relative(&mut copy, reloc_vaddr, count, 0)?;
    if copy != file {
        return Err("加载偏移为 0 时相对重定位必须保持已写出的 addend".to_owned());
    }
    Ok(())
}

fn defined_layout(request: &LinkRequest, header_end: u64) -> Result<BTreeMap<String, u64>, String> {
    let text_off = align_up(header_end, 16)?;
    let mut cursor = text_off;
    let mut map = BTreeMap::new();
    cursor = place(&mut map, "_start", cursor, rt0_code_len())?;
    cursor = place(
        &mut map,
        "__gugu_rt0_params",
        cursor,
        entry::rt0_params::BYTES,
    )?;
    cursor = place(&mut map, "__gugu_cold_trap", cursor, 2)?;
    cursor = place(&mut map, "__gugu_cage_control", cursor, 48)?;
    cursor = place(&mut map, "__gugu_syscall_write", cursor, 8)?;
    cursor = place(&mut map, "__gugu_syscall_mmap", cursor, 8)?;
    cursor = place(&mut map, "__gugu_stub_ret", cursor, 1)?;
    cursor = place(&mut map, "__gugu_stub_exit", cursor, 8)?;
    for index in 0..request.imports.len() {
        let key = plt_name(index);
        cursor = place(&mut map, &key, cursor, PLT_LEN)?;
        let symbol = &request.imports[index].1;
        if !map.contains_key(symbol) {
            let va = map[&key];
            map.insert(symbol.clone(), va);
        }
    }
    cursor = align_up(cursor, 16)?;
    let mut order: Vec<usize> = (0..request.symbols.len()).collect();
    order.sort_by(|left, right| {
        request.symbols[*left]
            .name
            .cmp(&request.symbols[*right].name)
    });
    let mut rva_cursor = 0_u64;
    let fragment_base = cursor;
    for index in order {
        rva_cursor = align_up(rva_cursor, 16)?;
        let symbol = &request.symbols[index];
        if map.contains_key(&symbol.name) {
            return Err(format!("符号 {} 重复定义", symbol.name));
        }
        map.insert(symbol.name.clone(), fragment_base + rva_cursor);
        for (alias, offset) in &symbol.aliases {
            if map
                .insert(
                    alias.clone(),
                    fragment_base + rva_cursor + u64::from(*offset),
                )
                .is_some()
            {
                return Err(format!("符号 {alias} 重复定义"));
            }
        }
        let span = (symbol.bytes.len() as u64).max(1);
        rva_cursor = rva_cursor.saturating_add(span);
    }
    cursor = fragment_base + rva_cursor;
    let ro_va = align_up(cursor, PAGE)?;
    let mut ro_cursor = ro_va;
    for blob in &request.readonly {
        ro_cursor = align_up(ro_cursor, 16)?;
        map.insert(blob.name.clone(), ro_cursor);
        ro_cursor = ro_cursor.saturating_add(blob.bytes.len() as u64);
    }
    let rw_va = align_up(ro_cursor, PAGE)?;
    map.insert("__gugu_stack_limit".to_owned(), rw_va);
    // 保留可写区紧接重定位表之后并独占页：rt0 只把只读区收尾，保留区保持可写。
    let mut reserved = align_up(
        reloc_table_va(rw_va, request)? + reloc_count(request)? * 16,
        PAGE,
    )?;
    map.insert("__gugu_initial_slot".to_owned(), reserved);
    reserved = align_up(reserved + SLOT_BYTES, 64)?;
    map.insert("__gugu_runtime_state".to_owned(), reserved);
    // mailbox 是 runtime 状态区首部的别名：morestack 与返回哨兵片段都按这个符号打补丁。
    map.insert(mangle::mangle_runtime("morestack_mailbox"), reserved);
    reserved = align_up(reserved + STATE_BYTES, 8)?;
    map.insert("__gugu_rt0_payload".to_owned(), reserved);
    reserved = align_up(reserved + entry::Rt0Payload::bytes(), 16)?;
    // 静态块放在载荷之后：符号本身是可写数据，值槽必须能被 release 写入。
    for (name, bytes) in &request.globals {
        if map.contains_key(name) {
            return Err(format!("静态块 {name} 与已有符号重名"));
        }
        reserved = align_up(reserved, 16)?;
        map.insert(name.clone(), reserved);
        reserved = reserved
            .checked_add(*bytes)
            .ok_or_else(|| "静态块地址溢出".to_owned())?;
    }
    map.insert("__gugu_fragments_base".to_owned(), fragment_base);
    let _ = text_off;
    Ok(map)
}

/// 初始协程槽保留字节数：`CoroutineSlot` 与对齐余量。
const SLOT_BYTES: u64 = entry::INITIAL_SLOT_BYTES;
/// runtime 状态区保留字节数：首部是 `MorestackMailbox`。
const STATE_BYTES: u64 = entry::RUNTIME_STATE_BYTES;

/// 相对重定位条数：符号里的 `Abs64` 加上每个导入的 GOT 槽，再加启动载荷的地址字段。
fn reloc_count(request: &LinkRequest) -> Result<u64, String> {
    let abs = request
        .symbols
        .iter()
        .flat_map(|symbol| &symbol.relocs)
        .filter(|reloc| reloc.kind == RelocKindCode::Abs64)
        .count();
    u64::try_from(abs + request.imports.len() + PAYLOAD_RELOCS)
        .map_err(|_| "重定位条数溢出".to_owned())
}

/// 启动载荷中需重定位的地址：入口、槽、状态、哨兵、栈图、展开表与函数基址。
const PAYLOAD_RELOCS: usize = 7;

fn place(map: &mut BTreeMap<String, u64>, name: &str, at: u64, len: u64) -> Result<u64, String> {
    map.insert(name.to_owned(), at);
    at.checked_add(len).ok_or_else(|| "镜像地址溢出".to_owned())
}

fn rt0_code_len() -> u64 {
    codec::rt0_bytes(0).len() as u64
}

/// rt0 机器码长度：参数块紧随其后，测试与放置共用同一来源。
pub fn rt0_len() -> u64 {
    rt0_code_len()
}

fn resolve_relocs(request: &LinkRequest, defined: &BTreeMap<String, u64>) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for symbol in &request.symbols {
        let base = *defined
            .get(&symbol.name)
            .ok_or_else(|| format!("符号 {} 没有地址", symbol.name))?;
        for reloc in &symbol.relocs {
            let slot = base + u64::from(reloc.offset);
            claim_slot(&mut seen, slot)?;
            let target = resolve_target(&reloc.target, defined, request)?;
            match reloc.kind {
                RelocKindCode::PcRel32 => {
                    let _ = pc_rel32(slot, target, reloc.addend)?;
                }
                RelocKindCode::Abs64 | RelocKindCode::Rva32 => {}
                RelocKindCode::Unknown(kind) => {
                    return Err(format!("未知重定位 {kind}"));
                }
            }
        }
    }
    Ok(())
}

fn resolve_target(
    name: &str,
    defined: &BTreeMap<String, u64>,
    request: &LinkRequest,
) -> Result<u64, String> {
    if let Some(va) = defined.get(name) {
        return Ok(*va);
    }
    // 类型描述符落在真实 type section 的记录上；section 缺席说明 metadata 没有写出。
    if let Some(offset) = request.type_descriptors.get(name) {
        let base = defined
            .get(TYPE_SECTION_NAME)
            .ok_or_else(|| format!("类型描述符 {name} 缺少 type section"))?;
        return Ok(base + offset);
    }
    if request.bound_glues.contains(name) {
        return Err(format!("runtime glue {name} 绑定在镜像内没有定义"));
    }
    Ok(defined["__gugu_stub_exit"])
}

pub fn claim_slot(seen: &mut BTreeSet<u64>, slot: u64) -> Result<(), String> {
    if !seen.insert(slot) {
        return Err(format!("重定位槽 {slot:#x} 重复"));
    }
    Ok(())
}

/// PC 相对位移必须落在 i32。`from` 是字段地址，PC 为 `from + 4`。
pub fn pc_rel32(from: u64, to: u64, addend: i64) -> Result<i32, String> {
    let pc = i64::try_from(from)
        .ok()
        .and_then(|value| value.checked_add(4))
        .ok_or_else(|| "PC 溢出".to_owned())?;
    let target = i64::try_from(to).map_err(|_| "目标地址溢出".to_owned())?;
    let disp = target
        .checked_sub(pc)
        .and_then(|value| value.checked_add(addend))
        .ok_or_else(|| "PC 相对位移溢出".to_owned())?;
    i32::try_from(disp).map_err(|_| format!("PC 相对位移 {disp} 超出 i32"))
}

/// 可写且可执行的段是非法节权限。
pub fn reject_writable_executable(write: bool, execute: bool) -> Result<(), String> {
    if write && execute {
        return Err("节权限不能同时可写可执行".to_owned());
    }
    Ok(())
}

fn relative_relocs(
    request: &LinkRequest,
    defined: &BTreeMap<String, u64>,
) -> Result<Vec<(u64, u64)>, String> {
    let mut relocs = Vec::new();
    for symbol in &request.symbols {
        let base = defined[&symbol.name];
        for reloc in &symbol.relocs {
            if reloc.kind != RelocKindCode::Abs64 {
                continue;
            }
            let slot = base + u64::from(reloc.offset);
            let target = resolve_target(&reloc.target, defined, request)?;
            let addend = u64::try_from(
                i64::try_from(target)
                    .unwrap_or(0)
                    .saturating_add(reloc.addend),
            )
            .unwrap_or(0);
            relocs.push((slot, addend));
        }
    }
    let stub = defined["__gugu_stub_exit"];
    let limit = defined["__gugu_stack_limit"];
    for index in 0..request.imports.len() {
        relocs.push((limit + 128 + index as u64 * 8, stub));
    }
    // 启动载荷的地址字段：写出链接期值，rt0 按同一批对补上加载偏移。
    let payload = defined["__gugu_rt0_payload"];
    let entry = defined[&request.entry];
    let slot = defined["__gugu_initial_slot"];
    let state = defined["__gugu_runtime_state"];
    relocs.push((
        payload + u64::try_from(offset_of!(entry::Rt0Payload, main_entry)).expect("字段偏移"),
        entry,
    ));
    relocs.push((
        payload + u64::try_from(offset_of!(entry::Rt0Payload, initial_slot)).expect("字段偏移"),
        slot,
    ));
    relocs.push((
        payload + u64::try_from(offset_of!(entry::Rt0Payload, runtime_state)).expect("字段偏移"),
        state,
    ));
    relocs.push((
        payload + u64::try_from(offset_of!(entry::Rt0Payload, main_return)).expect("字段偏移"),
        resolve_target(&mangle::mangle_runtime("main_return"), defined, request)?,
    ));
    relocs.push((
        payload + u64::try_from(offset_of!(entry::Rt0Payload, stackmap)).expect("字段偏移"),
        defined[".gugu.stackmap"],
    ));
    relocs.push((
        payload + u64::try_from(offset_of!(entry::Rt0Payload, fragment_base)).expect("字段偏移"),
        defined["__gugu_fragments_base"],
    ));
    relocs.push((
        payload + u64::try_from(offset_of!(entry::Rt0Payload, unwind)).expect("字段偏移"),
        defined[".eh_frame"],
    ));
    // 类型段与 metadata blob 的地址同样由 rt0 补加载偏移；段不存在时字段是 0，不能登记成
    // 「0 + bias」——那会指向加载基址而不是空。
    if let Some(types) = defined.get(TYPE_SECTION_NAME).copied() {
        relocs.push((
            payload + u64::try_from(offset_of!(entry::Rt0Payload, gc_types)).expect("字段偏移"),
            types,
        ));
    }
    if let Some(meta) = defined.get(GC_METADATA_NAME).copied() {
        relocs.push((
            payload + u64::try_from(offset_of!(entry::Rt0Payload, gc_meta)).expect("字段偏移"),
            meta,
        ));
    }
    relocs.sort_by_key(|item| item.0);
    Ok(relocs)
}

fn reloc_table_va(limit: u64, request: &LinkRequest) -> Result<u64, String> {
    align_up(limit + 128 + got_bytes(request), 16)
}

fn got_bytes(request: &LinkRequest) -> u64 {
    request.imports.len() as u64 * 8
}

fn plt_name(index: usize) -> String {
    format!("__gugu_plt_{index}")
}

fn assemble(
    request: &LinkRequest,
    defined: &BTreeMap<String, u64>,
    interpreter: Option<&str>,
    phnum: u16,
    relocs: &[(u64, u64)],
    rw_va: u64,
    rw_end: u64,
    protect_end: u64,
) -> Result<Vec<u8>, String> {
    let start = defined["_start"];
    let text_end = text_end(request, defined)?;
    let ro_va = align_up(text_end, PAGE)?;
    let ro_end = readonly_end(request, defined, ro_va)?;
    let mut file = vec![0_u8; rw_end as usize];
    write_ehdr(&mut file, start, phnum)?;
    let interp = interpreter.and_then(|path| {
        defined
            .get(".interp")
            .map(|va| (*va, path.len() as u64 + 1))
    });
    let dynamic = dynamic_span(request, defined);
    write_phdrs(
        &mut file, phnum, text_end, ro_va, ro_end, rw_va, rw_end, interp, dynamic,
    )?;
    write_text(&mut file, request, defined)?;
    write_readonly(&mut file, request, defined)?;
    codec::patch_dynamic(&mut file, defined)?;
    write_relro(&mut file, request, defined, relocs)?;
    write_payload(&mut file, defined, request)?;
    write_params(&mut file, defined, request, relocs, rw_va, protect_end)?;
    reject_writable_executable(false, true)?;
    Ok(file)
}

fn dynamic_span(request: &LinkRequest, defined: &BTreeMap<String, u64>) -> Option<(u64, u64)> {
    let va = *defined.get(".dynamic")?;
    let len = request
        .readonly
        .iter()
        .find(|blob| blob.name == ".dynamic")
        .map(|blob| blob.bytes.len() as u64)
        .unwrap_or(0);
    Some((va, len))
}

fn text_end(request: &LinkRequest, defined: &BTreeMap<String, u64>) -> Result<u64, String> {
    let limit = defined["__gugu_stack_limit"];
    let mut end = defined["_start"];
    for (name, va) in defined {
        if request.readonly.iter().any(|blob| blob.name == *name) {
            continue;
        }
        // 栈界之后的符号全在可写区（含保留区与重定位表），不属于文本段。
        if *va >= limit {
            continue;
        }
        let len = symbol_len(request, name);
        end = end.max(va + len);
    }
    Ok(end)
}

fn symbol_len(request: &LinkRequest, name: &str) -> u64 {
    if let Some(symbol) = request.symbols.iter().find(|item| item.name == name) {
        return (symbol.bytes.len() as u64).max(1);
    }
    match name {
        "_start" => rt0_len(),
        "__gugu_rt0_params" => entry::rt0_params::BYTES,
        "__gugu_cold_trap" => 2,
        "__gugu_cage_control" => 48,
        "__gugu_syscall_write" | "__gugu_syscall_mmap" | "__gugu_stub_exit" => 8,
        "__gugu_stub_ret" => 1,
        name if name.starts_with("__gugu_plt_") => PLT_LEN,
        _ => 0,
    }
}

fn readonly_end(
    request: &LinkRequest,
    defined: &BTreeMap<String, u64>,
    ro_va: u64,
) -> Result<u64, String> {
    let mut end = ro_va;
    for blob in &request.readonly {
        if let Some(va) = defined.get(&blob.name) {
            end = end.max(va + blob.bytes.len() as u64);
        }
    }
    Ok(end.max(ro_va))
}

fn write_ehdr(file: &mut [u8], entry: u64, phnum: u16) -> Result<(), String> {
    if file.len() < 64 {
        return Err("镜像装不下 ELF 头".to_owned());
    }
    file[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    file[4] = 2;
    file[5] = 1;
    file[6] = 1;
    put_u16(file, 16, ET_DYN)?;
    put_u16(file, 18, EM_X86_64)?;
    put_u32(file, 20, 1)?;
    put_u64(file, 24, entry)?;
    put_u64(file, 32, 64)?;
    put_u64(file, 40, 0)?;
    put_u16(file, 52, 64)?;
    put_u16(file, 54, 56)?;
    put_u16(file, 56, phnum)?;
    put_u16(file, 58, 64)?;
    put_u16(file, 60, 0)?;
    put_u16(file, 62, 0)?;
    Ok(())
}

fn write_phdrs(
    file: &mut [u8],
    phnum: u16,
    text_end: u64,
    ro_va: u64,
    ro_end: u64,
    rw_va: u64,
    rw_end: u64,
    interp: Option<(u64, u64)>,
    dynamic: Option<(u64, u64)>,
) -> Result<(), String> {
    let mut index = 0_u16;
    write_phdr(
        file,
        index,
        PT_PHDR,
        PF_R,
        64,
        64,
        u64::from(phnum) * 56,
        u64::from(phnum) * 56,
        8,
    )?;
    index += 1;
    if let Some((va, len)) = interp {
        write_phdr(file, index, PT_INTERP, PF_R, va, va, len, len, 1)?;
        index += 1;
    }
    write_phdr(
        file,
        index,
        PT_LOAD,
        PF_R | PF_X,
        0,
        0,
        text_end,
        text_end,
        PAGE,
    )?;
    index += 1;
    let ro_len = ro_end.saturating_sub(ro_va);
    write_phdr(
        file, index, PT_LOAD, PF_R, ro_va, ro_va, ro_len, ro_len, PAGE,
    )?;
    index += 1;
    let rw_len = rw_end.saturating_sub(rw_va);
    write_phdr(
        file,
        index,
        PT_LOAD,
        PF_R | PF_W,
        rw_va,
        rw_va,
        rw_len,
        rw_len,
        PAGE,
    )?;
    index += 1;
    if let Some((va, len)) = dynamic {
        write_phdr(file, index, PT_DYNAMIC, PF_R, va, va, len, len, 8)?;
        index += 1;
    }
    write_phdr(
        file,
        index,
        PT_GNU_RELRO,
        PF_R,
        rw_va,
        rw_va,
        rw_len,
        rw_len,
        1,
    )?;
    Ok(())
}

fn write_phdr(
    file: &mut [u8],
    index: u16,
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
) -> Result<(), String> {
    let at = 64 + usize::from(index) * 56;
    put_u32(file, at, kind)?;
    put_u32(file, at + 4, flags)?;
    put_u64(file, at + 8, offset)?;
    put_u64(file, at + 16, vaddr)?;
    put_u64(file, at + 24, vaddr)?;
    put_u64(file, at + 32, filesz)?;
    put_u64(file, at + 40, memsz)?;
    put_u64(file, at + 48, align)?;
    Ok(())
}

fn write_text(
    file: &mut [u8],
    request: &LinkRequest,
    defined: &BTreeMap<String, u64>,
) -> Result<(), String> {
    let start = defined["_start"] as usize;
    let code = codec::rt0_bytes(
        i32::try_from(defined["__gugu_rt0_params"] - defined["_start"] - 7).unwrap_or(0),
    );
    file[start..start + code.len()].copy_from_slice(&code);
    put_bytes(file, defined["__gugu_cold_trap"], &[0x0f, 0x0b])?;
    put_bytes(
        file,
        defined["__gugu_syscall_write"],
        &[0xb8, 1, 0, 0, 0, 0x0f, 0x05, 0xc3],
    )?;
    put_bytes(
        file,
        defined["__gugu_syscall_mmap"],
        &[0xb8, 9, 0, 0, 0, 0x0f, 0x05, 0xc3],
    )?;
    put_bytes(file, defined["__gugu_stub_ret"], &[0xc3])?;
    put_bytes(
        file,
        defined["__gugu_stub_exit"],
        &[0xb8, 60, 0, 0, 0, 0x0f, 0x05, 0xc3],
    )?;
    write_plt(file, request, defined)?;
    for symbol in &request.symbols {
        let va = defined[&symbol.name] as usize;
        let end = va
            .checked_add(symbol.bytes.len())
            .ok_or_else(|| format!("符号 {} 超出镜像", symbol.name))?;
        if end > file.len() {
            return Err(format!("符号 {} 超出镜像", symbol.name));
        }
        file[va..end].copy_from_slice(&symbol.bytes);
        for reloc in &symbol.relocs {
            let width = match reloc.kind {
                RelocKindCode::PcRel32 | RelocKindCode::Rva32 => 4,
                RelocKindCode::Abs64 => 8,
                RelocKindCode::Unknown(_) => 0,
            };
            if reloc.offset as usize + width > symbol.bytes.len() {
                return Err(format!("重定位超出符号 {}", symbol.name));
            }
            apply_link_reloc(file, va, reloc, defined, request)?;
        }
    }
    Ok(())
}

fn write_plt(
    file: &mut [u8],
    request: &LinkRequest,
    defined: &BTreeMap<String, u64>,
) -> Result<(), String> {
    let limit = defined["__gugu_stack_limit"];
    for index in 0..request.imports.len() {
        let plt = defined[&plt_name(index)];
        let got = limit + 128 + index as u64 * 8;
        let disp = i64::try_from(got).unwrap_or(0) - (i64::try_from(plt).unwrap_or(0) + 6);
        let disp = i32::try_from(disp).map_err(|_| "PLT 位移超出 i32".to_owned())?;
        let mut bytes = [0xff, 0x25, 0, 0, 0, 0];
        bytes[2..].copy_from_slice(&disp.to_le_bytes());
        put_bytes(file, plt, &bytes)?;
    }
    Ok(())
}

fn apply_link_reloc(
    file: &mut [u8],
    base: usize,
    reloc: &ObjectReloc,
    defined: &BTreeMap<String, u64>,
    request: &LinkRequest,
) -> Result<(), String> {
    let slot = base + reloc.offset as usize;
    let target = resolve_target(&reloc.target, defined, request)?;
    match reloc.kind {
        RelocKindCode::PcRel32 => {
            let disp = pc_rel32(slot as u64, target, reloc.addend)?;
            file[slot..slot + 4].copy_from_slice(&disp.to_le_bytes());
        }
        RelocKindCode::Abs64 => {
            let addend = i64::try_from(target)
                .unwrap_or(0)
                .saturating_add(reloc.addend);
            file[slot..slot + 8].copy_from_slice(&addend.to_le_bytes());
        }
        RelocKindCode::Rva32 => {
            let value = u32::try_from(target).map_err(|_| "RVA 超出 u32".to_owned())?;
            file[slot..slot + 4].copy_from_slice(&value.to_le_bytes());
        }
        RelocKindCode::Unknown(kind) => return Err(format!("未知重定位 {kind}")),
    }
    Ok(())
}

fn write_readonly(
    file: &mut [u8],
    request: &LinkRequest,
    defined: &BTreeMap<String, u64>,
) -> Result<(), String> {
    for blob in &request.readonly {
        let Some(va) = defined.get(&blob.name) else {
            continue;
        };
        put_bytes(file, *va, &blob.bytes)?;
    }
    Ok(())
}

fn write_relro(
    file: &mut [u8],
    request: &LinkRequest,
    defined: &BTreeMap<String, u64>,
    relocs: &[(u64, u64)],
) -> Result<(), String> {
    let limit = defined["__gugu_stack_limit"];
    let limit_at = usize::try_from(limit).map_err(|_| "栈界限超出镜像".to_owned())?;
    if limit_at + 128 > file.len() {
        return Err("栈界限超出镜像".to_owned());
    }
    for byte in &mut file[limit_at..limit_at + 128] {
        *byte = 0xff;
    }
    let stub = defined["__gugu_stub_exit"];
    for index in 0..request.imports.len() {
        put_u64(file, limit_at + 128 + index * 8, stub)?;
    }
    let mut at =
        usize::try_from(reloc_table_va(limit, request)?).map_err(|_| "重定位表越界".to_owned())?;
    for (slot, addend) in relocs {
        put_u64(file, at, *slot)?;
        put_u64(file, at + 8, *addend)?;
        at += 16;
    }
    Ok(())
}

fn write_params(
    file: &mut [u8],
    defined: &BTreeMap<String, u64>,
    request: &LinkRequest,
    relocs: &[(u64, u64)],
    rw_va: u64,
    protect_end: u64,
) -> Result<(), String> {
    use entry::rt0_params;
    let entry = *defined
        .get(&request.entry)
        .ok_or_else(|| format!("入口符号 {} 不存在", request.entry))?;
    let params = defined["__gugu_rt0_params"] as usize;
    let mut words = [0_u64; rt0_params::WORDS];
    words[rt0_params::BIAS_BASELINE] = 64;
    words[rt0_params::RELOC_TABLE] = reloc_table_va(defined["__gugu_stack_limit"], request)?;
    words[rt0_params::RELOC_COUNT] = relocs.len() as u64;
    words[rt0_params::PROTECT_VA] = rw_va;
    words[rt0_params::PROTECT_LEN] = protect_end - rw_va;
    words[rt0_params::ENTRY] = entry;
    words[rt0_params::BOOT_R14] = defined["__gugu_initial_slot"];
    words[rt0_params::PROCESSOR] = defined["__gugu_processor"];
    words[rt0_params::RT0_START] = rt0_start(defined, request);
    words[rt0_params::PAYLOAD_VA] = defined["__gugu_rt0_payload"];
    words[rt0_params::PAYLOAD_LEN] = entry::Rt0Payload::bytes();
    words[rt0_params::INITIAL_SLOT] = defined["__gugu_initial_slot"];
    words[rt0_params::RUNTIME_STATE] = defined["__gugu_runtime_state"];
    words[rt0_params::COROUTINE_ENTER] =
        resolve_target(&mangle::mangle_runtime("coroutine_enter"), defined, request)?;
    for (index, word) in words.iter().enumerate() {
        put_u64(file, params + index * 8, *word)?;
    }
    Ok(())
}

/// 镜像 runtime 入口；未登记的独立链接测试仍使用直调入口。
fn rt0_start(defined: &BTreeMap<String, u64>, request: &LinkRequest) -> u64 {
    let name = mangle::mangle_runtime("rt0_start");
    defined
        .get(&name)
        .copied()
        .filter(|_| request.bound_glues.contains(&name))
        .unwrap_or(0)
}

/// 启动载荷：地址字段写成链接期值，由 `relative_relocs` 登记的同名对在 rt0 里补上加载偏移。
fn write_payload(
    file: &mut [u8],
    defined: &BTreeMap<String, u64>,
    request: &LinkRequest,
) -> Result<(), String> {
    let payload = usize::try_from(defined["__gugu_rt0_payload"])
        .map_err(|_| "载荷地址超出宿主".to_owned())?;
    let entry = *defined
        .get(&request.entry)
        .ok_or_else(|| format!("入口符号 {} 不存在", request.entry))?;
    let fields = [
        (
            offset_of!(entry::Rt0Payload, revision),
            entry::Rt0Payload::REVISION,
        ),
        (offset_of!(entry::Rt0Payload, main_entry), entry),
        (
            offset_of!(entry::Rt0Payload, initial_slot),
            defined["__gugu_initial_slot"],
        ),
        (
            offset_of!(entry::Rt0Payload, runtime_state),
            defined["__gugu_runtime_state"],
        ),
        (
            offset_of!(entry::Rt0Payload, main_return),
            resolve_target(&mangle::mangle_runtime("main_return"), defined, request)?,
        ),
        (
            offset_of!(entry::Rt0Payload, stackmap),
            defined[".gugu.stackmap"],
        ),
        (
            offset_of!(entry::Rt0Payload, fragment_base),
            defined["__gugu_fragments_base"],
        ),
        (
            offset_of!(entry::Rt0Payload, stackmap_len),
            u64::try_from(
                request
                    .readonly
                    .iter()
                    .find(|blob| blob.name == ".gugu.stackmap")
                    .ok_or_else(|| "启动载荷缺少栈图节".to_owned())?
                    .bytes
                    .len(),
            )
            .map_err(|_| "栈图节长度溢出".to_owned())?,
        ),
        (offset_of!(entry::Rt0Payload, unwind), defined[".eh_frame"]),
        (
            offset_of!(entry::Rt0Payload, unwind_len),
            u64::try_from(
                request
                    .readonly
                    .iter()
                    .find(|blob| blob.name == ".eh_frame")
                    .ok_or_else(|| "启动载荷缺少展开节".to_owned())?
                    .bytes
                    .len(),
            )
            .map_err(|_| "展开节长度溢出".to_owned())?,
        ),
        (
            offset_of!(entry::Rt0Payload, gc_types),
            defined.get(TYPE_SECTION_NAME).copied().unwrap_or(0),
        ),
        (
            offset_of!(entry::Rt0Payload, gc_types_len),
            request
                .readonly
                .iter()
                .find(|blob| blob.name == TYPE_SECTION_NAME)
                .map_or(Ok(0), |blob| {
                    u64::try_from(blob.bytes.len()).map_err(|_| "类型节长度溢出".to_owned())
                })?,
        ),
        (
            offset_of!(entry::Rt0Payload, gc_meta),
            defined.get(GC_METADATA_NAME).copied().unwrap_or(0),
        ),
        (
            offset_of!(entry::Rt0Payload, gc_meta_len),
            request
                .readonly
                .iter()
                .find(|blob| blob.name == GC_METADATA_NAME)
                .map_or(Ok(0), |blob| {
                    u64::try_from(blob.bytes.len()).map_err(|_| "GC metadata 节长度溢出".to_owned())
                })?,
        ),
    ];
    for (offset, value) in fields {
        put_u64(file, payload + offset, value)?;
    }
    Ok(())
}

fn put_bytes(file: &mut [u8], va: u64, bytes: &[u8]) -> Result<(), String> {
    let at = usize::try_from(va).map_err(|_| "地址超出宿主".to_owned())?;
    let end = at
        .checked_add(bytes.len())
        .ok_or_else(|| "写入越界".to_owned())?;
    if end > file.len() {
        return Err("写入越界".to_owned());
    }
    file[at..end].copy_from_slice(bytes);
    Ok(())
}

fn put_u16(file: &mut [u8], at: usize, value: u16) -> Result<(), String> {
    let end = at.checked_add(2).ok_or_else(|| "写入越界".to_owned())?;
    file.get_mut(at..end)
        .ok_or_else(|| "写入越界".to_owned())?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u32(file: &mut [u8], at: usize, value: u32) -> Result<(), String> {
    let end = at.checked_add(4).ok_or_else(|| "写入越界".to_owned())?;
    file.get_mut(at..end)
        .ok_or_else(|| "写入越界".to_owned())?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u64(file: &mut [u8], at: usize, value: u64) -> Result<(), String> {
    let end = at.checked_add(8).ok_or_else(|| "写入越界".to_owned())?;
    file.get_mut(at..end)
        .ok_or_else(|| "写入越界".to_owned())?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// 按 rt0 的同一公式回填相对重定位。`bias` 是加载偏移。
pub fn apply_relative(
    file: &mut [u8],
    reloc_vaddr: u64,
    count: u32,
    bias: u64,
) -> Result<(), String> {
    let mut at = usize::try_from(reloc_vaddr).map_err(|_| "重定位表越界".to_owned())?;
    for _ in 0..count {
        let slot = read_u64(file, at)?;
        let addend = read_u64(file, at + 8)?;
        let where_at = usize::try_from(slot).map_err(|_| "重定位槽越界".to_owned())?;
        let value = addend.wrapping_add(bias);
        put_u64(file, where_at, value)?;
        at += 16;
    }
    Ok(())
}

fn read_u64(file: &[u8], at: usize) -> Result<u64, String> {
    let end = at.checked_add(8).ok_or_else(|| "读取越界".to_owned())?;
    let bytes: [u8; 8] = file
        .get(at..end)
        .ok_or_else(|| "读取越界".to_owned())?
        .try_into()
        .map_err(|_| "读取越界".to_owned())?;
    Ok(u64::from_le_bytes(bytes))
}

fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    *blake3::Hasher::new_derive_key("gugu-elf64-image-v1")
        .update(bytes)
        .finalize()
        .as_bytes()
}

fn align_up(value: u64, align: u64) -> Result<u64, String> {
    if !align.is_power_of_two() {
        return Err("对齐不是 2 的幂".to_owned());
    }
    value
        .checked_add(align - 1)
        .map(|sum| sum & !(align - 1))
        .ok_or_else(|| "对齐溢出".to_owned())
}

fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}
