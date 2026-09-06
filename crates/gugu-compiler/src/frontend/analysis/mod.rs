//! AbstractAnalysis：冻结前 HIR 上的局部证明与跨函数摘要固定点。
//!
//! 证明只依赖 HIR 自身的字面量与类型事实；跨 owner 摘要按效果并集传播，
//! 从保守初值单调精化，超预算时回退保守值并保留全部检查。
pub(crate) mod policy;
pub(crate) mod query;
pub(crate) mod solver;
mod types;

pub(crate) use policy::AnalysisPolicyV1;
pub(crate) use query::run_world;
pub(crate) use types::{AnalysisWorldV1, ProofStatus, WORLD_SCHEMA_VERSION};
#[cfg(test)]
pub(crate) use types::{FunctionSummary, RuntimeCheckKey};

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

#[cfg(test)]
mod tests;
