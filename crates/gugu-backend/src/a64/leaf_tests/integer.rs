use super::*;

#[test]
fn integer_widths_logic_arithmetic_and_modulo_shifts() {
    for (ty, bits) in [
        (Type::I8, 8),
        (Type::I16, 16),
        (Type::I32, 32),
        (Type::I64, 64),
    ] {
        let mask = u64::MAX >> (64 - bits);
        for op in [
            IntOp::Add,
            IntOp::Sub,
            IntOp::Mul,
            IntOp::And,
            IntOp::Or,
            IntOp::Xor,
            IntOp::Shl,
            IntOp::ShrSigned,
            IntOp::ShrUnsigned,
            IntOp::Neg,
            IntOp::Not,
        ] {
            let mut f = Fixture::new(&[ty, ty], &[ty]);
            let (a, b) = (f.param(0, 0), f.param(0, 1));
            let operands = if matches!(op, IntOp::Neg | IntOp::Not) {
                vec![a]
            } else {
                vec![a, b]
            };
            let out = f.op(Op::Integer(op), &operands, &[ty]);
            f.ret(&out);
            let fragment = f.compile();
            let x = mask - 2;
            let y = bits + 1;
            let signed = ((x << (64 - bits)) as i64) >> (64 - bits);
            let expected = match op {
                IntOp::Add => x.wrapping_add(y),
                IntOp::Sub => x.wrapping_sub(y),
                IntOp::Mul => x.wrapping_mul(y),
                IntOp::And => x & y,
                IntOp::Or => x | y,
                IntOp::Xor => x ^ y,
                IntOp::Shl => x.wrapping_shl((y % bits) as u32),
                IntOp::ShrUnsigned => x >> (y % bits),
                IntOp::ShrSigned => (signed >> (y % bits)) as u64,
                IntOp::Neg => x.wrapping_neg(),
                IntOp::Not => !x,
                _ => unreachable!(),
            } & mask;
            assert_eq!(vm::run(&fragment, &[x, y])[0], expected, "{ty:?} {op:?}");
        }
    }
}

#[test]
fn carry_borrow_and_wide_product() {
    for op in [IntOp::AddCarry, IntOp::SubBorrow, IntOp::MulWide] {
        let params = if op == IntOp::MulWide {
            vec![Type::I64; 2]
        } else {
            vec![Type::I64; 3]
        };
        let mut f = Fixture::new(&params, &[Type::I64; 2]);
        let args = (0..params.len()).map(|i| f.param(0, i)).collect::<Vec<_>>();
        let out = f.op(Op::Integer(op), &args, &[Type::I64; 2]);
        f.ret(&out);
        for (a, b, c) in [(u64::MAX, 1, 1), (0, 1, 1), (7, 11, 0)] {
            let expected = match op {
                IntOp::MulWide => {
                    let p = u128::from(a) * u128::from(b);
                    [p as u64, (p >> 64) as u64]
                }
                IntOp::AddCarry => {
                    let p = u128::from(a) + u128::from(b) + u128::from(c);
                    [p as u64, (p >> 64) as u64]
                }
                _ => [
                    a.wrapping_sub(b).wrapping_sub(c),
                    u64::from(u128::from(a) < u128::from(b) + u128::from(c)),
                ],
            };
            assert_eq!(vm::run(&f.compile(), &[a, b, c]), expected, "{op:?}");
        }
    }
}

#[test]
fn constants_choose_movn_and_round_trip() {
    for value in [0, 1, 0xffff_ffff_ffff_0000, u64::MAX, 0x1234_0000_5678_9abc] {
        let mut f = Fixture::new(&[], &[Type::I64]);
        let result = f.constant(Type::I64, value);
        f.ret(&[result]);
        let fragment = f.compile();
        assert_eq!(vm::run(&fragment, &[])[0], value);
        if value == u64::MAX {
            assert!(
                decoded(&fragment)
                    .iter()
                    .any(|i| table::get(i.form).unwrap().key == "movn.X")
            );
        }
    }
}

#[test]
fn comparisons_select_conversions_and_nonzero_division() {
    for signed in [false, true] {
        for condition in [
            Condition::Eq,
            Condition::Ne,
            Condition::Lt,
            Condition::Le,
            Condition::Gt,
            Condition::Ge,
        ] {
            let mut f = Fixture::new(&[Type::I8, Type::I8], &[Type::I8]);
            let (a, b) = (f.param(0, 0), f.param(0, 1));
            let out = f.op(Op::Compare { condition, signed }, &[a, b], &[Type::I8]);
            f.ret(&out);
            let (a, b) = if signed { (-1, 1) } else { (255, 1) };
            let expected = match condition {
                Condition::Eq => a == b,
                Condition::Ne => a != b,
                Condition::Lt => a < b,
                Condition::Le => a <= b,
                Condition::Gt => a > b,
                Condition::Ge => a >= b,
            };
            assert_eq!(vm::run(&f.compile(), &[255, 1])[0], u64::from(expected));
        }
    }
    for op in [
        IntOp::DivSigned,
        IntOp::DivUnsigned,
        IntOp::RemSigned,
        IntOp::RemUnsigned,
    ] {
        let mut f = Fixture::new(&[Type::I64], &[Type::I64]);
        let a = f.param(0, 0);
        let divisor = f.constant(Type::I64, 3);
        let out = f.op(Op::Integer(op), &[a, divisor], &[Type::I64]);
        f.ret(&out);
        let result = match op {
            IntOp::DivSigned => (-11_i64 / 3) as u64,
            IntOp::RemSigned => (-11_i64 % 3) as u64,
            IntOp::DivUnsigned => (-11_i64 as u64) / 3,
            _ => (-11_i64 as u64) % 3,
        };
        assert_eq!(vm::run(&f.compile(), &[(-11_i64) as u64])[0], result);
    }
    for op in [Conversion::SignExtend, Conversion::ZeroExtend] {
        let mut f = Fixture::new(&[Type::I8], &[Type::I64]);
        let a = f.param(0, 0);
        let out = f.op(Op::Convert(op), &[a], &[Type::I64]);
        f.ret(&out);
        assert_eq!(
            vm::run(&f.compile(), &[255])[0],
            if op == Conversion::SignExtend {
                u64::MAX
            } else {
                255
            }
        );
    }
    let mut f = Fixture::new(&[Type::I8, Type::I64, Type::I64], &[Type::I64]);
    let args = (0..3).map(|i| f.param(0, i)).collect::<Vec<_>>();
    let out = f.op(Op::Select, &args, &[Type::I64]);
    f.ret(&out);
    assert_eq!(vm::run(&f.compile(), &[1, 42, 7])[0], 42);
    assert_eq!(vm::run(&f.compile(), &[0, 42, 7])[0], 7);
}

#[test]
fn leaf_and_unsupported_operation_errors_are_contextual() {
    let mut f = Fixture::new(&[], &[]);
    f.ret(&[]);
    assert_eq!(vm::run(&f.compile(), &[]), [0, 0]);
    f.body.poll_interface.leaf = false;
    assert!(
        codegen::compile_leaf(&f.body)
            .unwrap_err()
            .0
            .contains("PollFreeLeaf")
    );
    f.body.poll_interface.leaf = true;
    for op in [
        Op::FConst(0),
        Op::TrapIf,
        Op::SafepointPoll {
            interval: std::num::NonZeroU32::new(1).unwrap(),
        },
    ] {
        let mut f = Fixture::new(&[], &[]);
        f.op(op, &[], &[]);
        f.ret(&[]);
        let error = codegen::compile_leaf(&f.body).unwrap_err().0;
        assert!(error.contains("a64_leaf_fixture block 0 site 0"), "{error}");
    }
    let mut f = Fixture::new(&[Type::I64, Type::I64], &[Type::I64]);
    let (a, b) = (f.param(0, 0), f.param(0, 1));
    let out = f.op(Op::Integer(IntOp::DivUnsigned), &[a, b], &[Type::I64]);
    f.ret(&out);
    // 非零非常量除数经 `TrapIf(divisor == 0)` → brk 陷阱保护后与 udiv 同层编译。
    let fragment = codegen::compile_leaf(&f.body).unwrap();
    assert!(
        fragment
            .bytes
            .windows(4)
            .any(|w| w == [0x00, 0x00, 0x20, 0xd4]),
        "除零路径必须落地 brk #0 陷阱"
    );
    f.body.blocks[0].instructions.end = u32::MAX;
    assert!(codegen::compile_leaf(&f.body).is_err());
}
