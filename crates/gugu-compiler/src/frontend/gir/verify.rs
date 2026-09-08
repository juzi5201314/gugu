//! GIR 结构、初始化、cleanup 序列、ScopedView 与 NoSafepoint 不变量。
use super::body::*;
use super::gir_error;
use crate::Diagnostic;
use crate::frontend::hir::{self, CleanupAction};
use std::collections::BTreeSet;

pub(crate) fn verify(module: &hir::Module, body: &GirBody) -> Result<(), Diagnostic> {
    structure(module, body)?;
    predecessors(body)?;
    storage(body)?;
    cleanup_sequences(module, body)?;
    suspend_cancelled(body)?;
    scoped_views(body)?;
    no_safepoint(body)?;
    Ok(())
}

fn structure(module: &hir::Module, body: &GirBody) -> Result<(), Diagnostic> {
    if body.revision != GIR_REVISION {
        return Err(gir_error("GIR revision 与规范不一致", None));
    }
    if body.entry.index() >= body.blocks.len() {
        return Err(gir_error("GIR 入口 block 越界", None));
    }
    if body.locals.is_empty() || body.locals[0].kind != LocalKind::Return {
        return Err(gir_error("LocalId(0) 必须是返回槽", None));
    }
    if body.expression_locals.len()
        != module
            .owners
            .iter()
            .find(|owner| owner.definition == body.owner)
            .map(|owner| owner.expressions.len())
            .unwrap_or(body.expression_locals.len())
    {
        return Err(gir_error("GIR 表达式 local 表与 HIR owner 不对齐", None));
    }
    for (index, block) in body.blocks.iter().enumerate() {
        if (block.statements.end as usize) > body.statements.len()
            || (block.predecessors.end as usize) > body.predecessors.len()
        {
            return Err(gir_error(&format!("block {index} 的范围越界"), None));
        }
        for successor in block.terminator.successors() {
            if successor.index() >= body.blocks.len() {
                return Err(gir_error("终结符后继越界", None));
            }
        }
        check_terminator(body, &block.terminator)?;
    }
    Ok(())
}

fn check_terminator(body: &GirBody, terminator: &Terminator) -> Result<(), Diagnostic> {
    match terminator {
        Terminator::Call { destination, .. } => place_in_body(body, *destination),
        Terminator::SwitchInt { value, .. } => operand_in_body(body, value),
        Terminator::Panic { payload, .. } => operand_in_body(body, payload),
        Terminator::Suspend {
            reason,
            safepoint,
            cancelled,
            ..
        } => {
            if safepoint.index() >= body.safepoints.len() {
                return Err(gir_error("Suspend 缺少 safepoint", None));
            }
            match reason {
                SuspendReason::ChanSend { .. } if cancelled.is_none() => {
                    Err(gir_error("ChanSend 必须有 cancelled 后继", None))
                }
                SuspendReason::ChanRecv { .. }
                | SuspendReason::JoinWait { .. }
                | SuspendReason::Yield
                    if cancelled.is_some() =>
                {
                    Err(gir_error("非 send 的 Suspend 不能有 cancelled", None))
                }
                _ => Ok(()),
            }
        }
        Terminator::SelectCommit {
            cases, safepoint, ..
        } => {
            if safepoint.index() >= body.safepoints.len()
                || (cases.end as usize) > body.select_cases.len()
            {
                return Err(gir_error("SelectCommit 范围越界", None));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn place_in_body(body: &GirBody, place: Place) -> Result<(), Diagnostic> {
    if place.local.index() >= body.locals.len()
        || place.projections.1 as usize > body.projections.len()
    {
        return Err(gir_error("place 引用越界", None));
    }
    Ok(())
}

fn operand_in_body(body: &GirBody, operand: &Operand) -> Result<(), Diagnostic> {
    match operand {
        Operand::Copy(place) | Operand::MoveInternal(place) => place_in_body(body, *place),
        Operand::Constant(id) if id.index() >= body.constants.len() => {
            Err(gir_error("常量引用越界", None))
        }
        _ => Ok(()),
    }
}

fn predecessors(body: &GirBody) -> Result<(), Diagnostic> {
    let mut computed = vec![Vec::new(); body.blocks.len()];
    for (index, block) in body.blocks.iter().enumerate() {
        for successor in block.terminator.successors() {
            computed[successor.index()].push(BlockId(index as u32));
        }
    }
    for (index, expected) in computed.iter().enumerate() {
        let actual = body.predecessors_of(BlockId(index as u32));
        if actual != expected.as_slice() {
            return Err(gir_error("前驱表与终结符后继不一致", None));
        }
    }
    Ok(())
}

fn storage(body: &GirBody) -> Result<(), Diagnostic> {
    let mut live = vec![false; body.locals.len()];
    let mut seen = vec![false; body.blocks.len()];
    walk_storage(body, body.entry, &mut live, &mut seen)
}

fn walk_storage(
    body: &GirBody,
    block: BlockId,
    live: &mut [bool],
    seen: &mut [bool],
) -> Result<(), Diagnostic> {
    if seen[block.index()] {
        return Ok(());
    }
    seen[block.index()] = true;
    for statement in body.block_statements(block) {
        match &statement.kind {
            StatementKind::StorageLive(local) => {
                if live[local.index()] {
                    return Err(gir_error("重复 StorageLive", None));
                }
                live[local.index()] = true;
            }
            StatementKind::StorageDead(local) => {
                if !live[local.index()] {
                    return Err(gir_error("未配对的 StorageDead", None));
                }
                live[local.index()] = false;
            }
            StatementKind::Assign(place, _) => {
                if !live[place.local.index()] {
                    return Err(gir_error("写入未激活 local", None));
                }
            }
            _ => {}
        }
    }
    let snapshot = live.to_vec();
    for successor in body.blocks[block.index()].terminator.successors() {
        live.copy_from_slice(&snapshot);
        walk_storage(body, successor, live, seen)?;
    }
    Ok(())
}

fn cleanup_sequences(module: &hir::Module, body: &GirBody) -> Result<(), Diagnostic> {
    let Some(owner) = module
        .owners
        .iter()
        .find(|owner| owner.definition == body.owner)
    else {
        return Err(gir_error("GIR body 没有对应 HIR owner", None));
    };
    for record in &body.exit_records {
        let Some(plan) = owner.cleanup_plans.get(record.plan as usize) else {
            return Err(gir_error("ExitRecord 引用未知 CleanupPlan", None));
        };
        let expected =
            &owner.cleanup_actions[plan.actions.start as usize..plan.actions.end as usize];
        let actual = reconstruct(body, record);
        if actual != expected {
            return Err(gir_error(
                "GIR cleanup 动作序列与 HIR CleanupPlan 不一致",
                None,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn reconstruct_for_test(body: &GirBody, record: &ExitRecord) -> Vec<CleanupAction> {
    reconstruct(body, record)
}

fn reconstruct(body: &GirBody, record: &ExitRecord) -> Vec<CleanupAction> {
    let mut regions: Vec<_> = body
        .cleanup_regions
        .iter()
        .filter(|region| region.chain == record.chain)
        .collect();
    regions.sort_by_key(|region| region.order);
    let mut actions = Vec::new();
    let mut current = Some(record.entry);
    while let Some(block) = current {
        if record.destination == Some(block) {
            break;
        }
        if matches!(
            body.blocks[block.index()].terminator,
            Terminator::Return | Terminator::ResumePanic
        ) {
            break;
        }
        if let Some(region) = regions.iter().find(|region| region.entry == block) {
            actions.push(region.action.clone());
            current = Some(region.exit);
            continue;
        }
        break;
    }
    actions
}

fn suspend_cancelled(body: &GirBody) -> Result<(), Diagnostic> {
    for block in &body.blocks {
        if let Terminator::SelectCommit {
            cases, cancelled, ..
        } = &block.terminator
        {
            let has_send = body.select_cases[cases.start as usize..cases.end as usize]
                .iter()
                .any(|case| matches!(case.operation, SelectOperation::Send { .. }));
            if has_send != cancelled.is_some() {
                return Err(gir_error(
                    "SelectCommit 的 cancelled 必须与 send case 一致",
                    None,
                ));
            }
        }
    }
    Ok(())
}

fn scoped_views(body: &GirBody) -> Result<(), Diagnostic> {
    let mut open: Vec<LocalId> = Vec::new();
    let mut seen = vec![false; body.blocks.len()];
    walk_views(body, body.entry, &mut open, &mut seen, false)
}

fn walk_views(
    body: &GirBody,
    block: BlockId,
    open: &mut Vec<LocalId>,
    seen: &mut [bool],
    in_cleanup: bool,
) -> Result<(), Diagnostic> {
    if seen[block.index()] {
        return Ok(());
    }
    seen[block.index()] = true;
    let start_len = open.len();
    for statement in body.block_statements(block) {
        match &statement.kind {
            StatementKind::ScopedViewBegin { token, source, .. } => {
                if body.projections_of(*source).iter().any(|projection| {
                    matches!(
                        projection,
                        Projection::Field {
                            access: Access::ScopedRead,
                            ..
                        }
                    ) && matches!(
                        statement.kind,
                        StatementKind::Assign(place, _) if place.local == source.local
                    )
                }) {
                    return Err(gir_error("ScopedRead 投影不能写入", None));
                }
                open.push(*token);
            }
            StatementKind::ScopedViewEnd { token } => {
                if open.pop() != Some(*token) {
                    return Err(gir_error("ScopedViewEnd 与 Begin 不成对", None));
                }
            }
            _ => {}
        }
    }
    match &body.blocks[block.index()].terminator {
        Terminator::Suspend { .. } if !open.is_empty() => {
            return Err(gir_error("ScopedView 内不能 suspend", None));
        }
        Terminator::Return | Terminator::ResumePanic | Terminator::Abort
            if !open.is_empty() && !in_cleanup =>
        {
            return Err(gir_error("ScopedView 在出口未闭合", None));
        }
        _ => {}
    }
    let snapshot = open.clone();
    for successor in body.blocks[block.index()].terminator.successors() {
        *open = snapshot.clone();
        walk_views(
            body,
            successor,
            open,
            seen,
            in_cleanup || body.blocks[successor.index()].cleanup,
        )?;
    }
    let _ = start_len;
    Ok(())
}

fn no_safepoint(body: &GirBody) -> Result<(), Diagnostic> {
    let mut open: Vec<NoSafepointRegionId> = Vec::new();
    let mut seen = BTreeSet::new();
    walk_regions(body, body.entry, &mut open, &mut seen)
}

fn walk_regions(
    body: &GirBody,
    block: BlockId,
    open: &mut Vec<NoSafepointRegionId>,
    seen: &mut BTreeSet<BlockId>,
) -> Result<(), Diagnostic> {
    if !seen.insert(block) {
        return Ok(());
    }
    for statement in body.block_statements(block) {
        match &statement.kind {
            StatementKind::NoSafepointBegin(id) => {
                if id.index() >= body.no_safepoint_regions.len() {
                    return Err(gir_error("NoSafepointRegion 越界", None));
                }
                open.push(*id);
            }
            StatementKind::NoSafepointEnd(id) => {
                if open.pop() != Some(*id) {
                    return Err(gir_error("NoSafepointEnd 与 Begin 不成对", None));
                }
            }
            StatementKind::SafepointPoll(_) if !open.is_empty() => {
                return Err(gir_error("NoSafepointRegion 内不能 poll", None));
            }
            _ => {}
        }
    }
    match &body.blocks[block.index()].terminator {
        Terminator::Call { call_kind, .. }
            if !open.is_empty() && !matches!(call_kind, CallKind::Managed) =>
        {
            return Err(gir_error("NoSafepointRegion 内不能外调", None));
        }
        Terminator::Suspend { .. } | Terminator::SelectCommit { .. } | Terminator::Panic { .. }
            if !open.is_empty() =>
        {
            return Err(gir_error("NoSafepointRegion 内不能 suspend/panic", None));
        }
        _ => {}
    }
    let snapshot = open.clone();
    for successor in body.blocks[block.index()].terminator.successors() {
        *open = snapshot.clone();
        walk_regions(body, successor, open, seen)?;
    }
    Ok(())
}
