//! 在 CFG 上做前向固定点，循环 header 使用 widening，收敛后再 narrowing。

use super::cfg::{self, BlockId, Cfg, Inst, Terminator};
use super::domain::{AbstractState, Interval, ValueKey};
use super::policy::AnalysisPolicyV1;
use super::transfer;
use super::types::{FunctionSummary, ProofFact, ProofStatus, ReturnRelation, RuntimeCheckKey};
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
    owner_index: u32,
    policy: AnalysisPolicyV1,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> BodyResult {
    let cfg = cfg::build(owner);
    let locals = owner.locals.len();
    let exprs = owner.expressions.len();
    let params = owner.parameters.len();
    let mut inbound: Vec<AbstractState> = (0..cfg.blocks.len())
        .map(|_| AbstractState::bottom(locals, exprs))
        .collect();
    inbound[cfg.entry.index()] = AbstractState::entry(locals, exprs, params);
    seed_param_lens(module, owner, &mut inbound[cfg.entry.index()]);
    let mut budget_exhausted = false;
    if !iterate(
        module,
        owner,
        &cfg,
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
            &cfg,
            &mut inbound,
            policy.max_block_iterations,
            callees,
        );
    }
    let proofs = collect_proofs(module, owner, owner_index, &cfg, &inbound, callees);
    let summary = if budget_exhausted {
        FunctionSummary::conservative()
    } else {
        summarize(module, owner, &cfg, &inbound, callees)
    };
    BodyResult {
        proofs: proofs
            .into_iter()
            .map(|fact| (fact.key.expression, fact.key.kind, fact.status))
            .collect(),
        summary,
        budget_exhausted,
    }
}

fn seed_param_lens(module: &Module, owner: &Owner, state: &mut AbstractState) {
    for (index, local) in owner.locals.iter().enumerate() {
        let mut ty = &module.types[local.ty.index()];
        if let hir::Type::Ref(inner) = ty {
            ty = &module.types[inner.index()];
        }
        if let hir::Type::Array(_, length) = ty {
            state.set_range(
                super::domain::ValueKey::LocalLen(hir::LocalId(index as u32)),
                super::domain::Interval::point(i128::from(*length)),
            );
        }
    }
}

fn iterate(
    module: &Module,
    owner: &Owner,
    cfg: &Cfg,
    inbound: &mut [AbstractState],
    rounds: u32,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> bool {
    for _ in 0..rounds {
        let mut changed = false;
        for index in 0..cfg.blocks.len() {
            if !inbound[index].reachable && index != cfg.entry.index() {
                continue;
            }
            let mut state = inbound[index].clone();
            execute_block(
                module,
                owner,
                cfg,
                BlockId(index as u32),
                &mut state,
                callees,
            );
            changed |= propagate(owner, cfg, inbound, BlockId(index as u32), state, false);
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
    cfg: &Cfg,
    inbound: &mut Vec<AbstractState>,
    rounds: u32,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) {
    let entry = inbound[cfg.entry.index()].clone();
    for _ in 0..rounds {
        // 对已收敛的上界同步应用 F，重新合并全部前驱；F(S) 仍是健全的后固定点。
        let mut next = vec![
            AbstractState::bottom(owner.locals.len(), owner.expressions.len());
            cfg.blocks.len()
        ];
        next[cfg.entry.index()] = entry.clone();
        for (index, old) in inbound.iter().enumerate() {
            if !old.reachable {
                continue;
            }
            let mut state = old.clone();
            let block = BlockId(u32::try_from(index).expect("CFG 块编号可表示"));
            execute_block(module, owner, cfg, block, &mut state, callees);
            propagate(owner, cfg, &mut next, block, state, true);
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
    cfg: &Cfg,
    id: BlockId,
    state: &mut AbstractState,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) {
    let block = &cfg.blocks[id.index()];
    for inst in &block.instructions {
        execute_inst(module, owner, state, inst, callees);
        if !state.reachable {
            return;
        }
    }
}

fn execute_inst(
    module: &Module,
    owner: &Owner,
    state: &mut AbstractState,
    inst: &Inst,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) {
    match inst {
        Inst::Eval(expr) => transfer::eval_expr(module, owner, state, *expr, callees),
        Inst::Bind { local, value } => transfer::bind(state, *local, *value),
        Inst::Assign {
            place,
            value,
            operation,
        } => transfer::assign(module, owner, state, *place, *value, *operation),
        Inst::Increment(local) => transfer::increment(state, *local),
        Inst::Dispatch(dispatch) => transfer::apply_dispatch(owner, state, *dispatch, callees),
        Inst::Initialize(statement) => transfer::apply_initializer(state, *statement, callees),
        Inst::Yield => {
            state.effects.suspend = true;
            state.bump_heap();
        }
    }
}

fn propagate(
    owner: &Owner,
    cfg: &Cfg,
    inbound: &mut [AbstractState],
    id: BlockId,
    state: AbstractState,
    narrowing: bool,
) -> bool {
    let Some(terminator) = cfg.blocks[id.index()].terminator.as_ref() else {
        return false;
    };
    match terminator {
        Terminator::Goto { target, backedge } => {
            join_into(inbound, *target, &state, *backedge && !narrowing)
        }
        Terminator::If {
            cond,
            then_block,
            else_block,
        } => {
            let mut then_state = state.clone();
            let mut else_state = state;
            transfer::assume(&mut then_state, *cond, true, owner);
            transfer::assume(&mut else_state, *cond, false, owner);
            let mut changed = join_into(inbound, *then_block, &then_state, false);
            changed |= join_into(inbound, *else_block, &else_state, false);
            changed
        }
        Terminator::Iv {
            local,
            end,
            body,
            exit,
        } => {
            let mut body_state = state.clone();
            let mut exit_state = state;
            transfer::assume_iv(&mut body_state, *local, *end, true);
            transfer::assume_iv(&mut exit_state, *local, *end, false);
            let mut changed = join_into(inbound, *body, &body_state, false);
            changed |= join_into(inbound, *exit, &exit_state, false);
            changed
        }
        Terminator::Switch { arms, .. } => {
            let mut changed = false;
            for &arm in arms {
                changed |= join_into(inbound, arm, &state, false);
            }
            changed
        }
        Terminator::Return(_) | Terminator::Unreachable => false,
    }
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
    cfg: &Cfg,
    inbound: &[AbstractState],
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> Vec<ProofFact> {
    use super::prove;
    let mut proofs = Vec::new();
    for (index, block) in cfg.blocks.iter().enumerate() {
        let mut state = inbound[index].clone();
        for inst in &block.instructions {
            if let Inst::Eval(expr) = *inst {
                for check in owner.checks.iter().filter(|check| check.expression == expr) {
                    let status = prove::prove_check(module, owner, &state, check);
                    proofs.push(ProofFact {
                        key: RuntimeCheckKey {
                            owner_index,
                            expression: expr,
                            kind: check.kind.clone(),
                        },
                        status,
                    });
                }
            }
            execute_inst(module, owner, &mut state, inst, callees);
        }
    }
    proofs
}

fn summarize(
    module: &Module,
    owner: &Owner,
    cfg: &Cfg,
    inbound: &[AbstractState],
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> FunctionSummary {
    let mut summary = FunctionSummary::default();
    let mut returns = Interval::EMPTY;
    let mut relation = None;
    let mut saw_return = false;
    for (index, inbound_state) in inbound.iter().enumerate() {
        if !inbound_state.reachable && index != cfg.entry.index() {
            continue;
        }
        let mut state = inbound_state.clone();
        execute_block(
            module,
            owner,
            cfg,
            BlockId(index as u32),
            &mut state,
            callees,
        );
        absorb_effects(&mut summary, &state);
        if let Some(Terminator::Return(value)) = cfg.blocks[index].terminator {
            returns = returns.join(value.map_or(Interval::UNKNOWN, |value| {
                state.range(ValueKey::Expr(value))
            }));
            let next = value.and_then(|value| return_relation(owner, &state, value));
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
    if !owner.checks.is_empty() {
        summary.may_panic = true;
    }
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
    let parameter = owner.parameters.iter().position(|pattern| matches!(owner.patterns[pattern.index()].kind, hir::PatternKind::Bind(bound) if bound == local))?;
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
