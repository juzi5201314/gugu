use super::*;

fn access() -> crate::lir::body::Access {
    crate::lir::body::Access {
        alias: AliasClass::Stack(SlotId(0)),
        align: 8,
        volatile: false,
    }
}

#[test]
fn local_load_store_widths_and_pointer_offsets() {
    for ty in [Type::I8, Type::I16, Type::I32, Type::I64] {
        let mut f = Fixture::new(&[ty], &[ty]);
        f.body.stack_slots.push(StackSlot {
            local: 0,
            bytes: 32,
            align: 16,
            descriptor: [0; 32],
            roots: vec![],
        });
        let base = f.op(Op::StackAddr(SlotId(0)), &[], &[Type::Ptr])[0];
        let displacement = f.constant(Type::I64, 8);
        let pointer = f.op(Op::PtrOffset, &[base, displacement], &[Type::Ptr])[0];
        let value = f.param(0, 0);
        f.op(Op::Store(access()), &[pointer, value], &[]);
        let result = f.op(Op::Load(access()), &[pointer], &[ty]);
        f.ret(&result);
        let fragment = f.compile();
        assert_eq!(fragment.frame.size % 16, 0);
        assert_eq!(
            vm::run(&fragment, &[0x1234_5678_9abc_def0])[0],
            0x1234_5678_9abc_def0 & (u64::MAX >> (64 - ty.bytes().unwrap() * 8))
        );
    }
}

#[test]
fn pressure_uses_shared_spills_and_restores_callee_saved_registers() {
    let mut f = Fixture::new(&[Type::I64; 10], &[Type::I64]);
    let mut sum = f.param(0, 0);
    for index in 1..10 {
        sum = f.op(
            Op::Integer(IntOp::Add),
            &[sum, f.param(0, index)],
            &[Type::I64],
        )[0];
    }
    f.ret(&[sum]);
    let fragment = codegen::compile_with_registers(&f.body, &Tiny).unwrap();
    assert!(!fragment.allocation.slots.slots.is_empty());
    assert_eq!(vm::run(&fragment, &(1..=10).collect::<Vec<_>>())[0], 55);
    let mut f = Fixture::new(&[Type::I64; 24], &[Type::I64]);
    let mut sum = f.param(0, 0);
    for index in 1..24 {
        sum = f.op(
            Op::Integer(IntOp::Add),
            &[sum, f.param(0, index)],
            &[Type::I64],
        )[0];
    }
    f.ret(&[sum]);
    let fragment = f.compile();
    assert!(!fragment.frame.saves.is_empty());
    assert_eq!(vm::run(&fragment, &(1..=24).collect::<Vec<_>>())[0], 300);
    for value in &fragment.allocation.values {
        if let Some(crate::regalloc::Location::Reg(reg)) = value.location() {
            assert!(abi::POOL.contains(&reg));
        }
    }
}

#[test]
fn large_legal_offsets_and_oversized_frames_fail_explicitly() {
    let mut f = Fixture::new(&[], &[Type::Ptr]);
    f.body.stack_slots.push(StackSlot {
        local: 0,
        bytes: 8192,
        align: 16,
        descriptor: [0; 32],
        roots: vec![],
    });
    f.body.stack_slots.push(StackSlot {
        local: 1,
        bytes: 16,
        align: 16,
        descriptor: [0; 32],
        roots: vec![],
    });
    let pointer = f.op(Op::StackAddr(SlotId(1)), &[], &[Type::Ptr]);
    f.ret(&pointer);
    assert_eq!(vm::run(&f.compile(), &[])[0] % 16, 0);
    f.body.stack_slots[0].bytes = u64::MAX;
    assert!(
        codegen::compile_leaf(&f.body)
            .unwrap_err()
            .0
            .contains("local too large")
    );
    f.body.stack_slots[0].bytes = 40000;
    assert!(
        codegen::compile_leaf(&f.body)
            .unwrap_err()
            .0
            .contains("legalization limit")
    );
}
