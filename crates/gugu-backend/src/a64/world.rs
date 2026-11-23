//! Owned image inputs shared by the existing Mach-O and ELF boundaries.
use std::collections::{BTreeMap, BTreeSet};

use super::{Error, codegen, foreign::Platform, glue::RuntimeBindings, layout, metadata};
use crate::{
    NativeImport, NativeImportKind,
    lir::body::{Body, Op, Symbol},
    macho::{Location, Section, SectionContent, Segment},
    x64::mangle,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pointer {
    pub offset: u64,
    pub target: String,
    pub addend: i64,
}

/// Explicit definitions include runtime machine fragments and frozen metadata blobs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Definition {
    pub symbol: String,
    pub align: u64,
    pub segment: Segment,
    pub content: SectionContent,
    pub relocations: Vec<layout::Relocation>,
    pub pointers: Vec<Pointer>,
}

impl Definition {
    pub fn fragment(symbol: impl Into<String>, fragment: super::fragments::Fragment) -> Self {
        Self {
            symbol: symbol.into(),
            align: 16,
            segment: Segment::Text,
            content: SectionContent::Code(fragment.bytes),
            relocations: fragment
                .relocations
                .into_iter()
                .map(|rel| layout::Relocation {
                    offset: u64::from(rel.offset),
                    relocation: rel.relocation,
                })
                .collect(),
            pointers: vec![],
        }
    }
}

pub struct BuildRequest<'a> {
    pub bodies: &'a [Body],
    pub entry: [u8; 32],
    pub platform: Platform,
    pub bindings: &'a RuntimeBindings,
    pub definitions: &'a [Definition],
    pub imports: &'a [NativeImport],
    pub universe: Option<&'a crate::late::universe::TypeUniverse>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionRelocation {
    pub at: Location,
    pub relocation: super::encode::Relocation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PointerRelocation {
    pub at: Location,
    pub target: String,
    pub addend: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct World {
    pub platform: Platform,
    pub sections: Vec<Section>,
    pub symbols: BTreeMap<String, Location>,
    pub sizes: BTreeMap<String, u64>,
    pub relocations: Vec<InstructionRelocation>,
    pub pointers: Vec<PointerRelocation>,
    pub imports: Vec<NativeImport>,
    pub entry: Location,
    pub maps: Location,
    pub unwind: Location,
    pub code: Location,
    /// `__gugutyp` 类型段内符号：名字 → 段内偏移插槽。type section 由整程序装配在
    /// `build` 之后追加（见 `program::assemble`），别名在那里注册进 `symbols`。
    pub type_symbols: Vec<(String, TypeSlot)>,
}

/// 类型段内的符号插槽：`Symbol::TypeDescriptor/TypeRecords/TypeNames` 的镜像符号都落在
/// `__gugutyp` 记录/名字池上，不为每个引用复制 blob（与 x64 `type_descriptors` 同语义）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypeSlot {
    /// `gc_metadata_section::descriptor_offset(type_id)` 的记录偏移。
    Descriptor(u64),
    /// `TypeRecord` 数组基址（段头 records_offset，即 TYPE_HEADER_BYTES）。
    Records,
    /// 类型名池基址（段头 name_offset 字段）。
    Names,
}

fn fail(message: impl std::fmt::Display) -> Error {
    super::error(&message.to_string())
}

pub(crate) fn coverage_symbol(index: u32) -> String {
    mangle::mangle_symbol(&Symbol::Global {
        key: crate::hash::hash_domain("gugu-a64-coverage-v1", &index.to_le_bytes()),
        thread_local: false,
        bytes: 4,
    })
}

fn zero(symbol: String, bytes: u64) -> Definition {
    Definition {
        symbol,
        align: 8,
        segment: Segment::Data,
        content: SectionContent::ZeroFill(bytes),
        relocations: vec![],
        pointers: vec![],
    }
}

fn insert(
    definitions: &mut BTreeMap<String, Definition>,
    definition: Definition,
) -> Result<(), Error> {
    if definitions.contains_key(&definition.symbol) {
        return Err(fail(format!(
            "duplicate A64 definition {}",
            definition.symbol
        )));
    }
    definitions.insert(definition.symbol.clone(), definition);
    Ok(())
}

fn shared(
    definitions: &mut BTreeMap<String, Definition>,
    definition: Definition,
) -> Result<(), Error> {
    if let Some(previous) = definitions.get(&definition.symbol) {
        if previous != &definition {
            return Err(fail(format!(
                "incompatible A64 storage {}",
                definition.symbol
            )));
        }
        return Ok(());
    }
    insert(definitions, definition)
}

fn data_pool(bodies: &[Body]) -> Result<BTreeMap<Vec<u8>, (String, u64)>, Error> {
    let mut pool = BTreeMap::<Vec<u8>, (String, u64)>::new();
    for data in bodies.iter().flat_map(|body| &body.data) {
        if !data.align.is_power_of_two() || data.align > 0x4000 {
            return Err(fail("invalid A64 constant alignment"));
        }
        let entry = pool.entry(data.bytes.clone()).or_insert((String::new(), 1));
        entry.1 = entry.1.max(u64::from(data.align));
    }
    for (index, entry) in pool.values_mut().enumerate() {
        entry.0 =
            mangle::const_symbol(u32::try_from(index).map_err(|_| fail("too many A64 constants"))?);
    }
    Ok(pool)
}

fn body_symbols(
    body: &Body,
    pool: &BTreeMap<Vec<u8>, (String, u64)>,
    definitions: &mut BTreeMap<String, Definition>,
    imports: &[NativeImport],
) -> Result<Vec<(Symbol, String)>, Error> {
    let mut symbols = vec![];
    for instruction in &body.instructions {
        match &instruction.op {
            Op::CoverageCounter(index) => shared(definitions, zero(coverage_symbol(*index), 4))?,
            Op::SymbolAddr(symbol) => {
                let name = match symbol {
                    Symbol::Data(index) => {
                        let data = body
                            .data
                            .get(*index as usize)
                            .ok_or_else(|| fail("A64 local data index out of bounds"))?;
                        pool[&data.bytes].0.clone()
                    }
                    Symbol::Global { bytes, .. } => {
                        let name = mangle::mangle_symbol(symbol);
                        shared(
                            definitions,
                            zero(
                                name.clone(),
                                bytes
                                    .checked_add(8)
                                    .ok_or_else(|| fail("A64 global size overflow"))?,
                            ),
                        )?;
                        name
                    }
                    Symbol::External { name, .. } => {
                        let value = body
                            .results(instruction)
                            .next()
                            .ok_or_else(|| fail("A64 symbol address result missing"))?;
                        let code = body
                            .values
                            .get(value.index())
                            .ok_or_else(|| fail("A64 symbol address result out of bounds"))?
                            .kind
                            .provenance
                            == Some(crate::lir::body::Provenance::Code);
                        if imports.iter().any(|import| {
                            import.name == *name
                                && import.kind
                                    != if code {
                                        NativeImportKind::Function
                                    } else {
                                        NativeImportKind::Data
                                    }
                        }) {
                            return Err(fail("A64 external symbol import kind mismatch"));
                        }
                        continue;
                    }
                    _ => mangle::mangle_symbol(symbol),
                };
                symbols.push((symbol.clone(), name));
            }
            _ => {}
        }
    }
    Ok(symbols)
}

pub fn build(request: BuildRequest<'_>) -> Result<World, Error> {
    build_with_catalog(request, None)
}

pub fn build_runtime(
    request: BuildRequest<'_>,
    catalog: &super::runtime::Catalog<'_>,
) -> Result<World, Error> {
    catalog.validate_bodies(request.bodies)?;
    build_with_catalog(request, Some(catalog))
}

fn build_with_catalog(
    request: BuildRequest<'_>,
    catalog: Option<&super::runtime::Catalog<'_>>,
) -> Result<World, Error> {
    request.bindings.validate()?;
    if !request
        .bodies
        .iter()
        .any(|body| body.instance == request.entry)
    {
        return Err(fail("A64 entry body missing"));
    }
    let pool = data_pool(request.bodies)?;
    let mut definitions = BTreeMap::new();
    for definition in request.definitions {
        insert(&mut definitions, definition.clone())?;
    }
    for (bytes, (name, align)) in &pool {
        insert(
            &mut definitions,
            Definition {
                symbol: name.clone(),
                align: *align,
                segment: Segment::DataConst,
                content: SectionContent::Data(if bytes.is_empty() {
                    vec![0]
                } else {
                    bytes.clone()
                }),
                relocations: vec![],
                pointers: vec![],
            },
        )?;
    }
    let mut bodies: Vec<_> = request.bodies.iter().collect();
    bodies.sort_by_key(|body| body.instance);
    let mut metadata = BTreeMap::new();
    let mut requirements = vec![];
    for body in bodies {
        if super::foreign_packet::platform(body)? != request.platform {
            return Err(fail("A64 world/LIR platform mismatch"));
        }
        let symbols = body_symbols(body, &pool, &mut definitions, request.imports)?;
        let fragment =
            codegen::compile_image(body, request.bindings, &symbols, catalog, request.universe)?;
        requirements.extend(symbols);
        metadata.insert(
            fragment.symbol.clone(),
            fragment
                .metadata
                .ok_or_else(|| fail("A64 function metadata missing"))?,
        );
        insert(
            &mut definitions,
            Definition {
                symbol: fragment.symbol,
                align: 16,
                segment: Segment::Text,
                content: SectionContent::Code(fragment.bytes),
                relocations: fragment.relocations,
                pointers: vec![],
            },
        )?;
    }
    let mut world = assemble(request.platform, definitions, metadata, request.imports)?;
    if let Some(catalog) = catalog {
        for (name, body) in catalog.bindings() {
            let source = super::abi::symbol(&body.instance);
            let Some(&at) = world.symbols.get(&source) else {
                return Err(fail(format!("A64 runtime body {name} is missing")));
            };
            let alias = mangle::mangle_runtime(name);
            world.symbols.insert(alias.clone(), at);
            if let Some(&size) = world.sizes.get(&source) {
                world.sizes.insert(alias, size);
            }
        }
    }
    // 未绑定的 runtime glue 目标兜底到 `__gugu_stub_exit`（与 x64 `resolve_target`
    // 语义一致）：只有片段/体对 `__gugu_runtime_*` 命名的引用才可能走到这里，绑定
    // 入口的别名已在上方注册，余下的都是不可达路径上的引用。
    let stub = "__gugu_stub_exit";
    for rel in &mut world.relocations {
        let target = &mut rel.relocation.target;
        if target.starts_with("__gugu_runtime_") && !world.symbols.contains_key(target.as_str()) {
            *target = stub.to_owned();
        }
    }
    for pointer in &mut world.pointers {
        if pointer.target.starts_with("__gugu_runtime_")
            && !world.symbols.contains_key(pointer.target.as_str())
        {
            pointer.target = stub.to_owned();
        }
    }
    world.entry = *world
        .symbols
        .get(&super::abi::symbol(&request.entry))
        .ok_or_else(|| fail("A64 entry function missing"))?;
    if world.entry.section != 0 {
        return Err(fail("A64 entry is not code"));
    }
    for (symbol, name) in requirements {
        match &symbol {
            // 显式 Definition 已提供该符号（测试/fixture 直接落数据）时走符号表，
            // 不强制 type universe；否则登记 `__gugutyp` 段内插槽。
            _ if world.symbols.contains_key(&name) => {}
            Symbol::TypeDescriptor(key) => {
                let universe = request
                    .universe
                    .ok_or_else(|| fail("A64 type descriptor requires a type universe"))?;
                let type_id = universe
                    .type_id(key)
                    .ok_or_else(|| fail("A64 type descriptor outside the frozen table"))?;
                world.type_symbols.push((
                    name,
                    TypeSlot::Descriptor(
                        crate::runtime::gc_metadata_section::descriptor_offset(type_id),
                    ),
                ));
                continue;
            }
            Symbol::TypeRecords => {
                world.type_symbols.push((name, TypeSlot::Records));
                continue;
            }
            Symbol::TypeNames => {
                world.type_symbols.push((name, TypeSlot::Names));
                continue;
            }
            _ => {}
        }
        let at = world
            .symbols
            .get(&name)
            .ok_or_else(|| fail(format!("undefined A64 symbol {name}")))?;
        let code = matches!(symbol, Symbol::Instance(_) | Symbol::FunctionAdapter { .. });
        if code != matches!(world.sections[at.section].content, SectionContent::Code(_)) {
            return Err(fail("incompatible A64 typed symbol definition"));
        }
    }
    // `type_symbols` 的别名要到整程序装配追加 `__gugutyp` 后才注册进 `symbols`；
    // `validate()` 留给镜像请求（elf/macho request）在别名注册之后执行。
    Ok(world)
}

fn section(segment: Segment, name: &str, content: SectionContent) -> Section {
    Section {
        segment,
        name: name.into(),
        align: 16,
        content,
    }
}

fn assemble(
    platform: Platform,
    definitions: BTreeMap<String, Definition>,
    metadata: BTreeMap<String, metadata::Metadata>,
    imports: &[NativeImport],
) -> Result<World, Error> {
    let start = Location {
        section: 0,
        offset: 0,
    };
    let mut world = World {
        platform,
        sections: vec![
            section(Segment::Text, "__text", SectionContent::Code(vec![])),
            section(Segment::DataConst, "__const", SectionContent::Data(vec![])),
            section(Segment::Data, "__data", SectionContent::Data(vec![])),
            section(Segment::Data, "__bss", SectionContent::ZeroFill(0)),
        ],
        symbols: BTreeMap::new(),
        sizes: BTreeMap::new(),
        relocations: vec![],
        pointers: vec![],
        imports: imports.to_vec(),
        entry: start,
        maps: start,
        unwind: start,
        code: start,
        type_symbols: vec![],
    };
    for definition in definitions.into_values() {
        world.append(definition)?;
    }
    let functions = metadata
        .iter()
        .map(|(name, meta)| crate::runtime::a64_binding::Function {
            code_rva: world.symbols[name].offset,
            maps: &meta.stackmaps,
            unwind: &meta.unwind,
        })
        .collect::<Vec<_>>();
    let metadata = crate::runtime::a64_binding::Sections::encode(&functions).map_err(fail)?;
    world.maps = Location {
        section: world.sections.len(),
        offset: 0,
    };
    world.sections.push(section(
        Segment::DataConst,
        "__gugu_maps",
        SectionContent::Data(metadata.maps().to_vec()),
    ));
    world.unwind = Location {
        section: world.sections.len(),
        offset: 0,
    };
    world.sections.push(section(
        Segment::DataConst,
        "__gugu_cfa",
        SectionContent::Data(metadata.unwind().to_vec()),
    ));
    Ok(world)
}

impl World {
    fn append(&mut self, definition: Definition) -> Result<(), Error> {
        if definition.symbol.is_empty()
            || definition.symbol.contains('\0')
            || !definition.align.is_power_of_two()
            || definition.align > 0x4000
            || definition.content.size() == 0
        {
            return Err(fail("invalid A64 definition name/size/alignment"));
        }
        let index = match (&definition.content, definition.segment) {
            (SectionContent::Code(bytes), Segment::Text)
                if bytes.len().is_multiple_of(4) && definition.align >= 4 =>
            {
                0
            }
            (SectionContent::Data(_), Segment::DataConst) => 1,
            (SectionContent::Data(_), Segment::Data) => 2,
            (SectionContent::ZeroFill(_), Segment::Data) => 3,
            _ => return Err(fail("incompatible A64 definition permissions")),
        };
        if index != 0 && !definition.relocations.is_empty() {
            return Err(fail("A64 instruction relocations require code"));
        }
        if index != 1 && index != 2 && !definition.pointers.is_empty() {
            return Err(fail("A64 pointer relocations require initialized data"));
        }
        let section = &mut self.sections[index];
        section.align = section.align.max(definition.align);
        let offset = section
            .content
            .size()
            .checked_add(definition.align - 1)
            .map(|n| n & !(definition.align - 1))
            .ok_or_else(|| fail("A64 section alignment overflow"))?;
        let size = definition.content.size();
        let end = offset
            .checked_add(size)
            .ok_or_else(|| fail("A64 section size overflow"))?;
        match (&mut section.content, definition.content) {
            (SectionContent::Code(bytes), SectionContent::Code(data))
            | (SectionContent::Data(bytes), SectionContent::Data(data)) => {
                bytes
                    .try_reserve_exact(
                        usize::try_from(end)
                            .map_err(fail)?
                            .saturating_sub(bytes.len()),
                    )
                    .map_err(fail)?;
                bytes.resize(usize::try_from(offset).map_err(fail)?, 0);
                bytes.extend(data);
            }
            (SectionContent::ZeroFill(bytes), SectionContent::ZeroFill(_)) => *bytes = end,
            _ => unreachable!(),
        }
        let at = Location {
            section: index,
            offset,
        };
        self.sizes.insert(definition.symbol.clone(), size);
        self.symbols.insert(definition.symbol, at);
        let mut occupied = BTreeSet::new();
        for rel in definition.relocations {
            if rel.offset % 4 != 0
                || rel.offset.checked_add(4).is_none_or(|n| n > size)
                || !occupied.insert(rel.offset)
            {
                return Err(fail("invalid/duplicate A64 instruction relocation"));
            }
            self.relocations.push(InstructionRelocation {
                at: Location {
                    section: index,
                    offset: offset + rel.offset,
                },
                relocation: rel.relocation,
            });
        }
        for rel in definition.pointers {
            if rel.offset % 8 != 0
                || rel.offset.checked_add(8).is_none_or(|n| n > size)
                || (offset + rel.offset) % 8 != 0
                || !occupied.insert(rel.offset)
            {
                return Err(fail("invalid/duplicate A64 pointer relocation"));
            }
            self.pointers.push(PointerRelocation {
                at: Location {
                    section: index,
                    offset: offset + rel.offset,
                },
                target: rel.target,
                addend: rel.addend,
            });
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        self.location(self.entry, 4, true)?;
        if self.symbols.len() != self.sizes.len() {
            return Err(fail("A64 symbol extents missing"));
        }
        for (name, at) in &self.symbols {
            let size = *self
                .sizes
                .get(name)
                .ok_or_else(|| fail("A64 symbol extent missing"))?;
            if size == 0 {
                return Err(fail("A64 zero-sized symbol extent"));
            }
            self.location(*at, size, false)?;
        }
        let mut names: BTreeSet<_> = self.symbols.keys().cloned().collect();
        let mut imports = BTreeSet::new();
        for import in &self.imports {
            if import.library.is_empty() || import.library.contains('\0') {
                return Err(fail("invalid A64 native library"));
            }
            if import.kind == NativeImportKind::Library {
                continue;
            }
            if import.name.is_empty() || import.name.contains('\0') || !imports.insert(&import.name)
            {
                return Err(fail("invalid/duplicate A64 native import"));
            }
            // Darwin 的 C 函数在镜像内由 `__gugu_darwin_c_*` 桩定义占名；Linux 的调用点
            // 另有 GOT 桩符号，import 在这里只申报动态符号真名。
            let name = if import.kind == NativeImportKind::Function
                && self.platform == super::foreign::Platform::Darwin
            {
                self.platform.symbol(&import.name)
            } else {
                import.name.clone()
            };
            if !names.insert(name) {
                return Err(fail("duplicate A64 import/definition"));
            }
        }
        for name in self
            .relocations
            .iter()
            .map(|rel| &rel.relocation.target)
            .chain(self.pointers.iter().map(|rel| &rel.target))
        {
            if !names.contains(name) {
                return Err(fail(format!("undefined A64 symbol {name}")));
            }
        }
        for rel in &self.relocations {
            self.location(rel.at, 4, true)?;
            self.target_extent(&rel.relocation.target, rel.relocation.addend)?;
            if matches!(
                rel.relocation.kind,
                super::inst::RelocKind::Call26 | super::inst::RelocKind::Branch26
            ) && self.imports.iter().any(|import| {
                import.name == rel.relocation.target && import.kind == NativeImportKind::Data
            }) {
                return Err(fail("A64 cannot branch to imported data"));
            }
            if matches!(
                rel.relocation.kind,
                super::inst::RelocKind::Call26 | super::inst::RelocKind::Branch26
            ) && let Some(at) = self.symbols.get(&rel.relocation.target)
            {
                self.location(*at, 4, true)?;
            }
        }
        let mut occupied = BTreeSet::new();
        for rel in &self.pointers {
            self.location(rel.at, 8, false)?;
            if rel.at.offset % 8 != 0
                || !matches!(
                    self.sections[rel.at.section].content,
                    SectionContent::Data(_)
                )
                || !occupied.insert(rel.at)
            {
                return Err(fail("invalid A64 pointer source location"));
            }
            self.target_extent(&rel.target, rel.addend)?;
        }
        Ok(())
    }

    fn target_extent(&self, name: &str, addend: i64) -> Result<(), Error> {
        if self
            .sizes
            .get(name)
            .is_some_and(|size| addend < 0 || addend as u64 >= *size)
        {
            return Err(fail("A64 target outside its definition"));
        }
        Ok(())
    }

    fn location(&self, at: Location, bytes: u64, code: bool) -> Result<(), Error> {
        let section = self
            .sections
            .get(at.section)
            .ok_or_else(|| fail("A64 location section out of bounds"))?;
        if at
            .offset
            .checked_add(bytes)
            .is_none_or(|end| end > section.content.size())
            || code
                && (!at.offset.is_multiple_of(4)
                    || !matches!(section.content, SectionContent::Code(_)))
        {
            return Err(fail("invalid A64 image location"));
        }
        Ok(())
    }
}
