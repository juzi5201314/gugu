//! 抽象域与可序列化的 world 结果。

use crate::frontend::hir::{CheckKind, DefId, ExprId};
use serde::{Deserialize, Serialize};

pub(crate) const WORLD_SCHEMA_VERSION: u32 = 1;

/// 检查的证明状态：`Proved` 表示 HIR 局部事实可证安全，`Disproved` 表示 HIR 局部
/// 事实可证必然失败，`Unknown` 表示局部事实不足、必须保留检查。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum ProofStatus {
    Proved,
    Disproved,
    Unknown,
}

/// 稳定 callable owner 身份（阶段 24 前用 owner 表下标 + `DefId`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct AnalysisOwnerKey {
    pub owner_index: u32,
    pub definition: DefId,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct RuntimeCheckKey {
    pub owner_index: u32,
    pub expression: ExprId,
    pub kind: CheckKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProofFact {
    pub key: RuntimeCheckKey,
    pub status: ProofStatus,
}

/// 跨函数效果摘要；布尔为真表示"可能发生"，只能从保守初值单调精化为假。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct FunctionSummary {
    pub may_panic: bool,
    pub may_call_unknown: bool,
    pub may_mutate_len: bool,
    pub reads_hidden_state: bool,
    pub writes_hidden_state: bool,
}

impl FunctionSummary {
    /// 保守起点：一切皆可能发生，固定点只能按证据把标志降为假。
    pub fn conservative() -> Self {
        Self {
            may_panic: true,
            may_call_unknown: true,
            may_mutate_len: true,
            reads_hidden_state: true,
            writes_hidden_state: true,
        }
    }

    /// 效果并集：吸收 `other` 的"可能发生"，用于固定点传播与 callee 合并。
    pub(crate) fn join_with(&mut self, other: &Self) {
        self.may_panic |= other.may_panic;
        self.may_call_unknown |= other.may_call_unknown;
        self.may_mutate_len |= other.may_mutate_len;
        self.reads_hidden_state |= other.reads_hidden_state;
        self.writes_hidden_state |= other.writes_hidden_state;
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct OwnerSummaryRecord {
    pub key: AnalysisOwnerKey,
    pub summary: FunctionSummary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AnalysisWorldV1 {
    pub schema: u32,
    pub input_fingerprint: [u8; 32],
    pub owners: Vec<OwnerSummaryRecord>,
    pub proofs: Vec<ProofFact>,
    pub budget_exhausted: bool,
    pub runtime_checks_elided_count: u32,
}

impl AnalysisWorldV1 {
    pub fn proof_status(&self, key: &RuntimeCheckKey) -> ProofStatus {
        self.proofs
            .iter()
            .find(|fact| &fact.key == key)
            .map(|fact| fact.status)
            .unwrap_or(ProofStatus::Unknown)
    }
}

pub(crate) fn sort_proofs(proofs: &mut [ProofFact]) {
    proofs.sort_by(|left, right| {
        left.key
            .owner_index
            .cmp(&right.key.owner_index)
            .then_with(|| left.key.expression.0.cmp(&right.key.expression.0))
            .then_with(|| check_kind_ord(&left.key.kind).cmp(&check_kind_ord(&right.key.kind)))
    });
}

fn check_kind_ord(kind: &CheckKind) -> u8 {
    match kind {
        CheckKind::Bounds { slice } => {
            if *slice {
                0
            } else {
                1
            }
        }
        CheckKind::Division { .. } => 2,
        CheckKind::Shift { .. } => 3,
        CheckKind::FloatToInt { .. } => 4,
        CheckKind::UnicodeScalar { .. } => 5,
        CheckKind::Utf8Boundary => 6,
    }
}
