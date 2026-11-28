use super::*;
use crate::regalloc::{Location, PhysReg, Placement};

fn bits(value: f64) -> u128 {
    u128::from(value.to_bits())
}
fn constant(f: &mut Fixture, ty: Type, value: u64) -> ValueId {
    f.op(Op::FConst(value), &[], &[ty])[0]
}
fn call(callee: &Fixture) -> Call {
    Call {
        target: CallTarget::Instance(callee.body.instance),
        kind: crate::gir::body::CallKind::Managed,
        parameters: callee.body.signature.parameters.clone(),
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
    }
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
fn scalar_fp_arithmetic_constants_and_ieee_comparisons() {
    for ty in [Type::F32, Type::F64] {
        let encode = |value: f64| {
            if ty == Type::F32 {
                u128::from((value as f32).to_bits())
            } else {
                bits(value)
            }
        };
        for (op, expected) in [
            (FloatOp::Add, 8.0),
            (FloatOp::Sub, 2.0),
            (FloatOp::Mul, 15.0),
            (FloatOp::Div, 5.0 / 3.0),
            (FloatOp::Neg, -5.0),
        ] {
            let mut f = Fixture::new(&[ty], &[ty]);
            let three = constant(&mut f, ty, encode(3.0) as u64);
            let args = if op == FloatOp::Neg {
                vec![f.param(0, 0)]
            } else {
                vec![f.param(0, 0), three]
            };
            let value = f.op(Op::Float(op), &args, &[ty]);
            f.ret(&value);
            assert_eq!(
                vm::execute_fp(&f.compile(), &[], &[(ty, encode(5.0))], None).1[0],
                encode(expected)
            );
        }
        for condition in [
            Condition::Eq,
            Condition::Ne,
            Condition::Lt,
            Condition::Le,
            Condition::Gt,
            Condition::Ge,
        ] {
            let mut f = Fixture::new(&[ty, ty], &[Type::I8]);
            let result = f.op(
                Op::Compare {
                    condition,
                    signed: false,
                },
                &[f.param(0, 0), f.param(0, 1)],
                &[Type::I8],
            );
            f.ret(&result);
            let fragment = f.compile();
            for (a, b) in [
                (f64::NAN, 1.0),
                (1.0, f64::NAN),
                (-0.0, 0.0),
                (-2.0, 3.0),
                (f64::INFINITY, 2.0),
            ] {
                let expected = match condition {
                    Condition::Eq => a == b,
                    Condition::Ne => a != b,
                    Condition::Lt => a < b,
                    Condition::Le => a <= b,
                    Condition::Gt => a > b,
                    Condition::Ge => a >= b,
                };
                assert_eq!(
                    vm::execute_fp(&fragment, &[], &[(ty, encode(a)), (ty, encode(b))], None).0[0],
                    u64::from(expected),
                    "{ty:?} {condition:?} {a} {b}"
                );
            }
        }
    }
}

#[test]
fn distinct_fp_constants_and_full_width_uint_survive_copy_and_arithmetic() {
    let mut f = Fixture::new(&[], &[Type::F64; 4]);
    let x = constant(&mut f, Type::F64, 1.5_f64.to_bits());
    let y = constant(&mut f, Type::F64, 2.25_f64.to_bits());
    f.body.stack_slots.push(StackSlot {
        local: 0,
        bytes: 8,
        align: 8,
        roots: vec![],
        descriptor: [0; 32],
    });
    let address = f.op(Op::StackAddr(SlotId(0)), &[], &[Type::Ptr])[0];
    let access = crate::lir::body::Access {
        alias: AliasClass::Stack(SlotId(0)),
        align: 8,
        volatile: false,
    };
    f.op(Op::Store(access.clone()), &[address, x], &[]);
    let copy = f.op(Op::Load(access), &[address], &[Type::F64])[0];
    let sum = f.op(Op::Float(FloatOp::Add), &[x, y], &[Type::F64])[0];
    let product = f.op(Op::Float(FloatOp::Mul), &[x, y], &[Type::F64])[0];
    let integer = f.constant(Type::I64, u64::MAX);
    let unsigned = f.op(
        Op::Convert(Conversion::IntToFloat { signed: false }),
        &[integer],
        &[Type::F64],
    )[0];
    f.ret(&[copy, sum, product, unsigned]);
    let (_, fp, _) = vm::execute_fp(&f.compile(), &[], &[], None);
    assert_eq!(
        &fp[..4],
        &[bits(1.5), bits(3.75), bits(3.375), bits(u64::MAX as f64)]
    );
}

#[test]
fn fp_conversions_preserve_bits_round_and_extend_narrow_integers() {
    for (source, target, signed, input, expected) in [
        (Type::I8, Type::F64, true, 255, bits(-1.0)),
        (
            Type::I16,
            Type::F32,
            true,
            65535,
            u128::from((-1.0_f32).to_bits()),
        ),
        (
            Type::I64,
            Type::F64,
            false,
            u128::from(u64::MAX),
            bits(u64::MAX as f64),
        ),
        (
            Type::I64,
            Type::F32,
            false,
            u128::from(u64::MAX),
            u128::from((u64::MAX as f32).to_bits()),
        ),
    ] {
        let mut f = Fixture::new(&[source], &[target]);
        let result = f.op(
            Op::Convert(Conversion::IntToFloat { signed }),
            &[f.param(0, 0)],
            &[target],
        );
        f.ret(&result);
        assert_eq!(
            vm::execute_fp(&f.compile(), &[], &[(source, input)], None).1[0],
            expected
        );
    }
    for (source, target, op, input, expected) in [
        (
            Type::F64,
            Type::I8,
            Conversion::FloatToInt { signed: true },
            bits(-3.75),
            253,
        ),
        (
            Type::F32,
            Type::I64,
            Conversion::FloatToInt { signed: false },
            u128::from(7.75_f32.to_bits()),
            7,
        ),
        (
            Type::F64,
            Type::F32,
            Conversion::FloatResize,
            bits(1.0 / 3.0),
            u128::from((1.0_f32 / 3.0).to_bits()),
        ),
        (
            Type::F32,
            Type::F64,
            Conversion::FloatResize,
            u128::from(f32::MIN_POSITIVE.to_bits()),
            bits(f64::from(f32::MIN_POSITIVE)),
        ),
        (
            Type::I64,
            Type::F64,
            Conversion::Bitcast,
            0x8000_0000_0000_0000,
            0x8000_0000_0000_0000,
        ),
        (
            Type::F64,
            Type::I64,
            Conversion::Bitcast,
            0x7ff8_1234_5678_abcd,
            0x7ff8_1234_5678_abcd,
        ),
        (
            Type::I32,
            Type::F32,
            Conversion::Bitcast,
            0x8000_0000,
            0x8000_0000,
        ),
        (
            Type::F32,
            Type::I32,
            Conversion::Bitcast,
            0x7fc0_abcd,
            0x7fc0_abcd,
        ),
    ] {
        let mut f = Fixture::new(&[source], &[target]);
        let result = f.op(Op::Convert(op), &[f.param(0, 0)], &[target]);
        f.ret(&result);
        let (gpr, fp, _) = vm::execute_fp(&f.compile(), &[], &[(source, input)], None);
        assert_eq!(
            if abi::class(target) == abi::FPR {
                fp[0]
            } else {
                u128::from(gpr[0])
            },
            expected
        );
    }
}

#[test]
fn mixed_fp_integer_and_q_arguments_use_independent_banks_and_aligned_overflow() {
    let q = Type::V128(Lane::I8);
    let mut types = [Type::F64, Type::I64].repeat(9);
    types.push(q);
    let mut callee = Fixture::new(&types, &[Type::F64, Type::I64, q]);
    callee.ret(&[
        callee.param(0, 16),
        callee.param(0, 17),
        callee.param(0, 18),
    ]);
    let mut caller = Fixture::new(&types, &[Type::F64, Type::I64, q]);
    caller.body.instance = [1; 32];
    caller.body.poll_interface.leaf = false;
    let args = (0..types.len())
        .map(|i| caller.param(0, i))
        .collect::<Vec<_>>();
    let result = caller.op(Op::Call(call(&callee)), &args, &[Type::F64, Type::I64, q]);
    caller.ret(&result);
    let mut values = (0..9)
        .flat_map(|i| {
            [
                (Type::F64, bits(i as f64 + 0.5)),
                (Type::I64, i as u128 + 100),
            ]
        })
        .collect::<Vec<_>>();
    let vector = 0xfedc_ba98_7654_3210_0123_4567_89ab_cdef;
    values.push((q, vector));
    let fragment = codegen::compile_function(&caller.body).unwrap();
    assert_eq!(fragment.frame.outgoing, 32);
    let (gpr, fp, _) = vm::execute_fp(&fragment, &[&callee.compile()], &values, None);
    assert_eq!((gpr[0], fp[0], fp[1]), (108, bits(8.5), vector));
}

#[test]
fn fp_pressure_spills_full_q_slots_and_preserves_d8_to_d15() {
    let mut f = Fixture::new(&[Type::F64; 35], &[Type::F64]);
    let mut sum = f.param(0, 0);
    for index in 1..35 {
        sum = f.op(
            Op::Float(FloatOp::Add),
            &[sum, f.param(0, index)],
            &[Type::F64],
        )[0];
    }
    f.ret(&[sum]);
    let fragment = f.compile();
    assert!(!fragment.allocation.slots.slots.is_empty());
    assert!(
        fragment
            .allocation
            .slots
            .slots
            .iter()
            .all(|slot| slot.size == 16 && slot.align == 16)
    );
    assert!(fragment.frame.saves.iter().any(|(reg, _)| *reg >= 40));
    let args = (1..=35)
        .map(|i| (Type::F64, bits(i as f64)))
        .collect::<Vec<_>>();
    assert_eq!(
        vm::execute_fp(&fragment, &[], &args, None).1[0],
        bits(630.0)
    );
}

#[test]
fn fp_parallel_copy_cycles_are_full_width_and_caller_live_values_survive_calls() {
    let mut callee = Fixture::new(&[Type::F64; 2], &[Type::F64; 2]);
    callee.ret(&[callee.param(0, 0), callee.param(0, 1)]);
    let mut f = Fixture::new(&[Type::F64; 2], &[Type::F64; 2]);
    f.body.instance = [1; 32];
    f.body.poll_interface.leaf = false;
    let result = f.op(
        Op::Call(call(&callee)),
        &[f.param(0, 1), f.param(0, 0)],
        &[Type::F64; 2],
    );
    f.ret(&result);
    let mut fragment = codegen::compile_function(&f.body).unwrap();
    for i in 0..2 {
        fragment.allocation.values[f.param(0, i).index()].placement =
            Placement::Location(Location::Reg(PhysReg(32 + i as u8)));
    }
    reassemble(&mut fragment);
    let (_, fp, _) = vm::execute_fp(
        &fragment,
        &[&callee.compile()],
        &[(Type::F64, bits(3.0)), (Type::F64, bits(7.0))],
        None,
    );
    assert_eq!(&fp[..2], &[bits(7.0), bits(3.0)]);

    let q = Type::V128(Lane::I64);
    let mut callee = Fixture::new(&[], &[Type::F64]);
    let c = constant(&mut callee, Type::F64, 4.25_f64.to_bits());
    callee.ret(&[c]);
    let mut f = Fixture::new(&[Type::F64, q], &[Type::F64, q]);
    f.body.instance = [1; 32];
    f.body.poll_interface.leaf = false;
    let value = f.op(Op::Call(call(&callee)), &[], &[Type::F64])[0];
    let sum = f.op(
        Op::Float(FloatOp::Add),
        &[value, f.param(0, 0)],
        &[Type::F64],
    )[0];
    f.ret(&[sum, f.param(0, 1)]);
    let fragment = codegen::compile_function(&f.body).unwrap();
    assert!(matches!(
        fragment.allocation.values[f.param(0, 1).index()].location(),
        Some(Location::Slot(_))
    ));
    let vector = 0x1234_5678_90ab_cdef_fedc_ba09_8765_4321;
    let (_, fp, _) = vm::execute_fp(
        &fragment,
        &[&callee.compile()],
        &[(Type::F64, bits(1.5)), (q, vector)],
        None,
    );
    assert_eq!(&fp[..2], &[bits(5.75), vector]);
}

#[test]
fn fp_select_and_vector_memory_preserve_both_halves() {
    for ty in [Type::F32, Type::F64, Type::V128(Lane::I8)] {
        let mut f = Fixture::new(&[Type::I8, ty, ty], &[ty]);
        let result = f.op(
            Op::Select,
            &[f.param(0, 0), f.param(0, 1), f.param(0, 2)],
            &[ty],
        );
        f.ret(&result);
        let fragment = f.compile();
        let a = if matches!(ty, Type::V128(_)) {
            0xabcd_ef01_2345_6789_aaaa_bbbb_cccc_dddd
        } else {
            0x8000_0000
        };
        let b = if matches!(ty, Type::V128(_)) {
            !a
        } else {
            0x7fc0_0001
        };
        for condition in [0, 1, 255] {
            assert_eq!(
                vm::execute_fp(
                    &fragment,
                    &[],
                    &[(Type::I8, condition), (ty, a), (ty, b)],
                    None
                )
                .1[0],
                if condition == 0 { b } else { a }
            );
        }
        let mut f = Fixture::new(&[ty], &[ty]);
        f.body.stack_slots.push(StackSlot {
            local: 0,
            bytes: 16,
            align: 16,
            roots: vec![],
            descriptor: [0; 32],
        });
        let ptr = f.op(Op::StackAddr(SlotId(0)), &[], &[Type::Ptr])[0];
        let access = crate::lir::body::Access {
            alias: AliasClass::Stack(SlotId(0)),
            align: 16,
            volatile: false,
        };
        f.op(Op::Store(access.clone()), &[ptr, f.param(0, 0)], &[]);
        let value = f.op(Op::Load(access), &[ptr], &[ty]);
        f.ret(&value);
        assert_eq!(vm::execute_fp(&f.compile(), &[], &[(ty, a)], None).1[0], a);
    }
}

#[test]
fn fp_morestack_preserves_q_arguments_and_poll_clobbers_caller_saved_vectors() {
    let q = Type::V128(Lane::I8);
    let mut f = Fixture::new(&[Type::F64, q], &[Type::F64, q]);
    f.body.poll_interface.leaf = false;
    f.op(Op::StackCheck, &[], &[]);
    f.op(
        Op::SafepointPoll {
            interval: std::num::NonZeroU32::new(1).unwrap(),
        },
        &[],
        &[],
    );
    let one = constant(&mut f, Type::F64, 1.0_f64.to_bits());
    let sum = f.op(Op::Float(FloatOp::Add), &[f.param(0, 0), one], &[Type::F64])[0];
    f.ret(&[sum, f.param(0, 1)]);
    let fragment = codegen::compile_runtime(&f.body, &runtime_vm::bindings()).unwrap();
    assert!(matches!(
        fragment.allocation.values[f.param(0, 1).index()].location(),
        Some(Location::Slot(_))
    ));
    let mut runtime = runtime_vm::Harness {
        watermark: crate::runtime::coroutine::POLL_SENTINEL as u64,
        growth: [0x10000, 0].into(),
        flags: 1,
        ..Default::default()
    };
    let vector = 0xdead_beef_aaaa_5555_1234_5678_90ab_cdef;
    let (_, fp, _) = vm::execute_fp(
        &fragment,
        &[],
        &[(Type::F64, bits(2.5)), (q, vector)],
        Some(&mut runtime),
    );
    assert_eq!(&fp[..2], &[bits(3.5), vector]);
    assert!(runtime.polls > 0);
    assert!(!runtime.candidates.is_empty());
}

#[test]
fn fp_empty_abi_and_invalid_signatures_are_explicit() {
    assert_eq!(abi::arguments([]).unwrap(), (vec![], 0));
    assert!(abi::results([]).unwrap().is_empty());
    let mut f = Fixture::new(&[Type::F64], &[Type::F64]);
    let value = f.op(
        Op::Convert(Conversion::FloatResize),
        &[f.param(0, 0)],
        &[Type::F64],
    );
    f.ret(&value);
    assert!(
        codegen::compile_leaf(&f.body)
            .unwrap_err()
            .0
            .contains("FP conversion")
    );
    let mut f = Fixture::new(&[Type::F64], &[Type::F64]);
    f.ret(&[f.param(0, 0)]);
    let parameter = f.param(0, 0);
    f.body.values[parameter.index()].kind.provenance = Some(Provenance::GcHeap);
    assert!(
        codegen::compile_leaf(&f.body)
            .unwrap_err()
            .0
            .contains("pointer provenance")
    );
}
