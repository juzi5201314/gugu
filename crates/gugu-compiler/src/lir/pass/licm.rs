//! 循环不变量外提：把纯指令上提到唯一 preheader。
use super::graph::dominates;
use super::loops::{self, LoopInfo};
use super::rewrite::Editor;
use crate::Diagnostic;
use crate::lir::body::{BlockId, Conversion, IntOp, Op, ValueId};

pub(crate) fn run(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut changed = false;
    let loops = loops::analyze(editor);
    for natural in &loops {
        let Some(preheader) = preheader_of(editor, natural) else {
            continue;
        };
        let Some(dominators) = editor.dominators() else {
            continue;
        };
        loop {
            let mut local = false;
            for block in natural.blocks.iter().copied() {
                for index in 0..editor.instruction_count(block) {
                    let instruction = editor.instruction((block, index)).clone();
                    if instruction.removed
                        || instruction.op.has_memory()
                        || instruction.op.safepoint_kind().is_some()
                        || instruction.results.len() != 1
                        || !hoistable(&instruction.op)
                    {
                        continue;
                    }
                    if !instruction.arguments.iter().all(|argument| {
                        invariant(editor, natural, &dominators, preheader, *argument)
                    }) {
                        continue;
                    }
                    let result = instruction.results[0];
                    let kind = editor.kind(result);
                    let origin = editor.origin(result).clone();
                    let hoisted = editor.emit(
                        preheader,
                        instruction.op.clone(),
                        &instruction.arguments,
                        &[(kind, origin)],
                    );
                    editor.replace_value(result, hoisted[0]);
                    editor.remove_instruction((block, index))?;
                    local = true;
                }
            }
            changed |= local;
            if !local {
                break;
            }
        }
    }
    Ok(changed)
}

fn preheader_of(editor: &Editor, natural: &LoopInfo) -> Option<BlockId> {
    let outside: Vec<_> = editor
        .predecessors(natural.header)
        .into_iter()
        .filter_map(|edge| editor.edge(edge))
        .filter(|edge| !natural.blocks.contains(&edge.from))
        .map(|edge| edge.from)
        .collect();
    match outside.as_slice() {
        [preheader] => Some(*preheader),
        _ => None,
    }
}

/// 除法、取余与浮点转整数可能触发陷阱，禁止外提。
fn hoistable(op: &Op) -> bool {
    match op {
        Op::Integer(
            IntOp::DivSigned | IntOp::DivUnsigned | IntOp::RemSigned | IntOp::RemUnsigned,
        ) => false,
        Op::Convert(Conversion::FloatToInt { .. }) => false,
        _ => true,
    }
}

fn invariant(
    editor: &Editor,
    natural: &LoopInfo,
    dominators: &[BlockId],
    preheader: BlockId,
    value: ValueId,
) -> bool {
    let Some((block, _)) = editor.defining_instruction(value) else {
        return false;
    };
    if natural.blocks.contains(&block) {
        return false;
    }
    dominates(dominators, block, preheader)
}
