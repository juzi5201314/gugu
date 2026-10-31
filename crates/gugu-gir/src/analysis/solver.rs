//! 以具体实例为固定点成员；共享 HIR 上的证明取全部可达实例的共同结论。

use super::interpret;
use super::policy::AnalysisPolicyV1;
use super::types::{
    AnalysisOwnerKey, AnalysisWorldV1, FunctionSummary, InstanceSummaryRecord, ProofFact,
    RuntimeCheckKey, SccSummaryV1, WORLD_SCHEMA_VERSION, sort_proofs,
};
use crate::gir::GirWorldV1;
use crate::hir::Module;
use crate::mono::instantiate::CallSite;

/// 一个实例 SCC 的求解成员：实例 key 与其定义的 owner 身份。
#[derive(Clone)]
pub struct SccMember {
    pub mono_key: Vec<u8>,
    pub owner: AnalysisOwnerKey,
    pub calls: Vec<(CallSite, usize)>,
}

pub fn analyze_scc(
    module: &Module,
    gir: &GirWorldV1,
    members: &[SccMember],
    component: &[usize],
    policy: AnalysisPolicyV1,
    callees: &dyn Fn(usize) -> FunctionSummary,
) -> SccSummaryV1 {
    let mut summaries = vec![FunctionSummary::default(); component.len()];
    let dependents = summary_dependents(members, component);
    let mut results: Vec<Option<interpret::BodyResult>> = std::iter::repeat_with(|| None)
        .take(component.len())
        .collect();
    let mut budget_exhausted = false;
    let mut converged = false;
    for iteration in 0..policy.max_scc_iterations {
        let mut changed = false;
        for (position, &index) in component.iter().enumerate() {
            let result = results[position].get_or_insert_with(|| {
                analyze_member(
                    module,
                    gir,
                    &members[index],
                    component,
                    &summaries,
                    policy,
                    callees,
                )
            });
            budget_exhausted |= result.budget_exhausted;
            let mut next = result.summary.clone();
            if iteration != 0 {
                next.join_with(&summaries[position]);
            }
            if next != summaries[position] {
                changed = true;
                summaries[position] = next;
                for &caller in &dependents[position] {
                    results[caller] = None;
                }
            }
        }
        if !changed {
            converged = true;
            break;
        }
        if budget_exhausted {
            break;
        }
    }
    budget_exhausted |= !converged;
    if budget_exhausted {
        let conservative = FunctionSummary::conservative();
        for (position, summary) in summaries.iter_mut().enumerate() {
            if *summary != conservative {
                *summary = conservative.clone();
                for &caller in &dependents[position] {
                    results[caller] = None;
                }
            }
        }
    }
    let mut proofs = Vec::new();
    for (position, &index) in component.iter().enumerate() {
        let member = &members[index];
        let result = results[position].take().unwrap_or_else(|| {
            analyze_member(module, gir, member, component, &summaries, policy, callees)
        });
        for (expression, kind, status) in result.proofs {
            proofs.push(ProofFact {
                key: RuntimeCheckKey {
                    owner_index: member.owner.owner_index,
                    expression,
                    kind,
                },
                status: if budget_exhausted || result.budget_exhausted {
                    super::types::ProofStatus::Unknown
                } else {
                    status
                },
            });
        }
    }
    sort_proofs(&mut proofs);
    let instances = component
        .iter()
        .zip(summaries)
        .map(|(&index, summary)| InstanceSummaryRecord {
            mono_key: members[index].mono_key.clone(),
            summary,
        })
        .collect();
    SccSummaryV1 {
        instances,
        proofs,
        budget_exhausted,
    }
}

fn summary_dependents(members: &[SccMember], component: &[usize]) -> Vec<Vec<usize>> {
    // SCC 成员位置是连续下标；外部摘要在本轮求解期间固定，不需要失效传播。
    debug_assert!(component.windows(2).all(|pair| pair[0] < pair[1]));
    let mut dependents = vec![Vec::new(); component.len()];
    for (caller, &index) in component.iter().enumerate() {
        for &(_, target) in &members[index].calls {
            if let Ok(callee) = component.binary_search(&target) {
                dependents[callee].push(caller);
            }
        }
    }
    for callers in &mut dependents {
        callers.dedup();
    }
    dependents
}

fn analyze_member(
    module: &Module,
    gir: &GirWorldV1,
    member: &SccMember,
    component: &[usize],
    summaries: &[FunctionSummary],
    policy: AnalysisPolicyV1,
    callees: &dyn Fn(usize) -> FunctionSummary,
) -> interpret::BodyResult {
    debug_assert_eq!(component.len(), summaries.len());
    let lookup = |site| {
        let position = member
            .calls
            .binary_search_by_key(&site, |(site, _)| *site)
            .ok()?;
        let target = member.calls[position].1;
        Some(match component.binary_search(&target) {
            Ok(position) => summaries[position].clone(),
            Err(_) => callees(target),
        })
    };
    let owner_index = member.owner.owner_index;
    interpret::analyze_owner(
        module,
        &module.owners[usize::try_from(owner_index).expect("owner 编号适配宿主")],
        interpret::body_of(module, gir, owner_index),
        owner_index,
        policy,
        &lookup,
    )
}

/// 预算耗尽沿调用边向 caller 传播：返回自身 SCC 或可达 callee SCC 耗尽的成员 owner。
///
/// `sccs` 必须是 Tarjan 的产出顺序（callee 分量先于 caller）；`successors[m]` 是成员 `m` 的
/// 直接 callee 成员，`owners[m]` 是其 owner 下标，`exhausted[c]` 是分量 `c` 自身的预算位。
/// 一次逆拓扑扫描即完成闭包；结果按 owner 升序去重。
pub fn exhausted_owners(
    sccs: &[Vec<usize>],
    successors: &[Vec<usize>],
    owners: &[u32],
    exhausted: &[bool],
) -> Vec<u32> {
    debug_assert_eq!(sccs.len(), exhausted.len());
    debug_assert_eq!(successors.len(), owners.len());
    let mut component_of = vec![usize::MAX; owners.len()];
    for (component, members) in sccs.iter().enumerate() {
        for &member in members {
            component_of[member] = component;
        }
    }
    let mut reached = exhausted.to_vec();
    let mut result = Vec::new();
    for (component, members) in sccs.iter().enumerate() {
        // callee 分量都排在前面，此时它们的可达位已经定稿。
        let inherited = members.iter().any(|&member| {
            successors[member].iter().any(|&callee| {
                let target = component_of[callee];
                debug_assert!(target <= component, "Tarjan 顺序保证 callee 分量先完成");
                reached[target]
            })
        });
        reached[component] |= inherited;
        if reached[component] {
            result.extend(members.iter().map(|&member| owners[member]));
        }
    }
    result.sort_unstable();
    result.dedup();
    result
}

pub fn world_from_sccs(
    sccs: Vec<SccSummaryV1>,
    exhausted_owners: Vec<u32>,
    input_fingerprint: [u8; 32],
) -> AnalysisWorldV1 {
    let mut instances = Vec::new();
    let mut proofs = Vec::new();
    for scc in sccs {
        instances.extend(scc.instances);
        proofs.extend(scc.proofs);
    }
    instances.sort_by(|left, right| left.mono_key.cmp(&right.mono_key));
    instances.dedup_by(|left, right| left.mono_key == right.mono_key);
    sort_proofs(&mut proofs);
    proofs.dedup_by(|candidate, retained| {
        if candidate.key != retained.key {
            return false;
        }
        if candidate.status != retained.status {
            retained.status = super::types::ProofStatus::Unknown;
        }
        true
    });
    let runtime_checks_elided_count = u32::try_from(
        proofs
            .iter()
            .filter(|fact| fact.status == super::types::ProofStatus::Proved)
            .count(),
    )
    .expect("证明数量适配 u32");
    AnalysisWorldV1 {
        schema: WORLD_SCHEMA_VERSION,
        input_fingerprint,
        instances,
        proofs,
        exhausted_owners,
        runtime_checks_elided_count,
    }
}
