use std::collections::{BTreeMap, BTreeSet};

use super::{
    decode, encode,
    inst::{Address, AddressMode, Operand, Target},
    machine,
    reg::Reg,
};
use crate::{
    NativeImportKind,
    macho::{
        self, Arch, Fixup, ImportSlot, Layout, Location, MachORequest, Section, SectionContent,
        Segment, Symbol,
    },
};

mod entry;
pub use entry::{DarwinEntry, DarwinRuntime, link_entry};

const STUB_SIZE: usize = 12;
const PAGE_SHIFT: u32 = 12;
const IMPORT_STUB_PREFIX: &str = "__gugu_darwin_c_";

/// 将 C 声明名转换为 arm64 Mach-O 内部调用目标名。
pub fn import_stub_symbol(name: &str) -> String {
    format!("{IMPORT_STUB_PREFIX}{name}")
}

/// 为 Function 导入追加调用桩并返回内部符号位置。
/// 调用方须先固定其余布局输入，调用后只能按原长度填写节内容。
pub fn append_import_stubs(
    request: &mut MachORequest,
    slots: &BTreeMap<String, ImportSlot>,
) -> Result<BTreeMap<String, Location>, String> {
    if request.arch != Arch::Arm64 {
        return Err("arm64 import stubs require an arm64 Mach-O image".to_owned());
    }
    let section = request.sections.len();
    let existing: BTreeSet<_> = request
        .symbols
        .iter()
        .map(|sym| sym.name.as_str())
        .collect();
    let mut stubs = BTreeMap::new();
    let mut targets = Vec::new();
    for (name, slot) in slots {
        if slot.kind != NativeImportKind::Function {
            continue;
        }
        let symbol = import_stub_symbol(name);
        if existing.contains(symbol.as_str()) {
            return Err(format!("duplicate arm64 import stub symbol {symbol}"));
        }
        let offset = (targets.len() * STUB_SIZE) as u64;
        stubs.insert(symbol, Location { section, offset });
        targets.push(slot.at);
    }
    if stubs.is_empty() {
        return Ok(stubs);
    }
    let symbols = request.symbols.len();
    request
        .symbols
        .extend(stubs.iter().map(|(name, &at)| Symbol {
            name: name.clone(),
            at,
        }));
    request.sections.push(Section {
        name: "__stubs".to_owned(),
        segment: Segment::Text,
        align: 4,
        content: SectionContent::Code(vec![0; STUB_SIZE * targets.len()]),
    });
    match stub_code(request, section, &targets) {
        Ok(code) => request.sections[section].content = SectionContent::Code(code),
        Err(error) => {
            request.symbols.truncate(symbols);
            request.sections.pop();
            return Err(error);
        }
    }
    Ok(stubs)
}

fn stub_code(
    request: &MachORequest,
    section: usize,
    targets: &[Location],
) -> Result<Vec<u8>, String> {
    let layout = macho::layout(request)?;
    let base = layout.sections[section].addr;
    let mut code = Vec::with_capacity(targets.len() * STUB_SIZE);
    for &at in targets {
        let slot = layout
            .sections
            .get(at.section)
            .filter(|slot| at.offset.checked_add(8).is_some_and(|end| end <= slot.size))
            .ok_or_else(|| format!("invalid arm64 import slot {at:?}"))?;
        let target = slot.addr + at.offset;
        if target % 8 != 0 {
            return Err(format!("unaligned arm64 import slot {at:?}"));
        }
        let pc = base + code.len() as u64;
        code.extend(encode_stub(pc, target).map_err(|error| error.to_string())?);
    }
    Ok(code)
}

fn encode_stub(pc: u64, target: u64) -> Result<[u8; STUB_SIZE], super::Error> {
    let page_delta = ((target >> PAGE_SHIFT) as i64 - (pc >> PAGE_SHIFT) as i64) << PAGE_SHIFT;
    let page_offset = (target & ((1 << PAGE_SHIFT) - 1)) as i64;
    let x16 = Operand::Reg(Reg::x(16));
    let mut code = [0; STUB_SIZE];
    for (word, (key, operands)) in code.as_chunks_mut::<4>().0.iter_mut().zip([
        (
            "adrp",
            vec![x16.clone(), Operand::Target(Target::Offset(page_delta))],
        ),
        (
            "ldr.X.Unsigned",
            vec![
                x16.clone(),
                Operand::Address(Address {
                    base: Reg::x(16),
                    mode: AddressMode::Unsigned(page_offset),
                }),
            ],
        ),
        ("br", vec![x16]),
    ]) {
        word.copy_from_slice(&encode::encode(&machine::inst(key, operands)?)?.bytes());
    }
    Ok(code)
}

fn prebuilt_import_stubs(
    request: &MachORequest,
) -> Result<BTreeMap<String, (Location, Location)>, String> {
    let symbols = request.symbols.iter().filter(|symbol| {
        symbol.name.starts_with(IMPORT_STUB_PREFIX)
            && request
                .sections
                .get(symbol.at.section)
                .is_some_and(|section| section.name == "__stubs")
    });
    if symbols.clone().next().is_none() {
        return Ok(BTreeMap::new());
    }
    let layout = macho::layout(request)?;
    let slots = request
        .fixups
        .iter()
        .filter_map(|fixup| match *fixup {
            Fixup::Bind { at, .. } => Some(layout.address(at).map(|addr| (addr, at))),
            _ => None,
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    symbols
        .map(|symbol| {
            let at = symbol.at;
            let pc = layout.address(at)?;
            let offset = at.offset as usize;
            let SectionContent::Code(bytes) = &request.sections[at.section].content else {
                return Err(format!("arm64 import stub {} is not code", symbol.name));
            };
            let slot = bytes
                .get(offset..offset + STUB_SIZE)
                .and_then(|bytes| decode_stub_target(pc, bytes))
                .and_then(|target| slots.get(&target).copied())
                .ok_or_else(|| format!("unresolved target of arm64 import stub {}", symbol.name))?;
            Ok((symbol.name.clone(), (at, slot)))
        })
        .collect()
}

fn decode_stub_target(pc: u64, bytes: &[u8]) -> Option<u64> {
    let inst = |offset| {
        decode::decode(u32::from_le_bytes(
            bytes[offset..offset + 4].try_into().unwrap(),
        ))
        .ok()
    };
    let adrp = inst(0)?;
    let ldr = inst(4)?;
    let Operand::Target(Target::Offset(page)) = *adrp.operands.get(1)? else {
        return None;
    };
    let Operand::Address(Address {
        mode: AddressMode::Unsigned(low),
        ..
    }) = *ldr.operands.get(1)?
    else {
        return None;
    };
    let target = (pc & !((1 << PAGE_SHIFT) - 1)).checked_add_signed(page + low)?;
    (encode_stub(pc, target).ok()? == bytes).then_some(target)
}

fn relocate_import_stub(
    request: &mut MachORequest,
    layout: &Layout,
    at: Location,
    slot: Location,
) -> Result<(), String> {
    let pc = layout.address(at)?;
    let SectionContent::Code(bytes) = &mut request.sections[at.section].content else {
        return Err("arm64 Mach-O import stub is not in a code section".to_owned());
    };
    let offset = at.offset as usize;
    let stub = bytes
        .get_mut(offset..offset + STUB_SIZE)
        .filter(|_| pc % 4 == 0)
        .ok_or("invalid arm64 Mach-O import stub location")?;
    let code = encode_stub(pc, layout.address(slot)?).map_err(|error| error.to_string())?;
    stub.copy_from_slice(&code);
    Ok(())
}
