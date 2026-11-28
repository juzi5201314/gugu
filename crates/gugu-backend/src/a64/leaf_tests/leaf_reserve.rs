use super::{
    runtime_vm::{self, Harness, bindings},
    *,
};
use crate::gir::body::CallKind;

fn pressure_leaf() -> Fixture {
    let mut f = Fixture::new(&[], &[Type::I64]);
    f.body.instance = [2; 32];
    let values = (1..=25)
        .map(|value| f.op(Op::IConst(value), &[], &[Type::I64])[0])
        .collect::<Vec<_>>();
    let result = values.iter().skip(1).fold(values[0], |sum, value| {
        f.op(Op::Integer(IntOp::Add), &[sum, *value], &[Type::I64])[0]
    });
    f.ret(&[result]);
    f
}

fn caller(callee: &Fixture) -> Fixture {
    let mut f = Fixture::new(&[], &[Type::I64]);
    f.body.instance = [1; 32];
    f.body.poll_interface.leaf = false;
    f.body.poll_summary.entry_stack_check = true;
    f.op(Op::StackCheck, &[], &[]);
    let call = Call {
        target: CallTarget::Instance(callee.body.instance),
        kind: CallKind::Managed,
        parameters: vec![],
        results: callee.body.signature.results.clone(),
        may_unwind: false,
        may_suspend: false,
        may_allocate: false,
        captures_arguments: false,
        by_value: vec![],
        foreign_signature: None,
        foreign_arguments: vec![],
        sret: None,
        poll_free_leaf: true,
        scoped_arguments: Vec::new(),
    };
    let result = f.op(Op::Call(call), &[], &[Type::I64])[0];
    f.ret(&[result]);
    f
}

#[test]
fn poll_free_leaf_with_spills_never_enters_morestack_without_caller_map() {
    let leaf = pressure_leaf();
    let callee = codegen::compile_runtime(&leaf.body, &bindings()).unwrap();
    assert!(callee.frame.size > 0);
    assert!(!callee.allocation.slots.slots.is_empty());
    let caller = codegen::compile_runtime(&caller(&leaf).body, &bindings()).unwrap();
    let maps = crate::runtime::decode_tables(&caller.metadata.as_ref().unwrap().stackmaps).unwrap();
    assert!(maps.safepoints.iter().all(|point| point.kind != 0));
    let mut harness = Harness {
        watermark: crate::runtime::coroutine::POLL_SENTINEL as u64,
        growth: [0x4000].into(),
        ..Default::default()
    };
    assert_eq!(runtime_vm::run(&callee, &[], &mut harness)[0], 325);
    assert_eq!(
        harness.candidates.len(),
        0,
        "poll-free leaf entered morestack despite the caller's legitimate absent CallReturn map"
    );
}

#[test]
fn caller_reserves_leaf_capacity_before_frame_writes_and_retries_after_growth() {
    let leaf = pressure_leaf();
    let callee = codegen::compile_runtime(&leaf.body, &bindings()).unwrap();
    let caller = codegen::compile_runtime(&caller(&leaf).body, &bindings()).unwrap();
    assert_eq!(caller.frame.max_leaf_reserve, frame::POLL_FREE_LEAF_RESERVE);
    assert_eq!(
        caller.frame.required_frame,
        caller.frame.size + frame::POLL_FREE_LEAF_RESERVE
    );
    assert!(callee.frame.unchecked_leaf && callee.runtime_sites.is_empty());
    let mut harness = Harness::default();
    let sp = harness.expected_sp;
    harness.watermark = sp - u64::from(caller.frame.size) - u64::from(callee.frame.size) + 16;
    harness.growth = [0x4000].into();
    assert_eq!(
        runtime_vm::run_linked(&caller, &[&callee], &[], &mut harness)[0],
        325
    );
    assert_eq!(
        harness.candidates,
        [sp - u64::from(caller.frame.required_frame)]
    );
    assert_eq!(harness.guard_loads, 2);
    let decoded =
        crate::runtime::decode_tables(&caller.metadata.as_ref().unwrap().stackmaps).unwrap();
    assert_eq!(decoded.functions[0].frame_size, caller.frame.size);
    assert!(decoded.safepoints.iter().all(|point| point.kind != 0));
}

#[test]
fn poll_free_leaf_frame_budget_has_exact_empty_and_error_boundaries() {
    let mut leaf = Fixture::new(&[], &[]);
    leaf.ret(&[]);
    let empty = codegen::compile_runtime(&leaf.body, &bindings()).unwrap();
    assert_eq!(empty.frame.size, 0);
    assert!(empty.frame.unchecked_leaf && empty.runtime_sites.is_empty());
    leaf.body.stack_slots.push(StackSlot {
        local: 0,
        bytes: u64::from(frame::POLL_FREE_LEAF_RESERVE - 24),
        align: 8,
        roots: vec![],
        descriptor: [0; 32],
    });
    let exact = codegen::compile_runtime(&leaf.body, &bindings()).unwrap();
    assert_eq!(exact.frame.size, frame::POLL_FREE_LEAF_RESERVE);
    assert!(exact.runtime_sites.is_empty());
    leaf.body.stack_slots[0].bytes += 1;
    assert!(
        codegen::compile_runtime(&leaf.body, &bindings())
            .unwrap_err()
            .0
            .contains("exceeds the 4096-byte caller reserve")
    );
    let caller = caller(&pressure_leaf());
    let mut overflow = exact.frame;
    overflow.size = u32::MAX - 15;
    assert!(
        frame::runtime_policy(&caller.body, &mut overflow)
            .unwrap_err()
            .0
            .contains("overflow")
    );
    let mut inconsistent = pressure_leaf();
    inconsistent.op(Op::StackCheck, &[], &[]);
    assert!(codegen::compile_runtime(&inconsistent.body, &bindings()).is_err());
}
