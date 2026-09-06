//! `PublicSummaryPolicyV1` 占位与 whole-program 分析预算。

/// 公共摘要策略 revision（占位，阶段 23 不产出跨 package 对象）。
pub const PUBLIC_SUMMARY_POLICY_REVISION: u32 = 1;

/// 默认 SCC 迭代上限；超预算 → `unknown`，不报错。
const DEFAULT_MAX_SCC_ITERATIONS: u32 = 32;

/// 单 owner 程序点数量软上限（CFG 块数）。
const DEFAULT_MAX_BLOCKS_PER_OWNER: u32 = 4096;

/// 每个函数摘要中关系条目软上限。
const DEFAULT_MAX_SUMMARY_RELATIONS: u32 = 256;

/// 进入 query fingerprint 与 action key 的分析策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AnalysisPolicyV1 {
    pub analysis_semantics_revision: u32,
    pub public_policy_revision: u32,
    pub max_scc_iterations: u32,
    pub max_blocks_per_owner: u32,
    pub max_summary_relations: u32,
}

impl Default for AnalysisPolicyV1 {
    fn default() -> Self {
        Self {
            analysis_semantics_revision: super::ANALYSIS_SEMANTICS_REVISION,
            public_policy_revision: PUBLIC_SUMMARY_POLICY_REVISION,
            max_scc_iterations: DEFAULT_MAX_SCC_ITERATIONS,
            max_blocks_per_owner: DEFAULT_MAX_BLOCKS_PER_OWNER,
            max_summary_relations: DEFAULT_MAX_SUMMARY_RELATIONS,
        }
    }
}

impl AnalysisPolicyV1 {
    /// 规范编码字节，用于 fingerprint 与 action key。
    pub fn canonical_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20);
        out.extend_from_slice(&self.analysis_semantics_revision.to_le_bytes());
        out.extend_from_slice(&self.public_policy_revision.to_le_bytes());
        out.extend_from_slice(&self.max_scc_iterations.to_le_bytes());
        out.extend_from_slice(&self.max_blocks_per_owner.to_le_bytes());
        out.extend_from_slice(&self.max_summary_relations.to_le_bytes());
        out
    }
}
