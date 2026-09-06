use super::*;

struct Step {
    kind: StepKind,
    input: hir::TypeId,
    ty: hir::TypeId,
    dereferences: u32,
}
enum StepKind {
    Root(hir::Res),
    Field(u32),
    Index(ast::ExprId),
    Slice(Option<ast::ExprId>, Option<ast::ExprId>),
}
impl Step {
    fn new(kind: StepKind, ty: hir::TypeId) -> Self {
        Self {
            kind,
            input: ty,
            ty,
            dereferences: 0,
        }
    }
}

impl BodyBuilder<'_, '_, '_, '_> {
    pub(super) fn path_expression(
        &mut self,
        source: ast::ExprId,
        id: hir::ExprId,
        path: ast::PathId,
        expected: &Ty,
        span: &Span,
    ) -> Result<(hir::ExprKind, u32), Diagnostic> {
        let segments = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments);
        if !self
            .names
            .contains_key(self.compiler.model.name(self.module, segments[0].name))
        {
            if let Some(member) = self.compiler.model.constant_member(self.module, path)? {
                if let crate::frontend::semantics::traits::MemberKind::Const {
                    value: Some(value),
                    ..
                } = member.kind
                {
                    let value = match value {
                        crate::frontend::semantics::model::ConstantValue::Int(value) => {
                            hir::Literal::Integer(value as u128)
                        }
                        crate::frontend::semantics::model::ConstantValue::Float(value) => {
                            hir::Literal::Float(value)
                        }
                        crate::frontend::semantics::model::ConstantValue::Bool(value) => {
                            hir::Literal::Bool(value)
                        }
                        crate::frontend::semantics::model::ConstantValue::String(value) => {
                            hir::Literal::String(value)
                        }
                    };
                    return Ok((hir::ExprKind::Literal(value), 0));
                }
                let parameters = self.compiler.model.parameters_at(self.module, span);
                let assumptions = self.compiler.model.assumptions_at(self.module, span)?;
                let (self_ty, interface) = self
                    .compiler
                    .model
                    .association_head(self.module, path, &parameters, &assumptions)?
                    .ok_or_else(|| self.error("关联值没有已解析类型头"))?;
                let definition = self.compiler.identities.item(
                    member
                        .definition
                        .ok_or_else(|| self.error("关联常量没有声明身份"))?,
                );
                return Ok((
                    hir::ExprKind::Resolved(hir::Res::Associated {
                        definition,
                        self_ty: self.type_id(&self_ty)?,
                        interface: interface
                            .as_ref()
                            .map(|interface| {
                                self.compiler.trait_ref(interface, self.output.definition)
                            })
                            .transpose()?,
                    }),
                    0,
                ));
            }
            let parts = self.compiler.model.path(self.module, path);
            if let Some((_, constructor)) =
                self.compiler
                    .model
                    .constructor(self.module, &parts, Some(expected))?
                && constructor.fields.is_empty()
            {
                return Ok((
                    hir::ExprKind::Construct {
                        variant: checked_id(constructor.index)?,
                        fields: 0..0,
                    },
                    0,
                ));
            }
        }
        let (steps, _) = self.path_steps(segments, Some(expected))?;
        self.emit_path(source, id, steps, span)
    }

    fn path_steps(
        &mut self,
        segments: &[ast::PathSegment],
        expected: Option<&Ty>,
    ) -> Result<(Vec<Step>, Ty), Diagnostic> {
        let first = segments
            .first()
            .ok_or_else(|| self.error("HIR 路径没有首段"))?;
        let first_name = self.compiler.model.name(self.module, first.name);
        let (root, mut ty, fields) = if let Some(&local) = self.names.get(first_name) {
            (
                hir::Res::Local(local),
                self.facts.body.slots[self.local_sources[local.index()]].clone(),
                &segments[1..],
            )
        } else {
            let names: Vec<_> = segments
                .iter()
                .map(|segment| self.compiler.model.name(self.module, segment.name))
                .collect();
            let definition = self.compiler.model.resolve(self.module, &names)?;
            let ty = if let Some(expected) = expected
                && matches!(
                    self.compiler.model.modules[definition.module].arena.items
                        [definition.item.0 as usize]
                        .kind,
                    ast::ItemKind::Function(_)
                ) {
                expected.clone()
            } else {
                self.compiler.model.value_type(definition)?
            };
            (
                hir::Res::Def(self.compiler.identities.item(definition)),
                ty,
                &segments[segments.len()..],
            )
        };
        let mut steps = vec![Step::new(StepKind::Root(root), self.type_id(&ty)?)];
        for field in fields {
            while let Ty::Ref(inner) = ty {
                ty = *inner;
                let last = steps.last_mut().expect("路径有根节点");
                last.dereferences += 1;
                last.ty = self.type_id(&ty)?;
            }
            let name = self.compiler.model.name(self.module, field.name);
            let (index, value, _) = self
                .compiler
                .model
                .find_field(&ty, name)
                .ok_or_else(|| self.error("值路径缺少已解析字段"))?;
            ty = value;
            steps.push(Step::new(
                StepKind::Field(checked_id(index)?),
                self.type_id(&ty)?,
            ));
        }
        let arguments = segments.last().expect("非空路径").args;
        if arguments.len != 0 {
            let arguments = arguments.as_slice(&self.arena().generic_args);
            let [ast::GenericArg::Expr(index)] = arguments else {
                return Err(self.error("值路径的下标没有唯一表达式"));
            };
            let kind = if let ast::ExprKind::Range { start, end } =
                self.arena().exprs[index.0 as usize].kind
            {
                StepKind::Slice(Some(start), Some(end))
            } else {
                StepKind::Index(*index)
            };
            ty = match (&kind, ty.deref()) {
                (StepKind::Slice(..), Ty::Array(element, _) | Ty::Slice(element)) => {
                    Ty::Ref(Box::new(Ty::Slice(element.clone())))
                }
                (StepKind::Slice(..), Ty::String) => Ty::String,
                (_, Ty::Array(element, _) | Ty::Slice(element)) => *element.clone(),
                (_, Ty::String) => Ty::Int {
                    signed: false,
                    bits: 8,
                },
                _ => expected
                    .cloned()
                    .ok_or_else(|| self.error("自定义下标缺少已检查结果类型"))?,
            };
            steps.push(Step::new(kind, self.type_id(&ty)?));
        }
        Ok((steps, ty))
    }

    fn emit_path(
        &mut self,
        source: ast::ExprId,
        root: hir::ExprId,
        steps: Vec<Step>,
        span: &Span,
    ) -> Result<(hir::ExprKind, u32), Diagnostic> {
        let mut ids = vec![root; steps.len()];
        for index in (0..steps.len() - 1).rev() {
            let id = hir::ExprId(checked_id(self.expressions.len())?);
            self.expressions.push(None);
            self.adjustments.push(Vec::new());
            self.output.expression_types.push(steps[index].ty);
            self.output.expression_inputs.push(steps[index].input);
            ids[index] = id;
        }
        let count = steps.len();
        for (index, step) in steps.into_iter().enumerate() {
            let id = ids[index];
            self.output.expression_types[id.index()] = step.ty;
            self.output.expression_inputs[id.index()] = step.input;
            self.adjustments[id.index()]
                .extend((0..step.dereferences).map(|_| hir::Adjustment::Dereference));
            let kind = match step.kind {
                StepKind::Root(root) => hir::ExprKind::Resolved(root),
                StepKind::Field(field) => hir::ExprKind::Field {
                    base: ids[index - 1],
                    index: field,
                },
                StepKind::Index(subscript) => hir::ExprKind::Index {
                    base: ids[index - 1],
                    index: self.expression(subscript)?,
                    read: self.selected_dispatch(source, Some("Index"), Some("index"))?,
                    write: self.selected_dispatch(source, Some("Index"), Some("index_set"))?,
                },
                StepKind::Slice(start, end) => hir::ExprKind::Slice {
                    base: ids[index - 1],
                    start: start.map(|value| self.expression(value)).transpose()?,
                    end: end.map(|value| self.expression(value)).transpose()?,
                },
            };
            if index + 1 == count {
                return Ok((kind, hir::Effects::READ));
            }
            self.set_expression(id, kind, self.scope, span, hir::Effects::READ)?;
        }
        unreachable!("值路径至少有一个步骤")
    }

    pub(super) fn receiver(&mut self, callee: ast::ExprId) -> Result<hir::ExprId, Diagnostic> {
        match self.arena().exprs[callee.0 as usize].kind {
            ast::ExprKind::Field { base, .. } | ast::ExprKind::TupleField { base, .. } => {
                self.expression(base)
            }
            ast::ExprKind::TypeApp { base, .. } | ast::ExprKind::Paren(base) => self.receiver(base),
            ast::ExprKind::Path(path) => {
                let segments = self.arena().paths[path.0 as usize]
                    .segments
                    .as_slice(&self.arena().segments);
                let (steps, ty) = self.path_steps(&segments[..segments.len() - 1], None)?;
                let id = self.reserve(&ty)?;
                let span = &self.arena().paths[path.0 as usize].span;
                let (kind, effects) = self.emit_path(callee, id, steps, span)?;
                self.set_expression(id, kind, self.scope, span, effects)?;
                Ok(id)
            }
            _ => Err(self.error("隐式方法调用缺少接收者位置")),
        }
    }

    pub(super) fn field_value(
        &mut self,
        source: ast::ExprId,
        name: &str,
        _span: &Span,
    ) -> Result<hir::ExprKind, Diagnostic> {
        let mut ty = self.ty(source)?;
        let base = self.expression(source)?;
        while let Ty::Ref(inner) = ty {
            self.adjustments[base.index()].push(hir::Adjustment::Dereference);
            ty = inner;
        }
        self.output.expression_types[base.index()] = self.type_id(ty)?;
        let (index, _, _) = self
            .compiler
            .model
            .find_field(ty, name)
            .ok_or_else(|| self.error("字段表达式没有已解析字段"))?;
        Ok(hir::ExprKind::Field {
            base,
            index: checked_id(index)?,
        })
    }

    pub(super) fn record_value(
        &mut self,
        path: ast::PathId,
        fields: ast::AstRange<ast::FieldExpr>,
        ty: &Ty,
    ) -> Result<hir::ExprKind, Diagnostic> {
        let variants = self
            .compiler
            .model
            .variants(ty)
            .ok_or_else(|| self.error("记录构造缺少形成后的变体表"))?;
        let single = variants.len() == 1;
        let parts = self.compiler.model.path(self.module, path);
        let constructor = variants
            .into_iter()
            .find(|variant| single || Some(variant.name.as_str()) == parts.last().copied())
            .ok_or_else(|| self.error("记录构造没有已选择变体"))?;
        let mut formed = Vec::with_capacity(fields.len as usize);
        for (offset, field) in fields
            .as_slice(&self.arena().field_exprs)
            .iter()
            .enumerate()
        {
            if !self.compiler.model.modules[self.module]
                .configured
                .field_expr_active(fields.start as usize + offset)
            {
                continue;
            }
            let name = self.compiler.model.name(self.module, field.name);
            let (index, shape) = constructor
                .fields
                .iter()
                .enumerate()
                .find(|(_, field)| field.name == name)
                .ok_or_else(|| self.error("记录构造没有已解析字段"))?;
            let value = if let Some(value) = field.value {
                self.expression(value)?
            } else {
                let local = *self
                    .names
                    .get(name)
                    .ok_or_else(|| self.error("字段简写没有局部绑定"))?;
                let id = self.reserve(&shape.ty)?;
                self.set_expression(
                    id,
                    hir::ExprKind::Resolved(hir::Res::Local(local)),
                    self.scope,
                    &field.span,
                    hir::Effects::READ,
                )?;
                id
            };
            formed.push(hir::FieldValue {
                field: checked_id(index)?,
                value,
            });
        }
        let start = checked_id(self.output.fields.len())?;
        self.output.fields.extend(formed);
        Ok(hir::ExprKind::Construct {
            variant: checked_id(constructor.index)?,
            fields: start..checked_id(self.output.fields.len())?,
        })
    }

    pub(super) fn type_value(
        &mut self,
        source: ast::ExprId,
        _id: hir::ExprId,
        ty: ast::TyId,
        span: &Span,
    ) -> Result<(hir::ExprKind, u32), Diagnostic> {
        let ast_ty = &self.arena().tys[ty.0 as usize];
        match ast_ty.kind {
            ast::TyKind::Path(path) => {
                let (steps, ty) = self.path_steps(
                    self.arena().paths[path.0 as usize]
                        .segments
                        .as_slice(&self.arena().segments),
                    None,
                )?;
                let id = _id;
                self.output.expression_types[id.index()] = self.type_id(&ty)?;
                self.emit_path(source, id, steps, span)
            }
            ast::TyKind::Ptr(inner) | ast::TyKind::Ref(inner) => {
                let ty = self.type_value_ty(inner)?;
                let child = self.reserve(&ty)?;
                let (kind, effects) = self.type_value(
                    source,
                    child,
                    inner,
                    &self.arena().tys[inner.0 as usize].span,
                )?;
                self.set_expression(
                    child,
                    kind,
                    self.scope,
                    &self.arena().tys[inner.0 as usize].span,
                    effects,
                )?;
                let operation = if matches!(ast_ty.kind, ast::TyKind::Ptr(_)) {
                    ast::UnOp::Deref
                } else {
                    ast::UnOp::Ref
                };
                Ok((
                    hir::ExprKind::Unary {
                        operation,
                        value: child,
                    },
                    if operation == ast::UnOp::Deref {
                        hir::Effects::READ | hir::Effects::UNSAFE
                    } else {
                        0
                    },
                ))
            }
            _ => Err(self.error("类型调用头没有对应的值操作")),
        }
    }
    fn type_value_ty(&mut self, source: ast::TyId) -> Result<Ty, Diagnostic> {
        match self.arena().tys[source.0 as usize].kind {
            ast::TyKind::Path(path) => self
                .path_steps(
                    self.arena().paths[path.0 as usize]
                        .segments
                        .as_slice(&self.arena().segments),
                    None,
                )
                .map(|(_, ty)| ty),
            ast::TyKind::Ref(inner) => Ok(Ty::Ref(Box::new(self.type_value_ty(inner)?))),
            ast::TyKind::Ptr(inner) => match self.type_value_ty(inner)? {
                Ty::Ref(inner) | Ty::Ptr(inner) => Ok(*inner),
                _ => Err(self.error("类型形状的解引用缺少引用或指针")),
            },
            _ => Err(self.error("类型调用头不是值操作")),
        }
    }
}
