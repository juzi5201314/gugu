//! 编码与解码共用的形状、范围和 ARMv8.0-A 约束。

use super::{Error, bits, error, inst::*, reg::*, table::*};

pub fn verify(inst: &Inst) -> Result<(), Error> {
    values(inst).map(|_| ())
}

pub(crate) fn values(inst: &Inst) -> Result<Vec<u32>, Error> {
    let form = get(inst.form)?;
    if form.operands.len() != inst.operands.len() {
        return Err(error("操作数个数不匹配"));
    }
    let values = form
        .operands
        .iter()
        .zip(&inst.operands)
        .map(|(slot, op)| pack(slot.kind, op))
        .collect::<Result<Vec<_>, _>>()?;
    constraints(form, &inst.operands)?;
    Ok(values)
}

pub(crate) fn register(reg: Reg, width: Width, sp: bool, vector: bool) -> Result<u32, Error> {
    if reg.width != width {
        return Err(error("寄存器视图宽度不匹配"));
    }
    match reg.kind {
        RegKind::Vector(n) if vector && n < 32 => Ok(u32::from(n)),
        RegKind::Gpr(n) if !vector && n < 31 => Ok(u32::from(n)),
        RegKind::Sp if !vector && sp => Ok(31),
        RegKind::Zero if !vector && !sp => Ok(31),
        _ => Err(error("寄存器种类或编号不适用于此槽")),
    }
}

pub(crate) fn pack(kind: Kind, operand: &Operand) -> Result<u32, Error> {
    match (kind, operand) {
        (Kind::Reg { width, sp, vector }, Operand::Reg(r)) => register(*r, width, sp, vector),
        (Kind::ExtendedReg(width), Operand::Reg(r)) => {
            if r.width != Width::W && !(width == 64 && r.width == Width::X) {
                return Err(error("扩展源宽度错误"));
            }
            register(*r, r.width, false, false)
        }
        (Kind::TestBit(high), Operand::Imm(n)) => {
            let value = n
                .checked_sub(if high { 32 } else { 0 })
                .ok_or_else(|| error("测试位编号越界"))?;
            bits::unsigned(value, 5, 0)
        }
        (
            Kind::Imm {
                bits,
                signed,
                scale,
            },
            Operand::Imm(n),
        ) => {
            if signed {
                bits::signed(*n, bits, scale)
            } else {
                bits::unsigned(*n, bits, scale)
            }
        }
        (Kind::AddImm, Operand::AddImm { imm12, shift12 }) if *imm12 < 4096 => {
            Ok(u32::from(*imm12) | (u32::from(*shift12) << 12))
        }
        (Kind::AddImm, Operand::Target(Target::Symbol { .. })) => Ok(0),
        (Kind::Logical(width), Operand::Bits(value)) => bits::encode_logical(*value, width),
        (Kind::Wide(width), Operand::Wide { imm16, shift })
            if *shift % 16 == 0 && *shift < width =>
        {
            Ok(u32::from(*imm16) | (u32::from(*shift / 16) << 16))
        }
        (Kind::Shift { width, ror }, Operand::Shift(shift, n))
            if *n < width && (ror || *shift != Shift::Ror) =>
        {
            Ok(u32::from(*n) | ((*shift as u32) << 6))
        }
        (Kind::Extend(_), Operand::Extend(ext, n)) if *n <= 4 => {
            Ok(((*ext as u32) << 3) | u32::from(*n))
        }
        (Kind::Condition, Operand::Condition(c)) if *c < 16 => Ok(u32::from(*c)),
        (Kind::Address { mode, size }, Operand::Address(a)) => address(a, mode, size),
        (Kind::Target { bits, scale, .. }, Operand::Target(Target::Offset(n))) => {
            bits::signed(*n, bits, scale)
        }
        (Kind::Target { reloc: Some(_), .. }, Operand::Target(Target::Symbol { .. })) => Ok(0),
        (Kind::FpImm, Operand::FpImm(n)) => Ok(u32::from(*n)),
        (Kind::System(expected), Operand::System(actual)) if expected == *actual => Ok(0),
        _ => Err(error("操作数形状或立即数范围错误")),
    }
}

fn address(a: &Address, mode: AddrMode, size: u8) -> Result<u32, Error> {
    let base = register(a.base, Width::X, true, false)?;
    let raw = match (mode, &a.mode) {
        (AddrMode::Unsigned, AddressMode::Unsigned(n)) => bits::unsigned(*n, 12, size)?,
        (AddrMode::Unsigned, AddressMode::Reloc { .. }) => 0,
        (AddrMode::Unscaled, AddressMode::Unscaled(n))
        | (AddrMode::Pre, AddressMode::Pre(n))
        | (AddrMode::Post, AddressMode::Post(n)) => bits::signed(*n, 9, 0)?,
        (AddrMode::Pair, AddressMode::Unsigned(n))
        | (AddrMode::PairPre, AddressMode::Pre(n))
        | (AddrMode::PairPost, AddressMode::Post(n)) => bits::signed(*n, 7, size)?,
        (AddrMode::Base, AddressMode::Base) => 0,
        (
            AddrMode::Register,
            AddressMode::Register {
                index,
                extend,
                scaled,
            },
        ) => {
            let width = match extend {
                Extend::Uxtw | Extend::Sxtw => Width::W,
                Extend::Uxtx | Extend::Sxtx => Width::X,
                _ => return Err(error("地址扩展只能为 UXTW/LSL/SXTW/SXTX")),
            };
            register(*index, width, false, false)?
                | ((*extend as u32) << 6)
                | (u32::from(*scaled) << 5)
        }
        _ => return Err(error("地址模式不匹配")),
    };
    Ok(base | (raw << 5))
}

fn constraints(form: &Form, operands: &[Operand]) -> Result<(), Error> {
    if let Some(index) = form
        .operands
        .iter()
        .position(|s| matches!(s.kind, Kind::ExtendedReg(_)))
        && let (Operand::Reg(r), Operand::Extend(ext, _)) = (&operands[index], &operands[index + 1])
    {
        let x = matches!(form.operands[index].kind, Kind::ExtendedReg(64))
            && matches!(ext, Extend::Uxtx | Extend::Sxtx);
        if (r.width == Width::X) != x {
            return Err(error("扩展种类与源寄存器宽度不一致"));
        }
    }
    if matches!(form.mnemonic, "dmb" | "dsb") && matches!(operands[0],Operand::Imm(n) if n & 3 == 0)
    {
        return Err(error("屏障选项超出基线"));
    }
    if form.mnemonic == "isb" && operands[0] != Operand::Imm(15) {
        return Err(error("ISB 仅支持 SY"));
    }
    check_memory(form, operands)
}

fn check_memory(form: &Form, operands: &[Operand]) -> Result<(), Error> {
    let Some(Operand::Address(a)) = operands.last() else {
        return Ok(());
    };
    let regs: Vec<Reg> = operands
        .iter()
        .filter_map(|op| {
            if let Operand::Reg(r) = op {
                Some(*r)
            } else {
                None
            }
        })
        .collect();
    let same = |r: Reg, s: Reg| matches!((r.kind,s.kind),(RegKind::Gpr(a),RegKind::Gpr(b)) if a==b);
    if matches!(a.mode, AddressMode::Pre(_) | AddressMode::Post(_))
        && regs.iter().any(|r| same(*r, a.base))
    {
        return Err(error("写回地址与传输寄存器重叠"));
    }
    if form.mnemonic == "ldp" && regs[0] == regs[1] {
        return Err(error("LDP 的目标不能重叠"));
    }
    if (form.mnemonic.starts_with("stxr") || form.mnemonic.starts_with("stlxr"))
        && (same(regs[0], regs[1]) || same(regs[0], a.base))
    {
        return Err(error("独占存储状态寄存器重叠"));
    }
    Ok(())
}
