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
    /// The same owned section graph for the A64 ELF writer: instruction relocations
    /// resolve their named targets through the world symbol table, and data pointers
    /// become RELATIVE entries that rt0 applies after measuring the load bias.
    pub fn elf_request(&self) -> Result<super::elf::ElfRequest, Error> {
        if self.platform != super::foreign::Platform::Linux {
            return Err(fail("ELF requires a Linux A64 world"));
        }
        self.validate()?;
        let symbols = &self.symbols;
        let target = |name: &str| {
            symbols
                .get(name)
                .copied()
                .ok_or_else(|| fail(format!("undefined A64 symbol {name}")))
        };
        Ok(super::elf::ElfRequest {
            sections: self.sections.clone(),
            entry: self.entry,
            relatives: self
                .pointers
                .iter()
                .map(|pointer| {
                    Ok(super::elf::Relative {
                        at: pointer.at,
                        target: target(&pointer.target)?,
                        addend: pointer.addend,
                    })
                })
                .collect::<Result<_, Error>>()?,
            relocations: self
                .relocations
                .iter()
                .map(|rel| {
                    Ok(super::elf::Relocation {
                        at: rel.at,
                        target: target(&rel.relocation.target)?,
                        addend: rel.relocation.addend,
                        kind: rel.relocation.kind,
                    })
                })
                .collect::<Result<_, Error>>()?,
        })
    }

    /// Instruction relocations become pending [`macho::Arm64Relocation`] records
    /// that `macho::link` patches on the final layout — startup sections appended
    /// later would otherwise move already-patched addresses.
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
            relocations: vec![],
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
            let site = request
                .sections
                .get(relocation.at.section)
                .is_some_and(|s| {
                    matches!(s.content, SectionContent::Code(_))
                        && relocation.at.offset % 4 == 0
                        && relocation
                            .at
                            .offset
                            .checked_add(4)
                            .is_some_and(|end| end <= s.content.size())
                });
            if !site {
                return Err(fail("A64 relocation outside instruction"));
            }
            request.relocations.push(macho::Arm64Relocation {
                at: relocation.at,
                target: at,
                kind: rel.kind,
            });
        }
        Ok(request)
    }
}
