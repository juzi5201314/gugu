//! 过程内事实收集与摘要固定点：只消费冻结前的 HIR，不回看语义侧表。

use super::policy::AnalysisPolicyV1;
use super::types::{
    AnalysisOwnerKey, AnalysisWorldV1, FunctionSummary, OwnerSummaryRecord, ProofFact, ProofStatus,
    RuntimeCheckKey, WORLD_SCHEMA_VERSION, sort_proofs,
};
use crate::frontend::hir::{self, DefinitionKind, ExprId, ExprKind, Literal, Module, RuntimeCheck};
use std::collections::BTreeMap;

/// 一个 owner 的过程内事实：字面量点区间与效果证据。
#[derive(Clone, Debug, Default)]
struct LocalFacts {
    /// 整数字面量的值，按 HIR 表达式下标索引。
    integer_points: BTreeMap<u32, i128>,
    /// 本体内至少一个调用点（含未知目标）。
    has_call: bool,
    /// 本体内至少一个效果位为 WRITE 的表达式。
    has_write: bool,
    /// 本体内至少一个外部（FFI/汇编）调用点。
    has_foreign: bool,
}

pub(crate) fn analyze(module: &Module, policy: AnalysisPolicyV1) -> AnalysisWorldV1 {
    let keys = callable_keys(module);
    let mut budget_exhausted = false;
    let summaries = fixpoint_summaries(module, &keys, policy, &mut budget_exhausted);
    let mut proofs = Vec::new();
    let mut elided = 0u32;
    for key in &keys {
        let owner = &module.owners[key.owner_index as usize];
        let facts = collect_facts(owner);
        for check in &owner.checks {
            let status = prove_check(module, owner, &facts, check);
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
        .map(|key| OwnerSummaryRecord {
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

fn callable_keys(module: &Module) -> Vec<AnalysisOwnerKey> {
    module
        .owners
        .iter()
        .enumerate()
        .filter(|(_, owner)| {
            matches!(
                module.definitions[owner.definition.index()].kind,
                DefinitionKind::Function | DefinitionKind::Closure | DefinitionKind::Async
            )
        })
        .map(|(index, owner)| AnalysisOwnerKey {
            owner_index: u32::try_from(index).expect("owner index"),
            definition: owner.definition,
        })
        .collect()
}

/// 按 SCC 迭代到固定点；每个 callable 的摘要只能从保守初值单调精化。
fn fixpoint_summaries(
    module: &Module,
    keys: &[AnalysisOwnerKey],
    policy: AnalysisPolicyV1,
    budget_exhausted: &mut bool,
) -> BTreeMap<u32, FunctionSummary> {
    let def_to_node: BTreeMap<hir::DefId, usize> = keys
        .iter()
        .enumerate()
        .map(|(node, key)| (key.definition, node))
        .collect();
    let call_graph = build_call_graph(module, keys, &def_to_node);
    let scc = tarjan(&call_graph, keys.len());
    let mut summaries: BTreeMap<u32, FunctionSummary> = keys
        .iter()
        .map(|key| (key.owner_index, FunctionSummary::conservative()))
        .collect();
    for round in 0..policy.max_scc_iterations {
        let mut changed = false;
        for component in &scc {
            for &node in component {
                let key = keys[node];
                // 候选 = 自身效果证据 ∪ 已知 callee 的当前摘要（效果并集）。
                let mut candidate = summarize_owner(module, key);
                for &callee in &call_graph[node] {
                    candidate.join_with(&summaries[&keys[callee].owner_index]);
                }
                let summary = summaries.get_mut(&key.owner_index).expect("owner summary");
                if *summary != candidate {
                    *summary = candidate;
                    changed = true;
                }
            }
        }
        if !changed {
            return summaries;
        }
        if round + 1 == policy.max_scc_iterations {
            *budget_exhausted = true;
        }
    }
    if *budget_exhausted {
        for key in keys {
            summaries.insert(key.owner_index, FunctionSummary::conservative());
        }
    }
    summaries
}

/// 过程内效果证据直接组合出的候选摘要。
fn summarize_owner(module: &Module, key: AnalysisOwnerKey) -> FunctionSummary {
    let owner = &module.owners[key.owner_index as usize];
    let facts = collect_facts(owner);
    FunctionSummary {
        may_panic: facts.has_call || has_check(owner) || has_panic(owner),
        may_call_unknown: facts.has_call,
        may_mutate_len: facts.has_call || facts.has_write,
        reads_hidden_state: facts.has_foreign || reads_static(module, owner),
        writes_hidden_state: facts.has_call || facts.has_write,
    }
}

fn has_check(owner: &hir::Owner) -> bool {
    !owner.checks.is_empty()
}

fn has_panic(owner: &hir::Owner) -> bool {
    owner.expressions.iter().any(|expression| {
        matches!(
            expression.kind,
            ExprKind::Resolved(hir::Res::Builtin(hir::Builtin::Panic))
        )
    })
}

fn reads_static(module: &Module, owner: &hir::Owner) -> bool {
    owner.expressions.iter().any(|expression| {
        matches!(
            &expression.kind,
            ExprKind::Resolved(hir::Res::Def(definition))
                if matches!(
                    module.definitions[definition.index()].kind,
                    DefinitionKind::Static | DefinitionKind::LocalStatic
                )
        )
    })
}

fn collect_facts(owner: &hir::Owner) -> LocalFacts {
    let mut facts = LocalFacts::default();
    for (index, expression) in owner.expressions.iter().enumerate() {
        if let ExprKind::Literal(Literal::Integer(value)) = &expression.kind {
            let signed = i128::try_from(*value).unwrap_or(i128::MAX);
            facts
                .integer_points
                .insert(u32::try_from(index).expect("expr"), signed);
        }
        if expression.effects.0 & hir::Effects::WRITE != 0 {
            facts.has_write = true;
        }
        if matches!(
            expression.kind,
            ExprKind::Call { .. } | ExprKind::SpawnCall { .. }
        ) {
            facts.has_call = true;
        }
    }
    facts.has_foreign = !owner.foreign_calls.is_empty() || !owner.assembly.is_empty();
    facts
}

/// 逐检查的证明：只有 HIR 局部事实充分时才离开 `Unknown`。
fn prove_check(
    module: &Module,
    owner: &hir::Owner,
    facts: &LocalFacts,
    check: &RuntimeCheck,
) -> ProofStatus {
    match &check.kind {
        hir::CheckKind::Bounds { slice: false } => prove_array_bounds(module, owner, facts, check),
        hir::CheckKind::Bounds { slice: true } => ProofStatus::Unknown,
        hir::CheckKind::Shift { amount, .. } => prove_shift(facts, *amount),
        hir::CheckKind::Division { divisor, .. } => prove_division(facts, *divisor),
        hir::CheckKind::FloatToInt { .. } => ProofStatus::Unknown,
        hir::CheckKind::UnicodeScalar { value } => prove_unicode_scalar(facts, *value),
        hir::CheckKind::Utf8Boundary => ProofStatus::Unknown,
    }
}

fn prove_array_bounds(
    module: &Module,
    owner: &hir::Owner,
    facts: &LocalFacts,
    check: &RuntimeCheck,
) -> ProofStatus {
    let ExprKind::Index { base, index, .. } = &owner.expressions[check.expression.index()].kind
    else {
        return ProofStatus::Unknown;
    };
    let Some(length) = array_length(module, owner, *base) else {
        return ProofStatus::Unknown;
    };
    let Some(value) = integer_point(facts, *index) else {
        return ProofStatus::Unknown;
    };
    if value < 0 || value >= i128::from(length) {
        return ProofStatus::Disproved;
    }
    ProofStatus::Proved
}

fn array_length(module: &Module, owner: &hir::Owner, base: ExprId) -> Option<u64> {
    let ty = &module.types[owner.expression_types[base.index()].index()];
    match ty {
        hir::Type::Array(_, length) => Some(*length),
        _ => None,
    }
}

/// spec：移位检查只针对负移位量，非负字面量即安全。
fn prove_shift(facts: &LocalFacts, amount: ExprId) -> ProofStatus {
    match integer_point(facts, amount) {
        Some(value) if value < 0 => ProofStatus::Disproved,
        Some(_) => ProofStatus::Proved,
        None => ProofStatus::Unknown,
    }
}

fn prove_division(facts: &LocalFacts, divisor: ExprId) -> ProofStatus {
    match integer_point(facts, divisor) {
        Some(0) => ProofStatus::Disproved,
        Some(_) => ProofStatus::Proved,
        None => ProofStatus::Unknown,
    }
}

fn prove_unicode_scalar(facts: &LocalFacts, value: ExprId) -> ProofStatus {
    let Some(raw) = integer_point(facts, value) else {
        return ProofStatus::Unknown;
    };
    match u32::try_from(raw).ok().and_then(char::from_u32) {
        Some(_) => ProofStatus::Proved,
        None => ProofStatus::Disproved,
    }
}

fn integer_point(facts: &LocalFacts, expression: ExprId) -> Option<i128> {
    facts
        .integer_points
        .get(&u32::try_from(expression.index()).ok()?)
        .copied()
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
    owner: &hir::Owner,
    def_to_node: &BTreeMap<hir::DefId, usize>,
    node: usize,
    graph: &mut [Vec<usize>],
) {
    for expression in &owner.expressions {
        if let ExprKind::Call {
            target: hir::CallTarget::Dispatch(dispatch_index),
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
                lowlink[v] = lowlink[v].min(indices[w].expect("indexed"));
            }
        }
        if lowlink[v] == indices[v].expect("indexed") {
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
