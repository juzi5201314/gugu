//! 确定性的 LLVM 汇编文本；不把基础指令改写为别名。

use super::{Error, inst::*, table::*, verify};

const CONDITIONS: [&str; 16] = [
    "eq", "ne", "hs", "lo", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "al", "nv",
];
const EXTENSIONS: [&str; 8] = [
    "uxtb", "uxth", "uxtw", "uxtx", "sxtb", "sxth", "sxtw", "sxtx",
];

pub(crate) fn text(inst: &Inst) -> Result<String, Error> {
    verify::verify(inst)?;
    let form = get(inst.form)?;
    if form.mnemonic == "b.cond" {
        let Operand::Condition(c) = inst.operands[1] else {
            unreachable!()
        };
        return Ok(format!(
            "b.{} {}",
            CONDITIONS[c as usize],
            operand(&inst.operands[0], form.operands[0].kind)
        ));
    }
    let args = inst
        .operands
        .iter()
        .zip(form.operands)
        .map(|(op, slot)| operand(op, slot.kind))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(if args.is_empty() {
        form.mnemonic.to_owned()
    } else {
        format!("{} {args}", form.mnemonic)
    })
}

fn operand(op: &Operand, kind: Kind) -> String {
    match op {
        Operand::Reg(r) => r.to_string(),
        Operand::Imm(n) => format!("#{n}"),
        Operand::Bits(n) => format!("#0x{n:x}"),
        Operand::AddImm { imm12, shift12 } => {
            format!("#{imm12}{}", if *shift12 { ", lsl #12" } else { "" })
        }
        Operand::Wide { imm16, shift } => format!("#{imm16}, lsl #{shift}"),
        Operand::Shift(shift, n) => {
            format!("{} #{n}", ["lsl", "lsr", "asr", "ror"][*shift as usize])
        }
        Operand::Extend(ext, n) => format!("{} #{n}", EXTENSIONS[*ext as usize]),
        Operand::Condition(c) => CONDITIONS[*c as usize].to_owned(),
        Operand::Address(a) => {
            let Kind::Address { size, .. } = kind else {
                unreachable!()
            };
            address(a, size)
        }
        Operand::Target(Target::Offset(n)) => format!("#{n}"),
        Operand::Target(Target::Symbol { name, addend }) => format!("{name}{addend:+}"),
        Operand::FpImm(n) => format!("#{:.8}", fp_value(*n)),
        Operand::System(s) => (*s).to_owned(),
    }
}

fn address(a: &Address, size: u8) -> String {
    let base = a.base;
    match &a.mode {
        AddressMode::Unsigned(n) | AddressMode::Unscaled(n) => format!("[{base}, #{n}]"),
        AddressMode::Pre(n) => format!("[{base}, #{n}]!"),
        AddressMode::Post(n) => format!("[{base}], #{n}"),
        AddressMode::Base => format!("[{base}]"),
        AddressMode::Reloc { name, addend } => format!("[{base}, {name}@PAGEOFF{addend:+}]"),
        AddressMode::Register {
            index,
            extend,
            scaled,
        } => {
            if !scaled {
                return if *extend == Extend::Uxtx {
                    format!("[{base}, {index}]")
                } else {
                    format!("[{base}, {index}, {}]", EXTENSIONS[*extend as usize])
                };
            }
            let ext = if *extend == Extend::Uxtx {
                "lsl"
            } else {
                EXTENSIONS[*extend as usize]
            };
            format!(
                "[{base}, {index}, {ext} #{}]",
                if *scaled { size } else { 0 }
            )
        }
    }
}

fn fp_value(imm: u8) -> f64 {
    let sign = if imm & 128 != 0 { -1.0 } else { 1.0 };
    let exp = if imm & 64 != 0 { -3 } else { 1 } + i32::from((imm >> 4) & 3);
    sign * (1.0 + f64::from(imm & 15) / 16.0) * 2f64.powi(exp)
}
