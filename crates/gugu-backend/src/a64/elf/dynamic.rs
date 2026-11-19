use std::collections::{BTreeMap, BTreeSet};

use super::{
    ElfImage, ElfRequest, Layout, Location, Section, SectionContent, Segment, headers,
    link_prepared, put, section_at,
};
use crate::{NativeImport, NativeImportKind};

pub const R_AARCH64_GLOB_DAT: u64 = 1025;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Import {
    pub native: NativeImport,
    pub slot: Location,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DynamicLink {
    pub interpreter: String,
    pub imports: Vec<Import>,
}

pub fn link_dynamic(request: &ElfRequest, config: &DynamicLink) -> Result<ElfImage, String> {
    let mut request = request.clone();
    let tables = prepare(&mut request, config)?;
    link_prepared(&request, Some(&tables))
}

pub(super) struct Tables {
    pub imports: Vec<Import>,
    interp: Location,
    strings: Location,
    symbols: Location,
    hash: Location,
    dynamic: Location,
    needed: Vec<u64>,
}

pub(super) fn prepare(request: &mut ElfRequest, config: &DynamicLink) -> Result<Tables, String> {
    validate(request, config)?;
    let mut strings = vec![0];
    let mut libraries = BTreeMap::new();
    let mut needed = vec![];
    let mut symbols = vec![0; 24];
    for import in &config.imports {
        let native = &import.native;
        if !libraries.contains_key(&native.library) {
            let offset = string(&mut strings, &native.library)?;
            libraries.insert(&native.library, offset);
            needed.push(u64::from(offset));
        }
        let name = string(&mut strings, &native.name)?;
        let mut symbol = [0; 24];
        symbol[..4].copy_from_slice(&name.to_le_bytes());
        symbol[4] = if native.kind == NativeImportKind::Function {
            0x12
        } else {
            0x11
        };
        symbols.extend(symbol);
    }
    let count = u32::try_from(config.imports.len() + 1).map_err(|_| "too many ELF imports")?;
    let hash = [1, count, 1]
        .into_iter()
        .chain((0..count).map(|i| if i != 0 && i + 1 < count { i + 1 } else { 0 }))
        .flat_map(u32::to_le_bytes)
        .collect();
    Ok(Tables {
        imports: config.imports.clone(),
        interp: append(
            request,
            ".interp",
            [config.interpreter.as_bytes(), &[0]].concat(),
        ),
        strings: append(request, ".dynstr", strings),
        symbols: append(request, ".dynsym", symbols),
        hash: append(request, ".hash", hash),
        dynamic: append(request, ".dynamic", vec![0; (needed.len() + 10) * 16]),
        needed,
    })
}

fn validate(request: &ElfRequest, config: &DynamicLink) -> Result<(), String> {
    if !config.interpreter.starts_with('/') || config.interpreter.contains('\0') {
        return Err("ELF interpreter must be an absolute NUL-free path".into());
    }
    if config.imports.is_empty() {
        return Err("dynamic ELF requires imports".into());
    }
    let mut names = BTreeSet::new();
    let mut slots: BTreeSet<_> = request.relatives.iter().map(|r| r.at).collect();
    for import in &config.imports {
        let native = &import.native;
        if native.kind == NativeImportKind::Library
            || [&native.library, &native.name]
                .into_iter()
                .any(|s| s.is_empty() || s.contains('\0'))
            || !names.insert(&native.name)
        {
            return Err("ELF imports require unique named functions or data and a library".into());
        }
        let section = section_at(request, import.slot, 8)?;
        if section.segment != Segment::Data
            || section.align < 8
            || import.slot.offset % 8 != 0
            || !matches!(section.content, SectionContent::Data(_))
            || !slots.insert(import.slot)
        {
            return Err("ELF import slot must be unique aligned file-backed RW data".into());
        }
    }
    Ok(())
}

fn string(bytes: &mut Vec<u8>, value: &str) -> Result<u32, String> {
    let offset = u32::try_from(bytes.len()).map_err(|_| "ELF dynamic string table exceeds u32")?;
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    Ok(offset)
}

fn append(request: &mut ElfRequest, name: &str, bytes: Vec<u8>) -> Location {
    let at = Location {
        section: request.sections.len(),
        offset: 0,
    };
    request.sections.push(Section {
        name: name.into(),
        segment: Segment::DataConst,
        align: 8,
        content: SectionContent::Data(bytes),
    });
    at
}

impl Tables {
    pub(super) fn write(
        &self,
        bytes: &mut [u8],
        request: &ElfRequest,
        layout: &Layout,
    ) -> Result<(), String> {
        for (index, kind, location) in [(1, 3, self.interp), (6, 2, self.dynamic)] {
            let at = layout.address(location)?;
            let size = layout.sections[location.section].size;
            headers::phdr(bytes, index, kind, 4, (at, size, size), 8);
        }
        let entries = self.needed.iter().map(|&value| (1, value)).chain([
            (4, layout.address(self.hash)?),
            (5, layout.address(self.strings)?),
            (6, layout.address(self.symbols)?),
            (7, layout.rela),
            (8, layout.rela_count * 24),
            (9, 24),
            (10, layout.sections[self.strings.section].size),
            (11, 24),
            (24, 0),
            (0, 0),
        ]);
        for (index, (tag, value)) in entries.enumerate() {
            let at = layout.address(self.dynamic)? as usize + index * 16;
            put(bytes, at, tag);
            put(bytes, at + 8, value);
        }
        for (index, import) in self.imports.iter().enumerate() {
            let at = layout.rela as usize + (request.relatives.len() + index) * 24;
            let slot = layout.address(import.slot)?;
            put(bytes, slot as usize, 0);
            put(bytes, at, slot);
            put(
                bytes,
                at + 8,
                ((index as u64 + 1) << 32) | R_AARCH64_GLOB_DAT,
            );
        }
        self.section_headers(bytes, layout)
    }

    fn section_headers(&self, bytes: &mut [u8], layout: &Layout) -> Result<(), String> {
        for (index, name, kind, location, link, size) in [
            (3, ".dynsym", 11, self.symbols, 4, 24),
            (4, ".dynstr", 3, self.strings, 0, 0),
            (5, ".hash", 5, self.hash, 3, 4),
            (6, ".dynamic", 6, self.dynamic, 4, 16),
        ] {
            let at = layout.shoff as usize + index * 64;
            let name = headers::SHSTRTAB
                .windows(name.len() + 1)
                .position(|s| s[..s.len() - 1] == *name.as_bytes() && s[s.len() - 1] == 0)
                .unwrap();
            headers::word(bytes, at, name as u32);
            headers::word(bytes, at + 4, kind);
            headers::word(bytes, at + 40, link);
            if kind == 11 {
                headers::word(bytes, at + 44, 1);
            }
            for (offset, value) in [
                (8, 2),
                (16, layout.address(location)?),
                (24, layout.address(location)?),
                (32, layout.sections[location.section].size),
                (48, 8),
                (56, size),
            ] {
                put(bytes, at + offset, value);
            }
        }
        headers::word(bytes, layout.shoff as usize + 64 + 40, 3);
        Ok(())
    }
}
