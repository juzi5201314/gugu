//! Windows PE32+ 可执行镜像。
//!
//! 不链接 CRT，不扫描 syscall 号，也不搜索宿主 DLL。导入表只含登记的
//! `kernel32.dll` 与 `ntdll.dll`。绝对地址进入 `IMAGE_REL_BASED_DIR64`；
//! PC 相对在写出前修完。rt0 在调用前留出 32 字节 shadow space。

use super::codegen::{FragmentPayload, X64World};
use super::inst::{RelocKind, RelocTarget};
use super::mangle;
use crate::runtime::entry;
use crate::runtime::fragments;
use std::collections::{BTreeMap, BTreeSet};

const IMAGE_BASE: u64 = 0x0000_0001_4000_0000;
const SECTION_ALIGN: u64 = 4096;
const FILE_ALIGN: u64 = 512;
const DIR64: u16 = 10 << 12;
const TEXT_CHARS: u32 = 0x6000_0020;
const RDATA_CHARS: u32 = 0x4000_0040;
const DATA_CHARS: u32 = 0xC000_0040;
const RELOC_CHARS: u32 = 0x4200_0040;

const IMPORTS: &[(&str, &[&str])] = &[
    (
        "kernel32.dll",
        &[
            "ExitProcess",
            "SetConsoleCtrlHandler",
            "GetStdHandle",
            "WriteFile",
        ],
    ),
    (
        "ntdll.dll",
        &[
            "NtTerminateProcess",
            "NtAllocateVirtualMemory",
            "NtFreeVirtualMemory",
        ],
    ),
];

/// 写出的 Windows 镜像。Linux 调用方使用 [`PeImage::absent`]。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PeImage {
    pub(crate) bytes: Vec<u8>,
    pub(crate) kind: String,
    pub(crate) entry_rva: u32,
    pub(crate) reloc_count: u32,
    pub(crate) import_dlls: u32,
    pub(crate) fingerprint: [u8; 32],
}

impl PeImage {
    pub(crate) fn absent() -> Self {
        Self {
            bytes: Vec::new(),
            kind: String::new(),
            entry_rva: 0,
            reloc_count: 0,
            import_dlls: 0,
            fingerprint: [0; 32],
        }
    }
}

/// 一个待链接的代码符号。
#[derive(Clone, Debug)]
pub(crate) struct ObjectSymbol {
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) relocs: Vec<ObjectReloc>,
    /// 同一字节串上的别名入口：名字与相对该符号起点的偏移。
    pub(crate) aliases: Vec<(String, u32)>,
}

/// 一条尚未解析的重定位。
#[derive(Clone, Debug)]
pub(crate) struct ObjectReloc {
    pub(crate) offset: u32,
    pub(crate) kind: RelocKindCode,
    pub(crate) target: String,
    pub(crate) addend: i64,
}

/// 写出前可见的重定位种类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelocKindCode {
    PcRel32,
    Abs64,
    Rva32,
    #[allow(dead_code)]
    Unknown(u8),
}

/// 链接请求。`dll` 为真时写出 `cdylib`。
#[derive(Clone, Debug)]
pub(crate) struct PeRequest {
    pub(crate) entry: String,
    pub(crate) symbols: Vec<ObjectSymbol>,
    pub(crate) readonly: Vec<(String, Vec<u8>)>,
    /// 常量数据块 `(符号名, 字节)`：进 `.rdata` 并按符号登记，不作为节名（节名限 8 字节）。
    pub(crate) constants: Vec<(String, Vec<u8>)>,
    pub(crate) dll: bool,
    /// 已绑定 glue 的 mangled 符号名；这些名字缺失定义时按 `E0062` 语义失败。
    pub(crate) bound_glues: std::collections::BTreeSet<String>,
}

struct Section {
    name: String,
    bytes: Vec<u8>,
    characteristics: u32,
    rva: u64,
    raw: u64,
}

struct RipSite {
    at: usize,
    target: String,
}

/// 链接一个已编码片段世界。
pub(crate) fn link_world(world: &X64World, types: &[u8], meta: &[u8]) -> Result<PeImage, String> {
    let mut symbols = Vec::new();
    for fragment in &world.fragments {
        symbols.push(symbol_from_fragment(fragment)?);
    }
    for blob in fragments::registered_fragments(world.target) {
        symbols.push(symbol_from_machine_fragment(&blob)?);
    }
    let readonly = vec![
        (
            world.metadata.stackmap_name.clone(),
            world.metadata.stackmap_section.clone(),
        ),
        (
            world.metadata.unwind_name.clone(),
            world.metadata.unwind_section.clone(),
        ),
        (
            world.metadata.source_name.clone(),
            world.metadata.source_section.clone(),
        ),
        (".gugutyp".to_owned(), types.to_vec()),
        (".ggmeta".to_owned(), meta.to_vec()),
    ];
    link(&PeRequest {
        entry: world.entry_symbol.clone(),
        symbols,
        readonly,
        constants: world.constants.clone(),
        dll: false,
        bound_glues: world.glues().bound_symbols(),
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

/// 把代码和逻辑节收成 PE32+。
pub(crate) fn link(request: &PeRequest) -> Result<PeImage, String> {
    link_inner(request).map(|(image, _)| image)
}

/// 与 [`link`] 相同，但一并返回符号 RVA 目录；测试据此断言参数块与保留区契约。
#[cfg(test)]
pub(crate) fn link_with_layout(
    request: &PeRequest,
) -> Result<(PeImage, BTreeMap<String, u64>), String> {
    link_inner(request)
}

fn link_inner(request: &PeRequest) -> Result<(PeImage, BTreeMap<String, u64>), String> {
    archive_ready()?;
    let (mut text, mut defined, sites) = assemble_prefix(
        request
            .bound_glues
            .contains(&mangle::mangle_runtime("rt0_start")),
    )?;
    let fragment_off = place_fragments(request, &mut text, &mut defined)?;
    if !defined.contains_key(&request.entry) {
        return Err(format!("入口符号 {} 不存在", request.entry));
    }
    let mut named = named_sections(request)?;
    let mut rdata = Vec::new();
    let mut rdata_at = BTreeMap::new();
    let import_at = append_imports(&mut rdata)?;
    append_standins(&mut rdata, &mut rdata_at)?;
    // 常量数据块：追加到 `.rdata` 并按符号登记（符号名不受节名 8 字节限制）。
    let mut const_at = Vec::with_capacity(request.constants.len());
    for (name, bytes) in &request.constants {
        let offset = align_up(
            u64::try_from(rdata.len()).map_err(|_| "常量偏移溢出".to_owned())?,
            16,
        )?;
        let offset = usize::try_from(offset).map_err(|_| "常量偏移超出宿主".to_owned())?;
        rdata.resize(offset, 0);
        rdata.extend_from_slice(bytes);
        const_at.push((name.clone(), offset as u64));
    }
    let (data, reserved) = data_with_reserved(iat_template())?;
    let mut sections = Vec::new();
    sections.push(section(".text", text, TEXT_CHARS));
    sections.append(&mut named);
    sections.push(section(".rdata", rdata, RDATA_CHARS));
    sections.push(section(".data", data, DATA_CHARS));
    sections.push(section(".pdata", Vec::new(), RDATA_CHARS));
    sections.push(section(".reloc", Vec::new(), RELOC_CHARS));
    assign_rvas(&mut sections)?;
    let text_rva = rva_of(&sections, ".text")?;
    let rdata_rva = rva_of(&sections, ".rdata")?;
    let data_rva = rva_of(&sections, ".data")?;
    for (name, offset) in &const_at {
        defined.insert(name.clone(), rdata_rva + offset);
        rdata_at.insert(name.clone(), *offset);
    }
    // 保留可写区在 `.data` 内，紧随 IAT；地址按 RVA 登记，供参数块与重定位使用。
    defined.insert("__gugu_initial_slot".to_owned(), data_rva + reserved.slot);
    defined.insert("__gugu_runtime_state".to_owned(), data_rva + reserved.state);
    // mailbox 是 runtime 状态区首部的别名：morestack 与返回哨兵片段都按这个符号打补丁。
    defined.insert(
        mangle::mangle_runtime("morestack_mailbox"),
        data_rva + reserved.state,
    );
    defined.insert("__gugu_rt0_payload".to_owned(), data_rva + reserved.payload);
    let fragment_rva = text_rva + fragment_off;
    defined.insert("__gugu_fragments_base".to_owned(), fragment_rva);
    fill_imports(&mut sections, rdata_rva, data_rva, import_at)?;
    patch_rip(
        &mut sections,
        text_rva,
        rdata_rva,
        data_rva,
        &defined,
        &rdata_at,
        &sites,
    )?;
    let abs64 = patch_fragments(request, &mut sections, text_rva, data_rva, &defined)?;
    fill_pdata(&mut sections, fragment_rva)?;
    let reloc = reloc_bytes(&abs64)?;
    if let Some(section) = sections.iter_mut().find(|item| item.name == ".reloc") {
        section.bytes = reloc;
    }
    assign_rvas(&mut sections)?;
    let text_rva = rva_of(&sections, ".text")?;
    let rdata_rva = rva_of(&sections, ".rdata")?;
    let data_rva = rva_of(&sections, ".data")?;
    fill_entry(
        &mut sections,
        request,
        &defined,
        &rdata_at,
        rdata_rva,
        data_rva,
    )?;
    let bytes = write_image(request, &sections, import_at)?;
    let image = PeImage {
        bytes: bytes.clone(),
        kind: if request.dll {
            "cdylib".to_owned()
        } else {
            "exe".to_owned()
        },
        entry_rva: u32::try_from(text_rva).map_err(|_| "入口 RVA 超出 u32".to_owned())?,
        reloc_count: u32::try_from(abs64.len()).unwrap_or(u32::MAX),
        import_dlls: IMPORTS.len() as u32,
        fingerprint: fingerprint(&bytes),
    };
    Ok((image, defined))
}

fn section(name: &str, bytes: Vec<u8>, characteristics: u32) -> Section {
    Section {
        name: name.to_owned(),
        bytes,
        characteristics,
        rva: 0,
        raw: 0,
    }
}

fn assemble_prefix(
    runtime_entry: bool,
) -> Result<(Vec<u8>, BTreeMap<String, u64>, Vec<RipSite>), String> {
    let mut text = Vec::new();
    let mut defined = BTreeMap::new();
    let mut sites = Vec::new();
    defined.insert("_start".to_owned(), 0);
    defined.insert("__gugu_image_base".to_owned(), 0);
    text.extend_from_slice(&[0x48, 0x83, 0xec, 0x28]);
    sites.push(rip(&mut text, &[0x4c, 0x8d, 0x35], "__gugu_initial_slot"));
    sites.push(rip(&mut text, &[0x4c, 0x8d, 0x3d], "__gugu_processor"));
    sites.push(rip(&mut text, &[0x48, 0x8d, 0x0d], "__gugu_handler"));
    text.extend_from_slice(&[0xba, 0x01, 0x00, 0x00, 0x00]);
    sites.push(rip(
        &mut text,
        &[0xff, 0x15],
        "__gugu_iat_SetConsoleCtrlHandler",
    ));
    if runtime_entry {
        sites.push(rip(&mut text, &[0x48, 0x8d, 0x05], "__gugu_rt0_params"));
        sites.push(rip(&mut text, &[0x48, 0x8d, 0x1d], "__gugu_image_base"));
        sites.push(rip(
            &mut text,
            &[0xe8],
            &mangle::mangle_runtime("rt0_start"),
        ));
        text.extend_from_slice(&[0x48, 0x89, 0xc1]); // 返回值 -> ExitProcess 参数 rcx。
    } else {
        sites.push(rip(&mut text, &[0x48, 0x8d, 0x05], "__gugu_user_entry"));
        text.extend_from_slice(&[0xff, 0xd0, 0x31, 0xc9]);
    }
    sites.push(rip(&mut text, &[0xff, 0x15], "__gugu_iat_ExitProcess"));
    text.push(0xcc);
    let handler = text.len() as u64;
    defined.insert("__gugu_handler".to_owned(), handler);
    text.extend_from_slice(&[
        0x48, 0x83, 0xec, 0x28, 0x48, 0xc7, 0xc1, 0xff, 0xff, 0xff, 0xff,
    ]);
    text.extend_from_slice(&[0xba, 0x01, 0x00, 0x00, 0x00]);
    sites.push(rip(
        &mut text,
        &[0xff, 0x15],
        "__gugu_iat_NtTerminateProcess",
    ));
    text.push(0xcc);
    defined.insert("__gugu_cold_trap".to_owned(), text.len() as u64);
    text.extend_from_slice(&[0x0f, 0x0b]);
    defined.insert("__gugu_cage_control".to_owned(), text.len() as u64);
    text.extend(std::iter::repeat_n(0, 48));
    defined.insert("__gugu_rt0_params".to_owned(), text.len() as u64);
    text.extend(std::iter::repeat_n(
        0,
        usize::try_from(crate::runtime::entry::rt0_params::BYTES).expect("参数块字节数"),
    ));
    defined.insert("__gugu_stub_ret".to_owned(), text.len() as u64);
    text.push(0xc3);
    defined.insert("__gugu_stub_abort".to_owned(), text.len() as u64);
    text.extend_from_slice(&[0xb9, 0x01, 0x00, 0x00, 0x00, 0x48, 0x83, 0xec, 0x28]);
    sites.push(rip(&mut text, &[0xff, 0x15], "__gugu_iat_ExitProcess"));
    text.push(0xcc);
    Ok((text, defined, sites))
}

fn rip(text: &mut Vec<u8>, opcode: &[u8], target: &str) -> RipSite {
    text.extend_from_slice(opcode);
    let at = text.len();
    text.extend_from_slice(&[0; 4]);
    RipSite {
        at,
        target: target.to_owned(),
    }
}

fn place_fragments(
    request: &PeRequest,
    text: &mut Vec<u8>,
    defined: &mut BTreeMap<String, u64>,
) -> Result<u64, String> {
    let mut order: Vec<usize> = (0..request.symbols.len()).collect();
    order.sort_by(|&left, &right| request.symbols[left].name.cmp(&request.symbols[right].name));
    let cursor = align_up(text.len() as u64, 16)?;
    pad_to(text, cursor as usize);
    let fragment_off = cursor;
    let mut rva_cursor = 0_u64;
    for index in order {
        rva_cursor = align_up(rva_cursor, 16)?;
        let symbol = &request.symbols[index];
        if defined.contains_key(&symbol.name) {
            return Err(format!("符号 {} 重复定义", symbol.name));
        }
        defined.insert(symbol.name.clone(), fragment_off + rva_cursor);
        for (alias, offset) in &symbol.aliases {
            if defined
                .insert(
                    alias.clone(),
                    fragment_off + rva_cursor + u64::from(*offset),
                )
                .is_some()
            {
                return Err(format!("符号 {alias} 重复定义"));
            }
        }
        let span = (symbol.bytes.len() as u64).max(1);
        rva_cursor = rva_cursor.saturating_add(span);
    }
    pad_to(text, (fragment_off + rva_cursor) as usize);
    if let Some(entry) = defined.get(&request.entry).copied() {
        defined.insert("__gugu_user_entry".to_owned(), entry);
    }
    Ok(fragment_off)
}

fn named_sections(request: &PeRequest) -> Result<Vec<Section>, String> {
    let mut blobs = request.readonly.clone();
    blobs.sort_by(|left, right| left.0.cmp(&right.0));
    let mut sections = Vec::new();
    for (name, bytes) in blobs {
        if name.len() > 8 {
            return Err(format!("节名 {name} 超过 8 字节"));
        }
        if bytes.is_empty() {
            continue;
        }
        sections.push(section(&name, bytes, RDATA_CHARS));
    }
    Ok(sections)
}

fn append_imports(rdata: &mut Vec<u8>) -> Result<usize, String> {
    let at = rdata.len();
    let dlls = IMPORTS.len();
    let desc = (dlls + 1) * 20;
    let mut ilt_len = 0_usize;
    for (_, symbols) in IMPORTS {
        ilt_len += (symbols.len() + 1) * 8;
    }
    let mut hint_len = 0_usize;
    for (dll, symbols) in IMPORTS {
        hint_len += dll.len() + 1;
        for symbol in *symbols {
            hint_len += 2 + symbol.len() + 1;
        }
    }
    rdata.resize(at + desc + ilt_len + hint_len, 0);
    Ok(at)
}

fn append_standins(rdata: &mut Vec<u8>, at: &mut BTreeMap<String, u64>) -> Result<(), String> {
    let limit = align_up(rdata.len() as u64, 16)?;
    pad_to(rdata, limit as usize);
    rdata.extend(std::iter::repeat_n(0xff, 128));
    at.insert("__gugu_stack_limit".to_owned(), limit);
    let processor = align_up(rdata.len() as u64, 16)?;
    pad_to(rdata, processor as usize);
    rdata.extend(std::iter::repeat_n(0xff, 4096));
    at.insert("__gugu_processor".to_owned(), processor);
    Ok(())
}

fn iat_template() -> Vec<u8> {
    let mut bytes = Vec::new();
    for (_, symbols) in IMPORTS {
        bytes.extend(std::iter::repeat_n(0, (symbols.len() + 1) * 8));
    }
    bytes
}

/// 保留可写区在 `.data` 内的偏移。
struct ReservedData {
    slot: u64,
    state: u64,
    payload: u64,
}

/// `.data` 内容：IAT 之后是按 64/8 对齐的保留可写区（初始槽、runtime 状态区、启动载荷），全部零初始化。
fn data_with_reserved(iat: Vec<u8>) -> Result<(Vec<u8>, ReservedData), String> {
    let slot = align_up(iat.len() as u64, 64)?;
    let state = align_up(slot + entry::INITIAL_SLOT_BYTES, 64)?;
    let payload = align_up(state + entry::RUNTIME_STATE_BYTES, 8)?;
    let end = payload + entry::Rt0Payload::bytes();
    let mut data = iat;
    data.resize(
        usize::try_from(end).map_err(|_| "保留区超出镜像".to_owned())?,
        0,
    );
    Ok((
        data,
        ReservedData {
            slot,
            state,
            payload,
        },
    ))
}

/// 写 rt0 参数块与启动载荷。
///
/// PE 不自举重定位表、也没有 mprotect 收尾，对应的 word 保持 0；参数块与保留区里的地址都是
/// RVA，入口 stub 把加载基址写进载荷的 bias 字段后，runtime 用它折算运行时地址。
fn fill_entry(
    sections: &mut [Section],
    request: &PeRequest,
    defined: &BTreeMap<String, u64>,
    rdata_at: &BTreeMap<String, u64>,
    rdata_rva: u64,
    data_rva: u64,
) -> Result<(), String> {
    use crate::runtime::entry::Rt0Payload;
    use crate::runtime::entry::rt0_params;
    let rdata = |name: &str| -> Result<u64, String> {
        rdata_at
            .get(name)
            .map(|offset| rdata_rva + offset)
            .ok_or_else(|| format!("缺少 .rdata 符号 {name}"))
    };
    let text_rva = rva_of(sections, ".text")?;
    let rt0_start = mangle::mangle_runtime("rt0_start");
    let mut words = [0_u64; rt0_params::WORDS];
    words[rt0_params::ENTRY] = defined[&request.entry];
    words[rt0_params::BOOT_R14] = defined["__gugu_initial_slot"];
    words[rt0_params::PROCESSOR] = rdata("__gugu_processor")?;
    words[rt0_params::RT0_START] = defined
        .get(&rt0_start)
        .copied()
        .filter(|_| request.bound_glues.contains(&rt0_start))
        .unwrap_or(0);
    words[rt0_params::PAYLOAD_VA] = defined["__gugu_rt0_payload"];
    words[rt0_params::PAYLOAD_LEN] = Rt0Payload::bytes();
    words[rt0_params::INITIAL_SLOT] = defined["__gugu_initial_slot"];
    words[rt0_params::RUNTIME_STATE] = defined["__gugu_runtime_state"];
    words[rt0_params::COROUTINE_ENTER] = defined
        .get(&mangle::mangle_runtime("coroutine_enter"))
        .map_or(0, |offset| text_rva + offset);
    // `defined` 里 `.text` 符号是节内偏移，`.data` 保留区是 RVA。
    let params =
        usize::try_from(defined["__gugu_rt0_params"]).map_err(|_| "参数块超出 .text".to_owned())?;
    let payload = usize::try_from(defined["__gugu_rt0_payload"] - data_rva)
        .map_err(|_| "载荷超出 .data".to_owned())?;
    let text = section_bytes(sections, ".text")?;
    for (index, word) in words.iter().enumerate() {
        put_u64_at(text, params + index * 8, *word)?;
    }
    let stackmap_rva = rva_of(sections, ".gugustk")?;
    let stackmap_len = u64::try_from(section_bytes(sections, ".gugustk")?.len())
        .map_err(|_| "栈图长度溢出".to_owned())?;
    let (unwind_rva, unwind_len) = sections
        .iter()
        .find(|section| section.name == ".xdata")
        .map(|section| {
            (
                section.rva,
                u64::try_from(section.bytes.len()).expect("展开长度"),
            )
        })
        .unwrap_or((0, 0));
    let data = section_bytes(sections, ".data")?;
    put_u64_at(data, payload, Rt0Payload::REVISION)?;
    put_u64_at(data, payload + 8, words[rt0_params::ENTRY])?;
    put_u64_at(data, payload + 16, words[rt0_params::INITIAL_SLOT])?;
    put_u64_at(data, payload + 24, words[rt0_params::RUNTIME_STATE])?;
    put_u64_at(
        data,
        payload + 40,
        defined
            .get(&mangle::mangle_runtime("main_return"))
            .map_or(words[rt0_params::ENTRY], |offset| text_rva + offset),
    )?;
    put_u64_at(data, payload + 48, stackmap_rva)?;
    put_u64_at(data, payload + 56, defined["__gugu_fragments_base"])?;
    put_u64_at(data, payload + 64, stackmap_len)?;
    put_u64_at(data, payload + 72, unwind_rva)?;
    put_u64_at(data, payload + 80, unwind_len)?;
    Ok(())
}

fn assign_rvas(sections: &mut [Section]) -> Result<(), String> {
    let header = header_size(sections.len())?;
    let mut rva = align_up(header, SECTION_ALIGN)?;
    let mut raw = header;
    for section in sections.iter_mut() {
        section.rva = rva;
        section.raw = raw;
        let span = section.bytes.len() as u64;
        rva = align_up(rva + span, SECTION_ALIGN)?;
        raw = align_up(raw + span, FILE_ALIGN)?;
    }
    Ok(())
}

fn header_size(section_count: usize) -> Result<u64, String> {
    let bytes = 64 + 4 + 20 + 240 + section_count * 40;
    align_up(bytes as u64, FILE_ALIGN)
}

fn rva_of(sections: &[Section], name: &str) -> Result<u64, String> {
    sections
        .iter()
        .find(|section| section.name == name)
        .map(|section| section.rva)
        .ok_or_else(|| format!("节 {name} 缺失"))
}

fn patch_rip(
    sections: &mut [Section],
    text_rva: u64,
    rdata_rva: u64,
    data_rva: u64,
    defined: &BTreeMap<String, u64>,
    rdata_at: &BTreeMap<String, u64>,
    sites: &[RipSite],
) -> Result<(), String> {
    let text = section_bytes(sections, ".text")?;
    for site in sites {
        let target = if let Some(offset) = rdata_at.get(site.target.as_str()) {
            rdata_rva + offset
        } else if let Some(offset) = iat_offset(site.target.as_str()) {
            data_rva + offset
        } else if let Some(offset) = defined.get(site.target.as_str()) {
            match site.target.as_str() {
                "__gugu_initial_slot" | "__gugu_image_base" => *offset,
                _ => text_rva + offset,
            }
        } else {
            return Err(format!("RIP 目标 {} 不存在", site.target));
        };
        let next = text_rva + site.at as u64 + 4;
        let disp = i64::try_from(target).unwrap_or(0) - i64::try_from(next).unwrap_or(0);
        let disp = i32::try_from(disp).map_err(|_| format!("RIP 位移 {disp} 超出 i32"))?;
        text[site.at..site.at + 4].copy_from_slice(&disp.to_le_bytes());
    }
    Ok(())
}

fn section_bytes<'a>(sections: &'a mut [Section], name: &str) -> Result<&'a mut Vec<u8>, String> {
    sections
        .iter_mut()
        .find(|section| section.name == name)
        .map(|section| &mut section.bytes)
        .ok_or_else(|| format!("节 {name} 缺失"))
}

fn iat_offset(name: &str) -> Option<u64> {
    let symbol = name.strip_prefix("__gugu_iat_")?;
    let mut cursor = 0_u64;
    for (_, symbols) in IMPORTS {
        for item in *symbols {
            if *item == symbol {
                return Some(cursor);
            }
            cursor += 8;
        }
        cursor += 8;
    }
    None
}

fn fill_imports(
    sections: &mut [Section],
    rdata_rva: u64,
    data_rva: u64,
    import_at: usize,
) -> Result<(), String> {
    let (region, iat) = import_bytes(rdata_rva, data_rva, import_at)?;
    {
        let rdata = section_bytes(sections, ".rdata")?;
        let end = import_at
            .checked_add(region.len())
            .ok_or_else(|| "导入表越界".to_owned())?;
        if end > rdata.len() {
            return Err("导入表越界".to_owned());
        }
        rdata[import_at..end].copy_from_slice(&region);
    }
    let data = section_bytes(sections, ".data")?;
    if data.len() < iat.len() {
        return Err("IAT 长度不一致".to_owned());
    }
    data[..iat.len()].copy_from_slice(&iat);
    Ok(())
}

fn import_bytes(
    rdata_rva: u64,
    data_rva: u64,
    import_at: usize,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let desc = (IMPORTS.len() + 1) * 20;
    let ilt: usize = IMPORTS
        .iter()
        .map(|(_, symbols)| (symbols.len() + 1) * 8)
        .sum();
    let hint: usize = IMPORTS
        .iter()
        .map(|(dll, symbols)| {
            dll.len()
                + 1
                + symbols
                    .iter()
                    .map(|symbol| 2 + symbol.len() + 1)
                    .sum::<usize>()
        })
        .sum();
    let mut region = vec![0_u8; desc + ilt + hint];
    let mut iat = vec![
        0_u8;
        IMPORTS
            .iter()
            .map(|(_, symbols)| (symbols.len() + 1) * 8)
            .sum()
    ];
    let mut hint_at = desc + ilt;
    let mut ilt_at = desc;
    let mut iat_at = 0_usize;
    for (index, (dll, symbols)) in IMPORTS.iter().enumerate() {
        let name_at = hint_at;
        region[hint_at..hint_at + dll.len()].copy_from_slice(dll.as_bytes());
        hint_at += dll.len() + 1;
        let desc_at = index * 20;
        put_u32(
            &mut region,
            desc_at,
            u32::try_from(rdata_rva + import_at as u64 + ilt_at as u64).unwrap_or(0),
        );
        put_u32(
            &mut region,
            desc_at + 12,
            u32::try_from(rdata_rva + import_at as u64 + name_at as u64).unwrap_or(0),
        );
        put_u32(
            &mut region,
            desc_at + 16,
            u32::try_from(data_rva + iat_at as u64).unwrap_or(0),
        );
        for symbol in *symbols {
            let entry_at = hint_at;
            hint_at += 2;
            region[hint_at..hint_at + symbol.len()].copy_from_slice(symbol.as_bytes());
            hint_at += symbol.len() + 1;
            let entry = rdata_rva + import_at as u64 + entry_at as u64;
            put_u64(&mut region, ilt_at, entry);
            put_u64(&mut iat, iat_at, entry);
            ilt_at += 8;
            iat_at += 8;
        }
        ilt_at += 8;
        iat_at += 8;
    }
    Ok((region, iat))
}

fn patch_fragments(
    request: &PeRequest,
    sections: &mut [Section],
    text_rva: u64,
    data_rva: u64,
    defined: &BTreeMap<String, u64>,
) -> Result<Vec<u64>, String> {
    let mut seen = BTreeSet::new();
    let mut abs64 = Vec::new();
    let text = section_bytes(sections, ".text")?;
    let mailbox = mangle::mangle_runtime("morestack_mailbox");
    let data_symbols: BTreeSet<&str> = request
        .constants
        .iter()
        .map(|(name, _)| name.as_str())
        .chain([
            "__gugu_initial_slot",
            "__gugu_runtime_state",
            "__gugu_rt0_payload",
            mailbox.as_str(),
        ])
        .collect();
    for symbol in &request.symbols {
        let base = *defined
            .get(&symbol.name)
            .ok_or_else(|| format!("符号 {} 没有地址", symbol.name))?;
        let start = usize::try_from(base).map_err(|_| "符号偏移越界".to_owned())?;
        let end = start
            .checked_add(symbol.bytes.len())
            .ok_or_else(|| format!("符号 {} 超出镜像", symbol.name))?;
        if end > text.len() {
            return Err(format!("符号 {} 超出镜像", symbol.name));
        }
        text[start..end].copy_from_slice(&symbol.bytes);
        for reloc in &symbol.relocs {
            let width = match reloc.kind {
                RelocKindCode::PcRel32 | RelocKindCode::Rva32 => 4,
                RelocKindCode::Abs64 => 8,
                RelocKindCode::Unknown(kind) => return Err(format!("未知重定位 {kind}")),
            };
            if reloc.offset as usize + width > symbol.bytes.len() {
                return Err(format!("重定位超出符号 {}", symbol.name));
            }
            let slot = base + u64::from(reloc.offset);
            if !seen.insert(slot) {
                return Err(format!("重定位槽 {slot:#x} 重复"));
            }
            let target_rva = if let Some(offset) = iat_offset(&reloc.target) {
                data_rva + offset
            } else {
                let target = resolve_target(&reloc.target, defined, &request.bound_glues)?;
                if data_symbols.contains(reloc.target.as_str()) {
                    target
                } else {
                    text_rva + target
                }
            };
            let at = usize::try_from(slot).map_err(|_| "重定位槽越界".to_owned())?;
            match reloc.kind {
                RelocKindCode::PcRel32 => {
                    let disp = pc_rel32(text_rva + slot, target_rva, reloc.addend)?;
                    text[at..at + 4].copy_from_slice(&disp.to_le_bytes());
                }
                RelocKindCode::Abs64 => {
                    let value = IMAGE_BASE
                        .wrapping_add(target_rva)
                        .wrapping_add(reloc.addend as u64);
                    text[at..at + 8].copy_from_slice(&value.to_le_bytes());
                    abs64.push(text_rva + slot);
                }
                RelocKindCode::Rva32 => {
                    let value = u32::try_from(target_rva.wrapping_add(reloc.addend as u64))
                        .map_err(|_| "RVA 超出 u32".to_owned())?;
                    text[at..at + 4].copy_from_slice(&value.to_le_bytes());
                }
                RelocKindCode::Unknown(kind) => return Err(format!("未知重定位 {kind}")),
            }
        }
    }
    Ok(abs64)
}

fn resolve_target(
    name: &str,
    defined: &BTreeMap<String, u64>,
    bound: &std::collections::BTreeSet<String>,
) -> Result<u64, String> {
    if let Some(offset) = defined.get(name) {
        return Ok(*offset);
    }
    if bound.contains(name) {
        return Err(format!("runtime glue {name} 绑定在镜像内没有定义"));
    }
    Ok(defined["__gugu_stub_abort"])
}

fn pc_rel32(from: u64, to: u64, addend: i64) -> Result<i32, String> {
    let next = i64::try_from(from)
        .ok()
        .and_then(|value| value.checked_add(4))
        .ok_or_else(|| "PC 溢出".to_owned())?;
    let target = i64::try_from(to).map_err(|_| "目标地址溢出".to_owned())?;
    let disp = target
        .checked_sub(next)
        .and_then(|value| value.checked_add(addend))
        .ok_or_else(|| "PC 相对位移溢出".to_owned())?;
    i32::try_from(disp).map_err(|_| format!("PC 相对位移 {disp} 超出 i32"))
}

fn fill_pdata(sections: &mut [Section], fragment_rva: u64) -> Result<(), String> {
    let parsed = sections
        .iter()
        .find(|section| section.name == ".xdata")
        .map(|section| (section.rva, section.bytes.clone()));
    let Some((xdata_rva, blob)) = parsed else {
        return Ok(());
    };
    if blob.len() < 48 || &blob[..8] != b"GUGUUN01" {
        return Err("展开节缺少 GUGUUN01".to_owned());
    }
    let platform = u64::from_le_bytes(blob[28..36].try_into().unwrap_or([0; 8])) as usize;
    if platform + 4 > blob.len() {
        return Err("展开平台尾越界".to_owned());
    }
    let count = u32::from_le_bytes(blob[platform..platform + 4].try_into().unwrap_or([0; 4]));
    let pdata_at = platform + 4;
    let info_base = pdata_at + count as usize * 12;
    if info_base > blob.len() {
        return Err("展开平台尾越界".to_owned());
    }
    let mut pdata = Vec::new();
    for index in 0..count as usize {
        let at = pdata_at + index * 12;
        let begin = u32::from_le_bytes(blob[at..at + 4].try_into().unwrap_or([0; 4]));
        let end = u32::from_le_bytes(blob[at + 4..at + 8].try_into().unwrap_or([0; 4]));
        let info = u32::from_le_bytes(blob[at + 8..at + 12].try_into().unwrap_or([0; 4]));
        let image_begin = u32::try_from(fragment_rva + u64::from(begin))
            .map_err(|_| "展开 RVA 超出 u32".to_owned())?;
        let image_end = u32::try_from(fragment_rva + u64::from(end))
            .map_err(|_| "展开 RVA 超出 u32".to_owned())?;
        let info_rva = u32::try_from(xdata_rva + info_base as u64 + u64::from(info))
            .map_err(|_| "UNWIND_INFO RVA 超出 u32".to_owned())?;
        pdata.extend_from_slice(&image_begin.to_le_bytes());
        pdata.extend_from_slice(&image_end.to_le_bytes());
        pdata.extend_from_slice(&info_rva.to_le_bytes());
    }
    let section = section_bytes(sections, ".pdata")?;
    *section = pdata;
    Ok(())
}

fn reloc_bytes(slots: &[u64]) -> Result<Vec<u8>, String> {
    let mut pages: BTreeMap<u64, Vec<u16>> = BTreeMap::new();
    for slot in slots {
        let page = slot & !0xfff;
        let offset = u16::try_from(slot - page).map_err(|_| "重定位页内偏移越界".to_owned())?;
        pages.entry(page).or_default().push(DIR64 | offset);
    }
    let mut bytes = Vec::new();
    for (page, mut entries) in pages {
        if entries.len() % 2 == 1 {
            entries.push(0);
        }
        let size = 8 + entries.len() * 2;
        bytes.extend_from_slice(&u32::try_from(page).unwrap_or(0).to_le_bytes());
        bytes.extend_from_slice(&(size as u32).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry.to_le_bytes());
        }
    }
    Ok(bytes)
}

fn write_image(
    request: &PeRequest,
    sections: &[Section],
    import_at: usize,
) -> Result<Vec<u8>, String> {
    let header = header_size(sections.len())? as usize;
    let last = sections.last().ok_or_else(|| "没有节".to_owned())?;
    let file_len = align_up(last.raw + last.bytes.len() as u64, FILE_ALIGN)? as usize;
    let mut file = vec![0_u8; file_len.max(header)];
    file[0] = b'M';
    file[1] = b'Z';
    file[0x3c..0x40].copy_from_slice(&64_u32.to_le_bytes());
    file[64..68].copy_from_slice(b"PE\0\0");
    put_u16_at(&mut file, 68, 0x8664)?;
    put_u16_at(&mut file, 70, sections.len() as u16)?;
    put_u16_at(&mut file, 84, 240)?;
    let characteristics = if request.dll { 0x2022 } else { 0x0022 };
    put_u16_at(&mut file, 86, characteristics)?;
    put_u16_at(&mut file, 88, 0x20b)?;
    let text = sections
        .iter()
        .find(|section| section.name == ".text")
        .ok_or_else(|| "缺少 .text".to_owned())?;
    put_u32_at(&mut file, 88 + 16, text.rva as u32)?;
    put_u32_at(&mut file, 88 + 20, text.rva as u32)?;
    put_u64_at(&mut file, 88 + 24, IMAGE_BASE)?;
    put_u32_at(&mut file, 88 + 32, SECTION_ALIGN as u32)?;
    put_u32_at(&mut file, 88 + 36, FILE_ALIGN as u32)?;
    put_u16_at(&mut file, 88 + 40, 6)?;
    put_u16_at(&mut file, 88 + 48, 6)?;
    let image_end = align_up(last.rva + last.bytes.len() as u64, SECTION_ALIGN)?;
    put_u32_at(&mut file, 88 + 56, image_end as u32)?;
    put_u32_at(&mut file, 88 + 60, header as u32)?;
    put_u16_at(&mut file, 88 + 68, 3)?;
    put_u16_at(&mut file, 88 + 70, 0x0160)?;
    put_u64_at(&mut file, 88 + 72, 0x10_0000)?;
    put_u64_at(&mut file, 88 + 80, 0x1000)?;
    put_u64_at(&mut file, 88 + 88, 0x10_0000)?;
    put_u64_at(&mut file, 88 + 96, 0x1000)?;
    put_u32_at(&mut file, 88 + 108, 16)?;
    write_directories(&mut file, sections, import_at)?;
    for (index, section) in sections.iter().enumerate() {
        reject_section(section)?;
        let at = 328 + index * 40;
        let mut name = [0_u8; 8];
        let raw = section.name.as_bytes();
        name[..raw.len().min(8)].copy_from_slice(&raw[..raw.len().min(8)]);
        file[at..at + 8].copy_from_slice(&name);
        put_u32_at(&mut file, at + 8, section.bytes.len() as u32)?;
        put_u32_at(&mut file, at + 12, section.rva as u32)?;
        let raw_size = align_up(section.bytes.len() as u64, FILE_ALIGN)? as u32;
        put_u32_at(&mut file, at + 16, raw_size)?;
        put_u32_at(&mut file, at + 20, section.raw as u32)?;
        put_u32_at(&mut file, at + 36, section.characteristics)?;
        let raw_at = section.raw as usize;
        let end = raw_at + section.bytes.len();
        if end > file.len() {
            return Err(format!("节 {} 超出文件", section.name));
        }
        file[raw_at..end].copy_from_slice(&section.bytes);
    }
    Ok(file)
}

fn write_directories(
    file: &mut [u8],
    sections: &[Section],
    import_at: usize,
) -> Result<(), String> {
    let rdata = sections
        .iter()
        .find(|section| section.name == ".rdata")
        .ok_or_else(|| "缺少 .rdata".to_owned())?;
    let data = sections
        .iter()
        .find(|section| section.name == ".data")
        .ok_or_else(|| "缺少 .data".to_owned())?;
    let pdata = sections
        .iter()
        .find(|section| section.name == ".pdata")
        .ok_or_else(|| "缺少 .pdata".to_owned())?;
    let reloc = sections
        .iter()
        .find(|section| section.name == ".reloc")
        .ok_or_else(|| "缺少 .reloc".to_owned())?;
    let dir = 88 + 112;
    put_directory(file, dir + 8, rdata.rva + import_at as u64, import_span())?;
    put_directory(file, dir + 24, pdata.rva, pdata.bytes.len() as u64)?;
    put_directory(file, dir + 40, reloc.rva, reloc.bytes.len() as u64)?;
    put_directory(file, dir + 96, data.rva, data.bytes.len() as u64)?;
    Ok(())
}

fn import_span() -> u64 {
    ((IMPORTS.len() + 1) * 20) as u64
}

fn put_directory(file: &mut [u8], at: usize, rva: u64, size: u64) -> Result<(), String> {
    if size == 0 {
        return Ok(());
    }
    put_u32_at(file, at, u32::try_from(rva).unwrap_or(0))?;
    put_u32_at(file, at + 4, u32::try_from(size).unwrap_or(0))?;
    Ok(())
}

fn reject_section(section: &Section) -> Result<(), String> {
    let execute = section.characteristics & 0x2000_0000 != 0;
    let write = section.characteristics & 0x8000_0000 != 0;
    reject_writable_executable(write, execute)
}

/// 可写且可执行的节是非法权限。
pub(crate) fn reject_writable_executable(write: bool, execute: bool) -> Result<(), String> {
    if write && execute {
        return Err("节权限不能同时可写可执行".to_owned());
    }
    Ok(())
}

fn archive_ready() -> Result<(), String> {
    let packed = pack_coff_archive(&[]);
    extract_coff_archive(&packed, &[])?;
    Ok(())
}

/// 写出确定性的 COFF archive：时间戳、uid、gid 为 0。
pub(crate) fn pack_coff_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, bytes) in members {
        let mut header = [b' '; 60];
        let raw = name.as_bytes();
        header[..raw.len().min(16)].copy_from_slice(&raw[..raw.len().min(16)]);
        header[16..28].copy_from_slice(b"0           ");
        header[28..34].copy_from_slice(b"0     ");
        header[34..40].copy_from_slice(b"0     ");
        header[40..48].copy_from_slice(b"100644  ");
        let size = format!("{:<10}", bytes.len());
        header[48..58].copy_from_slice(size.as_bytes());
        header[58..60].copy_from_slice(b"`\n");
        out.extend_from_slice(&header);
        out.extend_from_slice(bytes);
        if bytes.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// 按成员名抽取 COFF archive。
pub(crate) fn extract_coff_archive(
    archive: &[u8],
    needed: &[String],
) -> Result<Vec<Vec<u8>>, String> {
    if !archive.starts_with(b"!<arch>\n") {
        return Err("不是 COFF archive".to_owned());
    }
    let mut members = BTreeMap::new();
    let mut cursor = 8_usize;
    while cursor + 60 <= archive.len() {
        let header = &archive[cursor..cursor + 60];
        if &header[58..60] != b"`\n" {
            return Err("archive 头魔数不符".to_owned());
        }
        let name = std::str::from_utf8(&header[..16])
            .map_err(|_| "archive 成员名不是 UTF-8".to_owned())?
            .trim_end_matches([' ', '/'])
            .to_owned();
        let size: usize = std::str::from_utf8(&header[48..58])
            .map_err(|_| "archive 长度不是 UTF-8".to_owned())?
            .trim()
            .parse()
            .map_err(|_| "archive 长度非法".to_owned())?;
        let body = cursor + 60;
        let end = body
            .checked_add(size)
            .ok_or_else(|| "archive 成员越界".to_owned())?;
        if end > archive.len() {
            return Err("archive 成员越界".to_owned());
        }
        members.insert(name, archive[body..end].to_vec());
        cursor = end + (size & 1);
    }
    let mut out = Vec::new();
    for name in needed {
        let bytes = members
            .get(name)
            .ok_or_else(|| format!("归档缺少符号 {name}"))?;
        out.push(bytes.clone());
    }
    Ok(out)
}

fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    *blake3::Hasher::new_derive_key("gugu-pe32-image-v1")
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

fn pad_to(bytes: &mut Vec<u8>, len: usize) {
    if bytes.len() < len {
        bytes.resize(len, 0);
    }
}

fn put_u16_at(file: &mut [u8], at: usize, value: u16) -> Result<(), String> {
    file.get_mut(at..at + 2)
        .ok_or_else(|| "写入越界".to_owned())?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u32_at(file: &mut [u8], at: usize, value: u32) -> Result<(), String> {
    file.get_mut(at..at + 4)
        .ok_or_else(|| "写入越界".to_owned())?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u64_at(file: &mut [u8], at: usize, value: u64) -> Result<(), String> {
    file.get_mut(at..at + 8)
        .ok_or_else(|| "写入越界".to_owned())?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
