use super::runtime_vm::{Harness, bindings};
use super::*;
use crate::regalloc::{Location, Placement};

fn fixture(params: &[Type], results: &[Type]) -> Fixture {
    let mut f = Fixture::new(params, results);
    f.body.poll_interface.leaf = false;
    f.op(Op::StackCheck, &[], &[]);
    f
}
fn poll(f: &mut Fixture) {
    f.op(
        Op::SafepointPoll {
            interval: std::num::NonZeroU32::new(1).unwrap(),
        },
        &[],
        &[],
    );
}
fn compile(f: &Fixture) -> codegen::LeafFragment {
    let fragment = codegen::compile_runtime(&f.body, &bindings()).unwrap();
    for inst in decoded(&fragment) {
        let encoded = encode::encode(&inst).unwrap();
        assert_eq!(decode::decode(encoded.word).unwrap(), inst);
    }
    fragment
}

#[test]
fn checked_entry_retries_after_stack_movement_before_materializing_arguments() {
    let mut f = fixture(&[Type::I64; 9], &[Type::I64]);
    let result = f.op(
        Op::Integer(IntOp::Add),
        &[f.param(0, 0), f.param(0, 8)],
        &[Type::I64],
    )[0];
    f.ret(&[result]);
    let fragment = compile(&f);
    let mut harness = Harness {
        watermark: crate::runtime::coroutine::POLL_SENTINEL as u64,
        growth: [0x10000, 0].into(),
        ..Default::default()
    };
    assert_eq!(
        runtime_vm::run(&fragment, &[1, 2, 3, 4, 5, 6, 7, 8, 9], &mut harness)[0],
        10
    );
    assert_eq!(harness.guard_loads, 3);
    assert_eq!(
        harness.candidates,
        vec![
            (1 << 40) - u64::from(fragment.frame.size),
            (1 << 40) + 0x10000 - u64::from(fragment.frame.size)
        ]
    );
}

#[test]
fn checked_entry_accepts_equality_and_rejects_wrapping_candidate() {
    let mut f = fixture(&[], &[]);
    poll(&mut f);
    f.ret(&[]);
    let fragment = compile(&f);
    let mut fast = Harness {
        watermark: (1 << 40) - u64::from(fragment.frame.size),
        ..Default::default()
    };
    runtime_vm::run(&fragment, &[], &mut fast);
    assert!(fast.candidates.is_empty());
    assert_eq!(fast.guard_loads, 1);
    let mut underflow = Harness {
        expected_sp: 16,
        growth: [0x10000].into(),
        ..Default::default()
    };
    runtime_vm::run(&fragment, &[], &mut underflow);
    assert_eq!(
        underflow.candidates,
        [16_u64.wrapping_sub(u64::from(fragment.frame.size))]
    );
    assert_eq!(underflow.guard_loads, 2);
}

#[test]
fn poll_spills_all_live_gprs_and_uses_resumed_processor_context() {
    let mut f = fixture(&[Type::I64; 12], &[Type::I64]);
    poll(&mut f);
    poll(&mut f);
    let mut result = f.param(0, 0);
    for lane in 1..12 {
        result = f.op(
            Op::Integer(IntOp::Add),
            &[result, f.param(0, lane)],
            &[Type::I64],
        )[0];
    }
    f.ret(&[result]);
    let fragment = compile(&f);
    for lane in 0..12 {
        assert!(matches!(
            fragment.allocation.values[f.param(0, lane).index()].placement,
            Placement::Location(Location::Slot(_))
        ));
    }
    let args = (1..=12).collect::<Vec<_>>();
    let mut slow = Harness {
        flags: 1,
        ..Default::default()
    };
    assert_eq!(runtime_vm::run(&fragment, &args, &mut slow)[0], 78);
    assert_eq!(slow.polls, 1);
    let mut fast = Harness::default();
    assert_eq!(runtime_vm::run(&fragment, &args, &mut fast)[0], 78);
    assert_eq!(fast.polls, 0);
}

#[test]
fn empty_runtime_body_and_exact_return_pc_cfa_diagnostics() {
    let empty = fixture(&[], &[]);
    let fragment = compile(&empty);
    assert_eq!(fragment.frame.size, 0);
    let mut fast = Harness::default();
    runtime_vm::run(&fragment, &[], &mut fast);
    let mut f = fixture(&[], &[]);
    poll(&mut f);
    f.ret(&[]);
    let fragment = compile(&f);
    assert_eq!(fragment.runtime_sites.len(), 2);
    assert!(fragment.frame.size >= 32 && fragment.frame.size.is_multiple_of(16));
    assert_eq!(
        fragment
            .frame
            .saves
            .iter()
            .map(|(reg, _)| *reg)
            .collect::<Vec<_>>(),
        (19..=26).collect::<Vec<_>>()
    );
    let instructions = decoded(&fragment);
    for site in &fragment.runtime_sites {
        assert_eq!(
            table::get(instructions[site.pc_offset as usize / 4 - 1].form)
                .unwrap()
                .key,
            "bl"
        );
        assert_eq!(
            sites::lookup(&fragment.runtime_sites, site.pc_offset),
            Some(site)
        );
        assert!(sites::lookup(&fragment.runtime_sites, site.pc_offset + 1).is_none());
        assert!(sites::lookup(&fragment.runtime_sites, site.pc_offset - 4).is_none());
        let maps =
            crate::runtime::decode_tables(&fragment.metadata.as_ref().unwrap().stackmaps).unwrap();
        assert!(
            maps.maps
                .iter()
                .flat_map(|map| &map.slots)
                .all(|slots| slots.is_empty())
        );
        match site.kind {
            sites::SiteKind::MorestackEntry => {
                assert_eq!(site.cfa_offset, 0);
                assert_eq!(site.caller_lr, sites::CallerLr::Register(10));
            }
            sites::SiteKind::PollResume | sites::SiteKind::CallReturn => {
                assert_eq!(site.cfa_offset, fragment.frame.size);
                assert_eq!(
                    site.caller_lr,
                    sites::CallerLr::StackOffset(fragment.frame.record + 8)
                );
                assert_eq!(site.instruction, Some(1));
            }
        }
    }
}

#[test]
fn large_runtime_entry_materializes_full_candidate_without_writing_frame() {
    let mut f = fixture(&[], &[]);
    f.body.stack_slots.push(StackSlot {
        local: 0,
        bytes: 0x1000000,
        align: 16,
        roots: vec![],
        descriptor: [0; 32],
    });
    f.ret(&[]);
    let fragment = compile(&f);
    assert!(
        decoded(&fragment)
            .iter()
            .any(|i| table::get(i.form).unwrap().key == "sub.X.extend")
    );
    let mut harness = Harness {
        watermark: crate::runtime::coroutine::POLL_SENTINEL as u64,
        growth: [0x2000000].into(),
        ..Default::default()
    };
    runtime_vm::run(&fragment, &[], &mut harness);
    assert_eq!(
        harness.candidates,
        [(1 << 40) - u64::from(fragment.frame.size)]
    );
    assert_eq!(harness.guard_loads, 2);
}

#[test]
fn runtime_site_obligations_remain_strict_and_errors_are_contextual() {
    let mut f = fixture(&[], &[]);
    poll(&mut f);
    assert!(codegen::compile_leaf(&f.body).is_err());
    assert!(
        codegen::compile_function(&f.body)
            .unwrap_err()
            .0
            .contains("requires checked entry / stack maps (4c)")
    );
    let mut malformed = f.body.clone();
    malformed.instructions[1].safepoint = Some(SafepointId(100));
    assert!(
        codegen::compile_runtime(&malformed, &bindings())
            .unwrap_err()
            .0
            .contains("block Some(0) site 1")
    );
    f.op(Op::StackCheck, &[], &[]);
    assert!(
        codegen::compile_runtime(&f.body, &bindings())
            .unwrap_err()
            .0
            .contains("single entry marker")
    );
    let mut f = fixture(&[Type::Ptr], &[]);
    let pointer = f.param(0, 0);
    for provenance in [Provenance::GcHeap, Provenance::Stack] {
        f.body.values[pointer.index()].kind = ValueType::pointer(provenance);
        let error = codegen::compile_runtime(&f.body, &bindings())
            .unwrap_err()
            .0;
        assert!(error.contains("a64_leaf_fixture") && error.contains("4c.2"));
    }
    let mut invalid = bindings();
    invalid.safepoint_slow.clear();
    assert!(codegen::compile_runtime(&Fixture::new(&[], &[]).body, &invalid).is_err());
}

#[test]
fn runtime_metadata_references_and_automatic_nonzero_frame_check() {
    let mut f = Fixture::new(&[], &[]);
    poll(&mut f);
    f.ret(&[]);
    f.body.safepoints.push(Safepoint {
        kind: SafepointKind::Poll,
        block: BlockId(0),
        instruction: Some(InstId(0)),
    });
    f.body.instructions[0].safepoint = Some(SafepointId(0));
    let fragment = compile(&f);
    assert!(
        fragment
            .runtime_sites
            .iter()
            .any(|s| s.kind == sites::SiteKind::MorestackEntry)
    );
    let mut harness = Harness {
        flags: 1,
        ..Default::default()
    };
    runtime_vm::run(&fragment, &[], &mut harness);
    assert_eq!(harness.polls, 1);
    f.body.safepoints[0].kind = SafepointKind::CallReturn;
    assert!(codegen::compile_runtime(&f.body, &bindings()).is_err());
}
