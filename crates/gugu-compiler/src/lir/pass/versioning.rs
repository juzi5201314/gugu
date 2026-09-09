//! 循环去开关：把循环不变的 Branch 提到 preheader，复制循环为两个版本。
//!
//! 只在两侧 effect 顺序等价（循环体内无调用、原子、volatile、屏障与 safepoint）
//! 且条件支配 preheader 时复制；否则为空操作。
use super::graph::dominates;
use super::loops::{self, LoopInfo};
use super::rewrite::{Editor, Term};
use crate::Diagnostic;

pub(crate) fn run(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut changed = false;
    for natural in loops::analyze(editor) {
        if unswitch(editor, &natural)? {
            changed = true;
        }
    }
    Ok(changed)
}

fn unswitch(editor: &mut Editor, natural: &LoopInfo) -> Result<bool, Diagnostic> {
    let header = natural.header;
    let outside: Vec<_> = editor
        .predecessors(header)
        .into_iter()
        .filter(|edge| {
            editor
                .edge(*edge)
                .is_some_and(|data| !natural.blocks.contains(&data.from))
        })
        .collect();
    let [preheader_edge] = outside.as_slice() else {
        return Ok(false);
    };
    let preheader_edge = *preheader_edge;
    let Some(preheader) = editor.edge(preheader_edge).map(|edge| edge.from) else {
        return Ok(false);
    };
    let Some(dominators) = editor.dominators() else {
        return Ok(false);
    };
    let mut candidate = None;
    for block in natural.blocks.iter().copied() {
        if block == header {
            continue;
        }
        let Term::Branch { condition, yes, no } = editor.terminator(block).clone() else {
            continue;
        };
        let Some((definition, _)) = editor.defining_instruction(condition) else {
            continue;
        };
        if natural.blocks.contains(&definition) || !dominates(&dominators, definition, preheader) {
            continue;
        }
        candidate = Some((block, condition, yes, no));
        break;
    }
    let Some((branch_block, condition, yes, no)) = candidate else {
        return Ok(false);
    };
    if !loops::effect_free(editor, natural) {
        return Ok(false);
    }
    let blocks: Vec<_> = natural.blocks.iter().copied().collect();
    let map = loops::clone_subgraph(editor, &blocks);
    let clone_branch = map[&branch_block];
    let Term::Branch {
        yes: clone_yes,
        no: clone_no,
        ..
    } = editor.terminator(clone_branch).clone()
    else {
        return Err(crate::lir::invalid("克隆的 branch block 丢失了分支"));
    };
    // 版本 A（原循环）只走 yes，版本 B（克隆）只走 no。
    editor.set_terminator(branch_block, Term::Jump(yes));
    editor.remove_edge(no);
    editor.set_terminator(clone_branch, Term::Jump(clone_no));
    editor.remove_edge(clone_yes);
    // preheader 改为按不变量分支。
    let arguments = editor
        .edge(preheader_edge)
        .expect("活跃边")
        .arguments
        .clone();
    let yes_edge = editor.add_edge(preheader, header, arguments.clone());
    let clone_header = map[&header];
    let no_edge = editor.add_edge(preheader, clone_header, arguments);
    editor.set_terminator(
        preheader,
        Term::Branch {
            condition,
            yes: yes_edge,
            no: no_edge,
        },
    );
    editor.remove_edge(preheader_edge);
    super::cfg::remove_unreachable(editor)?;
    Ok(true)
}
