//! 指令与终结符的抽象状态转移、assume 与长度/别名失效。

use super::domain::{AbstractState, AliasClass, Interval, Relation, ValueKey};
use super::types::FunctionSummary;
use crate::frontend::ast::{BinOp, UnOp};
use crate::frontend::hir::{
    self, CallTarget, ExprId, ExprKind, Literal, LocalId, Module, Owner, Res, Type,
};
use crate::frontend::mono::instantiate::CallSite;

pub(crate) fn eval_expr(
    module: &Module,
    owner: &Owner,
    state: &mut AbstractState,
    id: ExprId,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) {
    if !state.reachable {
        return;
    }
    // 同一 HIR 表达式可在循环中再次执行；旧求值的范围与等式不能沿用。
    state.set_range(ValueKey::Expr(id), Interval::UNKNOWN);
    state.set_range(ValueKey::ExprLen(id), Interval::UNKNOWN);
    state.relations.retain(|relation| {
        !matches!(relation.left, ValueKey::Expr(expr) | ValueKey::ExprLen(expr) if expr == id)
            && !matches!(relation.right, ValueKey::Expr(expr) | ValueKey::ExprLen(expr) if expr == id)
    });
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
                state.effects.reads_hidden = true;
            }
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
            dispatch: Some(dispatch),
            ..
        } => {
            apply_dispatch(owner, state, *dispatch, callees);
            state.set_range(ValueKey::Expr(id), Interval::UNKNOWN);
            if callees(CallSite::Dispatch(*dispatch)).is_some() {
                apply_local_effects(module, owner, state, id);
            } else {
                apply_effects(owner, state, id);
            }
        }
        ExprKind::Binary {
            operation,
            left,
            right,
            dispatch: None,
        } => {
            eval_binary(state, id, *operation, *left, *right);
        }
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
            apply_call(module, owner, state, id, target, callees);
            if matches!(kind, ExprKind::SpawnCall { .. }) {
                state.effects.suspend = true;
                state.effects.allocate = true;
                state.bump_heap();
            }
        }
        ExprKind::Index {
            base, read, write, ..
        } => {
            for dispatch in read.iter().chain(write) {
                apply_dispatch(owner, state, *dispatch, callees);
            }
            seed_array_len(module, owner, state, *base);
            state.set_range(ValueKey::Expr(id), Interval::UNKNOWN);
            apply_effects(owner, state, id);
        }
        ExprKind::Try { from_value, .. } => {
            if let Some(dispatch) = from_value {
                apply_dispatch(owner, state, *dispatch, callees);
            }
            apply_effects(owner, state, id);
        }
        ExprKind::String { parts } => {
            for part in &owner.string_parts[usize::try_from(parts.start)
                .expect("HIR range 适配宿主")
                ..usize::try_from(parts.end).expect("HIR range 适配宿主")]
            {
                if let hir::StringPart::Value {
                    dispatch: Some(dispatch),
                    ..
                } = part
                {
                    apply_dispatch(owner, state, *dispatch, callees);
                }
            }
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
    let range = integer_range(module, owner, id, state.range(ValueKey::Expr(id)));
    state.set_range(ValueKey::Expr(id), range);
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

pub(crate) fn assign(
    module: &Module,
    owner: &Owner,
    state: &mut AbstractState,
    place: ExprId,
    value: ExprId,
    operation: crate::frontend::ast::AssignOp,
) {
    if let ExprKind::Resolved(Res::Local(local)) = owner.expressions[place.index()].kind {
        state.bump_local(local);
        use crate::frontend::ast::AssignOp;
        let previous = state.range(ValueKey::Expr(place));
        let rhs = state.range(ValueKey::Expr(value));
        let range = match operation {
            AssignOp::Assign => rhs,
            AssignOp::Add => previous.add(rhs),
            AssignOp::Sub => previous.sub(rhs),
            _ => Interval::UNKNOWN,
        };
        bind(state, local, value);
        state.set_range(
            ValueKey::Local(local),
            integer_range(module, owner, place, range),
        );
    } else {
        state.effects.alias_heap = true;
        state.effects.resource_publish = true;
        state.effects.writes_hidden = true;
        state.bump_heap();
    }
    let base = match owner.expressions[place.index()].kind {
        ExprKind::Index { base, .. } => base,
        _ => place,
    };
    state.effects.cow_seal |= matches!(
        module.types[owner.expression_types[base.index()].index()],
        Type::String
    );
    if let ExprKind::Resolved(Res::Def(definition)) = owner.expressions[place.index()].kind
        && matches!(
            module.definitions[definition.index()].kind,
            hir::DefinitionKind::Static | hir::DefinitionKind::LocalStatic
        )
    {
        state.effects.writes_hidden = true;
    }
    if let Some(local) = place_local(owner, place)
        && owner.captures.iter().any(|capture| capture.local == local)
    {
        state.effects.writes_hidden = true;
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
    module: &Module,
    owner: &Owner,
    state: &mut AbstractState,
    id: ExprId,
    target: &CallTarget,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) {
    if let Some(summary) = callees(CallSite::Expression(id.0)) {
        apply_summary(state, &summary);
        // HIR 的调用效果是类型检查期上界；闭合后由选中实例取代，不再重复并入旧上界。
        apply_local_effects(module, owner, state, id);
        if matches!(owner.expressions[id.index()].kind, ExprKind::Call { .. }) {
            state.set_range(
                ValueKey::Expr(id),
                Interval {
                    lo: summary.return_lo.map(i128::from).unwrap_or(i128::MIN),
                    hi: summary.return_hi.map(i128::from).unwrap_or(i128::MAX),
                },
            );
            apply_return_relations(module, owner, state, id, &summary);
        }
    } else {
        if !matches!(
            target,
            CallTarget::Builtin(_) | CallTarget::Constructor { .. }
        ) {
            apply_summary(state, &FunctionSummary::conservative());
        }
        apply_effects(owner, state, id);
    }
}

fn apply_return_relations(
    module: &Module,
    owner: &Owner,
    state: &mut AbstractState,
    id: ExprId,
    summary: &FunctionSummary,
) {
    if summary.may_call_unknown
        || summary.may_suspend
        || summary.may_mutate_len
        || summary.writes_hidden_state
        || summary.unknown_param_access
        || !summary.write_params.is_empty()
    {
        return;
    }
    let ExprKind::Call {
        receiver,
        ref arguments,
        ..
    } = owner.expressions[id.index()].kind
    else {
        return;
    };
    for relation in &summary.return_relations {
        let super::types::ReturnRelation::EqLen { parameter } = *relation;
        let argument = receiver
            .into_iter()
            .chain(
                owner.expression_ids[usize::try_from(arguments.start).expect("参数列表起点可表示")
                    ..usize::try_from(arguments.end).expect("参数列表终点可表示")]
                    .iter()
                    .copied(),
            )
            .nth(usize::try_from(parameter).expect("参数编号可表示"));
        let Some(argument) = argument else { continue };
        seed_array_len(module, owner, state, argument);
        let key = len_key(owner, argument);
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
}

fn apply_local_effects(module: &Module, owner: &Owner, state: &mut AbstractState, id: ExprId) {
    let adjustments = &owner.expressions[id.index()].adjustments;
    for adjustment in &owner.adjustments[usize::try_from(adjustments.start)
        .expect("HIR range 适配宿主")
        ..usize::try_from(adjustments.end).expect("HIR range 适配宿主")]
    {
        if let hir::Adjustment::Erase(ty) = adjustment
            && matches!(module.types[ty.index()], Type::Dyn(_))
        {
            state.effects.allocate = true;
        }
    }
    if owner.foreign_calls.iter().any(|call| call.expression == id) {
        state.bump_foreign();
    }
    state.effects.panic |= owner.checks.iter().any(|check| check.expression == id);
}

pub(crate) fn apply_dispatch(
    owner: &Owner,
    state: &mut AbstractState,
    dispatch: u32,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) {
    let summary = callees(CallSite::Dispatch(dispatch)).unwrap_or_else(|| {
        let dispatch = &owner.dispatches[usize::try_from(dispatch).expect("dispatch 编号适配宿主")];
        if dispatch.function.is_none() && !dispatch.dynamic {
            FunctionSummary::default()
        } else {
            FunctionSummary::conservative()
        }
    });
    apply_summary(state, &summary);
}

pub(crate) fn apply_initializer(
    state: &mut AbstractState,
    statement: u32,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) {
    apply_summary(
        state,
        &callees(CallSite::Initializer(statement)).unwrap_or_else(FunctionSummary::conservative),
    );
}

fn apply_summary(state: &mut AbstractState, summary: &FunctionSummary) {
    state.effects.panic |= summary.may_panic;
    state.effects.allocate |= summary.may_allocate;
    state.effects.suspend |= summary.may_suspend;
    state.effects.foreign |= summary.alias_foreign;
    state.effects.alias_heap |= summary.alias_heap;
    state.effects.call_unknown |= summary.may_call_unknown;
    state.effects.reads_hidden |= summary.reads_hidden_state;
    state.effects.writes_hidden |= summary.writes_hidden_state;
    if summary.may_call_unknown
        || summary.alias_foreign
        || summary.alias_heap
        || summary.may_mutate_len
        || summary.may_suspend
        || summary.writes_hidden_state
        || summary.unknown_param_access
        || !summary.write_params.is_empty()
    {
        state.bump_heap();
    }
    state.effects.resource_publish |= summary.may_mutate_len;
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
        ExprKind::Unary {
            operation: UnOp::Deref,
            value: base,
        } => place_local(owner, *base),
        _ => None,
    }
}

fn is_integer_ty(module: &Module, owner: &Owner, id: ExprId) -> bool {
    matches!(
        module.types[owner.expression_types[id.index()].index()],
        Type::Int { .. }
    )
}

fn integer_range(module: &Module, owner: &Owner, id: ExprId, range: Interval) -> Interval {
    let Type::Int { signed, bits } = module.types[owner.expression_types[id.index()].index()]
    else {
        return range;
    };
    debug_assert!(bits > 0 && bits <= 128, "整数位宽由类型形成保证");
    if bits == 128 {
        return range;
    }
    let bounds = if signed {
        let magnitude = 1i128 << (bits - 1);
        Interval {
            lo: -magnitude,
            hi: magnitude - 1,
        }
    } else {
        Interval {
            lo: 0,
            hi: (1i128 << bits) - 1,
        }
    };
    if range.lo < bounds.lo || range.hi > bounds.hi {
        bounds
    } else {
        range
    }
}
