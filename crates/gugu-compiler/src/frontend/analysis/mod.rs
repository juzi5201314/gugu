//! AbstractAnalysis：generic GIR 上的 CFG 固定点、范围证明与跨函数摘要。
//!
//! 证明消费 GIR 程序点上的 AbstractState；跨 owner 摘要按调用图 SCC 求解，
//! 超预算时回退保守值并保留全部检查。证明只写入 `AnalysisWorldV1.proofs`。
mod access;
pub(crate) mod callgraph;
mod domain;
mod interpret;
pub(crate) mod policy;
mod prove;
pub(crate) mod query;
pub(crate) mod solver;
mod transfer;
mod transfer_gir;
mod types;

pub(crate) use policy::AnalysisPolicyV1;
pub(crate) use query::run_world;
#[allow(unused_imports, reason = "测试与下游 crate 模块经此根导出证明类型")]
pub(crate) use types::{
    AnalysisWorldV1, FunctionSummary, ProofStatus, ReturnRelation, RuntimeCheckKey,
    WORLD_SCHEMA_VERSION,
};

pub(crate) const ANALYSIS_SEMANTICS_REVISION: u32 = 5;

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

#[cfg(test)]
mod tests;
