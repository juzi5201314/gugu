use super::{
    runtime_vm::{self, Harness, bindings},
    *,
};

fn fixture(params: &[Type], results: &[Type]) -> Fixture {
    let mut f = Fixture::new(params, results);
    f.body.poll_interface.leaf = false;
    f.body.poll_summary.entry_stack_check = true;
    f.op(Op::StackCheck, &[], &[]);
    f
}
fn slot(f: &mut Fixture, roots: Vec<(u64, Provenance)>, bytes: u64) {
    f.body.stack_slots.push(StackSlot {
        local: 0,
        bytes,
        align: 8,
        roots,
        descriptor: [0; 32],
    });
    f.body.lifetimes.push(Lifetime {
        slot: SlotId(0),
        block: BlockId(0),
        position: 0,
        live: true,
    });
}
fn base(f: &mut Fixture) -> ValueId {
    let value = f.op(Op::StackAddr(SlotId(0)), &[], &[Type::Ptr])[0];
    f.body.values[value.index()].kind = ValueType::pointer(Provenance::Stack);
    value
}
fn access() -> crate::lir::body::Access {
    crate::lir::body::Access {
        alias: AliasClass::Stack(SlotId(0)),
        align: 8,
        volatile: false,
    }
}
fn init(f: &mut Fixture, bit: u32, initialized: bool) {
    f.body.root_inits.push(RootInit {
        slot: SlotId(0),
        block: BlockId(f.block as u32),
        position: f.body.blocks[f.block].instructions.end
            - f.body.blocks[f.block].instructions.start,
        bit,
        initialized,
    });
}
fn poll(f: &mut Fixture) {
    let at = f.body.instructions.len() as u32;
    let id = SafepointId(f.body.safepoints.len() as u32);
    f.op(
        Op::SafepointPoll {
            interval: std::num::NonZeroU32::new(1).unwrap(),
        },
        &[],
        &[],
    );
    f.body.safepoints.push(Safepoint {
        kind: SafepointKind::Poll,
        block: BlockId(f.block as u32),
        instruction: Some(InstId(at)),
    });
    f.body.instructions[at as usize].safepoint = Some(id);
}
fn compile(f: &mut Fixture) -> codegen::LeafFragment {
    f.body.predecessors.clear();
    for (index, block) in f.body.blocks.iter_mut().enumerate() {
        let start = f.body.predecessors.len() as u32;
        f.body.predecessors.extend(
            f.body
                .edges
                .iter()
                .enumerate()
                .filter(|(_, e)| e.to.index() == index)
                .map(|(id, _)| EdgeId(id as u32)),
        );
        block.predecessors = start..f.body.predecessors.len() as u32;
    }
    let fragment = codegen::compile_runtime(&f.body, &bindings()).unwrap();
    for instruction in decoded(&fragment) {
        assert_eq!(
            decode::decode(encode::encode(&instruction).unwrap().word).unwrap(),
            instruction
        );
    }
    fragment
}
fn maps(f: &codegen::LeafFragment) -> crate::runtime::DecodedSection {
    crate::runtime::decode_tables(&f.metadata.as_ref().unwrap().stackmaps).unwrap()
}

#[test]
fn initialized_local_roots_keep_five_classes_and_dead_locals_leave_the_map() {
    let mut f = fixture(&[], &[]);
    let classes = [
        Provenance::GcHeap,
        Provenance::GcInterior,
        Provenance::SharedHandle,
        Provenance::CompressedRef,
        Provenance::Stack,
    ];
    slot(
        &mut f,
        classes
            .into_iter()
            .enumerate()
            .map(|(i, p)| (i as u64 * 8, p))
            .collect(),
        40,
    );
    let base = base(&mut f);
    for (index, provenance) in classes.into_iter().enumerate() {
        let offset = f.constant(Type::I64, index as u64 * 8);
        let at = f.op(Op::PtrOffset, &[base, offset], &[Type::Ptr])[0];
        f.body.values[at.index()].kind = ValueType::pointer(Provenance::Stack);
        let value = f.constant(Type::Ptr, 0x2000 + index as u64 * 8);
        f.body.values[value.index()].kind = ValueType::pointer(provenance);
        f.op(Op::Store(access()), &[at, value], &[]);
        init(&mut f, index as u32, true);
    }
    poll(&mut f);
    let position = f.body.instructions.len() as u32;
    f.body.lifetimes.push(Lifetime {
        slot: SlotId(0),
        block: BlockId(0),
        position,
        live: false,
    });
    for bit in 0..5 {
        init(&mut f, bit, false);
    }
    poll(&mut f);
    f.ret(&[]);
    let fragment = compile(&mut f);
    let decoded = maps(&fragment);
    let polls = decoded
        .safepoints
        .iter()
        .filter(|p| p.kind == 1)
        .collect::<Vec<_>>();
    let live = &decoded.maps[polls[0].map_index as usize];
    for class in 0..5 {
        assert!(live.slots[class].contains(&(fragment.frame.locals[0] / 8 + class as u32)));
    }
    assert!(live.typed.is_empty());
    let dead = &decoded.maps[polls[1].map_index as usize];
    assert!(dead.slots.iter().all(Vec::is_empty) && dead.typed.is_empty());
    assert_eq!(fragment.frame.init_bytes.len(), 5);
    runtime_vm::run(
        &fragment,
        &[],
        &mut Harness {
            flags: 1,
            ..Default::default()
        },
    );
}

#[test]
fn emitted_byte_gates_skip_uninitialized_and_reset_union_variants_in_large_frames() {
    for (gate, bytes) in [
        (None, 8),
        (Some(true), 8),
        (Some(false), 8),
        (Some(true), 8192),
    ] {
        let mut f = fixture(&[], &[Type::I64]);
        slot(
            &mut f,
            vec![(0, Provenance::GcHeap), (0, Provenance::GcInterior)],
            bytes,
        );
        let address = base(&mut f);
        let value = f.constant(Type::I64, 0x2000);
        f.op(Op::Store(access()), &[address, value], &[]);
        if let Some(set) = gate {
            init(&mut f, 0, true);
            if !set {
                init(&mut f, 0, false);
            }
        }
        poll(&mut f);
        let result = f.op(Op::Load(access()), &[address], &[Type::I64]);
        f.ret(&result);
        let fragment = compile(&mut f);
        let decoded = maps(&fragment);
        let point = decoded.safepoints.iter().find(|p| p.kind == 1).unwrap();
        let map = &decoded.maps[point.map_index as usize];
        assert_eq!(map.typed.len(), 2);
        assert_eq!(map.typed[0].word, fragment.frame.locals[0] / 8);
        assert_ne!(map.typed[0].init_byte, map.typed[1].init_byte);
        if bytes > 4095 {
            assert!(map.typed[0].init_byte > 4095);
        }
        let mut harness = Harness {
            flags: 1,
            gc_direct_delta: 0x1000,
            ..Default::default()
        };
        assert_eq!(
            runtime_vm::run(&fragment, &[], &mut harness)[0],
            if gate == Some(true) { 0x3000 } else { 0x2000 }
        );
        assert_eq!(harness.polls, 1);
    }
}

#[test]
fn branch_join_maps_use_the_path_specific_initialization_byte() {
    let mut f = fixture(&[Type::I8], &[Type::I64]);
    slot(&mut f, vec![(0, Provenance::GcHeap)], 8);
    let yes = f.block(&[]);
    let no = f.block(&[]);
    let join = f.block(&[]);
    let address = base(&mut f);
    let scalar = f.constant(Type::I64, 0xbeef);
    f.op(Op::Store(access()), &[address, scalar], &[]);
    let y = f.edge(yes, &[]);
    let n = f.edge(no, &[]);
    f.body.blocks[0].terminator = Terminator::Branch {
        condition: f.param(0, 0),
        yes: y,
        no: n,
    };
    f.switch_to(yes);
    let value = f.constant(Type::I64, 0x2000);
    f.op(Op::Store(access()), &[address, value], &[]);
    init(&mut f, 0, true);
    let edge = f.edge(join, &[]);
    f.body.blocks[yes].terminator = Terminator::Jump(edge);
    f.switch_to(no);
    let edge = f.edge(join, &[]);
    f.body.blocks[no].terminator = Terminator::Jump(edge);
    f.switch_to(join);
    poll(&mut f);
    let result = f.op(Op::Load(access()), &[address], &[Type::I64]);
    f.ret(&result);
    let fragment = compile(&mut f);
    let decoded = maps(&fragment);
    let point = decoded.safepoints.iter().find(|p| p.kind == 1).unwrap();
    assert_eq!(decoded.maps[point.map_index as usize].typed.len(), 1);
    for (flag, result) in [(1, 0x3000), (0, 0xbeef)] {
        let mut harness = Harness {
            flags: 1,
            gc_direct_delta: 0x1000,
            ..Default::default()
        };
        assert_eq!(runtime_vm::run(&fragment, &[flag], &mut harness)[0], result);
    }
}

#[test]
fn non_entry_future_set_does_not_initialize_an_earlier_poll() {
    let mut f = fixture(&[], &[Type::I64, Type::I64]);
    slot(&mut f, vec![(0, Provenance::GcHeap)], 8);
    let next = f.block(&[]);
    let address = base(&mut f);
    let scalar = f.constant(Type::I64, 0xbeef);
    f.op(Op::Store(access()), &[address, scalar], &[]);
    let edge = f.edge(next, &[]);
    f.body.blocks[0].terminator = Terminator::Jump(edge);
    f.switch_to(next);
    poll(&mut f);
    let first = f.op(Op::Load(access()), &[address], &[Type::I64])[0];
    let pointer = f.constant(Type::I64, 0x2000);
    f.op(Op::Store(access()), &[address, pointer], &[]);
    init(&mut f, 0, true);
    poll(&mut f);
    let last = f.op(Op::Load(access()), &[address], &[Type::I64])[0];
    f.ret(&[first, last]);
    let fragment = compile(&mut f);
    let mut harness = Harness {
        flags: 1,
        gc_direct_delta: 0x1000,
        repeat_polls: true,
        ..Default::default()
    };
    assert_eq!(
        runtime_vm::run(&fragment, &[], &mut harness),
        [0xbeef, 0x3000]
    );
    assert_eq!(harness.polls, 2);
    let maps = maps(&fragment);
    let polls = maps
        .safepoints
        .iter()
        .filter(|p| p.kind == 1)
        .collect::<Vec<_>>();
    assert_eq!(maps.maps[polls[0].map_index as usize].typed.len(), 1);
    assert!(maps.maps[polls[1].map_index as usize].typed.is_empty());
}

#[test]
fn typed_only_stack_roots_set_the_function_stack_interior_flag() {
    let mut f = fixture(&[], &[]);
    slot(&mut f, vec![(0, Provenance::Stack)], 8);
    poll(&mut f);
    f.ret(&[]);
    let fragment = compile(&mut f);
    let maps = maps(&fragment);
    let point = maps
        .safepoints
        .iter()
        .find(|point| point.kind == 1)
        .unwrap();
    let map = &maps.maps[point.map_index as usize];
    assert!(map.registers.iter().all(|mask| *mask == 0));
    assert!(map.slots.iter().all(Vec::is_empty));
    assert_eq!(map.typed.len(), 1);
    assert_eq!(map.typed[0].root_kind, 4);
    assert_ne!(maps.functions[0].flags & 4, 0);
}

#[test]
fn entry_header_backedges_keep_gates_but_do_not_prove_first_entry_initialized() {
    let mut f = fixture(&[Type::I64], &[Type::I64]);
    slot(&mut f, vec![(0, Provenance::GcHeap)], 8);
    let done = f.block(&[Type::I64]);
    let address = base(&mut f);
    poll(&mut f);
    let result = f.op(Op::Load(access()), &[address], &[Type::I64])[0];
    let value = f.constant(Type::I64, 0x2000);
    f.op(Op::Store(access()), &[address, value], &[]);
    init(&mut f, 0, true);
    let one = f.constant(Type::I64, 1);
    let next = f.op(Op::Integer(IntOp::Sub), &[f.param(0, 0), one], &[Type::I64])[0];
    let zero = f.constant(Type::I64, 0);
    let condition = f.op(
        Op::Compare {
            condition: Condition::Ne,
            signed: false,
        },
        &[next, zero],
        &[Type::I8],
    )[0];
    let repeat = f.edge(0, &[next]);
    let exit = f.edge(done, &[result]);
    f.body.blocks[0].terminator = Terminator::Branch {
        condition,
        yes: repeat,
        no: exit,
    };
    f.switch_to(done);
    f.ret(&[f.param(done, 0)]);
    let fragment = compile(&mut f);
    let decoded = maps(&fragment);
    let point = decoded.safepoints.iter().find(|p| p.kind == 1).unwrap();
    assert_eq!(decoded.maps[point.map_index as usize].typed.len(), 1);
    let mut harness = Harness {
        flags: 1,
        gc_direct_delta: 0x1000,
        repeat_polls: true,
        ..Default::default()
    };
    assert_eq!(runtime_vm::run(&fragment, &[2], &mut harness)[0], 0x3000);
    assert_eq!(harness.polls, 2);
}

#[test]
fn empty_local_frames_and_malformed_root_tables_events_and_addresses() {
    let mut f = fixture(&[], &[]);
    f.ret(&[]);
    let empty = compile(&mut f);
    assert!(empty.frame.init_bytes.is_empty() && empty.frame.init_bases.is_empty());
    slot(&mut f, vec![(0, Provenance::GcHeap)], 8);
    init(&mut f, 0, true);
    for case in 0..9 {
        let mut malformed = f.body.clone();
        match case {
            0 => malformed.stack_slots[0].roots[0].0 = 1,
            1 => malformed.stack_slots[0].roots[0].1 = Provenance::Raw,
            2 => malformed.stack_slots[0].roots.push((0, Provenance::GcHeap)),
            3 => malformed.root_inits[0].bit = 1,
            4 => malformed.root_inits[0].position = u32::MAX,
            5 => malformed.root_inits[0].block = BlockId(99),
            6 => malformed.root_inits[0].slot = SlotId(99),
            7 => malformed.lifetimes[0].position = u32::MAX,
            _ => malformed.stack_slots[0].align = 4,
        }
        assert!(
            codegen::compile_runtime(&malformed, &bindings()).is_err(),
            "case {case}"
        );
    }
    let value = base(&mut f);
    f.body.values[value.index()].kind = ValueType::pointer(Provenance::Raw);
    assert!(
        codegen::compile_runtime(&f.body, &bindings())
            .unwrap_err()
            .0
            .contains("Stack provenance")
    );
}

#[test]
fn rooted_locals_appear_in_managed_call_return_maps() {
    let mut callee = Fixture::new(&[], &[]);
    callee.body.instance = [2; 32];
    callee.ret(&[]);
    let mut f = fixture(&[], &[Type::I64]);
    slot(&mut f, vec![(0, Provenance::GcHeap)], 8);
    let address = base(&mut f);
    let value = f.constant(Type::I64, 0x2000);
    f.op(Op::Store(access()), &[address, value], &[]);
    init(&mut f, 0, true);
    let at = f.body.instructions.len() as u32;
    f.op(
        Op::Call(Call {
            target: CallTarget::Instance(callee.body.instance),
            kind: crate::gir::body::CallKind::Managed,
            parameters: vec![],
            results: vec![],
            sret: None,
            by_value: vec![],
            foreign_arguments: vec![],
            may_allocate: true,
            may_suspend: false,
            may_unwind: false,
            captures_arguments: false,
            poll_free_leaf: false,
        }),
        &[],
        &[],
    );
    let id = SafepointId(f.body.safepoints.len() as u32);
    f.body.safepoints.push(Safepoint {
        kind: SafepointKind::CallReturn,
        block: BlockId(0),
        instruction: Some(InstId(at)),
    });
    f.body.instructions[at as usize].safepoint = Some(id);
    let result = f.op(Op::Load(access()), &[address], &[Type::I64]);
    f.ret(&result);
    let fragment = compile(&mut f);
    let decoded = maps(&fragment);
    let point = decoded.safepoints.iter().find(|p| p.kind == 0).unwrap();
    assert!(
        decoded.maps[point.map_index as usize].slots[0].contains(&(fragment.frame.locals[0] / 8))
    );
    let callee = codegen::compile_runtime(&callee.body, &bindings()).unwrap();
    assert_eq!(
        runtime_vm::run_linked(&fragment, &[&callee], &[], &mut Harness::default())[0],
        0x2000
    );
}

#[test]
fn far_initialization_bytes_use_scratch_safe_extended_sp_addressing() {
    let mut f = fixture(&[], &[]);
    slot(
        &mut f,
        vec![(0, Provenance::GcHeap), (0, Provenance::GcInterior)],
        (1 << 24) + 16,
    );
    let address = base(&mut f);
    let value = f.constant(Type::I64, 0x2000);
    f.op(Op::Store(access()), &[address, value], &[]);
    init(&mut f, 0, true);
    f.ret(&[]);
    let fragment = compile(&mut f);
    assert!(fragment.frame.init_bytes.start > 0xff_ffff);
    let mut stub = Fixture::new(&[], &[]).compile();
    stub.symbol = bindings().morestack_or_poll;
    let mut harness = Harness::default();
    let (_, memory) = vm::execute_inner(&fragment, &[&stub], &[], Some(&mut harness));
    let base = harness.expected_sp - u64::from(fragment.frame.size);
    assert_eq!(
        memory.get(&(base + u64::from(fragment.frame.init_bytes.start))),
        Some(&1)
    );
    assert_eq!(
        memory.get(&(base + u64::from(fragment.frame.init_bytes.start) + 1)),
        Some(&0)
    );
    assert!(memory.len() < 256);
}
