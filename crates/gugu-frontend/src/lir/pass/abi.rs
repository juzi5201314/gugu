//! 目标 ABI 形态校验与目标 legalization 边界。
//!
//! 本阶段这两个 pass 只做显式校验，不改写语义；机器序列由注入的 [`TargetOracle`] 提供，
//! 真正的寄存器分配与指令选择由后端阶段在同一位置提供。
use super::rewrite::{Editor, Term};
use crate::Diagnostic;
use crate::frontend::gir::body::CallKind;
use crate::lir::body::{Call, Op, Type};
use crate::lir::invalid;
use crate::lir::target::TargetOracle;

pub(crate) fn lower_target_abi(editor: &mut Editor) -> Result<bool, Diagnostic> {
    for block in editor.live_blocks() {
        for index in 0..editor.instruction_count(block) {
            if let Op::Call(call) | Op::ForeignCall(call) = &editor.instruction((block, index)).op {
                check_call(call, editor)?;
            }
        }
        match editor.terminator(block) {
            Term::Invoke { call, .. } => check_call(call, editor)?,
            Term::TailCall { call, .. } => {
                check_call(call, editor)?;
                if call.sret.is_some()
                    || editor.signature.sret.is_some()
                    || call.results != editor.signature.results
                    || editor.stack_slots.iter().any(|slot| slot.bytes != 0)
                    || editor.block(block).expect("活跃 block").cleanup
                    || matches!(
                        call.kind,
                        CallKind::ForeignBridge | CallKind::ForeignBridgeDirtyCpu
                    )
                {
                    return Err(invalid("TailCall 不满足目标 ABI 资格"));
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

fn check_call(call: &Call, editor: &Editor) -> Result<(), Diagnostic> {
    if call.by_value.iter().any(|(index, _, _)| {
        usize::try_from(*index).map_or(true, |index| index >= call.parameters.len())
    }) || call.sret.is_some_and(|(index, _, _)| {
        usize::try_from(index).map_or(true, |index| index >= call.parameters.len())
    }) || call
        .parameters
        .iter()
        .chain(&call.results)
        .any(|kind| matches!(kind.ty, Type::Void))
    {
        return Err(invalid("调用 ABI 参数编号或类型非法"));
    }
    let _ = editor;
    Ok(())
}

/// 目标 legalization：数值、向量、原子与压缩解码指令必须存在基线机器序列。
///
/// 门禁与 codegen 同源：由目标 oracle 用虚拟寄存器试 lower 一次并跑 instruction verifier，
/// 任何缺失或非法序列都在这里变成 `E0057`，而不是留到片段生成阶段。
pub(crate) fn legalize_x86_64(
    editor: &mut Editor,
    target: &dyn TargetOracle,
) -> Result<bool, Diagnostic> {
    let kind = |value| editor.kind(value);
    for block in editor.live_blocks() {
        for index in 0..editor.instruction_count(block) {
            let instruction = editor.instruction((block, index));
            if instruction.removed {
                continue;
            }
            for result in &instruction.results {
                let kind = editor.kind(*result);
                if matches!(kind.ty, Type::V128(_)) && kind.provenance.is_some() {
                    return Err(invalid("V128 不允许携带指针 provenance"));
                }
            }
            target
                .legalize(
                    &instruction.op,
                    &instruction.arguments,
                    &instruction.results,
                    &kind,
                    &instruction.source,
                )
                .map_err(|error| invalid(&error.to_string()))?;
        }
    }
    Ok(false)
}
