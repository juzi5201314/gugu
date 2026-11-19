use super::*;
use crate::{
    compression_schema::{CompressionDemand, CompressionPolicyV1, CompressionRuntimeContract},
    stackmap_codec::{CodeLayout, SafepointLayout, TypedRoot},
    stackmap_schema::StackMapArchitecture,
    target::PointerCompression,
};

fn plane() -> CompressionPlane {
    CompressionPlane::new(
        &CompressionRuntimeContract::build(
            CompressionDemand::default(),
            CompressionPolicyV1::disabled(),
            PointerCompression::aarch64(),
        )
        .unwrap(),
    )
}

fn metadata(rva: u64, entry: bool, gate: Option<bool>) -> FrameMetadata {
    let code = CodeLayout {
        code_rva: rva,
        code_size: 64,
        frame_size: 64,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: false,
        has_stack_interior: true,
    };
    let mut point = SafepointLayout::<u32> {
        pc_offset: 8,
        kind: if entry { 4 } else { 2 },
        dirty: false,
        copy_allowed: !entry,
        scan_allowed: true,
        slots: std::array::from_fn(|_| vec![0]),
        registers: [0; 5],
        typed: vec![],
        function: 0,
        slot_count: if entry { 0 } else { 8 },
    };
    if entry {
        point.registers[4] = 1;
    } else if gate.is_some() {
        point.typed.push(TypedRoot {
            word: 2,
            root_kind: 4,
            init_byte: 56,
        });
    } else {
        point.slots[4][0] = 1 << 2;
    }
    let maps =
        stackmap_codec::encode_for(StackMapArchitecture::Aarch64, &[code], &[(0, point)]).unwrap();
    let unwind = a64_unwind::encode(
        code,
        0,
        &[a64_unwind::Site {
            pc_offset: 8,
            entry,
        }],
    )
    .unwrap();
    FrameMetadata::decode(&maps, &unwind).unwrap()
}

fn put(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn fixture(gate: Option<bool>) -> (Vec<u8>, Snapshot, Vec<FrameMetadata>, StackDescriptor) {
    let mut old = StackDescriptor::default();
    old.install(0x1000, 512).unwrap();
    let start = 0x1180;
    let mut bytes = vec![0; 128];
    put(&mut bytes, 0, start + 64);
    put(&mut bytes, 8, 0x2008);
    put(&mut bytes, 16, start + 32);
    put(&mut bytes, 24, start + 32);
    put(&mut bytes, 72, 0);
    put(&mut bytes, 80, start + 96);
    bytes[56] = u8::from(gate.unwrap_or(false));
    let snapshot = Snapshot {
        context: A64Context {
            sp: start,
            pc: 0x4004,
            x29: start,
            x19_to_x26: [start + 32; 8],
            d8_to_d15: [u64::MAX; 8],
        },
        return_pc: 0x1008,
        registers: [0; 31],
    };
    (
        bytes,
        snapshot,
        vec![metadata(0x1000, false, gate), metadata(0x2000, false, None)],
        old,
    )
}

#[test]
fn serialized_maps_fix_all_frames_and_context_but_not_lookalike_words() {
    for gate in [None, Some(true), Some(false)] {
        let (bytes, snapshot, metadata, old) = fixture(gate);
        let original = bytes.clone();
        let image = Image::capture(bytes, &snapshot, 0, &metadata, &[], &mut plane()).unwrap();
        assert_eq!(image.frame_links(), &[0, 64]);
        assert_eq!(
            image.stack_roots(),
            if gate == Some(false) {
                &[80][..]
            } else {
                &[16, 80][..]
            }
        );
        for (low, capacity) in [(0x8000, 1024), (0x400, 512)] {
            let (copied, restored) = image.relocate(snapshot, &old, low, capacity).unwrap();
            let start = (low + capacity - 128) as u64;
            assert_eq!(restored.context.sp, start);
            assert_eq!(restored.context.x29, start);
            assert_eq!(word(copied.bytes(), 0).unwrap(), start + 64);
            assert_eq!(word(copied.bytes(), 80).unwrap(), start + 96);
            assert_eq!(
                word(copied.bytes(), 16).unwrap(),
                if gate == Some(false) {
                    0x11a0
                } else {
                    start + 32
                }
            );
            assert_eq!(&copied.bytes()[24..64], &original[24..64]);
            assert_eq!(restored.context.pc, snapshot.context.pc);
            assert_eq!(restored.context.x19_to_x26, snapshot.context.x19_to_x26);
            assert_eq!(restored.context.d8_to_d15, snapshot.context.d8_to_d15);
        }
        assert_eq!(image.bytes(), original);
    }
}

#[test]
fn entry_scratch_stack_registers_are_fixed_by_provenance_before_restore() {
    let (bytes, mut snapshot, _, old) = fixture(None);
    snapshot.return_pc = 0x808;
    snapshot.registers[0] = 0x11a0;
    snapshot.registers[1] = 0x11a0;
    snapshot.registers[10] = 0x1008;
    let metadata = vec![
        metadata(0x800, true, None),
        metadata(0x1000, false, None),
        metadata(0x2000, false, None),
    ];
    let image = Image::capture(bytes, &snapshot, 0, &metadata, &[], &mut plane()).unwrap();
    let (_, restored) = image.relocate(snapshot, &old, 0x8000, 512).unwrap();
    assert_eq!(restored.registers[0], 0x81a0);
    assert_eq!(restored.registers[1], 0x11a0);
    assert_eq!(restored.registers[10], 0x1008);
}

#[test]
fn empty_images_and_invalid_cfa_snapshots_fail_without_mutation() {
    let mut old = StackDescriptor::default();
    old.install(0x1000, 512).unwrap();
    let snapshot = Snapshot {
        context: A64Context {
            sp: 0x1200,
            ..Default::default()
        },
        return_pc: 0,
        registers: [0; 31],
    };
    let empty = Image::capture(vec![], &snapshot, 0, &[], &[], &mut plane()).unwrap();
    assert!(empty.bytes().is_empty());
    assert_eq!(
        empty
            .relocate(snapshot, &old, 0x8000, 512)
            .unwrap()
            .1
            .context
            .sp,
        0x8200
    );
    for case in 0..9 {
        let (mut bytes, mut snapshot, mut metadata, old) = fixture(None);
        match case {
            0 => snapshot.return_pc += 4,
            1 => snapshot.context.sp += 1,
            2 => snapshot.context.x29 += 16,
            3 => put(&mut bytes, 0, 0x1190),
            4 => put(&mut bytes, 8, 0x100c),
            5 => {
                bytes.pop();
            }
            6 => metadata.reverse(),
            7 => metadata[0].maps.safepoints[0].flags = 2,
            _ => put(&mut bytes, 16, 0xffff),
        }
        let result = Image::capture(bytes, &snapshot, 0, &metadata, &[], &mut plane());
        if case == 8 {
            let image = result.unwrap();
            assert!(image.relocate(snapshot, &old, 0x8000, 512).is_err());
        } else {
            assert!(result.is_err(), "case {case}");
        }
    }
    let (bytes, snapshot, metadata, old) = fixture(None);
    let image = Image::capture(bytes, &snapshot, 0, &metadata, &[], &mut plane()).unwrap();
    for (low, capacity) in [(0x8001, 512), (0x8000, 64), (usize::MAX - 511, 512)] {
        assert!(image.relocate(snapshot, &old, low, capacity).is_err());
    }
    let mut changed = snapshot;
    changed.context.pc += 4;
    assert!(image.relocate(changed, &old, 0x8000, 512).is_err());
}

#[test]
fn malformed_or_wrong_architecture_metadata_is_rejected() {
    assert!(FrameMetadata::decode(&[], &[]).is_err());
    let legacy = stackmap_codec::encode(&[], &[]).unwrap();
    assert!(FrameMetadata::decode(&legacy, &[]).is_err());
}

#[test]
fn morestack_rejects_bad_mailboxes_and_failed_copies_leave_records_untouched() {
    let (bytes, mut snapshot, _, old) = fixture(None);
    snapshot.return_pc = 0x808;
    snapshot.registers[0] = 0x11a0;
    snapshot.registers[10] = 0x1008;
    let mut scratch = A64MorestackScratch {
        return_pc: 0x808,
        caller_lr: 0x1008,
        ..Default::default()
    };
    scratch.gpr[0] = 0x11a0;
    let mut mailbox = A64MorestackMailbox {
        entry_sp: snapshot.context.sp,
        candidate: snapshot.context.sp - 512,
        required: 512,
        ..Default::default()
    };
    assert_eq!(
        Snapshot::morestack(snapshot.context, &mailbox, &scratch, 0x4004).unwrap(),
        snapshot
    );
    for case in 0..4 {
        let mut invalid = mailbox;
        let mut context = snapshot.context;
        match case {
            0 => context.pc += 4,
            1 => context.sp += 16,
            2 => invalid.required = u64::MAX,
            _ => invalid.candidate += 8,
        }
        assert!(Snapshot::morestack(context, &invalid, &scratch, 0x4004).is_err());
    }
    let tables = vec![
        metadata(0x800, true, None),
        metadata(0x1000, false, None),
        metadata(0x2000, false, None),
    ];
    let image = Image::capture(bytes, &snapshot, 0, &tables, &[], &mut plane()).unwrap();
    let before = (mailbox, scratch);
    assert!(
        image
            .relocate_morestack(
                snapshot.context,
                &mut mailbox,
                &mut scratch,
                &old,
                (0x8001, 512)
            )
            .is_err()
    );
    assert_eq!((mailbox, scratch), before);
    let (image, context) = image
        .relocate_morestack(
            snapshot.context,
            &mut mailbox,
            &mut scratch,
            &old,
            (0x8000, 1024),
        )
        .unwrap();
    assert_eq!(scratch.gpr[0], 0x83a0);
    assert_eq!((scratch.return_pc, scratch.caller_lr), (0x808, 0x1008));
    assert_eq!(
        (mailbox.entry_sp, mailbox.candidate),
        (context.sp, context.sp - 512)
    );
    assert_eq!(image.bytes().len(), 128);
}

#[test]
fn image_code_bias_and_joint_metadata_are_checked_before_walking() {
    let code = CodeLayout {
        code_rva: 0,
        code_size: 64,
        frame_size: 16,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: false,
        has_stack_interior: false,
    };
    let mut point = SafepointLayout::<u32> {
        pc_offset: 8,
        kind: 2,
        dirty: false,
        copy_allowed: true,
        scan_allowed: true,
        slots: std::array::from_fn(|_| vec![0]),
        registers: [0; 5],
        typed: vec![],
        function: 0,
        slot_count: 2,
    };
    let maps = stackmap_codec::encode_for(
        StackMapArchitecture::Aarch64,
        &[code],
        &[(0, point.clone())],
    )
    .unwrap();
    let unwind = a64_unwind::encode(
        code,
        0,
        &[a64_unwind::Site {
            pc_offset: 8,
            entry: false,
        }],
    )
    .unwrap();
    let loaded = FrameMetadata::load(&maps, &unwind, 0x1000).unwrap();
    assert_eq!(loaded.maps.functions[0].code_rva, 0x1000);
    assert_eq!(loaded.unwind.code_rva, 0x1000);
    for mask in [1, 2] {
        point.slots[4][0] = mask;
        let invalid = stackmap_codec::encode_for(
            StackMapArchitecture::Aarch64,
            &[code],
            &[(0, point.clone())],
        )
        .unwrap();
        assert!(FrameMetadata::decode(&invalid, &unwind).is_err());
    }
    for bias in [1, u64::MAX - 3] {
        assert!(FrameMetadata::load(&maps, &unwind, bias).is_err());
    }
    let wrong = a64_unwind::encode(
        CodeLayout {
            frame_size: 32,
            ..code
        },
        0,
        &[a64_unwind::Site {
            pc_offset: 8,
            entry: false,
        }],
    )
    .unwrap();
    assert!(FrameMetadata::decode(&maps, &wrong).is_err());
}
