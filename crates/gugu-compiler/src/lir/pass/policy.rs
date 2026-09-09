//! 固定优化管线的策略常量与规范编码。
//!
//! 这些值决定 pass 行为与 action key 身份；运行时不得重排或改写。
use serde::{Deserialize, Serialize};

/// 任意 poll-free 路径允许累积的最大成本。
pub(crate) const POLL_BUDGET: u32 = 4096;
/// 允许被分类为 poll-free 叶调用的最大成本。
pub(crate) const POLL_FREE_LEAF_MAX_COST: u32 = 64;
/// LIR 固定管线顺序的 revision。
pub(crate) const PASS_PIPELINE_REVISION: u32 = 1;
/// poll 成本表的 revision。
pub(crate) const POLL_COST_REVISION: u32 = 1;
/// 内联策略的 revision。
pub(crate) const INLINE_POLICY_REVISION: u32 = 1;
/// 向量化策略的 revision。
pub(crate) const VECTOR_POLICY_REVISION: u32 = 1;
/// strip mining 策略的 revision。
pub(crate) const STRIP_MINING_REVISION: u32 = 1;

/// 进入 action key 的 LIR 优化策略快照。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct OptimizationPolicyV1 {
    pub(crate) pass_pipeline_revision: u32,
    pub(crate) poll_budget: u32,
    pub(crate) poll_cost_revision: u32,
    pub(crate) inline_policy_revision: u32,
    pub(crate) vector_policy_revision: u32,
    pub(crate) strip_mining_revision: u32,
}

impl Default for OptimizationPolicyV1 {
    fn default() -> Self {
        Self {
            pass_pipeline_revision: PASS_PIPELINE_REVISION,
            poll_budget: POLL_BUDGET,
            poll_cost_revision: POLL_COST_REVISION,
            inline_policy_revision: INLINE_POLICY_REVISION,
            vector_policy_revision: VECTOR_POLICY_REVISION,
            strip_mining_revision: STRIP_MINING_REVISION,
        }
    }
}

impl OptimizationPolicyV1 {
    /// 返回字段顺序固定的规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("优化策略可序列化")
    }
}
