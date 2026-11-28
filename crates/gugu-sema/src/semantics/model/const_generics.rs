use super::*;

impl ArrayLength {
    pub fn substitute(&self, bindings: &BTreeMap<String, Ty>) -> Self {
        match self {
            Self::Param(name) => match bindings.get(name) {
                Some(Ty::Const(value)) => value.clone(),
                Some(Ty::Var(id)) => Self::Param(format!("$const-var:{id}")),
                _ => self.clone(),
            },
            Self::Known(_) => self.clone(),
        }
    }
}

impl Model<'_> {
    pub(in super::super) fn unparenthesized_expression(
        &self,
        module: usize,
        mut expression: ExprId,
    ) -> ExprId {
        while let ExprKind::Paren(inner) =
            self.modules[module].arena.exprs[expression.0 as usize].kind
        {
            expression = inner;
        }
        expression
    }

    pub fn scoped_array_length(
        &self,
        module: usize,
        expression: ExprId,
    ) -> Result<ArrayLength, Diagnostic> {
        self.array_length(
            module,
            expression,
            &self.parameters_at(
                module,
                &self.modules[module].arena.exprs[expression.0 as usize].span,
            ),
        )
    }
    /// 按名字查找 `span` 作用域内的 comptime 参数：内层 fn 参数优先，其次外层 fn 与关联作用域。
    /// 与 `parameters_at` 的插入顺序一致（内层覆盖外层），但只为单个名字做定向查找，不建整张表。
    pub(in super::super) fn scoped_comptime_parameter(
        &self,
        module: usize,
        span: &crate::Span,
        name: &str,
    ) -> Option<Ty> {
        let arena = &self.modules[module].arena;
        // parser 先分配内层 FnDecl，顺序遍历即由内到外；首个命中即 `parameters_at` 的胜出项。
        for (index, owner) in arena.fns.iter().enumerate().filter(|(_, function)| {
            function.span.start() <= span.start() && function.span.end() >= span.end()
        }) {
            // 按名字先过滤：仅当 fn 有同名 comptime 参数才跑 `value_const_parameters`，
            // 避免对每个包含 span 的 owner 都执行 `mentions` 全模块扫描。
            let gated = owner
                .params
                .as_slice(&arena.params)
                .iter()
                .any(|parameter| {
                    parameter.comptime
                        && parameter.pat.is_some_and(|pat| {
                            matches!(arena.pats[pat.0 as usize].kind,
                            PatKind::Ident(symbol) if self.name(module, symbol) == name)
                        })
                });
            if gated {
                for (_, param_name, identity) in self.value_const_parameters(CallableId {
                    module,
                    function: index as u32,
                }) {
                    if param_name == name {
                        return Some(Ty::Const(ArrayLength::Param(identity)));
                    }
                }
            }
            for param in owner.generics.as_slice(&arena.generic_params) {
                let (generic_name, ty) = self.generic_binding(module, &param.kind);
                if generic_name == name && matches!(ty, Ty::Const(_)) {
                    return Some(ty);
                }
            }
        }
        self.associated_scope(module, span)
            .get(name)
            .filter(|ty| matches!(ty, Ty::Const(_)))
            .cloned()
    }

    pub fn value_const_parameters(&self, id: CallableId) -> Vec<(usize, String, String)> {
        let parsed = &self.modules[id.module];
        let arena = &parsed.arena;
        let function = &arena.fns[id.function as usize];
        function.params.as_slice(&arena.params).iter().enumerate().filter_map(|(index, parameter)| {
            if !parameter.comptime || !parsed.configured.param_active(function.params.start as usize + index) {
                return None;
            }
            let PatKind::Ident(symbol) = arena.pats[parameter.pat?.0 as usize].kind else { return None };
            let TyKind::Path(path) = arena.tys[parameter.ty?.0 as usize].kind else { return None };
            if self.path(id.module, path) != ["int"] { return None; }
            let name = self.name(id.module, symbol);
            let mentions = |expression: ExprId| {
                let span = &arena.exprs[expression.0 as usize].span;
                span.start() >= function.span.start() && span.end() <= function.span.end()
                    && arena.exprs.iter().any(|expr| expr.span.start() >= span.start() && expr.span.end() <= span.end()
                        && matches!(expr.kind, ExprKind::Path(path) if self.path(id.module, path) == [name]))
            };
            let used = arena.tys.iter().any(|ty| matches!(ty.kind, TyKind::Array { len, .. } if mentions(len))
                || ty.span.start() >= function.span.start() && ty.span.end() <= function.span.end()
                    && matches!(ty.kind, TyKind::Path(path) if self.path(id.module, path) == [name]))
                || arena.exprs.iter().any(|expr| matches!(expr.kind, ExprKind::Repeat { count, .. } if mentions(count)));
            used.then(|| (index, name.to_owned(), format!("$value-const:{}:{}:{index}", id.module, id.function)))
        }).collect()
    }

    pub(super) fn const_argument(
        &self,
        module: usize,
        expression: ExprId,
        params: &BTreeMap<String, Ty>,
        error: Diagnostic,
        stack: &[DefRef],
    ) -> Result<Ty, Diagnostic> {
        let ExprKind::Path(path) = self.modules[module].arena.exprs[expression.0 as usize].kind
        else {
            return Err(error);
        };
        let parts = self.path(module, path);
        if let Some(value @ Ty::Const(_)) = params.get(&parts.join("::")) {
            return Ok(value.clone());
        }
        if let Ok(definition) = self.resolve(module, &parts)
            && matches!(
                self.modules[definition.module].arena.items[definition.item.0 as usize].kind,
                ItemKind::Const { .. }
            )
        {
            return self
                .array_length_with_context(module, expression, params, stack)
                .map(Ty::Const);
        }
        Err(error)
    }
    pub(in super::super) fn const_parameter_at(
        &self,
        module: usize,
        span: &crate::Span,
        name: &str,
    ) -> Option<Ty> {
        if !self.modules[module].arena.generic_params.iter().any(|parameter| {
            matches!(parameter.kind, GenericParamKind::Comptime { name: parameter_name, .. } if self.name(module, parameter_name) == name)
        }) && !self.modules[module].arena.params.iter().any(|parameter| parameter.comptime
            && parameter.pat.is_some_and(|pat| matches!(self.modules[module].arena.pats[pat.0 as usize].kind,
                PatKind::Ident(symbol) if self.name(module, symbol) == name))) {
            return None;
        }
        self.scoped_comptime_parameter(module, span, name)
            .filter(|ty| matches!(ty, Ty::Const(_)))
    }
    pub(super) fn validate_const_generics(&self) -> Result<(), Diagnostic> {
        for (module, parsed) in self.modules.iter().enumerate() {
            for (index, item) in parsed.arena.items.iter().enumerate() {
                if !parsed.configured.item_active(ItemId(index as u32)) {
                    continue;
                }
                let generics = match item.kind {
                    ItemKind::Struct { generics, .. }
                    | ItemKind::Enum { generics, .. }
                    | ItemKind::Union { generics, .. }
                    | ItemKind::Impl { generics, .. } => generics,
                    ItemKind::Function(id) => parsed.arena.fns[id.0 as usize].generics,
                    _ => continue,
                };
                for parameter in generics.as_slice(&parsed.arena.generic_params) {
                    if let GenericParamKind::Comptime { ty, .. } = parameter.kind
                        && self.form(module, ty)? != Ty::int()
                    {
                        return Err(self.error_at(
                            &parsed.arena.tys[ty.0 as usize].span,
                            "comptime 泛型参数目前必须声明为 int",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
    pub(in super::super) fn check_generic_kinds(
        &self,
        definition: DefRef,
        arguments: &[Ty],
        span: &crate::Span,
    ) -> Result<(), Diagnostic> {
        let parsed = &self.modules[definition.module];
        let generics = match parsed.arena.items[definition.item.0 as usize].kind {
            ItemKind::Struct { generics, .. }
            | ItemKind::Enum { generics, .. }
            | ItemKind::Union { generics, .. }
            | ItemKind::TypeAlias { generics, .. } => generics,
            _ => return Ok(()),
        };
        for (parameter, argument) in generics
            .as_slice(&parsed.arena.generic_params)
            .iter()
            .zip(arguments)
        {
            if matches!(parameter.kind, GenericParamKind::Comptime { .. })
                != matches!(argument, Ty::Const(_))
            {
                return Err(self.error_at(span, "类型与 comptime 泛型实参种类不符"));
            }
        }
        Ok(())
    }
    pub fn array_length(
        &self,
        module: usize,
        expression: ExprId,
        params: &BTreeMap<String, Ty>,
    ) -> Result<ArrayLength, Diagnostic> {
        self.array_length_with_context(module, expression, params, &[])
    }

    pub(in super::super) fn array_length_with_context(
        &self,
        module: usize,
        expression: ExprId,
        params: &BTreeMap<String, Ty>,
        stack: &[DefRef],
    ) -> Result<ArrayLength, Diagnostic> {
        let arena = &self.modules[module].arena;
        let inner = self.unparenthesized_expression(module, expression);
        if let ExprKind::Path(path) = arena.exprs[inner.0 as usize].kind {
            let parts = self.path(module, path);
            if parts.len() == 1
                && let Some(Ty::Const(value)) = params.get(parts[0])
            {
                return Ok(value.clone());
            }
        }
        if self.has_const_parameter(module, expression, params) {
            return Err(Diagnostic::error(
                DiagnosticCode::InvalidType,
                "数组长度暂只支持独立的 comptime 泛型参数，不支持参数算术",
                Some(arena.exprs[expression.0 as usize].span.clone()),
            ));
        }
        u64::try_from(self.constant_int_with_context(module, expression, stack)?)
            .map(ArrayLength::Known)
            .map_err(|_| {
                self.error_at(
                    &arena.exprs[expression.0 as usize].span,
                    "comptime 泛型实参或数组长度必须非负且不超过 u64",
                )
            })
    }

    pub(in super::super) fn has_const_parameter(
        &self,
        module: usize,
        expression: ExprId,
        params: &BTreeMap<String, Ty>,
    ) -> bool {
        if !params.values().any(|ty| matches!(ty, Ty::Const(_))) {
            return false;
        }
        let arena = &self.modules[module].arena;
        let span = &arena.exprs[expression.0 as usize].span;
        arena.exprs.iter().any(|expr| {
            expr.span.start() >= span.start()
                && expr.span.end() <= span.end()
                && matches!(expr.kind, ExprKind::Path(path)
                    if matches!(self.path(module, path).as_slice(), [name]
                        if matches!(params.get(*name), Some(Ty::Const(_)))))
        })
    }

    pub(in super::super) fn generic_binding(
        &self,
        module: usize,
        kind: &GenericParamKind,
    ) -> (String, Ty) {
        match *kind {
            GenericParamKind::Type { name, .. } => {
                let name = self.name(module, name).to_owned();
                (name.clone(), Ty::Param(name))
            }
            GenericParamKind::Comptime { name, .. } => {
                let name = self.name(module, name).to_owned();
                (name.clone(), Ty::Const(ArrayLength::Param(name)))
            }
        }
    }
}
