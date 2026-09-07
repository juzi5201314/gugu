//! 过程内分析编排与 proof 写回。
//!
//! 阶段 24 起 SCC 成员是 `MonoKey` 实例；求解仍在定义级 HIR owner 上求固定点，
//! 每个实例投影其定义的摘要（GIR 就绪后升级为实例级求解）。

use super::interpret;
use super::policy::AnalysisPolicyV1;
use super::types::{
    AnalysisOwnerKey, AnalysisWorldV1, FunctionSummary, InstanceSummaryRecord, ProofFact,
    RuntimeCheckKey, SccSummaryV1, WORLD_SCHEMA_VERSION, sort_proofs,
};
use crate::frontend::hir::{self, Module};
use std::collections::BTreeMap;

/// 一个实例 SCC 的求解成员：实例 key 与其定义的 owner 身份。
#[derive(Clone)]
pub(crate) struct SccMember {
    pub mono_key: Vec<u8>,
    pub owner: AnalysisOwnerKey,
}

pub(crate) fn analyze_scc(
    module: &Module,
    members: &[SccMember],
    policy: AnalysisPolicyV1,
    callees: &dyn Fn(hir::DefId) -> FunctionSummary,
) -> SccSummaryV1 {
    // 定义级求解：同一定义的多个实例共享一次解释。
    let mut owners: Vec<AnalysisOwnerKey> = Vec::new();
    for member in members {
        if !owners.contains(&member.owner) {
            owners.push(member.owner);
        }
    }
    owners.sort_by_key(|owner| owner.owner_index);
    let mut summaries: BTreeMap<u32, FunctionSummary> = owners
        .iter()
        .map(|owner| (owner.owner_index, FunctionSummary::default()))
        .collect();
    let mut budget_exhausted = false;
    for _ in 0..policy.max_scc_iterations {
        let mut changed = false;
        for owner in &owners {
            let lookup = |def: hir::DefId| {
                owners
                    .iter()
                    .find(|item| item.definition == def)
                    .and_then(|item| summaries.get(&item.owner_index).cloned())
                    .unwrap_or_else(|| callees(def))
            };
            let result = interpret::analyze_owner(
                module,
                &module.owners[owner.owner_index as usize],
                owner.owner_index,
                policy,
                &lookup,
            );
            budget_exhausted |= result.budget_exhausted;
            let slot = summaries.get_mut(&owner.owner_index).expect("summary");
            if *slot != result.summary {
                *slot = result.summary.clone();
                changed = true;
            }
        }
        if !changed || budget_exhausted {
            break;
        }
    }
    if budget_exhausted {
        for summary in summaries.values_mut() {
            summary.join_with(&FunctionSummary::conservative());
        }
    }
    let mut proofs = Vec::new();
    for owner in &owners {
        let lookup = |def: hir::DefId| {
            owners
                .iter()
                .find(|item| item.definition == def)
                .and_then(|item| summaries.get(&item.owner_index).cloned())
                .unwrap_or_else(|| callees(def))
        };
        let result = interpret::analyze_owner(
            module,
            &module.owners[owner.owner_index as usize],
            owner.owner_index,
            policy,
            &lookup,
        );
        for (expression, kind, status) in result.proofs {
            proofs.push(ProofFact {
                key: RuntimeCheckKey {
                    owner_index: owner.owner_index,
                    expression,
                    kind,
                },
                status: if budget_exhausted {
                    super::types::ProofStatus::Unknown
                } else {
                    status
                },
            });
        }
    }
    sort_proofs(&mut proofs);
    let instances = members
        .iter()
        .map(|member| InstanceSummaryRecord {
            mono_key: member.mono_key.clone(),
            summary: summaries
                .get(&member.owner.owner_index)
                .cloned()
                .unwrap_or_else(FunctionSummary::conservative),
        })
        .collect();
    SccSummaryV1 {
        instances,
        proofs,
        budget_exhausted,
    }
}

pub(crate) fn world_from_sccs(
    sccs: Vec<SccSummaryV1>,
    input_fingerprint: [u8; 32],
) -> AnalysisWorldV1 {
    let mut instances = Vec::new();
    let mut proofs = Vec::new();
    let mut budget_exhausted = false;
    for scc in sccs {
        budget_exhausted |= scc.budget_exhausted;
        instances.extend(scc.instances);
        proofs.extend(scc.proofs);
    }
    instances.sort_by(|left, right| left.mono_key.cmp(&right.mono_key));
    instances.dedup_by(|left, right| left.mono_key == right.mono_key);
    sort_proofs(&mut proofs);
    proofs.dedup_by(|left, right| left.key == right.key);
    let runtime_checks_elided_count = proofs
        .iter()
        .filter(|fact| fact.status == super::types::ProofStatus::Proved)
        .count() as u32;
    AnalysisWorldV1 {
        schema: WORLD_SCHEMA_VERSION,
        input_fingerprint,
        instances,
        proofs,
        budget_exhausted,
        runtime_checks_elided_count,
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
