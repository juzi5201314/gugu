//! GIR 复制传播与块内公共子表达式消除。
//!
//! 复制传播只处理「临时 local 恰好一次读取、来源 local 至多一次定义」的形态，
//! 且读取点只限 `Assign` 的右值与 `SwitchInt` 条件；CSE 只在同一 block 内、
//! 操作数在该 block 内不被重定义时替换，绝不跨越 effect fence。
use super::super::body::{
    BlockId, LocalId, LocalKind, Operand, Place, Rvalue, StatementKind, Terminator,
};
use super::{Editor, LocalSet, Outcome};
use crate::Diagnostic;
use std::collections::BTreeMap;

pub fn run(editor: &mut Editor) -> Result<Outcome, Diagnostic> {
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

fn rvalue_locals(rvalue: &Rvalue, out: &mut LocalSet) {
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

fn operand_local(operand: &Operand, out: &mut LocalSet) {
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
                let mut locals = LocalSet::new();
                rvalue_locals(rvalue, &mut locals);
                for local in locals.iter() {
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
        Operand::Copy(place) | Operand::MoveInternal(place) if place.local == from => {
            place.local = to;
            true
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
        let mut defined_in_block = LocalSet::new();
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
            let mut locals = LocalSet::new();
            rvalue_locals(rvalue, &mut locals);
            locals.remove(place.local);
            if locals.iter().any(|local| defined_in_block.contains(local)) {
                defined_in_block.insert(place.local);
                continue;
            }
            let key = rvalue_key(rvalue);
            match seen.get(&key).copied() {
                Some(existing) if !defined_in_block.contains(existing) => {
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

fn rvalue_key(rvalue: &Rvalue) -> Vec<u8> {
    let mut key = Vec::new();
    match rvalue {
        Rvalue::Use(operand) => {
            key.push(0);
            operand_key(&mut key, operand);
        }
        Rvalue::UnaryOp { op, operand } => {
            key.extend_from_slice(&[1, unary_tag(*op)]);
            operand_key(&mut key, operand);
        }
        Rvalue::BinaryOp { op, left, right } => {
            key.extend_from_slice(&[2, binary_tag(*op)]);
            operand_key(&mut key, left);
            operand_key(&mut key, right);
        }
        Rvalue::Compare { op, left, right } => {
            key.extend_from_slice(&[3, compare_tag(*op)]);
            operand_key(&mut key, left);
            operand_key(&mut key, right);
        }
        Rvalue::Repeat { operand, count } => {
            key.push(4);
            operand_key(&mut key, operand);
            key.extend_from_slice(&count.to_le_bytes());
        }
        Rvalue::Discriminant(place) => place_key(&mut key, 5, *place),
        Rvalue::Len(place) => place_key(&mut key, 6, *place),
        Rvalue::Ref(place) => place_key(&mut key, 7, *place),
        Rvalue::RawAddress(place) => place_key(&mut key, 8, *place),
        Rvalue::Cast { kind, operand, ty } => {
            key.extend_from_slice(&[9, cast_tag(*kind)]);
            operand_key(&mut key, operand);
            key.extend_from_slice(&ty.0.to_le_bytes());
        }
        Rvalue::FunctionValue(candidate) => {
            key.push(10);
            key.extend_from_slice(&candidate.definition.0.to_le_bytes());
            key.extend_from_slice(&candidate.signature.0.to_le_bytes());
        }
        Rvalue::DynErase { operand, ty } => {
            key.push(11);
            operand_key(&mut key, operand);
            key.extend_from_slice(&ty.0.to_le_bytes());
        }
        Rvalue::Aggregate { kind, operands } => {
            key.push(12);
            aggregate_key(&mut key, kind);
            for operand in operands {
                operand_key(&mut key, operand);
            }
        }
        Rvalue::StackSlotAddress(local) => {
            key.push(13);
            key.extend_from_slice(&local.0.to_le_bytes());
        }
        _ => unreachable!("GVN key 只接受 pure_rvalue"),
    }
    key
}

fn aggregate_key(key: &mut Vec<u8>, kind: &super::super::body::AggregateKind) {
    match kind {
        super::super::body::AggregateKind::Tuple => key.push(0),
        super::super::body::AggregateKind::Array(ty) => {
            key.push(1);
            key.extend_from_slice(&ty.0.to_le_bytes());
        }
        super::super::body::AggregateKind::Adt { ty, variant } => {
            key.push(2);
            key.extend_from_slice(&ty.0.to_le_bytes());
            key.extend_from_slice(&variant.to_le_bytes());
        }
        super::super::body::AggregateKind::Union { ty, field } => {
            key.push(3);
            key.extend_from_slice(&ty.0.to_le_bytes());
            key.extend_from_slice(&field.to_le_bytes());
        }
        super::super::body::AggregateKind::Range => key.push(4),
        super::super::body::AggregateKind::Closure(definition)
        | super::super::body::AggregateKind::Coroutine(definition) => {
            key.push(5);
            key.extend_from_slice(&definition.0.to_le_bytes());
        }
    }
}

fn operand_key(key: &mut Vec<u8>, operand: &Operand) {
    match operand {
        Operand::Copy(place) => place_key(key, 0, *place),
        Operand::MoveInternal(place) => place_key(key, 1, *place),
        Operand::Constant(constant) => {
            key.push(2);
            key.extend_from_slice(&constant.0.to_le_bytes());
        }
        Operand::LateConstRef { expression } => {
            key.push(3);
            key.extend_from_slice(&expression.to_le_bytes());
        }
        Operand::Function(candidate) => {
            key.push(4);
            key.extend_from_slice(&candidate.definition.0.to_le_bytes());
            key.extend_from_slice(&candidate.signature.0.to_le_bytes());
        }
    }
}

fn place_key(key: &mut Vec<u8>, tag: u8, place: super::super::body::Place) {
    key.push(tag);
    key.extend_from_slice(&place.local.0.to_le_bytes());
    key.extend_from_slice(&place.projections.0.to_le_bytes());
    key.extend_from_slice(&place.projections.1.to_le_bytes());
}

fn unary_tag(op: super::super::body::UnaryOp) -> u8 {
    match op {
        super::super::body::UnaryOp::Neg => 0,
        super::super::body::UnaryOp::Not => 1,
    }
}

fn binary_tag(op: super::super::body::BinaryOp) -> u8 {
    match op {
        super::super::body::BinaryOp::Add => 0,
        super::super::body::BinaryOp::Sub => 1,
        super::super::body::BinaryOp::Mul => 2,
        super::super::body::BinaryOp::Div => 3,
        super::super::body::BinaryOp::Rem => 4,
        super::super::body::BinaryOp::BitAnd => 5,
        super::super::body::BinaryOp::BitOr => 6,
        super::super::body::BinaryOp::BitXor => 7,
        super::super::body::BinaryOp::Shl => 8,
        super::super::body::BinaryOp::Shr => 9,
    }
}

fn compare_tag(op: super::super::body::CompareOp) -> u8 {
    match op {
        super::super::body::CompareOp::Eq => 0,
        super::super::body::CompareOp::Ne => 1,
        super::super::body::CompareOp::Lt => 2,
        super::super::body::CompareOp::Le => 3,
        super::super::body::CompareOp::Gt => 4,
        super::super::body::CompareOp::Ge => 5,
    }
}

fn cast_tag(kind: super::super::body::CastKind) -> u8 {
    match kind {
        super::super::body::CastKind::Scalar => 0,
        super::super::body::CastKind::Pointer => 1,
        super::super::body::CastKind::Transmute => 2,
        super::super::body::CastKind::NeverTo => 3,
        super::super::body::CastKind::ArrayToSlice => 4,
        super::super::body::CastKind::Opaque => 5,
        super::super::body::CastKind::Instantiate => 6,
        super::super::body::CastKind::FunctionErase => 7,
        super::super::body::CastKind::AssumeInit => 8,
        super::super::body::CastKind::MaybeUninit => 9,
    }
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
