//! 抽象域与可序列化的 world 结果。

use crate::frontend::hir::{CheckKind, DefId, ExprId};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

pub(crate) const WORLD_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum ProofStatus {
    Proved,
    Disproved,
    Unknown,
}

/// 有符号整数范围；`min > max` 表示空；两端均为 `None` 表示 Unknown。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct IntRange {
    pub min: Option<i128>,
    pub max: Option<i128>,
}

impl IntRange {
    pub const UNKNOWN: Self = Self {
        min: None,
        max: None,
    };

    pub fn point(value: i128) -> Self {
        Self {
            min: Some(value),
            max: Some(value),
        }
    }

    pub fn at_least(min: i128) -> Self {
        Self {
            min: Some(min),
            max: None,
        }
    }

    pub fn less_than(max: i128) -> Self {
        Self {
            min: None,
            max: Some(max - 1),
        }
    }

    pub fn intersect(self, other: Self) -> Self {
        let min = match (self.min, other.min) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let max = match (self.max, other.max) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        if let (Some(lo), Some(hi)) = (min, max)
            && lo > hi
        {
            return Self {
                min: Some(1),
                max: Some(0),
            };
        }
        Self { min, max }
    }

    pub fn union_widen(self, other: Self) -> Self {
        if self == Self::UNKNOWN || other == Self::UNKNOWN {
            return Self::UNKNOWN;
        }
        let min = match (self.min, other.min) {
            (Some(a), Some(b)) => Some(a.min(b)),
            _ => None,
        };
        let max = match (self.max, other.max) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        Self { min, max }
    }

    pub fn is_empty(self) -> bool {
        matches!((self.min, self.max), (Some(lo), Some(hi)) if lo > hi)
    }
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct FunctionSummary {
    pub may_panic: bool,
    pub may_call_unknown: bool,
    pub may_mutate_len: bool,
    pub reads_hidden_state: bool,
    pub writes_hidden_state: bool,
}

impl FunctionSummary {
    pub fn conservative() -> Self {
        Self {
            may_panic: true,
            may_call_unknown: true,
            may_mutate_len: true,
            reads_hidden_state: true,
            writes_hidden_state: true,
        }
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
