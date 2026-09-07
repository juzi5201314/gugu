//! 调用图 SCC：Tarjan 产出按凝聚图拓扑可用的分量。

use super::types::AnalysisOwnerKey;
use crate::frontend::hir::{self, DefinitionKind, Module};
use std::collections::BTreeMap;

pub(crate) fn callable_keys(module: &Module) -> Vec<AnalysisOwnerKey> {
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

pub(crate) fn call_graph(
    module: &Module,
    keys: &[AnalysisOwnerKey],
) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let def_to_node: BTreeMap<hir::DefId, usize> = keys
        .iter()
        .enumerate()
        .map(|(node, key)| (key.definition, node))
        .collect();
    let mut graph = vec![Vec::new(); keys.len()];
    for (node, key) in keys.iter().enumerate() {
        let owner = &module.owners[key.owner_index as usize];
        collect_calls(owner, &def_to_node, node, &mut graph);
    }
    for edges in &mut graph {
        edges.sort_unstable();
        edges.dedup();
    }
    let scc = tarjan(&graph, keys.len());
    (graph, scc)
}

fn collect_calls(
    owner: &hir::Owner,
    def_to_node: &BTreeMap<hir::DefId, usize>,
    node: usize,
    graph: &mut [Vec<usize>],
) {
    for expression in &owner.expressions {
        let target = match &expression.kind {
            hir::ExprKind::Call { target, .. } | hir::ExprKind::SpawnCall { target, .. } => target,
            _ => continue,
        };
        if let Some(function) = callee_definition(owner, target)
            && let Some(&callee) = def_to_node.get(&function)
        {
            graph[node].push(callee);
        }
    }
}

pub(crate) fn callee_definition(
    owner: &hir::Owner,
    target: &hir::CallTarget,
) -> Option<hir::DefId> {
    match target {
        hir::CallTarget::Dispatch(index) => owner.dispatches[*index as usize].function,
        hir::CallTarget::Value(value) => match owner.expressions[value.index()].kind {
            hir::ExprKind::Resolved(hir::Res::Def(definition))
            | hir::ExprKind::Resolved(hir::Res::Associated { definition, .. }) => Some(definition),
            _ => None,
        },
        hir::CallTarget::Builtin(_) | hir::CallTarget::Constructor { .. } => None,
    }
}

fn tarjan(graph: &[Vec<usize>], n: usize) -> Vec<Vec<usize>> {
    let mut index = 0usize;
    let mut stack = Vec::new();
    let mut on_stack = vec![false; n];
    let mut indices = vec![None; n];
    let mut lowlink = vec![0usize; n];
    let mut scc = Vec::new();
    for v in 0..n {
        if indices[v].is_none() {
            connect(
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

fn connect(
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
            connect(w, graph, index, stack, on_stack, indices, lowlink, scc);
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
