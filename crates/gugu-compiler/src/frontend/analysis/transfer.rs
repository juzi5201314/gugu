//! 指令与终结符的抽象状态转移、assume 与长度/别名失效。

use super::callgraph;
use super::domain::{AbstractState, AliasClass, Interval, Relation, ValueKey};
use super::types::FunctionSummary;
use crate::frontend::ast::{BinOp, UnOp};
use crate::frontend::hir::{
    self, CallTarget, ExprId, ExprKind, Literal, LocalId, Module, Owner, Res, Type,
};

pub(crate) fn eval_expr(
    module: &Module,
    owner: &Owner,
    state: &mut AbstractState,
    id: ExprId,
    callees: &dyn Fn(hir::DefId) -> FunctionSummary,
) {
    if !state.reachable {
        return;
    }
    let kind = &owner.expressions[id.index()].kind;
    match kind {
        ExprKind::Literal(Literal::Integer(value)) => {
            let signed = i128::try_from(*value).unwrap_or(i128::MAX);
            state.set_range(ValueKey::Expr(id), Interval::point(signed));
        }
        ExprKind::Literal(Literal::Bool(true)) => {
            state.set_range(ValueKey::Expr(id), Interval::point(1));
        }
        ExprKind::Literal(Literal::Bool(false)) => {
            state.set_range(ValueKey::Expr(id), Interval::point(0));
        }
        ExprKind::Resolved(Res::Local(local)) => copy_local(module, owner, state, id, *local),
        ExprKind::Resolved(Res::Def(definition)) => {
            if matches!(
                module.definitions[definition.index()].kind,
                hir::DefinitionKind::Static | hir::DefinitionKind::LocalStatic
            ) {
                for alias in &mut state.alias {
                    if matches!(*alias, AliasClass::Heap) {
                        *alias = AliasClass::Static(definition.0);
                    }
                }
                state.effects.resource_publish = true;
            }
        }
        ExprKind::Unary {
            operation: UnOp::Neg,
            value,
        } => state.set_range(
            ValueKey::Expr(id),
            state.range(ValueKey::Expr(*value)).neg(),
        ),
        ExprKind::Binary {
            operation,
            left,
            right,
            ..
        } => eval_binary(state, id, *operation, *left, *right),
        ExprKind::Range { start, end } => {
            state.set_range(ValueKey::Expr(id), Interval::UNKNOWN);
            let _ = (start, end);
        }
        ExprKind::Intrinsic {
            operation: hir::Builtin::Len,
            arguments,
            ..
        } => {
            let receiver = owner.expression_ids[arguments.start as usize];
            let key = len_key(owner, receiver);
            seed_array_len(module, owner, state, receiver);
            state.set_range(ValueKey::Expr(id), state.range(key));
            state.relate(Relation {
                left: ValueKey::Expr(id),
                right: key,
                offset: 0,
            });
            state.relate(Relation {
                left: key,
                right: ValueKey::Expr(id),
                offset: 0,
            });
        }
        ExprKind::Call { target, .. } | ExprKind::SpawnCall { target, .. } => {
            apply_call(owner, state, id, target, callees);
            if matches!(kind, ExprKind::SpawnCall { .. }) {
                state.effects.suspend = true;
                state.bump_heap();
            }
        }
        ExprKind::Index { base, .. } => {
            seed_array_len(module, owner, state, *base);
            state.set_range(ValueKey::Expr(id), Interval::UNKNOWN);
            apply_effects(owner, state, id);
        }
        ExprKind::If {
            then_value,
            else_value,
            ..
        } => {
            let mut interval = state.range(ValueKey::Expr(*then_value));
            if let Some(else_value) = else_value {
                interval = interval.join(state.range(ValueKey::Expr(*else_value)));
            }
            state.set_range(ValueKey::Expr(id), interval);
        }
        ExprKind::Block { tail, .. } => {
            if let Some(tail) = tail {
                state.set_range(ValueKey::Expr(id), state.range(ValueKey::Expr(*tail)));
            }
        }
        ExprKind::Assembly(_) => {
            state.effects.foreign = true;
            state.bump_foreign();
        }
        _ => {
            apply_effects(owner, state, id);
            if is_integer_ty(module, owner, id) {
                // 保持未知，不覆盖已有收窄。
                if state.range(ValueKey::Expr(id)).is_empty() {
                    state.set_range(ValueKey::Expr(id), Interval::UNKNOWN);
                }
            }
        }
    }
}

fn copy_local(
    module: &Module,
    owner: &Owner,
    state: &mut AbstractState,
    id: ExprId,
    local: LocalId,
) {
    state.set_range(ValueKey::Expr(id), state.range(ValueKey::Local(local)));
    state.relate(Relation {
        left: ValueKey::Expr(id),
        right: ValueKey::Local(local),
        offset: 0,
    });
    state.relate(Relation {
        left: ValueKey::Local(local),
        right: ValueKey::Expr(id),
        offset: 0,
    });
    seed_array_len(module, owner, state, id);
    let len = state.range(ValueKey::LocalLen(local));
    state.set_range(ValueKey::ExprLen(id), len);
}

fn eval_binary(state: &mut AbstractState, id: ExprId, op: BinOp, left: ExprId, right: ExprId) {
    let lhs = state.range(ValueKey::Expr(left));
    let rhs = state.range(ValueKey::Expr(right));
    let interval = match op {
        BinOp::Add => lhs.add(rhs),
        BinOp::Sub => lhs.sub(rhs),
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            compare_interval(op, lhs, rhs)
        }
        BinOp::And | BinOp::Or => Interval::UNKNOWN,
        _ => Interval::UNKNOWN,
    };
    state.set_range(ValueKey::Expr(id), interval);
}

fn compare_interval(op: BinOp, lhs: Interval, rhs: Interval) -> Interval {
    if lhs.is_empty() || rhs.is_empty() {
        return Interval::EMPTY;
    }
    match op {
        BinOp::Lt if lhs.hi < rhs.lo => Interval::point(1),
        BinOp::Lt if lhs.lo >= rhs.hi => Interval::point(0),
        BinOp::Le if lhs.hi <= rhs.lo => Interval::point(1),
        BinOp::Le if lhs.lo > rhs.hi => Interval::point(0),
        BinOp::Gt if lhs.lo > rhs.hi => Interval::point(1),
        BinOp::Gt if lhs.hi <= rhs.lo => Interval::point(0),
        BinOp::Ge if lhs.lo >= rhs.hi => Interval::point(1),
        BinOp::Ge if lhs.hi < rhs.lo => Interval::point(0),
        BinOp::Eq if lhs.singleton().is_some() && lhs.singleton() == rhs.singleton() => {
            Interval::point(1)
        }
        BinOp::Eq if lhs.hi < rhs.lo || rhs.hi < lhs.lo => Interval::point(0),
        BinOp::Ne if lhs.singleton().is_some() && lhs.singleton() == rhs.singleton() => {
            Interval::point(0)
        }
        BinOp::Ne if lhs.hi < rhs.lo || rhs.hi < lhs.lo => Interval::point(1),
        _ => Interval { lo: 0, hi: 1 },
    }
}

pub(crate) fn assume(state: &mut AbstractState, cond: ExprId, value: bool, owner: &Owner) {
    if !state.reachable {
        return;
    }
    let interval = state.range(ValueKey::Expr(cond));
    if value && interval.hi < 1 {
        state.reachable = false;
        return;
    }
    if !value && interval.lo > 0 {
        state.reachable = false;
        return;
    }
    if let ExprKind::Binary {
        operation,
        left,
        right,
        ..
    } = &owner.expressions[cond.index()].kind
    {
        assume_compare(state, *operation, *left, *right, value);
    }
}

fn assume_compare(state: &mut AbstractState, op: BinOp, left: ExprId, right: ExprId, truth: bool) {
    let op = if truth { op } else { negate(op) };
    let mut lhs = state.range(ValueKey::Expr(left));
    let mut rhs = state.range(ValueKey::Expr(right));
    match op {
        BinOp::Lt => {
            lhs.hi = lhs.hi.min(rhs.hi.saturating_sub(1));
            rhs.lo = rhs.lo.max(lhs.lo.saturating_add(1));
            state.relate(Relation {
                left: ValueKey::Expr(left),
                right: ValueKey::Expr(right),
                offset: -1,
            });
        }
        BinOp::Le => {
            lhs.hi = lhs.hi.min(rhs.hi);
            rhs.lo = rhs.lo.max(lhs.lo);
            state.relate(Relation {
                left: ValueKey::Expr(left),
                right: ValueKey::Expr(right),
                offset: 0,
            });
        }
        BinOp::Gt => assume_compare(state, BinOp::Lt, right, left, true),
        BinOp::Ge => assume_compare(state, BinOp::Le, right, left, true),
        BinOp::Eq => {
            let meet = lhs.meet(rhs);
            lhs = meet;
            rhs = meet;
        }
        _ => return,
    }
    state.set_range(ValueKey::Expr(left), lhs);
    state.set_range(ValueKey::Expr(right), rhs);
    propagate_len(state, left, lhs);
    propagate_len(state, right, rhs);
}

fn negate(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Ge,
        BinOp::Le => BinOp::Gt,
        BinOp::Gt => BinOp::Le,
        BinOp::Ge => BinOp::Lt,
        BinOp::Eq => BinOp::Ne,
        BinOp::Ne => BinOp::Eq,
        other => other,
    }
}

fn propagate_len(state: &mut AbstractState, expr: ExprId, interval: Interval) {
    for relation in state.relations.clone() {
        if relation.offset != 0 {
            continue;
        }
        if relation.left == ValueKey::Expr(expr) {
            let current = state.range(relation.right);
            state.set_range(relation.right, current.meet(interval));
        }
        if relation.right == ValueKey::Expr(expr) {
            let current = state.range(relation.left);
            state.set_range(relation.left, current.meet(interval));
        }
    }
}

pub(crate) fn bind(state: &mut AbstractState, local: LocalId, value: ExprId) {
    let interval = state.range(ValueKey::Expr(value));
    state.set_range(ValueKey::Local(local), interval);
    state.set_range(
        ValueKey::LocalLen(local),
        state.range(ValueKey::ExprLen(value)),
    );
    if local.index() < state.init.len() {
        state.init[local.index()] = true;
    }
}

pub(crate) fn increment(state: &mut AbstractState, local: LocalId) {
    let next = state.range(ValueKey::Local(local)).add(Interval::point(1));
    state.set_range(ValueKey::Local(local), next);
}

pub(crate) fn assign(owner: &Owner, state: &mut AbstractState, place: ExprId, value: ExprId) {
    if let Some(local) = place_local(owner, place) {
        bind(state, local, value);
        state.bump_local(local);
        bind(state, local, value);
    } else {
        state.bump_heap();
    }
    let ty = &owner.expressions[place.index()];
    if ty.effects.0 & hir::Effects::WRITE != 0 {
        state.effects.cow_seal |= matches_string_write(owner, place);
    }
}

pub(crate) fn assume_iv(state: &mut AbstractState, local: LocalId, end: ExprId, take_body: bool) {
    let mut iv = state.range(ValueKey::Local(local));
    let bound = state.range(ValueKey::Expr(end));
    if take_body {
        iv.hi = iv.hi.min(bound.hi.saturating_sub(1));
        if iv.lo >= bound.lo && bound.lo != i128::MIN && iv.lo >= bound.lo {
            // 仍可能进入：i < end 用 end.lo 保守下界不够，保留 i.lo。
        }
        state.relate(Relation {
            left: ValueKey::Local(local),
            right: ValueKey::Expr(end),
            offset: -1,
        });
    } else {
        iv.lo = iv.lo.max(bound.lo);
    }
    if iv.is_empty() {
        state.reachable = false;
        return;
    }
    state.set_range(ValueKey::Local(local), iv);
}

fn apply_call(
    owner: &Owner,
    state: &mut AbstractState,
    id: ExprId,
    target: &CallTarget,
    callees: &dyn Fn(hir::DefId) -> FunctionSummary,
) {
    let summary = match callgraph::callee_definition(owner, target) {
        Some(definition) => callees(definition),
        None => match target {
            CallTarget::Builtin(_) => FunctionSummary::default(),
            _ => FunctionSummary::conservative(),
        },
    };
    state.effects.panic |= summary.may_panic;
    state.effects.allocate |= summary.may_allocate;
    state.effects.suspend |= summary.may_suspend;
    state.effects.foreign |= summary.alias_foreign;
    if summary.may_call_unknown
        || summary.alias_foreign
        || summary.alias_heap
        || summary.may_mutate_len
    {
        state.bump_heap();
    }
    if summary.may_mutate_len {
        state.effects.resource_publish = true;
    }
    apply_effects(owner, state, id);
}

fn apply_effects(owner: &Owner, state: &mut AbstractState, id: ExprId) {
    let bits = owner.expressions[id.index()].effects.0;
    if bits & hir::Effects::PANIC != 0 {
        state.effects.panic = true;
    }
    if bits & hir::Effects::ALLOCATE != 0 {
        state.effects.allocate = true;
    }
    if bits & hir::Effects::SUSPEND != 0 {
        state.effects.suspend = true;
        state.bump_heap();
    }
    if bits & hir::Effects::FOREIGN != 0 {
        state.bump_foreign();
    }
    if bits & hir::Effects::WRITE != 0 {
        state.bump_heap();
    }
}

pub(crate) fn seed_array_len(
    module: &Module,
    owner: &Owner,
    state: &mut AbstractState,
    id: ExprId,
) {
    if let Some(length) = array_len(module, owner, id) {
        let interval = Interval::point(i128::from(length));
        state.set_range(len_key(owner, id), interval);
        state.set_range(ValueKey::ExprLen(id), interval);
        if let Some(local) = place_local(owner, id) {
            state.set_range(ValueKey::LocalLen(local), interval);
        }
    }
}

pub(crate) fn array_len(module: &Module, owner: &Owner, id: ExprId) -> Option<u64> {
    let mut ty = &module.types[owner.expression_types[id.index()].index()];
    if let Type::Ref(inner) = ty {
        ty = &module.types[inner.index()];
    }
    match ty {
        Type::Array(_, length) => Some(*length),
        _ => None,
    }
}

pub(crate) fn len_key(owner: &Owner, id: ExprId) -> ValueKey {
    match place_local(owner, id) {
        Some(local) => ValueKey::LocalLen(local),
        None => ValueKey::ExprLen(id),
    }
}

pub(crate) fn place_local(owner: &Owner, id: ExprId) -> Option<LocalId> {
    match &owner.expressions[id.index()].kind {
        ExprKind::Resolved(Res::Local(local)) => Some(*local),
        ExprKind::Field { base, .. }
        | ExprKind::Unary {
            operation: UnOp::Deref,
            value: base,
        } => place_local(owner, *base),
        _ => None,
    }
}

fn matches_string_write(owner: &Owner, place: ExprId) -> bool {
    let _ = (owner, place);
    true
}

fn is_integer_ty(module: &Module, owner: &Owner, id: ExprId) -> bool {
    matches!(
        module.types[owner.expression_types[id.index()].index()],
        Type::Int { .. }
    )
}
