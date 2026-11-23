use std::collections::BTreeMap;
use std::mem::offset_of;

use super::{append_import_stubs, import_stub_symbol, prebuilt_import_stubs, relocate_import_stub};
use crate::{
    NativeImport, NativeImportKind,
    a64::{
        fragments::{Fragment, darwin_main, rt0},
        relocation,
    },
    macho::{
        self, Arch, Fixup, ImportSlot, Layout, Location, MachImage, MachORequest, Section,
        SectionContent, Segment,
    },
    runtime::{
        entry::{INITIAL_SLOT_BYTES, Rt0Payload, rt0_params},
        processor::PROCESSOR_PAGE_BYTES,
    },
};

const PARAMS: &str = "__gugu_rt0_params";
const RT0: &str = "__gugu_rt0";
const RT0_START: &str = "__gugu_rt0_start";

/// 由调用方排布的 runtime 位置；载荷位于可写且有文件内容的节中。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DarwinEntry {
    pub rt0_start: Location,
    pub payload: Location,
    pub initial_slot: Location,
    pub processor: Location,
    /// 绑定镜像 runtime 时补齐的启动契约；`None` 走只含 rt0_start 的探针形态。
    pub runtime: Option<DarwinRuntime>,
}

/// 生产 runtime 启动契约：main 入口、runtime 状态区、协程入口、返回哨兵与元数据节。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DarwinRuntime {
    /// 用户 `main` 入口。
    pub entry: Location,
    /// runtime 状态区（可写零初始化）。
    pub runtime_state: Location,
    /// 子协程入口片段。
    pub coroutine_enter: Location,
    /// main 返回哨兵片段。
    pub main_return: Location,
    /// 函数 RVA 的基址（代码节起点）。
    pub fragment_base: Location,
    /// 栈图节。
    pub stackmap: Location,
    /// 统一展开节。
    pub unwind: Location,
    /// 类型段；`None` 时字段写 0。
    pub gc_types: Option<Location>,
    /// GC metadata blob；`None` 时字段写 0。
    pub gc_meta: Option<Location>,
}

/// 添加 arm64 Darwin 启动节，绑定 libSystem 的 exit 并写出签名镜像。
/// 调用方提供独立的 rt0_start 代码和其余运行时数据；原有 entry 字段被替换。
pub fn link_entry(mut request: MachORequest, entry: DarwinEntry) -> Result<MachImage, String> {
    if request.arch != Arch::Arm64 {
        return Err("Darwin arm64 entry requires an arm64 Mach-O image".to_owned());
    }
    validate_entry(&request, entry)?;
    request.entry = entry.rt0_start;
    let mut prebuilt = prebuilt_import_stubs(&request)?;
    let exit = import_stub_symbol("exit");
    let main = darwin_main(PARAMS, RT0, &exit);
    let rt0 = rt0(RT0_START);
    let params = append(
        &mut request,
        "__rt0_params",
        8,
        SectionContent::Data(vec![0; rt0_params::BYTES as usize]),
    );
    let boot = append(
        &mut request,
        "__rt0",
        4,
        SectionContent::Code(rt0.bytes.clone()),
    );
    let main_at = append(
        &mut request,
        "__lc_main",
        4,
        SectionContent::Code(main.bytes.clone()),
    );
    request.entry = main_at;
    let existing = prebuilt.get_mut(&exit);
    let slot = exit_slot(&mut request, existing.as_ref().map(|stub| stub.1))?;
    let exit_at = if let Some((at, target)) = existing {
        *target = slot.at;
        *at
    } else {
        let stubs = append_import_stubs(&mut request, &BTreeMap::from([("exit".into(), slot)]))?;
        stubs[&exit]
    };
    let layout = macho::layout(&request)?;
    for (_, (at, slot)) in prebuilt {
        relocate_import_stub(&mut request, &layout, at, slot)?;
    }
    let symbols = BTreeMap::from([
        (PARAMS, params),
        (RT0, boot),
        (RT0_START, entry.rt0_start),
        (&exit, exit_at),
    ]);
    for (at, fragment) in [(main_at, main), (boot, rt0)] {
        request.sections[at.section].content =
            SectionContent::Code(relocate(fragment, at, &layout, &symbols)?);
    }
    write_startup_data(&mut request, &layout, params, entry)?;
    macho::link(&request)
}

fn exit_slot(request: &mut MachORequest, existing: Option<Location>) -> Result<ImportSlot, String> {
    let Some(import) = request
        .imports
        .iter()
        .position(|import| import.dylib == macho::LIB_SYSTEM && import.name == "_exit")
    else {
        let slots = macho::append_native_imports(
            request,
            &[NativeImport::function(macho::LIB_SYSTEM, "exit")],
        )?;
        return Ok(slots["exit"]);
    };
    let bound = request
        .fixups
        .iter()
        .filter_map(|fixup| match *fixup {
            Fixup::Bind {
                at,
                import: index,
                addend: 0,
            } if index == import => Some(at),
            _ => None,
        })
        .min_by_key(|&at| Some(at) != existing);
    let at = if let Some(at) = bound {
        at
    } else {
        let at = Location {
            section: request.sections.len(),
            offset: 0,
        };
        request.sections.push(Section {
            segment: Segment::DataConst,
            name: "__got".to_owned(),
            align: 8,
            content: SectionContent::Data(vec![0; 8]),
        });
        request.fixups.push(Fixup::Bind {
            at,
            import,
            addend: 0,
        });
        at
    };
    Ok(ImportSlot {
        at,
        kind: NativeImportKind::Function,
    })
}

fn append(request: &mut MachORequest, name: &str, align: u64, content: SectionContent) -> Location {
    let section = request.sections.len();
    request.sections.push(Section {
        segment: Segment::Text,
        name: name.to_owned(),
        align,
        content,
    });
    Location { section, offset: 0 }
}

fn validate_entry(request: &MachORequest, entry: DarwinEntry) -> Result<(), String> {
    for (name, at, size, align) in [
        ("rt0_start", entry.rt0_start, 4, 4),
        ("payload", entry.payload, Rt0Payload::bytes(), 8),
        ("initial slot", entry.initial_slot, INITIAL_SLOT_BYTES, 8),
        ("processor", entry.processor, PROCESSOR_PAGE_BYTES as u64, 8),
    ] {
        let section = request.sections.get(at.section).filter(|section| {
            section.align >= align
                && at.offset % align == 0
                && at
                    .offset
                    .checked_add(size)
                    .is_some_and(|end| end <= section.content.size())
        });
        let valid = section.is_some_and(|section| match name {
            "rt0_start" => matches!(section.content, SectionContent::Code(_)),
            "payload" => {
                section.segment == Segment::Data
                    && matches!(section.content, SectionContent::Data(_))
            }
            _ => section.segment == Segment::Data,
        });
        if !valid {
            return Err(format!("invalid arm64 Mach-O {name} location {at:?}"));
        }
    }
    if let Some(runtime) = entry.runtime {
        for (name, at) in [
            ("entry", runtime.entry),
            ("coroutine_enter", runtime.coroutine_enter),
            ("main_return", runtime.main_return),
        ] {
            let valid = request
                .sections
                .get(at.section)
                .is_some_and(|section| matches!(section.content, SectionContent::Code(_)));
            if !valid {
                return Err(format!("invalid arm64 Mach-O {name} location {at:?}"));
            }
        }
        for (name, at, size) in [
            (
                "runtime_state",
                runtime.runtime_state,
                crate::runtime::entry::state::BYTES,
            ),
            ("stackmap", runtime.stackmap, 0),
            ("unwind", runtime.unwind, 0),
            ("fragment_base", runtime.fragment_base, 0),
        ] {
            let valid = request.sections.get(at.section).is_some_and(|section| {
                at.offset
                    .checked_add(size)
                    .is_some_and(|end| end <= section.content.size())
            });
            if !valid {
                return Err(format!("invalid arm64 Mach-O {name} location {at:?}"));
            }
        }
        for (name, at) in [("gc_types", runtime.gc_types), ("gc_meta", runtime.gc_meta)] {
            if let Some(at) = at
                && request.sections.get(at.section).is_none()
            {
                return Err(format!("invalid arm64 Mach-O {name} location {at:?}"));
            }
        }
    }
    Ok(())
}

fn write_startup_data(
    request: &mut MachORequest,
    layout: &Layout,
    params: Location,
    entry: DarwinEntry,
) -> Result<(), String> {
    let rva = |at| layout.address(at).map(|addr| addr - macho::IMAGE_BASE);
    let slot = rva(entry.initial_slot)?;
    let SectionContent::Data(bytes) = &mut request.sections[params.section].content else {
        unreachable!()
    };
    let mut words = vec![
        (rt0_params::BIAS_BASELINE, rva(params)?),
        (rt0_params::BOOT_R14, slot),
        (rt0_params::PROCESSOR, rva(entry.processor)?),
        (rt0_params::RT0_START, rva(entry.rt0_start)?),
        (rt0_params::PAYLOAD_VA, rva(entry.payload)?),
        (rt0_params::PAYLOAD_LEN, Rt0Payload::bytes()),
        (rt0_params::INITIAL_SLOT, slot),
    ];
    if let Some(runtime) = entry.runtime {
        words.extend([
            (rt0_params::ENTRY, rva(runtime.entry)?),
            (rt0_params::RUNTIME_STATE, rva(runtime.runtime_state)?),
            (rt0_params::COROUTINE_ENTER, rva(runtime.coroutine_enter)?),
        ]);
    }
    for (word, value) in words {
        bytes[word * 8..word * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    let SectionContent::Data(bytes) = &mut request.sections[entry.payload.section].content else {
        unreachable!()
    };
    let mut fields = vec![
        (offset_of!(Rt0Payload, revision), Rt0Payload::REVISION),
        (offset_of!(Rt0Payload, initial_slot), slot),
    ];
    if let Some(runtime) = entry.runtime {
        let blob = |at: Option<Location>| -> Result<(u64, u64), String> {
            match at {
                None => Ok((0, 0)),
                Some(at) => {
                    let size = layout.sections[at.section].size;
                    Ok((if size == 0 { 0 } else { rva(at)? }, size))
                }
            }
        };
        let (types, types_len) = blob(runtime.gc_types)?;
        let (meta, meta_len) = blob(runtime.gc_meta)?;
        fields.extend([
            (offset_of!(Rt0Payload, main_entry), rva(runtime.entry)?),
            (
                offset_of!(Rt0Payload, runtime_state),
                rva(runtime.runtime_state)?,
            ),
            (
                offset_of!(Rt0Payload, main_return),
                rva(runtime.main_return)?,
            ),
            (offset_of!(Rt0Payload, stackmap), rva(runtime.stackmap)?),
            (
                offset_of!(Rt0Payload, fragment_base),
                rva(runtime.fragment_base)?,
            ),
            (
                offset_of!(Rt0Payload, stackmap_len),
                layout.sections[runtime.stackmap.section].size,
            ),
            (offset_of!(Rt0Payload, unwind), rva(runtime.unwind)?),
            (
                offset_of!(Rt0Payload, unwind_len),
                layout.sections[runtime.unwind.section].size,
            ),
            (offset_of!(Rt0Payload, gc_types), types),
            (offset_of!(Rt0Payload, gc_types_len), types_len),
            (offset_of!(Rt0Payload, gc_meta), meta),
            (offset_of!(Rt0Payload, gc_meta_len), meta_len),
        ]);
    }
    for (offset, value) in fields {
        let offset = entry.payload.offset as usize + offset;
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    Ok(())
}

fn relocate(
    fragment: Fragment,
    at: Location,
    layout: &Layout,
    symbols: &BTreeMap<&str, Location>,
) -> Result<Vec<u8>, String> {
    relocation::relocate(fragment, layout.address(at)?, |name| {
        let target = symbols
            .get(name)
            .ok_or_else(|| format!("unresolved arm64 Mach-O startup symbol {name}"))?;
        layout.address(*target)
    })
}
