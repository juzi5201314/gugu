//! GIR CFG 规范化：不可达 block 删除、单前驱合并与常量 `SwitchInt` 折叠。
use super::super::body::{BlockId, ConstValue, Operand, Place, Rvalue, StatementKind, Terminator};
use super::{Editor, Outcome};
use crate::Diagnostic;
use std::collections::BTreeMap;

pub(crate) fn simplify(editor: &mut Editor) -> Result<Outcome, Diagnostic> {
    let mut changed = editor.remove_unreachable();
    changed |= fold_switches(editor)?;
    changed |= merge_blocks(editor)?;
    changed |= editor.remove_unreachable();
    Ok(Outcome {
        changed,
        ..Outcome::default()
    })
}

/// 把常量条件的 `SwitchInt` 折叠成 `Goto`。
fn fold_switches(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut changed = false;
    for block in editor.live_blocks() {
        let Terminator::SwitchInt {
            value,
            targets,
            otherwise,
        } = editor.terminator(block).clone()
        else {
            continue;
        };
        let Some(constant) = constant_local(editor, block, &value) else {
            continue;
        };
        let Some(value) = const_to_u128(&constant) else {
            continue;
        };
        let target = targets
            .iter()
            .find(|(case, _)| *case == value)
            .map(|(_, block)| *block)
            .unwrap_or(otherwise);
        editor.set_terminator(block, Terminator::Goto { target });
        changed = true;
    }
    Ok(changed)
}

/// 若 `operand` 读取的 local 在本 block 内被赋值为常量且此后未重定义，返回该常量。
pub(crate) fn constant_local(
    editor: &Editor,
    block: BlockId,
    operand: &Operand,
) -> Option<ConstValue> {
    let Operand::Copy(Place {
        local,
        projections: (0, 0),
    }) = operand
    else {
        return None;
    };
    let mut known: BTreeMap<_, ConstValue> = BTreeMap::new();
    for (_, statement) in editor.statements(block) {
        if let StatementKind::Assign(place, Rvalue::Use(Operand::Constant(id))) = &statement.kind
            && place.is_local()
        {
            known.insert(
                place.local,
                editor.body().constants[id.index()].value.clone(),
            );
            continue;
        }
        if let Some(defined) = super::statement_defined(statement) {
            known.remove(&defined);
        }
    }
    known.get(local).cloned()
}

pub(crate) fn const_to_u128(value: &ConstValue) -> Option<u128> {
    match value {
        ConstValue::Bool(value) => Some(u128::from(*value)),
        ConstValue::Integer(value) => Some(*value),
        ConstValue::Char(value) => Some(u128::from(*value as u32)),
        _ => None,
    }
}

/// 合并「唯一前驱 + 唯一后继」的 block。
fn merge_blocks(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let protected = editor.protected_blocks();
    let mut changed = false;
    loop {
        let mut merged = false;
        for block in editor.live_blocks() {
            let Terminator::Goto { target } = editor.terminator(block) else {
                continue;
            };
            let target = *target;
            if target == block || target == editor.body().entry || protected.contains(&target) {
                continue;
            }
            if protected.contains(&block) {
                continue;
            }
            let predecessors = predecessor_count(editor, target);
            if predecessors != 1 {
                continue;
            }
            if editor.block(target).is_none() {
                continue;
            }
            if editor.block(block).expect("活跃 block").cleanup
                != editor.block(target).expect("活跃 block").cleanup
            {
                continue;
            }
            merge(editor, block, target);
            merged = true;
            changed = true;
            break;
        }
        if !merged {
            return Ok(changed);
        }
    }
}

fn predecessor_count(editor: &Editor, block: BlockId) -> usize {
    editor
        .live_blocks()
        .into_iter()
        .filter(|candidate| editor.terminator(*candidate).successors().contains(&block))
        .count()
}

fn merge(editor: &mut Editor, from: BlockId, into: BlockId) {
    let terminator = editor.terminator(into).clone();
    let indices: Vec<usize> = editor.block(into).expect("活跃 block").statements.clone();
    editor.append_statements(from, &indices);
    editor.set_terminator(from, terminator);
    editor.remove_block(into);
}
