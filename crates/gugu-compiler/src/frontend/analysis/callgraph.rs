//! 调用图 SCC：Tarjan 产出按凝聚图拓扑可用的分量。

use super::types::AnalysisOwnerKey;
use crate::frontend::hir::Module;

/// 按下标取定义级求解身份。
pub(crate) fn callable_key_at(module: &Module, index: usize) -> AnalysisOwnerKey {
    AnalysisOwnerKey {
        owner_index: u32::try_from(index).expect("owner index"),
        definition: module.owners[index].definition,
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

/// 通用 Tarjan SCC：节点为稠密下标；分量内成员按序，供实例图与分析共用。
pub(crate) fn strongly_connected_components(graph: &[Vec<usize>]) -> Vec<Vec<usize>> {
    tarjan(graph, graph.len())
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
