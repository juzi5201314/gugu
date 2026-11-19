//! 按固定表序匹配；保留位及受约束不可预测形式由共享 verifier 拒绝。

use super::{Error, bits, error, inst::*, reg::*, table::*, verify};

pub fn decode(word: u32) -> Result<Inst, Error> {
    for (index, form) in FORMS.iter().enumerate() {
        if word & form.mask != form.fixed {
            continue;
        }
        let mut operands = Vec::new();
        for slot in form.operands {
            let mut raw = 0;
            for field in slot.fields {
                if field.width != 0 {
                    raw |= ((word & field.mask()) >> field.lsb) << field.value_lsb;
                }
            }
            operands.push(unpack(slot.kind, raw)?);
        }
        fix_extended(form, &mut operands);
        let inst = Inst::new(FormId(index as u16), operands);
        verify::verify(&inst)?;
        return Ok(inst);
    }
    Err(error("未知或未分配的 A64 指令字"))
}

fn fix_extended(form: &Form, operands: &mut [Operand]) {
    for i in 1..operands.len() {
        if matches!(form.operands[i - 1].kind, Kind::ExtendedReg(64))
            && matches!(operands[i], Operand::Extend(Extend::Uxtx | Extend::Sxtx, _))
            && let Operand::Reg(r) = &mut operands[i - 1]
        {
            r.width = Width::X;
        }
    }
}

pub(crate) fn reg(raw: u32, width: Width, sp: bool, vector: bool) -> Reg {
    if vector {
        Reg::v(raw as u8, width)
    } else if raw == 31 {
        if sp { Reg::sp(width) } else { Reg::zr(width) }
    } else {
        Reg {
            kind: RegKind::Gpr(raw as u8),
            width,
        }
    }
}

pub(crate) fn extension(raw: u32) -> Extend {
    [
        Extend::Uxtb,
        Extend::Uxth,
        Extend::Uxtw,
        Extend::Uxtx,
        Extend::Sxtb,
        Extend::Sxth,
        Extend::Sxtw,
        Extend::Sxtx,
    ][raw as usize & 7]
}

pub(crate) fn unpack(kind: Kind, raw: u32) -> Result<Operand, Error> {
    Ok(match kind {
        Kind::Reg { width, sp, vector } => Operand::Reg(reg(raw, width, sp, vector)),
        Kind::ExtendedReg(_) => Operand::Reg(reg(raw, Width::W, false, false)),
        Kind::TestBit(high) => Operand::Imm(i64::from(raw) + if high { 32 } else { 0 }),
        Kind::Imm {
            bits,
            signed,
            scale,
        } => Operand::Imm(if signed {
            bits::sign_extend(raw, bits, scale)
        } else {
            i64::from(raw) << scale
        }),
        Kind::AddImm => Operand::AddImm {
            imm12: (raw & 4095) as u16,
            shift12: raw & 4096 != 0,
        },
        Kind::Logical(width) => Operand::Bits(bits::decode_logical(raw, width)?),
        Kind::Wide(_) => Operand::Wide {
            imm16: raw as u16,
            shift: ((raw >> 16) * 16) as u8,
        },
        Kind::Shift { .. } => Operand::Shift(
            [Shift::Lsl, Shift::Lsr, Shift::Asr, Shift::Ror][(raw >> 6) as usize],
            (raw & 63) as u8,
        ),
        Kind::Extend(_) => Operand::Extend(extension(raw >> 3), (raw & 7) as u8),
        Kind::Condition => Operand::Condition(raw as u8),
        Kind::Address { mode, size } => Operand::Address(address(raw, mode, size)),
        Kind::Target { bits, scale, .. } => {
            Operand::Target(Target::Offset(bits::sign_extend(raw, bits, scale)))
        }
        Kind::FpImm => Operand::FpImm(raw as u8),
        Kind::System(name) => Operand::System(name),
    })
}

fn address(raw: u32, mode: AddrMode, size: u8) -> Address {
    let base = reg(raw & 31, Width::X, true, false);
    let value = raw >> 5;
    let mode = match mode {
        AddrMode::Unsigned => AddressMode::Unsigned(i64::from(value) << size),
        AddrMode::Unscaled => AddressMode::Unscaled(bits::sign_extend(value, 9, 0)),
        AddrMode::Pre => AddressMode::Pre(bits::sign_extend(value, 9, 0)),
        AddrMode::Post => AddressMode::Post(bits::sign_extend(value, 9, 0)),
        AddrMode::Pair => AddressMode::Unsigned(bits::sign_extend(value, 7, size)),
        AddrMode::PairPre => AddressMode::Pre(bits::sign_extend(value, 7, size)),
        AddrMode::PairPost => AddressMode::Post(bits::sign_extend(value, 7, size)),
        AddrMode::Base => AddressMode::Base,
        AddrMode::Register => {
            let extend = extension(value >> 6);
            let width = if matches!(extend, Extend::Uxtx | Extend::Sxtx) {
                Width::X
            } else {
                Width::W
            };
            AddressMode::Register {
                index: reg(value & 31, width, false, false),
                extend,
                scaled: value & 32 != 0,
            }
        }
    };
    Address { base, mode }
}
