//! Whole-program assembly tests: minimal bodies plus a modeled runtime catalog.
use super::*;
use crate::lir::body::*;
use crate::runtime::entry::{Rt0Payload, rt0_params};
use std::collections::BTreeMap;
use std::mem::offset_of;

fn u64_at(image: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(image[at..at + 8].try_into().unwrap())
}

/// (p_type, p_offset, p_filesz) per ELF program header; file offsets equal VAs.
fn phdrs(image: &[u8]) -> Vec<(u32, u64, u64)> {
    let count = u16::from_le_bytes(image[56..58].try_into().unwrap()) as usize;
    (0..count)
        .map(|index| {
            let at = 64 + index * 56;
            (
                u32::from_le_bytes(image[at..at + 4].try_into().unwrap()),
                u64_at(image, at + 8),
                u64_at(image, at + 32),
            )
        })
        .collect()
}

fn dynamic_tags(image: &[u8], offset: usize, bytes: usize) -> BTreeMap<u64, u64> {
    let mut tags = BTreeMap::new();
    for at in (offset..offset + bytes).step_by(16) {
        let tag = i64::from_le_bytes(image[at..at + 8].try_into().unwrap());
        if tag == 0 {
            break;
        }
        tags.insert(tag as u64, u64_at(image, at + 8));
    }
    tags
}

/// rt0 参数块以字 0（BIAS_BASELINE）自指、字 `PAYLOAD_LEN` 记载荷长度——据此定位。
fn linux_params(image: &[u8]) -> usize {
    (0..image.len() - rt0_params::WORDS * 8)
        .step_by(8)
        .find(|&at| {
            u64_at(image, at) == at as u64
                && u64_at(image, at + rt0_params::PAYLOAD_LEN * 8) == size_of::<Rt0Payload>() as u64
        })
        .expect("rt0 params block")
}

/// (r_offset, r_addend) of each RELATIVE record in `.rela.dyn`.
fn linux_relatives(image: &[u8], tags: &BTreeMap<u64, u64>) -> Vec<(u64, u64)> {
    let (rela, size, entry) = (tags[&7] as usize, tags[&8] as usize, tags[&9] as usize);
    assert_eq!(entry, 24);
    (0..size / entry)
        .map(|index| {
            let at = rela + index * entry;
            (
                u64_at(image, at),
                u64_at(image, at + 8),
                u64_at(image, at + 16),
            )
        })
        .filter(|record| record.1 & 0xffff_ffff == 1027)
        .map(|record| (record.0, record.2))
        .collect()
}

/// Mach-O 节表：name → (vmaddr, fileoff, size)。
fn macho_sections(image: &[u8]) -> BTreeMap<String, (u64, u64, u64)> {
    let commands = u32::from_le_bytes(image[16..20].try_into().unwrap()) as usize;
    let mut sections = BTreeMap::new();
    let mut at = 32;
    for _ in 0..commands {
        let (cmd, size) = (
            u32::from_le_bytes(image[at..at + 4].try_into().unwrap()),
            u32::from_le_bytes(image[at + 4..at + 8].try_into().unwrap()) as usize,
        );
        if cmd == 0x19 {
            let count = u32::from_le_bytes(image[at + 64..at + 68].try_into().unwrap()) as usize;
            for section in 0..count {
                let base = at + 72 + section * 80;
                let name = String::from_utf8_lossy(&image[base..base + 16])
                    .trim_end_matches('\0')
                    .to_owned();
                sections.insert(
                    name,
                    (
                        u64_at(image, base + 32),
                        u32::from_le_bytes(image[base + 48..base + 52].try_into().unwrap()) as u64,
                        u64_at(image, base + 40),
                    ),
                );
            }
        }
        at += size;
    }
    sections
}

/// 载荷字段（按 [`Rt0Payload`] 字段偏移读取）。
fn payload_field(image: &[u8], payload: usize, field: usize) -> u64 {
    u64_at(image, payload + field)
}

fn source() -> crate::gir::body::SourceInfo {
    crate::gir::body::SourceInfo {
        location: crate::hir::Location {
            source: 0,
            start: 0,
            end: 0,
            expansion: u32::MAX,
        },
        scope: crate::gir::body::ScopeId(0),
    }
}

fn body(instance: [u8; 32], target: &str) -> Body {
    let memory = ValueId(0);
    Body {
        revision: REVISION,
        instance,
        owner: [0; 32],
        name: "a64_program_fixture".into(),
        target: target.into(),
        signature: Signature {
            parameters: vec![],
            results: vec![],
            sret: None,
            by_value: vec![],
        },
        values: vec![Value {
            kind: ValueType::scalar(Type::Mem),
            definition: Definition::Parameter {
                block: BlockId(0),
                index: 0,
            },
            origin: Origin::None,
            uses: 0..0,
        }],
        blocks: vec![crate::lir::body::Block {
            parameters: 0..1,
            instructions: 0..0,
            predecessors: 0..0,
            terminator: Terminator::Return {
                values: 0..0,
                memory,
            },
            source: source(),
            cleanup: false,
        }],
        instructions: vec![],
        operands: vec![],
        parameters: vec![Parameter {
            value: memory,
            source: None,
        }],
        edges: vec![],
        predecessors: vec![],
        switch_cases: vec![],
        stack_slots: vec![],
        lifetimes: vec![],
        root_inits: vec![],
        safepoints: vec![],
        barrier_permits: vec![],
        no_safepoint_regions: vec![],
        source_scopes: vec![],
        uses: vec![],
        data: vec![],
        assembly: vec![],
        environments: vec![],
        entry: BlockId(0),
        poll_summary: Default::default(),
        poll_interface: PollInterface {
            leaf: true,
            summary: Default::default(),
        },
    }
}

const ENTRY: [u8; 32] = [9; 32];

fn fixture(target: &str) -> (&'static [Body], runtime::Catalog<'static>) {
    let mut bodies = vec![body(ENTRY, target)];
    let mut entries = vec![];
    for (index, glue) in [
        "rt0_start",
        "callback_poll",
        "gc_dispatch",
        "foreign_dispatch",
        "foreign_dirty_dispatch",
        "panic_unwind",
    ]
    .into_iter()
    .enumerate()
    {
        let key = [index as u8 + 1; 32];
        bodies.push(body(key, target));
        entries.push((glue, key));
    }
    let bodies: &'static [Body] = Box::leak(bodies.into_boxed_slice());
    (bodies, runtime::Catalog::model(bodies, &entries))
}

#[test]
fn darwin_program_image_links_the_full_startup_contract() {
    let (bodies, catalog) = fixture("aarch64-macos");
    let program = program::build(&program::ProgramRequest {
        bodies,
        entry: ENTRY,
        platform: foreign::Platform::Darwin,
        catalog: &catalog,
        gc_types: &[0xAA; 16],
        gc_meta: &[0xBB; 16],
        interpreter: "",
        cage_bytes: 0,
        identifier: "test.a64",
        universe: None,
        imports: &[],
    })
    .unwrap();
    let image = &program.image;
    assert_eq!(&image[..4], &[0xcf, 0xfa, 0xed, 0xfe]);

    let sections = macho_sections(image);
    let world = &program.world;
    let data = sections["__data"];
    let payload = (data.1 + world.symbols[program::PAYLOAD].offset) as usize;
    assert_eq!(u64_at(image, payload), Rt0Payload::REVISION);
    for (field, section) in [
        (offset_of!(Rt0Payload, gc_types), "__gugutyp"),
        (offset_of!(Rt0Payload, gc_meta), "__ggmeta"),
    ] {
        let (addr, _offset, size) = sections[section];
        assert_eq!(size, 16);
        assert_eq!(
            payload_field(image, payload, field),
            addr - crate::macho::IMAGE_BASE,
            "{section} RVA"
        );
    }
    assert_eq!(
        payload_field(image, payload, offset_of!(Rt0Payload, gc_types_len)),
        16
    );
    assert_eq!(
        payload_field(image, payload, offset_of!(Rt0Payload, gc_meta_len)),
        16
    );
    assert_eq!(
        &image[sections["__gugutyp"].1 as usize..][..16],
        &[0xAA; 16]
    );
    assert_eq!(&image[sections["__ggmeta"].1 as usize..][..16], &[0xBB; 16]);
    let maps = sections["__gugu_maps"];
    assert_eq!(
        payload_field(image, payload, offset_of!(Rt0Payload, stackmap)),
        maps.0 - crate::macho::IMAGE_BASE
    );
    assert_eq!(
        payload_field(image, payload, offset_of!(Rt0Payload, stackmap_len)),
        maps.2
    );
}

#[test]
fn linux_program_image_links_the_full_startup_contract() {
    let (bodies, catalog) = fixture("aarch64-linux");
    let program = program::build(&program::ProgramRequest {
        bodies,
        entry: ENTRY,
        platform: foreign::Platform::Linux,
        catalog: &catalog,
        gc_types: &[0xAA; 16],
        gc_meta: &[0xBB; 16],
        interpreter: "/lib/ld-linux-aarch64.so.1",
        cage_bytes: 0,
        identifier: "",
        universe: None,
        imports: &[],
    })
    .unwrap();
    let image = &program.image;
    assert_eq!(&image[..4], b"\x7fELF");

    let headers = phdrs(image);
    let interp = headers
        .iter()
        .find(|header| header.0 == 3)
        .expect("PT_INTERP");
    assert_eq!(
        &image[interp.1 as usize..(interp.1 + interp.2) as usize],
        b"/lib/ld-linux-aarch64.so.1\0"
    );
    let dynamic = headers
        .iter()
        .find(|header| header.0 == 2)
        .expect("PT_DYNAMIC");
    let tags = dynamic_tags(image, dynamic.1 as usize, dynamic.2 as usize);
    let relatives = linux_relatives(image, &tags);
    // 8 个载荷指针字段 + initial_slot。
    assert_eq!(relatives.len(), 9);
    for (offset, addend) in &relatives {
        assert_eq!(u64_at(image, *offset as usize), *addend);
    }
    assert!(tags[&8] > 9 * 24, "GLOB_DAT records follow the RELATIVEs");

    let params = linux_params(image);
    let word = |index: usize| u64_at(image, params + index * 8);
    assert_eq!(word(rt0_params::RELOC_COUNT), 9);
    assert_eq!(
        word(rt0_params::PAYLOAD_LEN),
        size_of::<Rt0Payload>() as u64
    );
    for index in [
        rt0_params::ENTRY,
        rt0_params::RT0_START,
        rt0_params::RUNTIME_STATE,
        rt0_params::COROUTINE_ENTER,
    ] {
        assert_ne!(word(index), 0);
    }
    assert_eq!(word(rt0_params::BOOT_R14), word(rt0_params::INITIAL_SLOT));

    let world = &program.world;
    let payload = word(rt0_params::PAYLOAD_VA) as usize;
    assert_eq!(u64_at(image, payload), Rt0Payload::REVISION);
    for field in [
        offset_of!(Rt0Payload, main_entry),
        offset_of!(Rt0Payload, initial_slot),
        offset_of!(Rt0Payload, runtime_state),
        offset_of!(Rt0Payload, main_return),
        offset_of!(Rt0Payload, stackmap),
        offset_of!(Rt0Payload, fragment_base),
        offset_of!(Rt0Payload, unwind),
        offset_of!(Rt0Payload, gc_types),
        offset_of!(Rt0Payload, gc_meta),
    ] {
        assert!(
            relatives
                .iter()
                .any(|record| record.0 == (payload + field) as u64),
            "missing RELATIVE for payload field at +{field}"
        );
    }
    let gc_types = payload_field(image, payload, offset_of!(Rt0Payload, gc_types)) as usize;
    let gc_meta = payload_field(image, payload, offset_of!(Rt0Payload, gc_meta)) as usize;
    assert_eq!(&image[gc_types..gc_types + 16], &[0xAA; 16]);
    assert_eq!(&image[gc_meta..gc_meta + 16], &[0xBB; 16]);
    assert_eq!(
        payload_field(image, payload, offset_of!(Rt0Payload, gc_types_len)),
        16
    );
    assert_eq!(
        payload_field(image, payload, offset_of!(Rt0Payload, gc_meta_len)),
        16
    );
    let stackmap = &world.sections[world.maps.section];
    assert_eq!(
        payload_field(image, payload, offset_of!(Rt0Payload, stackmap_len)),
        stackmap.content.size() - world.maps.offset
    );
}
