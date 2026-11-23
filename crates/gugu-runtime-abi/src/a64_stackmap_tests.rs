use crate::{
    cage::CompressionPlane,
    compression_schema::{CompressionDemand, CompressionPolicyV1, CompressionRuntimeContract},
    gc_metadata_contract::GC_ARENA_BYTES,
    platform::{FakePlatform, PlatformProfile},
    stackmap::{self, HandleSlot, ScannedRoot},
    stackmap_codec::{self, CodeLayout, SafepointLayout, TypedRoot},
    stackmap_schema::StackMapArchitecture as Arch,
    target::PointerCompression,
};

fn function() -> CodeLayout {
    CodeLayout {
        code_rva: 0x1000,
        code_size: 64,
        frame_size: 64,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: false,
        has_stack_interior: true,
    }
}
fn point(kind: u8, slot_count: u32) -> (u32, SafepointLayout<u32>) {
    (
        0,
        SafepointLayout {
            pc_offset: 8,
            kind,
            dirty: false,
            copy_allowed: true,
            scan_allowed: true,
            slots: std::array::from_fn(|_| vec![0]),
            registers: [0; 5],
            typed: vec![],
            function: 0,
            slot_count,
        },
    )
}
fn encode(point: (u32, SafepointLayout<u32>)) -> Vec<u8> {
    stackmap_codec::encode_for(Arch::Aarch64, &[function()], &[point]).unwrap()
}
fn plane() -> CompressionPlane {
    let contract = CompressionRuntimeContract::build(
        CompressionDemand {
            decode_sites: 1,
            compressed_root_slots: 1,
        },
        CompressionPolicyV1::cage(GC_ARENA_BYTES * 4),
        PointerCompression::x86_64(),
    )
    .unwrap();
    let mut plane = CompressionPlane::new(&contract);
    plane
        .reserve(&mut FakePlatform::new(
            PlatformProfile::Linux,
            u64::from(u32::MAX),
        ))
        .unwrap();
    plane
}
fn map_offset(bytes: &[u8]) -> usize {
    u64::from_le_bytes(bytes[56..64].try_into().unwrap()) as usize
}

#[test]
fn architecture_tags_empty_world_and_legacy_v4_byte_anchor() {
    let legacy = stackmap_codec::encode(&[], &[]).unwrap();
    let mut expected = vec![0; 80];
    expected[..12].copy_from_slice(b"GUGUSM01\x04\x00\x08\x01");
    for offset in [32, 40, 48] {
        expected[offset] = 72;
    }
    for offset in [56, 64, 72] {
        expected[offset] = 80;
    }
    assert_eq!(legacy, expected);
    assert_eq!(
        stackmap_codec::decode_tables(&legacy).unwrap().architecture,
        Arch::X86_64
    );
    let empty = stackmap_codec::encode_for::<u32>(Arch::Aarch64, &[], &[]).unwrap();
    assert_eq!(&empty[8..12], &[5, 0, 8, 1]);
    assert_eq!(u32::from_le_bytes(empty[24..28].try_into().unwrap()), 183);
    assert_eq!(stackmap_codec::decode(&empty).unwrap(), (0, 0, 0));
    assert_eq!(
        stackmap_codec::decode_tables(&empty).unwrap().architecture,
        Arch::Aarch64
    );
    assert_eq!(Arch::Aarch64.register_bit(30), Some(1 << 30));
    assert_eq!(Arch::Aarch64.register_bit(31), None);
    assert_eq!(Arch::Aarch64.register_bit(255), None);
}

#[test]
fn a64_entry_scratch_uses_physical_x_register_numbers_and_exact_return_pc() {
    let mut entry = point(4, 0);
    entry.1.registers = [1, 2, 0, 0, 1 << 8];
    let bytes = encode(entry);
    let decoded = stackmap_codec::decode_tables(&bytes).unwrap();
    assert_eq!(decoded.maps[0].registers, [1, 2, 0, 0, 256]);
    let mut registers = [0; 9];
    registers[0] = 0x1000;
    registers[1] = 0x2003;
    registers[8] = 0x9000;
    let roots = stackmap::scan_frame(&decoded, 0x1008, &[], &registers, &[], &mut plane()).unwrap();
    assert!(roots.slots.is_empty());
    assert_eq!(
        roots.registers,
        [
            ScannedRoot::Direct(0),
            ScannedRoot::Interior {
                offset: 1,
                delta: 0x2003
            },
            ScannedRoot::Stack(8)
        ]
    );
    assert!(stackmap::scan_frame(&decoded, 0x1004, &[], &registers, &[], &mut plane()).is_err());
    assert!(stackmap::scan_frame(&decoded, 0x1009, &[], &registers, &[], &mut plane()).is_err());
    assert!(stackmap::scan_frame(&decoded, 0x1008, &[], &[], &[], &mut plane()).is_err());
}

#[test]
fn a64_frame_vm_scans_five_classes_and_initialization_gates() {
    let mut layout = point(0, 8);
    for (class, bitmap) in layout.1.slots.iter_mut().enumerate() {
        bitmap[0] = 1 << class;
    }
    layout.1.typed.push(TypedRoot {
        word: 5,
        root_kind: 0,
        init_byte: 56,
    });
    let decoded = stackmap_codec::decode_tables(&encode(layout)).unwrap();
    let mut plane = plane();
    let target = plane.descriptor(0).unwrap().base + 8;
    let compressed = plane.encode(target).unwrap();
    let mut words = [0x100, 0x2007, 1, compressed, 0x8888, 0x300, 0, 1];
    let handles = [
        HandleSlot {
            payload: 0,
            generation: 0,
        },
        HandleSlot {
            payload: 0x9000,
            generation: 1,
        },
    ];
    let roots = stackmap::scan_frame(&decoded, 0x1008, &words, &[], &handles, &mut plane).unwrap();
    assert_eq!(
        roots.slots,
        [
            ScannedRoot::Direct(0),
            ScannedRoot::Direct(5),
            ScannedRoot::Interior {
                offset: 1,
                delta: 0x2007
            },
            ScannedRoot::Handle {
                offset: 2,
                payload: 0x9000
            },
            ScannedRoot::Compressed { offset: 3, target },
            ScannedRoot::Stack(4)
        ]
    );
    assert!(roots.registers.is_empty());
    words[7] = 0;
    assert!(
        !stackmap::scan_frame(&decoded, 0x1008, &words, &[], &handles, &mut plane)
            .unwrap()
            .slots
            .contains(&ScannedRoot::Direct(5))
    );
    assert!(
        stackmap::scan_frame(&decoded, 0x1008, &words[..7], &[], &handles, &mut plane).is_err()
    );
}

#[test]
fn a64_rejects_reserved_registers_and_unspilled_call_poll_suspend_roots() {
    for number in [16, 17, 18, 27, 28, 29, 30, 31] {
        let mut entry = point(4, 0);
        entry.1.registers[0] = 1 << number;
        assert!(stackmap_codec::encode_for(Arch::Aarch64, &[function()], &[entry]).is_err());
    }
    for kind in 0..4 {
        let mut layout = point(kind, 8);
        layout.1.registers[0] = 1;
        assert!(stackmap_codec::encode_for(Arch::Aarch64, &[function()], &[layout]).is_err());
    }
    let mut entry = point(4, 0);
    entry.1.registers[0] = 1 << 26;
    assert!(stackmap_codec::encode_for(Arch::Aarch64, &[function()], &[entry]).is_err());
    let mut entry = point(4, 0);
    entry.1.registers = [1, 1, 0, 0, 0];
    assert!(stackmap_codec::encode_for(Arch::Aarch64, &[function()], &[entry]).is_err());
}

#[test]
fn a64_decoder_rejects_bad_tags_bounds_alignment_and_spill_policy() {
    let bytes = encode(point(0, 8));
    for (offset, value) in [(8, 4), (24, 62), (28, 1)] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        assert!(stackmap_codec::decode(&bad).is_err());
    }
    for length in [0, 7, 8, 71, bytes.len() - 1] {
        assert!(stackmap_codec::decode(&bytes[..length]).is_err());
    }
    for offset in [72, 80, 84, 104] {
        let mut bad = bytes.clone();
        bad[offset] += 1;
        assert!(stackmap_codec::decode(&bad).is_err());
    }
    for register in [0, 16, 31] {
        let mut bad = bytes.clone();
        let start = map_offset(&bad) + 4;
        bad[start..start + 4].copy_from_slice(&(1_u32 << register).to_le_bytes());
        assert!(stackmap_codec::decode(&bad).is_err());
    }
    let mut out_of_frame = bytes.clone();
    let start = map_offset(&out_of_frame);
    out_of_frame[start..start + 4].copy_from_slice(&9_u32.to_le_bytes());
    assert!(stackmap_codec::decode(&out_of_frame).is_err());
}

#[test]
fn a64_root_bounds_and_layout_deduplication() {
    for root in [
        TypedRoot {
            word: 8,
            root_kind: 0,
            init_byte: 63,
        },
        TypedRoot {
            word: 1,
            root_kind: 0,
            init_byte: 64,
        },
    ] {
        let mut layout = point(0, 8);
        layout.1.typed.push(root);
        assert!(stackmap_codec::encode_for(Arch::Aarch64, &[function()], &[layout]).is_err());
    }
    let mut second = point(1, 8);
    second.1.pc_offset = 12;
    let bytes =
        stackmap_codec::encode_for(Arch::Aarch64, &[function()], &[second, point(0, 8)]).unwrap();
    assert_eq!(stackmap_codec::decode(&bytes).unwrap(), (1, 2, 1));
    let mut bad = function();
    bad.frame_size = 24;
    assert!(stackmap_codec::encode_for(Arch::Aarch64, &[bad], &[point(0, 0)]).is_err());
}

#[test]
fn serialized_a64_cfa_recovers_entry_and_framed_callers() {
    use stackmap_codec::a64_unwind::{self, Caller, Site};
    let code = function();
    let sites = [
        Site {
            pc_offset: 8,
            entry: true,
        },
        Site {
            pc_offset: 12,
            entry: false,
        },
    ];
    let bytes = a64_unwind::encode(code, 16, &sites, &[]).unwrap();
    let table = a64_unwind::decode(&bytes).unwrap();
    let mut call = point(0, 8);
    call.1.pc_offset = 12;
    let maps = stackmap_codec::decode_tables(
        &stackmap_codec::encode_for(Arch::Aarch64, &[code], &[point(4, 0), call]).unwrap(),
    )
    .unwrap();
    table.verify_joint(&maps).unwrap();
    let mut registers = [0; 31];
    registers[10] = 0x2000;
    assert_eq!(
        table.caller(0x1008, 0x8000, &[], &registers).unwrap(),
        Caller {
            sp: 0x8000,
            pc: 0x2000
        }
    );
    let mut frame = [0; 8];
    frame[3] = 0x3000;
    assert_eq!(
        table.caller(0x100c, 0x7fc0, &frame, &[]).unwrap(),
        Caller {
            sp: 0x8000,
            pc: 0x3000
        }
    );
    assert!(table.caller(0x100d, 0x8000, &frame, &registers).is_err());
    assert!(table.caller(0x1008, 0x8000, &frame, &[]).is_err());
    assert!(table.caller(0x100c, 0x8000, &[], &registers).is_err());
    assert!(table.caller(0x100c, u64::MAX, &frame, &registers).is_err());
    let mut wrong = maps.clone();
    wrong.architecture = Arch::X86_64;
    assert!(table.verify_joint(&wrong).is_err());
}

#[test]
fn a64_cfa_decoder_rejects_corruption_and_accepts_empty_sites() {
    use stackmap_codec::a64_unwind::{self, Site};
    let code = function();
    let bytes = a64_unwind::encode(
        code,
        16,
        &[
            Site {
                pc_offset: 8,
                entry: true,
            },
            Site {
                pc_offset: 12,
                entry: false,
            },
        ], &[])
    .unwrap();
    for length in [0, 7, 39, bytes.len() - 1] {
        assert!(a64_unwind::decode(&bytes[..length]).is_err());
    }
    for at in [0, 8, 16, 20, 32, 36, 40, 44, 48, 52, 56, 60, 64, 68] {
        let mut invalid = bytes.clone();
        invalid[at] ^= 0x80;
        assert!(
            a64_unwind::decode(&invalid).is_err(),
            "accepted corruption at {at}"
        );
    }
    let mut empty_code = code;
    empty_code.frame_size = 0;
    let table = a64_unwind::decode(&a64_unwind::encode(empty_code, 0, &[], &[]).unwrap()).unwrap();
    assert!(table.sites.is_empty());
    assert!(table.caller(0x1008, 0x8000, &[], &[]).is_err());
}
