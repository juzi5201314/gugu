//! 过程内分析编排与 proof 写回。

use super::interpret;
use super::policy::AnalysisPolicyV1;
use super::types::{
    AnalysisOwnerKey, AnalysisWorldV1, FunctionSummary, OwnerSummaryRecord, ProofFact,
    RuntimeCheckKey, SccSummaryV1, WORLD_SCHEMA_VERSION, sort_proofs,
};
use crate::frontend::hir::{self, Module};
use std::collections::BTreeMap;

pub(crate) fn analyze_scc(
    module: &Module,
    keys: &[AnalysisOwnerKey],
    policy: AnalysisPolicyV1,
    callees: &dyn Fn(hir::DefId) -> FunctionSummary,
) -> SccSummaryV1 {
    let mut summaries: BTreeMap<u32, FunctionSummary> = keys
        .iter()
        .map(|key| (key.owner_index, FunctionSummary::default()))
        .collect();
    let mut budget_exhausted = false;
    for _ in 0..policy.max_scc_iterations {
        let mut changed = false;
        for key in keys {
            let lookup = |def: hir::DefId| {
                keys.iter()
                    .find(|item| item.definition == def)
                    .and_then(|item| summaries.get(&item.owner_index).cloned())
                    .unwrap_or_else(|| callees(def))
            };
            let result = interpret::analyze_owner(
                module,
                &module.owners[key.owner_index as usize],
                key.owner_index,
                policy,
                &lookup,
            );
            budget_exhausted |= result.budget_exhausted;
            let slot = summaries.get_mut(&key.owner_index).expect("summary");
            if *slot != result.summary {
                *slot = result.summary;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        if budget_exhausted {
            break;
        }
    }
    if budget_exhausted {
        for summary in summaries.values_mut() {
            summary.join_with(&FunctionSummary::conservative());
        }
    }
    let mut proofs = Vec::new();
    for key in keys {
        let lookup = |def: hir::DefId| {
            keys.iter()
                .find(|item| item.definition == def)
                .and_then(|item| summaries.get(&item.owner_index).cloned())
                .unwrap_or_else(|| callees(def))
        };
        let result = interpret::analyze_owner(
            module,
            &module.owners[key.owner_index as usize],
            key.owner_index,
            policy,
            &lookup,
        );
        for (expression, kind, status) in result.proofs {
            proofs.push(ProofFact {
                key: RuntimeCheckKey {
                    owner_index: key.owner_index,
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
    let owners = keys
        .iter()
        .map(|key| OwnerSummaryRecord {
            key: *key,
            summary: summaries
                .get(&key.owner_index)
                .cloned()
                .unwrap_or_else(FunctionSummary::conservative),
        })
        .collect();
    SccSummaryV1 {
        owners,
        proofs,
        budget_exhausted,
    }
}

pub(crate) fn world_from_sccs(
    sccs: Vec<SccSummaryV1>,
    input_fingerprint: [u8; 32],
) -> AnalysisWorldV1 {
    let mut owners = Vec::new();
    let mut proofs = Vec::new();
    let mut budget_exhausted = false;
    for scc in sccs {
        budget_exhausted |= scc.budget_exhausted;
        owners.extend(scc.owners);
        proofs.extend(scc.proofs);
    }
    owners.sort_by_key(|record| record.key);
    sort_proofs(&mut proofs);
    let runtime_checks_elided_count = proofs
        .iter()
        .filter(|fact| fact.status == super::types::ProofStatus::Proved)
        .count() as u32;
    AnalysisWorldV1 {
        schema: WORLD_SCHEMA_VERSION,
        input_fingerprint,
        owners,
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
