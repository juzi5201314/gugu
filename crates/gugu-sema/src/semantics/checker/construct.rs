use super::*;
impl Checker<'_, '_> {
    pub(super) fn construct(
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
                let count = if self.model.is_union(&ty) {
                    1
                } else {
                    ctor.fields.len()
                };
                if seen.len() != count {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "构造器初始化字段数量不符；union 必须恰好选择一个字段",
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
