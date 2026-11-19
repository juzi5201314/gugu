use super::{
    atomic_vm::{self, Harness},
    *,
};
use crate::gir::body::MemoryOrdering::{self, *};
use std::collections::BTreeMap;

const AT: u64 = 0x4000;
const ORDERS: [MemoryOrdering; 5] = [Relaxed, Acquire, Release, AcqRel, SeqCst];

fn fixture(
    op: AtomicOp,
    ty: Type,
    ordering: MemoryOrdering,
    failure: Option<MemoryOrdering>,
) -> Fixture {
    let (params, results) = match op {
        AtomicOp::Fence => (vec![], vec![]),
        AtomicOp::Load => (vec![Type::Ptr], vec![ty]),
        AtomicOp::Store => (vec![Type::Ptr, ty], vec![]),
        AtomicOp::CompareExchange => (vec![Type::Ptr, ty, ty], vec![ty, Type::I8]),
        _ => (vec![Type::Ptr, ty], vec![ty]),
    };
    let mut f = Fixture::new(&params, &results);
    let args = (0..params.len()).map(|i| f.param(0, i)).collect::<Vec<_>>();
    let result = f.op(
        Op::Atomic {
            op,
            ordering,
            failure,
            align: ty.bytes().unwrap_or(8) as u32,
        },
        &args,
        &results,
    );
    f.ret(&result);
    f
}

fn run(
    fragment: &codegen::LeafFragment,
    args: &[u64],
    bytes: u64,
    old: u64,
    harness: &mut Harness,
) -> ([u64; 2], u64) {
    let mut memory = BTreeMap::new();
    atomic_vm::store(&mut memory, AT, bytes, old);
    memory.insert(AT + bytes, 0xa5);
    let (result, memory) = vm::execute_atomic(fragment, args, memory, harness);
    assert_eq!(
        memory[&(AT + bytes)],
        0xa5,
        "narrow atomic overwrote adjacent byte"
    );
    (result, atomic_vm::word(&memory, AT, bytes))
}

#[test]
fn atomic_fetch_operations_cover_widths_orders_wraparound_and_exclusive_retries() {
    for ty in [Type::I8, Type::I16, Type::I32, Type::I64] {
        let bytes = ty.bytes().unwrap();
        let mask = u64::MAX >> (64 - bytes * 8);
        for op in [
            AtomicOp::Exchange,
            AtomicOp::Add,
            AtomicOp::Sub,
            AtomicOp::And,
            AtomicOp::Or,
            AtomicOp::Xor,
        ] {
            for ordering in ORDERS {
                let fragment = fixture(op, ty, ordering, None).compile();
                let keys = decoded(&fragment)
                    .iter()
                    .map(|i| table::get(i.form).unwrap().key)
                    .collect::<Vec<_>>();
                assert!(keys.iter().any(|k| k.starts_with(
                    if matches!(ordering, Acquire | AcqRel | SeqCst) {
                        "ldaxr."
                    } else {
                        "ldxr."
                    }
                )));
                assert!(keys.iter().any(|k| k.starts_with(
                    if matches!(ordering, Release | AcqRel | SeqCst) {
                        "stlxr."
                    } else {
                        "stxr."
                    }
                )));
                for retry in [false, true] {
                    let old = if retry { 7 } else { mask - 2 };
                    let mut harness = Harness {
                        failures_remaining: u32::from(retry),
                        interference: retry.then_some(7),
                        ..Default::default()
                    };
                    let (result, stored) = run(&fragment, &[AT, 5], bytes, mask - 2, &mut harness);
                    let expected = match op {
                        AtomicOp::Exchange => 5,
                        AtomicOp::Add => old.wrapping_add(5),
                        AtomicOp::Sub => old.wrapping_sub(5),
                        AtomicOp::And => old & 5,
                        AtomicOp::Or => old | 5,
                        AtomicOp::Xor => old ^ 5,
                        _ => unreachable!(),
                    } & mask;
                    assert_eq!(result[0], old, "{ty:?} {op:?} {ordering:?}");
                    assert_eq!(stored, expected);
                    assert_eq!(
                        (harness.loads, harness.stores, harness.failures),
                        (1 + u32::from(retry), 1, u32::from(retry))
                    );
                    if ordering == SeqCst {
                        assert_eq!(harness.trace.first(), Some(&"ish"));
                        assert_eq!(harness.trace.last(), Some(&"ish"));
                    }
                }
            }
        }
    }
}

#[test]
fn atomic_cas_is_strong_returns_actual_and_clears_mismatched_reservations() {
    for ty in [Type::I8, Type::I16, Type::I32, Type::I64, Type::Ptr] {
        for success in ORDERS {
            let failures: &[MemoryOrdering] = match success {
                Relaxed | Release => &[Relaxed],
                Acquire | AcqRel => &[Relaxed, Acquire],
                SeqCst => &[Relaxed, Acquire, SeqCst],
            };
            for failure in failures {
                let fragment =
                    fixture(AtomicOp::CompareExchange, ty, success, Some(*failure)).compile();
                for (expected, retries, interference, actual, ok) in [
                    (7, 0, None, 7, 1),
                    (6, 0, None, 7, 0),
                    (7, 2, None, 7, 1),
                    (7, 1, Some(9), 9, 0),
                ] {
                    let mut harness = Harness {
                        failures_remaining: retries,
                        interference,
                        ..Default::default()
                    };
                    let (result, stored) = run(
                        &fragment,
                        &[AT, expected, 23],
                        ty.bytes().unwrap(),
                        7,
                        &mut harness,
                    );
                    assert_eq!(result, [actual, ok], "{ty:?} {success:?}/{failure:?}");
                    assert_eq!(stored, if ok == 1 { 23 } else { actual });
                    assert_eq!(harness.clears, u32::from(ok == 0));
                    assert_eq!(harness.failures, retries);
                    assert_eq!(harness.stores, ok as u32);
                }
            }
        }
    }
}

#[test]
fn atomic_load_store_and_fences_use_valid_widths_and_barriers() {
    for ty in [Type::I8, Type::I16, Type::I32, Type::I64, Type::Ptr] {
        for op in [AtomicOp::Load, AtomicOp::Store] {
            for ordering in if op == AtomicOp::Load {
                [Relaxed, Acquire, SeqCst]
            } else {
                [Relaxed, Release, SeqCst]
            } {
                let fragment = fixture(op, ty, ordering, None).compile();
                let mut harness = Harness::default();
                let (result, stored) =
                    run(&fragment, &[AT, 23], ty.bytes().unwrap(), 7, &mut harness);
                if op == AtomicOp::Load {
                    assert_eq!(result[0], 7);
                    assert_eq!(stored, 7);
                } else {
                    assert_eq!(stored, 23);
                }
                if ordering == SeqCst {
                    assert_eq!(
                        harness.trace,
                        if op == AtomicOp::Load {
                            vec!["ish", "acquire", "ish"]
                        } else {
                            vec!["ish", "release", "ish"]
                        }
                    );
                }
                assert_eq!(harness.loads, 0);
            }
        }
    }
    for ordering in [Acquire, Release, AcqRel, SeqCst] {
        let fragment = fixture(AtomicOp::Fence, Type::I64, ordering, None).compile();
        let mut harness = Harness::default();
        vm::execute_atomic(&fragment, &[], BTreeMap::new(), &mut harness);
        assert_eq!(
            harness.trace,
            [if ordering == Acquire { "ishld" } else { "ish" }]
        );
    }
    let mut empty = Fixture::new(&[], &[]);
    empty.ret(&[]);
    let mut harness = Harness::default();
    vm::execute_atomic(&empty.compile(), &[], BTreeMap::new(), &mut harness);
    assert!(harness.trace.is_empty());
}

#[test]
fn atomic_exclusive_region_is_spill_free_with_live_values_and_parallel_aliases() {
    let mut params = vec![Type::Ptr];
    params.extend([Type::I64; 28]);
    let mut f = Fixture::new(&params, &[Type::I64]);
    let args = (0..29).map(|i| f.param(0, i)).collect::<Vec<_>>();
    let mut sum = f.op(
        Op::Atomic {
            op: AtomicOp::Add,
            ordering: AcqRel,
            failure: None,
            align: 8,
        },
        &args[..2],
        &[Type::I64],
    )[0];
    for value in &args[1..] {
        sum = f.op(Op::Integer(IntOp::Add), &[sum, *value], &[Type::I64])[0];
    }
    f.ret(&[sum]);
    let fragment = f.compile();
    assert!(!fragment.allocation.slots.slots.is_empty());
    let code = decoded(&fragment);
    let start = code
        .iter()
        .position(|i| table::get(i.form).unwrap().key == "ldaxr.X")
        .unwrap();
    let end = code
        .iter()
        .position(|i| table::get(i.form).unwrap().key == "stlxr.X")
        .unwrap();
    assert!(
        code[start + 1..end]
            .iter()
            .all(|i| table::get(i.form).unwrap().key == "add.X.shift")
    );
    let mut harness = Harness {
        failures_remaining: 1,
        interference: Some(41),
        ..Default::default()
    };
    let mut inputs = vec![AT];
    inputs.extend(1..=28);
    let (result, stored) = run(&fragment, &inputs, 8, 20, &mut harness);
    assert_eq!((result[0], stored), (447, 42));
    assert_eq!((harness.loads, harness.failures), (2, 1));
    let mut same = Fixture::new(&[Type::Ptr, Type::I64], &[Type::I64, Type::I8]);
    let (p, v) = (same.param(0, 0), same.param(0, 1));
    let result = same.op(
        Op::Atomic {
            op: AtomicOp::CompareExchange,
            ordering: Relaxed,
            failure: Some(Relaxed),
            align: 8,
        },
        &[p, v, v],
        &[Type::I64, Type::I8],
    );
    same.ret(&result);
    assert_eq!(
        run(&same.compile(), &[AT, 7], 8, 7, &mut Harness::default()).0,
        [7, 1]
    );
}

#[test]
fn atomic_invalid_orders_shapes_provenance_alignment_and_sequence_are_errors() {
    for (op, order, failure) in [
        (AtomicOp::Load, Release, None),
        (AtomicOp::Load, AcqRel, None),
        (AtomicOp::Store, Acquire, None),
        (AtomicOp::Store, AcqRel, None),
        (AtomicOp::Fence, Relaxed, None),
        (AtomicOp::Add, Relaxed, Some(Relaxed)),
    ] {
        let f = fixture(op, Type::I64, order, failure);
        assert!(
            codegen::compile_function(&f.body)
                .unwrap_err()
                .0
                .contains("atomic ordering")
        );
    }
    for success in ORDERS {
        for failure in ORDERS {
            let valid = failure == Relaxed
                || failure == Acquire && matches!(success, Acquire | AcqRel | SeqCst)
                || failure == SeqCst && success == SeqCst;
            assert_eq!(
                codegen::compile_function(
                    &fixture(AtomicOp::CompareExchange, Type::I64, success, Some(failure)).body
                )
                .is_ok(),
                valid
            );
        }
    }
    for case in 0..8 {
        let mut f = fixture(AtomicOp::Add, Type::I64, Relaxed, None);
        match case {
            0 => f.body.instructions[0].memory = None,
            1 => {
                if let Op::Atomic { align, .. } = &mut f.body.instructions[0].op {
                    *align = 4;
                }
            }
            2 => {
                if let Op::Atomic { align, .. } = &mut f.body.instructions[0].op {
                    *align = 3;
                }
            }
            3 => f.body.instructions[0].results = 0..0,
            4 => f.body.instructions[0].arguments = 0..0,
            5 => {
                let v = f.param(0, 1);
                f.body.values[v.index()].kind = ValueType::pointer(Provenance::GcHeap);
            }
            6 => {
                let v = f.param(0, 1);
                f.body.values[v.index()].kind = ValueType::scalar(Type::F64);
            }
            _ => {
                if let Op::Atomic { op, failure, .. } = &mut f.body.instructions[0].op {
                    *op = AtomicOp::CompareExchange;
                    *failure = None;
                }
            }
        }
        assert!(codegen::compile_function(&f.body).is_err(), "case {case}");
    }
}
