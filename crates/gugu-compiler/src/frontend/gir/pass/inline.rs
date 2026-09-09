//! GIR 内联：无 cleanup、单出口的纯标量直接调用展开到调用点。
//!
//! 候选限定为「入口直接跳转到一个以 `Return` 结束的块、出口记录只有 Normal 且
//! `entry == destination == 返回块`、语句只含 storage 标记与纯标量赋值」的 callee。
//! 参数、投影、跨体作用域与 cleanup 区域的复制属于后端阶段的内联器；这里保持调用。
use super::super::body::{
    BlockId, CallKind, CleanupChain, GirBody, LocalId, Operand, Place, Rvalue, Statement,
    StatementKind, Terminator, ValueActionKind,
};
use super::{Editor, Outcome};
use crate::Diagnostic;
use crate::frontend::gir::concrete::ConcreteCall;
use crate::frontend::hir;
use std::collections::BTreeMap;

/// 普通候选的成本上界。
const INLINE_MAX_COST: u32 = 32;

pub(crate) fn run(
    editor: &mut Editor,
    _module: &hir::Module,
    callees: &BTreeMap<[u8; 32], GirBody>,
    calls: &[ConcreteCall],
) -> Result<Outcome, Diagnostic> {
    let mut inlined = 0;
    for block in editor.live_blocks() {
        let Terminator::Call {
            args,
            destination,
            normal,
            call_kind,
            site,
            ..
        } = editor.terminator(block).clone()
        else {
            continue;
        };
        if call_kind != CallKind::Managed || !args.is_empty() {
            continue;
        }
        let Some(call) = calls.iter().find(|call| call.site == site) else {
            continue;
        };
        let Some(target) = call.target else {
            continue;
        };
        let Some(callee) = callees.get(&target) else {
            continue;
        };
        if cost(callee) > INLINE_MAX_COST {
            continue;
        }
        let Some(plan) = plan(callee) else {
            continue;
        };
        let source = editor.block(block).expect("活跃 block").source.clone();
        let mut map: Vec<Option<LocalId>> = Vec::with_capacity(callee.locals.len());
        map.push(Some(destination.local));
        for local in callee.locals.iter().skip(1) {
            let mut fresh = local.clone();
            fresh.source_scope = source.scope;
            fresh.hir_local = None;
            fresh.address_taken = false;
            map.push(Some(editor.add_local(fresh)));
        }
        for kind in plan {
            let kind = remap(&kind, &map, editor, callee)?;
            editor.push_statement(
                block,
                Statement {
                    kind,
                    source: source.clone(),
                },
            );
        }
        editor.set_terminator(block, Terminator::Goto { target: normal });
        inlined += 1;
    }
    Ok(Outcome {
        changed: inlined > 0,
        inlined,
        checks_elided: 0,
    })
}

/// 成本上界：普通语句 1、分支 2、直接调用 5、分配 8、可能 suspend 32。
fn cost(callee: &GirBody) -> u32 {
    let mut total = 0u32;
    for statement in &callee.statements {
        total = total.saturating_add(match &statement.kind {
            StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => 0,
            StatementKind::Assign(_, Rvalue::CheckedOp { .. }) => 2,
            StatementKind::ValueAction { .. }
            | StatementKind::ResourceAction { .. }
            | StatementKind::GcWrite { .. } => 8,
            _ => 1,
        });
    }
    for block in &callee.blocks {
        total = total.saturating_add(match &block.terminator {
            Terminator::Call { .. } => 5,
            Terminator::Suspend { .. } | Terminator::SelectCommit { .. } => 32,
            Terminator::SwitchInt { .. } => 2,
            _ => 1,
        });
    }
    total
}

/// 返回可展开语句序列；形态不符返回 `None`。
fn plan(callee: &GirBody) -> Option<Vec<StatementKind>> {
    if !callee.cleanup_regions.is_empty()
        || !callee.no_safepoint_regions.is_empty()
        || !callee.safepoints.is_empty()
        || callee.locals.is_empty()
        || callee.blocks.len() != 2
    {
        return None;
    }
    let Terminator::Goto { target } = callee.blocks[0].terminator else {
        return None;
    };
    if target.index() != 1 || !matches!(callee.blocks[1].terminator, Terminator::Return) {
        return None;
    }
    if callee.exit_records.iter().any(|record| {
        record.chain != CleanupChain::Normal
            || record.entry.index() != 1
            || record.destination.map(BlockId::index) != Some(1)
    }) {
        return None;
    }
    let mut kinds = Vec::new();
    for statement in callee.block_statements(BlockId(0)) {
        match &statement.kind {
            StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
            StatementKind::Assign(place, rvalue) => {
                if !place.is_local() || !pure_rvalue(rvalue) {
                    return None;
                }
            }
            StatementKind::ValueAction {
                action: ValueActionKind::Copy,
                place,
                ..
            } => {
                if !place.is_local() {
                    return None;
                }
            }
            _ => return None,
        }
        kinds.push(statement.kind.clone());
    }
    Some(kinds)
}

fn pure_rvalue(rvalue: &Rvalue) -> bool {
    match rvalue {
        Rvalue::Use(operand) => pure_operand(operand),
        Rvalue::UnaryOp { operand, .. } => pure_operand(operand),
        Rvalue::BinaryOp { left, right, .. } => pure_operand(left) && pure_operand(right),
        Rvalue::ValueCopy(place) => place.is_local(),
        _ => false,
    }
}

fn pure_operand(operand: &Operand) -> bool {
    match operand {
        Operand::Copy(place) | Operand::MoveInternal(place) => place.is_local(),
        Operand::Constant(_) => true,
        _ => false,
    }
}

fn remap(
    kind: &StatementKind,
    map: &[Option<LocalId>],
    editor: &mut Editor,
    callee: &GirBody,
) -> Result<StatementKind, Diagnostic> {
    Ok(match kind {
        // 返回槽的存储由调用方管理，内联体不再重复标记。
        StatementKind::StorageLive(local) if local.index() == 0 => StatementKind::Nop,
        StatementKind::StorageDead(local) if local.index() == 0 => StatementKind::Nop,
        StatementKind::StorageLive(local) => StatementKind::StorageLive(remap_local(*local, map)?),
        StatementKind::StorageDead(local) => StatementKind::StorageDead(remap_local(*local, map)?),
        StatementKind::Nop => StatementKind::Nop,
        StatementKind::Assign(place, rvalue) => StatementKind::Assign(
            remap_place(*place, map)?,
            remap_rvalue(rvalue, map, editor, callee)?,
        ),
        StatementKind::ValueAction {
            action: ValueActionKind::Copy,
            place,
            descriptor,
        } => StatementKind::ValueAction {
            action: ValueActionKind::Copy,
            place: remap_place(*place, map)?,
            descriptor: *descriptor,
        },
        _ => {
            return Err(crate::frontend::gir::gir_error(
                "内联候选包含未登记语句",
                None,
            ));
        }
    })
}

fn remap_local(local: LocalId, map: &[Option<LocalId>]) -> Result<LocalId, Diagnostic> {
    map.get(local.index())
        .copied()
        .flatten()
        .ok_or_else(|| crate::frontend::gir::gir_error("内联局部未映射", None))
}

fn remap_place(place: Place, map: &[Option<LocalId>]) -> Result<Place, Diagnostic> {
    if !place.is_local() {
        return Err(crate::frontend::gir::gir_error("内联候选包含投影", None));
    }
    Ok(Place::local(remap_local(place.local, map)?))
}

fn remap_rvalue(
    rvalue: &Rvalue,
    map: &[Option<LocalId>],
    editor: &mut Editor,
    callee: &GirBody,
) -> Result<Rvalue, Diagnostic> {
    let operand = |operand: &Operand, editor: &mut Editor| -> Result<Operand, Diagnostic> {
        Ok(match operand {
            Operand::Copy(place) => Operand::Copy(remap_place(*place, map)?),
            Operand::MoveInternal(place) => Operand::MoveInternal(remap_place(*place, map)?),
            Operand::Constant(id) => {
                Operand::Constant(editor.add_constant(callee.constants[id.index()].clone()))
            }
            _ => {
                return Err(crate::frontend::gir::gir_error(
                    "内联候选包含非常量操作数",
                    None,
                ));
            }
        })
    };
    Ok(match rvalue {
        Rvalue::Use(value) => Rvalue::Use(operand(value, editor)?),
        Rvalue::UnaryOp { op, operand: value } => Rvalue::UnaryOp {
            op: *op,
            operand: operand(value, editor)?,
        },
        Rvalue::BinaryOp { op, left, right } => Rvalue::BinaryOp {
            op: *op,
            left: operand(left, editor)?,
            right: operand(right, editor)?,
        },
        Rvalue::ValueCopy(place) => Rvalue::ValueCopy(remap_place(*place, map)?),
        _ => {
            return Err(crate::frontend::gir::gir_error(
                "内联候选包含未登记 rvalue",
                None,
            ));
        }
    })
}
