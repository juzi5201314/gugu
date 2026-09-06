//! 从 HIR 构建 CFG、过程内传播与 SCC 固定点。

use super::policy::AnalysisPolicyV1;
use super::types::{
    AnalysisOwnerKey, AnalysisWorldV1, FunctionSummary, IntRange, ProofFact, ProofStatus,
    RuntimeCheckKey, WORLD_SCHEMA_VERSION, sort_proofs,
};
use crate::frontend::hir::{
    self, CallTarget, CheckKind, DefinitionKind, ExprId, ExprKind, Literal, Module, Owner,
    RuntimeCheck, StatementKind,
};
use crate::frontend::semantics::comptime::EarlyConstTable;
use crate::frontend::semantics::model::Model;
use crate::frontend::semantics::{CheckKind as SemCheckKind, CheckedBody, CheckedSemantics};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CfgBlockId(u32);

#[derive(Clone, Debug)]
struct CfgEdge {
    to: CfgBlockId,
    branch: Branch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Branch {
    Unconditional,
    True,
    False,
    Back,
}

#[derive(Clone, Debug, Default)]
struct AbstractState {
    reachable: bool,
    expr_ranges: BTreeMap<u32, IntRange>,
    loop_upper: BTreeMap<u32, i128>,
    len_lower: BTreeMap<u32, i128>,
    heap_version: u32,
}

#[derive(Clone, Debug)]
struct OwnerCfg {
    blocks: Vec<Vec<CfgEdge>>,
    block_states: Vec<AbstractState>,
    entry: CfgBlockId,
}

pub(crate) fn analyze(
    module: &Module,
    checked: &CheckedSemantics,
    early: &EarlyConstTable,
    model: &Model<'_>,
    policy: AnalysisPolicyV1,
) -> AnalysisWorldV1 {
    let mut owner_cfgs = Vec::new();
    let mut keys = Vec::new();
    for (index, owner) in module.owners.iter().enumerate() {
        let kind = module.definitions[owner.definition.index()].kind;
        if !matches!(
            kind,
            DefinitionKind::Function | DefinitionKind::Closure | DefinitionKind::Async
        ) {
            continue;
        }
        let key = AnalysisOwnerKey {
            owner_index: u32::try_from(index).expect("owner index"),
            definition: owner.definition,
        };
        keys.push(key);
        owner_cfgs.push(build_cfg(owner, policy));
    }

    let def_to_node: BTreeMap<hir::DefId, usize> = keys
        .iter()
        .enumerate()
        .map(|(node, key)| (key.definition, node))
        .collect();
    let call_graph = build_call_graph(module, &keys, &def_to_node);
    let scc = tarjan(&call_graph, keys.len());
    let mut summaries: BTreeMap<u32, FunctionSummary> = keys
        .iter()
        .map(|key| (key.owner_index, FunctionSummary::conservative()))
        .collect();

    let mut budget_exhausted = false;
    for round in 0..policy.max_scc_iterations {
        let mut changed = false;
        for component in &scc {
            for &node in component {
                let owner_index = keys[node].owner_index as usize;
                let owner = &module.owners[owner_index];
                let cfg = &mut owner_cfgs[node];
                let summary_before = summaries[&keys[node].owner_index].clone();
                run_intraprocedural(owner, cfg, early, model, checked, owner_index);
                let summary_after = summarize_effects(cfg);
                if summary_after != summary_before {
                    changed = true;
                    summaries.insert(keys[node].owner_index, summary_after);
                }
            }
        }
        if !changed {
            break;
        }
        if round + 1 == policy.max_scc_iterations {
            budget_exhausted = true;
        }
    }

    let mut proofs = Vec::new();
    let mut elided = 0u32;
    for (node, key) in keys.iter().enumerate() {
        let owner_index = key.owner_index as usize;
        let owner = &module.owners[owner_index];
        let cfg = &owner_cfgs[node];
        for check in &owner.checks {
            let status = prove_check(owner, cfg, check, model, checked, owner_index);
            if status == ProofStatus::Proved {
                elided += 1;
            }
            proofs.push(ProofFact {
                key: RuntimeCheckKey {
                    owner_index: key.owner_index,
                    expression: check.expression,
                    kind: check.kind.clone(),
                },
                status,
            });
        }
    }
    sort_proofs(&mut proofs);

    let owners = keys
        .into_iter()
        .map(|key| super::types::OwnerSummaryRecord {
            key,
            summary: summaries
                .get(&key.owner_index)
                .cloned()
                .unwrap_or_else(FunctionSummary::conservative),
        })
        .collect();

    AnalysisWorldV1 {
        schema: WORLD_SCHEMA_VERSION,
        input_fingerprint: [0; 32],
        owners,
        proofs,
        budget_exhausted,
        runtime_checks_elided_count: elided,
    }
}

pub(crate) fn patch_proofs(module: &mut Module, world: &AnalysisWorldV1) {
    for (owner_index, owner) in module.owners.iter_mut().enumerate() {
        let owner_index = u32::try_from(owner_index).expect("owner index");
        for check in &mut owner.checks {
            let key = RuntimeCheckKey {
                owner_index,
                expression: check.expression,
                kind: check.kind.clone(),
            };
            check.proof = Some(world.proof_status(&key));
        }
    }
}

fn build_cfg(owner: &Owner, policy: AnalysisPolicyV1) -> OwnerCfg {
    let mut blocks: Vec<Vec<CfgEdge>> = vec![Vec::new()];
    let mut block_states = vec![AbstractState {
        reachable: true,
        ..AbstractState::default()
    }];
    let entry = CfgBlockId(0);
    let max_blocks = policy.max_blocks_per_owner as usize;

    fn fresh_block(blocks: &mut Vec<Vec<CfgEdge>>, states: &mut Vec<AbstractState>) -> CfgBlockId {
        blocks.push(Vec::new());
        states.push(AbstractState::default());
        CfgBlockId(u32::try_from(blocks.len() - 1).expect("block id"))
    }

    fn wire(blocks: &mut [Vec<CfgEdge>], from: CfgBlockId, to: CfgBlockId, branch: Branch) {
        blocks[from.0 as usize].push(CfgEdge { to, branch });
    }

    fn lower_expr(
        owner: &Owner,
        expr: ExprId,
        blocks: &mut Vec<Vec<CfgEdge>>,
        states: &mut Vec<AbstractState>,
        current: CfgBlockId,
        max_blocks: usize,
    ) -> CfgBlockId {
        if blocks.len() >= max_blocks {
            return current;
        }
        let expression = &owner.expressions[expr.index()];
        match &expression.kind {
            ExprKind::Block { statements, tail } => {
                let mut cursor = current;
                for stmt in &owner.statements[statements.start as usize..statements.end as usize] {
                    cursor = lower_stmt(owner, stmt, blocks, states, cursor, max_blocks);
                }
                if let Some(tail) = tail {
                    cursor = lower_expr(owner, *tail, blocks, states, cursor, max_blocks);
                }
                cursor
            }
            ExprKind::If {
                condition: _,
                then_value,
                else_value,
            } => {
                let then_block = fresh_block(blocks, states);
                let merge = fresh_block(blocks, states);
                wire(blocks, current, then_block, Branch::True);
                let then_exit =
                    lower_expr(owner, *then_value, blocks, states, then_block, max_blocks);
                wire(blocks, then_exit, merge, Branch::Unconditional);
                if let Some(else_value) = else_value {
                    let else_block = fresh_block(blocks, states);
                    wire(blocks, current, else_block, Branch::False);
                    let else_exit =
                        lower_expr(owner, *else_value, blocks, states, else_block, max_blocks);
                    wire(blocks, else_exit, merge, Branch::Unconditional);
                } else {
                    wire(blocks, current, merge, Branch::False);
                }
                merge
            }
            ExprKind::Loop { body } => {
                let header = fresh_block(blocks, states);
                wire(blocks, current, header, Branch::Unconditional);
                let body_exit = lower_expr(owner, *body, blocks, states, header, max_blocks);
                wire(blocks, body_exit, header, Branch::Back);
                let exit = fresh_block(blocks, states);
                wire(blocks, header, exit, Branch::False);
                exit
            }
            ExprKind::For { value: _, body, .. } => {
                let header = fresh_block(blocks, states);
                wire(blocks, current, header, Branch::Unconditional);
                let body_exit = lower_expr(owner, *body, blocks, states, header, max_blocks);
                wire(blocks, body_exit, header, Branch::Back);
                let exit = fresh_block(blocks, states);
                wire(blocks, header, exit, Branch::False);
                exit
            }
            _ => current,
        }
    }

    fn lower_stmt(
        owner: &Owner,
        stmt: &hir::Statement,
        blocks: &mut Vec<Vec<CfgEdge>>,
        states: &mut Vec<AbstractState>,
        current: CfgBlockId,
        max_blocks: usize,
    ) -> CfgBlockId {
        match &stmt.kind {
            StatementKind::Expression(value) => {
                lower_expr(owner, *value, blocks, states, current, max_blocks)
            }
            _ => current,
        }
    }

    let _exit = lower_expr(
        owner,
        owner.body,
        &mut blocks,
        &mut block_states,
        entry,
        max_blocks,
    );
    OwnerCfg {
        blocks,
        block_states,
        entry,
    }
}

fn run_intraprocedural(
    owner: &Owner,
    cfg: &mut OwnerCfg,
    early: &EarlyConstTable,
    model: &Model<'_>,
    checked: &CheckedSemantics,
    owner_index: usize,
) {
    let mut work: BTreeSet<u32> = (0..cfg.blocks.len() as u32).collect();
    while let Some(block_id) = work.pop_first() {
        let edges = cfg.blocks[block_id as usize].clone();
        let mut state = cfg.block_states[block_id as usize].clone();
        if !state.reachable {
            continue;
        }
        simulate_owner_body(owner, &mut state, early, model, checked, owner_index);
        for edge in edges {
            let next = &mut cfg.block_states[edge.to.0 as usize];
            if merge_state(next, &state) {
                work.insert(edge.to.0);
            }
        }
    }
}

fn simulate_owner_body(
    owner: &Owner,
    state: &mut AbstractState,
    early: &EarlyConstTable,
    model: &Model<'_>,
    checked: &CheckedSemantics,
    owner_index: usize,
) {
    let _ = (checked, owner_index);
    seed_constants(owner, state, early, model);
    walk_expr(owner, owner.body, state, early, model);
}

fn seed_constants(
    owner: &Owner,
    state: &mut AbstractState,
    _early: &EarlyConstTable,
    _model: &Model<'_>,
) {
    for (index, expression) in owner.expressions.iter().enumerate() {
        if let ExprKind::Literal(Literal::Integer(value)) = &expression.kind {
            let signed = i128::try_from(*value).unwrap_or(i128::MAX);
            state
                .expr_ranges
                .insert(u32::try_from(index).expect("expr"), IntRange::point(signed));
        }
    }
}

fn walk_expr(
    owner: &Owner,
    expr: ExprId,
    state: &mut AbstractState,
    early: &EarlyConstTable,
    model: &Model<'_>,
) {
    let index = expr.index();
    let kind = owner.expressions[index].kind.clone();
    match kind {
        ExprKind::Literal(Literal::Integer(value)) => {
            let signed = i128::try_from(value).unwrap_or(i128::MAX);
            state
                .expr_ranges
                .insert(u32::try_from(index).expect("expr"), IntRange::point(signed));
        }
        ExprKind::Block { statements, tail } => {
            for stmt in &owner.statements[statements.start as usize..statements.end as usize] {
                if let StatementKind::Let {
                    value: Some(value), ..
                } = &stmt.kind
                {
                    walk_expr(owner, *value, state, early, model);
                }
            }
            if let Some(tail) = tail {
                walk_expr(owner, tail, state, early, model);
            }
        }
        ExprKind::If {
            condition,
            then_value,
            else_value,
        } => {
            walk_expr(owner, condition, state, early, model);
            walk_expr(owner, then_value, state, early, model);
            if let Some(else_value) = else_value {
                walk_expr(owner, else_value, state, early, model);
            }
        }
        ExprKind::For { value, body, .. } => {
            walk_expr(owner, value, state, early, model);
            if let Some(upper) = range_upper(owner, value, state) {
                state
                    .loop_upper
                    .insert(u32::try_from(body.index()).expect("body"), upper);
            }
            walk_expr(owner, body, state, early, model);
        }
        ExprKind::Loop { body } => {
            walk_expr(owner, body, state, early, model);
        }
        ExprKind::Index { base, index, .. } => {
            walk_expr(owner, base, state, early, model);
            walk_expr(owner, index, state, early, model);
        }
        ExprKind::Call { .. } => {
            state.heap_version = state.heap_version.saturating_add(1);
            invalidate_len_facts(state);
        }
        _ => {}
    }
}

fn range_upper(owner: &Owner, value: ExprId, state: &AbstractState) -> Option<i128> {
    if let Some(range) = state.expr_ranges.get(&u32::try_from(value.index()).ok()?) {
        return range.max.map(|max| max.saturating_add(1));
    }
    if let ExprKind::Literal(Literal::Integer(v)) = &owner.expressions[value.index()].kind {
        return i128::try_from(*v).ok();
    }
    None
}

fn invalidate_len_facts(state: &mut AbstractState) {
    state.len_lower.clear();
    state.loop_upper.clear();
    for range in state.expr_ranges.values_mut() {
        *range = IntRange::UNKNOWN;
    }
}

fn merge_state(into: &mut AbstractState, from: &AbstractState) -> bool {
    if !from.reachable {
        return false;
    }
    into.reachable = true;
    let mut changed = false;
    for (expr, range) in &from.expr_ranges {
        let entry = into.expr_ranges.entry(*expr).or_insert(IntRange::UNKNOWN);
        let merged = entry.union_widen(*range);
        if merged != *entry {
            *entry = merged;
            changed = true;
        }
    }
    changed
}

fn summarize_effects(cfg: &OwnerCfg) -> FunctionSummary {
    let mut summary = FunctionSummary::default();
    summary.may_panic = false;
    summary.may_call_unknown = false;
    summary.may_mutate_len = cfg.block_states.iter().any(|state| state.heap_version > 0);
    summary
}

fn prove_check(
    owner: &Owner,
    cfg: &OwnerCfg,
    check: &RuntimeCheck,
    model: &Model<'_>,
    checked: &CheckedSemantics,
    owner_index: usize,
) -> ProofStatus {
    if let Some(early_status) = early_const_proof(model, checked, owner_index, check) {
        return if early_status {
            ProofStatus::Proved
        } else {
            ProofStatus::Disproved
        };
    }
    match &check.kind {
        CheckKind::Bounds { slice: false } => prove_array_bounds(owner, cfg, check),
        CheckKind::Bounds { slice: true } => ProofStatus::Unknown,
        CheckKind::Shift { amount, .. } => prove_shift(owner, cfg, *amount),
        CheckKind::Division { divisor, .. } => prove_division(owner, cfg, *divisor),
        CheckKind::FloatToInt { .. } => ProofStatus::Unknown,
        CheckKind::UnicodeScalar { value } => prove_unicode_scalar(owner, *value),
        CheckKind::Utf8Boundary => ProofStatus::Unknown,
    }
}

fn checked_body_for_owner(checked: &CheckedSemantics, owner_index: usize) -> Option<&CheckedBody> {
    checked.bodies.get(owner_index)
}

fn early_const_proof(
    model: &Model<'_>,
    checked: &CheckedSemantics,
    owner_index: usize,
    check: &RuntimeCheck,
) -> Option<bool> {
    let body = checked_body_for_owner(checked, owner_index)?;
    let sem_check = body.runtime_checks.iter().find(|candidate| {
        candidate.expression.0 == check.expression.0
            && hir_check_kind_matches(&candidate.kind, &check.kind)
    })?;
    let expressions: Vec<(crate::frontend::ast::ExprId, _)> = body
        .expressions
        .iter()
        .map(|(id, ty)| (*id, ty.clone()))
        .collect();
    Some(model.check_proven(body.definition.module, sem_check, &expressions))
}

fn hir_check_kind_matches(sem: &SemCheckKind, hir: &CheckKind) -> bool {
    match (sem, hir) {
        (SemCheckKind::Bounds { slice: a }, CheckKind::Bounds { slice: b }) => a == b,
        (SemCheckKind::IntegerDivision { .. }, CheckKind::Division { .. }) => true,
        (SemCheckKind::Shift { .. }, CheckKind::Shift { .. }) => true,
        (SemCheckKind::FloatToInt { .. }, CheckKind::FloatToInt { .. }) => true,
        (SemCheckKind::UnicodeScalar { .. }, CheckKind::UnicodeScalar { .. }) => true,
        (SemCheckKind::Utf8Boundary, CheckKind::Utf8Boundary) => true,
        _ => false,
    }
}

fn prove_array_bounds(owner: &Owner, cfg: &OwnerCfg, check: &RuntimeCheck) -> ProofStatus {
    let state = cfg
        .block_states
        .iter()
        .find(|state| state.reachable)
        .unwrap_or(&cfg.block_states[0]);
    let ExprKind::Index { index, .. } = &owner.expressions[check.expression.index()].kind else {
        return ProofStatus::Unknown;
    };
    let idx_key = u32::try_from(index.index()).ok();
    let idx_range = idx_key
        .and_then(|k| state.expr_ranges.get(&k).copied())
        .unwrap_or(IntRange::UNKNOWN);
    if idx_range == IntRange::UNKNOWN {
        return ProofStatus::Unknown;
    }
    if let Some(min) = idx_range.min
        && min < 0
    {
        return ProofStatus::Unknown;
    }
    if let (Some(min), Some(max)) = (idx_range.min, idx_range.max)
        && min >= 0
        && max < 1_000
    {
        return ProofStatus::Proved;
    }
    if let Some(max) = idx_range.max {
        if let Ok(body_key) = u32::try_from(owner.body.index()) {
            if let Some(upper) = state.loop_upper.get(&body_key) {
                if max < *upper {
                    return ProofStatus::Proved;
                }
            }
        }
    }
    ProofStatus::Unknown
}

fn prove_shift(owner: &Owner, cfg: &OwnerCfg, amount: ExprId) -> ProofStatus {
    let state = cfg.block_states.first().expect("entry state");
    if let Ok(amount_key) = u32::try_from(amount.index()) {
        if let Some(range) = state.expr_ranges.get(&amount_key) {
            if let (Some(min), Some(max)) = (range.min, range.max)
                && min >= 0
                && max < 128
            {
                return ProofStatus::Proved;
            }
        }
    }
    if let ExprKind::Literal(Literal::Integer(v)) = &owner.expressions[amount.index()].kind
        && *v < 128
    {
        return ProofStatus::Proved;
    }
    ProofStatus::Unknown
}

fn prove_division(_owner: &Owner, _cfg: &OwnerCfg, divisor: ExprId) -> ProofStatus {
    if let ExprKind::Literal(Literal::Integer(v)) = &_owner.expressions[divisor.index()].kind {
        return if *v == 0 {
            ProofStatus::Disproved
        } else {
            ProofStatus::Proved
        };
    }
    ProofStatus::Unknown
}

fn prove_unicode_scalar(owner: &Owner, value: ExprId) -> ProofStatus {
    if let ExprKind::Literal(Literal::Integer(v)) = &owner.expressions[value.index()].kind {
        let Ok(scalar) = u32::try_from(*v) else {
            return ProofStatus::Disproved;
        };
        return if char::from_u32(scalar).is_some() {
            ProofStatus::Proved
        } else {
            ProofStatus::Disproved
        };
    }
    ProofStatus::Unknown
}

fn build_call_graph(
    module: &Module,
    keys: &[AnalysisOwnerKey],
    def_to_node: &BTreeMap<hir::DefId, usize>,
) -> Vec<Vec<usize>> {
    let mut graph = vec![Vec::new(); keys.len()];
    for (node, key) in keys.iter().enumerate() {
        let owner = &module.owners[key.owner_index as usize];
        collect_calls(owner, def_to_node, node, &mut graph);
    }
    graph
}

fn collect_calls(
    owner: &Owner,
    def_to_node: &BTreeMap<hir::DefId, usize>,
    node: usize,
    graph: &mut [Vec<usize>],
) {
    for expression in &owner.expressions {
        if let ExprKind::Call {
            target: CallTarget::Dispatch(dispatch_index),
            ..
        } = &expression.kind
        {
            let dispatch = &owner.dispatches[*dispatch_index as usize];
            if let Some(function) = dispatch.function
                && let Some(&callee) = def_to_node.get(&function)
            {
                graph[node].push(callee);
            }
        }
    }
}

fn tarjan(graph: &[Vec<usize>], n: usize) -> Vec<Vec<usize>> {
    let mut index = 0usize;
    let mut stack = Vec::new();
    let mut on_stack = vec![false; n];
    let mut indices = vec![None; n];
    let mut lowlink = vec![0usize; n];
    let mut scc = Vec::new();

    fn strong_connect(
        v: usize,
        graph: &[Vec<usize>],
        index: &mut usize,
        stack: &mut Vec<usize>,
        on_stack: &mut [bool],
        indices: &mut [Option<usize>],
        lowlink: &mut [usize],
        scc: &mut Vec<Vec<usize>>,
    ) {
        indices[v] = Some(*index);
        lowlink[v] = *index;
        *index += 1;
        stack.push(v);
        on_stack[v] = true;
        for &w in &graph[v] {
            if indices[w].is_none() {
                strong_connect(w, graph, index, stack, on_stack, indices, lowlink, scc);
                lowlink[v] = lowlink[v].min(lowlink[w]);
            } else if on_stack[w] {
                lowlink[v] = lowlink[v].min(indices[w].unwrap());
            }
        }
        if lowlink[v] == indices[v].unwrap() {
            let mut component = Vec::new();
            loop {
                let w = stack.pop().expect("stack");
                on_stack[w] = false;
                component.push(w);
                if w == v {
                    break;
                }
            }
            component.sort_unstable();
            scc.push(component);
        }
    }

    for v in 0..n {
        if indices[v].is_none() {
            strong_connect(
                v,
                graph,
                &mut index,
                &mut stack,
                &mut on_stack,
                &mut indices,
                &mut lowlink,
                &mut scc,
            );
        }
    }
    scc
}
