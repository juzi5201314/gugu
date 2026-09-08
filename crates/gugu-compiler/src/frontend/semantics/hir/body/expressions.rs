use super::*;
mod calls;
mod literals;
mod paths;

impl BodyBuilder<'_, '_, '_, '_> {
    fn repeat_count(&self, count: ast::ExprId) -> Result<u64, Diagnostic> {
        let value = match self
            .compiler
            .checked
            .early_constants
            .expression_value(self.module, count.0)
        {
            Some(crate::frontend::semantics::comptime::eval::ConstantValue::Int(value)) => {
                Ok(*value)
            }
            _ => self.compiler.model.constant_int(self.module, count),
        };
        u64::try_from(value?).map_err(|_| self.error("已检查重复长度不在 u64 范围"))
    }

    pub(super) fn expression(&mut self, source: ast::ExprId) -> Result<hir::ExprId, Diagnostic> {
        if let Some(id) = self.expression_map[source.0 as usize] {
            return Ok(id);
        }
        let expression = &self.arena().exprs[source.0 as usize];
        if let ast::ExprKind::Paren(inner)
        | ast::ExprKind::TypeApp { base: inner, .. }
        | ast::ExprKind::Unsafe(inner) = expression.kind
        {
            let unsafe_block = matches!(expression.kind, ast::ExprKind::Unsafe(_));
            self.unsafe_depth += usize::from(unsafe_block);
            let result = self.expression(inner)?;
            self.unsafe_depth -= usize::from(unsafe_block);
            self.expression_map[source.0 as usize] = Some(result);
            if matches!(expression.kind, ast::ExprKind::TypeApp { .. }) {
                let target = self.type_id(self.ty(source)?)?;
                if self.output.expression_types[result.index()] != target {
                    self.adjustments[result.index()].push(hir::Adjustment::Instantiate(target));
                    self.output.expression_types[result.index()] = target;
                }
            }
            return Ok(result);
        }
        let ty = self
            .facts
            .body
            .adjustments
            .iter()
            .find(|adjustment| adjustment.expression == source)
            .map_or_else(|| self.ty(source), |adjustment| Ok(&adjustment.source))?;
        let id = self.reserve(ty)?;
        self.expression_map[source.0 as usize] = Some(id);
        let previous_scope = self.scope;
        match expression.kind {
            ast::ExprKind::Block { .. } => {
                self.new_scope(hir::ScopeKind::Block, &expression.span)?;
            }
            ast::ExprKind::Loop(_) | ast::ExprKind::While { .. } | ast::ExprKind::For { .. } => {
                self.new_scope(hir::ScopeKind::Loop, &expression.span)?;
            }
            ast::ExprKind::Try(_) => {
                self.new_scope(hir::ScopeKind::Try, &expression.span)?;
            }
            _ => {}
        }
        let scope = self.scope;
        let (kind, effects) = self.expression_kind(source, id, ty)?;
        self.set_expression(id, kind, scope, &expression.span, effects)?;
        self.scope = previous_scope;
        Ok(id)
    }

    fn expression_kind(
        &mut self,
        source: ast::ExprId,
        id: hir::ExprId,
        ty: &Ty,
    ) -> Result<(hir::ExprKind, u32), Diagnostic> {
        let expression = &self.arena().exprs[source.0 as usize];
        let mut effects = 0;
        let kind = match expression.kind {
            ast::ExprKind::Path(path) => {
                return self.path_expression(source, id, path, ty, &expression.span);
            }
            ast::ExprKind::Literal(literal) => {
                hir::ExprKind::Literal(self.literal(literal, ty, false)?)
            }
            ast::ExprKind::Tuple(items) | ast::ExprKind::Array(items) => {
                let items = self.lower_arguments(items)?;
                if matches!(expression.kind, ast::ExprKind::Tuple(_)) {
                    hir::ExprKind::Tuple(items)
                } else {
                    hir::ExprKind::Array(items)
                }
            }
            ast::ExprKind::Repeat { elem, count } => {
                let value = self.expression(elem)?;
                let count = self.repeat_count(count)?;
                hir::ExprKind::Repeat { value, count }
            }
            ast::ExprKind::Struct { path, fields } => self.record_value(path, fields, ty)?,
            ast::ExprKind::Block { stmts, tail } => self.block(stmts, tail)?,
            ast::ExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                let names = self.names.clone();
                let condition = self.condition(cond)?;
                let then_value = self.branch_expression(then_block)?;
                self.names.clone_from(&names);
                let else_value = else_branch
                    .map(|branch| self.branch_expression(branch))
                    .transpose()?;
                self.names = names;
                hir::ExprKind::If {
                    condition,
                    then_value,
                    else_value,
                }
            }
            ast::ExprKind::Match { scrutinee, arms } => self.matching(scrutinee, arms)?,
            ast::ExprKind::Loop(body) => {
                self.loops.push(self.scope);
                let body = self.branch_expression(body)?;
                self.loops.pop();
                hir::ExprKind::Loop { body }
            }
            ast::ExprKind::While { cond, body } => {
                let names = self.names.clone();
                self.loops.push(self.scope);
                let condition = self.condition(cond)?;
                let body = self.branch_expression(body)?;
                self.loops.pop();
                self.names = names;
                hir::ExprKind::While { condition, body }
            }
            ast::ExprKind::For { pat, iter, body } => {
                let names = self.names.clone();
                let value = self.expression(iter)?;
                let pattern = self.bind_pattern(pat)?;
                self.loops.push(self.scope);
                let body = self.branch_expression(body)?;
                self.loops.pop();
                self.names = names;
                hir::ExprKind::For {
                    pattern,
                    value,
                    body,
                    into_iter: self.selected_dispatch(
                        source,
                        Some("IntoIter"),
                        Some("into_iter"),
                    )?,
                    next: self.selected_dispatch(source, Some("Iter"), Some("next"))?,
                }
            }
            ast::ExprKind::Try(body) => {
                self.tries.push(self.scope);
                let body = self.branch_expression(body)?;
                self.tries.pop();
                hir::ExprKind::Try {
                    body,
                    from_value: self.selected_dispatch(source, Some("Try"), Some("from_value"))?,
                }
            }
            ast::ExprKind::TryOp(value) => {
                let value = self.expression(value)?;
                let target = self
                    .tries
                    .last()
                    .copied()
                    .map_or(hir::ExitTarget::Return, hir::ExitTarget::Try);
                hir::ExprKind::TryExit {
                    value,
                    branch: self.selected_dispatch(source, Some("Try"), Some("branch"))?,
                    from_error: self.selected_dispatch(source, Some("Try"), Some("from_error"))?,
                    target,
                    cleanup: self.cleanup_scopes(target)?,
                    plan: self.request_plan(Self::exit_kind(target), self.scope)?,
                }
            }
            ast::ExprKind::Select { arms } => {
                effects = hir::Effects::SUSPEND | hir::Effects::SAFEPOINT;
                self.selecting(arms)?
            }
            ast::ExprKind::Closure(function) => {
                let body = body_expression(self.arena().fns[function.0 as usize].body)
                    .ok_or_else(|| self.error("闭包没有 body"))?;
                let definition = self.nested(source, Some(function), body)?;
                if self
                    .compiler
                    .output
                    .owners
                    .iter()
                    .find(|owner| owner.definition == definition)
                    .is_some_and(|owner| !owner.captures.is_empty())
                {
                    effects = hir::Effects::ALLOCATE | hir::Effects::SAFEPOINT;
                }
                hir::ExprKind::Closure { definition }
            }
            ast::ExprKind::Async(body) => {
                effects = hir::Effects::ALLOCATE | hir::Effects::SAFEPOINT;
                if let ast::ExprKind::Call {
                    callee,
                    type_args,
                    args,
                } = self.arena().exprs[body.0 as usize].kind
                {
                    let (call, _) = self.call(body, id, callee, type_args, args, true)?;
                    call
                } else {
                    hir::ExprKind::Spawn {
                        definition: self.nested(source, None, body)?,
                    }
                }
            }
            ast::ExprKind::Call {
                callee,
                type_args,
                args,
            } => return self.call(source, id, callee, type_args, args, false),
            ast::ExprKind::Field { base, name } => self.field_value(
                base,
                self.compiler.model.name(self.module, name),
                &expression.span,
            )?,
            ast::ExprKind::TupleField { base, index } => {
                let base = self.expression(base)?;
                hir::ExprKind::Field { base, index }
            }
            ast::ExprKind::Index { base, index } => {
                let base = self.expression(base)?;
                match index {
                    ast::IndexKind::Expr(index) => hir::ExprKind::Index {
                        base,
                        index: self.expression(index)?,
                        read: self.selected_dispatch(source, Some("Index"), Some("index"))?,
                        write: self.selected_dispatch(source, Some("Index"), Some("index_set"))?,
                    },
                    ast::IndexKind::Range { start, end } => hir::ExprKind::Slice {
                        base,
                        start: start.map(|start| self.expression(start)).transpose()?,
                        end: end.map(|end| self.expression(end)).transpose()?,
                    },
                }
            }
            ast::ExprKind::Unary { op, expr } => {
                if op == ast::UnOp::Deref {
                    effects = hir::Effects::READ
                        | if matches!(self.ty(expr)?, Ty::Ptr(_)) {
                            hir::Effects::UNSAFE
                        } else {
                            0
                        };
                }
                hir::ExprKind::Unary {
                    operation: op,
                    value: self.expression(expr)?,
                }
            }
            ast::ExprKind::Binary { op, lhs, rhs } => {
                let left = self.expression(lhs)?;
                let right = self.expression(rhs)?;
                let dispatch = self.selected_dispatch(source, None, None)?;
                if dispatch.is_some() {
                    effects = hir::Effects::PANIC
                        | hir::Effects::ALLOCATE
                        | hir::Effects::SAFEPOINT
                        | hir::Effects::SUSPEND;
                }
                hir::ExprKind::Binary {
                    operation: op,
                    left,
                    right,
                    dispatch,
                }
            }
            ast::ExprKind::Range { start, end } => hir::ExprKind::Range {
                start: self.expression(start)?,
                end: self.expression(end)?,
            },
            ast::ExprKind::Comptime(value) => hir::ExprKind::Comptime {
                value: self.expression(value)?,
            },
            ast::ExprKind::Intrinsic {
                kind,
                tys,
                args,
                field,
            } => self.intrinsic(kind, tys, args, field)?,
            ast::ExprKind::Asm { .. } => {
                effects = hir::Effects::UNSAFE | hir::Effects::READ | hir::Effects::WRITE;
                self.assembly(source)?
            }
            ast::ExprKind::Return(value) | ast::ExprKind::Break(value) => {
                let target = if matches!(expression.kind, ast::ExprKind::Return(_)) {
                    hir::ExitTarget::Return
                } else {
                    hir::ExitTarget::Break(
                        *self
                            .loops
                            .last()
                            .ok_or_else(|| self.error("break 没有已解析循环目标"))?,
                    )
                };
                hir::ExprKind::Exit {
                    target,
                    value: value.map(|value| self.expression(value)).transpose()?,
                    cleanup: self.cleanup_scopes(target)?,
                    plan: self.request_plan(Self::exit_kind(target), self.scope)?,
                }
            }
            ast::ExprKind::Continue => {
                let target = hir::ExitTarget::Continue(
                    *self
                        .loops
                        .last()
                        .ok_or_else(|| self.error("continue 没有已解析循环目标"))?,
                );
                hir::ExprKind::Exit {
                    target,
                    value: None,
                    cleanup: self.cleanup_scopes(target)?,
                    plan: self.request_plan(Self::exit_kind(target), self.scope)?,
                }
            }
            ast::ExprKind::FString { parts } => {
                effects = hir::Effects::ALLOCATE | hir::Effects::SAFEPOINT;
                self.formatted_string(parts)?
            }
            ast::ExprKind::TypeCallee(ty) => {
                return self.type_value(source, id, ty, &expression.span);
            }
            ast::ExprKind::Paren(_) | ast::ExprKind::TypeApp { .. } | ast::ExprKind::Unsafe(_) => {
                unreachable!("无语义包装已归一化")
            }
            ast::ExprKind::Error | ast::ExprKind::SourceMacro { .. } => {
                return Err(self.error("错误或未展开表达式不能进入 HIR"));
            }
        };
        Ok((kind, effects))
    }

    fn condition(&mut self, source: ast::ExprId) -> Result<hir::ExprId, Diagnostic> {
        let expression = &self.arena().exprs[source.0 as usize];
        let normalized = match expression.kind {
            ast::ExprKind::Binary {
                op: ast::BinOp::And,
                lhs,
                rhs,
            } => Some((None, Some((lhs, rhs)))),
            ast::ExprKind::Block { stmts, tail: None } if stmts.len == 1 => {
                let statement =
                    &self.arena().stmts[stmts.as_slice(&self.arena().stmt_ids)[0].0 as usize];
                if let ast::StmtKind::Let {
                    pat,
                    init: Some(value),
                    ..
                } = statement.kind
                {
                    Some((Some((pat, value)), None))
                } else {
                    None
                }
            }
            _ => None,
        };
        let Some((binding, chain)) = normalized else {
            return self.expression(source);
        };
        let id = self.reserve(&Ty::Bool)?;
        self.expression_map[source.0 as usize] = Some(id);
        let kind = if let Some((pattern, value)) = binding {
            let value = self.expression(value)?;
            hir::ExprKind::LetCondition {
                pattern: self.bind_pattern(pattern)?,
                value,
            }
        } else {
            let (left, right) = chain.expect("条件分类为 let 或 &&");
            hir::ExprKind::Binary {
                operation: ast::BinOp::And,
                left: self.condition(left)?,
                right: self.condition(right)?,
                dispatch: None,
            }
        };
        self.set_expression(id, kind, self.scope, &expression.span, 0)?;
        Ok(id)
    }

    fn matching(
        &mut self,
        value: ast::ExprId,
        arms: ast::AstRange<ast::MatchArm>,
    ) -> Result<hir::ExprKind, Diagnostic> {
        let value = self.expression(value)?;
        let names = self.names.clone();
        let mut formed = Vec::new();
        for (offset, arm) in arms.as_slice(&self.arena().match_arms).iter().enumerate() {
            if !self.compiler.model.modules[self.module]
                .configured
                .match_arm_active(arms.start as usize + offset)
            {
                continue;
            }
            self.names.clone_from(&names);
            let pattern = self.bind_pattern(arm.pat)?;
            let guard = arm.guard.map(|guard| self.condition(guard)).transpose()?;
            formed.push(hir::MatchArm {
                pattern,
                guard,
                body: self.branch_expression(arm.body)?,
            });
        }
        self.names = names;
        let start = checked_id(self.output.arms.len())?;
        self.output.arms.extend(formed);
        Ok(hir::ExprKind::Match {
            value,
            arms: start..checked_id(self.output.arms.len())?,
        })
    }

    fn selecting(
        &mut self,
        arms: ast::AstRange<ast::SelectArm>,
    ) -> Result<hir::ExprKind, Diagnostic> {
        let names = self.names.clone();
        let mut formed = Vec::new();
        for (offset, arm) in arms.as_slice(&self.arena().select_arms).iter().enumerate() {
            if !self.compiler.model.modules[self.module]
                .configured
                .select_arm_active(arms.start as usize + offset)
            {
                continue;
            }
            self.names.clone_from(&names);
            formed.push(match arm.kind {
                ast::SelectArmKind::Send {
                    chan,
                    payload,
                    body,
                } => hir::SelectArm::Send {
                    channel: self.expression(chan)?,
                    value: self.expression(payload)?,
                    body: self.branch_expression(body)?,
                },
                ast::SelectArmKind::Recv { pat, chan, body } => hir::SelectArm::Recv {
                    channel: self.expression(chan)?,
                    pattern: self.bind_pattern(pat)?,
                    body: self.branch_expression(body)?,
                },
                ast::SelectArmKind::Wait { pat, join, body } => hir::SelectArm::Wait {
                    join: self.expression(join)?,
                    pattern: self.bind_pattern(pat)?,
                    body: self.branch_expression(body)?,
                },
                ast::SelectArmKind::Default { body } => hir::SelectArm::Default {
                    body: self.branch_expression(body)?,
                },
                ast::SelectArmKind::Error => return Err(self.error("错误 select arm 不能进入 HIR")),
            });
        }
        self.names = names;
        let start = checked_id(self.output.select_arms.len())?;
        self.output.select_arms.extend(formed);
        Ok(hir::ExprKind::Select {
            arms: start..checked_id(self.output.select_arms.len())?,
        })
    }

    fn lower_arguments(
        &mut self,
        arguments: ast::AstRange<ast::ExprId>,
    ) -> Result<std::ops::Range<u32>, Diagnostic> {
        let configured = &self.compiler.model.modules[self.module].configured;
        let values = arguments
            .as_slice(&self.arena().expr_ids)
            .iter()
            .copied()
            .filter(|&argument| configured.expr_active(argument))
            .map(|argument| self.expression(argument))
            .collect::<Result<Vec<_>, _>>()?;
        self.expression_list(values)
    }
}
