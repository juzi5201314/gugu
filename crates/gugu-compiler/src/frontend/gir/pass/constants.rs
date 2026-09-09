//! GIR 稀疏条件常量传播：block 内常量副本与不可达边折叠。
use super::super::body::{ConstId, LocalId, Operand, Place, Rvalue, StatementKind, Terminator};
use super::{Editor, Outcome};
use crate::Diagnostic;
use std::collections::BTreeMap;

pub(crate) fn propagate(editor: &mut Editor) -> Result<Outcome, Diagnostic> {
    let mut changed = false;
    for block in editor.live_blocks() {
        // block 内 `local -> 常量` 的格；只追踪显式常量与常量副本。
        let mut known: BTreeMap<LocalId, ConstId> = BTreeMap::new();
        let indices: Vec<usize> = editor.block(block).expect("活跃 block").statements.clone();
        for index in indices {
            let kind = editor.statement(index).kind.clone();
            match kind {
                StatementKind::Assign(place, Rvalue::Use(Operand::Constant(id)))
                    if place.is_local() =>
                {
                    known.insert(place.local, id);
                }
                StatementKind::Assign(place, Rvalue::Use(Operand::Copy(source)))
                    if place.is_local() && source.is_local() =>
                {
                    match known.get(&source.local).copied() {
                        Some(id) => {
                            editor.set_statement(
                                index,
                                StatementKind::Assign(place, Rvalue::Use(Operand::Constant(id))),
                            );
                            known.insert(place.local, id);
                            changed = true;
                        }
                        None => {
                            known.remove(&place.local);
                        }
                    }
                }
                _ => {
                    if let Some(defined) = super::statement_defined(editor.statement(index)) {
                        known.remove(&defined);
                    }
                }
            }
        }
        let Terminator::SwitchInt {
            value: Operand::Copy(Place { local, .. }),
            targets,
            otherwise,
        } = editor.terminator(block).clone()
        else {
            continue;
        };
        let Some(id) = known.get(&local) else {
            continue;
        };
        let Some(value) = super::cfg::const_to_u128(&editor.body().constants[id.index()].value)
        else {
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
    Ok(Outcome {
        changed,
        ..Outcome::default()
    })
}
