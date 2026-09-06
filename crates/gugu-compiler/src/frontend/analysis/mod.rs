//! AbstractAnalysis：HIR 上的范围证明与函数摘要固定点。
//!
//! 热路径 place→version 用稠密 `Vec<u32>`（上界为单 owner 槽数）；跨 owner 摘要用
//! `BTreeMap` 保证确定性序。SCC 成员用稳定排序的 `Vec`。
pub(crate) mod policy;
pub(crate) mod query;
pub(crate) mod solver;
mod types;

pub(crate) use policy::{AnalysisPolicyV1, PUBLIC_SUMMARY_POLICY_REVISION};
pub(crate) use query::run_world;
pub(crate) use types::{
    AnalysisOwnerKey, AnalysisWorldV1, FunctionSummary, ProofFact, ProofStatus, RuntimeCheckKey,
    WORLD_SCHEMA_VERSION,
};

pub(crate) const ANALYSIS_SEMANTICS_REVISION: u32 = 1;

pub(crate) fn empty_world() -> AnalysisWorldV1 {
    AnalysisWorldV1 {
        schema: WORLD_SCHEMA_VERSION,
        input_fingerprint: [0; 32],
        owners: Vec::new(),
        proofs: Vec::new(),
        budget_exhausted: false,
        runtime_checks_elided_count: 0,
    }
}
pub(crate) fn patch_module(module: &mut crate::frontend::hir::Module, world: &AnalysisWorldV1) {
    solver::patch_proofs(module, world);
}

pub(crate) fn proved_bounds_count(world: &AnalysisWorldV1) -> u32 {
    world
        .proofs
        .iter()
        .filter(|fact| {
            fact.status == ProofStatus::Proved
                && matches!(
                    fact.key.kind,
                    crate::frontend::hir::CheckKind::Bounds { .. }
                )
        })
        .count() as u32
}

#[cfg(test)]
mod tests;
