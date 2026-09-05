use super::*;
impl Checker<'_, '_> {
    pub(super) fn expression(&mut self, id: ExprId, expected: Option<&Ty>) -> Ty {
        let expr = &self.arena().exprs[id.0 as usize];
        let ty = match expr.kind {
            ExprKind::Literal(lit) => match lit {
                LitKind::Int { .. } | LitKind::Float { .. } => {
                    self.number_literal(lit, false, expected, &expr.span)
                }
                LitKind::Bool(_) => Ty::Bool,
                LitKind::Char { .. } => Ty::Char,
                LitKind::ByteChar { .. } => Ty::Int {
                    signed: false,
                    bits: 8,
                },
                LitKind::String { .. } | LitKind::RawString { .. } => Ty::String,
                LitKind::ByteString { .. } => Ty::Ref(Box::new(Ty::Slice(Box::new(Ty::Int {
                    signed: false,
                    bits: 8,
                })))),
                LitKind::CString { .. } => Ty::Ptr(Box::new(Ty::Int {
                    signed: false,
                    bits: 8,
                })),
            },
            ExprKind::Path(path) => self.path_value(path, true, expected),
            ExprKind::Paren(inner) => self.expression(inner, expected),
            ExprKind::Block { stmts, tail } => self.block(stmts, tail, expected),
            ExprKind::If {
                cond,
                then_block,
                else_branch,
            } => self.branch(
                cond,
                then_block,
                else_branch,
                expected,
                self.discarded_expression != Some(id),
            ),
            ExprKind::Match { scrutinee, arms } => self.matching(scrutinee, arms, expected),
            ExprKind::Loop(body) => self.looping(body, None, None),
            ExprKind::While { cond, body } => self.looping(body, Some(cond), None),
            ExprKind::For { pat, iter, body } => self.looping(body, None, Some((pat, iter))),
            ExprKind::Tuple(items) => {
                let mut ts = Vec::new();
                for &id in items.as_slice(&self.arena().expr_ids) {
                    if !self.model.modules[self.module].configured.expr_active(id) {
                        continue;
                    }
                    let t = self.expression(
                        id,
                        match expected {
                            Some(Ty::Tuple(expected)) => expected.get(ts.len()),
                            _ => None,
                        },
                    );
                    ts.push(t);
                }
                if ts.is_empty() {
                    Ty::Unit
                } else {
                    Ty::Tuple(ts)
                }
            }
            ExprKind::Array(items) => {
                let mut element = match expected {
                    Some(Ty::Array(t, _)) => Some((**t).clone()),
                    _ => None,
                };
                let mut n = 0;
                for &id in items.as_slice(&self.arena().expr_ids) {
                    if !self.model.modules[self.module].configured.expr_active(id) {
                        continue;
                    }
                    let ty = self.expression(id, element.as_ref());
                    element = Some(ty);
                    n += 1;
                }
                Ty::Array(Box::new(element.unwrap_or_else(|| self.fresh())), n)
            }
            ExprKind::Repeat { elem, count } => {
                let ty = self.expression(
                    elem,
                    match expected {
                        Some(Ty::Array(t, _)) => Some(t),
                        _ => None,
                    },
                );
                match self.model.constant_int(self.module, count).and_then(|n| {
                    u64::try_from(n).map_err(|_| {
                        Diagnostic::error(
                            DiagnosticCode::InvalidType,
                            "重复次数必须非负",
                            Some(expr.span.clone()),
                        )
                    })
                }) {
                    Ok(n) => Ty::Array(Box::new(ty), n),
                    Err(e) => {
                        self.errors.push(e);
                        Ty::Error
                    }
                }
            }
            ExprKind::Binary { op, lhs, rhs } => self.binary(id, op, lhs, rhs, expected),
            ExprKind::Unary {
                op: UnOp::Neg,
                expr: inner,
            } if matches!(
                self.arena().exprs[inner.0 as usize].kind,
                ExprKind::Literal(LitKind::Int { .. })
            ) =>
            {
                let ExprKind::Literal(lit) = self.arena().exprs[inner.0 as usize].kind else {
                    unreachable!()
                };
                let ty = self.number_literal(lit, true, expected, &expr.span);
                self.expressions.push((inner, ty.clone()));
                ty
            }
            ExprKind::Unary {
                op: UnOp::Ref,
                expr: inner,
            } => Ty::Ref(Box::new(self.place(inner, true))),
            ExprKind::Unary {
                op: UnOp::Deref,
                expr: inner,
            } => {
                let ty = self.expression(inner, None);
                match ty {
                    Ty::Ref(t) => *t,
                    Ty::Ptr(t) if self.unsafe_depth > 0 => *t,
                    _ => {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "解引用需要引用，原始指针还需要 unsafe",
                            expr.span.clone(),
                        );
                        Ty::Error
                    }
                }
            }
            ExprKind::Unary { op, expr: inner } => {
                let ty = self.expression(inner, expected);
                let valid = match op {
                    UnOp::Not => ty == Ty::Bool,
                    UnOp::Neg => self.require_signed(&ty, &expr.span),
                    UnOp::BitNot => self.is_integer(&ty),
                    _ => false,
                };
                if !valid {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "一元运算符与操作数类型不兼容",
                        expr.span.clone(),
                    );
                }
                ty
            }
            ExprKind::Range { start, end } => {
                self.expression(start, Some(&Ty::int()));
                self.expression(end, Some(&Ty::int()));
                Ty::Range
            }
            ExprKind::Call { callee, args, .. } => self.call(callee, args, expected),
            ExprKind::Field { base, name } => {
                let ty = self.expression(base, None);
                self.field(&ty, self.model.name(self.module, name), &expr.span)
            }
            ExprKind::TupleField { base, index } => {
                let ty = self.expression(base, None);
                self.field(&ty, &index.to_string(), &expr.span)
            }
            ExprKind::Index { base, index } => {
                let ty = self.expression(base, None);
                self.index(id, &ty, index, &expr.span)
            }
            ExprKind::Struct { path, fields } => self.construct(path, fields, expected, &expr.span),
            ExprKind::Return(value) => {
                if self.in_cleanup {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "defer 体不能 return",
                        expr.span.clone(),
                    );
                }
                let expected = self.return_ty.clone();
                let ty = value.map_or(Ty::Unit, |v| self.expression(v, Some(&expected)));
                self.unify(&ty, &expected, &expr.span);
                self.run_cleanups(0, true);
                self.state.reachable = false;
                Ty::Never
            }
            ExprKind::Break(value) => {
                let ty = value.map_or(Ty::Unit, |v| self.expression(v, None));
                if let Some(context) = self.loops.last() {
                    self.run_cleanups(context.cleanup_floor, false);
                }
                if let Some(loop_state) = self.loops.last_mut() {
                    if value.is_some() && !loop_state.value_allowed {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "while/for 不能 break 值",
                            expr.span.clone(),
                        );
                    } else {
                        loop_state.values.push(ty);
                        if self.state.reachable {
                            loop_state.exits.push(self.state.clone());
                        }
                    }
                } else {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "break 不在循环内",
                        expr.span.clone(),
                    );
                }
                self.state.reachable = false;
                Ty::Never
            }
            ExprKind::Continue => {
                if self.loops.is_empty() {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "continue 不在循环内",
                        expr.span.clone(),
                    );
                }
                if let Some(context) = self.loops.last() {
                    self.run_cleanups(context.cleanup_floor, false);
                }
                self.state.reachable = false;
                Ty::Never
            }
            ExprKind::Try(body) => {
                let target = expected.cloned().unwrap_or_else(|| self.fresh());
                let value = match self.resolve(&target) {
                    Ty::Option(t) | Ty::Result(t, _) => *t,
                    Ty::Var(_) => self.fresh(),
                    _ => {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "try 需要 Try 类型",
                            expr.span.clone(),
                        );
                        Ty::Error
                    }
                };
                let base = self.state.clone();
                self.tries.push(TryState {
                    ty: target.clone(),
                    failures: Vec::new(),
                    cleanup_floor: self.defers.len(),
                });
                let body_ty = self.expression(body, Some(&value));
                let mut context = self.tries.pop().expect("try 上下文");
                let result = self.resolve(&target);
                match &result {
                    Ty::Option(t) | Ty::Result(t, _) => {
                        self.unify(&value, t, &expr.span);
                    }
                    Ty::Var(_) => self.error(
                        DiagnosticCode::InvalidType,
                        "try 的出口类型无法唯一确定",
                        expr.span.clone(),
                    ),
                    _ => {}
                }
                let success = self.state.clone();
                if success.reachable {
                    context.failures.push(success);
                }
                self.merge(&base, &context.failures);
                if body_ty == Ty::Never && !self.state.reachable {
                    Ty::Never
                } else {
                    self.resolve(&target)
                }
            }
            ExprKind::TryOp(inner) => {
                let ty = self.expression(inner, None);
                let target = self
                    .tries
                    .last()
                    .map_or_else(|| self.return_ty.clone(), |context| context.ty.clone());
                if matches!(self.resolve(&target), Ty::Var(_)) {
                    let value = self.fresh();
                    let wrapper = match &ty {
                        Ty::Option(_) => Ty::Option(Box::new(value)),
                        Ty::Result(_, error) => Ty::Result(Box::new(value), error.clone()),
                        _ => Ty::Error,
                    };
                    self.unify(&wrapper, &target, &expr.span);
                }
                let success = self.state.clone();
                let floor = self.tries.last().map_or(0, |context| context.cleanup_floor);
                self.run_cleanups(floor, self.tries.is_empty());
                if self.state.reachable
                    && let Some(context) = self.tries.last_mut()
                {
                    context.failures.push(self.state.clone());
                }
                self.state = success;
                if self.in_cleanup {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "defer 体不能使用 ? 离开当前作用域",
                        expr.span.clone(),
                    );
                }
                match (&ty, self.resolve(&target)) {
                    (Ty::Option(t), Ty::Option(_)) => (**t).clone(),
                    (Ty::Result(t, e), Ty::Result(_, f)) => {
                        self.relate(e, &f, &expr.span, false);
                        (**t).clone()
                    }
                    _ => {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "? 操作数与最近出口必须具有相同 Try 错误类型",
                            expr.span.clone(),
                        );
                        Ty::Error
                    }
                }
            }
            ExprKind::Unsafe(body) => {
                self.unsafe_depth += 1;
                let ty = self.expression(body, expected);
                self.unsafe_depth -= 1;
                ty
            }
            ExprKind::Comptime(body) => self.expression(body, expected),
            ExprKind::Async(body) => {
                let state = self.state.clone();
                let ty = self.expression(body, None);
                self.state = state;
                Ty::Join(Box::new(ty))
            }
            ExprKind::FString { parts } => {
                for part in parts.as_slice(&self.arena().fstring_parts) {
                    if let FStringPart::Interp { expr, .. } = part {
                        self.expression(*expr, None);
                    }
                }
                Ty::String
            }
            ExprKind::Intrinsic {
                kind, tys, args, ..
            } => {
                for &arg in args.as_slice(&self.arena().expr_ids) {
                    self.expression(arg, Some(&Ty::int()));
                }
                match kind {
                    IntrinsicKind::Chan => {
                        let ty = match tys.as_slice(&self.arena().generic_args) {
                            [GenericArg::Type(t)] => self.form(*t),
                            _ => Ty::Error,
                        };
                        Ty::Chan(Box::new(ty))
                    }
                    _ => Ty::int(),
                }
            }
            ExprKind::TypeApp { base, .. } => self.expression(base, expected),
            ExprKind::Select { arms } => {
                let base = self.state.clone();
                let mut states = Vec::new();
                let mut result = Ty::Never;
                for (i, arm) in arms.as_slice(&self.arena().select_arms).iter().enumerate() {
                    if !self.model.modules[self.module]
                        .configured
                        .select_arm_active(arms.start as usize + i)
                    {
                        continue;
                    }
                    self.state = base.clone();
                    let body = match arm.kind {
                        SelectArmKind::Default { body } => body,
                        SelectArmKind::Send {
                            chan,
                            payload,
                            body,
                        } => {
                            let ty = self.expression(chan, None);
                            if let Ty::Chan(t) = ty {
                                self.expression(payload, Some(&t));
                            } else {
                                self.error(
                                    DiagnosticCode::InvalidExpression,
                                    "send 要求 channel",
                                    arm.span.clone(),
                                );
                            }
                            body
                        }
                        SelectArmKind::Recv { pat, chan, body } => {
                            let ty = self.expression(chan, None);
                            if let Ty::Chan(t) = ty {
                                self.bind(
                                    pat,
                                    &Ty::Result(t, Box::new(Ty::Unit)),
                                    true,
                                    Some(false),
                                );
                            } else {
                                self.error(
                                    DiagnosticCode::InvalidExpression,
                                    "recv 要求 channel",
                                    arm.span.clone(),
                                );
                            }
                            body
                        }
                        SelectArmKind::Wait { pat, join, body } => {
                            let ty = self.expression(join, None);
                            if let Ty::Join(t) = ty {
                                self.bind(
                                    pat,
                                    &Ty::Result(t, Box::new(Ty::Unit)),
                                    true,
                                    Some(false),
                                );
                            } else {
                                self.error(
                                    DiagnosticCode::InvalidExpression,
                                    "wait 要求 Join",
                                    arm.span.clone(),
                                );
                            }
                            body
                        }
                        SelectArmKind::Error => continue,
                    };
                    let ty = self.expression(body, expected);
                    result = self.join(&ty, &result, &arm.span);
                    states.push(self.state.clone());
                }
                self.merge(&base, &states);
                result
            }
            ExprKind::Closure(id) => {
                let saved = self.state.clone();
                let ret = self.return_ty.clone();
                let loops = std::mem::take(&mut self.loops);
                let tries = std::mem::take(&mut self.tries);
                self.function(id);
                let f = &self.arena().fns[id.0 as usize];
                let params = f
                    .params
                    .as_slice(&self.arena().params)
                    .iter()
                    .map(|p| p.ty.map_or(Ty::Error, |t| self.form(t)))
                    .collect();
                let ty = Ty::Function(params, Box::new(self.return_ty.clone()));
                self.state = saved;
                self.return_ty = ret;
                self.loops = loops;
                self.tries = tries;
                ty
            }
            ExprKind::Asm { .. } | ExprKind::SourceMacro { .. } | ExprKind::Error => {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "此节点尚未完成对应语义形成",
                    expr.span.clone(),
                );
                Ty::Error
            }
        };
        if ty == Ty::Never {
            self.state.reachable = false;
        }
        let checked = if let (Ty::Ref(source), Some(Ty::Ref(target))) = (&ty, expected)
            && let (Ty::Array(element, _), Ty::Slice(wanted)) = (&**source, &**target)
        {
            self.unify(element, wanted, &expr.span);
            Ty::Ref(target.clone())
        } else if let Some(expected) = expected {
            self.unify(&ty, expected, &expr.span)
        } else {
            ty.clone()
        };
        let checked = if ty == Ty::Never { Ty::Never } else { checked };
        self.expressions.push((id, self.resolve(&checked)));
        checked
    }
    pub(super) fn binary_type(&mut self, op: BinOp, left: &Ty, right: &Ty, span: &Span) -> Ty {
        let ty = self.unify(left, right, span);
        let valid = match op {
            BinOp::And | BinOp::Or => ty == Ty::Bool,
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                self.is_integer(&ty)
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.builtin_comparable(&ty)
            }
            BinOp::Add => self.is_number(&ty) || ty == Ty::String,
            _ => self.is_number(&ty),
        };
        if !valid && ty != Ty::Error {
            self.error(
                DiagnosticCode::InvalidExpression,
                "运算符没有匹配的内置操作或 trait 候选",
                span.clone(),
            );
        }
        if matches!(
            op,
            BinOp::Eq
                | BinOp::Ne
                | BinOp::Lt
                | BinOp::Le
                | BinOp::Gt
                | BinOp::Ge
                | BinOp::And
                | BinOp::Or
        ) {
            Ty::Bool
        } else {
            ty
        }
    }
    fn path_value(&mut self, path: PathId, read: bool, expected: Option<&Ty>) -> Ty {
        let a = self.arena();
        let p = &a.paths[path.0 as usize];
        let segs = p.segments.as_slice(&a.segments);
        if let Some(first) = segs.first() {
            if let Some(mut ty) = self.local(first.name, read || segs.len() > 1, &p.span) {
                for s in &segs[1..] {
                    ty = self.field(&ty, self.model.name(self.module, s.name), &s.span);
                }
                return ty;
            }
        }
        let parts = self.model.path(self.module, path);
        if parts == ["None"] {
            return expected
                .filter(|ty| matches!(ty, Ty::Option(_)))
                .cloned()
                .unwrap_or_else(|| Ty::Option(Box::new(self.fresh())));
        }
        if let Ok(Some((ty, ctor))) = self.model.constructor(self.module, &parts, expected) {
            if ctor.fields.is_empty() {
                return ty;
            }
        }
        match self.model.resolve(self.module, &parts) {
            Ok(def) => {
                if !self.dependencies.contains(&def) {
                    self.dependencies.push(def);
                }
                match self.model.value_type(def) {
                    Ok(ty) => ty,
                    Err(error) => {
                        self.errors.push(error);
                        Ty::Error
                    }
                }
            }
            Err(error) => {
                self.error(
                    error.code(),
                    error.message(),
                    a.paths[path.0 as usize].span.clone(),
                );
                Ty::Error
            }
        }
    }
    pub(super) fn place(&mut self, id: ExprId, read: bool) -> Ty {
        let expr = &self.arena().exprs[id.0 as usize];
        let ty = match expr.kind {
            ExprKind::Path(path) => {
                let segments = self.arena().paths[path.0 as usize]
                    .segments
                    .as_slice(&self.arena().segments);
                let local = segments
                    .first()
                    .is_some_and(|segment| self.state.names.contains_key(&segment.name));
                if !local
                    && let Ok(def) = self
                        .model
                        .resolve(self.module, &self.model.path(self.module, path))
                    && !matches!(
                        self.model.modules[def.module].arena.items[def.item.0 as usize].kind,
                        ItemKind::Static { .. }
                    )
                {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "常量、函数与构造器没有可赋值或取引用的槽",
                        expr.span.clone(),
                    );
                    return Ty::Error;
                }
                self.path_value(path, read, None)
            }
            ExprKind::Paren(inner) => self.place(inner, read),
            ExprKind::Field { base, name } => {
                let ty = self.place(base, true);
                self.field(&ty, self.model.name(self.module, name), &expr.span)
            }
            ExprKind::TupleField { base, index } => {
                let ty = self.place(base, true);
                self.field(&ty, &index.to_string(), &expr.span)
            }
            ExprKind::Index { base, index } => {
                let ty = self.place(base, true);
                self.index(id, &ty, index, &expr.span)
            }
            ExprKind::Unary {
                op: UnOp::Deref, ..
            } => self.expression(id, None),
            _ => {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "需要可寻址槽，不能使用临时值",
                    expr.span.clone(),
                );
                Ty::Error
            }
        };
        self.expressions.push((id, ty.clone()));
        ty
    }
    fn field(&mut self, ty: &Ty, name: &str, span: &Span) -> Ty {
        if let Ty::Tuple(ts) = ty.deref() {
            if let Ok(i) = name.parse::<usize>() {
                if let Some(ty) = ts.get(i) {
                    return ty.clone();
                }
            }
        }
        if let Some(fields) = self.model.fields(ty) {
            if let Some(f) = fields.iter().find(|f| f.name == name) {
                if !f.public && self.model.def_module(ty) != Some(self.module) {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "不能访问私有字段",
                        span.clone(),
                    );
                }
                return f.ty.clone();
            }
        }
        self.error(
            DiagnosticCode::InvalidExpression,
            format!("类型没有字段 `{name}`"),
            span.clone(),
        );
        Ty::Error
    }
    fn index(&mut self, id: ExprId, ty: &Ty, index: IndexKind, span: &Span) -> Ty {
        match index {
            IndexKind::Expr(i) => {
                self.expression(i, Some(&Ty::int()));
                match ty.deref() {
                    Ty::Array(t, _) | Ty::Slice(t) => {
                        if self.unsafe_depth == 0 {
                            self.record_check(
                                id,
                                super::super::output::CheckKind::Bounds { slice: false },
                            );
                        }
                        (**t).clone()
                    }
                    _ => {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "此类型不支持整数下标",
                            span.clone(),
                        );
                        Ty::Error
                    }
                }
            }
            IndexKind::Range { start, end } => {
                for i in [start, end].into_iter().flatten() {
                    self.expression(i, Some(&Ty::int()));
                }
                match ty.deref() {
                    Ty::Array(t, _) | Ty::Slice(t) => {
                        if self.unsafe_depth == 0 {
                            self.record_check(
                                id,
                                super::super::output::CheckKind::Bounds { slice: true },
                            );
                        }
                        Ty::Ref(Box::new(Ty::Slice(t.clone())))
                    }
                    Ty::String => {
                        if self.unsafe_depth == 0 {
                            self.record_check(
                                id,
                                super::super::output::CheckKind::Bounds { slice: true },
                            );
                            self.record_check(id, super::super::output::CheckKind::Utf8Boundary);
                        }
                        Ty::String
                    }
                    _ => {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "此类型不支持切片",
                            span.clone(),
                        );
                        Ty::Error
                    }
                }
            }
        }
    }
    pub(super) fn call(
        &mut self,
        callee: ExprId,
        args: AstRange<ExprId>,
        expected: Option<&Ty>,
    ) -> Ty {
        let expr = &self.arena().exprs[callee.0 as usize];
        let args: Vec<_> = args
            .as_slice(&self.arena().expr_ids)
            .iter()
            .copied()
            .filter(|&id| self.model.modules[self.module].configured.expr_active(id))
            .collect();
        if let ExprKind::Path(path) = expr.kind {
            let parts = self.model.path(self.module, path);
            if parts.len() == 2 {
                let receiver_name = self.arena().paths[path.0 as usize]
                    .segments
                    .as_slice(&self.arena().segments)[0]
                    .name;
                if let Some(receiver) = self.local(receiver_name, true, &expr.span) {
                    match (&receiver, parts[1]) {
                        (Ty::Chan(element), "send") => {
                            if args.len() == 1 {
                                self.expression(args[0], Some(element));
                            } else {
                                self.error(
                                    DiagnosticCode::InvalidExpression,
                                    "send 需要一个实参",
                                    expr.span.clone(),
                                );
                            }
                            return Ty::Unit;
                        }
                        (Ty::Chan(element), "recv") if args.is_empty() => {
                            return Ty::Result(element.clone(), Box::new(Ty::Unit));
                        }
                        (Ty::Chan(_), "close") if args.is_empty() => return Ty::Unit,
                        (Ty::Join(value), "wait") if args.is_empty() => {
                            return Ty::Result(value.clone(), Box::new(Ty::Unit));
                        }
                        (Ty::Chan(_), "recv") | (Ty::Chan(_), "close") | (Ty::Join(_), "wait") => {
                            self.error(
                                DiagnosticCode::InvalidExpression,
                                "并发方法实参数量不符",
                                expr.span.clone(),
                            );
                            return Ty::Error;
                        }
                        _ => {}
                    }
                }
            }
            if parts == ["panic"] {
                for &arg in &args {
                    self.expression(arg, None);
                }
                return Ty::Never;
            }
            if parts.len() == 1 {
                if let Some(target) = Ty::primitive(parts[0]) {
                    if args.len() != 1 {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "转换需要恰好一个实参",
                            expr.span.clone(),
                        );
                        return Ty::Error;
                    }
                    let source = self.expression(args[0], None);
                    if !(self.is_number(&source) || matches!(source, Ty::Char))
                        || !matches!(target, Ty::Int { .. } | Ty::Float(_) | Ty::Char)
                    {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "非法标量转换",
                            expr.span.clone(),
                        );
                    }
                    if self.number_kind(&source) == inference::NumberKind::Float
                        && let Ty::Int { signed, bits } = target
                    {
                        self.record_check(
                            callee,
                            super::super::output::CheckKind::FloatToInt { signed, bits },
                        );
                    }
                    if matches!(target, Ty::Char) {
                        self.record_check(callee, super::super::output::CheckKind::UnicodeScalar);
                    }
                    self.expressions
                        .push((callee, Ty::Function(vec![source], Box::new(target.clone()))));
                    return target;
                }
            }
            if let Some(name) = parts.last() {
                if matches!(*name, "Some" | "Ok" | "Err") {
                    let wanted = match (expected, *name) {
                        (Some(Ty::Option(t)), "Some") | (Some(Ty::Result(t, _)), "Ok") => {
                            Some(&**t)
                        }
                        (Some(Ty::Result(_, e)), "Err") => Some(&**e),
                        _ => None,
                    };
                    if args.len() != 1 {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "构造器实参数量不符",
                            expr.span.clone(),
                        );
                        return Ty::Error;
                    }
                    let value = self.expression(args[0], wanted);
                    return match *name {
                        "Some" => Ty::Option(Box::new(value)),
                        "Ok" => Ty::Result(
                            Box::new(value),
                            Box::new(match expected {
                                Some(Ty::Result(_, e)) => (**e).clone(),
                                _ => self.fresh(),
                            }),
                        ),
                        _ => Ty::Result(
                            Box::new(match expected {
                                Some(Ty::Result(t, _)) => (**t).clone(),
                                _ => self.fresh(),
                            }),
                            Box::new(value),
                        ),
                    };
                }
            }
            if let Ok(Some((ty, ctor))) = self.model.constructor(self.module, &parts, expected) {
                if ctor.record || ctor.fields.len() != args.len() {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "构造器形状或实参数量不符",
                        expr.span.clone(),
                    );
                }
                for (&arg, field) in args.iter().zip(&ctor.fields) {
                    if !field.public && self.model.def_module(&ty) != Some(self.module) {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "不能构造私有字段",
                            expr.span.clone(),
                        );
                    }
                    self.expression(arg, Some(&field.ty));
                }
                return ty;
            }
        }
        let ty = self.expression(callee, None);
        if let Ty::Function(params, ret) = ty {
            if params.len() != args.len() {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "调用实参数量不符",
                    expr.span.clone(),
                );
            }
            for (i, &arg) in args.iter().enumerate() {
                self.expression(arg, params.get(i));
            }
            *ret
        } else {
            for &arg in &args {
                self.expression(arg, None);
            }
            self.error(
                DiagnosticCode::InvalidExpression,
                "调用目标不是函数",
                expr.span.clone(),
            );
            Ty::Error
        }
    }
    fn construct(
        &mut self,
        path: PathId,
        fields: AstRange<FieldExpr>,
        expected: Option<&Ty>,
        span: &Span,
    ) -> Ty {
        let parts = self.model.path(self.module, path);
        match self.model.constructor(self.module, &parts, expected) {
            Ok(Some((ty, ctor))) => {
                let mut seen = std::collections::BTreeSet::new();
                for (i, f) in fields
                    .as_slice(&self.arena().field_exprs)
                    .iter()
                    .enumerate()
                {
                    if !self.model.modules[self.module]
                        .configured
                        .field_expr_active(fields.start as usize + i)
                    {
                        continue;
                    }
                    let name = self.model.name(self.module, f.name);
                    if !seen.insert(name) {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "重复初始化字段",
                            f.span.clone(),
                        );
                    }
                    if let Some(field) = ctor.fields.iter().find(|f| f.name == name) {
                        if !field.public && self.model.def_module(&ty) != Some(self.module) {
                            self.error(
                                DiagnosticCode::InvalidExpression,
                                "不能构造私有字段",
                                f.span.clone(),
                            );
                        }
                        if let Some(value) = f.value {
                            self.expression(value, Some(&field.ty));
                        } else if let Some(value) = self.local(f.name, true, &f.span) {
                            self.unify(&value, &field.ty, &f.span);
                        } else {
                            self.error(
                                DiagnosticCode::InvalidDeclaration,
                                "字段简写引用未定义绑定",
                                f.span.clone(),
                            );
                        }
                    } else {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "构造器没有该字段",
                            f.span.clone(),
                        );
                    }
                }
                if seen.len() != ctor.fields.len() {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "构造器缺少字段",
                        span.clone(),
                    );
                }
                ty
            }
            Ok(None) => {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "未知构造器",
                    span.clone(),
                );
                Ty::Error
            }
            Err(e) => {
                self.errors.push(e);
                Ty::Error
            }
        }
    }
}
