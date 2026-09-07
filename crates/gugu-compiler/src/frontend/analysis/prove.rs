//! 在检查点当前 memory version 上判定 `proved` / `disproved` / `unknown`。

use super::domain::{AbstractState, Interval, ValueKey};
use super::transfer::{array_len, len_key, seed_array_len};
use crate::frontend::hir::{CheckKind, ExprKind, Module, Owner, RuntimeCheck};

pub(crate) fn prove_check(
    module: &Module,
    owner: &Owner,
    state: &AbstractState,
    check: &RuntimeCheck,
) -> super::types::ProofStatus {
    if !state.reachable {
        return super::types::ProofStatus::Unknown;
    }
    match &check.kind {
        CheckKind::Bounds { slice: false } => prove_index(module, owner, state, check),
        CheckKind::Bounds { slice: true } => prove_slice(module, owner, state, check),
        CheckKind::Division { divisor, .. } => prove_nonzero(state.range(ValueKey::Expr(*divisor))),
        CheckKind::Shift { amount, .. } => prove_shift(state.range(ValueKey::Expr(*amount))),
        CheckKind::UnicodeScalar { value } => prove_unicode(state.range(ValueKey::Expr(*value))),
        CheckKind::FloatToInt { .. } | CheckKind::Utf8Boundary => {
            super::types::ProofStatus::Unknown
        }
    }
}

fn prove_index(
    module: &Module,
    owner: &Owner,
    state: &AbstractState,
    check: &RuntimeCheck,
) -> super::types::ProofStatus {
    let ExprKind::Index { base, index, .. } = owner.expressions[check.expression.index()].kind
    else {
        return super::types::ProofStatus::Unknown;
    };
    let mut state = state.clone();
    seed_array_len(module, owner, &mut state, base);
    let index = state.range(ValueKey::Expr(index));
    let length = array_len(module, owner, base)
        .map(|length| Interval::point(i128::from(length)))
        .unwrap_or_else(|| state.range(len_key(owner, base)));
    compare_bounds(index, length)
}

fn prove_slice(
    module: &Module,
    owner: &Owner,
    state: &AbstractState,
    check: &RuntimeCheck,
) -> super::types::ProofStatus {
    let ExprKind::Slice { base, start, end } = owner.expressions[check.expression.index()].kind
    else {
        return super::types::ProofStatus::Unknown;
    };
    let mut state = state.clone();
    seed_array_len(module, owner, &mut state, base);
    let length = array_len(module, owner, base)
        .map(|length| Interval::point(i128::from(length)))
        .unwrap_or_else(|| state.range(len_key(owner, base)));
    let start = start
        .map(|start| state.range(ValueKey::Expr(start)))
        .unwrap_or_else(|| Interval::point(0));
    let end = end
        .map(|end| state.range(ValueKey::Expr(end)))
        .unwrap_or(length);
    let start_status = compare_bounds(start, length);
    let end_status = compare_bounds(
        end,
        Interval {
            lo: length.lo.saturating_add(1),
            hi: length.hi.saturating_add(1),
        },
    );
    match (start_status, end_status) {
        (super::types::ProofStatus::Proved, super::types::ProofStatus::Proved)
            if start.hi <= end.lo =>
        {
            super::types::ProofStatus::Proved
        }
        (super::types::ProofStatus::Disproved, _) | (_, super::types::ProofStatus::Disproved) => {
            super::types::ProofStatus::Disproved
        }
        _ => super::types::ProofStatus::Unknown,
    }
}

fn compare_bounds(index: Interval, length: Interval) -> super::types::ProofStatus {
    if index.is_empty() || length.is_empty() {
        return super::types::ProofStatus::Unknown;
    }
    if index.lo >= 0 && index.hi < length.lo {
        return super::types::ProofStatus::Proved;
    }
    if index.hi < 0 || index.lo >= length.hi && length.hi != i128::MAX {
        return super::types::ProofStatus::Disproved;
    }
    super::types::ProofStatus::Unknown
}

fn prove_nonzero(divisor: Interval) -> super::types::ProofStatus {
    if divisor.is_empty() {
        return super::types::ProofStatus::Unknown;
    }
    if !divisor.contains(0) {
        return super::types::ProofStatus::Proved;
    }
    if divisor.singleton() == Some(0) {
        return super::types::ProofStatus::Disproved;
    }
    super::types::ProofStatus::Unknown
}

fn prove_shift(amount: Interval) -> super::types::ProofStatus {
    if amount.is_empty() {
        return super::types::ProofStatus::Unknown;
    }
    if amount.lo >= 0 {
        return super::types::ProofStatus::Proved;
    }
    if amount.hi < 0 {
        return super::types::ProofStatus::Disproved;
    }
    super::types::ProofStatus::Unknown
}

fn prove_unicode(value: Interval) -> super::types::ProofStatus {
    let Some(raw) = value.singleton() else {
        return super::types::ProofStatus::Unknown;
    };
    match u32::try_from(raw).ok().and_then(char::from_u32) {
        Some(_) => super::types::ProofStatus::Proved,
        None => super::types::ProofStatus::Disproved,
    }
}
