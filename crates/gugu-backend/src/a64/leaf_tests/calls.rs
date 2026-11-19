use super::*;
use crate::gir::body::CallKind;

fn access(alias: AliasClass) -> crate::lir::body::Access {
    crate::lir::body::Access {
        alias,
        align: 8,
        volatile: false,
    }
}

fn direct(callee: &Fixture) -> Call {
    assert!(callee.body.poll_interface.leaf);
    assert!(callee.body.signature.sret.is_none());
    assert!(callee.body.stack_slots.is_empty());
    Call {
        target: CallTarget::Instance(callee.body.instance),
        kind: CallKind::Managed,
        parameters: callee.body.signature.parameters.clone(),
        results: callee.body.signature.results.clone(),
        may_unwind: false,
        may_suspend: false,
        may_allocate: false,
        captures_arguments: false,
        by_value: vec![],
        foreign_arguments: vec![],
        sret: None,
        poll_free_leaf: true,
    }
}

fn caller(params: &[Type], results: &[Type]) -> Fixture {
    let mut f = Fixture::new(params, results);
    f.body.instance = [1; 32];
    f.body.poll_interface.leaf = false;
    f
}

fn compile(f: &Fixture) -> codegen::LeafFragment {
    codegen::compile_function(&f.body).unwrap()
}

fn reassemble(fragment: &mut codegen::LeafFragment) {
    let items =
        rewrite::rewrite(&fragment.selected, &fragment.allocation, &fragment.frame).unwrap();
    let (bytes, layout, relocations) = crate::a64::layout::assemble_function(&items).unwrap();
    fragment.bytes = bytes;
    fragment.layout = layout;
    fragment.relocations = relocations;
}

#[test]
fn parallel_call_register_cycles_are_simultaneous() {
    use crate::regalloc::{Location, PhysReg, Placement};
    let mut callee = Fixture::new(&[Type::I64; 2], &[Type::I64; 2]);
    callee.ret(&[callee.param(0, 0), callee.param(0, 1)]);
    let mut f = caller(&[Type::I64; 2], &[Type::I64; 2]);
    let results = f.op(
        Op::Call(direct(&callee)),
        &[f.param(0, 1), f.param(0, 0)],
        &[Type::I64; 2],
    );
    f.ret(&results);
    let mut fragment = compile(&f);
    for index in 0..2 {
        fragment.allocation.values[f.param(0, index).index()].placement =
            Placement::Location(Location::Reg(PhysReg(index as u8)));
    }
    reassemble(&mut fragment);
    assert_eq!(
        vm::run_linked(&fragment, &[&callee.compile()], &[3, 7]),
        [7, 3]
    );
}

#[test]
fn large_slot_copy_cycles_do_not_clobber_address_scratch() {
    use crate::regalloc::{Location, Placement};
    let mut f = Fixture::new(&[Type::I64; 2], &[Type::I64; 2]);
    let a = f.param(0, 0);
    let b = f.param(0, 1);
    f.ret(&[a, b]);
    let mut fragment = f.compile();
    let base = (1 << 24) + 16;
    fragment.frame = frame::Frame {
        size: base + 64,
        required_frame: base + 64,
        max_leaf_reserve: 0,
        unchecked_leaf: false,
        outgoing: base,
        record: base,
        copy_scratch: base + 16,
        saves: vec![],
        spills: vec![base + 32, base + 40],
        locals: vec![],
        init_bases: vec![],
        init_bytes: 0..0,
    };
    for (index, value) in [a, b].iter().enumerate() {
        fragment.allocation.values[value.index()].placement =
            Placement::Location(Location::Slot(index as u32));
    }
    fragment.selected.blocks[1].items.insert(
        0,
        machine::Site {
            context: "synthetic large-slot cycle".into(),
            item: machine::Item::Copies(vec![
                (machine::MReg::Virtual(a.0), machine::MReg::Virtual(b.0)),
                (machine::MReg::Virtual(b.0), machine::MReg::Virtual(a.0)),
            ]),
        },
    );
    reassemble(&mut fragment);
    assert_eq!(vm::run(&fragment, &[3, 7]), [7, 3]);
}

#[test]
fn hidden_pointer_multiword_returns_use_lir_storage_not_extra_result_registers() {
    let mut f = caller(&[Type::Ptr], &[]);
    f.body.signature.sret = Some((24, 8, [7; 32]));
    let pointer = f.param(0, 0);
    for (index, bits) in [11, 22, 33].into_iter().enumerate() {
        let displacement = f.constant(Type::I64, index as u64 * 8);
        let at = f.op(Op::PtrOffset, &[pointer, displacement], &[Type::Ptr])[0];
        let value = f.constant(Type::I64, bits);
        f.op(Op::Store(access(AliasClass::Foreign)), &[at, value], &[]);
    }
    f.ret(&[]);
    let fragment = compile(&f);
    assert!(fragment.relocations.is_empty());
    let (_, memory) = vm::execute(&fragment, &[], &[4096]);
    for (index, expected) in [11, 22, 33].into_iter().enumerate() {
        let value = (0..8).fold(0_u64, |value, byte| {
            value | u64::from(memory[&(4096 + index as u64 * 8 + byte)]) << (byte * 8)
        });
        assert_eq!(value, expected);
    }
}

#[test]
fn direct_call_stack_arguments_two_word_results_and_live_spills() {
    let mut callee = Fixture::new(&[Type::I64; 10], &[Type::I64; 2]);
    let first = callee.param(0, 0);
    let ninth = callee.param(0, 8);
    let tenth = callee.param(0, 9);
    let result = callee.op(Op::Integer(IntOp::Sub), &[ninth, first], &[Type::I64])[0];
    callee.ret(&[result, tenth]);
    let mut f = caller(&[Type::I64; 12], &[Type::I64; 2]);
    let args = (0..10)
        .rev()
        .map(|index| f.param(0, index))
        .collect::<Vec<_>>();
    let result = f.op(Op::Call(direct(&callee)), &args, &[Type::I64; 2]);
    let mut sum = result[0];
    for index in 0..12 {
        sum = f.op(
            Op::Integer(IntOp::Add),
            &[sum, f.param(0, index)],
            &[Type::I64],
        )[0];
    }
    f.ret(&[sum, result[1]]);
    let fragment = compile(&f);
    let target = callee.compile();
    assert_eq!(fragment.frame.outgoing, 16);
    assert_eq!(fragment.frame.record, 16);
    assert!(!fragment.frame.saves.is_empty());
    assert!(!fragment.frame.spills.is_empty());
    assert_eq!(fragment.allocation.stats.call_sites, 1);
    assert_eq!(fragment.relocations.len(), 1);
    let fixup = &fragment.relocations[0];
    assert_eq!(fixup.relocation.kind, inst::RelocKind::Call26);
    assert_eq!(fixup.relocation.target, target.symbol);
    assert_eq!(fixup.relocation.addend, 0);
    assert_eq!(
        decoded(&fragment)[fixup.offset as usize / 4]
            .text()
            .unwrap(),
        "bl #0"
    );
    assert_eq!(
        vm::run_linked(&fragment, &[&target], &(1..=12).collect::<Vec<_>>()),
        [70, 1]
    );
    for index in 0..12 {
        if let Some(crate::regalloc::Location::Reg(reg)) =
            fragment.allocation.values[f.param(0, index).index()].location()
        {
            assert!(abi::CALLEE_SAVED.contains(&reg.0));
        }
    }
}

#[test]
fn odd_stack_argument_count_and_narrow_results_are_aligned() {
    let mut parameters = [Type::I64; 9];
    parameters[8] = Type::I8;
    let mut callee = Fixture::new(&parameters, &[Type::I8]);
    callee.ret(&[callee.param(0, 8)]);
    let mut f = caller(&parameters, &[Type::I8]);
    let args = (0..9).map(|index| f.param(0, index)).collect::<Vec<_>>();
    let results = f.op(Op::Call(direct(&callee)), &args, &[Type::I8]);
    f.ret(&results);
    let fragment = compile(&f);
    assert_eq!(fragment.frame.outgoing, 16);
    assert_eq!(
        vm::run_linked(
            &fragment,
            &[&callee.compile()],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0x123]
        )[0],
        0x23
    );
}

#[test]
fn empty_calls_force_link_record_and_unresolved_symbols_are_errors() {
    let mut callee = Fixture::new(&[], &[]);
    callee.ret(&[]);
    let mut f = caller(&[], &[]);
    f.op(Op::Call(direct(&callee)), &[], &[]);
    f.op(Op::Call(direct(&callee)), &[], &[]);
    f.ret(&[]);
    let fragment = compile(&f);
    assert!(fragment.frame.size >= 16);
    assert!(codegen::compile_leaf(&f.body).is_err());
    assert_eq!(
        vm::link(&[&fragment]).unwrap_err(),
        "unknown fragment symbol"
    );
    assert_eq!(vm::run_linked(&fragment, &[&callee.compile()], &[]), [0, 0]);
    assert!(vm::link(&[&fragment, &fragment]).is_err());
    f.body.poll_interface.leaf = true;
    assert!(
        codegen::compile_leaf(&f.body)
            .unwrap_err()
            .0
            .contains("compile_leaf rejects calls")
    );
}

#[test]
fn tail_calls_reuse_incoming_stack_area_and_restore_saves() {
    let mut callee = Fixture::new(&[Type::I64; 10], &[Type::I64; 2]);
    callee.ret(&[callee.param(0, 8), callee.param(0, 0)]);
    let mut f = caller(&[Type::I64; 10], &[Type::I64; 2]);
    let args = (0..10)
        .rev()
        .map(|index| f.param(0, index))
        .collect::<Vec<_>>();
    let arguments = f.args(&args);
    f.body.blocks[0].terminator = Terminator::TailCall {
        call: direct(&callee),
        arguments,
        memory: f.memory,
    };
    let fragment = compile(&f);
    assert_eq!(fragment.frame.outgoing, 0);
    assert_eq!(
        fragment.relocations[0].relocation.kind,
        inst::RelocKind::Branch26
    );
    assert!(
        decoded(&fragment)
            .iter()
            .all(|inst| table::get(inst.form).unwrap().key != "ret")
    );
    assert_eq!(
        vm::run_linked(
            &fragment,
            &[&callee.compile()],
            &(1..=10).collect::<Vec<_>>()
        ),
        [2, 10]
    );
    let call = direct(&callee);
    let mut small = caller(&[Type::I64; 8], &[Type::I64; 2]);
    let mut args = (0..8)
        .map(|index| small.param(0, index))
        .collect::<Vec<_>>();
    args.extend([args[0], args[1]]);
    let arguments = small.args(&args);
    small.body.blocks[0].terminator = Terminator::TailCall {
        call,
        arguments,
        memory: small.memory,
    };
    assert!(
        codegen::compile_function(&small.body)
            .unwrap_err()
            .0
            .contains("reusable incoming area")
    );
}

#[test]
fn unsafe_call_and_checked_entry_rejections_are_contextual() {
    let mut callee = Fixture::new(&[], &[]);
    callee.ret(&[]);
    let safe = direct(&callee);
    for field in 0..5 {
        let mut call = safe.clone();
        match field {
            0 => call.poll_free_leaf = false,
            1 => call.may_unwind = true,
            2 => call.may_suspend = true,
            3 => call.may_allocate = true,
            _ => {
                call.sret = Some((0, 24, [0; 32]));
                call.poll_free_leaf = false;
            }
        }
        let mut f = caller(&[], &[]);
        f.op(Op::Call(call), &[], &[]);
        f.ret(&[]);
        let error = codegen::compile_function(&f.body).unwrap_err().0;
        assert!(error.contains("site 0"), "{error}");
        assert!(
            error.contains("requires checked entry / stack maps (4c)"),
            "{error}"
        );
    }
    for op in [
        Op::StackCheck,
        Op::SafepointPoll {
            interval: std::num::NonZeroU32::new(1).unwrap(),
        },
    ] {
        let mut f = caller(&[], &[]);
        f.op(op, &[], &[]);
        f.ret(&[]);
        let error = codegen::compile_function(&f.body).unwrap_err().0;
        assert!(
            error.contains("site 0") && error.contains("requires checked entry / stack maps (4c)"),
            "{error}"
        );
    }
    for target in [
        CallTarget::Indirect,
        CallTarget::Vtable { slot: 0 },
        CallTarget::External {
            key: [0; 32],
            name: "foreign".into(),
        },
    ] {
        let mut f = caller(&[], &[]);
        let mut call = safe.clone();
        call.target = target;
        f.op(Op::Call(call), &[], &[]);
        let error = codegen::compile_function(&f.body).unwrap_err().0;
        assert!(
            error.contains("block 0 site 0") && error.contains("direct Instance"),
            "{error}"
        );
    }
    let mut f = caller(&[], &[]);
    f.ret(&[]);
    f.body.safepoints.push(Safepoint {
        kind: SafepointKind::CallReturn,
        block: BlockId(0),
        instruction: None,
    });
    assert!(
        codegen::compile_function(&f.body)
            .unwrap_err()
            .0
            .contains("requires checked entry / stack maps (4c)")
    );
    let mut f = caller(&[], &[]);
    f.op(Op::IConst(0), &[], &[Type::I64]);
    f.body.instructions[0].safepoint = Some(SafepointId(0));
    assert!(
        codegen::compile_function(&f.body)
            .unwrap_err()
            .0
            .contains("requires checked entry / stack maps (4c)")
    );
    let mut f = caller(&[Type::Ptr], &[]);
    let parameter = f.param(0, 0);
    let managed = ValueType::pointer(Provenance::GcHeap);
    f.body.signature.parameters[0] = managed;
    f.body.values[parameter.index()].kind = managed;
    let mut call = safe.clone();
    call.parameters = vec![managed];
    call.poll_free_leaf = false;
    f.op(Op::Call(call), &[parameter], &[]);
    assert!(
        codegen::compile_function(&f.body)
            .unwrap_err()
            .0
            .contains("requires checked entry / stack maps (4c)")
    );
}

#[test]
fn malformed_call_signatures_and_foreign_effects_are_rejected() {
    let mut callee = Fixture::new(&[], &[]);
    callee.ret(&[]);
    for case in 0..5 {
        let mut call = direct(&callee);
        match case {
            0 => call.captures_arguments = true,
            1 => call.parameters.push(value_type(Type::I64)),
            2 => call.results = vec![value_type(Type::I64); 3],
            3 => call.sret = Some((1, 24, [0; 32])),
            _ => call.kind = CallKind::ForeignBridge,
        }
        let mut f = caller(&[], &[]);
        f.op(Op::Call(call), &[], &[]);
        let error = codegen::compile_function(&f.body).unwrap_err().0;
        assert!(error.contains("block 0 site 0"), "{error}");
    }
    let mut f = caller(&[Type::Flags], &[]);
    f.ret(&[]);
    assert!(codegen::compile_function(&f.body).is_err());
    let mut f = caller(&[], &[]);
    f.op(Op::Call(direct(&callee)), &[], &[]);
    f.body.instructions[0].memory = None;
    assert!(
        codegen::compile_function(&f.body)
            .unwrap_err()
            .0
            .contains("call lacks sequencing token")
    );
}

#[test]
fn large_frames_materialize_sp_and_preserve_stack_values_across_calls() {
    let mut callee = Fixture::new(&[], &[Type::I64]);
    let value = callee.constant(Type::I64, 7);
    callee.ret(&[value]);
    let mut f = caller(&[], &[Type::I64]);
    for bytes in [(1 << 24) + 16, 8] {
        f.body.stack_slots.push(StackSlot {
            local: 0,
            bytes,
            align: 8,
            descriptor: [0; 32],
            roots: vec![],
        });
    }
    let pointer = f.op(Op::StackAddr(SlotId(1)), &[], &[Type::Ptr])[0];
    f.body.values[pointer.index()].kind = ValueType::pointer(Provenance::Stack);
    let value = f.constant(Type::I64, 41);
    f.op(
        Op::Store(access(AliasClass::Stack(SlotId(1)))),
        &[pointer, value],
        &[],
    );
    let result = f.op(Op::Call(direct(&callee)), &[], &[Type::I64])[0];
    let loaded = f.op(
        Op::Load(access(AliasClass::Stack(SlotId(1)))),
        &[pointer],
        &[Type::I64],
    )[0];
    let sum = f.op(Op::Integer(IntOp::Add), &[loaded, result], &[Type::I64]);
    f.ret(&sum);
    let fragment = compile(&f);
    assert!(fragment.frame.size > 1 << 24);
    assert_eq!(fragment.frame.size % 16, 0);
    assert!(
        decoded(&fragment)
            .iter()
            .any(|inst| table::get(inst.form).unwrap().key == "sub.X.extend")
    );
    assert_eq!(vm::run_linked(&fragment, &[&callee.compile()], &[])[0], 48);
    f.body.stack_slots[0].bytes = u64::from(u32::MAX) - 16;
    assert!(
        codegen::compile_function(&f.body)
            .unwrap_err()
            .0
            .contains("overflow")
    );
}
