use super::*;
#[path = "dynamic_tests.rs"]
mod dynamic_tests;
use crate::{
    a64::{
        decode, encode,
        inst::{Address, AddressMode, Operand, Target},
        machine,
        reg::Reg,
        table,
    },
    runtime::{
        entry::{INITIAL_SLOT_BYTES, Rt0Payload, rt0_params},
        processor::PROCESSOR_PAGE_BYTES,
    },
};

fn loc(section: usize, offset: u64) -> Location {
    Location { section, offset }
}
fn section(segment: Segment, content: SectionContent) -> Section {
    Section {
        name: "test".into(),
        segment,
        align: 8,
        content,
    }
}
fn request() -> ElfRequest {
    ElfRequest {
        sections: vec![
            section(
                Segment::Text,
                SectionContent::Code(0xd65f03c0_u32.to_le_bytes().to_vec()),
            ),
            section(Segment::DataConst, SectionContent::Data(vec![0; 32])),
            section(Segment::Data, SectionContent::Data(vec![0; 8])),
        ],
        entry: loc(0, 0),
        relatives: vec![],
        relocations: vec![],
    }
}
fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

#[test]
fn static_pie_headers_cover_all_supported_kernel_page_sizes() {
    let request = request();
    let image = link(&request).unwrap();
    let bytes = &image.bytes;
    assert_eq!(&bytes[..7], b"\x7fELF\x02\x01\x01");
    assert_eq!((u16_at(bytes, 16), u16_at(bytes, 18)), (3, 183));
    assert_eq!(u64_at(bytes, 24), HEADER_BYTES);
    assert_eq!(u64_at(bytes, 32), 64);
    assert_eq!((u16_at(bytes, 54), u16_at(bytes, 56)), (56, PHNUM));
    let kinds: Vec<_> = (0..5).map(|i| u32_at(bytes, 64 + i * 56)).collect();
    assert_eq!(kinds, [6, 1, 1, 1, 0x6474_e551]);
    assert_eq!(u64_at(bytes, 64 + 16), 64);
    assert_eq!(u64_at(bytes, 64 + 32), PHNUM as u64 * 56);
    for (index, flags) in [5, 4, 6].into_iter().enumerate() {
        let at = 64 + (index + 1) * 56;
        assert_eq!(u32_at(bytes, at + 4), flags);
        let (off, va) = (u64_at(bytes, at + 8), u64_at(bytes, at + 16));
        assert_eq!(u64_at(bytes, at + 48), SEGMENT_ALIGN);
        for page in [4096, 16384, 65536] {
            assert_eq!((off % page, va % page), (0, 0));
        }
        assert!(off + u64_at(bytes, at + 32) <= bytes.len() as u64);
        assert!(u64_at(bytes, at + 32) <= u64_at(bytes, at + 40));
    }
    assert_eq!(link(&request).unwrap(), image);
    assert_eq!(image.fingerprint, *blake3::hash(bytes).as_bytes());
}

#[test]
fn relative_records_are_rela_and_only_target_rw_slots() {
    let mut request = request();
    request.relatives.push(Relative {
        at: loc(2, 0),
        target: loc(1, 8),
        addend: 3,
    });
    let layout = layout(&request).unwrap();
    let image = link(&request).unwrap();
    let at = layout.rela as usize;
    let slot = layout.address(loc(2, 0)).unwrap();
    let target = layout.address(loc(1, 11)).unwrap();
    assert_eq!(u64_at(&image.bytes, at), slot);
    assert_eq!(u64_at(&image.bytes, at + 8), R_AARCH64_RELATIVE);
    assert_eq!(u64_at(&image.bytes, at + 16), target);
    assert_eq!(u64_at(&image.bytes, slot as usize), target);
    let sh = layout.shoff as usize + 64;
    assert_eq!(
        (u32_at(&image.bytes, sh + 4), u64_at(&image.bytes, sh + 56)),
        (4, 24)
    );
    assert_eq!(u64_at(&image.bytes, sh + 32), 24);
    for bias in [0, 0x10000, 0xaaaa_0000] {
        let runtime_slot = u64_at(&image.bytes, at) + bias;
        let runtime_value = u64_at(&image.bytes, at + 16) + bias;
        assert_eq!(runtime_slot, slot + bias);
        assert_eq!(runtime_value, target + bias);
    }
}

#[test]
fn no_relocations_and_bss_have_no_file_payload() {
    let mut request = request();
    request
        .sections
        .push(section(Segment::Data, SectionContent::ZeroFill(100)));
    let layout = layout(&request).unwrap();
    let bytes = link(&request).unwrap().bytes;
    assert_eq!(u64_at(&bytes, layout.shoff as usize + 64 + 32), 0);
    let (_, file, memory) = layout.segments[2];
    assert_eq!(memory - file, 100);
    assert_eq!(
        layout.address(loc(3, 100)).unwrap(),
        layout.segments[2].0 + memory
    );
    assert!(layout.address(loc(3, 101)).is_err());
}

fn instruction(key: &str, operands: Vec<Operand>) -> [u8; 4] {
    encode::encode(&machine::inst(key, operands).unwrap())
        .unwrap()
        .bytes()
}
fn reg(n: u8) -> Operand {
    Operand::Reg(Reg::x(n))
}
fn offset(value: i64) -> Operand {
    Operand::Target(Target::Offset(value))
}

fn instruction_request() -> ElfRequest {
    let mut request = request();
    let words = [
        instruction("bl", vec![offset(0)]),
        instruction("b", vec![offset(0)]),
        instruction("adrp", vec![reg(9), offset(0)]),
        instruction(
            "add.X.imm",
            vec![
                reg(9),
                reg(9),
                Operand::AddImm {
                    imm12: 0,
                    shift12: false,
                },
            ],
        ),
        instruction(
            "ldr.X.Unsigned",
            vec![
                reg(0),
                Operand::Address(Address {
                    base: Reg::x(9),
                    mode: AddressMode::Unsigned(0),
                }),
            ],
        ),
    ];
    request.sections[0].content = SectionContent::Code(words.concat());
    request.relocations = [
        RelocKind::Call26,
        RelocKind::Branch26,
        RelocKind::AdrpPage21,
        RelocKind::AddLo12,
        RelocKind::LdStLo12(3),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, kind)| Relocation {
        at: loc(0, i as u64 * 4),
        target: if i < 2 { loc(0, 0) } else { loc(1, 8) },
        addend: 0,
        kind,
    })
    .collect();
    request
}

#[test]
fn link_time_relocations_decode_to_resolved_operands() {
    let request = instruction_request();
    let layout = layout(&request).unwrap();
    let image = link(&request).unwrap();
    let start = layout.address(loc(0, 0)).unwrap() as usize;
    let decoded: Vec<_> = (0..5)
        .map(|i| decode::decode(u32_at(&image.bytes, start + i * 4)).unwrap())
        .collect();
    assert_eq!(decoded[0].operands, [offset(0)]);
    assert_eq!(decoded[1].operands, [offset(-4)]);
    let page_delta =
        ((layout.address(loc(1, 8)).unwrap() >> 12) as i64 - ((start + 8) >> 12) as i64) << 12;
    assert_eq!(decoded[2].operands, [reg(9), offset(page_delta)]);
    assert_eq!(
        decoded[3].operands[2],
        Operand::AddImm {
            imm12: 8,
            shift12: false
        }
    );
    assert_eq!(
        decoded[4].operands[1],
        Operand::Address(Address {
            base: Reg::x(9),
            mode: AddressMode::Unsigned(8)
        })
    );
}

#[test]
fn malformed_instruction_relocations_are_errors() {
    for (index, kind, addend) in [
        (0, RelocKind::Branch26, 0),
        (0, RelocKind::Call26, 1 << 28),
        (0, RelocKind::Call26, 1),
        (2, RelocKind::AdrpPage21, 1 << 33),
        (4, RelocKind::LdStLo12(2), 0),
        (4, RelocKind::LdStLo12(3), 1),
        (0, RelocKind::CondBr19, 0),
        (0, RelocKind::Call26, i64::MIN),
    ] {
        let mut request = instruction_request();
        request.relocations[index].kind = kind;
        request.relocations[index].addend = addend;
        assert!(link(&request).is_err(), "{index}: {kind:?}, {addend}");
    }
    let mut request = instruction_request();
    request.relocations.push(request.relocations[0]);
    assert!(link(&request).is_err());
    request.relocations.pop();
    request.relocations[0].at.offset = 1;
    assert!(link(&request).is_err());
    request.relocations[0].at.offset = 20;
    assert!(link(&request).is_err());
}

#[test]
fn invalid_sections_and_locations_are_errors() {
    let mut empty = request();
    empty.sections.clear();
    assert!(link(&empty).is_err());
    for alignment in [0, 3, SEGMENT_ALIGN * 2] {
        let mut request = request();
        request.sections[0].align = alignment;
        assert!(link(&request).is_err());
    }
    let mut bad = request();
    bad.sections[0].segment = Segment::Data;
    assert!(link(&bad).is_err());
    let mut bad = request();
    bad.sections[1].content = SectionContent::ZeroFill(8);
    assert!(link(&bad).is_err());
}

#[test]
fn invalid_pointer_fixups_and_layout_overflow_are_errors() {
    for (at, target) in [
        (loc(1, 0), loc(0, 0)),
        (loc(2, 1), loc(0, 0)),
        (loc(2, 0), loc(9, 0)),
        (loc(2, 0), loc(0, 5)),
    ] {
        let mut request = request();
        request.relatives.push(Relative {
            at,
            target,
            addend: 0,
        });
        assert!(link(&request).is_err());
    }
    let mut bad = request();
    let rel = Relative {
        at: loc(2, 0),
        target: loc(0, 0),
        addend: 0,
    };
    bad.relatives = vec![rel, rel];
    assert!(link(&bad).is_err());
    bad.relatives = vec![Relative {
        addend: i64::MIN,
        ..rel
    }];
    assert!(link(&bad).is_err());
    bad.relatives.clear();
    bad.sections
        .push(section(Segment::Data, SectionContent::ZeroFill(u64::MAX)));
    assert!(link(&bad).is_err());
    bad.sections[3].content = SectionContent::ZeroFill(8);
    bad.sections
        .push(section(Segment::Data, SectionContent::Data(vec![0; 8])));
    assert!(link(&bad).is_err());
}

fn runtime_request() -> (ElfRequest, LinuxEntry) {
    let mut request = request();
    request.sections.push(section(
        Segment::Data,
        SectionContent::Data(vec![0; Rt0Payload::bytes() as usize]),
    ));
    request.sections.push(section(
        Segment::Data,
        SectionContent::ZeroFill(INITIAL_SLOT_BYTES + PROCESSOR_PAGE_BYTES as u64),
    ));
    let entry = LinuxEntry {
        rt0_start: loc(0, 0),
        payload: loc(3, 0),
        initial_slot: loc(4, 0),
        processor: loc(4, INITIAL_SLOT_BYTES),
    };
    (request, entry)
}

#[test]
fn linux_entry_decodes_stack_capture_relocation_loop_and_exit_group() {
    let (request, entry) = runtime_request();
    let image = link_entry(request.clone(), entry).unwrap();
    assert_eq!(link_entry(request, entry).unwrap(), image);
    let start = u64_at(&image.bytes, 24) as usize;
    let decoded: Vec<_> = (0..24)
        .map(|i| decode::decode(u32_at(&image.bytes, start + i * 4)).unwrap())
        .collect();
    let keys: Vec<_> = decoded
        .iter()
        .map(|i| table::get(i.form).unwrap().key)
        .collect();
    assert_eq!(
        &keys[..4],
        ["ldr.X.Unsigned", "add.X.imm", "add.X.shift", "add.X.imm"]
    );
    assert_eq!(decoded[0].operands[0], reg(3));
    assert_eq!(decoded[2].operands[2], reg(3));
    assert_eq!(
        decoded[2].operands[3],
        Operand::Shift(crate::a64::inst::Shift::Lsl, 3)
    );
    assert_relocation_loop_and_exit(&decoded, &keys);
}

fn assert_relocation_loop_and_exit(decoded: &[crate::a64::inst::Inst], keys: &[&str]) {
    assert_eq!(
        &keys[11..21],
        [
            "cbz.X",
            "ldr.X.Unsigned",
            "ldr.X.Unsigned",
            "add.X.shift",
            "add.X.shift",
            "str.X.Unsigned",
            "add.X.imm",
            "sub.X.imm",
            "cbnz.X",
            "bl"
        ]
    );
    assert_eq!(&keys[21..24], ["movz.X", "svc", "brk"]);
    assert_eq!(
        decoded[21].operands,
        [
            reg(8),
            Operand::Wide {
                imm16: 94,
                shift: 0
            }
        ]
    );
    assert_eq!(decoded[22].operands, [Operand::Imm(0)]);
    assert_eq!(decoded[11].operands[1], offset(36));
    assert_eq!(decoded[19].operands[1], offset(-28));
}

#[test]
fn startup_data_contains_rvas_and_relocated_payload_slot() {
    let (request, entry) = runtime_request();
    let old = layout(&request).unwrap();
    let image = link_entry(request, entry).unwrap();
    let sh = u64_at(&image.bytes, 40) as usize;
    let rela = u64_at(&image.bytes, sh + 64 + 24) as usize;
    assert_eq!(u64_at(&image.bytes, sh + 64 + 32), 24);
    let payload_slot =
        old.address(entry.payload).unwrap() + std::mem::offset_of!(Rt0Payload, initial_slot) as u64;
    assert_eq!(u64_at(&image.bytes, rela), payload_slot);
    assert_eq!(u64_at(&image.bytes, rela + 8), R_AARCH64_RELATIVE);
    assert_eq!(
        u64_at(&image.bytes, rela + 16),
        old.address(entry.initial_slot).unwrap()
    );
    let params = old.sections[1].addr as usize + 32;
    assert_eq!(u64_at(&image.bytes, params), params as u64);
    assert_eq!(
        u64_at(&image.bytes, params + rt0_params::RELOC_TABLE * 8),
        rela as u64
    );
    assert_eq!(
        u64_at(&image.bytes, params + rt0_params::RELOC_COUNT * 8),
        1
    );
    assert_eq!(
        u64_at(&image.bytes, params + rt0_params::PAYLOAD_VA * 8),
        old.address(entry.payload).unwrap()
    );
    assert_eq!(
        u64_at(&image.bytes, params + rt0_params::PROCESSOR * 8),
        old.address(entry.processor).unwrap()
    );
}

#[test]
fn invalid_runtime_storage_is_rejected() {
    let (request, entry) = runtime_request();
    for bad in [
        LinuxEntry {
            rt0_start: loc(1, 0),
            ..entry
        },
        LinuxEntry {
            payload: loc(2, 0),
            ..entry
        },
        LinuxEntry {
            initial_slot: loc(4, 1),
            ..entry
        },
        LinuxEntry {
            processor: loc(4, INITIAL_SLOT_BYTES + 8),
            ..entry
        },
    ] {
        assert!(link_entry(request.clone(), bad).is_err());
    }
    let mut request = request;
    request.sections[3].content = SectionContent::ZeroFill(Rt0Payload::bytes());
    assert!(link_entry(request, entry).is_err());
}
