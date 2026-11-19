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
    pub(super) fn const_argument(
        &self,
        module: usize,
        expression: ExprId,
        params: &BTreeMap<String, Ty>,
        error: Diagnostic,
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
            return self.array_length(module, expression, params).map(Ty::Const);
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
        }) {
            return None;
        }
        self.parameters_at(module, span)
            .remove(name)
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
                        return Err(self.error(module, "comptime 泛型参数目前必须声明为 int"));
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn check_generic_kinds(
        &self,
        definition: DefRef,
        arguments: &[Ty],
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
                return Err(self.error(definition.module, "类型与 comptime 泛型实参种类不符"));
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
        let arena = &self.modules[module].arena;
        if let ExprKind::Path(path) = arena.exprs[expression.0 as usize].kind {
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
        u64::try_from(self.constant_int(module, expression)?)
            .map(ArrayLength::Known)
            .map_err(|_| self.error(module, "comptime 泛型实参或数组长度必须非负且不超过 u64"))
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
                    if self.path(module, path).as_slice().first().is_some_and(|name|
                        matches!(params.get(*name), Some(Ty::Const(_)))))
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
