//! 数值、浮点、比较、转换与选择 lowering。
//!
//! 宽度规则：值搬运（`mov`）用 32/64 位形式以维持零扩展不变量；算术与比较用值的实际
//! 宽度（8/16 位 ALU 写只改低位，输入的高位零保持不变）。

use crate::backend::x64::inst::Operand;
use crate::backend::x64::reg::{Gpr, Reg};
use crate::backend::x64::table::{Access, OperandKind};
use crate::lir::body::{Condition, Conversion, FloatOp, IntOp, Op, Type};

use super::{
    Builder, LoweringError, SiteValue, gpr, imm, integer_bits, move_kinds, reg, scalar_type,
    unary_kinds, wide_kinds,
};

const R32_IMM32: &[OperandKind] = &[OperandKind::R32, OperandKind::Imm32];
const R64_IMM32: &[OperandKind] = &[OperandKind::R64, OperandKind::Imm32];
const R64_IMM64: &[OperandKind] = &[OperandKind::R64, OperandKind::Imm64];
const RM8_IMM8: &[OperandKind] = &[OperandKind::Rm8, OperandKind::Imm8];
const RM8_R8: &[OperandKind] = &[OperandKind::Rm8, OperandKind::R8];
const RM32_R32: &[OperandKind] = &[OperandKind::Rm32, OperandKind::R32];
const RM64_R64: &[OperandKind] = &[OperandKind::Rm64, OperandKind::R64];
const RM8: &[OperandKind] = &[OperandKind::Rm8];
const RM32: &[OperandKind] = &[OperandKind::Rm32];
const RM64: &[OperandKind] = &[OperandKind::Rm64];
const RM32_IMM8: &[OperandKind] = &[OperandKind::Rm32, OperandKind::Imm8];
const RM64_IMM8: &[OperandKind] = &[OperandKind::Rm64, OperandKind::Imm8];
const RM8_R32: &[OperandKind] = &[OperandKind::Rm8, OperandKind::R32];
const RM16_R32: &[OperandKind] = &[OperandKind::Rm16, OperandKind::R32];
const RM8_R64: &[OperandKind] = &[OperandKind::Rm8, OperandKind::R64];
const RM16_R64: &[OperandKind] = &[OperandKind::Rm16, OperandKind::R64];
const RM32_R64: &[OperandKind] = &[OperandKind::Rm32, OperandKind::R64];
const RM32_XMM: &[OperandKind] = &[OperandKind::Rm32, OperandKind::Xmm];
const RM64_XMM: &[OperandKind] = &[OperandKind::Rm64, OperandKind::Xmm];
const XMMRM_XMM: &[OperandKind] = &[OperandKind::XmmRm, OperandKind::Xmm];
const XMMRM_R32: &[OperandKind] = &[OperandKind::XmmRm, OperandKind::R32];
const XMMRM_R64: &[OperandKind] = &[OperandKind::XmmRm, OperandKind::R64];
const RM32_CL: &[OperandKind] = &[OperandKind::Rm32, OperandKind::Cl];
const RM64_CL: &[OperandKind] = &[OperandKind::Rm64, OperandKind::Cl];
const RM8_CL: &[OperandKind] = &[OperandKind::Rm8, OperandKind::Cl];
const RM16_CL: &[OperandKind] = &[OperandKind::Rm16, OperandKind::Cl];
const REL32: &[OperandKind] = &[OperandKind::Rel32];

fn shift_count_kinds(bits: u32) -> &'static [OperandKind] {
    match bits {
        8 => RM8_CL,
        16 => RM16_CL,
        32 => RM32_CL,
        _ => RM64_CL,
    }
}

pub(super) fn lower(
    op: &Op,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match op {
        Op::IConst(value) => iconst(*value, results, builder),
        Op::FConst(value) => fconst(*value, results, builder),
        Op::Integer(inner) => integer(*inner, operands, results, builder),
        Op::Float(inner) => float(*inner, operands, results, builder),
        Op::Compare { condition, signed } => {
            compare(*condition, *signed, operands, results, builder)
        }
        Op::Convert(conversion) => convert(*conversion, operands, results, builder),
        Op::Select => select(operands, results, builder),
        _ => Err(LoweringError::InvalidOperands),
    }
}

fn iconst(value: u64, results: &[SiteValue], builder: &mut Builder) -> Result<(), LoweringError> {
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    match result.ty.ty {
        Type::I8 | Type::I16 | Type::I32 => {
            builder.emit(
                "mov",
                R32_IMM32,
                Access::Write,
                vec![reg(result.reg), imm(value & 0xFFFF_FFFF)],
            );
        }
        Type::I64 => {
            if value <= u64::from(u32::MAX) {
                builder.emit(
                    "mov",
                    R32_IMM32,
                    Access::Write,
                    vec![reg(result.reg), imm(value)],
                );
            } else if i32::try_from(value.cast_signed()).is_ok() {
                builder.emit(
                    "mov",
                    R64_IMM32,
                    Access::Write,
                    vec![reg(result.reg), imm(value & 0xFFFF_FFFF)],
                );
            } else {
                builder.emit(
                    "mov",
                    R64_IMM64,
                    Access::Write,
                    vec![reg(result.reg), imm(value)],
                );
            }
        }
        Type::Ptr => {
            if value != 0 {
                return Err(LoweringError::InvalidOperands);
            }
            builder.emit(
                "mov",
                R32_IMM32,
                Access::Write,
                vec![reg(result.reg), imm(0)],
            );
        }
        _ => return Err(LoweringError::InvalidOperands),
    }
    Ok(())
}

fn fconst(value: u64, results: &[SiteValue], builder: &mut Builder) -> Result<(), LoweringError> {
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    builder.clobber_gpr(Gpr::R11);
    match result.ty.ty {
        Type::F32 => {
            builder.emit(
                "mov",
                R32_IMM32,
                Access::Write,
                vec![reg(gpr(Gpr::R11)), imm(value & 0xFFFF_FFFF)],
            );
            builder.emit(
                "movd",
                RM32_XMM,
                Access::Read,
                vec![reg(gpr(Gpr::R11)), reg(result.reg)],
            );
        }
        Type::F64 => {
            builder.emit(
                "mov",
                R64_IMM64,
                Access::Write,
                vec![reg(gpr(Gpr::R11)), imm(value)],
            );
            builder.emit(
                "movq",
                RM64_XMM,
                Access::Read,
                vec![reg(gpr(Gpr::R11)), reg(result.reg)],
            );
        }
        _ => return Err(LoweringError::InvalidOperands),
    }
    Ok(())
}

fn integer(
    op: IntOp,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match op {
        IntOp::Add | IntOp::Sub | IntOp::And | IntOp::Or | IntOp::Xor => {
            binary_integer(op, operands, results, builder)
        }
        IntOp::Neg | IntOp::Not => unary_integer(op, operands, results, builder),
        IntOp::Mul => multiply(operands, results, builder),
        IntOp::DivSigned | IntOp::DivUnsigned | IntOp::RemSigned | IntOp::RemUnsigned => {
            divide(op, operands, results, builder)
        }
        IntOp::Shl | IntOp::ShrSigned | IntOp::ShrUnsigned => shift(op, operands, results, builder),
        IntOp::AddCarry | IntOp::SubBorrow => carry_chain(op, operands, results, builder),
        IntOp::MulWide => multiply_wide(operands, results, builder),
    }
}

fn binary_integer(
    op: IntOp,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = integer_bits(scalar_type(result)?)?;
    let mnemonic = match op {
        IntOp::Add => "add",
        IntOp::Sub => "sub",
        IntOp::And => "and",
        IntOp::Or => "or",
        IntOp::Xor => "xor",
        _ => return Err(LoweringError::InvalidOperands),
    };
    builder.emit(
        "mov",
        move_kinds(bits),
        Access::Write,
        vec![reg(result.reg), reg(left.reg)],
    );
    builder.emit(
        mnemonic,
        wide_kinds(bits),
        Access::ReadWrite,
        vec![reg(result.reg), reg(right.reg)],
    );
    Ok(())
}

fn unary_integer(
    op: IntOp,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [operand] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = integer_bits(scalar_type(result)?)?;
    let mnemonic = match op {
        IntOp::Neg => "neg",
        IntOp::Not => "not",
        _ => return Err(LoweringError::InvalidOperands),
    };
    builder.emit(
        "mov",
        move_kinds(bits),
        Access::Write,
        vec![reg(result.reg), reg(operand.reg)],
    );
    builder.emit(
        mnemonic,
        unary_kinds(bits),
        Access::ReadWrite,
        vec![reg(result.reg)],
    );
    Ok(())
}

fn multiply(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = integer_bits(scalar_type(result)?)?;
    builder.emit(
        "mov",
        move_kinds(bits),
        Access::Write,
        vec![reg(result.reg), reg(left.reg)],
    );
    // 8 位没有两操作数 `imul`；按 32 位相乘后再掩回单字节（低 8 位即 8 位乘积）。
    let multiply_kinds = if bits == 8 {
        RM32_R32
    } else {
        wide_kinds(bits)
    };
    // `imul r, r/m` 的目标在 ModRM.reg：向量的第一个元素是 r/m 源。
    builder.emit(
        "imul",
        multiply_kinds,
        Access::Read,
        vec![reg(right.reg), reg(result.reg)],
    );
    if bits == 8 {
        // 8 位乘法只写低位；补零扩展回到规范形。
        builder.emit(
            "movzx",
            RM8_R32,
            Access::Read,
            vec![reg(result.reg), reg(result.reg)],
        );
    }
    Ok(())
}

fn divide(
    op: IntOp,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = integer_bits(scalar_type(result)?)?;
    let signed = matches!(op, IntOp::DivSigned | IntOp::RemSigned);
    let quotient = matches!(op, IntOp::DivSigned | IntOp::DivUnsigned);
    builder.clobber_gpr(Gpr::Rax);
    builder.clobber_gpr(Gpr::Rcx);
    builder.clobber_gpr(Gpr::Rdx);
    match bits {
        8 | 16 => divide_narrow(bits, signed, quotient, left, right, result, builder),
        32 => divide_word(signed, quotient, left, right, result, builder),
        _ => divide_wide(signed, quotient, left, right, result, builder),
    }
}

/// 8/16 位除法：把源符号（或零）扩展到 32 位后做 `idiv`/`div`。
fn divide_narrow(
    bits: u32,
    signed: bool,
    quotient: bool,
    left: &SiteValue,
    right: &SiteValue,
    result: &SiteValue,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    // 8/16 位宽度都走 32 位除法。
    let source_kinds = if bits == 8 { RM8_R32 } else { RM16_R32 };
    let result_kinds = source_kinds;
    let extend = if signed { "movsx" } else { "movzx" };
    builder.emit(
        extend,
        source_kinds,
        Access::Read,
        vec![reg(left.reg), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        extend,
        source_kinds,
        Access::Read,
        vec![reg(right.reg), reg(gpr(Gpr::Rcx))],
    );
    if signed {
        builder.emit("cdq", &[], Access::Read, Vec::new());
    } else {
        builder.emit(
            "xor",
            RM32_R32,
            Access::ReadWrite,
            vec![reg(gpr(Gpr::Rdx)), reg(gpr(Gpr::Rdx))],
        );
    }
    builder.emit(
        if signed { "idiv" } else { "div" },
        RM32,
        Access::Read,
        vec![reg(gpr(Gpr::Rcx))],
    );
    let source = if quotient { Gpr::Rax } else { Gpr::Rdx };
    builder.emit(
        "movzx",
        result_kinds,
        Access::Read,
        vec![reg(gpr(source)), reg(result.reg)],
    );
    Ok(())
}

/// 32 位除法：有符号把源提升到 64 位避免 `INT_MIN` 陷阱。
fn divide_word(
    signed: bool,
    quotient: bool,
    left: &SiteValue,
    right: &SiteValue,
    result: &SiteValue,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    if signed {
        builder.emit(
            "movsxd",
            RM32_R64,
            Access::Read,
            vec![reg(left.reg), reg(gpr(Gpr::Rax))],
        );
        builder.emit(
            "movsxd",
            RM32_R64,
            Access::Read,
            vec![reg(right.reg), reg(gpr(Gpr::Rcx))],
        );
        builder.emit("cqo", &[], Access::Read, Vec::new());
        builder.emit("idiv", RM64, Access::Read, vec![reg(gpr(Gpr::Rcx))]);
    } else {
        builder.emit(
            "mov",
            RM32_R32,
            Access::Write,
            vec![reg(gpr(Gpr::Rax)), reg(left.reg)],
        );
        builder.emit(
            "mov",
            RM32_R32,
            Access::Write,
            vec![reg(gpr(Gpr::Rcx)), reg(right.reg)],
        );
        builder.emit(
            "xor",
            RM32_R32,
            Access::ReadWrite,
            vec![reg(gpr(Gpr::Rdx)), reg(gpr(Gpr::Rdx))],
        );
        builder.emit("div", RM32, Access::Read, vec![reg(gpr(Gpr::Rcx))]);
    }
    let source = if quotient { Gpr::Rax } else { Gpr::Rdx };
    builder.emit(
        "mov",
        RM32_R32,
        Access::Write,
        vec![reg(result.reg), reg(gpr(source))],
    );
    Ok(())
}

/// 64 位除法：有符号 `MIN / -1` 会触发硬件陷阱，按语言规则单独环绕。
fn divide_wide(
    signed: bool,
    quotient: bool,
    left: &SiteValue,
    right: &SiteValue,
    result: &SiteValue,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), reg(left.reg)],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rcx)), reg(right.reg)],
    );
    if !signed {
        builder.emit(
            "xor",
            RM32_R32,
            Access::ReadWrite,
            vec![reg(gpr(Gpr::Rdx)), reg(gpr(Gpr::Rdx))],
        );
        builder.emit("div", RM64, Access::Read, vec![reg(gpr(Gpr::Rcx))]);
        let source = if quotient { Gpr::Rax } else { Gpr::Rdx };
        builder.emit(
            "mov",
            RM64_R64,
            Access::Write,
            vec![reg(result.reg), reg(gpr(source))],
        );
        return Ok(());
    }
    let minus_one = builder.label();
    let end = builder.label();
    builder.emit(
        "cmp",
        RM64_IMM8,
        Access::Read,
        vec![reg(gpr(Gpr::Rcx)), imm(0xFF)],
    );
    builder.emit("je", REL32, Access::Read, vec![Operand::Label(minus_one)]);
    builder.emit("cqo", &[], Access::Read, Vec::new());
    builder.emit("idiv", RM64, Access::Read, vec![reg(gpr(Gpr::Rcx))]);
    let source = if quotient { Gpr::Rax } else { Gpr::Rdx };
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(result.reg), reg(gpr(source))],
    );
    builder.emit("jmp", REL32, Access::Read, vec![Operand::Label(end)]);
    builder.define(minus_one);
    if quotient {
        builder.emit(
            "mov",
            RM64_R64,
            Access::Write,
            vec![reg(result.reg), reg(left.reg)],
        );
        builder.emit("neg", RM64, Access::ReadWrite, vec![reg(result.reg)]);
    } else {
        builder.emit(
            "xor",
            RM32_R32,
            Access::ReadWrite,
            vec![reg(result.reg), reg(result.reg)],
        );
    }
    builder.define(end);
    Ok(())
}

fn shift(
    op: IntOp,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [value, amount] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = integer_bits(scalar_type(result)?)?;
    builder.clobber_gpr(Gpr::Rcx);
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rcx)), reg(amount.reg)],
    );
    if bits < 32 {
        // 硬件只按 32/64 取模；语言按操作数位宽取模。
        builder.emit(
            "and",
            RM32_IMM8,
            Access::ReadWrite,
            vec![reg(gpr(Gpr::Rcx)), imm(u64::from(bits - 1))],
        );
    }
    let count_kinds = shift_count_kinds(bits);
    match op {
        IntOp::Shl | IntOp::ShrUnsigned => {
            let mnemonic = if op == IntOp::Shl { "shl" } else { "shr" };
            builder.emit(
                "mov",
                move_kinds(bits),
                Access::Write,
                vec![reg(result.reg), reg(value.reg)],
            );
            builder.emit(
                mnemonic,
                count_kinds,
                Access::ReadWrite,
                vec![reg(result.reg), reg(gpr(Gpr::Rcx))],
            );
        }
        IntOp::ShrSigned => {
            if bits <= 16 {
                let kinds = if bits == 8 { RM8_R32 } else { RM16_R32 };
                builder.emit(
                    "movsx",
                    kinds,
                    Access::Read,
                    vec![reg(value.reg), reg(result.reg)],
                );
                builder.emit(
                    "sar",
                    RM32_CL,
                    Access::ReadWrite,
                    vec![reg(result.reg), reg(gpr(Gpr::Rcx))],
                );
                builder.emit(
                    "movzx",
                    kinds,
                    Access::Read,
                    vec![reg(result.reg), reg(result.reg)],
                );
            } else {
                builder.emit(
                    "mov",
                    move_kinds(bits),
                    Access::Write,
                    vec![reg(result.reg), reg(value.reg)],
                );
                builder.emit(
                    "sar",
                    count_kinds,
                    Access::ReadWrite,
                    vec![reg(result.reg), reg(gpr(Gpr::Rcx))],
                );
            }
        }
        _ => return Err(LoweringError::InvalidOperands),
    }
    Ok(())
}

fn carry_chain(
    op: IntOp,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right, carry_in] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [sum, carry_out] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let mnemonic = if op == IntOp::AddCarry { "add" } else { "sub" };
    builder.clobber_gpr(Gpr::R11);
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(sum.reg), reg(left.reg)],
    );
    builder.emit(
        mnemonic,
        RM64_R64,
        Access::ReadWrite,
        vec![reg(sum.reg), reg(right.reg)],
    );
    builder.emit("setb", RM8, Access::Write, vec![reg(gpr(Gpr::R11))]);
    // 第二段也用 `add`/`sub`：`adc`/`sbb` 会把上一段的 CF 再加一次，而 `carry_in` 本身是 0/1 值。
    builder.emit(
        mnemonic,
        RM64_R64,
        Access::ReadWrite,
        vec![reg(sum.reg), reg(carry_in.reg)],
    );
    builder.emit("setb", RM8, Access::Write, vec![reg(carry_out.reg)]);
    builder.emit(
        "or",
        RM8_R8,
        Access::ReadWrite,
        vec![reg(carry_out.reg), reg(gpr(Gpr::R11))],
    );
    builder.emit(
        "movzx",
        RM8_R32,
        Access::Read,
        vec![reg(carry_out.reg), reg(carry_out.reg)],
    );
    Ok(())
}

fn multiply_wide(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [low, high] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    builder.clobber_gpr(Gpr::Rax);
    builder.clobber_gpr(Gpr::Rdx);
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), reg(left.reg)],
    );
    builder.emit("imul", RM64, Access::Read, vec![reg(right.reg)]);
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(low.reg), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(high.reg), reg(gpr(Gpr::Rdx))],
    );
    Ok(())
}

fn float(
    op: FloatOp,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    match op {
        FloatOp::Neg => {
            let [operand] = operands else {
                return Err(LoweringError::InvalidOperands);
            };
            let (move_mnemonic, move_kinds, bit) = match result.ty.ty {
                Type::F32 => ("movd", RM32_XMM, 31),
                Type::F64 => ("movq", RM64_XMM, 63),
                _ => return Err(LoweringError::InvalidOperands),
            };
            let flip_kinds = if bit == 31 { RM32_IMM8 } else { RM64_IMM8 };
            builder.clobber_gpr(Gpr::R11);
            builder.emit(
                move_mnemonic,
                move_kinds,
                Access::Write,
                vec![reg(gpr(Gpr::R11)), reg(operand.reg)],
            );
            builder.emit(
                "btc",
                flip_kinds,
                Access::ReadWrite,
                vec![reg(gpr(Gpr::R11)), imm(bit)],
            );
            builder.emit(
                move_mnemonic,
                move_kinds,
                Access::Read,
                vec![reg(gpr(Gpr::R11)), reg(result.reg)],
            );
        }
        _ => {
            let [left, right] = operands else {
                return Err(LoweringError::InvalidOperands);
            };
            let (move_mnemonic, op_mnemonic) = match (result.ty.ty, op) {
                (Type::F32, FloatOp::Add) => ("movss", "addss"),
                (Type::F32, FloatOp::Sub) => ("movss", "subss"),
                (Type::F32, FloatOp::Mul) => ("movss", "mulss"),
                (Type::F32, FloatOp::Div) => ("movss", "divss"),
                (Type::F64, FloatOp::Add) => ("movsd", "addsd"),
                (Type::F64, FloatOp::Sub) => ("movsd", "subsd"),
                (Type::F64, FloatOp::Mul) => ("movsd", "mulsd"),
                (Type::F64, FloatOp::Div) => ("movsd", "divsd"),
                _ => return Err(LoweringError::InvalidOperands),
            };
            builder.emit(
                move_mnemonic,
                XMMRM_XMM,
                Access::Read,
                vec![reg(left.reg), reg(result.reg)],
            );
            builder.emit(
                op_mnemonic,
                XMMRM_XMM,
                Access::Read,
                vec![reg(right.reg), reg(result.reg)],
            );
        }
    }
    Ok(())
}

fn compare(
    condition: Condition,
    signed: bool,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [left, right] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    match left.ty.ty {
        Type::F32 | Type::F64 => float_compare(condition, left, right, results, builder),
        ty => {
            let bits = integer_bits(ty)?;
            builder.emit(
                "cmp",
                wide_kinds(bits),
                Access::Read,
                vec![reg(left.reg), reg(right.reg)],
            );
            let [result] = results else {
                return Err(LoweringError::InvalidOperands);
            };
            if result.ty.ty == Type::Flags {
                // 分支直接消费 `cmp` 建立的标志位。
                return Ok(());
            }
            builder.clobber_gpr(Gpr::R11);
            builder.emit(
                setcc_mnemonic(condition, signed),
                RM8,
                Access::Write,
                vec![reg(gpr(Gpr::R11))],
            );
            builder.emit(
                "movzx",
                RM8_R32,
                Access::Read,
                vec![reg(gpr(Gpr::R11)), reg(result.reg)],
            );
            Ok(())
        }
    }
}

fn setcc_mnemonic(condition: Condition, signed: bool) -> &'static str {
    match condition {
        Condition::Eq => "sete",
        Condition::Ne => "setne",
        Condition::Lt => {
            if signed {
                "setl"
            } else {
                "setb"
            }
        }
        Condition::Le => {
            if signed {
                "setle"
            } else {
                "setbe"
            }
        }
        Condition::Gt => {
            if signed {
                "setg"
            } else {
                "seta"
            }
        }
        Condition::Ge => {
            if signed {
                "setge"
            } else {
                "setae"
            }
        }
    }
}

/// 浮点比较：结果布尔物化到 `target`（8 位寄存器），NaN 语义按条件修正。
fn float_compare_boolean(
    condition: Condition,
    left: &SiteValue,
    right: &SiteValue,
    target: Reg,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let mnemonic = if left.ty.ty == Type::F32 {
        "ucomiss"
    } else {
        "ucomisd"
    };
    // `ucomis*` 的「第一个操作数」在 ModRM.reg，即向量第二位；先给源后给目标方向。
    builder.emit(
        mnemonic,
        XMMRM_XMM,
        Access::Read,
        vec![reg(right.reg), reg(left.reg)],
    );
    builder.emit(
        setcc_mnemonic(condition, false),
        RM8,
        Access::Write,
        vec![reg(target)],
    );
    let end = builder.label();
    builder.emit("jnp", REL32, Access::Read, vec![Operand::Label(end)]);
    if condition == Condition::Ne {
        // 无序（NaN）时「不等于」为真。
        builder.emit("mov", RM8_IMM8, Access::Write, vec![reg(target), imm(1)]);
    } else {
        // 有序条件在 NaN 时必须为假。
        builder.emit(
            "xor",
            RM8_R8,
            Access::ReadWrite,
            vec![reg(target), reg(target)],
        );
    }
    builder.define(end);
    Ok(())
}

fn float_compare(
    condition: Condition,
    left: &SiteValue,
    right: &SiteValue,
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    builder.clobber_gpr(Gpr::R11);
    float_compare_boolean(condition, left, right, gpr(Gpr::R11), builder)?;
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    if result.ty.ty == Type::Flags {
        // Flags 结果：把布尔物化进 r11b 后补 test 建立 ZF。
        builder.emit(
            "test",
            RM8_R8,
            Access::Read,
            vec![reg(gpr(Gpr::R11)), reg(gpr(Gpr::R11))],
        );
        return Ok(());
    }
    builder.emit(
        "movzx",
        RM8_R32,
        Access::Read,
        vec![reg(gpr(Gpr::R11)), reg(result.reg)],
    );
    Ok(())
}

fn convert(
    conversion: Conversion,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [operand] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let source = scalar_type(operand)?;
    let target = scalar_type(result)?;
    match conversion {
        Conversion::ZeroExtend => zero_extend(target, operand, result, builder),
        Conversion::SignExtend => sign_extend(source, target, operand, result, builder)?,
        Conversion::Truncate => truncate(target, operand, result, builder)?,
        Conversion::IntToFloat { signed } => {
            int_to_float(signed, source, target, operand, result, builder)?
        }
        Conversion::FloatToInt { signed: _ } => {
            float_to_int(source, target, operand, result, builder)?
        }
        Conversion::FloatResize => {
            let mnemonic = match (source, target) {
                (Type::F32, Type::F64) => "cvtss2sd",
                (Type::F64, Type::F32) => "cvtsd2ss",
                _ => return Err(LoweringError::InvalidOperands),
            };
            builder.emit(
                mnemonic,
                XMMRM_XMM,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        Conversion::Bitcast => bitcast(source, target, operand, result, builder)?,
        Conversion::PointerToInt
        | Conversion::IntToPointer
        | Conversion::PointerCast
        | Conversion::RawToReference => {
            let bits = integer_bits(target)?;
            builder.emit(
                "mov",
                move_kinds(bits),
                Access::Write,
                vec![reg(result.reg), reg(operand.reg)],
            );
        }
    }
    Ok(())
}

fn zero_extend(target: Type, operand: &SiteValue, result: &SiteValue, builder: &mut Builder) {
    // 值已是目标宽度的规范形，一次 mov 即可。
    let bits = if target == Type::Ptr {
        64
    } else {
        integer_bits(target).unwrap_or(64)
    };
    builder.emit(
        "mov",
        move_kinds(bits),
        Access::Write,
        vec![reg(result.reg), reg(operand.reg)],
    );
}

fn sign_extend(
    source: Type,
    target: Type,
    operand: &SiteValue,
    result: &SiteValue,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match (source, target) {
        (Type::I8, Type::I16) => {
            // 先符号扩展到 32 位，再对 16 位补零扩展回到规范形。
            builder.emit(
                "movsx",
                RM8_R32,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
            builder.emit(
                "movzx",
                RM16_R32,
                Access::Read,
                vec![reg(result.reg), reg(result.reg)],
            );
        }
        (Type::I8, Type::I32) => {
            builder.emit(
                "movsx",
                RM8_R32,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        (Type::I8, Type::I64) => {
            builder.emit(
                "movsx",
                RM8_R64,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        (Type::I16, Type::I32) => {
            builder.emit(
                "movsx",
                RM16_R32,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        (Type::I16, Type::I64) => {
            builder.emit(
                "movsx",
                RM16_R64,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        (Type::I32, Type::I64) => {
            builder.emit(
                "movsxd",
                RM32_R64,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        _ => return Err(LoweringError::InvalidOperands),
    }
    Ok(())
}

fn truncate(
    target: Type,
    operand: &SiteValue,
    result: &SiteValue,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match target {
        Type::I8 => {
            builder.emit(
                "movzx",
                RM8_R32,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        Type::I16 => {
            builder.emit(
                "movzx",
                RM16_R32,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        Type::I32 | Type::I64 | Type::Ptr => {
            builder.emit(
                "mov",
                RM32_R32,
                Access::Write,
                vec![reg(result.reg), reg(operand.reg)],
            );
        }
        _ => return Err(LoweringError::InvalidOperands),
    }
    Ok(())
}

fn int_to_float(
    signed: bool,
    source: Type,
    target: Type,
    operand: &SiteValue,
    result: &SiteValue,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let mnemonic = match target {
        Type::F32 => "cvtsi2ss",
        Type::F64 => "cvtsi2sd",
        _ => return Err(LoweringError::InvalidOperands),
    };
    if signed {
        match source {
            Type::I8 | Type::I16 => {
                let kinds = if source == Type::I8 {
                    RM8_R32
                } else {
                    RM16_R32
                };
                builder.clobber_gpr(Gpr::Rax);
                builder.emit(
                    "movsx",
                    kinds,
                    Access::Read,
                    vec![reg(operand.reg), reg(gpr(Gpr::Rax))],
                );
                builder.emit(
                    mnemonic,
                    RM32_XMM,
                    Access::Read,
                    vec![reg(gpr(Gpr::Rax)), reg(result.reg)],
                );
            }
            Type::I32 => {
                builder.emit(
                    mnemonic,
                    RM32_XMM,
                    Access::Read,
                    vec![reg(operand.reg), reg(result.reg)],
                );
            }
            Type::I64 => {
                builder.emit(
                    mnemonic,
                    RM64_XMM,
                    Access::Read,
                    vec![reg(operand.reg), reg(result.reg)],
                );
            }
            _ => return Err(LoweringError::InvalidOperands),
        }
        return Ok(());
    }
    match source {
        Type::I8 | Type::I16 => {
            // 规范形已把值零扩展；按 32 位有符号读是正确的。
            builder.emit(
                mnemonic,
                RM32_XMM,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        Type::I32 => {
            builder.clobber_gpr(Gpr::Rax);
            builder.emit(
                "mov",
                RM64_R64,
                Access::Write,
                vec![reg(gpr(Gpr::Rax)), reg(operand.reg)],
            );
            builder.emit(
                mnemonic,
                RM64_XMM,
                Access::Read,
                vec![reg(gpr(Gpr::Rax)), reg(result.reg)],
            );
        }
        Type::I64 => unsigned_wide_to_float(mnemonic, target, operand, result, builder),
        _ => return Err(LoweringError::InvalidOperands),
    }
    Ok(())
}

/// `u64 → float`：最高位为 1 时按 `(v >> 1) | (v & 1)` 转换后翻倍。
fn unsigned_wide_to_float(
    mnemonic: &'static str,
    target: Type,
    operand: &SiteValue,
    result: &SiteValue,
    builder: &mut Builder,
) {
    let big = builder.label();
    let end = builder.label();
    builder.clobber_gpr(Gpr::R11);
    builder.clobber_gpr(Gpr::Rax);
    builder.emit(
        "test",
        RM64_R64,
        Access::Read,
        vec![reg(operand.reg), reg(operand.reg)],
    );
    builder.emit("js", REL32, Access::Read, vec![Operand::Label(big)]);
    builder.emit(
        mnemonic,
        RM64_XMM,
        Access::Read,
        vec![reg(operand.reg), reg(result.reg)],
    );
    builder.emit("jmp", REL32, Access::Read, vec![Operand::Label(end)]);
    builder.define(big);
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::R11)), reg(operand.reg)],
    );
    builder.emit(
        "shr",
        RM64_IMM8,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R11)), imm(1)],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), reg(operand.reg)],
    );
    builder.emit(
        "and",
        RM32_IMM8,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rax)), imm(1)],
    );
    builder.emit(
        "or",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R11)), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        mnemonic,
        RM64_XMM,
        Access::Read,
        vec![reg(gpr(Gpr::R11)), reg(result.reg)],
    );
    let double = if target == Type::F32 {
        "addss"
    } else {
        "addsd"
    };
    builder.emit(
        double,
        XMMRM_XMM,
        Access::Read,
        vec![reg(result.reg), reg(result.reg)],
    );
    builder.define(end);
}

fn float_to_int(
    source: Type,
    target: Type,
    operand: &SiteValue,
    result: &SiteValue,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let mnemonic = match source {
        Type::F32 => "cvttss2si",
        Type::F64 => "cvttsd2si",
        _ => return Err(LoweringError::InvalidOperands),
    };
    let dest_kinds = match target {
        Type::I64 | Type::Ptr => XMMRM_R64,
        Type::I8 | Type::I16 | Type::I32 => XMMRM_R32,
        _ => return Err(LoweringError::InvalidOperands),
    };
    builder.emit(
        mnemonic,
        dest_kinds,
        Access::Read,
        vec![reg(operand.reg), reg(result.reg)],
    );
    if target == Type::I8 || target == Type::I16 {
        let kinds = if target == Type::I8 {
            RM8_R32
        } else {
            RM16_R32
        };
        builder.emit(
            "movzx",
            kinds,
            Access::Read,
            vec![reg(result.reg), reg(result.reg)],
        );
    }
    Ok(())
}

fn bitcast(
    source: Type,
    target: Type,
    operand: &SiteValue,
    result: &SiteValue,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match (source, target) {
        (Type::I32, Type::F32) => {
            builder.emit(
                "movd",
                RM32_XMM,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        (Type::F32, Type::I32) => {
            builder.emit(
                "movd",
                RM32_XMM,
                Access::Write,
                vec![reg(result.reg), reg(operand.reg)],
            );
        }
        (Type::I64, Type::F64) => {
            builder.emit(
                "movq",
                RM64_XMM,
                Access::Read,
                vec![reg(operand.reg), reg(result.reg)],
            );
        }
        (Type::F64, Type::I64) => {
            builder.emit(
                "movq",
                RM64_XMM,
                Access::Write,
                vec![reg(result.reg), reg(operand.reg)],
            );
        }
        _ => return Err(LoweringError::InvalidOperands),
    }
    Ok(())
}

fn select(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [condition, yes, no] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    if condition.ty.ty != Type::I8 {
        return Err(LoweringError::InvalidOperands);
    }
    match result.ty.ty {
        Type::F32 | Type::F64 => {
            let (move_mnemonic, move_kinds) = if result.ty.ty == Type::F32 {
                ("movd", RM32_XMM)
            } else {
                ("movq", RM64_XMM)
            };
            builder.clobber_gpr(Gpr::R11);
            builder.clobber_gpr(Gpr::Rax);
            builder.emit(
                move_mnemonic,
                move_kinds,
                Access::Write,
                vec![reg(gpr(Gpr::R11)), reg(yes.reg)],
            );
            builder.emit(
                move_mnemonic,
                move_kinds,
                Access::Write,
                vec![reg(gpr(Gpr::Rax)), reg(no.reg)],
            );
            builder.emit(
                "test",
                RM8_R8,
                Access::Read,
                vec![reg(condition.reg), reg(condition.reg)],
            );
            builder.emit(
                "cmove",
                RM64_R64,
                Access::Read,
                vec![reg(gpr(Gpr::Rax)), reg(gpr(Gpr::R11))],
            );
            builder.emit(
                move_mnemonic,
                move_kinds,
                Access::Read,
                vec![reg(gpr(Gpr::R11)), reg(result.reg)],
            );
        }
        Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::Ptr => {
            let bits = integer_bits(result.ty.ty)?;
            let cmov_kinds = if bits <= 32 { RM32_R32 } else { RM64_R64 };
            builder.emit(
                "mov",
                move_kinds(bits),
                Access::Write,
                vec![reg(result.reg), reg(yes.reg)],
            );
            builder.emit(
                "test",
                RM8_R8,
                Access::Read,
                vec![reg(condition.reg), reg(condition.reg)],
            );
            builder.emit(
                "cmove",
                cmov_kinds,
                Access::Read,
                vec![reg(no.reg), reg(result.reg)],
            );
        }
        // `V128` 选择没有 SSE2 基线序列；vectorizer 必须保留标量路径。
        _ => {
            return Err(LoweringError::Unsupported {
                op: "Select",
                detail: "SSE2 基线没有按条件选择 128 位值的序列",
            });
        }
    }
    Ok(())
}
