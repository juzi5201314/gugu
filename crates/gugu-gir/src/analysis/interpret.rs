//! 在 generic GIR CFG 上做前向固定点，循环 header 使用 widening，收敛后再 narrowing。

use super::domain::{AbstractState, Interval, ValueKey};
use super::policy::AnalysisPolicyV1;
use super::transfer;
use super::transfer_gir::{self, CompareFact, IndexedBody};
use super::types::{FunctionSummary, ProofFact, ProofStatus, ReturnRelation, RuntimeCheckKey};
use crate::gir::GirWorldV1;
use crate::gir::body::{BlockId, GirBody, Rvalue, StatementKind, Terminator};
use crate::gir::passing::PassingTable;
use crate::hir::{self, ExprId, Module, Owner};
use crate::mono::instantiate::CallSite;
#[derive(Debug, Eq, PartialEq)]
pub struct BodyResult {
    pub proofs: Vec<(ExprId, hir::CheckKind, ProofStatus)>,
    pub summary: FunctionSummary,
    pub budget_exhausted: bool,
}

pub fn analyze_owner(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    owner_index: u32,
    policy: AnalysisPolicyV1,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    passing: &PassingTable,
) -> BodyResult {
    if policy.max_block_iterations != 0
        && let Some(result) = analyze_linear_leaf(module, owner, body, passing, callees)
    {
        return result;
    }
    analyze_generic(module, owner, body, owner_index, policy, callees, passing)
}

pub(super) fn analyze_generic(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    owner_index: u32,
    policy: AnalysisPolicyV1,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    passing: &PassingTable,
) -> BodyResult {
    let indexed = IndexedBody::new(body, owner);
    let body = &indexed;
    let locals = owner.locals.len();
    let exprs = owner.expressions.len();
    let params = owner.parameters.len();
    // 不可达状态不读事实数组；首次 join/widen 从真实入边复制完整状态。
    let mut inbound: Vec<AbstractState> = (0..body.blocks.len())
        .map(|_| AbstractState::bottom(0, 0))
        .collect();
    inbound[body.entry.index()] = AbstractState::entry(locals, exprs, params);
    seed_param_lens(module, owner, &mut inbound[body.entry.index()]);
    let mut budget_exhausted = false;
    let mut widened = false;
    if !iterate(
        module,
        owner,
        body,
        &mut inbound,
        policy.max_block_iterations,
        passing,
        callees,
        &mut widened,
    ) {
        budget_exhausted = true;
    }
    // 只有在前向固定点实际触发过回边 widening 时，narrowing 才可能收紧精度；无 widening 时
    // `inbound` 已是精确不动点，跳过 narrowing 与逐状态对比结果完全一致。
    if !budget_exhausted && widened {
        narrow(
            module,
            owner,
            body,
            &mut inbound,
            policy.max_block_iterations,
            passing,
            callees,
        );
    }
    let proofs = collect_proofs(module, owner, owner_index, body, &inbound, passing, callees);
    let mut summary = if budget_exhausted {
        FunctionSummary::conservative()
    } else {
        summarize(module, owner, body, &inbound, passing, callees)
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

pub(super) fn analyze_linear_leaf(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    passing: &PassingTable,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> Option<BodyResult> {
    if body.entry.index() != 0
        || body.blocks.is_empty()
        || body.blocks.iter().enumerate().any(|(index, block)| {
            let linear = match block.terminator {
                Terminator::Goto { target } => target.index() == index + 1,
                Terminator::Return => index + 1 == body.blocks.len(),
                _ => false,
            };
            !linear
                || block.cleanup
                || (index == 0 && block.statements.start != 0)
                || (index > 0 && body.blocks[index - 1].statements.end != block.statements.start)
        })
        || usize::try_from(body.blocks.last()?.statements.end).expect("语句编号适配宿主")
            != body.statements.len()
        || body
            .locals
            .iter()
            .any(|local| !passing.class(local.ty).is_pure_bits())
        || !owner.foreign_calls.is_empty()
        || !owner.assembly.is_empty()
        || !body.safepoints.is_empty()
        || !body.no_safepoint_regions.is_empty()
        || body.statements.iter().any(|statement| {
            !matches!(
                statement.kind,
                StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::ValueAction {
                        action: crate::gir::body::ValueActionKind::Copy,
                        ..
                    }
                    | StatementKind::Nop
                    | StatementKind::Assign(
                        _,
                        Rvalue::Use(_)
                            | Rvalue::ValueCopy(_)
                            | Rvalue::UnaryOp { .. }
                            | Rvalue::BinaryOp { .. }
                            | Rvalue::Compare { .. }
                            | Rvalue::Cast { .. }
                            | Rvalue::CheckedOp { .. }
                    )
            )
        })
    {
        return None;
    }
    let indexed = IndexedBody::new(body, owner);
    let mut state = AbstractState::entry(
        owner.locals.len(),
        owner.expressions.len(),
        owner.parameters.len(),
    );
    seed_param_lens(module, owner, &mut state);
    let mut proofs = Vec::new();
    let mut compare = None;
    for statement in &body.statements {
        if let StatementKind::Assign(_, Rvalue::CheckedOp { check, .. }) = &statement.kind {
            transfer_gir::sync_resolved(module, owner, &indexed, &mut state);
            let check = &owner.checks[usize::try_from(*check).expect("检查编号适配宿主")];
            proofs.push((
                check.expression,
                check.kind.clone(),
                super::prove::prove_check(module, owner, &state, check),
            ));
        }
        transfer_gir::execute_statement(
            module,
            owner,
            &indexed,
            &mut state,
            &statement.kind,
            passing,
            callees,
            &mut compare,
        );
    }
    transfer_gir::sync_resolved(module, owner, &indexed, &mut state);
    let mut summary = FunctionSummary::default();
    absorb_effects(&mut summary, &state);
    let returns = state.range(ValueKey::Expr(owner.body));
    if !returns.is_empty() {
        summary.return_lo = i64::try_from(returns.lo).ok();
        summary.return_hi = i64::try_from(returns.hi).ok();
    }
    summary
        .return_relations
        .extend(return_relation(owner, &state, owner.body));
    super::access::summarize(module, owner, callees, &mut summary);
    summary.may_panic |= proofs
        .iter()
        .any(|(_, _, status)| *status != ProofStatus::Proved);
    Some(BodyResult {
        proofs,
        summary,
        budget_exhausted: false,
    })
}

pub fn body_of<'a>(module: &Module, gir: &'a GirWorldV1, owner_index: u32) -> &'a GirBody {
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
        if let hir::Type::Array(_, length) = ty
            && let Some(length) = length.known()
        {
            state.set_range(ValueKey::LocalLen(id), Interval::point(i128::from(length)));
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "固定点迭代需要 module/owner/body/inbound/轮次/传递表/callee 摘要与 widening 标记"
)]
fn iterate(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
    inbound: &mut [AbstractState],
    rounds: u32,
    passing: &PassingTable,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    widened: &mut bool,
) -> bool {
    // 块编号是稠密下标；只有入状态改变后，原传播结果才可能改变。
    let mut dirty = vec![false; body.blocks.len()];
    dirty[body.entry.index()] = true;
    let mut state = AbstractState::bottom(0, 0);
    let mut branch = AbstractState::bottom(0, 0);
    for _ in 0..rounds {
        let mut changed = false;
        for index in 0..body.blocks.len() {
            if !dirty[index] || (!inbound[index].reachable && index != body.entry.index()) {
                continue;
            }
            dirty[index] = false;
            state.copy_from(&inbound[index]);
            let compare = execute_block(
                module,
                owner,
                body,
                BlockId(index as u32),
                &mut state,
                passing,
                callees,
            );
            changed |= propagate(
                module,
                owner,
                body,
                inbound,
                BlockId(index as u32),
                &mut state,
                &mut branch,
                compare.as_ref(),
                callees,
                false,
                Some(&mut dirty),
                Some(widened),
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
    body: &IndexedBody<'_>,
    inbound: &mut Vec<AbstractState>,
    rounds: u32,
    passing: &PassingTable,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) {
    let entry = inbound[body.entry.index()].clone();
    let mut next = vec![AbstractState::bottom(0, 0); body.blocks.len()];
    let mut state = AbstractState::bottom(0, 0);
    let mut branch = AbstractState::bottom(0, 0);
    for _ in 0..rounds {
        for slot in &mut next {
            slot.reset_unreachable();
        }
        next[body.entry.index()].copy_from(&entry);
        for (index, old) in inbound.iter().enumerate() {
            if !old.reachable {
                continue;
            }
            state.copy_from(old);
            let block = BlockId(u32::try_from(index).expect("GIR 块编号可表示"));
            let compare = execute_block(module, owner, body, block, &mut state, passing, callees);
            propagate(
                module,
                owner,
                body,
                &mut next,
                block,
                &mut state,
                &mut branch,
                compare.as_ref(),
                callees,
                true,
                None,
                None,
            );
        }
        if next
            .iter()
            .zip(inbound.iter())
            .all(|(left, right)| left.semantic_eq(right))
        {
            break;
        }
        std::mem::swap(inbound, &mut next);
    }
}

fn execute_block(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
    id: BlockId,
    state: &mut AbstractState,
    passing: &PassingTable,
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
            passing,
            callees,
            &mut compare,
        );
        if !state.reachable {
            return compare;
        }
    }
    compare
}

#[expect(
    clippy::too_many_arguments,
    reason = "块传播需要 module/owner/body/inbound/块号/状态缓冲/比较事实/摘要查询"
)]
fn propagate(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
    inbound: &mut [AbstractState],
    id: BlockId,
    state: &mut AbstractState,
    branch: &mut AbstractState,
    compare: Option<&CompareFact>,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    narrowing: bool,
    mut dirty: Option<&mut [bool]>,
    mut widened: Option<&mut bool>,
) -> bool {
    let mut changed = false;
    transfer_gir::for_each_successor(
        module,
        owner,
        body,
        state,
        branch,
        &body.blocks[id.index()].terminator,
        compare,
        callees,
        |target, next| {
            let backedge = target.0 <= id.0 && !narrowing;
            let mut did_widen = false;
            if join_into(inbound, target, next, backedge, &mut did_widen) {
                changed = true;
                if let Some(dirty) = dirty.as_deref_mut() {
                    dirty[target.index()] = true;
                }
            }
            if did_widen && let Some(flag) = widened.as_deref_mut() {
                *flag = true;
            }
        },
    );
    changed
}

fn join_into(
    inbound: &mut [AbstractState],
    target: BlockId,
    state: &AbstractState,
    widen: bool,
    widened_bound: &mut bool,
) -> bool {
    if widen {
        inbound[target.index()].widen_tracked(state, widened_bound)
    } else {
        inbound[target.index()].join(state)
    }
}

fn collect_proofs(
    module: &Module,
    owner: &Owner,
    owner_index: u32,
    body: &IndexedBody<'_>,
    inbound: &[AbstractState],
    passing: &PassingTable,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> Vec<ProofFact> {
    use super::prove;
    let mut proofs = Vec::new();
    let mut state = AbstractState::bottom(0, 0);
    for (index, block) in body.blocks.iter().enumerate() {
        let statements =
            &body.statements[block.statements.start as usize..block.statements.end as usize];
        let Some(last_check) = statements.iter().rposition(|statement| {
            matches!(
                statement.kind,
                StatementKind::Assign(_, Rvalue::CheckedOp { .. })
            )
        }) else {
            continue;
        };
        state.copy_from(&inbound[index]);
        let mut compare = None;
        // inbound 已由不动点求得；末个检查后的转移不能影响本块任何证明。
        for statement in &statements[..=last_check] {
            if let StatementKind::Assign(_, Rvalue::CheckedOp { check, .. }) = &statement.kind {
                transfer_gir::sync_resolved(module, owner, body, &mut state);
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
            if !state.reachable {
                continue;
            }
            transfer_gir::execute_statement(
                module,
                owner,
                body,
                &mut state,
                &statement.kind,
                passing,
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
    body: &IndexedBody<'_>,
    inbound: &[AbstractState],
    passing: &PassingTable,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> FunctionSummary {
    let mut summary = FunctionSummary::default();
    let mut returns = Interval::EMPTY;
    let mut relation = None;
    let mut saw_return = false;
    let mut state = AbstractState::bottom(0, 0);
    for (index, inbound_state) in inbound.iter().enumerate() {
        if !inbound_state.reachable && index != body.entry.index() {
            continue;
        }
        state.copy_from(inbound_state);
        let _ = execute_block(
            module,
            owner,
            body,
            BlockId(index as u32),
            &mut state,
            passing,
            callees,
        );
        absorb_effects(&mut summary, &state);
        if matches!(body.blocks[index].terminator, Terminator::Return) {
            transfer_gir::sync_resolved(module, owner, body, &mut state);
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
