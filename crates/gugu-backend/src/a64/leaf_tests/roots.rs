use super::{
    runtime_vm::{self, Harness, bindings},
    *,
};
use crate::{
    gir::body::CallKind,
    regalloc::{Location, Placement, RootClass},
    runtime::{stackmap_codec::a64_unwind, stackmap_schema::StackMapArchitecture},
};

fn root(f: &mut Fixture, value: ValueId, provenance: Provenance) {
    f.body.values[value.index()].kind = ValueType::pointer(provenance);
    if let Definition::Parameter { index, .. } = f.body.values[value.index()].definition {
        f.body.signature.parameters[index as usize - 1] = ValueType::pointer(provenance);
    }
}
fn fixture(params: &[Type], results: &[Type]) -> Fixture {
    let mut f = Fixture::new(params, results);
    f.body.poll_interface.leaf = false;
    f.body.poll_summary.entry_stack_check = true;
    f.op(Op::StackCheck, &[], &[]);
    f
}
fn mark(f: &mut Fixture, instruction: usize, kind: SafepointKind) {
    let id = SafepointId(f.body.safepoints.len() as u32);
    f.body.safepoints.push(Safepoint {
        kind,
        block: BlockId(0),
        instruction: Some(InstId(instruction as u32)),
    });
    f.body.instructions[instruction].safepoint = Some(id);
}
fn poll(f: &mut Fixture) {
    let at = f.body.instructions.len();
    f.op(
        Op::SafepointPoll {
            interval: std::num::NonZeroU32::new(1).unwrap(),
        },
        &[],
        &[],
    );
    mark(f, at, SafepointKind::Poll);
}
fn compile(f: &Fixture) -> codegen::LeafFragment {
    codegen::compile_runtime(&f.body, &bindings()).unwrap()
}
fn maps(fragment: &codegen::LeafFragment) -> crate::runtime::DecodedSection {
    let metadata = fragment.metadata.as_ref().unwrap();
    let maps = crate::runtime::decode_tables(&metadata.stackmaps).unwrap();
    assert_eq!(maps.architecture, StackMapArchitecture::Aarch64);
    a64_unwind::decode(&metadata.unwind)
        .unwrap()
        .verify_joint(&maps)
        .unwrap();
    maps
}
fn call(callee: &Fixture) -> Call {
    Call {
        target: CallTarget::Instance(callee.body.instance),
        kind: CallKind::Managed,
        parameters: callee.body.signature.parameters.clone(),
        results: callee.body.signature.results.clone(),
        may_unwind: false,
        may_suspend: false,
        may_allocate: true,
        captures_arguments: false,
        by_value: vec![],
        foreign_arguments: vec![],
        sret: None,
        poll_free_leaf: false,
    }
}

#[test]
fn mixed_fp_and_pointer_arguments_share_abi_placement_with_all_root_maps() {
    let mut params = vec![Type::F64; 9];
    params.extend([Type::Ptr; 9]);
    let mut callee = fixture(&params, &[]);
    callee.body.instance = [2; 32];
    for index in 9..18 {
        let p = callee.param(0, index);
        root(&mut callee, p, Provenance::GcHeap);
    }
    callee.ret(&[]);
    for suspend in [false, true] {
        let mut f = fixture(&params, &[]);
        f.body.instance = [1; 32];
        for index in 9..18 {
            let p = f.param(0, index);
            root(&mut f, p, Provenance::GcHeap);
        }
        let args = (0..18).map(|index| f.param(0, index)).collect::<Vec<_>>();
        let at = f.body.instructions.len();
        let mut call = call(&callee);
        call.may_suspend = suspend;
        f.op(Op::Call(call), &args, &[]);
        mark(
            &mut f,
            at,
            if suspend {
                SafepointKind::Suspend
            } else {
                SafepointKind::CallReturn
            },
        );
        for p in &args[9..] {
            f.op(
                Op::Load(crate::lir::body::Access {
                    alias: AliasClass::Heap,
                    align: 8,
                    volatile: false,
                }),
                &[*p],
                &[Type::I64],
            );
        }
        f.ret(&[]);
        let fragment = compile(&f);
        assert_eq!(fragment.frame.outgoing, 16);
        let decoded = maps(&fragment);
        let entry = decoded.safepoints.iter().find(|p| p.kind == 4).unwrap();
        assert_eq!(
            decoded.maps[entry.map_index as usize].registers,
            [0xff, 0, 0, 0, 0]
        );
        let point = decoded
            .safepoints
            .iter()
            .find(|p| p.kind == if suspend { 2 } else { 0 })
            .unwrap();
        let map = &decoded.maps[point.map_index as usize];
        assert!(map.slots[0].contains(&1));
        assert!(!map.slots[0].contains(&0));
        assert_eq!(map.slots[0].len(), 10);
        assert_eq!(map.registers, [0; 5]);
    }
}

#[test]
fn suspending_calls_use_authoritative_suspend_maps_and_forced_pointer_slots() {
    let mut callee = fixture(&[Type::Ptr; 9], &[]);
    callee.body.instance = [2; 32];
    for index in 0..9 {
        let parameter = callee.param(0, index);
        root(&mut callee, parameter, Provenance::Stack);
    }
    callee.ret(&[]);
    let mut f = fixture(&[Type::Ptr], &[Type::Ptr]);
    let p = f.param(0, 0);
    root(&mut f, p, Provenance::Stack);
    f.body.signature.results[0] = ValueType::pointer(Provenance::Stack);
    let mut suspend = call(&callee);
    suspend.may_suspend = true;
    let at = f.body.instructions.len();
    f.op(Op::Call(suspend), &[p; 9], &[]);
    mark(&mut f, at, SafepointKind::Suspend);
    f.ret(&[p]);
    let fragment = compile(&f);
    let decoded = maps(&fragment);
    let metadata = fragment.metadata.as_ref().unwrap();
    crate::runtime::stack::a64::FrameMetadata::load(&metadata.stackmaps, &metadata.unwind, 0x1000)
        .unwrap();
    let point = decoded
        .safepoints
        .iter()
        .find(|point| point.kind == 2)
        .unwrap();
    let map = &decoded.maps[point.map_index as usize];
    assert!(!map.slots[4].is_empty());
    assert!(map.slots[4].contains(&0));
    assert!(map.slots[4].len() >= 2);
    assert_eq!(map.registers, [0; 5]);
    assert!(
        fragment
            .runtime_sites
            .iter()
            .any(|site| site.kind == sites::SiteKind::SuspendResume)
    );
    let callee = codegen::compile_runtime(&callee.body, &bindings()).unwrap();
    let mut harness = Harness::default();
    assert_eq!(
        runtime_vm::run_linked(&fragment, &[&callee], &[0x8000], &mut harness)[0],
        0x8000
    );
    assert_eq!(harness.guard_loads, 2);
    let mut bad = f.body.clone();
    bad.safepoints[0].kind = SafepointKind::CallReturn;
    assert!(codegen::compile_runtime(&bad, &bindings()).is_err());
    let mut bad = f.body.clone();
    if let Op::Call(call) = &mut bad.instructions[at].op {
        call.poll_free_leaf = true;
    }
    assert!(codegen::compile_runtime(&bad, &bindings()).is_err());
    let mut empty = fixture(&[], &[]);
    let mut suspend = call(&Fixture::new(&[], &[]));
    suspend.may_suspend = true;
    empty.op(Op::Call(suspend), &[], &[]);
    mark(&mut empty, 1, SafepointKind::Suspend);
    empty.ret(&[]);
    let maps = maps(&compile(&empty));
    let point = maps
        .safepoints
        .iter()
        .find(|point| point.kind == 2)
        .unwrap();
    assert!(
        maps.maps[point.map_index as usize]
            .slots
            .iter()
            .all(Vec::is_empty)
    );
}

#[test]
fn poll_roots_use_shared_pointer_spills_and_reload_after_modeled_gc() {
    let mut f = fixture(&[Type::Ptr], &[Type::Ptr]);
    let p = f.param(0, 0);
    root(&mut f, p, Provenance::GcHeap);
    f.body.signature.results[0] = ValueType::pointer(Provenance::GcHeap);
    poll(&mut f);
    f.ret(&[p]);
    let fragment = compile(&f);
    let allocated = &fragment.allocation.values[p.index()];
    assert_eq!(allocated.root, RootClass::HeapPointer);
    let Placement::Location(Location::Slot(slot)) = allocated.placement else {
        panic!("root must spill")
    };
    let maps = maps(&fragment);
    let point = maps.safepoints.iter().find(|p| p.kind == 1).unwrap();
    assert_eq!(
        maps.maps[point.map_index as usize].slots[0],
        [fragment.frame.spills[slot as usize] / 8]
    );
    let mut harness = Harness {
        flags: 1,
        gc_direct_delta: 0x1000,
        ..Default::default()
    };
    assert_eq!(
        runtime_vm::run(&fragment, &[0x2000], &mut harness)[0],
        0x3000
    );
    assert_eq!(harness.polls, 1);
    assert!(codegen::compile_function(&f.body).is_err());
}

#[test]
fn entry_and_poll_maps_preserve_all_five_provenance_classes() {
    let mut f = fixture(&[Type::Ptr; 5], &[Type::Ptr; 2]);
    let classes = [
        Provenance::GcHeap,
        Provenance::GcInterior,
        Provenance::SharedHandle,
        Provenance::CompressedRef,
        Provenance::Stack,
    ];
    for (index, class) in classes.into_iter().enumerate() {
        let p = f.param(0, index);
        root(&mut f, p, class);
    }
    poll(&mut f);
    for index in 0..5 {
        let p = f.param(0, index);
        f.op(
            Op::Load(crate::lir::body::Access {
                alias: AliasClass::Heap,
                align: 8,
                volatile: false,
            }),
            &[p],
            &[Type::I64],
        );
    }
    f.ret(&[]);
    f.body.signature.results.clear();
    let fragment = compile(&f);
    let maps = maps(&fragment);
    let entry = maps.safepoints.iter().find(|p| p.kind == 4).unwrap();
    assert_eq!(
        maps.maps[entry.map_index as usize].registers,
        [1, 2, 4, 8, 16]
    );
    let poll = maps.safepoints.iter().find(|p| p.kind == 1).unwrap();
    let roots = &maps.maps[poll.map_index as usize];
    for class in 0..5 {
        assert_eq!(roots.slots[class].len(), 1);
        assert_eq!(roots.registers[class], 0);
    }
    assert_eq!(
        fragment.allocation.values[f.param(0, 4).index()].root,
        RootClass::StackPointer
    );
}

#[test]
fn managed_calls_map_stack_arguments_and_exclude_unmaterialized_results() {
    let mut callee = Fixture::new(&[Type::Ptr; 10], &[Type::Ptr]);
    callee.body.instance = [2; 32];
    for i in 0..10 {
        let p = callee.param(0, i);
        root(&mut callee, p, Provenance::GcHeap);
    }
    callee.body.signature.results[0] = ValueType::pointer(Provenance::GcHeap);
    callee.ret(&[callee.param(0, 9)]);
    let callee_fragment = compile(&callee);
    assert_eq!(callee_fragment.frame.size, 0);
    let mut f = fixture(&[Type::Ptr; 10], &[Type::Ptr]);
    f.body.instance = [1; 32];
    for i in 0..10 {
        let p = f.param(0, i);
        root(&mut f, p, Provenance::GcHeap);
    }
    f.body.signature.results[0] = ValueType::pointer(Provenance::GcHeap);
    let args = (0..10).map(|i| f.param(0, i)).collect::<Vec<_>>();
    let site = f.body.instructions.len();
    let result = f.op(Op::Call(call(&callee)), &args, &[Type::Ptr])[0];
    f.body.values[result.index()].kind = ValueType::pointer(Provenance::GcHeap);
    mark(&mut f, site, SafepointKind::CallReturn);
    for argument in &args {
        f.op(
            Op::Load(crate::lir::body::Access {
                alias: AliasClass::Heap,
                align: 8,
                volatile: false,
            }),
            &[*argument],
            &[Type::I64],
        );
    }
    f.ret(&[result]);
    let fragment = compile(&f);
    let maps = maps(&fragment);
    let point = maps.safepoints.iter().find(|p| p.kind == 0).unwrap();
    let roots = &maps.maps[point.map_index as usize];
    assert!(roots.slots[0].contains(&0) && roots.slots[0].contains(&1));
    assert_eq!(roots.slots[0].len(), 12);
    let Placement::Location(Location::Slot(result_slot)) =
        fragment.allocation.values[result.index()].placement
    else {
        panic!("result spill")
    };
    assert!(!roots.slots[0].contains(&(fragment.frame.spills[result_slot as usize] / 8)));
    let decoded = decoded(&fragment);
    assert_eq!(
        table::get(decoded[point.pc_offset as usize / 4 - 1].form)
            .unwrap()
            .key,
        "bl"
    );
    let args = (1..=10).map(|v| v * 0x1000).collect::<Vec<_>>();
    assert_eq!(
        runtime_vm::run_linked(
            &fragment,
            &[&callee_fragment],
            &args,
            &mut Harness::default()
        )[0],
        0xa000
    );
}

#[test]
fn scalar_stack_roots_feed_existing_stack_copy_fixups() {
    let mut f = fixture(&[Type::Ptr], &[Type::Ptr]);
    let p = f.param(0, 0);
    root(&mut f, p, Provenance::Stack);
    f.body.signature.results[0] = ValueType::pointer(Provenance::Stack);
    poll(&mut f);
    f.ret(&[p]);
    let fragment = compile(&f);
    let maps = maps(&fragment);
    let point = maps.safepoints.iter().find(|p| p.kind == 1).unwrap();
    let slot = maps.maps[point.map_index as usize].slots[4][0];
    let image = crate::runtime::stackmap::copy_input(
        &[crate::runtime::stackmap::ScannedRoot::Stack(slot)],
        fragment.frame.size as usize,
    )
    .unwrap();
    assert_eq!(image.stack_roots, [slot * 8]);
}

#[test]
fn managed_map_admission_rejects_missing_maps_and_suspension() {
    let mut callee = Fixture::new(&[], &[]);
    callee.body.instance = [2; 32];
    callee.ret(&[]);
    let mut f = fixture(&[], &[]);
    f.op(Op::Call(call(&callee)), &[], &[]);
    f.ret(&[]);
    assert!(
        codegen::compile_runtime(&f.body, &bindings())
            .unwrap_err()
            .0
            .contains("authoritative CallReturn")
    );
    mark(&mut f, 1, SafepointKind::CallReturn);
    let empty = compile(&f);
    let empty_maps = maps(&empty);
    let call_map = empty_maps.safepoints.iter().find(|p| p.kind == 0).unwrap();
    assert!(
        empty_maps.maps[call_map.map_index as usize]
            .slots
            .iter()
            .all(Vec::is_empty)
    );
    let mut suspended = f.body.clone();
    if let Op::Call(call) = &mut suspended.instructions[1].op {
        call.may_suspend = true;
    }
    assert!(
        codegen::compile_runtime(&suspended, &bindings())
            .unwrap_err()
            .0
            .contains("invalid runtime safepoint table reference")
    );
    let mut invalid = f.body.clone();
    invalid.safepoints[0].kind = SafepointKind::Poll;
    assert!(codegen::compile_runtime(&invalid, &bindings()).is_err());
    let mut root_slot = f.body.clone();
    root_slot.stack_slots.push(StackSlot {
        local: 0,
        bytes: 8,
        align: 8,
        roots: vec![(0, Provenance::GcHeap)],
        descriptor: [0; 32],
    });
    assert!(codegen::compile_runtime(&root_slot, &bindings()).is_ok());
}
