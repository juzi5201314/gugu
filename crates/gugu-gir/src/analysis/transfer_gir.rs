//! GIR 语句与终结符上的抽象状态转移。

use super::domain::{AbstractState, AliasClass, Interval, Relation, ValueKey};
use super::transfer;
use super::types::FunctionSummary;
use crate::ast::BinOp;
use crate::gir::body::{
    BinaryOp, BlockId, CompareOp, ConstValue, GirBody, IntrinsicOp, LocalId, Operand, Place,
    Rvalue, StatementKind, Terminator, UnaryOp,
};
use crate::gir::passing::PassingTable;
use crate::hir::{self, ExprId, Module, Owner};
use crate::mono::instantiate::CallSite;

#[derive(Clone, Debug)]
pub struct CompareFact {
    pub local: LocalId,
    pub op: CompareOp,
    pub left: Operand,
    pub right: Operand,
}

/// GIR local 是 `body.locals` 的稠密下标；反向表达式表按原 ExprId 顺序分组。
/// 每个表达式至多映射一个 local，因此平坦表大小不超过表达式总数。
pub struct IndexedBody<'a> {
    body: &'a GirBody,
    offsets: Vec<usize>,
    expressions: Vec<ExprId>,
    resolved: Vec<(ExprId, hir::LocalId)>,
    relations: Vec<Relation>,
    /// `resolved` 的派生缓存，与 `resolved` 同长：`(本地区间, 本地长度区间, 表达式区间)`。
    ///
    /// 派生结果只由这两个输入决定（表达式类型固定），因此输入未变时可直接复用，
    /// 不必重跑 `clamp_integer`；缓存按 body 分析期存活，随 `IndexedBody` 一起丢弃。
    resolved_cache: std::cell::RefCell<Vec<(Interval, Interval, Interval)>>,
}

impl<'a> IndexedBody<'a> {
    pub fn new(body: &'a GirBody, owner: &Owner) -> Self {
        let mut offsets = vec![0; body.locals.len() + 1];
        for local in body.expression_locals.iter().flatten() {
            debug_assert!(local.index() < body.locals.len());
            offsets[local.index() + 1] += 1;
        }
        for index in 1..offsets.len() {
            offsets[index] += offsets[index - 1];
        }
        let mut positions = offsets[..body.locals.len()].to_vec();
        let mut expressions = vec![ExprId(0); offsets[body.locals.len()]];
        for (index, local) in body.expression_locals.iter().enumerate() {
            if let Some(local) = local {
                expressions[positions[local.index()]] =
                    ExprId(u32::try_from(index).expect("表达式编号已由 HIR 验证"));
                positions[local.index()] += 1;
            }
        }
        let resolved: Vec<_> = owner
            .expressions
            .iter()
            .enumerate()
            .filter_map(|(index, expression)| {
                if let hir::ExprKind::Resolved(hir::Res::Local(local)) = expression.kind {
                    Some((
                        ExprId(u32::try_from(index).expect("表达式编号已由 HIR 验证")),
                        local,
                    ))
                } else {
                    None
                }
            })
            .collect();
        let mut relations = Vec::with_capacity(resolved.len() * 4);
        for &(expr, local) in &resolved {
            for (left, right) in [
                (ValueKey::Expr(expr), ValueKey::Local(local)),
                (ValueKey::ExprLen(expr), ValueKey::LocalLen(local)),
            ] {
                relations.push(Relation::new(left, right, 0));
                relations.push(Relation::new(right, left, 0));
            }
        }
        relations.sort_unstable();
        let cached =
            vec![(Interval::UNKNOWN, Interval::UNKNOWN, Interval::UNKNOWN); resolved.len()];
        Self {
            body,
            offsets,
            expressions,
            resolved,
            relations,
            resolved_cache: std::cell::RefCell::new(cached),
        }
    }

    fn expressions(&self, local: LocalId) -> &[ExprId] {
        &self.expressions[self.offsets[local.index()]..self.offsets[local.index() + 1]]
    }
}

impl std::ops::Deref for IndexedBody<'_> {
    type Target = GirBody;

    fn deref(&self) -> &Self::Target {
        self.body
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "抽象解释逐语句需要 module/owner/body/state/passing/callees 同时在场"
)]
pub fn execute_statement(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
    state: &mut AbstractState,
    kind: &StatementKind,
    passing: &PassingTable,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    compare: &mut Option<CompareFact>,
) {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            sync_resolved(module, owner, body, state);
            assign(module, owner, body, state, *place, rvalue, callees, compare);
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
        // 平台调用可能撤销物理页并改变 mapping 状态；它既不读也不写受管堆，因此按 foreign
        // 效果处理，并使已解析的 raw 事实失效。
        StatementKind::PlatformCall { .. } => state.bump_foreign(),
        StatementKind::SetDiscriminant { place, .. } => invalidate_place(body, state, *place),
        _ => {}
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "边展开需要上下文、当前状态、分支复写缓冲与边回调"
)]
pub fn for_each_successor(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
    state: &mut AbstractState,
    branch_scratch: &mut AbstractState,
    terminator: &Terminator,
    compare: Option<&CompareFact>,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    mut on_edge: impl FnMut(BlockId, &AbstractState),
) {
    sync_resolved(module, owner, body, state);
    match terminator {
        Terminator::Goto { target } => on_edge(*target, state),
        Terminator::SwitchInt {
            value,
            targets,
            otherwise,
        } => {
            for &(imm, block) in targets {
                branch_scratch.copy_from(state);
                assume_switch(body, owner, branch_scratch, value, Some(imm), compare);
                on_edge(block, branch_scratch);
            }
            let fallback = match targets.as_slice() {
                [(1, _)] => Some(0),
                [(0, _)] => Some(1),
                _ => None,
            };
            assume_switch(body, owner, state, value, fallback, compare);
            on_edge(*otherwise, state);
        }
        Terminator::Call {
            site,
            destination,
            normal,
            unwind,
            ..
        } => {
            transfer::apply_site(module, owner, state, *site, callees);
            sync_resolved(module, owner, body, state);
            if destination.is_local()
                && let CallSite::Expression(id) = *site
            {
                let range = state.range(ValueKey::Expr(hir::ExprId(id)));
                write_place(module, owner, body, state, *destination, range);
            }
            on_edge(*normal, state);
            if let Some(unwind) = *unwind
                && state.effects.panic
            {
                on_edge(unwind, state);
            }
        }
        Terminator::Panic { unwind, .. } => {
            state.effects.panic = true;
            on_edge(*unwind, state);
        }
        Terminator::Suspend {
            resume, cancelled, ..
        } => {
            state.effects.suspend = true;
            state.bump_heap();
            on_edge(*resume, state);
            if let Some(cancelled) = *cancelled {
                on_edge(cancelled, state);
            }
        }
        Terminator::SelectCommit {
            ready,
            suspend,
            cancelled,
            ..
        } => {
            state.effects.suspend = true;
            state.bump_heap();
            on_edge(*ready, state);
            if let Some(suspend) = *suspend {
                on_edge(suspend, state);
            }
            if let Some(cancelled) = *cancelled {
                on_edge(cancelled, state);
            }
        }
        Terminator::Return
        | Terminator::ResumePanic
        | Terminator::Abort
        | Terminator::Unreachable => {}
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "赋值转移与 execute_statement 共享同一组上下文参数"
)]
fn assign(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
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
    body: &IndexedBody<'_>,
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
        IntrinsicOp::Asm(_) | IntrinsicOp::PackFor => state.bump_foreign(),
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
        IntrinsicOp::ChanNew => state.effects.allocate = true,
        _ => {}
    }
    Interval::UNKNOWN
}

fn write_place(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
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
    for &expr in body.expressions(place.local) {
        let range = transfer::integer_range(module, owner, expr, interval);
        state.set_range(ValueKey::Expr(expr), range);
    }
}

fn sync_copy(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
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
    for &expr in body.expressions(place.local) {
        transfer::copy_local(module, owner, state, expr, src_hir);
    }
}

fn sync_len_relation(
    owner: &Owner,
    body: &IndexedBody<'_>,
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
    for &expr in body.expressions(place.local) {
        let expr = ValueKey::Expr(expr);
        state.relate(Relation::new(expr, key, 0));
        state.relate(Relation::new(key, expr, 0));
    }
}

fn eval_len(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
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

pub fn sync_resolved(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
    state: &mut AbstractState,
) {
    if !state.reachable {
        return;
    }
    // `body.relations` 是每个 body 固定的关系集，`resolved` 派生是状态上的幂等纯函数：
    // 只要区间没有改动，重复同步只会重算出同一结果，因此按标记跳过。
    if !state.derived_ready() {
        derive_resolved(module, owner, body, state);
    }
    if !state.relations_synced {
        state.relate_sorted(&body.relations);
        state.relations_synced = true;
    }
}

/// 把 `body.resolved` 的表达式区间从当前本地区间重新派生出来。
///
/// 每个条目的结果只由「本地区间 + 本地长度区间」决定（表达式类型固定），因此按这两个输入
/// 缓存：输入未变时直接写入缓存值，跳过 `clamp_integer` 与类型查询。
fn derive_resolved(
    module: &Module,
    owner: &Owner,
    body: &IndexedBody<'_>,
    state: &mut AbstractState,
) {
    if body.resolved.is_empty() {
        state.mark_derived();
        return;
    }
    let mut cache = body.resolved_cache.borrow_mut();
    for (index, &(expr, local)) in body.resolved.iter().enumerate() {
        let base = state.local_range[local.index()];
        let len = state.local_len[local.index()];
        let slot = &mut cache[index];
        if slot.0 != base || slot.1 != len {
            *slot = (
                base,
                len,
                transfer::integer_range(module, owner, expr, base),
            );
        }
        state.expr_range[expr.index()] = slot.2;
        state.expr_len[expr.index()] = len;
    }
    state.mark_derived();
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
    if let hir::Type::Array(_, length) = ty
        && let Some(length) = length.known()
    {
        state.set_range(
            ValueKey::LocalLen(local),
            Interval::point(i128::from(length)),
        );
    }
}

fn operand_range(body: &IndexedBody<'_>, state: &AbstractState, operand: &Operand) -> Interval {
    match operand {
        Operand::Copy(place) | Operand::MoveInternal(place) => place_range(body, state, *place),
        Operand::Constant(id) => const_range(&body.constants[id.index()].value),
        Operand::LateConstRef { .. } | Operand::Function(_) => Interval::UNKNOWN,
    }
}

fn place_range(body: &IndexedBody<'_>, state: &AbstractState, place: Place) -> Interval {
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

fn place_len(body: &IndexedBody<'_>, state: &AbstractState, place: Place) -> Interval {
    if let Some(hir) = body.locals[place.local.index()].hir_local {
        return state.range(ValueKey::LocalLen(hir));
    }
    mapped_expr(body, place.local)
        .map(|expr| state.range(ValueKey::ExprLen(expr)))
        .unwrap_or(Interval::UNKNOWN)
}

fn mapped_expr(body: &IndexedBody<'_>, local: LocalId) -> Option<ExprId> {
    body.expressions(local).first().copied()
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

fn invalidate_place(body: &IndexedBody<'_>, state: &mut AbstractState, place: Place) {
    if let Some(hir) = body.locals[place.local.index()].hir_local {
        state.bump_local(hir);
    } else {
        state.bump_heap();
    }
}

fn assume_switch(
    body: &IndexedBody<'_>,
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

fn assume_fact(body: &IndexedBody<'_>, state: &mut AbstractState, fact: &CompareFact, truth: bool) {
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
    body: &IndexedBody<'_>,
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

fn operand_expr(body: &IndexedBody<'_>, operand: &Operand) -> Option<ExprId> {
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

fn operand_key(body: &IndexedBody<'_>, operand: &Operand) -> Option<ValueKey> {
    let local = operand_local(operand)?;
    if let Some(hir) = body.locals[local.index()].hir_local {
        return Some(ValueKey::Local(hir));
    }
    mapped_expr(body, local).map(ValueKey::Expr)
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
