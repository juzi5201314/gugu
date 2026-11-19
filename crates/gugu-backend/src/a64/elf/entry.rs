use std::{collections::BTreeMap, mem::offset_of};

use super::{
    DynamicLink, ElfImage, ElfRequest, Layout, Location, Relative, Section, SectionContent,
    Segment, dynamic, link_prepared, prepared_layout, relocate_fragment, section_at,
};
use crate::{
    a64::fragments::{linux_main, rt0},
    runtime::{
        entry::{INITIAL_SLOT_BYTES, Rt0Payload, rt0_params},
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
    register_initial_slot(&mut request, entry);
    let tables = imports
        .map(|config| dynamic::prepare(&mut request, config))
        .transpose()?;
    let layout = prepared_layout(&request, tables.as_ref())?;
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
    for (word, value) in [
        (rt0_params::BIAS_BASELINE, layout.address(params)?),
        (rt0_params::RELOC_TABLE, layout.rela),
        (rt0_params::RELOC_COUNT, request.relatives.len() as u64),
        (rt0_params::BOOT_R14, slot),
        (rt0_params::PROCESSOR, layout.address(entry.processor)?),
        (rt0_params::RT0_START, layout.address(entry.rt0_start)?),
        (rt0_params::PAYLOAD_VA, layout.address(entry.payload)?),
        (rt0_params::PAYLOAD_LEN, Rt0Payload::bytes()),
        (rt0_params::INITIAL_SLOT, slot),
    ] {
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
