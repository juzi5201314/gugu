//! GIR 边界检查消除：消费全程序分析证明，删除可证安全的检查分支。
use super::super::body::{BlockId, LocalId, Operand, Place, Rvalue, StatementKind, Terminator};
use super::{Editor, Outcome};
use crate::Diagnostic;
use crate::frontend::analysis::{AnalysisWorldV1, ProofStatus, RuntimeCheckKey};
use crate::frontend::hir;

pub(crate) fn run(
    editor: &mut Editor,
    module: &hir::Module,
    analysis: &AnalysisWorldV1,
    bodies: &[crate::frontend::gir::body::GirBody],
    generic_body: u32,
) -> Result<Outcome, Diagnostic> {
    let Some(owner_definition) = bodies.get(generic_body as usize).map(|body| body.owner) else {
        return Ok(Outcome::default());
    };
    let Some(owner_index) = module
        .owners
        .iter()
        .position(|owner| owner.definition == owner_definition)
    else {
        return Ok(Outcome::default());
    };
    let owner = &module.owners[owner_index];
    let owner_index = u32::try_from(owner_index).expect("owner 下标适配 u32");
    let mut elided = 0;
    for block in editor.live_blocks() {
        let Terminator::SwitchInt {
            value: Operand::Copy(Place { local, .. }),
            targets,
            ..
        } = editor.terminator(block).clone()
        else {
            continue;
        };
        let Some((1, ok)) = targets.first().copied() else {
            continue;
        };
        let Some(check) = last_check(editor, block, local) else {
            continue;
        };
        let Some(runtime) = owner.checks.get(check as usize) else {
            continue;
        };
        let key = RuntimeCheckKey {
            owner_index,
            expression: runtime.expression,
            kind: runtime.kind.clone(),
        };
        if analysis.proof_status(&key) == ProofStatus::Proved {
            editor.set_terminator(block, Terminator::Goto { target: ok });
            elided += 1;
        }
    }
    Ok(Outcome {
        changed: elided > 0,
        checks_elided: elided,
        ..Outcome::default()
    })
}

/// 该 block 内对 `local` 的最后一次 `CheckedOp` 检查编号；被重定义则返回 `None`。
fn last_check(editor: &Editor, block: BlockId, local: LocalId) -> Option<u32> {
    let mut found = None;
    for (_, statement) in editor.statements(block) {
        match &statement.kind {
            StatementKind::Assign(place, Rvalue::CheckedOp { check, .. })
                if place.is_local() && place.local == local =>
            {
                found = Some(*check);
            }
            _ => {
                if super::statement_defined(statement) == Some(local) {
                    found = None;
                }
            }
        }
    }
    found
}
