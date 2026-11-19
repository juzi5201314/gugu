use std::collections::BTreeMap;
use std::mem::offset_of;

use super::{PAGE_SHIFT, STUB_SIZE, append_import_stubs, encode_stub, import_stub_symbol};
use crate::{
    NativeImport, NativeImportKind,
    a64::{
        decode, encode,
        fragments::{Fragment, darwin_main, rt0},
        inst::{Operand, RelocKind, Target},
        table,
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
}

/// 添加 arm64 Darwin 启动节，绑定 libSystem 的 exit 并写出签名镜像。
/// 调用方提供独立的 rt0_start 代码和其余运行时数据；原有 entry 字段被替换。
pub fn link_entry(mut request: MachORequest, entry: DarwinEntry) -> Result<MachImage, String> {
    if request.arch != Arch::Arm64 {
        return Err("Darwin arm64 entry requires an arm64 Mach-O image".to_owned());
    }
    validate_entry(&request, entry)?;
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
    let slot = exit_slot(&mut request)?;
    let existing = request.symbols.iter().find(|symbol| symbol.name == exit);
    let (exit_at, layout) = if let Some(symbol) = existing {
        let at = symbol.at;
        let layout = macho::layout(&request)?;
        relocate_exit_stub(&mut request, &layout, at, slot.at)?;
        (at, layout)
    } else {
        let stubs = append_import_stubs(&mut request, &BTreeMap::from([("exit".into(), slot)]))?;
        (stubs[&exit], macho::layout(&request)?)
    };
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

fn exit_slot(request: &mut MachORequest) -> Result<ImportSlot, String> {
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
    let at = if let Some(at) = request.fixups.iter().find_map(|fixup| match *fixup {
        Fixup::Bind {
            at,
            import: index,
            addend: 0,
        } if index == import => Some(at),
        _ => None,
    }) {
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

fn relocate_exit_stub(
    request: &mut MachORequest,
    layout: &Layout,
    at: Location,
    slot: Location,
) -> Result<(), String> {
    let pc = layout.address(at)?;
    let SectionContent::Code(bytes) = &mut request.sections[at.section].content else {
        return Err("arm64 Mach-O exit stub is not in a code section".to_owned());
    };
    let offset = at.offset as usize;
    let stub = bytes
        .get_mut(offset..offset + STUB_SIZE)
        .filter(|_| pc % 4 == 0)
        .ok_or("invalid arm64 Mach-O exit stub location")?;
    let mut code = Vec::with_capacity(STUB_SIZE);
    encode_stub(&mut code, pc, layout.address(slot)?).map_err(|error| error.to_string())?;
    stub.copy_from_slice(&code);
    Ok(())
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
    for (word, value) in [
        (rt0_params::BIAS_BASELINE, rva(params)?),
        (rt0_params::BOOT_R14, slot),
        (rt0_params::PROCESSOR, rva(entry.processor)?),
        (rt0_params::RT0_START, rva(entry.rt0_start)?),
        (rt0_params::PAYLOAD_VA, rva(entry.payload)?),
        (rt0_params::PAYLOAD_LEN, Rt0Payload::bytes()),
        (rt0_params::INITIAL_SLOT, slot),
    ] {
        bytes[word * 8..word * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    let SectionContent::Data(bytes) = &mut request.sections[entry.payload.section].content else {
        unreachable!()
    };
    for (offset, value) in [
        (offset_of!(Rt0Payload, revision), Rt0Payload::REVISION),
        (offset_of!(Rt0Payload, initial_slot), slot),
    ] {
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
    let base = layout.address(at)?;
    let mut bytes = fragment.bytes;
    for site in fragment.relocations {
        let rel = site.relocation;
        let target = symbols
            .get(rel.target.as_str())
            .ok_or_else(|| format!("unresolved arm64 Mach-O startup symbol {}", rel.target))?;
        let target = layout
            .address(*target)?
            .checked_add_signed(rel.addend)
            .ok_or_else(|| format!("arm64 Mach-O startup target overflow: {}", rel.target))?;
        let offset = site.offset as usize;
        let field = &mut bytes[offset..offset + 4];
        let word = u32::from_le_bytes(field.try_into().unwrap());
        let patched =
            patch(word, rel.kind, base + u64::from(site.offset), target).map_err(|error| {
                format!(
                    "arm64 Mach-O startup relocation for {}: {error}",
                    rel.target
                )
            })?;
        field.copy_from_slice(&patched);
    }
    Ok(bytes)
}

fn patch(word: u32, kind: RelocKind, pc: u64, target: u64) -> Result<[u8; 4], crate::a64::Error> {
    let mut inst = decode::decode(word)?;
    let low = (target & ((1 << PAGE_SHIFT) - 1)) as u16;
    let (operand, value) = match (kind, table::get(inst.form)?.key) {
        (RelocKind::AdrpPage21, "adrp") => {
            let pages = (target >> PAGE_SHIFT) as i64 - (pc >> PAGE_SHIFT) as i64;
            (1, Operand::Target(Target::Offset(pages << PAGE_SHIFT)))
        }
        (RelocKind::AddLo12, "add.X.imm") => (
            2,
            Operand::AddImm {
                imm12: low,
                shift12: false,
            },
        ),
        (RelocKind::Call26, "bl") => {
            let delta = i64::try_from(i128::from(target) - i128::from(pc))
                .map_err(|_| crate::a64::error("call displacement overflow"))?;
            (0, Operand::Target(Target::Offset(delta)))
        }
        _ => return Err(crate::a64::error("unexpected startup relocation")),
    };
    inst.operands[operand] = value;
    Ok(encode::encode(&inst)?.bytes())
}
