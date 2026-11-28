use std::collections::{BTreeMap, BTreeSet};
use std::mem::offset_of;

use crate::macho::{
    self, Arch, Fixup, Location, MachImage, MachORequest, Section, SectionContent, Segment,
};
use crate::runtime::cage_control::CageControlRecord;
use crate::runtime::entry::{self, Rt0Payload, rt0_params};
use crate::{NativeImport, NativeLink};

use super::super::codegen::X64World;
use super::super::elf::{self, ObjectReloc, ObjectSymbol, RelocKindCode};
use super::super::{fragments, fs_native, mangle};

const PARAMS: &str = "__gugu_rt0_params";
const PAYLOAD: &str = "__gugu_rt0_payload";
const SLOT: &str = "__gugu_initial_slot";
const PROCESSOR: &str = "__gugu_processor";
const STATE: &str = "__gugu_runtime_state";
const FRAGMENTS: &str = "__gugu_fragments_base";
const STACKMAP: &str = "__gugu_stackmap";
const UNWIND: &str = "__gugu_unwind";
const TYPES: &str = "__gugu_types";
const META: &str = "__gugu_meta";

/// 将 x86_64 的闭世界片段与 Mach-O LC_MAIN 启动边界连接。
pub fn link_world(
    world: &X64World,
    types: &[u8],
    meta: &[u8],
    native: &NativeLink,
) -> Result<MachImage, String> {
    if !world.native_exports.is_empty() {
        return Err("Mach-O executable does not support C exports".into());
    }
    let mut image = WorldImage::new();
    image.code(world)?;
    image.data(world, types, meta)?;
    let mut imports = native.imports.clone();
    let prefix = fs_native::darwin::C_PREFIX;
    let mut imported: std::collections::BTreeSet<_> =
        imports.iter().map(|item| item.name.clone()).collect();
    for (_, site) in &image.code_sites {
        if let Some(name) = site.target.strip_prefix(prefix)
            && imported.insert(name.to_owned())
        {
            imports.push(NativeImport::function(macho::LIB_SYSTEM, name));
        }
    }
    if !imports.iter().any(|import| import.name == "exit") {
        imports.push(NativeImport::function(macho::LIB_SYSTEM, "exit"));
    }
    let slots = macho::append_native_imports(&mut image.request, &imports)?;
    for (name, slot) in &slots {
        image.define(format!("__gugu_got_{name}"), slot.at)?;
    }
    for (name, at) in super::append_import_stubs(&mut image.request, &slots)? {
        image.define(format!("{prefix}{name}"), at)?;
        image.define(name, at)?;
    }
    for relocation in &world.blob_relocations {
        let blob = image.addr(&relocation.blob)?;
        let target = image.addr(&relocation.symbol)?;
        image.request.fixups.push(Fixup::Rebase {
            at: Location {
                offset: blob.offset + relocation.offset,
                ..blob
            },
            target,
        });
    }
    let layout = macho::layout(&image.request)?;
    image.patch_code(&layout, world)?;
    image.startup(&layout, world)?;
    macho::link(&image.request)
}

struct WorldImage {
    request: MachORequest,
    definitions: BTreeMap<String, Location>,
    code_sites: Vec<(Location, ObjectReloc)>,
}

impl WorldImage {
    fn new() -> Self {
        Self {
            request: MachORequest {
                arch: Arch::X86_64,
                identifier: "gugu".into(),
                sections: Vec::new(),
                libraries: Vec::new(),
                imports: Vec::new(),
                fixups: Vec::new(),
                relocations: Vec::new(),
                symbols: Vec::new(),
                entry: Location {
                    section: 0,
                    offset: 0,
                },
            },
            definitions: BTreeMap::new(),
            code_sites: Vec::new(),
        }
    }

    fn section(
        &mut self,
        name: &str,
        segment: Segment,
        align: u64,
        content: SectionContent,
    ) -> Location {
        let section = self.request.sections.len();
        self.request.sections.push(Section {
            name: name.into(),
            segment,
            align,
            content,
        });
        Location { section, offset: 0 }
    }

    fn define(&mut self, name: String, at: Location) -> Result<(), String> {
        if self.definitions.insert(name.clone(), at).is_some() {
            return Err(format!("Mach-O symbol {name} defined twice"));
        }
        Ok(())
    }

    fn addr(&self, name: &str) -> Result<Location, String> {
        self.definitions
            .get(name)
            .copied()
            .ok_or_else(|| format!("Mach-O symbol {name} is undefined"))
    }

    fn code(&mut self, world: &X64World) -> Result<(), String> {
        let (rt0, sites) = rt0();
        self.request.entry = self.section("__rt0", Segment::Text, 16, SectionContent::Code(rt0));
        self.code_sites
            .extend(sites.into_iter().map(|site| (self.request.entry, site)));
        let mut symbols = world
            .fragments
            .iter()
            .map(elf::symbol_from_fragment)
            .collect::<Result<Vec<_>, _>>()?;
        for fragment in fragments::registered_fragments(world.target) {
            if fs_native::required(&world.fragments, &fragment) {
                symbols.push(elf::symbol_from_machine_fragment(&fragment)?);
            }
        }
        symbols.sort_by(|left, right| left.name.cmp(&right.name));
        let base = self.section(
            "__text",
            Segment::Text,
            16,
            SectionContent::Code(Vec::new()),
        );
        self.define(FRAGMENTS.into(), base)?;
        let mut code = Vec::new();
        for symbol in symbols {
            self.add_symbol(&mut code, base.section, symbol)?;
        }
        for name in ["__gugu_cold_trap", "__gugu_stub_abort"] {
            self.define(
                name.into(),
                Location {
                    section: base.section,
                    offset: code.len() as u64,
                },
            )?;
            code.extend_from_slice(&[0x0f, 0x0b]);
        }
        self.request.sections[base.section].content = SectionContent::Code(code);
        Ok(())
    }

    fn add_symbol(
        &mut self,
        code: &mut Vec<u8>,
        section: usize,
        symbol: ObjectSymbol,
    ) -> Result<(), String> {
        code.resize(code.len().next_multiple_of(16), 0x90);
        let at = Location {
            section,
            offset: code.len() as u64,
        };
        self.define(symbol.name, at)?;
        for (name, offset) in symbol.aliases {
            self.define(
                name,
                Location {
                    offset: at.offset + u64::from(offset),
                    ..at
                },
            )?;
        }
        let mut seen = BTreeSet::new();
        for site in symbol.relocs {
            let offset = site.offset as usize;
            if offset + 4 > symbol.bytes.len() || !seen.insert(offset) {
                return Err("Mach-O relocation slot is duplicated or outside the fragment".into());
            }
            self.code_sites.push((at, site));
        }
        if symbol.bytes.is_empty() {
            code.push(0xcc);
        } else {
            code.extend(symbol.bytes);
        }
        Ok(())
    }

    fn data(&mut self, world: &X64World, types: &[u8], meta: &[u8]) -> Result<(), String> {
        let constants = [
            (
                STACKMAP,
                "__gugu_stackmap",
                world.metadata.stackmap_section.as_slice(),
            ),
            (
                UNWIND,
                "__gugu_unwind",
                world.metadata.unwind_section.as_slice(),
            ),
            (
                "__gugu_source",
                "__gugu_source",
                world.metadata.source_section.as_slice(),
            ),
            (TYPES, "__gugu_types", types),
            (META, "__gugu_meta", meta),
        ];
        for (symbol, name, bytes) in constants {
            let at = self.section(
                name,
                Segment::Text,
                16,
                SectionContent::Data(bytes.to_vec()),
            );
            self.define(symbol.into(), at)?;
        }
        let types_at = self.addr(TYPES)?;
        for (name, offset) in world.type_symbols(types)? {
            self.define(name, Location { offset, ..types_at })?;
        }
        let const_at = self.section(
            "__const",
            Segment::Text,
            16,
            SectionContent::Data(Vec::new()),
        );
        let mut data = Vec::new();
        for (name, bytes) in &world.constants {
            data.resize(data.len().next_multiple_of(16), 0);
            self.define(
                name.clone(),
                Location {
                    offset: data.len() as u64,
                    ..const_at
                },
            )?;
            data.extend_from_slice(bytes);
        }
        self.request.sections[const_at.section].content = SectionContent::Data(data);
        self.runtime_data(world.cage_bytes, &world.globals)
    }

    fn runtime_data(&mut self, cage_bytes: u64, globals: &[(String, u64)]) -> Result<(), String> {
        let mut processor = vec![0xff; 4096];
        processor[..64].fill(0);
        let at = self.section(
            "__boot_proc",
            Segment::Text,
            16,
            SectionContent::Data(processor),
        );
        self.define(PROCESSOR.into(), at)?;
        let params = self.section(
            "__rt0_params",
            Segment::Text,
            8,
            SectionContent::Data(vec![0; rt0_params::BYTES as usize]),
        );
        self.define(PARAMS.into(), params)?;
        let payload = self.section(
            "__rt0_payload",
            Segment::Data,
            8,
            SectionContent::Data(vec![0; Rt0Payload::bytes() as usize]),
        );
        self.define(PAYLOAD.into(), payload)?;
        let cage = self.section(
            "__cage_control",
            Segment::Data,
            16,
            SectionContent::Data(CageControlRecord::boot_bytes(cage_bytes)),
        );
        self.define("__gugu_cage_control".into(), cage)?;
        let reserved = self.request.sections.len();
        let state = entry::INITIAL_SLOT_BYTES.next_multiple_of(64);
        let mailbox = (state + entry::RUNTIME_STATE_BYTES).next_multiple_of(16);
        for (name, offset) in [
            (SLOT.into(), 0),
            (STATE.into(), state),
            (mangle::mangle_runtime("morestack_mailbox"), state),
            (
                "__gugu_callback_control".into(),
                state + entry::state::CALLBACK,
            ),
            ("__gugu_signal_mailbox".into(), mailbox),
        ] {
            self.define(
                name,
                Location {
                    section: reserved,
                    offset,
                },
            )?;
        }
        let mut bytes = mailbox + 16;
        for (name, size) in globals {
            bytes = bytes.next_multiple_of(16);
            self.define(
                name.clone(),
                Location {
                    section: reserved,
                    offset: bytes,
                },
            )?;
            bytes = bytes
                .checked_add(*size)
                .ok_or("Mach-O globals exceed address space")?;
        }
        self.section("__bss", Segment::Data, 64, SectionContent::ZeroFill(bytes));
        Ok(())
    }

    fn patch_code(&mut self, layout: &macho::Layout, world: &X64World) -> Result<(), String> {
        let bound = world.glues().bound_symbols();
        for (base, site) in &self.code_sites {
            let at = Location {
                offset: base.offset + u64::from(site.offset),
                ..*base
            };
            let target = match self.addr(&site.target) {
                result @ Ok(_) => result,
                Err(_)
                    if site.target.starts_with("__gugu_runtime_")
                        && !bound.contains(&site.target) =>
                {
                    self.addr("__gugu_stub_abort")
                }
                error => error,
            }?;
            let to = layout.address(target)?;
            let value = match site.kind {
                RelocKindCode::PcRel32 => {
                    elf::pc_rel32(layout.address(at)?, to, site.addend)?.to_le_bytes()
                }
                RelocKindCode::Rva32 => u32::try_from(to - macho::IMAGE_BASE)
                    .map_err(|_| "Mach-O RVA exceeds u32")?
                    .to_le_bytes(),
                _ => return Err("unsupported Mach-O code relocation".into()),
            };
            write_at(&mut self.request, at, &value)?;
        }
        for site in &world.blob_relocations {
            let at = Location {
                offset: site.offset,
                ..self.addr(META)?
            };
            let to = layout.address(self.addr(&site.symbol)?)? - macho::IMAGE_BASE;
            write_at(&mut self.request, at, &to.to_le_bytes())?;
        }
        Ok(())
    }

    fn startup(&mut self, layout: &macho::Layout, world: &X64World) -> Result<(), String> {
        let rva = |name: &str| {
            layout
                .address(self.addr(name)?)
                .map(|at| at - macho::IMAGE_BASE)
        };
        let len = |name| self.addr(name).map(|at| layout.sections[at.section].size);
        let params = self.addr(PARAMS)?;
        let payload = self.addr(PAYLOAD)?;
        let words = [
            (rt0_params::BIAS_BASELINE, rva(PARAMS)?),
            (rt0_params::ENTRY, rva(&world.entry_symbol)?),
            (rt0_params::BOOT_R14, rva(SLOT)?),
            (rt0_params::PROCESSOR, rva(PROCESSOR)?),
            (
                rt0_params::RT0_START,
                rva(&mangle::mangle_runtime("rt0_start"))?,
            ),
            (rt0_params::PAYLOAD_VA, rva(PAYLOAD)?),
            (rt0_params::PAYLOAD_LEN, Rt0Payload::bytes()),
            (rt0_params::INITIAL_SLOT, rva(SLOT)?),
            (rt0_params::RUNTIME_STATE, rva(STATE)?),
            (
                rt0_params::COROUTINE_ENTER,
                rva(&mangle::mangle_runtime("coroutine_enter"))?,
            ),
        ];
        let fields = [
            (offset_of!(Rt0Payload, revision), Rt0Payload::REVISION),
            (
                offset_of!(Rt0Payload, main_entry),
                rva(&world.entry_symbol)?,
            ),
            (offset_of!(Rt0Payload, initial_slot), rva(SLOT)?),
            (offset_of!(Rt0Payload, runtime_state), rva(STATE)?),
            (
                offset_of!(Rt0Payload, main_return),
                rva(&mangle::mangle_runtime("main_return"))?,
            ),
            (offset_of!(Rt0Payload, stackmap), rva(STACKMAP)?),
            (offset_of!(Rt0Payload, fragment_base), rva(FRAGMENTS)?),
            (offset_of!(Rt0Payload, stackmap_len), len(STACKMAP)?),
            (offset_of!(Rt0Payload, unwind), rva(UNWIND)?),
            (offset_of!(Rt0Payload, unwind_len), len(UNWIND)?),
            (
                offset_of!(Rt0Payload, gc_types),
                if len(TYPES)? == 0 { 0 } else { rva(TYPES)? },
            ),
            (offset_of!(Rt0Payload, gc_types_len), len(TYPES)?),
            (
                offset_of!(Rt0Payload, gc_meta),
                if len(META)? == 0 { 0 } else { rva(META)? },
            ),
            (offset_of!(Rt0Payload, gc_meta_len), len(META)?),
        ];
        for (base, fields) in [
            (
                params,
                words
                    .into_iter()
                    .map(|(word, value)| (word * 8, value))
                    .collect::<Vec<_>>(),
            ),
            (payload, fields.into()),
        ] {
            for (offset, value) in fields {
                let at = Location {
                    offset: offset as u64,
                    ..base
                };
                write_at(&mut self.request, at, &value.to_le_bytes())?;
            }
        }
        Ok(())
    }
}

fn write_at(request: &mut MachORequest, at: Location, value: &[u8]) -> Result<(), String> {
    let offset = usize::try_from(at.offset).map_err(|_| "Mach-O fixup offset exceeds usize")?;
    let section = &mut request.sections[at.section];
    let (SectionContent::Data(bytes) | SectionContent::Code(bytes)) = &mut section.content else {
        return Err("Mach-O fixup names a zero-fill section".into());
    };
    bytes
        .get_mut(offset..offset + value.len())
        .ok_or("Mach-O fixup outside section")?
        .copy_from_slice(value);
    Ok(())
}

fn rt0() -> (Vec<u8>, Vec<ObjectReloc>) {
    let mut code = vec![0x48, 0x83, 0xec, 8, 0x89, 0xff]; // sub rsp,8; mov edi,edi
    let mut sites = Vec::new();
    let mut rip = |code: &mut Vec<u8>, op: &[u8], name: &str| {
        code.extend_from_slice(op);
        sites.push(ObjectReloc {
            offset: code.len() as u32,
            kind: RelocKindCode::PcRel32,
            target: name.into(),
            addend: 0,
        });
        code.extend_from_slice(&[0; 4]);
    };
    rip(&mut code, &[0x4c, 0x8d, 0x35], SLOT); // lea r14, initial_slot
    rip(&mut code, &[0x4c, 0x8d, 0x3d], PROCESSOR); // lea r15, processor
    rip(&mut code, &[0x4c, 0x8d, 0x1d], PAYLOAD); // lea r11, payload
    code.extend_from_slice(&[
        0x49, 0x89, 0x53, 120, // mov [r11+120],rdx (envp)
        0x49, 0x89, 0xbb, 128, 0, 0, 0, // mov [r11+128],rdi (argc)
        0x49, 0x89, 0xb3, 136, 0, 0, 0, // mov [r11+136],rsi (argv)
    ]);
    rip(&mut code, &[0x48, 0x8d, 0x05], PARAMS); // lea rax, params
    code.extend_from_slice(&[0x48, 0x89, 0xc3, 0x48, 0x2b, 0x18]); // rbx = rax - [rax]
    rip(&mut code, &[0xe8], &mangle::mangle_runtime("rt0_start"));
    code.extend_from_slice(&[0x89, 0xc7]); // mov edi,eax
    rip(&mut code, &[0xe8], "exit");
    code.extend_from_slice(&[0x0f, 0x0b]);
    (code, sites)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cage_control_is_file_backed_with_the_configured_capacity() {
        for bytes in [0_u64, 32 << 20] {
            let mut world = WorldImage::new();
            world.request.entry =
                world.section("__text", Segment::Text, 1, SectionContent::Code(vec![0xc3]));
            world.runtime_data(bytes, &[]).unwrap();
            let at = world.addr("__gugu_cage_control").unwrap();
            assert_eq!(world.request.sections[at.section].segment, Segment::Data);
            let layout = macho::layout(&world.request).unwrap();
            let image = macho::link(&world.request).unwrap();
            let field = layout.sections[at.section].fileoff
                + at.offset
                + offset_of!(CageControlRecord, configured_bytes) as u64;
            assert_eq!(&image.bytes[field as usize..][..8], &bytes.to_le_bytes());
        }
    }
}
