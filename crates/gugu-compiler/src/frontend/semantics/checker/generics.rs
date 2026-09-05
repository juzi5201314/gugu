//! 泛型调用保持函数项身份；Fn 只提供签名约束，不把参数擦除成胖指针。
use super::super::model::{CallableId, substitute};
use super::*;

impl Checker<'_, '_> {
    pub(super) fn instantiate_callable(
        &mut self,
        ty: Ty,
        arguments: AstRange<GenericArg>,
        span: &Span,
    ) -> Ty {
        let Ty::Callable(id, captured, signature) = ty else {
            if arguments.len != 0 {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "非泛型值不能应用类型实参",
                    span.clone(),
                );
            }
            return ty;
        };
        let function = &self.model.modules[id.module].arena.fns[id.function as usize];
        let generics = function
            .generics
            .as_slice(&self.model.modules[id.module].arena.generic_params);
        let arguments = arguments.as_slice(&self.arena().generic_args);
        let fixed = generics
            .iter()
            .filter(|param| matches!(param.kind, GenericParamKind::Type { pack: false, .. }))
            .count();
        let pack = generics.iter().find_map(|param| {
            if let GenericParamKind::Type {
                name, pack: true, ..
            } = param.kind
            {
                Some(name)
            } else {
                None
            }
        });
        if !arguments.is_empty()
            && (arguments.len() < fixed || pack.is_none() && arguments.len() != fixed)
        {
            self.error(
                DiagnosticCode::InvalidExpression,
                "函数类型实参数量不符",
                span.clone(),
            );
        }
        let context = self.model.callable_context(id);
        let mut bindings = self.model.parameters_at(id.module, &function.span);
        bindings.extend(context.keys().cloned().zip(captured));
        if let Some(definition) = self.model.function_definition(id) {
            match self.model.value_type(definition) {
                Ok(formal) => {
                    let Ty::Callable(_, _, formal) = formal else {
                        unreachable!("函数项签名");
                    };
                    let mut inferred = BTreeMap::new();
                    if super::super::traits::select::matches(&formal, &signature, &mut inferred) {
                        bindings.extend(inferred);
                    }
                }
                Err(error) => {
                    self.errors.push(error);
                    return Ty::Error;
                }
            }
        }
        let mut index = 0;
        for generic in generics {
            if let GenericParamKind::Type {
                name, pack: false, ..
            } = generic.kind
            {
                let ty = match arguments.get(index) {
                    Some(argument) => match self.model.form_argument(self.module, *argument) {
                        Ok(ty) => ty,
                        Err(error) => {
                            self.errors.push(error);
                            Ty::Error
                        }
                    },
                    None => self.fresh(),
                };
                bindings.insert(self.model.name(id.module, name).to_owned(), ty);
                index += 1;
            }
        }
        if let Some(name) = pack
            && !arguments.is_empty()
            && arguments.len() >= fixed
        {
            let mut elements = Vec::new();
            for &argument in &arguments[fixed..] {
                match self.model.form_argument(self.module, argument) {
                    Ok(ty) => elements.push(ty),
                    Err(error) => {
                        self.errors.push(error);
                        elements.push(Ty::Error);
                    }
                }
            }
            bindings.insert(
                self.model.name(id.module, name).to_owned(),
                Ty::Tuple(elements),
            );
        }
        if arguments.is_empty()
            && let Some(name) = pack
        {
            let ty = self.fresh();
            bindings.insert(self.model.name(id.module, name).into(), ty);
        }
        let associated: Vec<_> = bindings
            .iter()
            .filter(|(name, _)| name.starts_with("Self::"))
            .map(|(name, ty)| (name.clone(), substitute(ty, &bindings)))
            .collect();
        bindings.extend(associated);
        for generic in generics {
            if let GenericParamKind::Type {
                name,
                bounds,
                pack: false,
                ..
            } = generic.kind
            {
                for bound in bounds.as_slice(&self.model.modules[id.module].arena.bounds) {
                    if let Some(signature) = self.bound_signature(id.module, &bound.kind) {
                        let signature = substitute(&signature, &bindings);
                        self.callable_constraints.push((
                            bindings[self.model.name(id.module, name)].clone(),
                            signature,
                            span.clone(),
                        ));
                    }
                    if let BoundKind::Path(path) = bound.kind {
                        match self.model.trait_ref(id.module, path, &bindings) {
                            Ok(interface) => {
                                let assumptions = self.model.assumptions_at(self.module, span);
                                match assumptions {
                                    Ok(assumptions) => self.trait_constraints.push((
                                        super::super::traits::Obligation {
                                            ty: bindings[self.model.name(id.module, name)].clone(),
                                            interface,
                                            span: span.clone(),
                                        },
                                        assumptions,
                                    )),
                                    Err(error) => self.errors.push(error),
                                }
                            }
                            Err(error) => self.errors.push(error),
                        }
                    }
                }
            }
        }
        self.instantiate_apits(id, &mut bindings, span);
        let arguments = context.keys().map(|name| bindings[name].clone()).collect();
        Ty::Callable(id, arguments, Box::new(substitute(&signature, &bindings)))
    }
    pub(super) fn typed_callable(
        &mut self,
        expression: ExprId,
        arguments: AstRange<GenericArg>,
        expected: Option<&Ty>,
    ) -> Ty {
        if arguments.len == 0 {
            return self.expression(expression, expected);
        }
        if let ExprKind::Path(path) = self.arena().exprs[expression.0 as usize].kind {
            let ty = self.path_value(expression, path, true, expected, arguments);
            self.expressions.push((expression, ty.clone()));
            ty
        } else {
            self.error(
                DiagnosticCode::InvalidExpression,
                "显式类型实参要求具名函数项",
                self.arena().exprs[expression.0 as usize].span.clone(),
            );
            Ty::Error
        }
    }

    fn bound_signature(&mut self, module: usize, bound: &BoundKind) -> Option<Ty> {
        let BoundKind::Fn { params, ret } = *bound else {
            return None;
        };
        let arena = &self.model.modules[module].arena;
        let mut parameters = Vec::new();
        for &id in params.as_slice(&arena.ty_ids) {
            match self.model.form(module, id) {
                Ok(ty) => parameters.push(ty),
                Err(error) => {
                    self.errors.push(error);
                    parameters.push(Ty::Error);
                }
            }
        }
        let ret = ret.map_or(Ok(Ty::Unit), |id| self.model.form(module, id));
        let ret = match ret {
            Ok(ret) => ret,
            Err(error) => {
                self.errors.push(error);
                Ty::Error
            }
        };
        Some(Ty::Function(parameters, Box::new(ret)))
    }

    pub(super) fn register_callable_bounds(&mut self, function: FnId) {
        let function = &self.arena().fns[function.0 as usize];
        if let Err(error) = self.model.assumptions_at(self.module, &function.span) {
            self.errors.push(error);
        }
        let mut names = std::collections::BTreeSet::new();
        let generics = function.generics.as_slice(&self.arena().generic_params);
        for (index, generic) in generics.iter().enumerate() {
            if let GenericParamKind::Type { name, pack, .. } = generic.kind {
                if !names.insert(name) || pack && index + 1 != generics.len() {
                    self.error(
                        DiagnosticCode::InvalidDeclaration,
                        "泛型参数不能重名，类型参数包必须位于最后",
                        generic.span.clone(),
                    );
                }
            }
        }
        for generic in function.generics.as_slice(&self.arena().generic_params) {
            if let GenericParamKind::Type { name, bounds, .. } = generic.kind {
                self.callable_bounds
                    .remove(self.model.name(self.module, name));
                for bound in bounds.as_slice(&self.arena().bounds) {
                    if let Some(signature) = self.bound_signature(self.module, &bound.kind) {
                        let name = self.model.name(self.module, name).to_owned();
                        if self
                            .callable_bounds
                            .get(&name)
                            .is_some_and(|previous| *previous != signature)
                        {
                            self.error(
                                DiagnosticCode::InvalidDeclaration,
                                "同一泛型参数具有不相容的 Fn 签名",
                                bound.span.clone(),
                            );
                        }
                        self.callable_bounds.insert(name, signature);
                    }
                }
            }
        }
    }

    pub(super) fn is_parameter_pack(&self, function: FnId, ty: &Ty) -> bool {
        let Ty::Param(name) = ty else { return false };
        self.arena().fns[function.0 as usize].generics.as_slice(&self.arena().generic_params).iter().any(|param| {
            matches!(param.kind, GenericParamKind::Type { name: symbol, pack: true, .. } if self.model.name(self.module, symbol) == name)
        })
    }

    pub(super) fn callable_parameter_pack(&self, ty: &Ty) -> bool {
        let Ty::Callable(CallableId { module, function }, _, _) = ty else {
            return false;
        };
        let parsed = &self.model.modules[*module];
        let declaration = &parsed.arena.fns[*function as usize];
        let Some((_, parameter)) = declaration
            .params
            .as_slice(&parsed.arena.params)
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                parsed
                    .configured
                    .param_active(declaration.params.start as usize + index)
            })
            .last()
        else {
            return false;
        };
        if !parameter.variadic {
            return false;
        }
        let Some(id) = parameter.ty else { return false };
        let Ok(Ty::Param(name)) = self.model.form(*module, id) else {
            return false;
        };
        declaration.generics.as_slice(&parsed.arena.generic_params).iter().any(|param| {
            matches!(param.kind, GenericParamKind::Type { name: symbol, pack: true, .. } if self.model.name(*module, symbol) == name)
        })
    }

    pub(super) fn constrain_parameter_pack(&mut self, ty: &Ty, elements: &[Ty], span: &Span) {
        let Ty::Callable(id, arguments, _) = ty else {
            return;
        };
        let parsed = &self.model.modules[id.module];
        let function = &parsed.arena.fns[id.function as usize];
        let generics = function.generics.as_slice(&parsed.arena.generic_params);
        let bindings: BTreeMap<_, _> = self
            .model
            .callable_context(*id)
            .into_keys()
            .zip(arguments.iter().cloned())
            .collect();
        for generic in generics {
            if let GenericParamKind::Type {
                name,
                bounds,
                pack: true,
                ..
            } = generic.kind
            {
                self.unify(
                    &bindings[self.model.name(id.module, name)],
                    &Ty::Tuple(elements.to_vec()),
                    span,
                );
                for bound in bounds.as_slice(&parsed.arena.bounds) {
                    if let Some(signature) = self.bound_signature(id.module, &bound.kind) {
                        let expected = substitute(&signature, &bindings);
                        for element in elements {
                            self.callable_constraints.push((
                                element.clone(),
                                expected.clone(),
                                span.clone(),
                            ));
                        }
                    }
                }
            }
        }
    }
}
