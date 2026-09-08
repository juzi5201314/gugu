//! 在 generic GIR CFG 上做前向固定点，循环 header 使用 widening，收敛后再 narrowing。

use super::domain::{AbstractState, Interval, ValueKey};
use super::policy::AnalysisPolicyV1;
use super::transfer;
use super::transfer_gir::{self, CompareFact};
use super::types::{FunctionSummary, ProofFact, ProofStatus, ReturnRelation, RuntimeCheckKey};
use crate::frontend::gir::GirWorldV1;
use crate::frontend::gir::body::{BlockId, GirBody, Rvalue, StatementKind, Terminator};
use crate::frontend::hir::{self, ExprId, Module, Owner};
use crate::frontend::mono::instantiate::CallSite;

pub(crate) struct BodyResult {
    pub proofs: Vec<(ExprId, hir::CheckKind, ProofStatus)>,
    pub summary: FunctionSummary,
    pub budget_exhausted: bool,
}

pub(crate) fn analyze_owner(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    owner_index: u32,
    policy: AnalysisPolicyV1,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> BodyResult {
    let locals = owner.locals.len();
    let exprs = owner.expressions.len();
    let params = owner.parameters.len();
    let mut inbound: Vec<AbstractState> = (0..body.blocks.len())
        .map(|_| AbstractState::bottom(locals, exprs))
        .collect();
    inbound[body.entry.index()] = AbstractState::entry(locals, exprs, params);
    seed_param_lens(module, owner, &mut inbound[body.entry.index()]);
    let mut budget_exhausted = false;
    if !iterate(
        module,
        owner,
        body,
        &mut inbound,
        policy.max_block_iterations,
        callees,
    ) {
        budget_exhausted = true;
    }
    if !budget_exhausted {
        narrow(
            module,
            owner,
            body,
            &mut inbound,
            policy.max_block_iterations,
            callees,
        );
    }
    let proofs = collect_proofs(module, owner, owner_index, body, &inbound, callees);
    let mut summary = if budget_exhausted {
        FunctionSummary::conservative()
    } else {
        summarize(module, owner, body, &inbound, callees)
    };
    if !budget_exhausted && proofs.iter().any(|fact| fact.status != ProofStatus::Proved) {
        summary.may_panic = true;
    }
    BodyResult {
        proofs: proofs
            .into_iter()
            .map(|fact| (fact.key.expression, fact.key.kind, fact.status))
            .collect(),
        summary,
        budget_exhausted,
    }
}

pub(crate) fn body_of<'a>(module: &Module, gir: &'a GirWorldV1, owner_index: u32) -> &'a GirBody {
    let owner = &module.owners[usize::try_from(owner_index).expect("owner 编号适配宿主")];
    gir.bodies
        .iter()
        .find(|body| body.owner == owner.definition)
        .expect("每个 HIR owner 都有 generic GIR body")
}

fn seed_param_lens(module: &Module, owner: &Owner, state: &mut AbstractState) {
    for (index, local) in owner.locals.iter().enumerate() {
        let id = hir::LocalId(index as u32);
        if let Some(bounds) = transfer::integer_bounds(module, local.ty) {
            state.set_range(ValueKey::Local(id), bounds);
        }
        let mut ty = &module.types[local.ty.index()];
        if let hir::Type::Ref(inner) = ty {
            ty = &module.types[inner.index()];
        }
        if let hir::Type::Array(_, length) = ty {
            state.set_range(ValueKey::LocalLen(id), Interval::point(i128::from(*length)));
        }
    }
}

fn iterate(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    inbound: &mut [AbstractState],
    rounds: u32,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> bool {
    for _ in 0..rounds {
        let mut changed = false;
        for index in 0..body.blocks.len() {
            if !inbound[index].reachable && index != body.entry.index() {
                continue;
            }
            let mut state = inbound[index].clone();
            let compare = execute_block(
                module,
                owner,
                body,
                BlockId(index as u32),
                &mut state,
                callees,
            );
            changed |= propagate(
                module,
                owner,
                body,
                inbound,
                BlockId(index as u32),
                state,
                compare.as_ref(),
                callees,
                false,
            );
        }
        if !changed {
            return true;
        }
    }
    false
}

fn narrow(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    inbound: &mut Vec<AbstractState>,
    rounds: u32,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) {
    let entry = inbound[body.entry.index()].clone();
    for _ in 0..rounds {
        let mut next = vec![
            AbstractState::bottom(owner.locals.len(), owner.expressions.len());
            body.blocks.len()
        ];
        next[body.entry.index()] = entry.clone();
        for (index, old) in inbound.iter().enumerate() {
            if !old.reachable {
                continue;
            }
            let mut state = old.clone();
            let block = BlockId(u32::try_from(index).expect("GIR 块编号可表示"));
            let compare = execute_block(module, owner, body, block, &mut state, callees);
            propagate(
                module,
                owner,
                body,
                &mut next,
                block,
                state,
                compare.as_ref(),
                callees,
                true,
            );
        }
        if next == *inbound {
            break;
        }
        *inbound = next;
    }
}

fn execute_block(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    id: BlockId,
    state: &mut AbstractState,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> Option<CompareFact> {
    let block = &body.blocks[id.index()];
    let mut compare = None;
    for statement in
        &body.statements[block.statements.start as usize..block.statements.end as usize]
    {
        transfer_gir::execute_statement(
            module,
            owner,
            body,
            state,
            &statement.kind,
            callees,
            &mut compare,
        );
        if !state.reachable {
            return compare;
        }
    }
    compare
}

#[allow(clippy::too_many_arguments)]
fn propagate(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    inbound: &mut [AbstractState],
    id: BlockId,
    mut state: AbstractState,
    compare: Option<&CompareFact>,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    narrowing: bool,
) -> bool {
    let edges = transfer_gir::successor_states(
        module,
        owner,
        body,
        &mut state,
        &body.blocks[id.index()].terminator,
        compare,
        callees,
    );
    let mut changed = false;
    for (target, next) in edges {
        let backedge = target.0 <= id.0 && !narrowing;
        changed |= join_into(inbound, target, &next, backedge);
    }
    changed
}

fn join_into(
    inbound: &mut [AbstractState],
    target: BlockId,
    state: &AbstractState,
    widen: bool,
) -> bool {
    if widen {
        inbound[target.index()].widen(state)
    } else {
        inbound[target.index()].join(state)
    }
}

fn collect_proofs(
    module: &Module,
    owner: &Owner,
    owner_index: u32,
    body: &GirBody,
    inbound: &[AbstractState],
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> Vec<ProofFact> {
    use super::prove;
    let mut proofs = Vec::new();
    for (index, block) in body.blocks.iter().enumerate() {
        let mut state = inbound[index].clone();
        let mut compare = None;
        for statement in
            &body.statements[block.statements.start as usize..block.statements.end as usize]
        {
            if let StatementKind::Assign(_, Rvalue::CheckedOp { check, .. }) = &statement.kind {
                transfer_gir::sync_resolved(module, owner, &mut state);
                let check = &owner.checks[usize::try_from(*check).expect("检查编号适配宿主")];
                proofs.push(ProofFact {
                    key: RuntimeCheckKey {
                        owner_index,
                        expression: check.expression,
                        kind: check.kind.clone(),
                    },
                    status: prove::prove_check(module, owner, &state, check),
                });
            }
            transfer_gir::execute_statement(
                module,
                owner,
                body,
                &mut state,
                &statement.kind,
                callees,
                &mut compare,
            );
        }
    }
    proofs
}

fn summarize(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    inbound: &[AbstractState],
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> FunctionSummary {
    let mut summary = FunctionSummary::default();
    let mut returns = Interval::EMPTY;
    let mut relation = None;
    let mut saw_return = false;
    for (index, inbound_state) in inbound.iter().enumerate() {
        if !inbound_state.reachable && index != body.entry.index() {
            continue;
        }
        let mut state = inbound_state.clone();
        let _ = execute_block(
            module,
            owner,
            body,
            BlockId(index as u32),
            &mut state,
            callees,
        );
        absorb_effects(&mut summary, &state);
        if matches!(body.blocks[index].terminator, Terminator::Return) {
            returns = returns.join(state.range(ValueKey::Expr(owner.body)));
            let next = return_relation(owner, &state, owner.body);
            if !saw_return {
                relation = next;
            } else if relation != next {
                relation = None;
            }
            saw_return = true;
        }
    }
    if !owner.foreign_calls.is_empty() || !owner.assembly.is_empty() {
        summary.alias_foreign = true;
        summary.reads_hidden_state = true;
        summary.writes_hidden_state = true;
        summary.may_call_unknown = true;
    }
    if !returns.is_empty() {
        summary.return_lo = i64::try_from(returns.lo).ok();
        summary.return_hi = i64::try_from(returns.hi).ok();
    }
    summary.return_relations.extend(relation);
    super::access::summarize(module, owner, callees, &mut summary);
    summary
}

fn return_relation(
    owner: &Owner,
    state: &AbstractState,
    mut value: ExprId,
) -> Option<ReturnRelation> {
    while let hir::ExprKind::Block {
        tail: Some(tail), ..
    } = owner.expressions[value.index()].kind
    {
        value = tail;
    }
    let hir::ExprKind::Intrinsic {
        operation: hir::Builtin::Len,
        ref arguments,
        ..
    } = owner.expressions[value.index()].kind
    else {
        return None;
    };
    let receiver =
        owner.expression_ids[usize::try_from(arguments.start).expect("表达式列表编号可表示")];
    let local = transfer::place_local(owner, receiver)?;
    if state.heap_version != 0 || state.local_version[local.index()] != 0 {
        return None;
    }
    let parameter = owner.parameters.iter().position(|pattern| {
        matches!(
            owner.patterns[pattern.index()].kind,
            hir::PatternKind::Bind(bound) if bound == local
        )
    })?;
    Some(ReturnRelation::EqLen {
        parameter: u32::try_from(parameter).expect("参数编号可表示"),
    })
}

fn absorb_effects(summary: &mut FunctionSummary, state: &AbstractState) {
    summary.may_panic |= state.effects.panic;
    summary.may_allocate |= state.effects.allocate;
    summary.may_suspend |= state.effects.suspend;
    summary.alias_foreign |= state.effects.foreign;
    summary.may_mutate_len |= state.effects.resource_publish;
    summary.writes_hidden_state |= state.effects.cow_seal;
    summary.alias_heap |= state.effects.alias_heap;
    summary.may_call_unknown |= state.effects.call_unknown;
    summary.reads_hidden_state |= state.effects.reads_hidden;
    summary.writes_hidden_state |= state.effects.writes_hidden;
}
