use super::*;
use crate::stackmap_codec::TypedRoot;

fn fragment(root_word: u32) -> (Vec<u8>, Vec<u8>) {
    let code = CodeLayout {
        code_rva: 0,
        code_size: 128,
        frame_size: 96,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: false,
        has_stack_interior: true,
    };
    let point = SafepointLayout {
        pc_offset: 16,
        kind: 1,
        dirty: false,
        copy_allowed: true,
        scan_allowed: true,
        slots: std::array::from_fn(|_| vec![0]),
        registers: [0_u32; 5],
        typed: vec![TypedRoot {
            word: root_word,
            root_kind: 4,
            init_byte: 64,
        }],
        function: 0,
        slot_count: 12,
    };
    (
        stackmap_codec::encode_for(StackMapArchitecture::Aarch64, &[code], &[(0, point)]).unwrap(),
        a64_unwind::encode(
            code,
            80,
            &[a64_unwind::Site {
                pc_offset: 16,
                entry: false,
            }],
        )
        .unwrap(),
    )
}

fn image() -> Sections {
    let (maps, unwind) = fragment(4);
    Sections::encode(&[
        Function {
            code_rva: 256,
            maps: &maps,
            unwind: &unwind,
        },
        Function {
            code_rva: 0,
            maps: &maps,
            unwind: &unwind,
        },
    ])
    .unwrap()
}

fn payload() -> Rt0Payload {
    Rt0Payload {
        revision: Rt0Payload::REVISION,
        main_entry: 99,
        initial_slot: 0,
        runtime_state: 0,
        bias: 0,
        main_return: 0,
        stackmap: 0,
        fragment_base: 0,
        stackmap_len: 0,
        unwind: 0,
        unwind_len: 0,
        gc_types: 0,
        gc_types_len: 0,
        gc_meta: 0,
        gc_meta_len: 0,
        boot_environment: 0,
        boot_argc: 0,
        boot_argv: 0,
    }
}

#[test]
fn a64_image_aggregates_rebases_and_binds_existing_payload() {
    let image = image();
    let maps = stackmap_codec::decode_tables(image.maps()).unwrap();
    assert_eq!(
        maps.functions
            .iter()
            .map(|f| (f.code_rva, f.unwind_index, f.safepoint_start))
            .collect::<Vec<_>>(),
        [(0, 0, 0), (256, 1, 1)]
    );
    assert_eq!(maps.maps.len(), 1);
    assert_eq!(maps.maps[0].typed[0].root_kind, 4);
    assert_eq!(
        Sections::decode(image.maps(), image.unwind()).unwrap(),
        image
    );
    assert_eq!(image.frames(65536).unwrap().len(), 2);
    assert!(image.frames(1).is_err());
    let mut payload = payload();
    image.bind(&mut payload, 4096, 8192, 65536).unwrap();
    assert_eq!(
        (payload.stackmap, payload.unwind, payload.fragment_base),
        (4096, 8192, 65536)
    );
    assert_eq!(payload.stackmap_len, image.maps().len() as u64);
    assert_eq!(payload.unwind_len, image.unwind().len() as u64);
    assert_eq!(payload.main_entry, 99);
    assert!(image.bind(&mut payload, 4097, 8192, 65536).is_err());
    assert!(image.bind(&mut payload, 4096, 4096, 65536).is_err());
    assert!(image.bind(&mut payload, 4096, 8192, u64::MAX - 3).is_err());
    assert_eq!(payload.stackmap, 4096);
}

#[test]
fn a64_image_empty_and_malformed_sections_are_deterministic() {
    let empty = Sections::encode(&[]).unwrap();
    assert!(
        stackmap_codec::decode_tables(empty.maps())
            .unwrap()
            .functions
            .is_empty()
    );
    assert_eq!(
        Sections::decode(empty.maps(), empty.unwind()).unwrap(),
        empty
    );
    let x64 = stackmap_codec::encode(&[], &[]).unwrap();
    assert!(Sections::decode(&x64, empty.unwind()).is_err());
    let image = image();
    for size in [0, 8, 16, 23, image.unwind().len() - 1] {
        assert!(Sections::decode(image.maps(), &image.unwind()[..size]).is_err());
    }
    let mut corrupt = image.unwind().to_vec();
    corrupt[16..24].copy_from_slice(&0_u64.to_le_bytes());
    assert!(Sections::decode(image.maps(), &corrupt).is_err());
    let mut corrupt = image.unwind().to_vec();
    let table = u64::from_le_bytes(corrupt[16..24].try_into().unwrap()) as usize;
    corrupt[table + 24..table + 32].copy_from_slice(&4_u64.to_le_bytes());
    assert!(Sections::decode(image.maps(), &corrupt).is_err());
    let (maps, unwind) = fragment(10);
    assert!(
        Sections::encode(&[Function {
            code_rva: 0,
            maps: &maps,
            unwind: &unwind
        }])
        .is_err()
    );
    let (maps, unwind) = fragment(4);
    assert!(
        Sections::encode(&[
            Function {
                code_rva: 0,
                maps: &maps,
                unwind: &unwind
            },
            Function {
                code_rva: 64,
                maps: &maps,
                unwind: &unwind
            }
        ])
        .is_err()
    );
}

#[test]
fn a64_control_block_binding_preserves_saved_state() {
    let mut state = A64ControlBlock::default();
    state.context.x19_to_x26[3] = 0xcafe;
    state.scratch.vector[7] = [0xab; 16];
    state.mailbox.entry_sp = 0x8000;
    state.bind(0x1000, 0x2000, 0x3000).unwrap();
    assert_eq!(
        state.mailbox.scratch,
        0x1000 + offset_of!(A64ControlBlock, scratch) as u64
    );
    assert_eq!(state.mailbox.mutator_context, 0x1000);
    assert_eq!(state.mailbox.system_context, 0x3000);
    assert_eq!(state.context.x19_to_x26[3], 0xcafe);
    assert_eq!(state.scratch.vector[7], [0xab; 16]);
    assert_eq!(state.mailbox.entry_sp, 0x8000);
    let saved = state.clone();
    for address in [0, 0x1001, u64::MAX - 15] {
        assert!(state.bind(address, 1, 8).is_err());
    }
    assert_eq!(state, saved);
    assert_eq!(offset_of!(Snapshot, return_pc), 152);
    assert_eq!(offset_of!(Snapshot, registers), 160);
}
