//! 地址、内存访问、覆盖计数 lowering。

use crate::backend::x64::inst::{Mem, Operand, RelocKind, RelocTarget, Scale};
use crate::backend::x64::mangle;
use crate::backend::x64::reg::Reg;
use crate::backend::x64::table::{Access, OperandKind};
use crate::lir::body::{Op, Symbol, Type};

use super::{Builder, LoweringError, SiteValue, access_bits, imm, mem_base, move_kinds, reg};

const MEM_R64: &[OperandKind] = &[OperandKind::Mem, OperandKind::R64];
const REL32: &[OperandKind] = &[OperandKind::Rel32];
const RM32: &[OperandKind] = &[OperandKind::Rm32];

pub(super) fn lower(
    op: &Op,
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match op {
        Op::SymbolAddr(symbol) => symbol_addr(symbol, results, builder),
        Op::StackAddr(slot) => stack_addr(*slot, results, builder),
        Op::PtrOffset => ptr_offset(operands, results, builder),
        Op::Load(_) => load(operands, results, builder),
        Op::Store(_) => store(operands, builder),
        Op::Memcpy { bytes } => memory_call("memcpy", operands, *bytes, builder),
        Op::Memmove { bytes } => memory_call("memmove", operands, *bytes, builder),
        Op::Memset { bytes } => memory_call("memset", operands, *bytes, builder),
        Op::CoverageCounter(index) => coverage(*index, builder),
        _ => Err(LoweringError::InvalidOperands),
    }
}

fn symbol_addr(
    symbol: &Symbol,
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![
            Operand::Rip(RelocTarget::Lir(symbol.clone()), 0),
            reg(result.reg),
        ],
    );
    Ok(())
}

fn stack_addr(
    slot: crate::lir::body::SlotId,
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    // 阶段 54 把 Virtual(slot) 换成真实 frame slot；本阶段编码器对虚拟基址已有占位。
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![mem_base(Reg::Virtual(slot.0), 0), reg(result.reg)],
    );
    Ok(())
}

fn ptr_offset(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [base, index] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![
            Operand::Mem(Mem {
                base: Some(base.reg),
                index: Some(index.reg),
                scale: Scale::One,
                disp: 0,
            }),
            reg(result.reg),
        ],
    );
    Ok(())
}

fn load(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let [address] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let [result] = results else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = access_bits(result.ty.ty)?;
    let kinds = match bits {
        8 => &[OperandKind::Rm8, OperandKind::R8][..],
        16 => &[OperandKind::Rm16, OperandKind::R16][..],
        32 => &[OperandKind::Rm32, OperandKind::R32][..],
        _ => &[OperandKind::Rm64, OperandKind::R64][..],
    };
    builder.emit(
        "mov",
        kinds,
        Access::Read,
        vec![mem_base(address.reg, 0), reg(result.reg)],
    );
    Ok(())
}

fn store(operands: &[SiteValue], builder: &mut Builder) -> Result<(), LoweringError> {
    let [address, value] = operands else {
        return Err(LoweringError::InvalidOperands);
    };
    let bits = access_bits(value.ty.ty).or_else(|_| match value.ty.ty {
        Type::F32 => Ok(32),
        Type::F64 | Type::Ptr => Ok(64),
        _ => Err(LoweringError::InvalidOperands),
    })?;
    let kinds = match bits {
        8 => &[OperandKind::Rm8, OperandKind::R8][..],
        16 => &[OperandKind::Rm16, OperandKind::R16][..],
        32 => &[OperandKind::Rm32, OperandKind::R32][..],
        _ => &[OperandKind::Rm64, OperandKind::R64][..],
    };
    builder.emit(
        "mov",
        kinds,
        Access::Write,
        vec![mem_base(address.reg, 0), reg(value.reg)],
    );
    Ok(())
}

fn memory_call(
    name: &'static str,
    operands: &[SiteValue],
    bytes: u64,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let dest = operands.first().ok_or(LoweringError::InvalidOperands)?;
    emit_move(
        builder,
        dest.reg,
        Reg::Gpr(crate::backend::x64::reg::Gpr::Rax),
    )?;
    if name != "memset" {
        let src = operands.get(1).ok_or(LoweringError::InvalidOperands)?;
        emit_move(
            builder,
            src.reg,
            Reg::Gpr(crate::backend::x64::reg::Gpr::Rbx),
        )?;
    } else {
        let value = operands.get(1).ok_or(LoweringError::InvalidOperands)?;
        emit_move(
            builder,
            value.reg,
            Reg::Gpr(crate::backend::x64::reg::Gpr::Rbx),
        )?;
    }
    builder.emit(
        "mov",
        &[OperandKind::R64, OperandKind::Imm32],
        Access::Write,
        vec![
            reg(Reg::Gpr(crate::backend::x64::reg::Gpr::Rcx)),
            imm(bytes),
        ],
    );
    builder.clobber_gpr(crate::backend::x64::reg::Gpr::Rax);
    builder.clobber_gpr(crate::backend::x64::reg::Gpr::Rbx);
    builder.clobber_gpr(crate::backend::x64::reg::Gpr::Rcx);
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(mangle::glue_symbol(name)),
            RelocKind::PcRel32,
        )],
    );
    Ok(())
}

fn coverage(index: u32, builder: &mut Builder) -> Result<(), LoweringError> {
    builder.emit(
        "inc",
        RM32,
        Access::ReadWrite,
        vec![Operand::Rip(RelocTarget::Lir(Symbol::Data(index)), 0)],
    );
    Ok(())
}

fn emit_move(builder: &mut Builder, src: Reg, dest: Reg) -> Result<(), LoweringError> {
    if src == dest {
        return Ok(());
    }
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![reg(dest), reg(src)],
    );
    Ok(())
}
