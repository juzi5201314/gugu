use super::*;
use crate::semantics::DefRef;
use crate::string::{self, Alignment, Sign};

impl BodyBuilder<'_, '_, '_, '_> {
    pub(super) fn runtime_formatted_string(
        &mut self,
        parts: ast::AstRange<ast::FStringPart>,
    ) -> Result<hir::ExprKind, Diagnostic> {
        let span = self.compiler.model.modules[self.module].arena.items
            [self.facts.body.definition.item.0 as usize]
            .span
            .clone();
        let (new, definition) = self.format_begin("__formatter_new", &span)?;
        self.format_end(new, definition, Vec::new(), &span)?;
        let ty = self.compiler.model.formatter_type();
        let (formatter, statement) = self.format_binding(&ty, new, &span)?;
        let mut statements = vec![statement];
        for (offset, part) in parts
            .as_slice(&self.arena().fstring_parts)
            .to_vec()
            .into_iter()
            .enumerate()
        {
            match part {
                ast::FStringPart::Text { text, .. } => {
                    let (call, definition) = self.format_begin("__formatter_write", &span)?;
                    let reference = self.format_reference(formatter, &span)?;
                    let text =
                        string::decode_fstring_text(self.compiler.model.name(self.module, text))
                            .into_owned();
                    let text =
                        self.format_literal(&Ty::String, hir::Literal::String(text), &span)?;
                    self.format_end(call, definition, vec![reference, text], &span)?;
                    statements
                        .push(self.format_statement(hir::StatementKind::Expression(call), &span)?);
                }
                ast::FStringPart::Interp {
                    expr,
                    span: hole_span,
                    ..
                } => {
                    let expression = self.expression(expr)?;
                    let ty = self
                        .facts
                        .ty(expr)
                        .ok_or_else(|| self.error("格式值缺少类型"))?
                        .clone();
                    if ty == Ty::Never {
                        statements.push(self.format_statement(
                            hir::StatementKind::Expression(expression),
                            &hole_span,
                        )?);
                        continue;
                    }
                    let (value, statement) = self.format_binding(&ty, expression, &hole_span)?;
                    statements.push(statement);
                    let plan = self
                        .facts
                        .body
                        .formatting
                        .iter()
                        .find(|plan| plan.part == parts.start + offset as u32)
                        .ok_or_else(|| self.error("插值缺少已解析格式计划"))?
                        .clone();
                    let (call, definition) = self.format_begin("__formatter_spec", &span)?;
                    let reference = self.format_reference(formatter, &span)?;
                    let spec = plan
                        .spec
                        .try_map(|count| self.format_count(count, &hole_span))?;
                    let fill =
                        self.format_literal(&Ty::Char, hir::Literal::Char(spec.fill), &span)?;
                    let alignment = self.format_integer(
                        match spec.alignment {
                            None => 0,
                            Some(Alignment::Left) => 1,
                            Some(Alignment::Center) => 2,
                            Some(Alignment::Right) => 3,
                        },
                        &span,
                    )?;
                    let sign = self.format_integer(
                        match spec.sign {
                            None => 0,
                            Some(Sign::Plus) => 1,
                            Some(Sign::Minus) => 2,
                            Some(Sign::Space) => 3,
                        },
                        &span,
                    )?;
                    let alternate =
                        self.format_literal(&Ty::Bool, hir::Literal::Bool(spec.alternate), &span)?;
                    let zero =
                        self.format_literal(&Ty::Bool, hir::Literal::Bool(spec.zero), &span)?;
                    let has_width = self.format_literal(
                        &Ty::Bool,
                        hir::Literal::Bool(spec.width.is_some()),
                        &span,
                    )?;
                    let width = self.format_count_value(spec.width, &span)?;
                    let has_precision = self.format_literal(
                        &Ty::Bool,
                        hir::Literal::Bool(spec.precision.is_some()),
                        &span,
                    )?;
                    let precision = self.format_count_value(spec.precision, &span)?;
                    self.format_end(
                        call,
                        definition,
                        vec![
                            reference,
                            fill,
                            alignment,
                            sign,
                            alternate,
                            zero,
                            has_width,
                            width,
                            has_precision,
                            precision,
                        ],
                        &span,
                    )?;
                    statements
                        .push(self.format_statement(hir::StatementKind::Expression(call), &span)?);
                    let (interface, member) = spec.kind.trait_method();
                    let dispatch = self
                        .selected_dispatch(expr, Some(interface), Some(member))?
                        .ok_or_else(|| self.error("格式化缺少 trait 派发"))?;
                    let call = self.reserve(&Ty::Unit)?;
                    let receiver = self.format_local(value, &span)?;
                    let reference = self.format_reference(formatter, &span)?;
                    let arguments = self.expression_list([reference])?;
                    self.set_expression(
                        call,
                        hir::ExprKind::Call {
                            target: hir::CallTarget::Dispatch(dispatch),
                            receiver: Some(receiver),
                            arguments,
                        },
                        self.scope,
                        &span,
                        hir::Effects::SAFEPOINT | hir::Effects::WRITE,
                    )?;
                    statements
                        .push(self.format_statement(hir::StatementKind::Expression(call), &span)?);
                }
            }
        }
        let (finish, definition) = self.format_begin("__formatter_finish", &span)?;
        let reference = self.format_reference(formatter, &span)?;
        self.format_end(finish, definition, vec![reference], &span)?;
        let start = checked_id(self.output.statement_ids.len())?;
        self.output.statement_ids.extend(statements);
        Ok(hir::ExprKind::Block {
            statements: start..checked_id(self.output.statement_ids.len())?,
            tail: Some(finish),
            end_plan: None,
        })
    }

    fn format_begin(
        &mut self,
        name: &str,
        _span: &Span,
    ) -> Result<(hir::ExprId, DefRef), Diagnostic> {
        let module = self
            .compiler
            .model
            .modules
            .iter()
            .position(|module| module.builtin && module.path == "std.fmt")
            .ok_or_else(|| self.error("缺少 std.fmt"))?;
        let definition = self.compiler.model.resolve(module, &[name])?;
        let ty = self.compiler.model.value_type(definition)?;
        let result = ty
            .signature()
            .ok_or_else(|| self.error("Formatter 入口缺少函数签名"))?
            .1
            .clone();
        Ok((self.reserve(&result)?, definition))
    }

    fn format_end(
        &mut self,
        id: hir::ExprId,
        definition: DefRef,
        args: Vec<hir::ExprId>,
        span: &Span,
    ) -> Result<(), Diagnostic> {
        let ty = self.compiler.model.value_type(definition)?;
        let callee = self.reserve(&ty)?;
        self.set_expression(
            callee,
            hir::ExprKind::Resolved(hir::Res::Def(self.compiler.identities.item(definition))),
            self.scope,
            span,
            0,
        )?;
        let arguments = self.expression_list(args)?;
        self.set_expression(
            id,
            hir::ExprKind::Call {
                target: hir::CallTarget::Value(callee),
                receiver: None,
                arguments,
            },
            self.scope,
            span,
            hir::Effects::ALLOCATE
                | hir::Effects::SAFEPOINT
                | hir::Effects::READ
                | hir::Effects::WRITE,
        )
    }

    fn format_literal(
        &mut self,
        ty: &Ty,
        value: hir::Literal,
        span: &Span,
    ) -> Result<hir::ExprId, Diagnostic> {
        let id = self.reserve(ty)?;
        self.set_expression(id, hir::ExprKind::Literal(value), self.scope, span, 0)?;
        Ok(id)
    }
    fn format_integer(&mut self, value: u64, span: &Span) -> Result<hir::ExprId, Diagnostic> {
        self.format_literal(&Ty::int(), hir::Literal::Integer(u128::from(value)), span)
    }
    fn format_count_value(
        &mut self,
        count: Option<hir::FormatCount>,
        span: &Span,
    ) -> Result<hir::ExprId, Diagnostic> {
        match count {
            Some(hir::FormatCount::Value(value)) => Ok(value),
            Some(hir::FormatCount::Fixed(value)) => self.format_integer(value, span),
            None => self.format_integer(0, span),
        }
    }
    fn format_local(
        &mut self,
        local: hir::LocalId,
        span: &Span,
    ) -> Result<hir::ExprId, Diagnostic> {
        let id = hir::ExprId(checked_id(self.expressions.len())?);
        self.expressions.push(None);
        self.adjustments.push(Vec::new());
        let ty = self.output.locals[local.index()].ty;
        self.output.expression_types.push(ty);
        self.output.expression_inputs.push(ty);
        self.set_expression(
            id,
            hir::ExprKind::Resolved(hir::Res::Local(local)),
            self.scope,
            span,
            hir::Effects::READ,
        )?;
        Ok(id)
    }
    fn format_reference(
        &mut self,
        local: hir::LocalId,
        span: &Span,
    ) -> Result<hir::ExprId, Diagnostic> {
        let ty = Ty::Ref(Box::new(self.compiler.model.formatter_type()));
        let id = self.reserve(&ty)?;
        let value = self.format_local(local, span)?;
        self.set_expression(
            id,
            hir::ExprKind::Unary {
                operation: ast::UnOp::Ref,
                value,
            },
            self.scope,
            span,
            0,
        )?;
        Ok(id)
    }
    fn format_binding(
        &mut self,
        ty: &Ty,
        value: hir::ExprId,
        span: &Span,
    ) -> Result<(hir::LocalId, hir::StmtId), Diagnostic> {
        let local = hir::LocalId(checked_id(self.output.locals.len())?);
        let ty = self.type_id(ty)?;
        let location = identity::location(self.compiler.sources, span)?;
        self.output.locals.push(hir::Local {
            name: String::new(),
            ty,
            location: location.clone(),
            storage: 1,
        });
        self.local_sources.push(None);
        let pattern = hir::PatternId(checked_id(self.patterns.len())?);
        self.patterns.push(Some(hir::Pattern {
            location,
            ty,
            kind: hir::PatternKind::Bind(local),
        }));
        let statement = self.format_statement(
            hir::StatementKind::Let {
                pattern,
                value: Some(value),
                otherwise: None,
            },
            span,
        )?;
        Ok((local, statement))
    }
    fn format_statement(
        &mut self,
        kind: hir::StatementKind,
        span: &Span,
    ) -> Result<hir::StmtId, Diagnostic> {
        let id = hir::StmtId(checked_id(self.statements.len())?);
        let unwind_plan = self.request_plan(hir::ExitKind::Unwind(self.scope), self.scope)?;
        self.statements.push(Some(hir::Statement {
            unwind_plan,
            location: identity::location(self.compiler.sources, span)?,
            scope: self.scope,
            kind,
        }));
        Ok(id)
    }
}
