//! 以具体实例为固定点成员；共享 HIR 上的证明取全部可达实例的共同结论。

use super::interpret;
use super::policy::AnalysisPolicyV1;
use super::types::{
    AnalysisOwnerKey, AnalysisWorldV1, FunctionSummary, InstanceSummaryRecord, ProofFact,
    RuntimeCheckKey, SccSummaryV1, WORLD_SCHEMA_VERSION, sort_proofs,
};
use crate::frontend::hir::Module;
use crate::frontend::mono::instantiate::CallSite;

/// 一个实例 SCC 的求解成员：实例 key 与其定义的 owner 身份。
#[derive(Clone)]
pub(crate) struct SccMember {
    pub mono_key: Vec<u8>,
    pub owner: AnalysisOwnerKey,
    pub calls: Vec<(CallSite, usize)>,
}

pub(crate) fn analyze_scc(
    module: &Module,
    members: &[SccMember],
    component: &[usize],
    policy: AnalysisPolicyV1,
    callees: &dyn Fn(usize) -> FunctionSummary,
) -> SccSummaryV1 {
    let mut summaries = vec![FunctionSummary::default(); component.len()];
    let mut budget_exhausted = false;
    let mut converged = false;
    for _ in 0..policy.max_scc_iterations {
        let mut changed = false;
        for (position, &index) in component.iter().enumerate() {
            let result = analyze_member(
                module,
                &members[index],
                component,
                &summaries,
                policy,
                callees,
            );
            budget_exhausted |= result.budget_exhausted;
            let mut next = summaries[position].clone();
            next.join_with(&result.summary);
            changed |= next != summaries[position];
            summaries[position] = next;
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
        summaries.fill(FunctionSummary::conservative());
    }
    let mut proofs = Vec::new();
    for &index in component {
        let member = &members[index];
        let result = analyze_member(module, member, component, &summaries, policy, callees);
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

fn analyze_member(
    module: &Module,
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
    interpret::analyze_owner(
        module,
        &module.owners[usize::try_from(member.owner.owner_index).expect("owner 编号适配宿主")],
        member.owner.owner_index,
        policy,
        &lookup,
    )
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
    proofs.dedup_by(|candidate, retained| {
        if candidate.key != retained.key {
            return false;
        }
        if candidate.status != retained.status {
            retained.status = super::types::ProofStatus::Unknown;
        }
        true
    });
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
