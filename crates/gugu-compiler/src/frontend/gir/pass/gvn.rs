//! GIR 复制传播与块内公共子表达式消除。
//!
//! 复制传播只处理「临时 local 恰好一次读取、来源 local 至多一次定义」的形态，
//! 且读取点只限 `Assign` 的右值与 `SwitchInt` 条件；CSE 只在同一 block 内、
//! 操作数在该 block 内不被重定义时替换，绝不跨越 effect fence。
use super::super::body::{
    BlockId, LocalId, LocalKind, Operand, Place, Rvalue, StatementKind, Terminator,
};
use super::{Editor, Outcome};
use crate::Diagnostic;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn run(editor: &mut Editor) -> Result<Outcome, Diagnostic> {
    let mut changed = copy_propagate(editor)?;
    changed |= cse(editor);
    Ok(Outcome {
        changed,
        ..Outcome::default()
    })
}

/// 复制传播能安全替换的读取点。
#[derive(Clone, Copy, Debug)]
enum ReadSite {
    Statement(usize),
    Terminator(BlockId),
}

fn rvalue_locals(rvalue: &Rvalue, out: &mut BTreeSet<LocalId>) {
    match rvalue {
        Rvalue::Use(operand) | Rvalue::UnaryOp { operand, .. } => operand_local(operand, out),
        Rvalue::BinaryOp { left, right, .. } | Rvalue::Compare { left, right, .. } => {
            operand_local(left, out);
            operand_local(right, out);
        }
        Rvalue::CheckedOp { operands, .. }
        | Rvalue::Aggregate { operands, .. }
        | Rvalue::AllocObject { operands, .. }
        | Rvalue::Intrinsic { operands, .. } => {
            for operand in operands {
                operand_local(operand, out);
            }
        }
        Rvalue::Repeat { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::DynErase { operand, .. } => operand_local(operand, out),
        Rvalue::AllocArray { length, .. } => operand_local(length, out),
        Rvalue::Discriminant(place)
        | Rvalue::Len(place)
        | Rvalue::Ref(place)
        | Rvalue::RawAddress(place)
        | Rvalue::ValueCopy(place)
        | Rvalue::CowSnapshot(place) => {
            out.insert(place.local);
        }
        Rvalue::StackSlotAddress(_) | Rvalue::FunctionValue(_) => {}
    }
}

fn operand_local(operand: &Operand, out: &mut BTreeSet<LocalId>) {
    if let Operand::Copy(place) | Operand::MoveInternal(place) = operand {
        out.insert(place.local);
    }
}

fn copy_propagate(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut reads: BTreeMap<LocalId, Vec<ReadSite>> = BTreeMap::new();
    let mut defs: BTreeMap<LocalId, u32> = BTreeMap::new();
    for block in editor.live_blocks() {
        for (index, statement) in editor.statements(block) {
            if let StatementKind::Assign(_, rvalue) = &statement.kind {
                let mut locals = BTreeSet::new();
                rvalue_locals(rvalue, &mut locals);
                for local in locals {
                    reads
                        .entry(local)
                        .or_default()
                        .push(ReadSite::Statement(index));
                }
            }
            if let Some(defined) = super::statement_defined(statement) {
                *defs.entry(defined).or_default() += 1;
            }
        }
        if let Terminator::SwitchInt {
            value: Operand::Copy(place),
            ..
        } = editor.terminator(block)
        {
            reads
                .entry(place.local)
                .or_default()
                .push(ReadSite::Terminator(block));
        }
    }
    let mut changed = false;
    for block in editor.live_blocks() {
        let indices: Vec<usize> = editor.block(block).expect("活跃 block").statements.clone();
        for index in indices {
            let StatementKind::Assign(place, Rvalue::Use(Operand::Copy(source))) =
                editor.statement(index).kind.clone()
            else {
                continue;
            };
            if !place.is_local()
                || !source.is_local()
                || place.local == source.local
                || editor.body().locals[place.local.index()].kind != LocalKind::Temporary
            {
                continue;
            }
            if reads.get(&place.local).map_or(0, Vec::len) != 1
                || defs.get(&source.local).copied().unwrap_or(0) > 1
            {
                continue;
            }
            let site = reads[&place.local][0];
            let rewritten = match site {
                ReadSite::Statement(target) => {
                    let mut kind = editor.statement(target).kind.clone();
                    if let StatementKind::Assign(_, rvalue) = &mut kind {
                        if rewrite_rvalue(rvalue, place.local, source.local) {
                            editor.set_statement(target, kind);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
                ReadSite::Terminator(target) => {
                    let mut terminator = editor.terminator(target).clone();
                    if let Terminator::SwitchInt { value, .. } = &mut terminator {
                        if rewrite_operand(value, place.local, source.local) {
                            editor.set_terminator(target, terminator);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
            };
            if rewritten {
                editor.remove_statement(block, index);
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn rewrite_operand(operand: &mut Operand, from: LocalId, to: LocalId) -> bool {
    match operand {
        Operand::Copy(place) | Operand::MoveInternal(place) => {
            if place.local == from {
                place.local = to;
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

fn rewrite_rvalue(rvalue: &mut Rvalue, from: LocalId, to: LocalId) -> bool {
    match rvalue {
        Rvalue::Use(operand) | Rvalue::UnaryOp { operand, .. } => {
            rewrite_operand(operand, from, to)
        }
        Rvalue::BinaryOp { left, right, .. } | Rvalue::Compare { left, right, .. } => {
            rewrite_operand(left, from, to) | rewrite_operand(right, from, to)
        }
        Rvalue::CheckedOp { operands, .. }
        | Rvalue::Aggregate { operands, .. }
        | Rvalue::AllocObject { operands, .. }
        | Rvalue::Intrinsic { operands, .. } => {
            let mut changed = false;
            for operand in operands {
                changed |= rewrite_operand(operand, from, to);
            }
            changed
        }
        Rvalue::Repeat { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::DynErase { operand, .. } => rewrite_operand(operand, from, to),
        Rvalue::AllocArray { length, .. } => rewrite_operand(length, from, to),
        Rvalue::Discriminant(place)
        | Rvalue::Len(place)
        | Rvalue::Ref(place)
        | Rvalue::RawAddress(place)
        | Rvalue::ValueCopy(place)
        | Rvalue::CowSnapshot(place) => {
            if place.local == from {
                place.local = to;
                true
            } else {
                false
            }
        }
        Rvalue::StackSlotAddress(_) | Rvalue::FunctionValue(_) => false,
    }
}

/// 块内 CSE：同一 block 内相同纯 `Rvalue` 只计算一次。
fn cse(editor: &mut Editor) -> bool {
    let mut changed = false;
    for block in editor.live_blocks() {
        let mut seen: BTreeMap<Vec<u8>, LocalId> = BTreeMap::new();
        let mut defined_in_block: BTreeSet<LocalId> = BTreeSet::new();
        let indices: Vec<usize> = editor.block(block).expect("活跃 block").statements.clone();
        for index in indices {
            let kind = editor.statement(index).kind.clone();
            let StatementKind::Assign(place, rvalue) = &kind else {
                if let Some(defined) = super::statement_defined(editor.statement(index)) {
                    defined_in_block.insert(defined);
                }
                continue;
            };
            if !place.is_local() || !pure_rvalue(rvalue) {
                if let Some(defined) = super::statement_defined(editor.statement(index)) {
                    defined_in_block.insert(defined);
                }
                continue;
            }
            let mut locals = BTreeSet::new();
            rvalue_locals(rvalue, &mut locals);
            locals.remove(&place.local);
            if locals.iter().any(|local| defined_in_block.contains(local)) {
                defined_in_block.insert(place.local);
                continue;
            }
            let key = serde_json::to_vec(rvalue).expect("GIR rvalue 可序列化");
            match seen.get(&key).copied() {
                Some(existing) if !defined_in_block.contains(&existing) => {
                    editor.set_statement(
                        index,
                        StatementKind::Assign(
                            *place,
                            Rvalue::Use(Operand::Copy(Place::local(existing))),
                        ),
                    );
                    changed = true;
                }
                _ => {
                    seen.insert(key, place.local);
                }
            }
            defined_in_block.insert(place.local);
        }
    }
    changed
}

fn pure_rvalue(rvalue: &Rvalue) -> bool {
    !matches!(
        rvalue,
        Rvalue::AllocObject { .. }
            | Rvalue::AllocArray { .. }
            | Rvalue::ValueCopy(_)
            | Rvalue::CowSnapshot(_)
            | Rvalue::Intrinsic { .. }
            | Rvalue::CheckedOp { .. }
    )
}
