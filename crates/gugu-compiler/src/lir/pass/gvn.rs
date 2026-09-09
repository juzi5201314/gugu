//! 全局值编号：对纯表达式做 CSE，只在定义支配使用时替换。
use super::graph::dominates;
use super::rewrite::Editor;
use crate::Diagnostic;
use crate::lir::body::{BlockId, Op, ValueId};
use std::collections::BTreeMap;

pub(crate) fn run(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let Some(dominators) = editor.dominators() else {
        return Ok(false);
    };
    let order = editor.reverse_postorder();
    let mut table: BTreeMap<Vec<u8>, (BlockId, usize, ValueId)> = BTreeMap::new();
    let mut changed = false;
    for block in order {
        let count = editor.instruction_count(block);
        for index in 0..count {
            let instruction = editor.instruction((block, index)).clone();
            if instruction.removed
                || instruction.op.has_memory()
                || instruction.op.safepoint_kind().is_some()
                || instruction.results.len() != 1
                || matches!(instruction.op, Op::IConst(_) | Op::FConst(_))
            {
                continue;
            }
            let key = serde_json::to_vec(&(&instruction.op, &instruction.arguments))
                .expect("LIR op 可序列化");
            let result = instruction.results[0];
            if let Some((definition_block, definition_index, definition)) = table.get(&key).copied()
            {
                let available = if definition_block == block {
                    definition_index < index
                } else {
                    dominates(&dominators, definition_block, block)
                };
                if available {
                    editor.replace_value(result, definition);
                    editor.remove_instruction((block, index))?;
                    changed = true;
                    continue;
                }
            }
            table.insert(key, (block, index, result));
        }
    }
    Ok(changed)
}
