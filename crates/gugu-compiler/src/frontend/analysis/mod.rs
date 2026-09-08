//! AbstractAnalysis：冻结前 HIR 上的 CFG 固定点、范围证明与跨函数摘要。
//!
//! 证明消费 HIR 程序点上的 AbstractState；跨 owner 摘要按调用图 SCC 求解，
//! 超预算时回退保守值并保留全部检查。
mod access;
pub(crate) mod callgraph;
pub(crate) mod cfg;
mod domain;
mod interpret;
pub(crate) mod policy;
mod prove;
pub(crate) mod query;
pub(crate) mod solver;
mod transfer;
mod types;

pub(crate) use policy::AnalysisPolicyV1;
pub(crate) use query::run_world;
#[cfg(test)]
pub(crate) use types::RuntimeCheckKey;
pub(crate) use types::{
    AnalysisWorldV1, FunctionSummary, ProofStatus, ReturnRelation, WORLD_SCHEMA_VERSION,
};

pub(crate) const ANALYSIS_SEMANTICS_REVISION: u32 = 4;

pub(crate) fn empty_world() -> AnalysisWorldV1 {
    AnalysisWorldV1 {
        schema: WORLD_SCHEMA_VERSION,
        input_fingerprint: [0; 32],
        instances: Vec::new(),
        proofs: Vec::new(),
        budget_exhausted: false,
        runtime_checks_elided_count: 0,
    }
}

pub(crate) fn patch_module(module: &mut crate::frontend::hir::Module, world: &AnalysisWorldV1) {
    solver::patch_proofs(module, world);
}

#[cfg(test)]
mod tests;
