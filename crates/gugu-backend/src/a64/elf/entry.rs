use std::{collections::BTreeMap, mem::offset_of};

use super::{
    DynamicLink, ElfImage, ElfRequest, Layout, Location, Relative, Section, SectionContent,
    Segment, dynamic, link_prepared, prepared_layout, relocate_fragment, section_at,
};
use crate::{
    a64::fragments::{linux_main, rt0},
    runtime::{
        entry::{INITIAL_SLOT_BYTES, Rt0Payload, rt0_params, state},
        processor::PROCESSOR_PAGE_BYTES,
    },
};

const PARAMS: &str = "__gugu_rt0_params";
const RT0: &str = "__gugu_rt0";
const RT0_START: &str = "__gugu_rt0_start";

/// Runtime storage supplied by the caller; remaining payload pointers are caller fixups.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinuxEntry {
    pub rt0_start: Location,
    pub payload: Location,
    pub initial_slot: Location,
    pub processor: Location,
    /// 生产 runtime 启动契约；`None` 走只含 rt0_start 的探针形态。
    pub runtime: Option<LinuxRuntime>,
}

/// 生产 runtime 启动契约：main 入口、runtime 状态区、协程入口、返回哨兵与元数据节。
/// 载荷地址字段登记为 RELATIVE，rt0 算完加载偏移后补写；长度字段写链接期值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinuxRuntime {
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

pub fn link_entry(request: ElfRequest, entry: LinuxEntry) -> Result<ElfImage, String> {
    link_entry_impl(request, entry, None)
}

pub fn link_entry_dynamic(
    request: ElfRequest,
    entry: LinuxEntry,
    imports: &DynamicLink,
) -> Result<ElfImage, String> {
    link_entry_impl(request, entry, Some(imports))
}

fn link_entry_impl(
    mut request: ElfRequest,
    entry: LinuxEntry,
    imports: Option<&DynamicLink>,
) -> Result<ElfImage, String> {
    validate_entry(&request, entry)?;
    register_initial_slot(&mut request, entry);
    if let Some(runtime) = entry.runtime {
        register_payload_pointers(&mut request, entry.payload, runtime);
    }
    let main = linux_main(PARAMS, RT0);
    let boot = rt0(RT0_START);
    let params = append(
        &mut request,
        "rt0_params",
        Segment::DataConst,
        8,
        SectionContent::Data(vec![0; rt0_params::BYTES as usize]),
    );
    let rt0_at = append(
        &mut request,
        "rt0",
        Segment::Text,
        4,
        SectionContent::Code(boot.bytes.clone()),
    );
    let start = append(
        &mut request,
        "_start",
        Segment::Text,
        4,
        SectionContent::Code(main.bytes.clone()),
    );
    request.entry = start;
    let tables = imports
        .map(|config| dynamic::prepare(&mut request, config))
        .transpose()?;
    let layout = prepared_layout(&request, tables.as_ref())?;
    if let Some(runtime) = entry.runtime {
        write_payload_lengths(&mut request, &layout, entry.payload, runtime)?;
    }
    let symbols = BTreeMap::from([
        (PARAMS, params),
        (RT0, rt0_at),
        (RT0_START, entry.rt0_start),
    ]);
    for (at, fragment) in [(start, main), (rt0_at, boot)] {
        relocate_fragment(&mut request, at, fragment, &layout, &symbols)?;
    }
    write_startup_data(&mut request, &layout, params, entry)?;
    link_prepared(&request, tables.as_ref())
}

fn register_initial_slot(request: &mut ElfRequest, entry: LinuxEntry) {
    request.relatives.push(Relative {
        at: Location {
            section: entry.payload.section,
            offset: entry.payload.offset + offset_of!(Rt0Payload, initial_slot) as u64,
        },
        target: entry.initial_slot,
        addend: 0,
    });
}

fn append(
    request: &mut ElfRequest,
    name: &str,
    segment: Segment,
    align: u64,
    content: SectionContent,
) -> Location {
    let section = request.sections.len();
    request.sections.push(Section {
        name: name.into(),
        segment,
        align,
        content,
    });
    Location { section, offset: 0 }
}

fn validate_entry(request: &ElfRequest, entry: LinuxEntry) -> Result<(), String> {
    for (name, at, size, align) in [
        ("rt0_start", entry.rt0_start, 4, 4),
        ("payload", entry.payload, Rt0Payload::bytes(), 8),
        ("initial slot", entry.initial_slot, INITIAL_SLOT_BYTES, 8),
        ("processor", entry.processor, PROCESSOR_PAGE_BYTES as u64, 8),
    ] {
        let section = section_at(request, at, size)?;
        let valid = if name == "rt0_start" {
            matches!(section.content, SectionContent::Code(_))
        } else {
            section.segment == Segment::Data
                && (name != "payload" || matches!(section.content, SectionContent::Data(_)))
        };
        if !valid || section.align < align || at.offset % align != 0 {
            return Err(format!("invalid A64 Linux {name} location {at:?}"));
        }
    }
    if let Some(runtime) = entry.runtime {
        for (name, at) in [
            ("entry", runtime.entry),
            ("coroutine_enter", runtime.coroutine_enter),
            ("main_return", runtime.main_return),
        ] {
            let valid = section_at(request, at, 4).is_ok_and(|section| {
                matches!(section.content, SectionContent::Code(_)) && at.offset % 4 == 0
            });
            if !valid {
                return Err(format!("invalid A64 Linux {name} location {at:?}"));
            }
        }
        if runtime.fragment_base.offset != 0
            || !matches!(
                request
                    .sections
                    .get(runtime.fragment_base.section)
                    .map(|section| &section.content),
                Some(SectionContent::Code(_))
            )
        {
            return Err(format!(
                "invalid A64 Linux fragment_base location {:?}",
                runtime.fragment_base
            ));
        }
        let valid = section_at(request, runtime.runtime_state, state::BYTES)
            .is_ok_and(|section| section.segment == Segment::Data);
        if !valid {
            return Err(format!(
                "invalid A64 Linux runtime_state location {:?}",
                runtime.runtime_state
            ));
        }
        section_at(request, runtime.stackmap, 0)?;
        section_at(request, runtime.unwind, 0)?;
        for (name, at) in [("gc_types", runtime.gc_types), ("gc_meta", runtime.gc_meta)] {
            if let Some(at) = at {
                section_at(request, at, 0)
                    .map_err(|_| format!("invalid A64 Linux {name} location {at:?}"))?;
            }
        }
    }
    Ok(())
}

fn register_payload_pointers(request: &mut ElfRequest, payload: Location, runtime: LinuxRuntime) {
    for (field, target) in [
        (offset_of!(Rt0Payload, main_entry), Some(runtime.entry)),
        (
            offset_of!(Rt0Payload, runtime_state),
            Some(runtime.runtime_state),
        ),
        (
            offset_of!(Rt0Payload, main_return),
            Some(runtime.main_return),
        ),
        (offset_of!(Rt0Payload, stackmap), Some(runtime.stackmap)),
        (
            offset_of!(Rt0Payload, fragment_base),
            Some(runtime.fragment_base),
        ),
        (offset_of!(Rt0Payload, unwind), Some(runtime.unwind)),
        (offset_of!(Rt0Payload, gc_types), runtime.gc_types),
        (offset_of!(Rt0Payload, gc_meta), runtime.gc_meta),
    ] {
        if let Some(target) = target {
            request.relatives.push(Relative {
                at: Location {
                    section: payload.section,
                    offset: payload.offset + field as u64,
                },
                target,
                addend: 0,
            });
        }
    }
}

fn write_payload_lengths(
    request: &mut ElfRequest,
    layout: &Layout,
    payload: Location,
    runtime: LinuxRuntime,
) -> Result<(), String> {
    let SectionContent::Data(bytes) = &mut request.sections[payload.section].content else {
        unreachable!()
    };
    let len = |at: Option<Location>| -> Result<u64, String> {
        match at {
            None => Ok(0),
            Some(at) => layout.sections[at.section]
                .size
                .checked_sub(at.offset)
                .ok_or_else(|| format!("invalid A64 Linux metadata location {at:?}")),
        }
    };
    for (field, value) in [
        (
            offset_of!(Rt0Payload, stackmap_len),
            len(Some(runtime.stackmap))?,
        ),
        (
            offset_of!(Rt0Payload, unwind_len),
            len(Some(runtime.unwind))?,
        ),
        (offset_of!(Rt0Payload, gc_types_len), len(runtime.gc_types)?),
        (offset_of!(Rt0Payload, gc_meta_len), len(runtime.gc_meta)?),
    ] {
        super::put(bytes, payload.offset as usize + field, value);
    }
    Ok(())
}

fn write_startup_data(
    request: &mut ElfRequest,
    layout: &Layout,
    params: Location,
    entry: LinuxEntry,
) -> Result<(), String> {
    let slot = layout.address(entry.initial_slot)?;
    let SectionContent::Data(bytes) = &mut request.sections[params.section].content else {
        unreachable!()
    };
    let mut words = vec![
        (rt0_params::BIAS_BASELINE, layout.address(params)?),
        (rt0_params::RELOC_TABLE, layout.rela),
        (rt0_params::RELOC_COUNT, request.relatives.len() as u64),
        (rt0_params::BOOT_R14, slot),
        (rt0_params::PROCESSOR, layout.address(entry.processor)?),
        (rt0_params::RT0_START, layout.address(entry.rt0_start)?),
        (rt0_params::PAYLOAD_VA, layout.address(entry.payload)?),
        (rt0_params::PAYLOAD_LEN, Rt0Payload::bytes()),
        (rt0_params::INITIAL_SLOT, slot),
    ];
    if let Some(runtime) = entry.runtime {
        words.extend([
            (rt0_params::ENTRY, layout.address(runtime.entry)?),
            (
                rt0_params::RUNTIME_STATE,
                layout.address(runtime.runtime_state)?,
            ),
            (
                rt0_params::COROUTINE_ENTER,
                layout.address(runtime.coroutine_enter)?,
            ),
        ]);
    }
    for (word, value) in words {
        super::put(bytes, word * 8, value);
    }
    let SectionContent::Data(bytes) = &mut request.sections[entry.payload.section].content else {
        unreachable!()
    };
    super::put(
        bytes,
        entry.payload.offset as usize + offset_of!(Rt0Payload, revision),
        Rt0Payload::REVISION,
    );
    Ok(())
}
