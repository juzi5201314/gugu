use std::collections::BTreeMap;

use crate::{
    NativeImportKind,
    macho::{self, Arch, ImportSlot, Location, MachORequest, Section, SectionContent, Segment},
};

const STUB: [u8; 6] = [0xff, 0x25, 0, 0, 0, 0];

/// 为 Function 导入追加 RIP 相对间接跳转，返回 C 名字对应的可调用地址。
pub fn append_import_stubs(
    request: &mut MachORequest,
    slots: &BTreeMap<String, ImportSlot>,
) -> Result<BTreeMap<String, Location>, String> {
    if request.arch != Arch::X86_64 {
        return Err("x86_64 import stubs require an x86_64 Mach-O image".to_owned());
    }
    let functions: Vec<_> = slots
        .iter()
        .filter(|(_, slot)| slot.kind == NativeImportKind::Function)
        .collect();
    if functions.is_empty() {
        return Ok(BTreeMap::new());
    }
    let section = request.sections.len();
    request.sections.push(Section {
        name: "__stubs".to_owned(),
        segment: Segment::Text,
        align: 2,
        content: SectionContent::Code(STUB.repeat(functions.len())),
    });
    match patch_stubs(request, section, &functions) {
        Ok(symbols) => Ok(symbols),
        Err(error) => {
            request.sections.pop();
            Err(error)
        }
    }
}

fn patch_stubs(
    request: &mut MachORequest,
    section: usize,
    functions: &[(&String, &ImportSlot)],
) -> Result<BTreeMap<String, Location>, String> {
    let layout = macho::layout(request)?;
    let base = layout.sections[section].addr;
    let SectionContent::Code(code) = &mut request.sections[section].content else {
        unreachable!()
    };
    let mut symbols = BTreeMap::new();
    for ((name, slot), (index, stub)) in functions
        .iter()
        .zip(code.chunks_exact_mut(STUB.len()).enumerate())
    {
        let offset = (index * STUB.len()) as u64;
        let target = layout
            .sections
            .get(slot.at.section)
            .filter(|section| slot.at.offset < section.size)
            .ok_or_else(|| format!("invalid GOT slot for {name}"))?;
        let displacement =
            super::elf::pc_rel32(base + offset + 2, target.addr + slot.at.offset, 0)?;
        stub[2..].copy_from_slice(&displacement.to_le_bytes());
        symbols.insert((*name).clone(), Location { section, offset });
    }
    Ok(symbols)
}
