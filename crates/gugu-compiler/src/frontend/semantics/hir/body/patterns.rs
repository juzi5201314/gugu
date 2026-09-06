use super::*;

impl BodyBuilder<'_, '_, '_, '_> {
    fn evaluated_int(&self, expression: ast::ExprId) -> Result<i128, Diagnostic> {
        match self
            .compiler
            .checked
            .early_constants
            .expression_value(self.module, expression.0)
        {
            Some(crate::frontend::semantics::comptime::eval::ConstantValue::Int(value)) => {
                Ok(*value)
            }
            _ => self.compiler.model.constant_int(self.module, expression),
        }
    }

    pub(super) fn bind_pattern(
        &mut self,
        pattern: ast::PatId,
    ) -> Result<hir::PatternId, Diagnostic> {
        let plan = self
            .facts
            .body
            .patterns
            .iter()
            .find(|plan| plan.pattern == pattern)
            .ok_or_else(|| self.error("模式缺少已检查绑定计划"))?;
        for slot in plan.bound_slots.clone() {
            self.local(slot)?;
        }
        self.pattern(pattern, &plan.ty)
    }

    fn pattern(&mut self, source: ast::PatId, ty: &Ty) -> Result<hir::PatternId, Diagnostic> {
        let pattern = &self.arena().pats[source.0 as usize];
        let id = hir::PatternId(checked_id(self.patterns.len())?);
        self.patterns.push(None);
        let kind = if let Ty::Ref(inner) = ty
            && !matches!(
                pattern.kind,
                ast::PatKind::Ident(_)
                    | ast::PatKind::At { .. }
                    | ast::PatKind::Ref(_)
                    | ast::PatKind::Or(_)
                    | ast::PatKind::Wildcard
            ) {
            hir::PatternKind::Ref(self.pattern(source, inner)?)
        } else {
            self.pattern_kind(&pattern.kind, ty, &pattern.span)?
        };
        self.patterns[id.index()] = Some(hir::Pattern {
            location: identity::location(self.compiler.sources, &pattern.span)?,
            ty: self.type_id(ty)?,
            kind,
        });
        Ok(id)
    }

    fn pattern_kind(
        &mut self,
        kind: &ast::PatKind,
        ty: &Ty,
        span: &Span,
    ) -> Result<hir::PatternKind, Diagnostic> {
        Ok(match kind {
            ast::PatKind::Wildcard => hir::PatternKind::Wildcard,
            ast::PatKind::Ident(name) => {
                let text = self.compiler.model.name(self.module, *name);
                if let Some((_, constructor)) =
                    self.compiler
                        .model
                        .constructor(self.module, &[text], Some(ty))?
                    && constructor.fields.is_empty()
                {
                    hir::PatternKind::Construct {
                        variant: checked_id(constructor.index)?,
                        fields: 0..0,
                    }
                } else {
                    hir::PatternKind::Bind(
                        *self
                            .names
                            .get(text)
                            .ok_or_else(|| self.error("模式绑定没有 HIR local"))?,
                    )
                }
            }
            ast::PatKind::At { name, pat, .. } => {
                let text = self.compiler.model.name(self.module, *name);
                let local = *self
                    .names
                    .get(text)
                    .ok_or_else(|| self.error("at 模式没有 HIR local"))?;
                hir::PatternKind::At {
                    local,
                    pattern: self.pattern(*pat, ty)?,
                }
            }
            ast::PatKind::Ref(pattern) => {
                let Ty::Ref(inner) = ty else {
                    return Err(self.error("已检查引用模式丢失引用类型"));
                };
                hir::PatternKind::Ref(self.pattern(*pattern, inner)?)
            }
            ast::PatKind::Literal(literal) | ast::PatKind::NegativeLiteral(literal) => {
                hir::PatternKind::Literal(self.literal(
                    *literal,
                    ty,
                    matches!(kind, ast::PatKind::NegativeLiteral(_)),
                )?)
            }
            ast::PatKind::Range { start, end } => {
                let start = self.evaluated_int(*start)?;
                let end = self.evaluated_int(*end)?;
                let literal = |value: i128| {
                    if *ty == Ty::Char {
                        char::from_u32(value as u32)
                            .map(hir::Literal::Char)
                            .ok_or_else(|| self.error("范围端点不是 Unicode 标量"))
                    } else {
                        Ok(hir::Literal::Integer(value as u128))
                    }
                };
                hir::PatternKind::Range {
                    start: literal(start)?,
                    end: literal(end)?,
                }
            }
            ast::PatKind::Tuple(patterns) => {
                let types = match ty {
                    Ty::Tuple(types) => types.as_slice(),
                    Ty::Unit => &[],
                    _ => return Err(self.error("已检查元组模式丢失元组类型")),
                };
                let nodes = patterns
                    .as_slice(&self.arena().pat_ids)
                    .iter()
                    .zip(types)
                    .map(|(&pattern, ty)| self.pattern(pattern, ty))
                    .collect::<Result<Vec<_>, _>>()?;
                hir::PatternKind::Tuple(self.pattern_list(nodes)?)
            }
            ast::PatKind::Array {
                prefix,
                rest,
                suffix,
            } => {
                let element = match ty {
                    Ty::Array(element, _) | Ty::Slice(element) => element,
                    _ => return Err(self.error("已检查序列模式丢失元素类型")),
                };
                let prefix = prefix
                    .as_slice(&self.arena().pat_ids)
                    .iter()
                    .map(|&pattern| self.pattern(pattern, element))
                    .collect::<Result<Vec<_>, _>>()?;
                let prefix = self.pattern_list(prefix)?;
                let suffix = suffix
                    .as_slice(&self.arena().pat_ids)
                    .iter()
                    .map(|&pattern| self.pattern(pattern, element))
                    .collect::<Result<Vec<_>, _>>()?;
                let suffix = self.pattern_list(suffix)?;
                let binding = rest
                    .as_ref()
                    .and_then(|rest| rest.name)
                    .map(|name| {
                        self.names
                            .get(self.compiler.model.name(self.module, name))
                            .copied()
                            .ok_or_else(|| self.error("rest 模式没有 HIR local"))
                    })
                    .transpose()?;
                hir::PatternKind::Array {
                    prefix,
                    rest: binding,
                    has_rest: rest.is_some(),
                    suffix,
                }
            }
            ast::PatKind::Constructor { path, fields } => {
                let constructor = self.pattern_constructor(*path, ty)?;
                let mut formed = Vec::with_capacity(constructor.fields.len());
                for (index, (&pattern, field)) in fields
                    .as_slice(&self.arena().pat_ids)
                    .iter()
                    .zip(&constructor.fields)
                    .enumerate()
                {
                    formed.push(hir::PatternField {
                        field: checked_id(index)?,
                        pattern: self.pattern(pattern, &field.ty)?,
                    });
                }
                hir::PatternKind::Construct {
                    variant: checked_id(constructor.index)?,
                    fields: self.pattern_fields(formed)?,
                }
            }
            ast::PatKind::Struct { path, fields, .. } => {
                let constructor = self.pattern_constructor(*path, ty)?;
                let mut formed = Vec::with_capacity(fields.len as usize);
                for field in fields.as_slice(&self.arena().field_pats) {
                    let name = self.compiler.model.name(self.module, field.name);
                    let (index, shape) = constructor
                        .fields
                        .iter()
                        .enumerate()
                        .find(|(_, field)| field.name == name)
                        .ok_or_else(|| self.error("已检查记录模式字段消失"))?;
                    let pattern = if let Some(pattern) = field.pat {
                        self.pattern(pattern, &shape.ty)?
                    } else {
                        let local = *self
                            .names
                            .get(name)
                            .ok_or_else(|| self.error("字段简写没有 HIR local"))?;
                        let id = hir::PatternId(checked_id(self.patterns.len())?);
                        let ty = self.type_id(&shape.ty)?;
                        self.patterns.push(Some(hir::Pattern {
                            location: identity::location(self.compiler.sources, &field.span)?,
                            ty,
                            kind: hir::PatternKind::Bind(local),
                        }));
                        id
                    };
                    formed.push(hir::PatternField {
                        field: checked_id(index)?,
                        pattern,
                    });
                }
                hir::PatternKind::Construct {
                    variant: checked_id(constructor.index)?,
                    fields: self.pattern_fields(formed)?,
                }
            }
            ast::PatKind::Or(patterns) => {
                let nodes = patterns
                    .as_slice(&self.arena().pat_ids)
                    .iter()
                    .map(|&pattern| self.pattern(pattern, ty))
                    .collect::<Result<Vec<_>, _>>()?;
                hir::PatternKind::Or(self.pattern_list(nodes)?)
            }
            ast::PatKind::SourceMacro { .. } | ast::PatKind::Error => {
                return Err(Diagnostic::error(
                    DiagnosticCode::InvalidPattern,
                    "未展开或错误模式不能进入 HIR",
                    Some(span.clone()),
                ));
            }
        })
    }
    fn pattern_constructor(
        &self,
        path: ast::PathId,
        ty: &Ty,
    ) -> Result<crate::frontend::semantics::model::Constructor, Diagnostic> {
        self.compiler
            .model
            .constructor(
                self.module,
                &self.compiler.model.path(self.module, path),
                Some(ty),
            )?
            .map(|(_, constructor)| constructor)
            .ok_or_else(|| self.error("模式没有已解析构造器"))
    }
    fn pattern_list(
        &mut self,
        patterns: Vec<hir::PatternId>,
    ) -> Result<std::ops::Range<u32>, Diagnostic> {
        let start = checked_id(self.output.pattern_ids.len())?;
        self.output.pattern_ids.extend(patterns);
        Ok(start..checked_id(self.output.pattern_ids.len())?)
    }
    fn pattern_fields(
        &mut self,
        fields: Vec<hir::PatternField>,
    ) -> Result<std::ops::Range<u32>, Diagnostic> {
        let start = checked_id(self.output.pattern_fields.len())?;
        self.output.pattern_fields.extend(fields);
        Ok(start..checked_id(self.output.pattern_fields.len())?)
    }
}
