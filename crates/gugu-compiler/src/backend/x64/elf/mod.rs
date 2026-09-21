//! Linux ELF64 static PIE。无动态导入时不写 `PT_INTERP`，rt0 自行完成相对重定位与 RELRO。

mod archive;
mod asm;
mod entries;
mod link;
mod runtime;

use std::collections::{BTreeMap, BTreeSet};

use crate::lir::body::{Body, Symbol};
use crate::runtime::RuntimeRawContractV1;

use super::codegen::X64World;
use super::inst::RelocTarget;
use super::mangle::mangle_symbol;
use super::metadata::{SOURCE_SECTION, UNWIND_SECTION};

pub(crate) const ELF_SCHEMA: u32 = 1;
pub(crate) const ELF_DOMAIN: &str = "gugu-elf64-pie-v1";
/// `ET_DYN`。
pub(crate) const ET_DYN: u32 = 3;

use link::{LinkCode, LinkReloc, LinkRequest, link};

/// 启动代码读取的控制块偏移。`map_base` 紧跟契约里的栈检查槽，供扩栈知道保留映射下界。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BootOffsets {
    pub(crate) stack_check: u32,
    pub(crate) map_base: u32,
    pub(crate) poll: u32,
    pub(crate) tlab_cursor: u32,
    pub(crate) tlab_limit: u32,
    pub(crate) turn_cursor: u32,
    pub(crate) turn_limit: u32,
    pub(crate) barrier: u32,
}

impl BootOffsets {
    pub(crate) fn from_contract(raw: &RuntimeRawContractV1) -> Self {
        let stack_check = raw.coroutine().stack_check_offset;
        let turn_limit = raw.scheduler().turn_region_limit_offset();
        Self {
            stack_check,
            map_base: stack_check.saturating_add(8),
            poll: raw.scheduler().poll_flags_offset(),
            tlab_cursor: raw.scheduler().tlab_cursor_offset(),
            tlab_limit: raw.scheduler().tlab_limit_offset(),
            turn_cursor: raw.scheduler().turn_region_cursor_offset(),
            turn_limit,
            barrier: turn_limit.saturating_add(16),
        }
    }
}

/// 一次成功写出的 ELF64。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ElfImage {
    pub(crate) bytes: Vec<u8>,
    pub(crate) schema: u32,
    pub(crate) elf_type: u32,
    pub(crate) entry: u64,
    pub(crate) load_segments: u32,
    pub(crate) relative_relocs: u32,
    pub(crate) interp: String,
    pub(crate) fingerprint: [u8; 32],
    pub(crate) reloc_table: u64,
    pub(crate) sentinel_vaddr: u64,
}

impl ElfImage {
    pub(crate) fn absent() -> Self {
        Self {
            bytes: Vec::new(),
            schema: 0,
            elf_type: 0,
            entry: 0,
            load_segments: 0,
            relative_relocs: 0,
            interp: String::new(),
            fingerprint: [0; 32],
            reloc_table: 0,
            sentinel_vaddr: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ElfError {
    message: String,
}

impl ElfError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

/// 把已编码片段、常量与元数据节链接成 Linux static PIE。
pub(crate) fn link_world(
    world: &X64World,
    bodies: &[Body],
    interpreter: Option<&str>,
    boot: BootOffsets,
) -> Result<ElfImage, ElfError> {
    let owners = bodies_by_instance(bodies)?;
    let mut codes = Vec::new();
    for fragment in &world.fragments {
        let owner = owners
            .get(&fragment.instance)
            .copied()
            .ok_or_else(|| ElfError::new("片段缺少常量池"))?;
        let mut relocs = Vec::new();
        for reloc in &fragment.relocations {
            relocs.push(const_reloc(owner, reloc)?);
        }
        codes.push(LinkCode {
            symbol: fragment.symbol.clone(),
            bytes: fragment.bytes.clone(),
            included: fragment.metadata.included,
            relocs,
        });
    }
    let consts = image_blobs(world, bodies)?;
    let sections = [
        (
            world.metadata.section_name.as_str(),
            world.metadata.stackmap.as_slice(),
        ),
        (UNWIND_SECTION, world.metadata.unwind.as_slice()),
        (SOURCE_SECTION, world.metadata.source.as_slice()),
    ];
    link(&LinkRequest {
        codes: &codes,
        consts: &consts,
        sections: &sections,
        entry: &world.entry_symbol,
        interpreter,
        archives: &[],
        boot,
        emit_runtime: true,
    })
}

#[cfg(test)]
mod tests;

/// 常量字节按序号折叠。类型描述符、类型号、vtable 与类型节基址在本写出里是
/// 8 字节对齐的只读占位；`channel_new` 只接收描述符地址。
fn image_blobs(world: &X64World, bodies: &[Body]) -> Result<Vec<(String, Vec<u8>, u32)>, ElfError> {
    let mut blobs = const_blobs(bodies)?;
    let mut known: BTreeSet<String> = blobs.iter().map(|(name, _, _)| name.clone()).collect();
    for fragment in &world.fragments {
        for reloc in &fragment.relocations {
            let RelocTarget::Lir(symbol) = &reloc.target else {
                continue;
            };
            if !ro_placeholder(symbol) {
                continue;
            }
            let name = mangle_symbol(symbol);
            if known.insert(name.clone()) {
                blobs.push((name, vec![0; 8], 8));
            }
        }
    }
    Ok(blobs)
}

fn ro_placeholder(symbol: &Symbol) -> bool {
    matches!(
        symbol,
        Symbol::TypeDescriptor(_)
            | Symbol::TypeId(_)
            | Symbol::TypeRecords
            | Symbol::TypeNames
            | Symbol::Vtable { .. }
            | Symbol::Global { .. }
    )
}

fn bodies_by_instance(bodies: &[Body]) -> Result<BTreeMap<[u8; 32], &Body>, ElfError> {
    let mut map = BTreeMap::new();
    for body in bodies {
        if let Some(previous) = map.insert(body.instance, body)
            && previous.data != body.data
        {
            return Err(ElfError::new("同一实例的常量池不一致"));
        }
    }
    Ok(map)
}

fn const_reloc(body: &Body, reloc: &super::inst::Relocation) -> Result<LinkReloc, ElfError> {
    let mut link = LinkReloc::from_fragment(reloc);
    if let RelocTarget::Lir(crate::lir::body::Symbol::Data(index)) = &reloc.target {
        let index = usize::try_from(*index).map_err(|_| ElfError::new("常量序号超出范围"))?;
        let data = body
            .data
            .get(index)
            .ok_or_else(|| ElfError::new("常量序号超出范围"))?;
        link.target = super::mangle::mangle_const_bytes(&data.bytes);
    }
    Ok(link)
}

fn const_blobs(bodies: &[Body]) -> Result<Vec<(String, Vec<u8>, u32)>, ElfError> {
    let mut folded: BTreeMap<String, (Vec<u8>, u32)> = BTreeMap::new();
    for body in bodies {
        for data in &body.data {
            let name = super::mangle::mangle_const_bytes(&data.bytes);
            let align = data.align.max(1);
            if let Some((bytes, old)) = folded.get_mut(&name) {
                if bytes != &data.bytes {
                    return Err(ElfError::new("常量字节哈希冲突"));
                }
                *old = (*old).max(align);
                continue;
            }
            folded.insert(name, (data.bytes.clone(), align));
        }
    }
    Ok(folded
        .into_iter()
        .map(|(name, (bytes, align))| (name, bytes, align))
        .collect())
}
