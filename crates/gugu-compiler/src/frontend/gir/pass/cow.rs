//! GIR COW 与资源动作消除：基于局部活跃性删除无读取的复制与相邻租约对。
use super::super::body::{BlockId, LocalId, ResourceActionKind, Rvalue, StatementKind};
use super::{Editor, Outcome};
use crate::Diagnostic;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn run(editor: &mut Editor) -> Result<Outcome, Diagnostic> {
    let mut changed = remove_lease_pairs(editor);
    changed |= remove_dead_copies(editor)?;
    Ok(Outcome {
        changed,
        ..Outcome::default()
    })
}

/// 相邻的 `AcquireLease` + `ReleaseLease`（同一 slot、中间无读取）成对删除。
fn remove_lease_pairs(editor: &mut Editor) -> bool {
    let mut changed = false;
    for block in editor.live_blocks() {
        if editor.block(block).expect("活跃 block").cleanup {
            continue;
        }
        let indices: Vec<usize> = editor.block(block).expect("活跃 block").statements.clone();
        let mut position = 0;
        while position + 1 < indices.len() {
            let acquire = indices[position];
            let release = indices[position + 1];
            let paired = match (
                &editor.statement(acquire).kind,
                &editor.statement(release).kind,
            ) {
                (
                    StatementKind::ResourceAction {
                        action: ResourceActionKind::AcquireLease,
                        place: acquire,
                        ..
                    },
                    StatementKind::ResourceAction {
                        action: ResourceActionKind::ReleaseLease,
                        place: release,
                        ..
                    },
                ) => acquire.local == release.local && acquire.is_local(),
                _ => false,
            };
            if paired {
                editor.remove_statement(block, acquire);
                editor.remove_statement(block, release);
                changed = true;
                position += 2;
            } else {
                position += 1;
            }
        }
    }
    changed
}

fn remove_dead_copies(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let (generated_by_block, kill) = block_transfer(editor);
    let live_in = liveness(editor, &generated_by_block, &kill);
    let mut changed = false;
    for block in editor.live_blocks() {
        if editor.block(block).expect("活跃 block").cleanup {
            // cleanup 序列由 `cleanup_sequences` verifier 逐条重建，不能删除动作。
            continue;
        }
        let indices: Vec<usize> = editor.block(block).expect("活跃 block").statements.clone();
        let mut live = live_out(editor, block, &live_in);
        for index in indices.into_iter().rev() {
            let statement = editor.statement(index).clone();
            let dead = match &statement.kind {
                // `CowSnapshot` 的写入目标是 `Assign` 的 local，可安全按活跃性删除；
                // `ValueAction::Copy` 同时承担初始化语义，删除会破坏初始化数据流。
                StatementKind::Assign(place, Rvalue::CowSnapshot(source))
                    if place.is_local() && place.local != source.local =>
                {
                    !live.contains(&place.local)
                }
                _ => false,
            };
            if dead {
                editor.remove_statement(block, index);
                changed = true;
                continue;
            }
            if let Some(defined) = super::statement_defined(&statement) {
                live.remove(&defined);
            }
            let mut reads = BTreeSet::new();
            super::statement_reads(&statement, &mut reads);
            live.extend(reads);
        }
    }
    Ok(changed)
}

fn block_transfer(
    editor: &Editor,
) -> (
    BTreeMap<BlockId, BTreeSet<LocalId>>,
    BTreeMap<BlockId, BTreeSet<LocalId>>,
) {
    let mut generated_by_block = BTreeMap::new();
    let mut kill = BTreeMap::new();
    for block in editor.live_blocks() {
        let mut generated = BTreeSet::new();
        let mut killed = BTreeSet::new();
        for (_, statement) in editor.statements(block) {
            let mut reads = BTreeSet::new();
            super::statement_reads(statement, &mut reads);
            for read in reads {
                if !killed.contains(&read) {
                    generated.insert(read);
                }
            }
            if let Some(defined) = super::statement_defined(statement) {
                killed.insert(defined);
            }
        }
        generated_by_block.insert(block, generated);
        kill.insert(block, killed);
    }
    (generated_by_block, kill)
}

fn liveness(
    editor: &Editor,
    generated_by_block: &BTreeMap<BlockId, BTreeSet<LocalId>>,
    kill: &BTreeMap<BlockId, BTreeSet<LocalId>>,
) -> BTreeMap<BlockId, BTreeSet<LocalId>> {
    let blocks = editor.live_blocks();
    let mut live_in: BTreeMap<BlockId, BTreeSet<LocalId>> = blocks
        .iter()
        .map(|block| (*block, BTreeSet::new()))
        .collect();
    loop {
        let mut changed = false;
        for block in &blocks {
            let mut out = BTreeSet::new();
            for successor in editor.terminator(*block).successors() {
                if let Some(successor_live) = live_in.get(&successor) {
                    out.extend(successor_live.iter().copied());
                }
            }
            for read in generated_by_block.get(block).into_iter().flatten() {
                if !kill.get(block).is_some_and(|kill| kill.contains(read)) {
                    out.insert(*read);
                }
            }
            for defined in kill.get(block).into_iter().flatten() {
                out.remove(defined);
            }
            // 终结符读取的值也必须活跃。
            let mut terminator_reads = BTreeSet::new();
            terminator_locals(editor, *block, &mut terminator_reads);
            out.extend(terminator_reads);
            if live_in[block] != out {
                live_in.insert(*block, out);
                changed = true;
            }
        }
        if !changed {
            return live_in;
        }
    }
}

fn live_out(
    editor: &Editor,
    block: BlockId,
    live_in: &BTreeMap<BlockId, BTreeSet<LocalId>>,
) -> BTreeSet<LocalId> {
    let mut out = BTreeSet::new();
    for successor in editor.terminator(block).successors() {
        if let Some(successor_live) = live_in.get(&successor) {
            out.extend(successor_live.iter().copied());
        }
    }
    terminator_locals(editor, block, &mut out);
    out
}

fn terminator_locals(editor: &Editor, block: BlockId, out: &mut BTreeSet<LocalId>) {
    use super::super::body::{SuspendReason, Terminator};
    let operand = |operand: &super::super::body::Operand, out: &mut BTreeSet<LocalId>| {
        if let super::super::body::Operand::Copy(place)
        | super::super::body::Operand::MoveInternal(place) = operand
        {
            out.insert(place.local);
        }
    };
    match editor.terminator(block) {
        Terminator::SwitchInt { value, .. } => operand(value, out),
        Terminator::Call { args, .. } => {
            for argument in args {
                operand(argument, out);
            }
        }
        Terminator::Panic { payload, .. } => operand(payload, out),
        // `Return` 隐式读取返回槽 LocalId(0)。
        Terminator::Return => {
            out.insert(LocalId(0));
        }
        Terminator::Suspend { reason, .. } => match reason {
            SuspendReason::ChanSend { channel, value } => {
                operand(channel, out);
                operand(value, out);
            }
            SuspendReason::ChanRecv { channel } | SuspendReason::JoinWait { join: channel } => {
                operand(channel, out)
            }
            SuspendReason::Yield => {}
        },
        _ => {}
    }
}
