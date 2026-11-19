//! Scalar IEEE operations and bit-preserving SIMD transport.
use super::{Builder, bits, reg, signature};
use crate::{
    a64::{
        Error,
        inst::Operand,
        machine::{self, Access, Item, MReg},
        reg::Width,
    },
    lir::body::{Body, Condition, Conversion, FloatOp, Op, Type, ValueId},
};

fn name(ty: Type) -> Result<&'static str, Error> {
    match ty {
        Type::F32 => Ok("S"),
        Type::F64 => Ok("D"),
        _ => Err(crate::a64::error("expected scalar FP type")),
    }
}

fn operand(id: ValueId, ty: Type, access: Access) -> machine::MOperand {
    reg(id.0, crate::a64::fp::width(ty), access)
}

pub fn lower(
    body: &Body,
    op: &Op,
    args: &[ValueId],
    results: &[ValueId],
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    let [dest] = results else {
        return Err(crate::a64::error("FP operation needs one result"));
    };
    let target = body.values[dest.index()].kind.ty;
    match op {
        Op::FConst(bits) => {
            signature(body, args, results, &[], &[target])?;
            let lane = name(target)?;
            let constant = b.constant(if target == Type::F32 {
                *bits & u64::from(u32::MAX)
            } else {
                *bits
            })?;
            b.emit(
                &format!(
                    "fmov.{lane}.{}",
                    if target == Type::F32 { "W" } else { "X" }
                ),
                vec![
                    operand(*dest, target, Access::Write),
                    reg(
                        constant,
                        if target == Type::F32 {
                            Width::W
                        } else {
                            Width::X
                        },
                        Access::Read,
                    ),
                ],
            )
        }
        Op::Float(op) => {
            let lane = name(target)?;
            let expected = if *op == FloatOp::Neg {
                vec![target]
            } else {
                vec![target; 2]
            };
            signature(body, args, results, &expected, &[target])?;
            let mnemonic = match op {
                FloatOp::Add => "fadd",
                FloatOp::Sub => "fsub",
                FloatOp::Mul => "fmul",
                FloatOp::Div => "fdiv",
                FloatOp::Neg => "fneg",
            };
            let mut operands = vec![operand(*dest, target, Access::Write)];
            operands.extend(args.iter().map(|id| operand(*id, target, Access::Read)));
            b.emit(
                &format!(
                    "{mnemonic}.{lane}{}",
                    if *op == FloatOp::Neg { ".reg" } else { "" }
                ),
                operands,
            )
        }
        Op::Compare { condition, .. } => {
            let [left, right] = args else {
                return Err(crate::a64::error("FP compare needs two operands"));
            };
            let ty = body.values[left.index()].kind.ty;
            let lane = name(ty)?;
            signature(body, args, results, &[ty, ty], &[Type::I8])?;
            b.emit(
                &format!("fcmp.{lane}.reg"),
                vec![
                    operand(*left, ty, Access::Read),
                    operand(*right, ty, Access::Read),
                ],
            )?;
            let code = match condition {
                Condition::Eq => 0,
                Condition::Ne => 1,
                Condition::Lt => 4,
                Condition::Le => 9,
                Condition::Gt => 12,
                Condition::Ge => 10,
            };
            b.emit(
                "csinc.X",
                vec![
                    machine::write(dest.0),
                    machine::zero(),
                    machine::zero(),
                    machine::fixed(Operand::Condition(code ^ 1)),
                ],
            )
        }
        Op::Select => select(body, args, results, b),
        Op::Convert(conversion) => convert(body, *conversion, args, results, b),
        _ => Err(crate::a64::error("unsupported FP/vector operation")),
    }
}

fn select(
    body: &Body,
    args: &[ValueId],
    results: &[ValueId],
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    let [condition, yes, no] = args else {
        return Err(crate::a64::error("FP select needs three operands"));
    };
    let target = body.values[results[0].index()].kind.ty;
    signature(body, args, results, &[Type::I8, target, target], &[target])?;
    b.emit(
        "subs.X.imm",
        vec![
            machine::zero(),
            machine::read(condition.0),
            machine::fixed(Operand::AddImm {
                imm12: 0,
                shift12: false,
            }),
        ],
    )?;
    if matches!(target, Type::V128(_)) {
        let mask = b.temp()?;
        b.emit(
            "csinv.X",
            vec![
                machine::write(mask),
                machine::zero(),
                machine::zero(),
                machine::fixed(Operand::Condition(0)),
            ],
        )?;
        let vector = b.function.temp(target)?;
        b.emit(
            "dup.Q.X",
            vec![reg(vector, Width::Q, Access::Write), machine::read(mask)],
        )?;
        b.emit(
            "bsl.Q",
            vec![
                reg(vector, Width::Q, Access::ReadWrite),
                operand(*yes, target, Access::Read),
                operand(*no, target, Access::Read),
            ],
        )?;
        b.push(Item::Copies(vec![(
            MReg::Virtual(vector),
            MReg::Virtual(results[0].0),
        )]));
        Ok(())
    } else {
        b.emit(
            &format!("fcsel.{}", name(target)?),
            vec![
                operand(results[0], target, Access::Write),
                operand(*yes, target, Access::Read),
                operand(*no, target, Access::Read),
                machine::fixed(Operand::Condition(1)),
            ],
        )
    }
}

fn convert(
    body: &Body,
    conversion: Conversion,
    args: &[ValueId],
    results: &[ValueId],
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    let [source] = args else {
        return Err(crate::a64::error("FP conversion needs one input"));
    };
    let dest = results[0];
    let from = body.values[source.index()].kind.ty;
    let to = body.values[dest.index()].kind.ty;
    match conversion {
        Conversion::IntToFloat { signed } if from.integer() => {
            let source_bits = bits(from)?;
            let id = if source_bits < 64 {
                let temp = b.temp()?;
                b.normalize(source.0, temp, source_bits, signed)?;
                temp
            } else {
                source.0
            };
            b.emit(
                &format!("{}.{}.X", if signed { "scvtf" } else { "ucvtf" }, name(to)?),
                vec![operand(dest, to, Access::Write), machine::read(id)],
            )
        }
        Conversion::FloatToInt { signed } if to.integer() => {
            let lane = name(from)?;
            b.emit(
                &format!("fcvt{}.{}.X", if signed { "zs" } else { "zu" }, lane),
                vec![machine::write(dest.0), operand(*source, from, Access::Read)],
            )?;
            b.normalize(dest.0, dest.0, bits(to)?, false)
        }
        Conversion::FloatResize if from != to => b.emit(
            &format!("fcvt.{}.{}", name(to)?, name(from)?),
            vec![
                operand(dest, to, Access::Write),
                operand(*source, from, Access::Read),
            ],
        ),
        Conversion::Bitcast if from.bytes() == to.bytes() => {
            if crate::a64::abi::class(from) == crate::a64::abi::class(to) {
                b.push(Item::Copies(vec![(
                    MReg::Virtual(source.0),
                    MReg::Virtual(dest.0),
                )]));
                return Ok(());
            }
            let (destination, source_width, key) = match (from, to) {
                (Type::I32, Type::F32) => (Width::S, Width::W, "fmov.S.W"),
                (Type::I64, Type::F64) => (Width::D, Width::X, "fmov.D.X"),
                (Type::F32, Type::I32) => (Width::W, Width::S, "fmov.W.S"),
                (Type::F64, Type::I64) => (Width::X, Width::D, "fmov.X.D"),
                _ => return Err(crate::a64::error("invalid FP bitcast")),
            };
            b.emit(
                key,
                vec![
                    reg(dest.0, destination, Access::Write),
                    reg(source.0, source_width, Access::Read),
                ],
            )
        }
        _ => Err(crate::a64::error("invalid FP conversion signature")),
    }
}
