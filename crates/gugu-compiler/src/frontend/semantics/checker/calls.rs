//! 调用目标、实参物化与参数包共享一条签名检查路径。
use super::*;

impl Checker<'_, '_> {
    pub(super) fn call(
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
        if let Some((params, ret)) = ty.signature() {
            if let Some(expected) = expected
                && !matches!(ret, Ty::Projection(..))
            {
                self.unify(ret, expected, &expr.span);
            }
            let offset = usize::from(receiver.is_some());
            if let Some(receiver) = receiver.as_ref() {
                if let Some(parameter) = params.first() {
                    self.unify(receiver, parameter, &expr.span);
                }
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
            if let Ty::Callable(id, _, _) = &ty {
                if let Some(definition) = self.model.function_definition(*id) {
                    if !self.dependencies.contains(&definition) {
                        self.dependencies.push(definition);
                    }
                }
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
            .filter(|(offset, _)| {
                module
                    .configured
                    .param_active(function.params.start as usize + offset)
            })
            .last()?
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
