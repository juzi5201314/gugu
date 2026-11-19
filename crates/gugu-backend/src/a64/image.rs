//! Mach-O adapter over the owned A64 section graph; no startup or scheduler policy.
use super::{Error, inst::RelocKind, world::World};
use crate::{
    NativeImportKind,
    macho::{self, Arch, Fixup, Location, MachORequest, SectionContent, Symbol},
};
use std::collections::BTreeMap;

fn fail(message: impl std::fmt::Display) -> Error {
    super::error(&message.to_string())
}

fn address(layout: &macho::Layout, at: Location) -> Result<u64, Error> {
    let section = layout
        .sections
        .get(at.section)
        .ok_or_else(|| fail("A64 location section out of bounds"))?;
    if at.offset >= section.size {
        return Err(fail("A64 location offset out of bounds"));
    }
    section
        .addr
        .checked_add(at.offset)
        .ok_or_else(|| fail("A64 address overflow"))
}

fn target(
    request: &MachORequest,
    symbols: &BTreeMap<String, Location>,
    sizes: &BTreeMap<String, u64>,
    name: &str,
    addend: i64,
) -> Result<Location, Error> {
    if sizes
        .get(name)
        .is_some_and(|size| addend < 0 || addend as u64 >= *size)
    {
        return Err(fail("A64 target outside its definition"));
    }
    let mut at = *symbols
        .get(name)
        .ok_or_else(|| fail(format!("undefined A64 symbol {name}")))?;
    at.offset = at
        .offset
        .checked_add_signed(addend)
        .ok_or_else(|| fail("A64 target addend overflow"))?;
    if request
        .sections
        .get(at.section)
        .is_none_or(|section| at.offset >= section.content.size())
    {
        return Err(fail("A64 target addend out of bounds"));
    }
    Ok(at)
}

impl World {
    /// Final instruction addresses use the writer's layout, including import stubs.
    /// Metadata RVAs remain relative to `code`, as required by a64_binding::Sections.
    pub fn macho_request(&self, identifier: &str) -> Result<MachORequest, Error> {
        if self.platform != super::foreign::Platform::Darwin {
            return Err(fail("Mach-O requires a Darwin A64 world"));
        }
        self.validate()?;
        let mut request = MachORequest {
            arch: Arch::Arm64,
            identifier: identifier.into(),
            sections: self.sections.clone(),
            libraries: vec![],
            imports: vec![],
            fixups: vec![],
            symbols: self
                .symbols
                .iter()
                .map(|(name, at)| Symbol {
                    name: name.clone(),
                    at: *at,
                })
                .collect(),
            entry: self.entry,
        };
        let slots = macho::append_native_imports(&mut request, &self.imports).map_err(fail)?;
        for (name, slot) in &slots {
            if slot.kind == NativeImportKind::Data {
                request.symbols.push(Symbol {
                    name: name.clone(),
                    at: slot.at,
                });
            }
        }
        let first = request.fixups.len();
        for pointer in &self.pointers {
            request.fixups.push(Fixup::Rebase {
                at: pointer.at,
                target: self.entry,
            });
        }
        super::macho::append_import_stubs(&mut request, &slots).map_err(fail)?;
        let symbols = request
            .symbols
            .iter()
            .map(|symbol| (symbol.name.clone(), symbol.at))
            .collect::<BTreeMap<_, _>>();
        for (index, pointer) in self.pointers.iter().enumerate() {
            request.fixups[first + index] = if slots
                .get(&pointer.target)
                .is_some_and(|slot| slot.kind == NativeImportKind::Data)
            {
                let import = request
                    .imports
                    .iter()
                    .position(|import| import.name == format!("_{}", pointer.target))
                    .ok_or_else(|| fail("A64 native data import missing"))?;
                Fixup::Bind {
                    at: pointer.at,
                    import,
                    addend: u8::try_from(pointer.addend).map_err(|_| {
                        fail("A64 native pointer addend exceeds chained-bind range")
                    })?,
                }
            } else {
                Fixup::Rebase {
                    at: pointer.at,
                    target: target(
                        &request,
                        &symbols,
                        &self.sizes,
                        &pointer.target,
                        pointer.addend,
                    )?,
                }
            };
        }
        let layout = macho::layout(&request).map_err(fail)?;
        for relocation in &self.relocations {
            let rel = &relocation.relocation;
            let at = target(&request, &symbols, &self.sizes, &rel.target, rel.addend)?;
            if matches!(rel.kind, RelocKind::Call26 | RelocKind::Branch26)
                && (!matches!(
                    request.sections[at.section].content,
                    SectionContent::Code(_)
                ) || at.offset % 4 != 0)
            {
                return Err(fail("A64 branch target is not aligned code"));
            }
            let pc = address(&layout, relocation.at)?;
            let destination = address(&layout, at)?;
            let section = request
                .sections
                .get_mut(relocation.at.section)
                .ok_or_else(|| fail("A64 relocation section missing"))?;
            let SectionContent::Code(bytes) = &mut section.content else {
                return Err(fail("A64 instruction relocation outside code"));
            };
            let offset = usize::try_from(relocation.at.offset).map_err(fail)?;
            let field = bytes
                .get_mut(
                    offset
                        ..offset
                            .checked_add(4)
                            .ok_or_else(|| fail("A64 relocation offset overflow"))?,
                )
                .filter(|_| offset % 4 == 0)
                .ok_or_else(|| fail("A64 relocation outside instruction"))?;
            let word = u32::from_le_bytes(field.try_into().unwrap());
            field.copy_from_slice(&super::relocation::patch(word, rel.kind, pc, destination)?);
        }
        Ok(request)
    }
}
