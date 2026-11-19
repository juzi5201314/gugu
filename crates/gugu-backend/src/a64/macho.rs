use std::collections::{BTreeMap, BTreeSet};

use super::{
    encode,
    inst::{Address, AddressMode, Operand, Target},
    machine,
    reg::Reg,
};
use crate::{
    NativeImportKind,
    macho::{
        self, Arch, ImportSlot, Location, MachORequest, Section, SectionContent, Segment, Symbol,
    },
};

mod entry;
pub use entry::{DarwinEntry, link_entry};

const STUB_SIZE: usize = 12;
const PAGE_SHIFT: u32 = 12;

/// 将 C 声明名转换为 arm64 Mach-O 内部调用目标名。
pub fn import_stub_symbol(name: &str) -> String {
    format!("__gugu_darwin_c_{name}")
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
        encode_stub(&mut code, pc, target).map_err(|error| error.to_string())?;
    }
    Ok(code)
}

fn encode_stub(code: &mut Vec<u8>, pc: u64, target: u64) -> Result<(), super::Error> {
    let page_delta = ((target >> PAGE_SHIFT) as i64 - (pc >> PAGE_SHIFT) as i64) << PAGE_SHIFT;
    let page_offset = (target & ((1 << PAGE_SHIFT) - 1)) as i64;
    let x16 = Operand::Reg(Reg::x(16));
    for (key, operands) in [
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
    ] {
        code.extend_from_slice(&encode::encode(&machine::inst(key, operands)?)?.bytes());
    }
    Ok(())
}
