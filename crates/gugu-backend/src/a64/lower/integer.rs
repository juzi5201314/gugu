use super::{Builder, bits, signature};
use crate::a64::{
    Error,
    inst::{Operand, Shift},
    machine::{self, Item, MReg},
};
use crate::lir::body::{Body, Condition, Conversion, IntOp, Op, Type, ValueId};
use machine::{fixed as f, read as r, write as w, zero as z};

pub fn lower(
    body: &Body,
    op: &Op,
    args: &[ValueId],
    results: &[ValueId],
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    match op {
        Op::IConst(value) => {
            let [dest] = results else {
                return Err(crate::a64::error("constant needs one result"));
            };
            if !args.is_empty() {
                return Err(crate::a64::error("constant has operands"));
            }
            let width = bits(body.values[dest.index()].kind.ty)?;
            b.push(Item::Constant {
                dest: dest.0,
                bits: value & mask(width),
            });
            Ok(())
        }
        Op::Integer(inner) => integer(body, *inner, args, results, b),
        Op::Compare { condition, signed } => {
            let [left, right] = args else {
                return Err(crate::a64::error("compare needs two inputs"));
            };
            let [dest] = results else {
                return Err(crate::a64::error("compare needs one result"));
            };
            let ty = body.values[left.index()].kind.ty;
            signature(body, args, results, &[ty, ty], &[Type::I8])?;
            let width = bits(ty)?;
            let (left, right) = (
                extended(b, left.0, width, *signed)?,
                extended(b, right.0, width, *signed)?,
            );
            compare(b, left, right)?;
            boolean(b, dest.0, condition_code(*condition, *signed))
        }
        Op::Convert(conversion) => convert(body, *conversion, args, results, b),
        Op::Select => {
            let [condition, yes, no] = args else {
                return Err(crate::a64::error("select needs three inputs"));
            };
            let [dest] = results else {
                return Err(crate::a64::error("select needs one result"));
            };
            let ty = body.values[yes.index()].kind.ty;
            signature(body, args, results, &[Type::I8, ty, ty], &[ty])?;
            compare_zero(b, condition.0)?;
            b.emit(
                "csel.X",
                vec![w(dest.0), r(yes.0), r(no.0), f(Operand::Condition(1))],
            )
        }
        _ => Err(crate::a64::error("unsupported integer leaf operation")),
    }
}

pub fn mask(bits: u32) -> u64 {
    u64::MAX >> (64 - bits)
}

fn extended(b: &mut Builder<'_>, source: u32, width: u32, signed: bool) -> Result<u32, Error> {
    if !signed || width == 64 {
        return Ok(source);
    }
    let dest = b.temp()?;
    b.normalize(source, dest, width, true)?;
    Ok(dest)
}

fn binary(b: &mut Builder<'_>, op: &str, dest: u32, left: u32, right: u32) -> Result<(), Error> {
    b.emit(
        op,
        vec![w(dest), r(left), r(right), f(Operand::Shift(Shift::Lsl, 0))],
    )
}

pub fn compare(b: &mut Builder<'_>, left: u32, right: u32) -> Result<(), Error> {
    b.emit(
        "subs.X.shift",
        vec![z(), r(left), r(right), f(Operand::Shift(Shift::Lsl, 0))],
    )
}

pub fn compare_zero(b: &mut Builder<'_>, value: u32) -> Result<(), Error> {
    b.emit(
        "subs.X.imm",
        vec![
            z(),
            r(value),
            f(Operand::AddImm {
                imm12: 0,
                shift12: false,
            }),
        ],
    )
}

fn boolean(b: &mut Builder<'_>, dest: u32, condition: u8) -> Result<(), Error> {
    b.emit(
        "csinc.X",
        vec![w(dest), z(), z(), f(Operand::Condition(condition ^ 1))],
    )
}

pub fn condition_code(condition: Condition, signed: bool) -> u8 {
    match (condition, signed) {
        (Condition::Eq, _) => 0,
        (Condition::Ne, _) => 1,
        (Condition::Lt, true) => 11,
        (Condition::Le, true) => 13,
        (Condition::Gt, true) => 12,
        (Condition::Ge, true) => 10,
        (Condition::Lt, false) => 3,
        (Condition::Le, false) => 9,
        (Condition::Gt, false) => 8,
        (Condition::Ge, false) => 2,
    }
}

fn integer(
    body: &Body,
    op: IntOp,
    args: &[ValueId],
    results: &[ValueId],
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    if matches!(op, IntOp::AddCarry | IntOp::SubBorrow | IntOp::MulWide) {
        return wide(body, op, args, results, b);
    }
    let [dest] = results else {
        return Err(crate::a64::error("integer operation needs one result"));
    };
    let ty = body.values[dest.index()].kind.ty;
    if !ty.integer() {
        return Err(crate::a64::error("integer operation needs integer types"));
    }
    let width = bits(ty)?;
    let unary = matches!(op, IntOp::Neg | IntOp::Not);
    signature(
        body,
        args,
        results,
        &vec![ty; if unary { 1 } else { 2 }],
        &[ty],
    )?;
    let left = args[0].0;
    let out = if width < 64 { b.temp()? } else { dest.0 };
    if unary {
        b.emit(
            if op == IntOp::Neg {
                "sub.X.shift"
            } else {
                "orn.X.shift"
            },
            vec![w(out), z(), r(left), f(Operand::Shift(Shift::Lsl, 0))],
        )?;
    } else {
        let right = args[1].0;
        match op {
            IntOp::Add | IntOp::Sub | IntOp::And | IntOp::Or | IntOp::Xor => {
                let key = match op {
                    IntOp::Add => "add.X.shift",
                    IntOp::Sub => "sub.X.shift",
                    IntOp::And => "and.X.shift",
                    IntOp::Or => "orr.X.shift",
                    _ => "eor.X.shift",
                };
                binary(b, key, out, left, right)?;
            }
            IntOp::Mul => b.emit("madd.X", vec![w(out), r(left), r(right), z()])?,
            IntOp::Shl | IntOp::ShrSigned | IntOp::ShrUnsigned => {
                let left = extended(b, left, width, op == IntOp::ShrSigned)?;
                let count = if width < 64 {
                    let count = b.temp()?;
                    b.emit(
                        "and.X.imm",
                        vec![w(count), r(right), f(Operand::Bits(u64::from(width - 1)))],
                    )?;
                    count
                } else {
                    right
                };
                b.emit(
                    match op {
                        IntOp::Shl => "lslv.X",
                        IntOp::ShrSigned => "asrv.X",
                        _ => "lsrv.X",
                    },
                    vec![w(out), r(left), r(count)],
                )?;
            }
            IntOp::DivSigned | IntOp::DivUnsigned | IntOp::RemSigned | IntOp::RemUnsigned => {
                let signed = matches!(op, IntOp::DivSigned | IntOp::RemSigned);
                let left = extended(b, left, width, signed)?;
                let right = extended(b, right, width, signed)?;
                // 除零一律 panic（types.md）：a64 的 sdiv/udiv 不产生硬件异常，显式发射
                // `TrapIf(divisor == 0)` → `brk`，与 x64 `div` 的 #DE 同层语义。
                compare_zero(b, right)?;
                let zero_divisor = b.temp()?;
                boolean(b, zero_divisor, 0)?;
                b.push(Item::TrapIf {
                    condition: zero_divisor,
                });
                let remainder = matches!(op, IntOp::RemSigned | IntOp::RemUnsigned);
                let quotient = if remainder { b.temp()? } else { out };
                b.emit(
                    if signed { "sdiv.X" } else { "udiv.X" },
                    vec![w(quotient), r(left), r(right)],
                )?;
                if remainder {
                    let product = b.temp()?;
                    b.emit("madd.X", vec![w(product), r(quotient), r(right), z()])?;
                    binary(b, "sub.X.shift", out, left, product)?;
                }
            }
            _ => return Err(crate::a64::error("unsupported integer opcode")),
        }
    }
    if width < 64 {
        b.normalize(out, dest.0, width, false)?;
    }
    Ok(())
}

fn wide(
    body: &Body,
    op: IntOp,
    args: &[ValueId],
    results: &[ValueId],
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    signature(
        body,
        args,
        results,
        &vec![Type::I64; if op == IntOp::MulWide { 2 } else { 3 }],
        &[Type::I64, Type::I64],
    )?;
    let (left, right) = (args[0].0, args[1].0);
    if op == IntOp::MulWide {
        b.emit("madd.X", vec![w(results[0].0), r(left), r(right), z()])?;
        return b.emit("umulh", vec![w(results[1].0), r(left), r(right)]);
    }
    let intermediate = b.temp()?;
    let first_carry = b.temp()?;
    let second_carry = b.temp()?;
    let key = if op == IntOp::AddCarry {
        "adds.X.shift"
    } else {
        "subs.X.shift"
    };
    let condition = if op == IntOp::AddCarry { 2 } else { 3 };
    binary(b, key, intermediate, left, right)?;
    boolean(b, first_carry, condition)?;
    binary(b, key, results[0].0, intermediate, args[2].0)?;
    boolean(b, second_carry, condition)?;
    binary(b, "orr.X.shift", results[1].0, first_carry, second_carry)
}

fn convert(
    body: &Body,
    op: Conversion,
    args: &[ValueId],
    results: &[ValueId],
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    let ([source], [dest]) = (args, results) else {
        return Err(crate::a64::error("conversion needs one input and result"));
    };
    let from = bits(body.values[source.index()].kind.ty)?;
    let to = bits(body.values[dest.index()].kind.ty)?;
    match op {
        Conversion::SignExtend if to > from => {
            let wide = extended(b, source.0, from, true)?;
            b.normalize(wide, dest.0, to, false)
        }
        Conversion::ZeroExtend if to > from => b.normalize(source.0, dest.0, from, false),
        Conversion::Truncate if to < from => b.normalize(source.0, dest.0, to, false),
        Conversion::Bitcast
        | Conversion::PointerCast
        | Conversion::PointerToInt
        | Conversion::IntToPointer
            if from == to =>
        {
            b.push(Item::Copies(vec![(
                MReg::Virtual(source.0),
                MReg::Virtual(dest.0),
            )]));
            Ok(())
        }
        _ => Err(crate::a64::error("unsupported integer/pointer conversion")),
    }
}
