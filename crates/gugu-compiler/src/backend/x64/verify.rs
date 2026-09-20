//! x86_64 instruction verifier：与编码器共享 form 表，拒绝超出 baseline 的形式与
//! 违反寄存器纪律的序列。

use std::fmt;

use super::encode::sequence_clobbers;
use super::inst::{Inst, LabelId, Operand, RelocKind, RelocTarget, Scale, Sequence};
use super::reg::{Clobbers, Gpr, Reg};
use super::table::{self, Access, Lock, OperandKind};
use crate::target::CpuBaseline;

/// 校验失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VerifyError {
    message: String,
}

impl VerifyError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 返回错误信息；仅供负例断言。
    #[cfg(test)]
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for VerifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for VerifyError {}

/// 校验单条指令：形状、baseline、lock、内存操作数与保留寄存器。
pub(crate) fn verify_inst(inst: &Inst, baseline: CpuBaseline) -> Result<(), VerifyError> {
    let Some(form) = table::try_form(inst.form) else {
        return Err(VerifyError::new("form 编号越界"));
    };
    if inst.operands.len() != form.operands.len() {
        return Err(VerifyError::new(format!(
            "{} 需要 {} 个操作数，实际 {} 个",
            form.mnemonic,
            form.operands.len(),
            inst.operands.len()
        )));
    }
    if !baseline.allows(form.feature) {
        return Err(VerifyError::new(format!(
            "{} 超出 {} 的指令可接受面",
            form.mnemonic,
            baseline.name()
        )));
    }
    for (kind, operand) in form.operands.iter().zip(&inst.operands) {
        if !operand_matches(*kind, operand) {
            return Err(VerifyError::new(format!(
                "{} 的操作数形状与表不一致",
                form.mnemonic
            )));
        }
    }
    if inst.lock {
        if form.lock != Lock::Allowed {
            return Err(VerifyError::new(format!(
                "{} 不接受显式 lock 前缀",
                form.mnemonic
            )));
        }
        if !inst
            .operands
            .iter()
            .any(|operand| matches!(operand, Operand::Mem(_) | Operand::Rip(..)))
        {
            return Err(VerifyError::new("lock 前缀要求内存操作数"));
        }
        if form.mnemonic == "xchg" {
            return Err(VerifyError::new("带内存的 xchg 已隐式锁定，只保留一种写法"));
        }
    }
    for operand in &inst.operands {
        if let Operand::Mem(mem) = operand {
            if mem.index == Some(Reg::Gpr(Gpr::Rsp)) {
                return Err(VerifyError::new("SIB index 不能是 rsp"));
            }
            if mem.index.is_none() && mem.scale != Scale::One {
                return Err(VerifyError::new("无 index 的内存操作数不能带 scale"));
            }
        }
    }
    for (index, operand) in inst.operands.iter().enumerate() {
        let Some(access) = form.access.get(index) else {
            continue;
        };
        let Operand::Reg(Reg::Gpr(gpr)) = operand else {
            continue;
        };
        if matches!(access, Access::Write | Access::ReadWrite)
            && matches!(gpr, Gpr::Rsp | Gpr::R14 | Gpr::R15)
        {
            return Err(VerifyError::new(format!(
                "普通序列不得写内部 ABI 保留寄存器 {}",
                gpr.name()
            )));
        }
        if matches!(form.operands[index], OperandKind::Rm8 | OperandKind::R8)
            && !gpr.is_byte_encodable()
        {
            return Err(VerifyError::new(format!(
                "{} 的低字节无法编码，需回避 {}",
                form.mnemonic,
                gpr.name()
            )));
        }
    }
    Ok(())
}

/// 校验整段序列：逐指令规则、标签结构、冷边引用、`cmpxchg` 的 `rax` 约定与
/// 虚拟寄存器先写后读纪律；返回序列写到的物理寄存器集合。
pub(crate) fn verify_sequence(
    sequence: &Sequence,
    baseline: CpuBaseline,
) -> Result<Clobbers, VerifyError> {
    for inst in &sequence.instructions {
        verify_inst(inst, baseline)?;
    }
    verify_labels(sequence)?;
    verify_cold_edges(sequence)?;
    verify_message_and_register_discipline(sequence)?;
    Ok(sequence_clobbers(sequence))
}

/// 函数级序列：逐指令、标签、冷边；不检查跨站点的虚拟寄存器先写后读。
///
/// 活入参数和前序站点的结果会在后续站点被读，也会在块参数拷贝中再次写入同一
/// Virtual；那是函数级数据流，不是单站点 lowering 错误。
pub(crate) fn verify_function_sequence(
    sequence: &Sequence,
    baseline: CpuBaseline,
) -> Result<Clobbers, VerifyError> {
    for inst in &sequence.instructions {
        verify_inst(inst, baseline)?;
    }
    verify_labels(sequence)?;
    verify_cold_edges(sequence)?;
    Ok(sequence_clobbers(sequence))
}

/// 每个被引用的标签都有定义；定义编号唯一且从 0 起连续。
///
/// 同一标签可以被多条分支引用（合并点由多条边汇入），编码器按指令各自回填。
fn verify_labels(sequence: &Sequence) -> Result<(), VerifyError> {
    let mut defined: Vec<Option<u32>> = Vec::new();
    for definition in &sequence.labels {
        let index = usize::try_from(definition.label.0)
            .map_err(|_| VerifyError::new("标签编号超出宿主范围"))?;
        if defined.len() <= index {
            defined.resize(index + 1, None);
        }
        if defined[index].replace(definition.at).is_some() {
            return Err(VerifyError::new("标签被重复定义"));
        }
    }
    if defined.iter().any(Option::is_none) {
        return Err(VerifyError::new("标签编号必须从 0 起连续定义"));
    }
    for inst in &sequence.instructions {
        for operand in &inst.operands {
            if let Operand::Label(LabelId(id)) = operand {
                let index =
                    usize::try_from(*id).map_err(|_| VerifyError::new("标签编号超出宿主范围"))?;
                if defined.get(index).is_none() {
                    return Err(VerifyError::new("分支引用了未定义的标签"));
                }
            }
        }
    }
    Ok(())
}

/// 冷边只作为分支目标出现，且同一条冷边最多被引用一次；冷段在镜像中的连续性由
/// 后续拼接阶段保证。
fn verify_cold_edges(sequence: &Sequence) -> Result<(), VerifyError> {
    let mut edges = Vec::new();
    for inst in &sequence.instructions {
        let Some(form) = table::try_form(inst.form) else {
            return Err(VerifyError::new("form 编号越界"));
        };
        for (kind, operand) in form.operands.iter().zip(&inst.operands) {
            match operand {
                Operand::Rip(RelocTarget::Cold(_), _) => {
                    return Err(VerifyError::new("冷边只能作为分支目标引用"));
                }
                Operand::Reloc(RelocTarget::Cold(edge), reloc) => {
                    if *kind != OperandKind::Rel32 || *reloc != RelocKind::PcRel32 {
                        return Err(VerifyError::new("冷边引用必须落在 rel32 分支操作数上"));
                    }
                    if edges.contains(edge) {
                        return Err(VerifyError::new("同一条冷边最多被引用一次"));
                    }
                    edges.push(edge.clone());
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// `cmpxchg` 之前必须有对 `rax` 的写（expected 寄存器约定）；虚拟寄存器先写后读。
///
/// 只对**本序列写过的**虚拟寄存器强制「第一次读在首次写之后」：序列从未写过的虚拟寄存器是
/// 活入操作数（由上游站点定义），读它们属于正常数据流。`ReadWrite` 位置算作写并豁免读序
/// 检查——两地址形式的目标是「取值 + 更新」，第一次出现就是定义。
fn verify_message_and_register_discipline(sequence: &Sequence) -> Result<(), VerifyError> {
    let mut written = virtual_registers_written_by(sequence)?;
    let mut defined = written.clone();
    defined.fill(false);
    let mut rax_written = false;
    for inst in &sequence.instructions {
        let Some(form) = table::try_form(inst.form) else {
            return Err(VerifyError::new("form 编号越界"));
        };
        if form.mnemonic == "cmpxchg" && !rax_written {
            return Err(VerifyError::new("cmpxchg 之前必须先写 rax"));
        }
        for (index, operand) in inst.operands.iter().enumerate() {
            let Some(access) = form.access.get(index) else {
                continue;
            };
            match operand {
                Operand::Reg(Reg::Gpr(Gpr::Rax))
                    if matches!(access, Access::Write | Access::ReadWrite) =>
                {
                    rax_written = true;
                }
                Operand::Reg(Reg::Virtual(id)) => {
                    let slot = register_slot(*id)?;
                    if written.len() <= slot {
                        written.resize(slot + 1, false);
                        defined.resize(slot + 1, false);
                    }
                    match access {
                        Access::Read if !defined[slot] && written[slot] => {
                            return Err(VerifyError::new(format!(
                                "{} 在虚拟寄存器 v{id} 第一次写之前读取它",
                                form.mnemonic
                            )));
                        }
                        Access::Read | Access::Address => {}
                        Access::Write | Access::ReadWrite => defined[slot] = true,
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// 虚拟寄存器编号 → 位图下标。
fn register_slot(id: u32) -> Result<usize, VerifyError> {
    usize::try_from(id).map_err(|_| VerifyError::new("虚拟寄存器编号超出宿主范围"))
}

/// 序列写过的虚拟寄存器位图（按编号索引）。
fn virtual_registers_written_by(sequence: &Sequence) -> Result<Vec<bool>, VerifyError> {
    let mut written: Vec<bool> = Vec::new();
    for inst in &sequence.instructions {
        let Some(form) = table::try_form(inst.form) else {
            return Err(VerifyError::new("form 编号越界"));
        };
        for (index, operand) in inst.operands.iter().enumerate() {
            let Some(access) = form.access.get(index) else {
                continue;
            };
            if !matches!(access, Access::Write | Access::ReadWrite) {
                continue;
            }
            if let Operand::Reg(Reg::Virtual(id)) = operand {
                let slot = register_slot(*id)?;
                if written.len() <= slot {
                    written.resize(slot + 1, false);
                }
                written[slot] = true;
            }
        }
    }
    Ok(written)
}

fn operand_matches(kind: OperandKind, operand: &Operand) -> bool {
    match kind {
        // RIP 相对操作数占据 r/m 位（控制记录字段访问等）。
        OperandKind::Rm8 | OperandKind::Rm16 | OperandKind::Rm32 | OperandKind::Rm64 => matches!(
            operand,
            Operand::Reg(Reg::Gpr(_) | Reg::Virtual(_)) | Operand::Mem(_) | Operand::Rip(..)
        ),
        OperandKind::R8 | OperandKind::R16 | OperandKind::R32 | OperandKind::R64 => {
            matches!(operand, Operand::Reg(Reg::Gpr(_) | Reg::Virtual(_)))
        }
        OperandKind::Xmm => matches!(operand, Operand::Reg(Reg::Xmm(_) | Reg::Virtual(_))),
        OperandKind::XmmRm => matches!(
            operand,
            Operand::Reg(Reg::Xmm(_) | Reg::Virtual(_)) | Operand::Mem(_)
        ),
        OperandKind::Mem => matches!(operand, Operand::Mem(_) | Operand::Rip(..)),
        OperandKind::Imm8 | OperandKind::Imm16 | OperandKind::Imm32 | OperandKind::Imm64 => {
            matches!(operand, Operand::Imm(_))
        }
        OperandKind::Rel32 => matches!(
            operand,
            Operand::Label(_)
                | Operand::Reloc(RelocTarget::Cold(_), RelocKind::PcRel32)
                | Operand::Reloc(RelocTarget::Lir(_), RelocKind::PcRel32)
        ),
        OperandKind::Rel8 => matches!(operand, Operand::Label(_)),
        OperandKind::Cl => matches!(operand, Operand::Reg(Reg::Gpr(Gpr::Rcx))),
    }
}
