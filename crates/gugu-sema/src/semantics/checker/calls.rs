//! 调用目标、实参物化与参数包共享一条签名检查路径。
use super::*;

impl Checker<'_, '_> {
    pub(super) fn call(
        &mut self,
        expression: ExprId,
        callee: ExprId,
        type_args: AstRange<GenericArg>,
        args: AstRange<ExprId>,
        expected: Option<&Ty>,
    ) -> Ty {
        let previous = self.call_site.replace(expression);
        let ty = self.call_inner(callee, type_args, args, expected);
        if self.model.has_attribute(
            self.module,
            self.arena().exprs[expression.0 as usize].attributes,
            "ffi",
        ) && self
            .foreign_calls
            .last()
            .is_none_or(|call| call.expression != expression)
        {
            self.error(
                DiagnosticCode::InvalidExpression,
                "ffi 调用点属性需要直接 C 调用",
                self.arena().exprs[expression.0 as usize].span.clone(),
            );
        }
        self.call_site = previous;
        ty
    }

    fn call_inner(
        &mut self,
        callee: ExprId,
        type_args: AstRange<GenericArg>,
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
        if let Some(result) = self.conversion(callee, &args) {
            return result;
        }
        if let ExprKind::Path(path) = expr.kind {
            if let Some(result) = self.memory_call(callee, path, type_args, &args, expected) {
                return result;
            }
            if let Some(result) = self.runtime_call(callee, path, type_args, &args, expected) {
                return result;
            }
            if let Some(result) =
                self.runtime_machine_call(callee, path, type_args, &args, expected)
            {
                return result;
            }
            if let Some(result) = self.platform_call(callee, path, type_args, &args, expected) {
                return result;
            }
            if let Some(result) = self.src_call(callee, path, &args) {
                return result;
            }
            if let Some(result) = self.embed_call(callee, path, &args) {
                return result;
            }
            let parts = self.model.path(self.module, path);
            if parts == ["panic"] {
                if self.native_definition().is_some() {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "opaque native definition 不能 panic",
                        expr.span.clone(),
                    );
                }
                for &arg in &args {
                    self.expression(arg, None);
                }
                return Ty::Never;
            }
            if let Some(name) = parts.last()
                && matches!(*name, "Some" | "Ok" | "Err")
            {
                let wanted = match (expected, *name) {
                    (Some(Ty::Option(t)), "Some") | (Some(Ty::Result(t, _)), "Ok") => Some(&**t),
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
        if let Some(ret) = self.method_call(callee, type_args, &args, expected) {
            return ret;
        }
        let ty = self.typed_callable(callee, type_args, None);
        let ty = match &ty {
            Ty::Param(name) => self.callable_bounds.get(name).cloned().unwrap_or(ty),
            _ => ty,
        };
        self.invoke(callee, ty, args, None, expected)
    }

    pub(super) fn invoke(
        &mut self,
        callee: ExprId,
        ty: Ty,
        args: Vec<ExprId>,
        receiver: Option<Ty>,
        expected: Option<&Ty>,
    ) -> Ty {
        let expr = &self.arena().exprs[callee.0 as usize];
        if self.model.callable_is_unsafe(&ty) && self.unsafe_depth == 0 {
            self.error(
                DiagnosticCode::InvalidExpression,
                "调用 unsafe fn 必须处于显式 unsafe 块中",
                expr.span.clone(),
            );
        }
        let ty = match self.model.opaque_function(&ty) {
            Ok(Some(signature)) => signature,
            Ok(None) => ty,
            Err(error) => {
                self.errors.push(error);
                return Ty::Error;
            }
        };
        let offset = usize::from(receiver.is_some());
        let ty = self.bind_comptime_arguments(ty, &args, offset);
        if let Some((_, recorded)) = self
            .expressions
            .iter_mut()
            .rev()
            .find(|(id, _)| *id == callee)
        {
            *recorded = ty.clone();
        }
        if let Some(dispatch) = self
            .dispatches
            .iter_mut()
            .rev()
            .find(|dispatch| dispatch.expression == callee)
            && let Some((parameters, result)) = ty.signature()
        {
            dispatch.signature = Ty::Function(parameters.to_vec(), Box::new(result.clone()));
        }
        if let Some((params, ret)) = ty.signature() {
            if let Some(expected) = expected
                && !matches!(ret, Ty::Projection(..))
            {
                self.unify(ret, expected, &expr.span);
            }
            let offset = usize::from(receiver.is_some());
            if let Some(receiver) = receiver.as_ref()
                && let Some(parameter) = params.first()
            {
                self.unify(receiver, parameter, &expr.span);
            }
            let count = args.len() + offset;
            let variadic = self.variadic_element(&ty);
            let heterogeneous = self.callable_parameter_pack(&ty);
            let fixed = params.len() - usize::from(variadic.is_some() || heterogeneous);
            let explicit_pack = if heterogeneous {
                if let Some(Ty::Tuple(types)) = params.last() {
                    Some(types.as_slice())
                } else {
                    None
                }
            } else {
                None
            };
            if count < fixed
                || variadic.is_none() && !heterogeneous && count != fixed
                || explicit_pack.is_some_and(|types| count != fixed + types.len())
            {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "调用实参数量不符",
                    expr.span.clone(),
                );
            }
            let mut pack_types = Vec::new();
            for (i, &arg) in args.iter().enumerate() {
                let i = i + offset;
                let expected = if i < fixed {
                    params.get(i)
                } else {
                    explicit_pack
                        .and_then(|types| types.get(i - fixed))
                        .or(variadic)
                };
                let argument = self.expression(arg, expected);
                if heterogeneous && i >= fixed {
                    pack_types.push(argument);
                }
            }
            if let Some(expected) = expected {
                self.unify(ret, expected, &expr.span);
            }
            self.require_value_captures(callee);
            if let Ty::Callable(id, _, _) = &ty
                && let Some(definition) = self.model.function_definition(*id)
                && !self.dependencies.contains(&definition)
            {
                self.dependencies.push(definition);
            }
            self.record_call_dependencies(callee);
            for &argument in &args {
                self.require_value_captures(argument);
            }
            if heterogeneous {
                self.constrain_parameter_pack(&ty, &pack_types, &expr.span);
                self.variadic_calls
                    .push(super::super::output::VariadicCall {
                        callee,
                        arguments: args,
                        fixed_count: fixed,
                        element: Ty::Tuple(pack_types),
                        heterogeneous: true,
                    });
            } else if let Some(element) = variadic {
                self.variadic_calls
                    .push(super::super::output::VariadicCall {
                        callee,
                        arguments: args,
                        fixed_count: fixed,
                        element: element.clone(),
                        heterogeneous: false,
                    });
            }
            self.check_foreign_call(callee, &ty);
            ret.clone()
        } else {
            for &arg in &args {
                self.expression(arg, None);
            }
            self.error(
                DiagnosticCode::InvalidExpression,
                format!("调用目标不是函数：{}", self.model.describe(&ty)),
                expr.span.clone(),
            );
            Ty::Error
        }
    }

    /// comptime 值参数的实参必须是早期常量；未求值立即失败，不推迟到运行时。
    fn bind_comptime_arguments(&mut self, ty: Ty, args: &[ExprId], offset: usize) -> Ty {
        let Ty::Callable(id, ..) = &ty else { return ty };
        let Some(definition) = self.model.function_definition(*id) else {
            return ty;
        };
        let parsed = &self.model.modules[definition.module];
        let ItemKind::Function(function) = parsed.arena.items[definition.item.0 as usize].kind
        else {
            return ty;
        };
        let declaration = &parsed.arena.fns[function.0 as usize];
        let identities = self.model.value_const_parameters(*id);
        let mut bindings = BTreeMap::new();
        let mut active_index: usize = 0;
        for (index, param) in declaration
            .params
            .as_slice(&parsed.arena.params)
            .iter()
            .enumerate()
        {
            if !parsed
                .configured
                .param_active(declaration.params.start as usize + index)
            {
                continue;
            }
            let is_receiver = active_index == 0 && offset == 1;
            let arg_index = active_index.checked_sub(offset);
            active_index += 1;
            if is_receiver || !param.comptime {
                continue;
            }
            let Some(&arg) = arg_index.and_then(|position| args.get(position)) else {
                continue;
            };
            let span = self.arena().exprs[arg.0 as usize].span.clone();
            let name = param
                .pat
                .and_then(|pat| match parsed.arena.pats[pat.0 as usize].kind {
                    PatKind::Ident(name) => {
                        Some(self.model.name(definition.module, name).to_owned())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "comptime 参数".to_owned());
            let symbolic = self.symbolic_comptime_argument(arg);
            let unavailable = self
                .state
                .names
                .keys()
                .map(|&name| (self.model.name(self.module, name).to_owned(), None))
                .collect();
            let value = symbolic
                .map(|length| Ok(Ty::Const(length)))
                .unwrap_or_else(|| {
                    self.model
                        .eval_early_const_with_bindings(self.module, arg, &Ty::int(), &unavailable)
                        .map(|value| match value {
                            super::super::model::ConstantValue::Int(value)
                                if value >= 0 && value <= i128::from(i64::MAX) =>
                            {
                                Ty::Const(super::super::model::ArrayLength::Known(value as u64))
                            }
                            _ => Ty::Error,
                        })
                });
            if let Err(error) = &value {
                if error.code() == DiagnosticCode::LateComptime {
                    self.errors.push(error.clone());
                } else {
                    self.error(
                        DiagnosticCode::ComptimeCapability,
                        format!("comptime 参数 `{name}` 需要编译期已知值"),
                        span.clone(),
                    );
                }
            }
            if let Some((_, _, identity)) = identities
                .iter()
                .find(|(position, _, _)| *position == index)
            {
                let value = value.unwrap_or(Ty::Error);
                if value == Ty::Error {
                    self.error(
                        DiagnosticCode::InvalidType,
                        "数组长度需要非负 int 编译期常量",
                        span,
                    );
                }
                bindings.insert(identity.clone(), value);
            }
        }
        super::super::model::substitute(&ty, &bindings)
    }

    pub(super) fn symbolic_comptime_argument(
        &self,
        expression: ExprId,
    ) -> Option<super::super::model::ArrayLength> {
        let ExprKind::Path(path) = self.arena().exprs[expression.0 as usize].kind else {
            return None;
        };
        let segments = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments);
        let [segment] = segments else { return None };
        if let Some(&slot) = self.state.names.get(&segment.name)
            && !self.arena().params.iter().any(|param| {
                param.comptime
                    && param.pat.is_some_and(|pat| {
                        let span = &self.arena().pats[pat.0 as usize].span;
                        self.slots[slot].range == [span.start(), span.end()]
                    })
            })
        {
            return None;
        }
        let params = self
            .model
            .parameters_at(self.module, &self.arena().exprs[expression.0 as usize].span);
        match params.get(self.model.name(self.module, segment.name)) {
            Some(Ty::Const(length)) => Some(length.clone()),
            _ => None,
        }
    }

    fn record_call_dependencies(&mut self, expression: ExprId) {
        for callable in self.value_callables(expression) {
            if let Some(definition) = self.model.function_definition(callable) {
                if !self.dependencies.contains(&definition) {
                    self.dependencies.push(definition);
                }
            } else {
                for plan in &self.capture_plans {
                    if plan.function == Some(callable) {
                        for dependency in &plan.dependencies {
                            if !self.dependencies.contains(dependency) {
                                self.dependencies.push(*dependency);
                            }
                        }
                    }
                }
            }
        }
    }

    fn variadic_element<'ty>(&self, ty: &'ty Ty) -> Option<&'ty Ty> {
        let Ty::Callable(id, _, signature) = ty else {
            return None;
        };
        let module = &self.model.modules[id.module];
        let function = &module.arena.fns[id.function as usize];
        let last = function
            .params
            .as_slice(&module.arena.params)
            .iter()
            .enumerate()
            .rfind(|(offset, _)| {
                module
                    .configured
                    .param_active(function.params.start as usize + offset)
            })?
            .1;
        if !last.variadic {
            return None;
        }
        let (parameters, _) = signature.signature()?;
        if let Some(Ty::Ref(inner)) = parameters.last()
            && let Ty::Slice(element) = &**inner
        {
            Some(element)
        } else {
            None
        }
    }
}
