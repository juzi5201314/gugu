//! CFG 规范化：常量分支折叠、不可达 block 删除与单前驱合并。
use super::constants::{Known, fold_terminators};
use super::rewrite::Editor;
use crate::Diagnostic;
use std::collections::BTreeSet;

pub(crate) fn canonicalize(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut changed = false;
    loop {
        let known = syntactic_constants(editor);
        if !fold_terminators(editor, &known) {
            break;
        }
        changed = true;
    }
    changed |= remove_unreachable(editor)?;
    changed |= merge_all(editor)?;
    Ok(changed)
}

/// 只识别 `IConst` 结果；更完整的常量传播由 SparseConditionalConstants 负责。
fn syntactic_constants(editor: &Editor) -> Known {
    let mut known: Known = vec![None; editor.values.len()];
    for block in editor.live_blocks() {
        for index in 0..editor.instruction_count(block) {
            let instruction = editor.instruction((block, index));
            if let crate::lir::body::Op::IConst(value) = &instruction.op
                && let Some(result) = instruction.results.first()
            {
                known[result.index()] = Some((*value, editor.kind(*result)));
            }
        }
    }
    known
}

/// 删除从入口不可达的 block 及其边。
pub(crate) fn remove_unreachable(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut live = BTreeSet::new();
    let mut stack = vec![editor.entry()];
    while let Some(block) = stack.pop() {
        if live.insert(block) {
            stack.extend(editor.successors(block));
        }
    }
    let dead: Vec<_> = editor
        .live_blocks()
        .into_iter()
        .filter(|block| !live.contains(block))
        .collect();
    if dead.is_empty() {
        return Ok(false);
    }
    let edges: Vec<_> = editor
        .edges()
        .map(|(id, edge)| (id, edge.from, edge.to))
        .collect();
    for (id, from, to) in edges {
        if !live.contains(&from) || !live.contains(&to) {
            editor.remove_edge(id);
        }
    }
    for block in dead {
        editor.remove_block(block)?;
    }
    Ok(true)
}

/// 合并「唯一前驱 + 唯一后继」的 block，直到不动点。
fn merge_all(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut changed = false;
    loop {
        let mut merged = false;
        for a in editor.live_blocks() {
            let successors = editor.successors(a);
            if successors.len() != 1 {
                continue;
            }
            let b = successors[0];
            if b == a || b == editor.entry() {
                continue;
            }
            let predecessors = editor.predecessors(b);
            if predecessors.len() != 1 {
                continue;
            }
            let Some(edge) = editor.edge(predecessors[0]) else {
                continue;
            };
            if edge.unwind {
                continue;
            }
            let (a_cleanup, b_cleanup) = (
                editor.block(a).expect("活跃 block").cleanup,
                editor.block(b).expect("活跃 block").cleanup,
            );
            if a_cleanup || b_cleanup {
                continue;
            }
            editor.merge_blocks(a, b)?;
            merged = true;
            changed = true;
            break;
        }
        if !merged {
            return Ok(changed);
        }
    }
}
