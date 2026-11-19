use super::TargetName;
use super::harness::{Harness, put, word};
use crate::runtime::{
    self,
    a64_binding::{Function, Sections},
    stackmap_codec::{self, a64_unwind},
    stackmap_schema::StackMapArchitecture,
};
use std::mem::offset_of;
use std::sync::LazyLock;

const MODULE: &str = "std/a64_binding_probe.gg";
const MAPS: u64 = 4096;
const CFA: u64 = 8192;
const OUT: u64 = 12288;
const SNAP: u64 = 16384;
const CONTROL: u64 = 20480;
const FRAME: u64 = 24576;
const BASE: u64 = 65536;
const NONE: u64 = u32::MAX as u64;
const A64_GUARD: &str = "#![cfg(arch = \"aarch64\")]";

#[derive(Debug)]
struct A64Fixtures;

impl runtime::RuntimeArtifactProvider for A64Fixtures {
    fn artifact(
        &self,
        target: TargetName,
    ) -> Result<&'static runtime::RuntimeArtifactV1, runtime::RuntimeArtifactError> {
        assert_eq!(target, TargetName::X86_64Linux);
        static ARTIFACT: LazyLock<runtime::RuntimeArtifactV1> = LazyLock::new(|| {
            let base = crate::testing::provider()
                .artifact(TargetName::X86_64Linux)
                .unwrap();
            let a64 = base
                .payload
                .sources
                .iter()
                .find(|blob| blob.logical_path == "std/runtime/a64.gg")
                .unwrap();
            assert!(a64.source.starts_with(A64_GUARD));
            let host_guard = format!(
                "#![cfg(arch = \"{}\")]",
                TargetName::X86_64Linux.descriptor().arch.name()
            );
            let interpreted = a64.source.replacen(A64_GUARD, &host_guard, 1);
            let roots = base
                .payload
                .sources
                .iter()
                .find(|blob| blob.logical_path == "std/runtime/roots.gg")
                .unwrap();
            let interpreted_roots = roots.source.replace(
                "#[cfg(arch = \"aarch64\")]",
                &host_guard.replace("#![", "#["),
            );
            let mut sources: Vec<_> = base
                .payload
                .sources
                .iter()
                .map(|blob| gugu_compiler_core::RuntimeSourceInput {
                    logical_path: &blob.logical_path,
                    source: if blob.logical_path == a64.logical_path {
                        &interpreted
                    } else if blob.logical_path == roots.logical_path {
                        &interpreted_roots
                    } else {
                        &blob.source
                    },
                    role: blob.role,
                })
                .collect();
            sources.push(gugu_compiler_core::RuntimeSourceInput {
                logical_path: MODULE,
                source: include_str!("fixtures/a64_binding.gg"),
                role: gugu_compiler_core::RuntimeSourceRole::StandardLibrary,
            });
            let artifact = gugu_compiler_core::RuntimeArtifactBuilder::build(
                gugu_compiler_core::TargetName::X86_64Linux,
                &sources,
            )
            .unwrap();
            crate::frontend::prebuild::materialize_tables(TargetName::X86_64Linux, artifact)
                .unwrap()
        });
        Ok(&ARTIFACT)
    }
}

#[test]
fn a64_architecture_cfg_excludes_declarations_from_x64_artifacts() {
    for target in [
        TargetName::X86_64Linux,
        TargetName::X86_64Windows,
        TargetName::X86_64Macos,
    ] {
        let artifact = crate::testing::provider().artifact(target).unwrap();
        let source = artifact
            .payload
            .sources
            .iter()
            .find(|source| source.logical_path == "std/runtime/a64.gg")
            .unwrap();
        assert!(source.source.starts_with(A64_GUARD));
        assert_ne!(target.descriptor().arch.name(), "aarch64");
        let hir: crate::frontend::hir::Module =
            crate::canonical::from_slice(&artifact.payload.prebuilt.as_ref().unwrap().hir).unwrap();
        assert!(
            !hir.definitions
                .iter()
                .any(|definition| definition
                    .location
                    .as_ref()
                    .is_some_and(|location| hir.sources[location.source as usize].path
                        == source.logical_path)),
            "{target}"
        );
        for helper in ["gc_metadata_word", "gc_metadata_lookup", "gc_metadata_emit"] {
            assert!(
                !hir.definitions
                    .iter()
                    .any(|definition| definition.name == helper
                        && definition
                            .location
                            .as_ref()
                            .is_some_and(|location| hir.sources[location.source as usize].path
                                == "std/runtime/roots.gg")),
                "{target}: {helper}"
            );
        }
    }
}

type RuntimeLayouts = (
    crate::frontend::hir::Module,
    Vec<crate::lir::body::Body>,
    Vec<gugu_gir::gir::concrete::RecordTypes>,
);

#[test]
fn a64_production_gugu_binding_contracts() {
    let runtime = runtime_layouts();
    a64_production_gugu_decodes_all_classes_gates_and_capacity(&runtime);
    a64_production_gugu_walks_payload_cfa_and_morestack_snapshot(&runtime);
    a64_production_entry_cfa_does_not_require_the_unallocated_callee_frame(&runtime);
    a64_production_gugu_empty_wrong_architecture_and_exact_pc(&runtime);
    a64_source_layouts_and_nonmoving_mailbox_binding_match_rust(&runtime);
}

fn runtime_layouts() -> RuntimeLayouts {
    let target = TargetName::X86_64Linux;
    let artifact = runtime::RuntimeArtifactProvider::artifact(&A64Fixtures, target).unwrap();
    let resources = runtime::RuntimeResources::new(artifact);
    let queries = crate::QueryEngine::new();
    let mut source_map = crate::SourceMap::new(vec![
        crate::SourceSnapshot::from_str("main.gg", "").unwrap(),
    ])
    .unwrap();
    let cfg = crate::frontend::cfg::CfgContext::target_only(target);
    let output = crate::frontend::bootstrap_with(
        crate::frontend::SourceInput::Sources {
            source_map: &mut source_map,
            entry: "main.gg",
            source_root: "",
            package_identity: "single-file",
            require_main: false,
            cfg: &cfg,
            external_packages: &std::collections::BTreeSet::new(),
            embed: None,
        },
        &queries,
        resources,
        crate::frontend::PrefixMode::Install,
    )
    .unwrap();
    let lir = crate::lir::build(
        &output.hir,
        &output.gir,
        &output.mono,
        target,
        crate::lir::installed::RECORDED_COMPRESSION,
        &crate::backend::x64::oracle::X64Oracle::new(target.descriptor().cpu_baseline),
        &queries,
        &source_map,
        (&output.installed, resources.prebuilt()),
    )
    .unwrap();
    let hir = output.hir.module();
    let records = output
        .gir
        .concrete
        .iter()
        .filter(|body| {
            let definition = &hir.definitions[body.body.owner.index()];
            definition.kind == crate::frontend::hir::DefinitionKind::Function
                && definition.location.as_ref().is_some_and(|location| {
                    hir.sources[location.source as usize].path == "std/runtime/a64.gg"
                })
        })
        .map(|body| gugu_gir::gir::concrete::RecordTypes {
            definition: body.body.owner,
            parameters: body.body.signature.parameters.clone(),
            types: body.types.clone(),
        })
        .collect();
    (hir.clone(), lir.bodies().to_vec(), records)
}

fn review_image(slot_count: u32, scan_allowed: bool) -> Sections {
    let code = runtime::CodeLayout {
        code_rva: 0,
        code_size: 128,
        frame_size: 96,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: false,
        has_stack_interior: true,
    };
    let point = runtime::SafepointLayout {
        pc_offset: 16,
        kind: 1,
        dirty: false,
        copy_allowed: true,
        scan_allowed,
        slots: std::array::from_fn(|_| vec![0; (slot_count as usize).div_ceil(64).max(1)]),
        registers: [0_u32; 5],
        typed: vec![runtime::TypedRoot {
            word: 4,
            root_kind: 4,
            init_byte: 64,
        }],
        function: 0,
        slot_count,
    };
    let maps =
        stackmap_codec::encode_for(StackMapArchitecture::Aarch64, &[code], &[(0, point)]).unwrap();
    let unwind = a64_unwind::encode(
        code,
        80,
        &[a64_unwind::Site {
            pc_offset: 16,
            entry: false,
        }],
    )
    .unwrap();
    let directory = Sections::encode(&[Function {
        code_rva: 0,
        maps: &maps,
        unwind: &unwind,
    }])
    .unwrap();
    Sections::decode(&maps, directory.unwind()).unwrap()
}

#[test]
fn a64_gated_root_beyond_fixed_bitmap_scans_by_frame_bounds() {
    let (hir, bodies, _) = runtime_layouts();
    let mut h = Harness::from_bodies(&hir, &bodies);
    for slots in [0, 4] {
        let image = review_image(slots, true);
        assert_eq!(image.frames(BASE).unwrap().len(), 1);
        let mut memory = vec![0; 262144];
        install(&mut memory, &image);
        for initialized in [0, 1] {
            memory[OUT as usize..OUT as usize + 512].fill(0);
            memory[FRAME as usize + 64] = initialized;
            assert_eq!(
                h.invoke(
                    MODULE,
                    "gc_a64_scan_frame",
                    &[
                        OUT,
                        4,
                        MAPS,
                        image.maps().len() as u64,
                        BASE,
                        FRAME,
                        BASE + 16,
                        1,
                        0,
                        0
                    ],
                    &mut memory
                ),
                [u64::from(initialized)],
                "slot_count={slots}, gate={initialized}"
            );
            assert_eq!(word(&memory, OUT + 16), 1);
            if initialized != 0 {
                assert_eq!(word(&memory, OUT + 96), FRAME + 32);
            }
            memory[OUT as usize..OUT as usize + 512].fill(0);
            put(&mut memory, SNAP + 152, BASE + 16);
            put(&mut memory, FRAME + 88, 0);
            assert_eq!(
                h.invoke(
                    MODULE,
                    "gc_a64_walk_roots",
                    &[OUT, 4, 256, 0, SNAP, 1, FRAME, FRAME + 96, 1],
                    &mut memory
                ),
                [u64::from(initialized)]
            );
            assert_eq!(word(&memory, OUT + 16), 1);
        }
    }
}

#[test]
fn a64_scan_disallowed_safepoints_reject_both_scan_entries_without_emitting() {
    let (hir, bodies, _) = runtime_layouts();
    let mut h = Harness::from_bodies(&hir, &bodies);
    for allowed in [false, true] {
        let image = review_image(12, allowed);
        for initialized in [0, 1] {
            let mut memory = vec![0; 262144];
            install(&mut memory, &image);
            put(&mut memory, SNAP + 152, BASE + 16);
            put(&mut memory, FRAME + 88, 0);
            memory[FRAME as usize + 64] = initialized;
            let direct = h.invoke(
                MODULE,
                "gc_a64_scan_frame",
                &[
                    OUT,
                    4,
                    MAPS,
                    image.maps().len() as u64,
                    BASE,
                    FRAME,
                    BASE + 16,
                    1,
                    0,
                    0,
                ],
                &mut memory,
            )[0];
            let direct_counts = [
                word(&memory, OUT),
                word(&memory, OUT + 8),
                word(&memory, OUT + 16),
            ];
            memory[OUT as usize..OUT as usize + 512].fill(0);
            let walk = h.invoke(
                MODULE,
                "gc_a64_walk_roots",
                &[OUT, 4, 256, 0, SNAP, 1, FRAME, FRAME + 96, 1],
                &mut memory,
            )[0];
            let expected = if allowed {
                u64::from(initialized)
            } else {
                NONE
            };
            assert_eq!(
                [direct, walk],
                [expected; 2],
                "scan_allowed={allowed}, gate={initialized}"
            );
            let counts = if allowed {
                [u64::from(initialized), u64::from(initialized), 1]
            } else {
                [0; 3]
            };
            assert_eq!(direct_counts, counts);
            assert_eq!(
                [
                    word(&memory, OUT),
                    word(&memory, OUT + 8),
                    word(&memory, OUT + 16)
                ],
                counts
            );
        }
    }
}

fn fragment(roots: bool) -> (Vec<u8>, Vec<u8>) {
    fragment_size(roots, 96)
}

fn fragment_size(roots: bool, frame_size: u32) -> (Vec<u8>, Vec<u8>) {
    let code = runtime::CodeLayout {
        code_rva: 0,
        code_size: 128,
        frame_size,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: false,
        has_stack_interior: roots,
    };
    let point = |entry: bool| runtime::SafepointLayout {
        pc_offset: if entry { 4 } else { 16 },
        kind: if entry { 4 } else { 1 },
        dirty: false,
        copy_allowed: true,
        scan_allowed: true,
        slots: std::array::from_fn(|class| {
            let mut words = vec![
                0;
                if entry {
                    1
                } else {
                    (frame_size as usize / 8).div_ceil(64)
                }
            ];
            if roots && !entry && class < 4 {
                words[0] = 1 << class;
            }
            words
        }),
        registers: std::array::from_fn(|class| if roots && entry { 1_u32 << class } else { 0 }),
        typed: if roots && !entry {
            vec![runtime::TypedRoot {
                word: 4,
                root_kind: 4,
                init_byte: 64,
            }]
        } else {
            vec![]
        },
        function: 0,
        slot_count: if entry { 0 } else { frame_size / 8 },
    };
    (
        stackmap_codec::encode_for(
            StackMapArchitecture::Aarch64,
            &[code],
            &[(0, point(true)), (0, point(false))],
        )
        .unwrap(),
        a64_unwind::encode(
            code,
            frame_size - 16,
            &[
                a64_unwind::Site {
                    pc_offset: 4,
                    entry: true,
                },
                a64_unwind::Site {
                    pc_offset: 16,
                    entry: false,
                },
            ],
        )
        .unwrap(),
    )
}

fn image(roots: bool) -> Sections {
    let (maps, unwind) = fragment(roots);
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

fn install(memory: &mut [u8], image: &Sections) {
    put(memory, 256, runtime::entry::Rt0Payload::REVISION);
    memory[MAPS as usize..MAPS as usize + image.maps().len()].copy_from_slice(image.maps());
    memory[CFA as usize..CFA as usize + image.unwind().len()].copy_from_slice(image.unwind());
    for (offset, value) in [
        (48, MAPS),
        (56, BASE),
        (64, image.maps().len() as u64),
        (72, CFA),
        (80, image.unwind().len() as u64),
    ] {
        put(memory, 256 + offset, value);
    }
    put(memory, SNAP, FRAME);
    put(memory, SNAP + 152, BASE + 16);
    for index in 0..5 {
        put(
            memory,
            FRAME + index * 8,
            if index == 4 {
                FRAME + 32
            } else {
                0x9000 + index * 8
            },
        );
        put(
            memory,
            FRAME + 96 + index * 8,
            if index == 4 {
                FRAME + 128
            } else {
                0xa000 + index * 8
            },
        );
        put(
            memory,
            SNAP + 160 + index * 8,
            if index == 4 {
                FRAME + 32
            } else {
                0xb000 + index * 8
            },
        );
    }
    memory[FRAME as usize + 64] = 1;
    memory[FRAME as usize + 160] = 1;
    put(memory, FRAME + 88, BASE + 272);
}

fn a64_production_gugu_decodes_all_classes_gates_and_capacity(runtime: &RuntimeLayouts) {
    let (hir, bodies, _) = runtime;
    let mut h = Harness::from_bodies(hir, bodies);
    h.heap = 131072;
    let image = image(true);
    let mut memory = vec![0; 262144];
    install(&mut memory, &image);
    let mut args = [
        OUT,
        8,
        MAPS,
        image.maps().len() as u64,
        BASE,
        FRAME,
        BASE + 16,
        1,
        SNAP + 160,
        0,
    ];
    assert_eq!(
        h.invoke(MODULE, "gc_a64_scan_frame", &args, &mut memory),
        [5]
    );
    assert_eq!((word(&memory, OUT + 8), word(&memory, OUT + 16)), (5, 1));
    for class in 0..5 {
        let at = OUT + 96 + class * 32;
        assert_eq!(word(&memory, at), FRAME + class * 8);
        assert_eq!(word(&memory, at + 8), class);
        assert_eq!(
            word(&memory, at + 16),
            if class == 4 {
                FRAME + 32
            } else {
                0x9000 + class * 8
            }
        );
    }
    memory[OUT as usize..OUT as usize + 512].fill(0);
    memory[FRAME as usize + 64] = 0;
    args[1] = 2;
    assert_eq!(
        h.invoke(MODULE, "gc_a64_scan_frame", &args, &mut memory),
        [4]
    );
    assert_eq!(word(&memory, OUT + 8), 2);
    assert_eq!(word(&memory, FRAME + 32), FRAME + 32);
    memory[OUT as usize..OUT as usize + 512].fill(0);
    args[6] = BASE + 4;
    args[7] = 4;
    args[9] = 0x1ff;
    args[1] = 8;
    assert_eq!(
        h.invoke(MODULE, "gc_a64_scan_frame", &args, &mut memory),
        [5]
    );
    assert_eq!(word(&memory, OUT + 96), SNAP + 160);
    args[9] = 0;
    assert_eq!(
        h.invoke(MODULE, "gc_a64_scan_frame", &args, &mut memory),
        [NONE]
    );
}

fn a64_production_gugu_walks_payload_cfa_and_morestack_snapshot(runtime: &RuntimeLayouts) {
    let (hir, bodies, _) = runtime;
    let mut h = Harness::from_bodies(hir, bodies);
    let image = image(true);
    let mut memory = vec![0; 262144];
    install(&mut memory, &image);
    let mut args = [OUT, 16, 256, 0, SNAP, 1, FRAME, FRAME + 192, 2];
    assert_eq!(
        h.invoke(MODULE, "gc_a64_walk_roots", &args, &mut memory),
        [10]
    );
    assert_eq!((word(&memory, OUT + 8), word(&memory, OUT + 16)), (10, 2));
    memory[OUT as usize..OUT as usize + 1024].fill(0);
    put(&mut memory, SNAP + 152, BASE + 4);
    put(&mut memory, SNAP + 240, BASE + 272);
    put(&mut memory, FRAME + 88, 0);
    args[5] = 4;
    assert_eq!(
        h.invoke(MODULE, "gc_a64_walk_roots", &args, &mut memory),
        [10]
    );
    assert_eq!(word(&memory, OUT + 96 + 5 * 32), FRAME);
    assert_eq!(word(&memory, OUT + 32), 4);
    assert_eq!(word(&memory, OUT + 40), 272);
    memory[OUT as usize..OUT as usize + 1024].fill(0);
    args[8] = 1;
    assert_eq!(
        h.invoke(MODULE, "gc_a64_walk_roots", &args, &mut memory),
        [NONE]
    );
    memory[OUT as usize..OUT as usize + 1024].fill(0);
    args[8] = 2;
    put(&mut memory, FRAME + 88, BASE + 1024);
    assert_eq!(
        h.invoke(MODULE, "gc_a64_walk_roots", &args, &mut memory),
        [NONE]
    );
    memory[OUT as usize..OUT as usize + 1024].fill(0);
    put(&mut memory, 256 + 40, BASE + 1024);
    assert_eq!(
        h.invoke(MODULE, "gc_a64_walk_roots", &args, &mut memory),
        [10]
    );
    assert_eq!(word(&memory, OUT + 24), 1024);
}

fn a64_production_entry_cfa_does_not_require_the_unallocated_callee_frame(
    runtime: &RuntimeLayouts,
) {
    let (hir, bodies, _) = runtime;
    let mut h = Harness::from_bodies(hir, bodies);
    let (maps, unwind) = fragment_size(true, 4096);
    let (caller_maps, caller_unwind) = fragment(true);
    let image = Sections::encode(&[
        Function {
            code_rva: 0,
            maps: &maps,
            unwind: &unwind,
        },
        Function {
            code_rva: 256,
            maps: &caller_maps,
            unwind: &caller_unwind,
        },
    ])
    .unwrap();
    let mut memory = vec![0; 262144];
    install(&mut memory, &image);
    put(&mut memory, SNAP + 152, BASE + 4);
    put(&mut memory, SNAP + 240, BASE + 272);
    put(&mut memory, FRAME + 88, 0);
    assert_eq!(
        h.invoke(
            MODULE,
            "gc_a64_walk_roots",
            &[OUT, 16, 256, 0, SNAP, 4, FRAME, FRAME + 96, 2],
            &mut memory
        ),
        [10]
    );
}

fn a64_production_gugu_empty_wrong_architecture_and_exact_pc(runtime: &RuntimeLayouts) {
    let (hir, bodies, _) = runtime;
    let mut h = Harness::from_bodies(hir, bodies);
    let image = image(false);
    let mut memory = vec![0; 262144];
    install(&mut memory, &image);
    let mut args = [
        OUT,
        0,
        MAPS,
        image.maps().len() as u64,
        BASE,
        FRAME,
        BASE + 16,
        1,
        0,
        0,
    ];
    assert_eq!(
        h.invoke(MODULE, "gc_a64_scan_frame", &args, &mut memory),
        [0]
    );
    args[6] = BASE + 20;
    assert_eq!(
        h.invoke(MODULE, "gc_a64_scan_frame", &args, &mut memory),
        [NONE]
    );
    args[6] = BASE + 16;
    memory[MAPS as usize + 8] = 4;
    assert_eq!(
        h.invoke(MODULE, "gc_a64_scan_frame", &args, &mut memory),
        [NONE]
    );
    memory[MAPS as usize + 8] = 5;
    args[3] = 32;
    assert_eq!(
        h.invoke(MODULE, "gc_a64_scan_frame", &args, &mut memory),
        [NONE]
    );
    assert_eq!(word(&memory, OUT + 8), 0);
}

fn a64_source_layouts_and_nonmoving_mailbox_binding_match_rust(runtime: &RuntimeLayouts) {
    let artifact = crate::testing::provider()
        .artifact(super::TargetName::X86_64Linux)
        .unwrap();
    let resources = runtime::RuntimeResources::new(artifact);
    let (hir, bodies, records) = runtime;
    let mut contract = runtime::CoroutineRuntimeContract::build(Default::default()).unwrap();
    contract.records = runtime::a64_binding::source_layouts();
    gugu_gir::contract::coroutine_layout::verify_source(&contract, hir, records, &resources)
        .unwrap();
    contract.records[0].fields[0].offset += 8;
    assert!(
        gugu_gir::contract::coroutine_layout::verify_source(&contract, hir, records, &resources)
            .is_err()
    );
    let mut h = Harness::from_bodies(hir, bodies);
    let mut memory = vec![0; 262144];
    put(&mut memory, CONTROL, 0xcafe);
    assert_eq!(
        h.invoke(
            MODULE,
            "a64_bind_control",
            &[CONTROL, 128, 1024],
            &mut memory
        ),
        [1]
    );
    let mailbox = CONTROL + offset_of!(runtime::a64_binding::A64ControlBlock, mailbox) as u64;
    assert_eq!(word(&memory, mailbox), 128);
    assert_eq!(
        word(&memory, mailbox + 32),
        CONTROL + offset_of!(runtime::a64_binding::A64ControlBlock, scratch) as u64
    );
    assert_eq!(word(&memory, mailbox + 40), CONTROL);
    assert_eq!(word(&memory, mailbox + 48), 1024);
    assert_eq!(word(&memory, CONTROL), 0xcafe);
    assert_eq!(
        h.invoke(
            MODULE,
            "a64_bind_control",
            &[CONTROL + 1, 128, 1024],
            &mut memory
        ),
        [0]
    );
    put(&mut memory, CONTROL, FRAME);
    put(&mut memory, CONTROL + 8, BASE + 64);
    put(&mut memory, mailbox + 8, FRAME);
    put(&mut memory, mailbox + 16, FRAME - 4096);
    put(&mut memory, mailbox + 24, 4096);
    let scratch = CONTROL + offset_of!(runtime::a64_binding::A64ControlBlock, scratch) as u64;
    put(&mut memory, scratch, BASE + 4);
    put(&mut memory, scratch + 8, BASE + 272);
    put(&mut memory, scratch + 144 + 32, 0xb020);
    memory[SNAP as usize..SNAP as usize + 408].fill(0xaa);
    assert_eq!(
        h.invoke(
            MODULE,
            "a64_morestack_snapshot",
            &[CONTROL, SNAP, BASE + 64],
            &mut memory
        ),
        [1]
    );
    assert_eq!(
        (word(&memory, SNAP), word(&memory, SNAP + 8)),
        (FRAME, BASE + 64)
    );
    assert_eq!(word(&memory, SNAP + 152), BASE + 4);
    assert_eq!(
        (word(&memory, SNAP + 192), word(&memory, SNAP + 240)),
        (0xb020, BASE + 272)
    );
    assert_eq!(word(&memory, SNAP + 232), 0);
    let saved = memory[SNAP as usize..SNAP as usize + 408].to_vec();
    assert_eq!(
        h.invoke(
            MODULE,
            "a64_morestack_snapshot",
            &[CONTROL, SNAP, BASE + 68],
            &mut memory
        ),
        [0]
    );
    assert_eq!(memory[SNAP as usize..SNAP as usize + 408], saved);
}
