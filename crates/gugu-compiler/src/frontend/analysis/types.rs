//! 抽象域与可序列化的 world 结果。
//!
//! 阶段 24 起，摘要记录与 query 身份键使用 `MonoKey` 规范字节；world-local
//! 证明键仍为 `(owner 表下标, ExprId)`（不跨 query 持久化）。

use crate::frontend::hir::{CheckKind, ExprId};
use serde::{Deserialize, Serialize};

pub(crate) const WORLD_SCHEMA_VERSION: u32 = 4;

/// 检查的证明状态：`Proved` 表示 HIR 局部事实可证安全，`Disproved` 表示 HIR 局部
/// 事实可证必然失败，`Unknown` 表示局部事实不足、必须保留检查。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum ProofStatus {
    Proved,
    Disproved,
    Unknown,
}

/// 定义级求解身份（owner 表下标 + `DefId`）；仅驱动 SCC 求解，不进入 query key。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct AnalysisOwnerKey {
    pub owner_index: u32,
    pub definition: crate::frontend::hir::DefId,
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

/// 返回值与参数长度之间的关系。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ReturnRelation {
    EqLen { parameter: u32 },
}

/// 跨函数效果摘要。`may_*` 为真表示可能发生；LFP 从全假出发，超预算回退保守值。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct FunctionSummary {
    pub preconditions: Vec<()>,
    pub return_lo: Option<i64>,
    pub return_hi: Option<i64>,
    pub return_relations: Vec<ReturnRelation>,
    pub read_params: Vec<u32>,
    pub write_params: Vec<u32>,
    /// 无体或未知调用可访问全部实参；投影时按实际参数数量展开。
    pub unknown_param_access: bool,
    pub alias_heap: bool,
    pub alias_foreign: bool,
    pub reads_hidden_state: bool,
    pub writes_hidden_state: bool,
    pub may_allocate: bool,
    pub may_panic: bool,
    pub may_suspend: bool,
    pub may_call_unknown: bool,
    pub may_mutate_len: bool,
}

impl FunctionSummary {
    /// 保守上界：一切皆可能发生。
    pub fn conservative() -> Self {
        Self {
            preconditions: Vec::new(),
            return_lo: None,
            return_hi: None,
            return_relations: Vec::new(),
            read_params: Vec::new(),
            write_params: Vec::new(),
            unknown_param_access: true,
            alias_heap: true,
            alias_foreign: true,
            reads_hidden_state: true,
            writes_hidden_state: true,
            may_allocate: true,
            may_panic: true,
            may_suspend: true,
            may_call_unknown: true,
            may_mutate_len: true,
        }
    }

    /// 效果并集：吸收 `other` 的"可能发生"。
    pub(crate) fn join_with(&mut self, other: &Self) {
        self.return_lo = match (self.return_lo, other.return_lo) {
            (Some(a), Some(b)) => Some(a.min(b)),
            _ => None,
        };
        self.return_hi = match (self.return_hi, other.return_hi) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        if self.return_relations != other.return_relations {
            self.return_relations.clear();
        }
        self.read_params.extend_from_slice(&other.read_params);
        self.read_params.sort_unstable();
        self.read_params.dedup();
        self.write_params.extend_from_slice(&other.write_params);
        self.write_params.sort_unstable();
        self.write_params.dedup();
        self.unknown_param_access |= other.unknown_param_access;
        self.alias_heap |= other.alias_heap;
        self.alias_foreign |= other.alias_foreign;
        self.reads_hidden_state |= other.reads_hidden_state;
        self.writes_hidden_state |= other.writes_hidden_state;
        self.may_allocate |= other.may_allocate;
        self.may_panic |= other.may_panic;
        self.may_suspend |= other.may_suspend;
        self.may_call_unknown |= other.may_call_unknown;
        self.may_mutate_len |= other.may_mutate_len;
    }
}

/// 单态化实例摘要投影；`mono_key` 为规范字节，不含 session-local 编号。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct InstanceSummaryRecord {
    pub mono_key: Vec<u8>,
    pub summary: FunctionSummary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SccSummaryV1 {
    pub instances: Vec<InstanceSummaryRecord>,
    pub proofs: Vec<ProofFact>,
    pub budget_exhausted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AnalysisWorldV1 {
    pub schema: u32,
    pub input_fingerprint: [u8; 32],
    pub instances: Vec<InstanceSummaryRecord>,
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
