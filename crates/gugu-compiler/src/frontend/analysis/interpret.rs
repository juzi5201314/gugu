//! 在 CFG 上做前向固定点，循环 header 使用 widening，收敛后再 narrowing。

use super::cfg::{self, BlockId, Cfg, Inst, Terminator};
use super::domain::AbstractState;
use super::policy::AnalysisPolicyV1;
use super::transfer;
use super::types::{FunctionSummary, ProofFact, ProofStatus, RuntimeCheckKey};
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
    let mut proofs = Vec::new();
    if !iterate(
        module,
        owner,
        &cfg,
        &mut inbound,
        policy.max_block_iterations,
        false,
        callees,
        &mut proofs,
        &mut budget_exhausted,
    ) {
        budget_exhausted = true;
    }
    if !budget_exhausted {
        let _ = iterate(
            module,
            owner,
            &cfg,
            &mut inbound,
            1,
            true,
            callees,
            &mut proofs,
            &mut budget_exhausted,
        );
    }
    proofs = collect_proofs(module, owner, owner_index, &cfg, &inbound, callees);
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
    narrowing: bool,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    proofs: &mut Vec<ProofFact>,
    budget_exhausted: &mut bool,
) -> bool {
    let _ = proofs;
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
            changed |= propagate(owner, cfg, inbound, BlockId(index as u32), state, narrowing);
        }
        if !changed {
            return true;
        }
    }
    *budget_exhausted = true;
    false
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
        match inst {
            Inst::Eval(expr) => transfer::eval_expr(module, owner, state, *expr, callees),
            Inst::Bind { local, value } => transfer::bind(state, *local, *value),
            Inst::Assign { place, value } => transfer::assign(module, owner, state, *place, *value),
            Inst::Increment(local) => transfer::increment(state, *local),
            Inst::Dispatch(dispatch) => transfer::apply_dispatch(owner, state, *dispatch, callees),
            Inst::Initialize(statement) => transfer::apply_initializer(state, *statement, callees),
            Inst::Yield => {
                state.effects.suspend = true;
                state.bump_heap();
            }
        }
        if !state.reachable {
            return;
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
        Terminator::Return | Terminator::Unreachable => false,
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
                transfer::eval_expr(module, owner, &mut state, expr, callees);
            } else {
                match inst {
                    Inst::Bind { local, value } => transfer::bind(&mut state, *local, *value),
                    Inst::Assign { place, value } => {
                        transfer::assign(module, owner, &mut state, *place, *value)
                    }
                    Inst::Increment(local) => transfer::increment(&mut state, *local),
                    Inst::Dispatch(dispatch) => {
                        transfer::apply_dispatch(owner, &mut state, *dispatch, callees)
                    }
                    Inst::Initialize(statement) => {
                        transfer::apply_initializer(&mut state, *statement, callees)
                    }
                    Inst::Yield => {
                        state.effects.suspend = true;
                        state.bump_heap();
                    }
                    Inst::Eval(_) => {}
                }
            }
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
    }
    if !owner.foreign_calls.is_empty() || !owner.assembly.is_empty() {
        summary.alias_foreign = true;
        summary.reads_hidden_state = true;
        summary.writes_hidden_state = true;
        summary.may_call_unknown = true;
    }
    super::access::summarize(module, owner, callees, &mut summary);
    if !owner.checks.is_empty() {
        summary.may_panic = true;
    }
    summary
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
