//! 局部槽按声明分配，名称环境仅保存当前可见槽；分支按可达前驱交集合流。
use super::super::{ast::*, intern::Symbol};
use super::{
    model::{DefRef, Model, Ty},
    patterns,
};
use crate::{Diagnostic, DiagnosticCode, Span};
use std::collections::BTreeMap;
mod calls;
mod captures;
mod defer;
mod expr;
mod flow;
mod generics;
mod inference;
mod methods;
mod opaque;
mod operations;
mod reflection;

#[derive(Clone)]
struct State {
    names: BTreeMap<Symbol, usize>,
    initialized: Vec<bool>,
    callables: Vec<Vec<super::model::CallableId>>,
    cleanup_paths: BTreeMap<usize, CleanupPath>,
    reachable: bool,
}
#[derive(Clone)]
struct CleanupPath {
    initialized: Vec<bool>,
    mandatory: bool,
}
struct Slot {
    ty: Ty,
    storage: u8,
}
struct LoopState {
    values: Vec<Ty>,
    exits: Vec<State>,
    value_allowed: bool,
    cleanup_floor: usize,
}
struct TryState {
    ty: Ty,
    failures: Vec<State>,
    cleanup_floor: usize,
}
struct Checker<'m, 'a> {
    model: &'m Model<'a>,
    module: usize,
    slots: Vec<Slot>,
    state: State,
    errors: Vec<Diagnostic>,
    return_ty: Ty,
    loops: Vec<LoopState>,
    tries: Vec<TryState>,
    vars: Vec<Option<Ty>>,
    number_kinds: Vec<inference::NumberKind>,
    literals: Vec<(Ty, LitKind, bool, Span)>,
    unsafe_depth: usize,
    dependencies: Vec<DefRef>,
    expressions: Vec<(ExprId, Ty)>,
    local_statics: Vec<super::output::LocalStatic>,
    defers: Vec<defer::Deferred>,
    cleanup_plan: Vec<super::output::CleanupRegistration>,
    in_cleanup: bool,
    discarded_expression: Option<ExprId>,
    runtime_checks: Vec<super::output::RuntimeCheck>,
    capture_frames: Vec<captures::CaptureFrame>,
    capture_plans: Vec<super::output::CapturePlan>,
    pattern_plans: Vec<super::output::PatternPlan>,
    variadic_calls: Vec<super::output::VariadicCall>,
    callable_bounds: BTreeMap<String, Ty>,
    callable_constraints: Vec<(Ty, Ty, Span)>,
    expression_callables: BTreeMap<u32, Vec<super::model::CallableId>>,
    trait_constraints: Vec<(super::traits::Obligation, Vec<super::traits::Obligation>)>,
    dispatches: Vec<super::output::Dispatch>,
    hidden_candidates: Vec<(u32, Vec<Ty>, Ty)>,
    erasures: Vec<super::output::Erasure>,
    reflections: Vec<super::output::Reflection>,
}

pub(super) fn check(model: &Model<'_>) -> Result<super::output::CheckedSemantics, Vec<Diagnostic>> {
    let mut errors = Vec::new();
    let mut bodies = Vec::new();
    let mut hidden_types = vec![None; model.opaques.definitions.len()];
    let mut dependencies: Vec<Vec<Vec<DefRef>>> = model
        .modules
        .iter()
        .map(|m| vec![Vec::new(); m.arena.items.len()])
        .collect();
    for (module, parsed) in model.modules.iter().enumerate() {
        for (index, item) in parsed.arena.items.iter().enumerate() {
            if !parsed.configured.item_active(ItemId(index as u32)) {
                continue;
            }
            let mut checker = Checker::new(model, module);
            match item.kind {
                ItemKind::Function(id) => {
                    checker.function(id, None);
                }
                ItemKind::Const {
                    ty,
                    value: Some(value),
                } => {
                    let expected = ty.map(|ty| checker.form(ty));
                    checker.expression(value, expected.as_ref());
                }
                ItemKind::Static { ty, value } => {
                    let expected = checker.form(ty);
                    checker.expression(value, Some(&expected));
                }
                _ => {}
            }
            checker.finish_inference();
            checker.finish_hidden(&mut hidden_types);
            if checker.vars.iter().any(Option::is_none) {
                checker.error(
                    DiagnosticCode::InvalidType,
                    "声明中的类型变量未能唯一收敛",
                    item.span.clone(),
                );
            }
            let mut expressions = checker.expressions;
            expressions.sort_by_key(|(id, _)| id.0);
            expressions.dedup_by_key(|(id, _)| id.0);
            let (slots, slot_storage) = checker
                .slots
                .into_iter()
                .map(|slot| (slot.ty, slot.storage))
                .unzip();
            bodies.push(super::output::CheckedBody {
                definition: DefRef {
                    module,
                    item: ItemId(u32::try_from(index).expect("arena 项编号")),
                },
                expressions,
                slots,
                local_statics: checker.local_statics,
                cleanup: checker.cleanup_plan,
                runtime_checks: checker.runtime_checks,
                patterns: checker.pattern_plans,
                captures: checker.capture_plans,
                slot_storage,
                variadic_calls: checker.variadic_calls,
                dispatches: checker.dispatches,
                erasures: checker.erasures,
                reflections: checker.reflections,
            });
            dependencies[module][index] = checker.dependencies;
            errors.extend(checker.errors);
        }
    }
    for (id, hidden) in hidden_types.iter().enumerate() {
        if hidden.is_none() && model.opaque_requires_definition(id as u32) {
            errors.push(Diagnostic::error(
                DiagnosticCode::InvalidType,
                "隐藏类型未能唯一形成",
                Some(model.opaque_span(id as u32).clone()),
            ));
        }
    }
    let initialization = match super::initialization::plan(model, &dependencies) {
        Ok(plan) => plan,
        Err(found) => {
            errors.extend(found);
            Vec::new()
        }
    };
    if errors.is_empty() {
        let output = super::output::CheckedSemantics {
            bodies,
            initialization,
            input_fingerprint: [0; 32],
            hidden_types,
        };
        output.verify(model).map_err(|error| vec![error])?;
        Ok(output)
    } else {
        Err(errors)
    }
}

impl<'m, 'a> Checker<'m, 'a> {
    fn new(model: &'m Model<'a>, module: usize) -> Self {
        Self {
            model,
            module,
            slots: Vec::new(),
            state: State {
                names: BTreeMap::new(),
                cleanup_paths: BTreeMap::new(),
                initialized: Vec::new(),
                callables: Vec::new(),
                reachable: true,
            },
            errors: Vec::new(),
            return_ty: Ty::Unit,
            loops: Vec::new(),
            tries: Vec::new(),
            vars: Vec::new(),
            number_kinds: Vec::new(),
            literals: Vec::new(),
            unsafe_depth: 0,
            dependencies: Vec::new(),
            expressions: Vec::new(),
            local_statics: Vec::new(),
            defers: Vec::new(),
            cleanup_plan: Vec::new(),
            in_cleanup: false,
            discarded_expression: None,
            runtime_checks: Vec::new(),
            pattern_plans: Vec::new(),
            capture_frames: Vec::new(),
            capture_plans: Vec::new(),
            variadic_calls: Vec::new(),
            callable_bounds: BTreeMap::new(),
            callable_constraints: Vec::new(),
            expression_callables: BTreeMap::new(),
            trait_constraints: Vec::new(),
            dispatches: Vec::new(),
            hidden_candidates: Vec::new(),
            erasures: Vec::new(),
            reflections: Vec::new(),
        }
    }
    fn arena(&self) -> &'a AstArena {
        &self.model.modules[self.module].arena
    }
    fn error(&mut self, code: DiagnosticCode, message: impl Into<String>, span: Span) {
        self.errors
            .push(Diagnostic::error(code, message, Some(span)));
    }
    fn form(&mut self, id: TyId) -> Ty {
        if self.arena().tys[usize::try_from(id.0).expect("类型下标")].kind == TyKind::Infer {
            return self.fresh();
        }
        match self.model.form(self.module, id) {
            Ok(ty) => ty,
            Err(error) => {
                self.errors.push(error);
                Ty::Error
            }
        }
    }
    fn fresh(&mut self) -> Ty {
        let id = self.vars.len();
        debug_assert!(id < u32::MAX as usize);
        self.vars.push(None);
        self.number_kinds.push(inference::NumberKind::Any);
        Ty::Var(id as u32)
    }
    fn resolve(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Var(id) => self
                .vars
                .get(*id as usize)
                .and_then(Option::as_ref)
                .map_or_else(|| ty.clone(), |ty| self.resolve(ty)),
            Ty::Ref(t) => Ty::Ref(Box::new(self.resolve(t))),
            Ty::Ptr(t) => Ty::Ptr(Box::new(self.resolve(t))),
            Ty::Slice(t) => Ty::Slice(Box::new(self.resolve(t))),
            Ty::Array(t, n) => Ty::Array(Box::new(self.resolve(t)), *n),
            Ty::Option(t) => Ty::Option(Box::new(self.resolve(t))),
            Ty::Result(t, e) => Ty::Result(Box::new(self.resolve(t)), Box::new(self.resolve(e))),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.resolve(t)).collect()),
            Ty::Named(i, args) => Ty::Named(*i, args.iter().map(|t| self.resolve(t)).collect()),
            Ty::Opaque(id, arguments) => {
                Ty::Opaque(*id, arguments.iter().map(|ty| self.resolve(ty)).collect())
            }
            Ty::Dyn(interfaces) => Ty::Dyn(
                interfaces
                    .iter()
                    .map(|interface| super::traits::TraitRef {
                        id: interface.id,
                        arguments: interface
                            .arguments
                            .iter()
                            .map(|ty| self.resolve(ty))
                            .collect(),
                    })
                    .collect(),
            ),
            Ty::Function(args, ret) => Ty::Function(
                args.iter().map(|t| self.resolve(t)).collect(),
                Box::new(self.resolve(ret)),
            ),
            Ty::Callable(id, arguments, signature) => Ty::Callable(
                *id,
                arguments.iter().map(|ty| self.resolve(ty)).collect(),
                Box::new(self.resolve(signature)),
            ),
            Ty::Projection(base, interface, name) => Ty::Projection(
                Box::new(self.resolve(base)),
                super::traits::TraitRef {
                    id: interface.id,
                    arguments: interface
                        .arguments
                        .iter()
                        .map(|ty| self.resolve(ty))
                        .collect(),
                },
                name.clone(),
            ),
            Ty::Chan(t) => Ty::Chan(Box::new(self.resolve(t))),
            Ty::Join(t) => Ty::Join(Box::new(self.resolve(t))),
            _ => ty.clone(),
        }
    }
    fn normalized(&mut self, ty: &Ty) -> Ty {
        match self.model.normalize(&self.resolve(ty), &[]) {
            Ok(ty) => ty,
            Err(error) => {
                self.errors.push(error);
                Ty::Error
            }
        }
    }
    fn unify(&mut self, actual: &Ty, expected: &Ty, span: &Span) -> Ty {
        self.relate(actual, expected, span, true)
    }

    fn join(&mut self, left: &Ty, right: &Ty, span: &Span) -> Ty {
        if self.resolve(right) == Ty::Never {
            self.resolve(left)
        } else {
            self.unify(left, right, span)
        }
    }

    fn relate(&mut self, actual: &Ty, expected: &Ty, span: &Span, coercion: bool) -> Ty {
        let a = self.normalized(actual);
        let b = self.normalized(expected);
        if a == Ty::Error || b == Ty::Error {
            return Ty::Error;
        }
        if a == b || coercion && a == Ty::Never {
            return b;
        }
        if let Some(ty) = self.opaque_relation(&a, &b, span, coercion) {
            return ty;
        }
        match (&a, &b) {
            (Ty::Var(id), ty) | (ty, Ty::Var(id)) => {
                if contains_var(ty, *id) {
                    self.error(
                        DiagnosticCode::InvalidType,
                        "类型变量不能包含自身",
                        span.clone(),
                    );
                    return Ty::Error;
                }
                if !self.constrain_number(*id, ty, span) {
                    return Ty::Error;
                }
                self.vars[usize::try_from(*id).expect("推断下标")] = Some(ty.clone());
                return ty.clone();
            }
            (Ty::Ref(x), Ty::Ref(y))
                if coercion && matches!((&**x, &**y), (Ty::Array(..), Ty::Slice(_))) =>
            {
                let (Ty::Array(t, _), Ty::Slice(u)) = (&**x, &**y) else {
                    unreachable!()
                };
                self.relate(t, u, span, false);
                return b;
            }
            (Ty::Ref(x), Ty::Ref(y))
            | (Ty::Ptr(x), Ty::Ptr(y))
            | (Ty::Slice(x), Ty::Slice(y))
            | (Ty::Option(x), Ty::Option(y))
            | (Ty::Chan(x), Ty::Chan(y))
            | (Ty::Join(x), Ty::Join(y)) => {
                self.relate(x, y, span, false);
                return self.resolve(&b);
            }
            (Ty::Result(x, e), Ty::Result(y, f)) => {
                self.relate(x, y, span, false);
                self.relate(e, f, span, false);
                return self.resolve(&b);
            }
            (Ty::Array(x, n), Ty::Array(y, m)) if n == m => {
                self.relate(x, y, span, false);
                return self.resolve(&b);
            }
            (Ty::Tuple(xs), Ty::Tuple(ys)) if xs.len() == ys.len() => {
                for (x, y) in xs.iter().zip(ys) {
                    self.relate(x, y, span, false);
                }
                return self.resolve(&b);
            }
            (Ty::Named(i, xs), Ty::Named(j, ys)) if i == j && xs.len() == ys.len() => {
                for (x, y) in xs.iter().zip(ys) {
                    self.relate(x, y, span, false);
                }
                return self.resolve(&b);
            }
            (Ty::Callable(left, args, signature), Ty::Callable(right, wanted, expected))
                if left == right && args.len() == wanted.len() =>
            {
                for (argument, expected) in args.iter().zip(wanted) {
                    self.relate(argument, expected, span, false);
                }
                self.relate(signature, expected, span, false);
                return self.resolve(&b);
            }
            (Ty::Callable(_, _, signature), Ty::Function(_, _)) if coercion => {
                self.relate(signature, &b, span, true);
                return self.resolve(&b);
            }
            (Ty::Function(xs, x), Ty::Function(ys, y)) if xs.len() == ys.len() => {
                for (x, y) in xs.iter().zip(ys) {
                    self.relate(x, y, span, false);
                }
                self.relate(x, y, span, coercion);
                return self.resolve(&b);
            }
            _ => {}
        }
        self.error(
            DiagnosticCode::InvalidExpression,
            format!(
                "类型不一致：{} 与 {}",
                self.model.describe(&a),
                self.model.describe(&b)
            ),
            span.clone(),
        );
        Ty::Error
    }
    fn function(&mut self, id: FnId, expected: Option<&Ty>) -> Ty {
        self.register_callable_bounds(id);
        self.register_apits(id);
        let f = &self.arena().fns[id.0 as usize];
        let opaque_hint = match expected
            .map(|ty| self.model.opaque_function(ty))
            .transpose()
        {
            Ok(signature) => signature.flatten(),
            Err(error) => {
                self.errors.push(error);
                None
            }
        };
        let expected = opaque_hint.as_ref().or(expected).and_then(Ty::signature);
        self.return_ty = if let Some(ty) = f.return_ty {
            self.form(ty)
        } else if f.name.is_none() {
            expected.map_or_else(
                || {
                    if matches!(f.body, FnBody::Eq(_)) {
                        self.fresh()
                    } else {
                        Ty::Unit
                    }
                },
                |(_, ret)| ret.clone(),
            )
        } else {
            Ty::Unit
        };
        let mut parameters = Vec::new();
        for (offset, param) in f.params.as_slice(&self.arena().params).iter().enumerate() {
            if !self.model.modules[self.module]
                .configured
                .param_active(f.params.start as usize + offset)
            {
                continue;
            }
            let ty = match param.ty {
                Some(id) => self.form(id),
                None if self.model.is_receiver(self.module, param) => self
                    .model
                    .parameters_at(self.module, &f.span)
                    .get("Self")
                    .cloned()
                    .unwrap_or_else(|| {
                        self.error(
                            DiagnosticCode::InvalidDeclaration,
                            "self 只能用于关联方法",
                            param.span.clone(),
                        );
                        Ty::Error
                    }),
                None => expected
                    .and_then(|(params, _)| params.get(parameters.len()))
                    .cloned()
                    .unwrap_or_else(|| self.fresh()),
            };
            if param.variadic {
                let trailing = f
                    .params
                    .as_slice(&self.arena().params)
                    .iter()
                    .enumerate()
                    .skip(offset + 1)
                    .any(|(next, _)| {
                        self.model.modules[self.module]
                            .configured
                            .param_active(f.params.start as usize + next)
                    });
                if trailing
                    || !matches!(&ty, Ty::Ref(inner) if matches!(**inner, Ty::Slice(_)))
                        && !self.is_parameter_pack(id, &ty)
                {
                    self.error(
                        DiagnosticCode::InvalidDeclaration,
                        "变参必须是最后一个参数，类型为切片或类型参数包",
                        param.span.clone(),
                    );
                }
            }
            if !param.variadic && self.is_parameter_pack(id, &ty) {
                self.error(
                    DiagnosticCode::InvalidDeclaration,
                    "类型参数包只能作为变参包使用",
                    param.span.clone(),
                );
            }
            parameters.push(ty.clone());
            if let Some(pat) = param.pat {
                self.bind(pat, &ty, true, Some(false));
            }
            if let Some(name) = param.variadic_name {
                self.slot(name, ty, true);
            }
        }
        if f.name
            .is_some_and(|name| self.model.name(self.module, name) == "main")
        {
            let valid_ret = self.return_ty == Ty::Unit
                || matches!(&self.return_ty,Ty::Result(t,_) if **t==Ty::Unit);
            if f.params
                .as_slice(&self.arena().params)
                .iter()
                .enumerate()
                .any(|(i, _)| {
                    self.model.modules[self.module]
                        .configured
                        .param_active(usize::try_from(f.params.start).expect("参数下标") + i)
                })
                || !valid_ret
                || f.generics.len != 0
            {
                self.error(
                    DiagnosticCode::InvalidMainSignature,
                    "main 必须无参数、无泛型并返回 () 或 Result[(), E]",
                    f.span.clone(),
                );
            }
        }
        if let FnBody::Block(body) | FnBody::Eq(body) = f.body {
            let expected = self.return_ty.clone();
            let actual = self.expression(body, Some(&expected));
            if f.name.is_none()
                && f.return_ty.is_none()
                && actual == Ty::Never
                && matches!(self.resolve(&self.return_ty), Ty::Var(_) | Ty::Never)
            {
                self.error(
                    DiagnosticCode::InvalidDeclaration,
                    "never 闭包必须显式写出返回类型 !",
                    f.span.clone(),
                );
            }
            self.run_cleanups(0, true);
            self.defers.clear();
            if self.state.reachable {
                self.require_value_captures(body);
            }
        }
        Ty::Function(parameters, Box::new(self.return_ty.clone()))
    }
    fn slot(&mut self, name: Symbol, ty: Ty, initialized: bool) {
        let id = self.slots.len();
        self.slots.push(Slot { ty, storage: 0 });
        self.state.initialized.resize(id + 1, false);
        self.state.callables.resize_with(id + 1, Vec::new);
        self.initialize(id, initialized);
        self.state.names.insert(name, id);
    }
    fn initialize(&mut self, slot: usize, value: bool) {
        if value {
            self.capture_slot(slot, false);
        }
        self.state.initialized[slot] = value;
        for path in self.state.cleanup_paths.values_mut() {
            path.initialized.resize(self.slots.len(), false);
            path.initialized[slot] = value;
        }
    }
    fn bind(&mut self, pat: PatId, ty: &Ty, initialized: bool, refutable: Option<bool>) -> bool {
        let ty = if refutable == Some(true) {
            self.pattern_type(ty)
        } else {
            self.resolve(ty)
        };
        match patterns::check(self.model, self.module, pat, &ty) {
            Ok(result) => {
                if refutable.is_some_and(|required| required == result.irrefutable) {
                    self.error(
                        DiagnosticCode::InvalidPattern,
                        if result.irrefutable {
                            "此处必须使用可驳模式"
                        } else {
                            "此处必须使用不可驳模式"
                        },
                        self.arena().pats[pat.0 as usize].span.clone(),
                    );
                }
                let start = self.slots.len();
                for binding in result.bindings {
                    debug_assert!(binding.span.start() <= binding.span.end());
                    self.slot(binding.name, binding.ty, initialized);
                }
                self.pattern_plans.push(super::output::PatternPlan {
                    pattern: pat,
                    ty,
                    bound_slots: start..self.slots.len(),
                    irrefutable: result.irrefutable,
                });
                result.irrefutable
            }
            Err(errors) => {
                self.errors.extend(errors);
                false
            }
        }
    }
    fn local(&mut self, name: Symbol, read: bool, span: &Span) -> Option<Ty> {
        let id = *self.state.names.get(&name)?;
        let captured = self.capture_slot(id, read);
        if read
            && !captured
            && self.state.reachable
            && self.state.initialized.get(id) != Some(&true)
        {
            self.error(
                DiagnosticCode::InvalidDeclaration,
                "读取未初始化的局部槽",
                span.clone(),
            );
        }
        Some(self.resolve(&self.slots[id].ty))
    }
}

fn contains_var(ty: &Ty, id: u32) -> bool {
    match ty {
        Ty::Var(v) => *v == id,
        Ty::Ref(t)
        | Ty::Ptr(t)
        | Ty::Slice(t)
        | Ty::Array(t, _)
        | Ty::Option(t)
        | Ty::Chan(t)
        | Ty::Join(t) => contains_var(t, id),
        Ty::Result(t, e) => contains_var(t, id) || contains_var(e, id),
        Ty::Tuple(ts) | Ty::Named(_, ts) | Ty::Opaque(_, ts) => {
            ts.iter().any(|t| contains_var(t, id))
        }
        Ty::Function(ts, ret) => ts.iter().any(|t| contains_var(t, id)) || contains_var(ret, id),
        Ty::Callable(_, arguments, signature) => {
            arguments.iter().any(|ty| contains_var(ty, id)) || contains_var(signature, id)
        }
        Ty::Dyn(interfaces) => interfaces
            .iter()
            .flat_map(|interface| &interface.arguments)
            .any(|ty| contains_var(ty, id)),
        Ty::Projection(base, interface, _) => {
            contains_var(base, id) || interface.arguments.iter().any(|ty| contains_var(ty, id))
        }
        _ => false,
    }
}
