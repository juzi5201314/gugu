//! 整程序 A64 镜像装配：closed-world LIR 体、runtime 片段与启动存储拼成可执行镜像。
//!
//! 前端只提供编译好的 `Body`、runtime catalog 与 GC 元数据；这里补齐启动载荷、
//! 初始协程槽、`LogicalProcessor` 与 runtime 状态区四个保留区，再按平台走
//! Mach-O `link_entry` 或 ELF `link_entry(_dynamic)` 出完整镜像。

use super::{
    definitions::{runtime_definitions, runtime_imports},
    elf::{self, DynamicLink, Import, LinuxEntry, LinuxRuntime},
    foreign::Platform,
    fragments::linux_import_slot_symbol,
    glue::{ForeignBindings, RuntimeBindings},
    macho::{self, DarwinEntry, DarwinRuntime},
    runtime::Catalog,
    world::{BuildRequest, Definition, TypeSlot, World, build_runtime},
};
use crate::{
    late::universe::TypeUniverse,
    lir::body::Body,
    macho::{Location, Section, SectionContent, Segment},
    runtime::{
        entry::{INITIAL_SLOT_BYTES, Rt0Payload, state},
        processor::PROCESSOR_PAGE_BYTES,
    },
    x64::mangle,
};

use super::Error;
use super::error as fail;

/// rt0 启动载荷。
pub(crate) const PAYLOAD: &str = "__gugu_a64_payload";
/// 初始协程槽（rt0 装入 r14 的地址）。
pub(crate) const INITIAL_SLOT: &str = "__gugu_a64_initial_slot";
/// 初始 `LogicalProcessor` 页。
pub(crate) const PROCESSOR: &str = "__gugu_a64_processor";
/// runtime 状态区（首部为 `MorestackMailbox`）。
pub(crate) const RUNTIME_STATE: &str = "__gugu_a64_runtime_state";
/// 类型段（`.gugutyp` 对应物）。
pub(crate) const GC_TYPES: &str = "__gugu_a64_gc_types";
/// GC metadata blob（`.ggmeta` 对应物）。
pub(crate) const GC_META: &str = "__gugu_a64_gc_meta";
/// cage 控制记录（`CageControlRecord` 启动字节）。
pub(crate) const CAGE_CONTROL: &str = "__gugu_cage_control";

fn rt(name: &str) -> String {
    mangle::mangle_runtime(name)
}

/// 整程序装配输入；`catalog` 必须解析过本次 bodies 覆盖的 glue。
pub struct ProgramRequest<'a> {
    /// 全部 LIR 体（含 entry 与 catalog 引用的 runtime 体）。
    pub bodies: &'a [Body],
    /// 用户 `main` 的实例 key。
    pub entry: [u8; 32],
    /// 目标平台；与 bodies 的平台必须一致（world 侧再校验）。
    pub platform: Platform,
    /// 已解析的 runtime catalog。
    pub catalog: &'a Catalog<'a>,
    /// 类型段字节；为空时载荷字段写 0。
    pub gc_types: &'a [u8],
    /// GC metadata blob；为空时载荷字段写 0。
    pub gc_meta: &'a [u8],
    /// Linux `PT_INTERP`；Darwin 忽略。
    pub interpreter: &'a str,
    /// 压缩契约预留的 cage 字节数；未启用 cage 时为 0。
    pub cage_bytes: u64,
    /// Darwin 镜像标识；Linux 忽略。
    pub identifier: &'a str,
    /// 类型宇宙（foreign 类型签名用）。
    pub universe: Option<&'a TypeUniverse>,
}

/// 装配产物：world 留给上层消费（`plan` 需要节图与符号），image 是可执行字节。
#[derive(Clone, Debug)]
pub struct Program {
    pub world: World,
    pub image: Vec<u8>,
}

fn storage(cage_bytes: u64) -> Vec<Definition> {
    let data = |symbol: &str, bytes: Vec<u8>| Definition {
        symbol: symbol.into(),
        align: 8,
        segment: Segment::Data,
        content: SectionContent::Data(bytes),
        relocations: vec![],
        pointers: vec![],
    };
    let zero = |symbol: &str, bytes: u64, align: u64| Definition {
        symbol: symbol.into(),
        align,
        segment: Segment::Data,
        content: SectionContent::ZeroFill(bytes),
        relocations: vec![],
        pointers: vec![],
    };
    vec![
        data(PAYLOAD, vec![0; Rt0Payload::bytes() as usize]),
        data(INITIAL_SLOT, vec![0; INITIAL_SLOT_BYTES as usize]),
        // `LogicalProcessor` 页必须是 4KiB 对齐的零初始化区。
        zero(PROCESSOR, PROCESSOR_PAGE_BYTES as u64, 4096),
        zero(RUNTIME_STATE, state::BYTES, 8),
        data(
            CAGE_CONTROL,
            crate::runtime::cage_control::CageControlRecord::boot_bytes(cage_bytes),
        ),
    ]
}

fn bindings() -> RuntimeBindings {
    RuntimeBindings {
        morestack_or_poll: rt("morestack_or_poll"),
        safepoint_slow: rt("safepoint_slow"),
        foreign: Some(ForeignBindings {
            ordinary_dispatch: rt("foreign_dispatch"),
            dirty_dispatch: rt("foreign_dirty_dispatch"),
        }),
    }
}

fn assemble(request: &ProgramRequest<'_>) -> Result<World, Error> {
    let definitions = runtime_definitions(request.platform)
        .into_iter()
        .chain(storage(request.cage_bytes))
        .collect::<Vec<_>>();
    let imports = runtime_imports(request.platform);
    let mut world = build_runtime(
        BuildRequest {
            bodies: request.bodies,
            entry: request.entry,
            platform: request.platform,
            bindings: &bindings(),
            definitions: &definitions,
            imports: &imports,
            universe: request.universe,
        },
        request.catalog,
    )?;
    // 两类 GC 元数据各独占一个只读节（与 `__gugu_maps`/`__gugu_cfa` 同法）：载荷的
    // 长度字段按节尾截断，共享 `__const` 会把后续常量误计入元数据长度。
    let mut type_section = None;
    for (name, symbol, bytes) in [
        ("__gugutyp", GC_TYPES, request.gc_types),
        ("__ggmeta", GC_META, request.gc_meta),
    ] {
        if bytes.is_empty() {
            continue;
        }
        let at = Location {
            section: world.sections.len(),
            offset: 0,
        };
        if symbol == GC_TYPES {
            type_section = Some(at.section);
        }
        world.sections.push(Section {
            segment: Segment::DataConst,
            name: name.into(),
            align: 8,
            content: SectionContent::Data(bytes.to_vec()),
        });
        world.symbols.insert(symbol.into(), at);
        world.sizes.insert(symbol.into(), bytes.len() as u64);
    }
    register_type_symbols(&mut world, type_section, request.gc_types)?;
    Ok(world)
}

/// `World::type_symbols` 记的是 `__gugutyp` 段内偏移插槽；节追加后在这里落成真实符号。
/// Records/Names 偏移取自段头字段，避免与 gc_metadata_section 的布局常量脱同步。
fn register_type_symbols(
    world: &mut World,
    section: Option<usize>,
    gc_types: &[u8],
) -> Result<(), Error> {
    if world.type_symbols.is_empty() {
        return Ok(());
    }
    let index = section.ok_or_else(|| fail("A64 type symbols without a type section"))?;
    let header = |at: usize| -> Result<u64, Error> {
        gc_types
            .get(at..at + 8)
            .map(|f| u64::from_le_bytes(f.try_into().expect("8 字节字段")))
            .ok_or_else(|| fail("A64 truncated __gugutyp header"))
    };
    let (records, names) = (header(24)?, header(64)?);
    for (name, slot) in &world.type_symbols {
        let offset = match slot {
            TypeSlot::Descriptor(offset) => *offset,
            TypeSlot::Records => records,
            TypeSlot::Names => names,
        };
        if offset >= gc_types.len() as u64 {
            return Err(fail("A64 type symbol outside __gugutyp"));
        }
        world.symbols.insert(
            name.clone(),
            Location {
                section: index,
                offset,
            },
        );
        // 描述符按单条记录定界；Records/Names 是区域起点，宽松到节尾（addend 仍受节界校验）。
        let size = match slot {
            TypeSlot::Descriptor(_) => {
                crate::runtime::gc_metadata_section::TYPE_RECORD_BYTES as u64
            }
            _ => gc_types.len() as u64 - offset,
        };
        world.sizes.insert(name.clone(), size);
    }
    Ok(())
}

fn symbol(world: &World, name: &str) -> Result<Location, Error> {
    world
        .symbols
        .get(name)
        .copied()
        .ok_or_else(|| fail(&format!("undefined A64 symbol {name}")))
}

fn optional(world: &World, name: &str) -> Option<Location> {
    world.symbols.get(name).copied()
}

/// 装配为完整可执行镜像。
pub fn build(request: &ProgramRequest<'_>) -> Result<Program, Error> {
    let world = assemble(request)?;
    let image = match request.platform {
        Platform::Linux => linux_image(&world, request)?,
        Platform::Darwin => darwin_image(&world, request)?,
    };
    Ok(Program { world, image })
}

fn linux_image(world: &World, request: &ProgramRequest<'_>) -> Result<Vec<u8>, Error> {
    let mut imports = vec![];
    for native in runtime_imports(Platform::Linux) {
        let slot = symbol(world, &linux_import_slot_symbol(&native.name))?;
        imports.push(Import { native, slot });
    }
    let dynamic = DynamicLink {
        interpreter: request.interpreter.into(),
        imports,
    };
    let entry = LinuxEntry {
        rt0_start: symbol(world, &rt("rt0_start"))?,
        payload: symbol(world, PAYLOAD)?,
        initial_slot: symbol(world, INITIAL_SLOT)?,
        processor: symbol(world, PROCESSOR)?,
        runtime: Some(LinuxRuntime {
            entry: world.entry,
            runtime_state: symbol(world, RUNTIME_STATE)?,
            coroutine_enter: symbol(world, &rt("coroutine_enter"))?,
            main_return: symbol(world, &rt("main_return"))?,
            fragment_base: world.code,
            stackmap: world.maps,
            unwind: world.unwind,
            gc_types: optional(world, GC_TYPES),
            gc_meta: optional(world, GC_META),
        }),
    };
    elf::link_entry_dynamic(world.elf_request()?, entry, &dynamic)
        .map(|image| image.bytes)
        .map_err(|message| fail(message.as_str()))
}

fn darwin_image(world: &World, request: &ProgramRequest<'_>) -> Result<Vec<u8>, Error> {
    let entry = DarwinEntry {
        rt0_start: symbol(world, &rt("rt0_start"))?,
        payload: symbol(world, PAYLOAD)?,
        initial_slot: symbol(world, INITIAL_SLOT)?,
        processor: symbol(world, PROCESSOR)?,
        runtime: Some(DarwinRuntime {
            entry: world.entry,
            runtime_state: symbol(world, RUNTIME_STATE)?,
            coroutine_enter: symbol(world, &rt("coroutine_enter"))?,
            main_return: symbol(world, &rt("main_return"))?,
            fragment_base: world.code,
            stackmap: world.maps,
            unwind: world.unwind,
            gc_types: optional(world, GC_TYPES),
            gc_meta: optional(world, GC_META),
        }),
    };
    macho::link_entry(world.macho_request(request.identifier)?, entry)
        .map(|image| image.bytes)
        .map_err(|message| fail(message.as_str()))
}
