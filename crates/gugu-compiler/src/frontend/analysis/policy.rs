//! whole-program 分析预算与统一公共摘要策略。

use crate::frontend::mono::summary::PUBLIC_POLICY_REVISION;

/// 摘要固定点的 SCC 迭代上限；超限置 `budget_exhausted`，摘要保持保守值。
const DEFAULT_MAX_SCC_ITERATIONS: u32 = 32;

/// 过程内 CFG 块迭代上限；超限回退保守并保留检查。
const DEFAULT_MAX_BLOCK_ITERATIONS: u32 = 256;

/// 进入 query fingerprint 与 action key 的分析策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AnalysisPolicyV1 {
    pub analysis_semantics_revision: u32,
    pub public_policy_revision: u32,
    pub max_scc_iterations: u32,
    pub max_block_iterations: u32,
}

impl Default for AnalysisPolicyV1 {
    fn default() -> Self {
        Self {
            analysis_semantics_revision: super::ANALYSIS_SEMANTICS_REVISION,
            public_policy_revision: PUBLIC_POLICY_REVISION,
            max_scc_iterations: DEFAULT_MAX_SCC_ITERATIONS,
            max_block_iterations: DEFAULT_MAX_BLOCK_ITERATIONS,
        }
    }
}

impl AnalysisPolicyV1 {
    /// 规范编码字节，用于 fingerprint 与 action key。
    pub fn canonical_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&self.analysis_semantics_revision.to_le_bytes());
        out.extend_from_slice(&self.public_policy_revision.to_le_bytes());
        out.extend_from_slice(&self.max_scc_iterations.to_le_bytes());
        out.extend_from_slice(&self.max_block_iterations.to_le_bytes());
        out
    }
}
