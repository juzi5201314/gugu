use super::*;

impl BodyBuilder<'_, '_, '_, '_> {
    pub(super) fn block(
        &mut self,
        statements: ast::AstRange<ast::StmtId>,
        tail: Option<ast::ExprId>,
    ) -> Result<hir::ExprKind, Diagnostic> {
        let saved = self.names.clone();
        let mut nodes = Vec::with_capacity(statements.len as usize);
        for &statement in statements.as_slice(&self.arena().stmt_ids) {
            if self.compiler.model.modules[self.module]
                .configured
                .stmt_active(statement)
            {
                nodes.push(self.statement(statement)?);
            }
        }
        let tail = tail.map(|tail| self.expression(tail)).transpose()?;
        let start = checked_id(self.output.statement_ids.len())?;
        self.output.statement_ids.extend(nodes);
        self.names = saved;
        Ok(hir::ExprKind::Block {
            statements: start..checked_id(self.output.statement_ids.len())?,
            tail,
        })
    }

    fn statement(&mut self, source: ast::StmtId) -> Result<hir::StmtId, Diagnostic> {
        let statement = &self.arena().stmts[source.0 as usize];
        let id = hir::StmtId(checked_id(self.statements.len())?);
        self.statements.push(None);
        self.statement_map[source.0 as usize] = Some(id);
        let kind = match statement.kind {
            ast::StmtKind::Let {
                pat,
                init,
                else_block,
                ..
            } => {
                let recursive = init.is_some_and(|value| {
                    matches!(
                        self.arena().exprs[value.0 as usize].kind,
                        ast::ExprKind::Closure(_)
                    )
                }) && matches!(
                    self.arena().pats[pat.0 as usize].kind,
                    ast::PatKind::Ident(_)
                );
                let prebound = if recursive {
                    Some(self.bind_pattern(pat)?)
                } else {
                    None
                };
                let value = init.map(|value| self.expression(value)).transpose()?;
                // let-else 的失败分支看不到本次成功绑定。
                let otherwise = else_block
                    .map(|branch| self.branch_expression(branch))
                    .transpose()?;
                let pattern = if let Some(pattern) = prebound {
                    pattern
                } else {
                    self.bind_pattern(pat)?
                };
                hir::StatementKind::Let {
                    pattern,
                    value,
                    otherwise,
                }
            }
            ast::StmtKind::Static { name, value, .. } => {
                let slot =
                    self.source_slot(self.compiler.model.name(self.module, name), &statement.span)?;
                let local = self.local(slot)?;
                let definition = self.compiler.identities.local_statics[self.module]
                    [source.0 as usize]
                    .ok_or_else(|| self.error("local static 没有稳定定义"))?;
                self.compiler.lower_owner(
                    self.facts,
                    definition,
                    self.module,
                    None,
                    value,
                    Vec::new(),
                )?;
                hir::StatementKind::Static { local, definition }
            }
            ast::StmtKind::Assign { op, place, value } => {
                let discarded = matches!(self.arena().exprs[place.0 as usize].kind, ast::ExprKind::Path(path) if self.compiler.model.path(self.module, path) == ["_"]);
                if discarded {
                    hir::StatementKind::Expression(self.expression(value)?)
                } else {
                    let location = self.expression(place)?;
                    let value = self.expression(value)?;
                    let dispatch = self.assignment_dispatch(place)?;
                    hir::StatementKind::Assign {
                        place: location,
                        value,
                        operation: op,
                        dispatch,
                    }
                }
            }
            ast::StmtKind::Defer { ret, body } => {
                let mut captures = Vec::new();
                for plan in &self.facts.body.cleanup {
                    if plan.statement != source {
                        continue;
                    }
                    for &slot in &plan.captures {
                        let local = self.slots[slot]
                            .ok_or_else(|| self.error("清理捕获槽不在注册点可见"))?;
                        captures.push(local);
                    }
                }
                captures.sort_unstable();
                captures.dedup();
                let body = self.branch_expression(body)?;
                let action = checked_id(self.output.cleanup.len())?;
                self.output.cleanup.push(hir::Cleanup {
                    statement: id,
                    body,
                    scope: if ret { hir::ScopeId(0) } else { self.scope },
                    function_exit: ret,
                    captures,
                });
                hir::StatementKind::Defer(action)
            }
            ast::StmtKind::Yield => hir::StatementKind::Yield,
            ast::StmtKind::Expr { expr, .. } => {
                hir::StatementKind::Expression(self.expression(expr)?)
            }
            ast::StmtKind::SourceMacro { .. } => return Err(self.error("未展开语句不能进入 HIR")),
        };
        self.statements[id.index()] = Some(hir::Statement {
            location: identity::location(self.compiler.sources, &statement.span)?,
            scope: self.scope,
            kind,
        });
        Ok(id)
    }

    pub(super) fn branch_expression(
        &mut self,
        source: ast::ExprId,
    ) -> Result<hir::ExprId, Diagnostic> {
        let names = self.names.clone();
        let scope = self.scope;
        self.new_scope(
            hir::ScopeKind::Branch,
            &self.arena().exprs[source.0 as usize].span,
        )?;
        let result = self.expression(source)?;
        self.names = names;
        self.scope = scope;
        Ok(result)
    }

    pub(super) fn cleanup_scopes(
        &mut self,
        target: hir::ExitTarget,
    ) -> Result<std::ops::Range<u32>, Diagnostic> {
        let start = checked_id(self.output.scope_ids.len())?;
        let mut scope = self.scope;
        loop {
            if matches!(target, hir::ExitTarget::Continue(target) if target == scope) {
                break;
            }
            self.output.scope_ids.push(scope);
            if matches!(target, hir::ExitTarget::Break(target) | hir::ExitTarget::Try(target) if target == scope)
            {
                break;
            }
            let Some(parent) = self.output.scopes[scope.index()].parent else {
                break;
            };
            scope = parent;
        }
        Ok(start..checked_id(self.output.scope_ids.len())?)
    }
}
