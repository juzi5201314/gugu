//! GIR 语句与终结符上的抽象状态转移。

use super::domain::{AbstractState, AliasClass, Interval, Relation, ValueKey};
use super::transfer;
use super::types::FunctionSummary;
use crate::frontend::ast::BinOp;
use crate::frontend::gir::body::{
    BinaryOp, BlockId, CompareOp, ConstValue, GirBody, IntrinsicOp, LocalId, Operand, Place,
    Rvalue, StatementKind, Terminator, UnaryOp,
};
use crate::frontend::gir::passing::PassingTable;
use crate::frontend::hir::{self, ExprId, Module, Owner};
use crate::frontend::mono::instantiate::CallSite;

#[derive(Clone, Debug)]
pub(crate) struct CompareFact {
    pub local: LocalId,
    pub op: CompareOp,
    pub left: Operand,
    pub right: Operand,
}

pub(crate) fn execute_statement(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    state: &mut AbstractState,
    kind: &StatementKind,
    passing: &PassingTable,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    compare: &mut Option<CompareFact>,
) {
    sync_resolved(module, owner, state);
    match kind {
        StatementKind::Assign(place, rvalue) => {
            assign(module, owner, body, state, *place, rvalue, callees, compare);
            sync_resolved(module, owner, state);
        }
        StatementKind::Atomic { .. } | StatementKind::Volatile { .. } => state.bump_foreign(),
        StatementKind::ValueAction { descriptor, .. } => {
            let class = passing.class(*descriptor);
            if class.has_resource() || class.is_unknown() {
                state.effects.alias_heap = true;
                state.bump_heap();
            }
        }
        StatementKind::ResourceAction { .. } | StatementKind::GcWrite { .. } => {
            state.effects.alias_heap = true;
            state.bump_heap();
        }
        StatementKind::SetDiscriminant { place, .. } => invalidate_place(body, state, *place),
        _ => {}
    }
}

pub(crate) fn successor_states(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    state: &mut AbstractState,
    terminator: &Terminator,
    compare: Option<&CompareFact>,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> Vec<(BlockId, AbstractState)> {
    sync_resolved(module, owner, state);
    match terminator {
        Terminator::Goto { target } => vec![(*target, state.clone())],
        Terminator::SwitchInt {
            value,
            targets,
            otherwise,
        } => switch_edges(body, owner, state, value, targets, *otherwise, compare),
        Terminator::Call {
            site,
            destination,
            normal,
            unwind,
            ..
        } => call_edges(
            module,
            owner,
            body,
            state,
            *site,
            *destination,
            *normal,
            *unwind,
            callees,
        ),
        Terminator::Panic { unwind, .. } => {
            state.effects.panic = true;
            vec![(*unwind, state.clone())]
        }
        Terminator::Suspend {
            resume, cancelled, ..
        } => suspend_edges(state, *resume, *cancelled),
        Terminator::SelectCommit {
            ready,
            suspend,
            cancelled,
            ..
        } => select_edges(state, *ready, *suspend, *cancelled),
        Terminator::Return
        | Terminator::ResumePanic
        | Terminator::Abort
        | Terminator::Unreachable => Vec::new(),
    }
}

fn assign(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    state: &mut AbstractState,
    place: Place,
    rvalue: &Rvalue,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    compare: &mut Option<CompareFact>,
) {
    if let Rvalue::Compare { op, left, right } = rvalue
        && place.is_local()
    {
        *compare = Some(CompareFact {
            local: place.local,
            op: *op,
            left: left.clone(),
            right: right.clone(),
        });
    }
    let interval = eval_rvalue(module, owner, body, state, rvalue, callees);
    write_place(module, owner, body, state, place, interval);
    sync_copy(module, owner, body, state, place, rvalue);
    sync_len_relation(owner, body, state, place, rvalue);
}

fn eval_rvalue(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    state: &mut AbstractState,
    rvalue: &Rvalue,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> Interval {
    let _ = (module, owner, callees);
    match rvalue {
        Rvalue::Use(operand) => operand_range(body, state, operand),
        Rvalue::UnaryOp {
            op: UnaryOp::Neg,
            operand,
        } => operand_range(body, state, operand).neg(),
        Rvalue::BinaryOp { op, left, right } => binary_range(
            *op,
            operand_range(body, state, left),
            operand_range(body, state, right),
        ),
        Rvalue::CheckedOp { .. } => Interval { lo: 0, hi: 1 },
        Rvalue::Compare { op, left, right } => transfer::compare_interval(
            compare_bin(*op),
            operand_range(body, state, left),
            operand_range(body, state, right),
        ),
        Rvalue::Len(place) => eval_len(module, owner, body, state, *place),
        Rvalue::Cast { operand, .. } => operand_range(body, state, operand),
        Rvalue::ValueCopy(place) => place_range(body, state, *place),
        Rvalue::CowSnapshot(place) => {
            state.effects.cow_seal = true;
            place_range(body, state, *place)
        }
        Rvalue::AllocObject { .. } | Rvalue::AllocArray { .. } => {
            state.effects.allocate = true;
            Interval::UNKNOWN
        }
        Rvalue::Intrinsic { op, .. } => intrinsic_effects(state, op),
        _ => Interval::UNKNOWN,
    }
}

fn binary_range(op: BinaryOp, left: Interval, right: Interval) -> Interval {
    match op {
        BinaryOp::Add => left.add(right),
        BinaryOp::Sub => left.sub(right),
        _ => Interval::UNKNOWN,
    }
}

fn intrinsic_effects(state: &mut AbstractState, op: &IntrinsicOp) -> Interval {
    match op {
        IntrinsicOp::Asm(_) => state.bump_foreign(),
        IntrinsicOp::Spawn(_) => {
            state.effects.suspend = true;
            state.effects.allocate = true;
            state.bump_heap();
        }
        IntrinsicOp::StaticRef(definition) => {
            state.effects.reads_hidden = true;
            state.effects.resource_publish = true;
            for alias in &mut state.alias {
                if matches!(*alias, AliasClass::Heap) {
                    *alias = AliasClass::Static(definition.0);
                }
            }
        }
        IntrinsicOp::ChanNew | IntrinsicOp::Format { .. } => state.effects.allocate = true,
        _ => {}
    }
    Interval::UNKNOWN
}

fn write_place(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    state: &mut AbstractState,
    place: Place,
    interval: Interval,
) {
    if !place.is_local() {
        state.effects.alias_heap = true;
        state.bump_heap();
        if let Some(hir) = body.locals[place.local.index()].hir_local {
            state.bump_local(hir);
        }
        return;
    }
    if let Some(hir) = body.locals[place.local.index()].hir_local {
        if hir.index() < state.init.len() && state.init[hir.index()] {
            state.bump_local(hir);
        }
        state.set_range(
            ValueKey::Local(hir),
            transfer::clamp_integer(module, owner.locals[hir.index()].ty, interval),
        );
        if hir.index() < state.init.len() {
            state.init[hir.index()] = true;
        }
        seed_local_array_len(module, owner, state, hir);
    }
    // 只发布值区间。HIR 效果是类型检查上界，Call 等操作由 apply_site 的实例摘要取代。
    for (index, mapped) in body.expression_locals.iter().enumerate() {
        if *mapped == Some(place.local) {
            let expr = ExprId(index as u32);
            let range = transfer::integer_range(module, owner, expr, interval);
            state.set_range(ValueKey::Expr(expr), range);
        }
    }
}

fn sync_copy(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    state: &mut AbstractState,
    place: Place,
    rvalue: &Rvalue,
) {
    let src = match rvalue {
        Rvalue::Use(Operand::Copy(src) | Operand::MoveInternal(src))
        | Rvalue::ValueCopy(src)
        | Rvalue::CowSnapshot(src) => src,
        _ => return,
    };
    if !place.is_local() || !src.is_local() {
        return;
    }
    let Some(src_hir) = body.locals[src.local.index()].hir_local else {
        return;
    };
    if let Some(dest_hir) = body.locals[place.local.index()].hir_local {
        state.set_range(
            ValueKey::LocalLen(dest_hir),
            state.range(ValueKey::LocalLen(src_hir)),
        );
    }
    for (index, mapped) in body.expression_locals.iter().enumerate() {
        if *mapped == Some(place.local) {
            transfer::copy_local(module, owner, state, ExprId(index as u32), src_hir);
        }
    }
}

fn sync_len_relation(
    owner: &Owner,
    body: &GirBody,
    state: &mut AbstractState,
    place: Place,
    rvalue: &Rvalue,
) {
    let Rvalue::Len(src) = rvalue else {
        return;
    };
    let key = if let Some(hir) = body.locals[src.local.index()].hir_local {
        ValueKey::LocalLen(hir)
    } else if let Some(expr) = mapped_expr(body, src.local) {
        transfer::len_key(owner, expr)
    } else {
        return;
    };
    for (index, mapped) in body.expression_locals.iter().enumerate() {
        if *mapped == Some(place.local) {
            let expr = ValueKey::Expr(ExprId(index as u32));
            state.relate(Relation {
                left: expr,
                right: key,
                offset: 0,
            });
            state.relate(Relation {
                left: key,
                right: expr,
                offset: 0,
            });
        }
    }
}

fn eval_len(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    state: &mut AbstractState,
    place: Place,
) -> Interval {
    if let Some(expr) = mapped_expr(body, place.local) {
        transfer::seed_array_len(module, owner, state, expr);
        if let Some(length) = transfer::array_len(module, owner, expr) {
            return Interval::point(i128::from(length));
        }
        return state.range(transfer::len_key(owner, expr));
    }
    place_len(body, state, place)
}

pub(crate) fn sync_resolved(module: &Module, owner: &Owner, state: &mut AbstractState) {
    for (index, expression) in owner.expressions.iter().enumerate() {
        let hir::ExprKind::Resolved(hir::Res::Local(local)) = expression.kind else {
            continue;
        };
        let expr = ExprId(index as u32);
        let range =
            transfer::integer_range(module, owner, expr, state.range(ValueKey::Local(local)));
        state.set_range(ValueKey::Expr(expr), range);
        state.set_range(
            ValueKey::ExprLen(expr),
            state.range(ValueKey::LocalLen(local)),
        );
        state.relate(Relation {
            left: ValueKey::Expr(expr),
            right: ValueKey::Local(local),
            offset: 0,
        });
        state.relate(Relation {
            left: ValueKey::Local(local),
            right: ValueKey::Expr(expr),
            offset: 0,
        });
        state.relate(Relation {
            left: ValueKey::ExprLen(expr),
            right: ValueKey::LocalLen(local),
            offset: 0,
        });
        state.relate(Relation {
            left: ValueKey::LocalLen(local),
            right: ValueKey::ExprLen(expr),
            offset: 0,
        });
    }
}

fn seed_local_array_len(
    module: &Module,
    owner: &Owner,
    state: &mut AbstractState,
    local: hir::LocalId,
) {
    let mut ty = &module.types[owner.locals[local.index()].ty.index()];
    if let hir::Type::Ref(inner) = ty {
        ty = &module.types[inner.index()];
    }
    if let hir::Type::Array(_, length) = ty {
        state.set_range(
            ValueKey::LocalLen(local),
            Interval::point(i128::from(*length)),
        );
    }
}

fn operand_range(body: &GirBody, state: &AbstractState, operand: &Operand) -> Interval {
    match operand {
        Operand::Copy(place) | Operand::MoveInternal(place) => place_range(body, state, *place),
        Operand::Constant(id) => const_range(&body.constants[id.index()].value),
        Operand::LateConstRef { .. } | Operand::Function(_) => Interval::UNKNOWN,
    }
}

fn place_range(body: &GirBody, state: &AbstractState, place: Place) -> Interval {
    if !place.is_local() {
        return Interval::UNKNOWN;
    }
    if let Some(hir) = body.locals[place.local.index()].hir_local {
        return state.range(ValueKey::Local(hir));
    }
    mapped_expr(body, place.local)
        .map(|expr| state.range(ValueKey::Expr(expr)))
        .unwrap_or(Interval::UNKNOWN)
}

fn place_len(body: &GirBody, state: &AbstractState, place: Place) -> Interval {
    if let Some(hir) = body.locals[place.local.index()].hir_local {
        return state.range(ValueKey::LocalLen(hir));
    }
    mapped_expr(body, place.local)
        .map(|expr| state.range(ValueKey::ExprLen(expr)))
        .unwrap_or(Interval::UNKNOWN)
}

fn mapped_expr(body: &GirBody, local: LocalId) -> Option<ExprId> {
    body.expression_locals
        .iter()
        .enumerate()
        .find_map(|(index, mapped)| (*mapped == Some(local)).then_some(ExprId(index as u32)))
}

fn const_range(value: &ConstValue) -> Interval {
    match value {
        ConstValue::Integer(value) => Interval::point(i128::try_from(*value).unwrap_or(i128::MAX)),
        ConstValue::Bool(true) => Interval::point(1),
        ConstValue::Bool(false) => Interval::point(0),
        ConstValue::Char(value) => Interval::point(i128::from(u32::from(*value))),
        _ => Interval::UNKNOWN,
    }
}

fn invalidate_place(body: &GirBody, state: &mut AbstractState, place: Place) {
    if let Some(hir) = body.locals[place.local.index()].hir_local {
        state.bump_local(hir);
    } else {
        state.bump_heap();
    }
}

fn switch_edges(
    body: &GirBody,
    owner: &Owner,
    state: &AbstractState,
    value: &Operand,
    targets: &[(u128, BlockId)],
    otherwise: BlockId,
    compare: Option<&CompareFact>,
) -> Vec<(BlockId, AbstractState)> {
    let mut edges = Vec::with_capacity(targets.len() + 1);
    for &(imm, block) in targets {
        let mut next = state.clone();
        assume_switch(body, owner, &mut next, value, Some(imm), compare);
        edges.push((block, next));
    }
    let mut next = state.clone();
    let fallback = match targets {
        [(1, _)] => Some(0),
        [(0, _)] => Some(1),
        _ => None,
    };
    assume_switch(body, owner, &mut next, value, fallback, compare);
    edges.push((otherwise, next));
    edges
}

fn assume_switch(
    body: &GirBody,
    owner: &Owner,
    state: &mut AbstractState,
    value: &Operand,
    taken: Option<u128>,
    compare: Option<&CompareFact>,
) {
    if let Some(expr) = operand_expr(body, value)
        && let hir::ExprKind::Binary { .. } = owner.expressions[expr.index()].kind
    {
        match taken {
            Some(1) => transfer::assume(state, expr, true, owner),
            Some(0) => transfer::assume(state, expr, false, owner),
            _ => {}
        }
        return;
    }
    if let (Some(fact), Some(imm)) = (compare, taken)
        && operand_local(value) == Some(fact.local)
    {
        let truth = imm != 0;
        assume_fact(body, state, fact, truth);
    }
    refine_switch_range(body, state, value, taken);
}

fn assume_fact(body: &GirBody, state: &mut AbstractState, fact: &CompareFact, truth: bool) {
    let left = operand_key(body, &fact.left);
    let right = operand_key(body, &fact.right);
    transfer::refine_compare(
        state,
        compare_bin(fact.op),
        left,
        operand_range(body, state, &fact.left),
        right,
        operand_range(body, state, &fact.right),
        truth,
    );
}

fn refine_switch_range(
    body: &GirBody,
    state: &mut AbstractState,
    value: &Operand,
    taken: Option<u128>,
) {
    let range = operand_range(body, state, value);
    match taken {
        Some(1) if range.hi < 1 => state.reachable = false,
        Some(0) if range.lo > 0 => state.reachable = false,
        _ => {}
    }
}

fn operand_expr(body: &GirBody, operand: &Operand) -> Option<ExprId> {
    operand_local(operand).and_then(|local| mapped_expr(body, local))
}

fn operand_local(operand: &Operand) -> Option<LocalId> {
    match operand {
        Operand::Copy(place) | Operand::MoveInternal(place) if place.is_local() => {
            Some(place.local)
        }
        _ => None,
    }
}

fn operand_key(body: &GirBody, operand: &Operand) -> Option<ValueKey> {
    let local = operand_local(operand)?;
    if let Some(hir) = body.locals[local.index()].hir_local {
        return Some(ValueKey::Local(hir));
    }
    mapped_expr(body, local).map(ValueKey::Expr)
}

fn call_edges(
    module: &Module,
    owner: &Owner,
    body: &GirBody,
    state: &mut AbstractState,
    site: CallSite,
    destination: Place,
    normal: BlockId,
    unwind: Option<BlockId>,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
) -> Vec<(BlockId, AbstractState)> {
    transfer::apply_site(module, owner, state, site, callees);
    sync_resolved(module, owner, state);
    if destination.is_local()
        && let CallSite::Expression(id) = site
    {
        write_place(
            module,
            owner,
            body,
            state,
            destination,
            state.range(ValueKey::Expr(hir::ExprId(id))),
        );
    }
    let mut edges = vec![(normal, state.clone())];
    if let Some(unwind) = unwind
        && state.effects.panic
    {
        edges.push((unwind, state.clone()));
    }
    edges
}

fn suspend_edges(
    state: &mut AbstractState,
    resume: BlockId,
    cancelled: Option<BlockId>,
) -> Vec<(BlockId, AbstractState)> {
    state.effects.suspend = true;
    state.bump_heap();
    let mut edges = vec![(resume, state.clone())];
    if let Some(cancelled) = cancelled {
        edges.push((cancelled, state.clone()));
    }
    edges
}

fn select_edges(
    state: &mut AbstractState,
    ready: BlockId,
    suspend: Option<BlockId>,
    cancelled: Option<BlockId>,
) -> Vec<(BlockId, AbstractState)> {
    state.effects.suspend = true;
    state.bump_heap();
    let mut edges = vec![(ready, state.clone())];
    if let Some(suspend) = suspend {
        edges.push((suspend, state.clone()));
    }
    if let Some(cancelled) = cancelled {
        edges.push((cancelled, state.clone()));
    }
    edges
}

fn compare_bin(op: CompareOp) -> BinOp {
    match op {
        CompareOp::Eq => BinOp::Eq,
        CompareOp::Ne => BinOp::Ne,
        CompareOp::Lt => BinOp::Lt,
        CompareOp::Le => BinOp::Le,
        CompareOp::Gt => BinOp::Gt,
        CompareOp::Ge => BinOp::Ge,
    }
}
