//! 消费已校验的 AT&T 模板，映射到封闭 form 表；表外 opcode 为 `E0060`。

use crate::backend::x64::inst::{Inst, Operand};
use crate::backend::x64::reg::{Gpr, Reg, Xmm};
use crate::backend::x64::table::{self, Access, FormId, OperandKind};
use crate::frontend::semantics::assembly::{Register, RegisterClass};
use crate::lir::body::Op;

use super::{Builder, LowerCtx, LoweringError, SiteValue, gpr, reg};

pub(super) fn lower(
    op: &Op,
    _operands: &[SiteValue],
    _results: &[SiteValue],
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let Op::InlineAsm(index) = *op else {
        return Err(LoweringError::InvalidOperands);
    };
    let Some(body) = ctx.body else {
        // legalize/探针：空序列，站点仍存在。
        return Ok(());
    };
    let template = body
        .assembly
        .get(usize::try_from(index).expect("汇编下标适配 usize"))
        .ok_or(LoweringError::InvalidOperands)?;
    builder.clobbers_union(template.clobbers);
    for line in template.template.split(|ch| ch == ';' || ch == '\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        emit_line(line, builder)?;
    }
    Ok(())
}

impl Builder {
    fn clobbers_union(&mut self, mask: u64) {
        for index in 0..16_u8 {
            if mask & (1_u64 << index) != 0
                && let Some(gpr) = gpr_from_index(index)
            {
                self.clobber_gpr(gpr);
            }
            if mask & (1_u64 << (16 + index)) != 0
                && let Some(xmm) = xmm_from_index(index)
            {
                self.clobber_xmm(xmm);
            }
        }
    }
}

fn emit_line(line: &str, builder: &mut Builder) -> Result<(), LoweringError> {
    let mut line = line.trim();
    if line.starts_with('.') {
        return Ok(());
    }
    if let Some((label, rest)) = line.split_once(':') {
        let label = label.trim();
        if let Some(id) = numeric_label(label) {
            for prior in 0..=id {
                if !builder.has_label(crate::backend::x64::inst::LabelId(prior)) {
                    builder.define(crate::backend::x64::inst::LabelId(prior));
                }
            }
        }
        line = rest.trim();
        if line.is_empty() {
            return Ok(());
        }
    }
    if line.is_empty() {
        return Ok(());
    }
    let lock = line.starts_with("lock ");
    let (mnemonic, rest) = split_mnemonic(line);
    let operands = parse_operands(rest)?;
    let Some((form, operands)) = find_form(mnemonic, operands) else {
        return Err(LoweringError::Unsupported {
            op: "InlineAsm",
            detail: "模板助记符不在封闭指令表内",
        });
    };
    builder.push_inst(Inst {
        form,
        operands,
        lock,
    });
    Ok(())
}

fn split_mnemonic(line: &str) -> (&str, &str) {
    let line = line.trim_start_matches("lock ").trim();
    match line.split_once(char::is_whitespace) {
        Some((mnemonic, rest)) => (mnemonic, rest),
        None => (line, ""),
    }
}

fn parse_operands(rest: &str) -> Result<Vec<Operand>, LoweringError> {
    if rest.trim().is_empty() {
        return Ok(Vec::new());
    }
    rest.split(',')
        .map(|part| parse_operand(part.trim()))
        .collect()
}

fn parse_operand(text: &str) -> Result<Operand, LoweringError> {
    let text = text.trim();
    if text.ends_with(':') {
        return Err(LoweringError::Unsupported {
            op: "InlineAsm",
            detail: "模板操作数无法映射到封闭 form",
        });
    }
    let stripped = text.trim_start_matches('%');
    if let Some(reg) = parse_register(stripped) {
        return Ok(reg);
    }
    if let Some(imm) = text.strip_prefix('$') {
        let value = parse_imm(imm)?;
        return Ok(Operand::Imm(value));
    }
    // AT&T 局部标签：`1f` / `1b` / 裸标识。
    if is_local_label(text) {
        let id = numeric_label(text.trim_end_matches(['f', 'b'])).unwrap_or(0);
        return Ok(Operand::Label(crate::backend::x64::inst::LabelId(id)));
    }
    Err(LoweringError::Unsupported {
        op: "InlineAsm",
        detail: "模板操作数无法映射到封闭 form",
    })
}

fn is_local_label(text: &str) -> bool {
    let core = text.trim_end_matches(['f', 'b']);
    !core.is_empty()
        && core
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn numeric_label(text: &str) -> Option<u32> {
    text.parse::<u32>().ok()
}

fn parse_register(name: &str) -> Option<Operand> {
    let parsed = Register::parse(name)?;
    match parsed.class {
        RegisterClass::Gpr => Some(reg(gpr(gpr_from_index(parsed.index)?))),
        RegisterClass::Xmm => Some(Operand::Reg(Reg::Xmm(xmm_from_index(parsed.index)?))),
        RegisterClass::HighByte => None,
    }
}

fn parse_imm(text: &str) -> Result<u64, LoweringError> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).map_err(|_| LoweringError::InvalidOperands);
    }
    text.parse::<i64>()
        .map(|value| value as u64)
        .or_else(|_| text.parse::<u64>())
        .map_err(|_| LoweringError::InvalidOperands)
}

fn find_form(mnemonic: &str, operands: Vec<Operand>) -> Option<(FormId, Vec<Operand>)> {
    let n = operands.len();
    if n == 0 {
        let form = lookup(mnemonic, &[], Access::Read)?;
        return Some((form, operands));
    }
    for kinds in kind_candidates(&operands) {
        for first in [
            Access::Write,
            Access::Read,
            Access::ReadWrite,
            Access::Address,
        ] {
            if let Some(form) = lookup(mnemonic, &kinds, first) {
                return Some((form, operands));
            }
        }
    }
    None
}

fn kind_candidates(operands: &[Operand]) -> Vec<Vec<OperandKind>> {
    let mut out = Vec::new();
    out.push(kinds_of(operands, false));
    out.push(kinds_of(operands, true));
    out
}

fn kinds_of(operands: &[Operand], dest_as_r: bool) -> Vec<OperandKind> {
    operands
        .iter()
        .enumerate()
        .map(|(index, operand)| match operand {
            Operand::Reg(Reg::Gpr(_)) => {
                if dest_as_r && index + 1 == operands.len() {
                    OperandKind::R64
                } else if dest_as_r {
                    OperandKind::Rm64
                } else if index == 0 {
                    OperandKind::Rm64
                } else {
                    OperandKind::R64
                }
            }
            Operand::Reg(Reg::Xmm(_)) => {
                if index + 1 == operands.len() {
                    OperandKind::Xmm
                } else {
                    OperandKind::XmmRm
                }
            }
            Operand::Imm(value) if *value <= u64::from(u8::MAX) => OperandKind::Imm8,
            Operand::Imm(value) if *value <= u64::from(u32::MAX) => OperandKind::Imm32,
            Operand::Imm(_) => OperandKind::Imm64,
            Operand::Label(_) => OperandKind::Rel32,
            _ => OperandKind::Rm64,
        })
        .collect()
}

fn lookup(mnemonic: &str, kinds: &[OperandKind], first: Access) -> Option<FormId> {
    table::form_id_with_access(mnemonic, kinds, first)
}

fn gpr_from_index(index: u8) -> Option<Gpr> {
    match index {
        0 => Some(Gpr::Rax),
        1 => Some(Gpr::Rcx),
        2 => Some(Gpr::Rdx),
        3 => Some(Gpr::Rbx),
        4 => Some(Gpr::Rsp),
        5 => Some(Gpr::Rbp),
        6 => Some(Gpr::Rsi),
        7 => Some(Gpr::Rdi),
        8 => Some(Gpr::R8),
        9 => Some(Gpr::R9),
        10 => Some(Gpr::R10),
        11 => Some(Gpr::R11),
        12 => Some(Gpr::R12),
        13 => Some(Gpr::R13),
        14 => Some(Gpr::R14),
        15 => Some(Gpr::R15),
        _ => None,
    }
}

fn xmm_from_index(index: u8) -> Option<Xmm> {
    match index {
        0 => Some(Xmm::Xmm0),
        1 => Some(Xmm::Xmm1),
        2 => Some(Xmm::Xmm2),
        3 => Some(Xmm::Xmm3),
        4 => Some(Xmm::Xmm4),
        5 => Some(Xmm::Xmm5),
        6 => Some(Xmm::Xmm6),
        7 => Some(Xmm::Xmm7),
        8 => Some(Xmm::Xmm8),
        9 => Some(Xmm::Xmm9),
        10 => Some(Xmm::Xmm10),
        11 => Some(Xmm::Xmm11),
        12 => Some(Xmm::Xmm12),
        13 => Some(Xmm::Xmm13),
        14 => Some(Xmm::Xmm14),
        15 => Some(Xmm::Xmm15),
        _ => None,
    }
}
