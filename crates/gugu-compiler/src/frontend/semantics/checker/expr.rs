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
            ExprKind::Path(path) => self.path_value(id, path, true, expected, AstRange::empty()),
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
            } => {
                let ty = self.place(inner, true);
                self.address_taken(inner);
                Ty::Ref(Box::new(ty))
            }
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
            ExprKind::Call {
                callee,
                type_args,
                args,
            } => self.call(callee, type_args, args, expected),
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
                self.index(id, &ty, index, false, &expr.span)
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
                if let Some(value) = value {
                    self.require_value_captures(value);
                }
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
                    ty => self.try_parts(&ty, &expr.span).0,
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
                    ty => {
                        let result_value = self.try_parts(ty, &expr.span).0;
                        self.unify(&value, &result_value, &expr.span);
                        self.language_method(id, ty, "Try", Vec::new(), "from_value", &expr.span);
                    }
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
                    let wrapper = match &ty {
                        Ty::Option(_) => Ty::Option(Box::new(self.fresh())),
                        Ty::Result(_, error) => Ty::Result(Box::new(self.fresh()), error.clone()),
                        _ => ty.clone(),
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
                let target = self.resolve(&target);
                let (value, error) = self.try_parts(&ty, &expr.span);
                let (_, target_error) = self.try_parts(&target, &expr.span);
                self.relate(&error, &target_error, &expr.span, false);
                if !matches!(ty, Ty::Option(_) | Ty::Result(_, _) | Ty::Error) {
                    self.language_method(id, &ty, "Try", Vec::new(), "branch", &expr.span);
                }
                if !matches!(target, Ty::Option(_) | Ty::Result(_, _) | Ty::Error) {
                    self.language_method(id, &target, "Try", Vec::new(), "from_error", &expr.span);
                }
                value
            }
            ExprKind::Unsafe(body) => {
                self.unsafe_depth += 1;
                let ty = self.expression(body, expected);
                self.unsafe_depth -= 1;
                ty
            }
            ExprKind::Comptime(body) => self.expression(body, expected),
            ExprKind::Async(body) => self.launch(id, body, expected),
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
                    IntrinsicKind::TypeId => {
                        let ty = match tys.as_slice(&self.arena().generic_args) {
                            [argument] => match self.model.form_argument(self.module, *argument) {
                                Ok(ty) => ty,
                                Err(error) => {
                                    self.errors.push(error);
                                    Ty::Error
                                }
                            },
                            _ => {
                                self.error(
                                    DiagnosticCode::InvalidExpression,
                                    "type_id 需要一个目标类型",
                                    expr.span.clone(),
                                );
                                Ty::Error
                            }
                        };
                        if ty == Ty::Never {
                            self.error(
                                DiagnosticCode::InvalidType,
                                "! 没有 TypeId",
                                expr.span.clone(),
                            );
                        }
                        self.reflections.push(super::super::output::Reflection {
                            expression: id,
                            kind: super::super::output::ReflectionKind::TypeId(ty),
                        });
                        Ty::TypeId
                    }
                    IntrinsicKind::TypeIdCount => {
                        self.reflections.push(super::super::output::Reflection {
                            expression: id,
                            kind: super::super::output::ReflectionKind::TypeIdCount,
                        });
                        Ty::int()
                    }
                    _ => Ty::int(),
                }
            }
            ExprKind::TypeApp { base, args } => self.typed_callable(base, args, expected),
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
            ExprKind::Closure(function) => self.closure(id, function, expected),
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
        let checked = if ty == Ty::Never {
            Ty::Never
        } else {
            self.normalized(&checked)
        };
        self.record_erasure(id, &ty, &checked);
        self.expressions.push((id, checked.clone()));
        self.record_callable_value(id);
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
    pub(super) fn path_value(
        &mut self,
        expression: ExprId,
        path: PathId,
        read: bool,
        expected: Option<&Ty>,
        type_args: AstRange<GenericArg>,
    ) -> Ty {
        let a = self.arena();
        let p = &a.paths[path.0 as usize];
        let segs = p.segments.as_slice(&a.segments);
        let path_arguments = segs.last().expect("路径至少有一段").args;
        if let Some(first) = segs.first() {
            if let Some(mut ty) = self.local(first.name, read || segs.len() > 1, &p.span) {
                if type_args.len != 0 {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "已绑定函数值不能重新指定泛型实参",
                        p.span.clone(),
                    );
                }
                for s in &segs[1..] {
                    ty = self.field(&ty, self.model.name(self.module, s.name), &s.span);
                }
                if path_arguments.len != 0 {
                    return self.path_index(expression, &ty, path_arguments, !read, &p.span);
                }
                return ty;
            }
        }
        let parts = self.model.path(self.module, path);
        if let Some(ty) = self.associated_constant(path) {
            return ty;
        }
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
                if matches!(
                    self.model.modules[def.module].arena.items[def.item.0 as usize].kind,
                    ItemKind::Static { .. } | ItemKind::Const { .. }
                ) && !self.dependencies.contains(&def)
                {
                    self.dependencies.push(def);
                }
                match self.model.value_type(def) {
                    Ok(ty) if path_arguments.len != 0 => {
                        self.path_index(expression, &ty, path_arguments, !read, &p.span)
                    }
                    Ok(ty) => self.instantiate_callable(ty, type_args, &p.span),
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

    fn path_index(
        &mut self,
        expression: ExprId,
        ty: &Ty,
        arguments: AstRange<GenericArg>,
        write: bool,
        span: &Span,
    ) -> Ty {
        let [argument] = arguments.as_slice(&self.arena().generic_args) else {
            self.error(
                DiagnosticCode::InvalidExpression,
                "值的方括号下标需要一个参数",
                span.clone(),
            );
            return Ty::Error;
        };
        match *argument {
            GenericArg::Expr(index) => {
                self.index(expression, ty, IndexKind::Expr(index), write, span)
            }
            GenericArg::Type(index) => {
                let TyKind::Path(path) = self.arena().tys[index.0 as usize].kind else {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "值下标不能是类型",
                        span.clone(),
                    );
                    return Ty::Error;
                };
                let index_ty =
                    self.path_value(expression, path, true, Some(&Ty::int()), AstRange::empty());
                self.unify(&index_ty, &Ty::int(), span);
                match ty.deref() {
                    Ty::Array(element, _) | Ty::Slice(element) => {
                        if self.unsafe_depth == 0 {
                            self.record_check(
                                expression,
                                super::super::output::CheckKind::Bounds { slice: false },
                            );
                        }
                        (**element).clone()
                    }
                    _ => self.user_index(expression, ty, write, span),
                }
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
                self.path_value(id, path, read, None, AstRange::empty())
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
                self.index(id, &ty, index, !read, &expr.span)
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
    pub(super) fn field(&mut self, ty: &Ty, name: &str, span: &Span) -> Ty {
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
    fn index(&mut self, id: ExprId, ty: &Ty, index: IndexKind, write: bool, span: &Span) -> Ty {
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
                    _ => self.user_index(id, ty, write, span),
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
    fn construct(
        &mut self,
        path: PathId,
        fields: AstRange<FieldExpr>,
        expected: Option<&Ty>,
        span: &Span,
    ) -> Ty {
        let mut parts = self.model.path(self.module, path);
        let scope = self.model.parameters_at(self.module, span);
        let arguments = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments)
            .last()
            .expect("非空路径")
            .args;
        let target = if parts == ["Self"] {
            scope.get("Self").cloned()
        } else if arguments.len != 0 {
            match self
                .model
                .form_kind(self.module, TyKind::Path(path), &scope, &mut Vec::new())
            {
                Ok(ty) => Some(ty),
                Err(error) => {
                    self.errors.push(error);
                    return Ty::Error;
                }
            }
        } else if expected.is_some() {
            expected.cloned()
        } else {
            self.model
                .resolve(self.module, &parts)
                .ok()
                .and_then(|definition| {
                    self.model
                        .nominal
                        .iter()
                        .position(|nominal| nominal.definition == definition)
                })
                .map(|index| {
                    Ty::Named(
                        index,
                        (0..self.model.nominal[index].params.len())
                            .map(|_| self.fresh())
                            .collect(),
                    )
                })
        };
        if parts == ["Self"]
            && let Some(Ty::Named(index, _)) = &target
        {
            parts = vec![self.model.nominal[*index].name.as_str()];
        }
        match self.model.constructor(self.module, &parts, target.as_ref()) {
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
