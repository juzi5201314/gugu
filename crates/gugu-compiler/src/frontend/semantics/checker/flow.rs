use super::*;
impl Checker<'_, '_> {
    pub(super) fn merge(&mut self, base: &State, branches: &[State]) {
        let reachable: Vec<_> = branches.iter().filter(|s| s.reachable).collect();
        self.state = base.clone();
        self.state.reachable = !reachable.is_empty();
        for (slot, values) in self.state.callables.iter_mut().enumerate() {
            values.clear();
            for state in &reachable {
                if let Some(origins) = state.callables.get(slot) {
                    values.extend_from_slice(origins);
                }
            }
            values.sort_unstable();
            values.dedup();
        }
        self.state.cleanup_paths.clear();
        for branch in &reachable {
            for (&id, path) in &branch.cleanup_paths {
                self.state
                    .cleanup_paths
                    .entry(id)
                    .or_insert_with(|| path.clone());
            }
        }
        for (&id, path) in &mut self.state.cleanup_paths {
            path.mandatory = reachable.iter().all(|state| {
                state
                    .cleanup_paths
                    .get(&id)
                    .is_some_and(|path| path.mandatory)
            });
            path.initialized.resize(self.slots.len(), false);
            for (slot, initialized) in path.initialized.iter_mut().enumerate() {
                *initialized = reachable
                    .iter()
                    .filter_map(|state| state.cleanup_paths.get(&id))
                    .all(|path| path.initialized.get(slot) == Some(&true));
            }
        }
        for i in 0..self.state.initialized.len() {
            self.state.initialized[i] = reachable
                .iter()
                .all(|s| s.initialized.get(i) == Some(&true));
        }
    }
    pub(super) fn block(
        &mut self,
        stmts: AstRange<StmtId>,
        tail: Option<ExprId>,
        expected: Option<&Ty>,
    ) -> Ty {
        let names = self.state.names.clone();
        let cleanup_floor = self.defers.len();
        for &id in stmts.as_slice(&self.arena().stmt_ids) {
            if self.model.modules[self.module].configured.stmt_active(id) {
                self.statement(id);
            }
        }
        let ty = tail.map_or(Ty::Unit, |id| {
            let statement_if = expected == Some(&Ty::Unit)
                && matches!(
                    self.arena().exprs[id.0 as usize].kind,
                    ExprKind::If {
                        else_branch: None,
                        ..
                    }
                );
            if statement_if {
                let outer = self.discarded_expression.replace(id);
                let ty = self.expression(id, None);
                self.discarded_expression = outer;
                ty
            } else {
                self.expression(id, expected)
            }
        });
        self.run_cleanups(cleanup_floor, false);
        let retained: Vec<_> = self.defers[cleanup_floor..]
            .iter()
            .filter(|deferred| deferred.function_exit())
            .cloned()
            .collect();
        self.defers.truncate(cleanup_floor);
        self.defers.extend(retained);
        self.state.names = names;
        if self.state.reachable { ty } else { Ty::Never }
    }
    fn statement(&mut self, id: StmtId) {
        let stmt = &self.arena().stmts[id.0 as usize];
        match stmt.kind {
            StmtKind::Static { name, ty, value } => {
                let ty = self.form(ty);
                // 延迟初始化器没有外层自动槽；每次访问复用这个 static 的身份。
                let names = std::mem::take(&mut self.state.names);
                let saved_state = self.state.clone();
                self.expression(value, Some(&ty));
                self.state = saved_state;
                self.state.names = names;
                self.slot(name, ty.clone(), true);
                self.local_statics.push(super::super::output::LocalStatic {
                    statement: id,
                    initializer: value,
                    ty,
                });
            }
            StmtKind::Let {
                pat,
                ty,
                init,
                else_block,
            } => {
                if init.is_none() && ty.is_none() {
                    self.error(
                        DiagnosticCode::InvalidDeclaration,
                        "未初始化声明必须显式标注类型",
                        stmt.span.clone(),
                    );
                }
                let annotation = ty.map(|id| self.form(id));
                if let Some(initializer) = init
                    && matches!(
                        self.arena().exprs[initializer.0 as usize].kind,
                        ExprKind::Closure(_)
                    )
                    && let PatKind::Ident(name) = self.arena().pats[pat.0 as usize].kind
                {
                    let ty = annotation.unwrap_or_else(|| self.fresh());
                    let slot = self.slots.len();
                    self.bind(pat, &ty, false, Some(false));
                    self.expression(initializer, Some(&ty));
                    self.initialize(slot, true);
                    self.state.callables[slot] = self.value_callables(initializer);
                    debug_assert_eq!(self.state.names.get(&name), Some(&slot));
                    return;
                }
                let value = init
                    .map(|id| self.expression(id, annotation.as_ref()))
                    .or(annotation)
                    .unwrap_or_else(|| self.fresh());
                if init.is_none()
                    && !matches!(self.arena().pats[pat.0 as usize].kind, PatKind::Ident(_))
                {
                    self.error(
                        DiagnosticCode::InvalidDeclaration,
                        "无初始化器的 let 只允许简单标识符",
                        stmt.span.clone(),
                    );
                }
                if let Some(block) = else_block {
                    let success = self.state.clone();
                    let ty = self.expression(block, None);
                    if ty != Ty::Never {
                        self.error(
                            DiagnosticCode::InvalidLetElse,
                            "let-else 失败分支必须发散",
                            stmt.span.clone(),
                        );
                    }
                    self.state = success;
                }
                let origins =
                    init.map_or_else(Vec::new, |initializer| self.value_callables(initializer));
                self.bind(pat, &value, init.is_some(), Some(else_block.is_some()));
                if let PatKind::Ident(name) = self.arena().pats[pat.0 as usize].kind
                    && let Some(&slot) = self.state.names.get(&name)
                {
                    self.state.callables[slot] = origins;
                }
            }
            StmtKind::Assign { op, place, value } => {
                let discard = matches!(self.arena().exprs[place.0 as usize].kind,ExprKind::Path(path) if self.model.path(self.module,path).as_slice()==["_"]);
                if discard {
                    if op != AssignOp::Assign {
                        self.error(
                            DiagnosticCode::InvalidExpression,
                            "丢弃赋值只允许使用 `=`",
                            stmt.span.clone(),
                        );
                    }
                    let outer = self.discarded_expression.replace(value);
                    self.expression(value, None);
                    self.discarded_expression = outer;
                } else {
                    let left = self.place(place, op != AssignOp::Assign);
                    let trait_assignment =
                        op != AssignOp::Assign && !self.is_number(&left) && left != Ty::String;
                    let right = self.expression(
                        value,
                        if matches!(op, AssignOp::Shl | AssignOp::Shr) || trait_assignment {
                            None
                        } else {
                            Some(&left)
                        },
                    );
                    if trait_assignment {
                        self.compound_trait(place, op, &left, &right, &stmt.span);
                    } else if op != AssignOp::Assign {
                        self.operation_type(
                            place,
                            match op {
                                AssignOp::Add => BinOp::Add,
                                AssignOp::Sub => BinOp::Sub,
                                AssignOp::Mul => BinOp::Mul,
                                AssignOp::Div => BinOp::Div,
                                AssignOp::Rem => BinOp::Rem,
                                AssignOp::BitAnd => BinOp::BitAnd,
                                AssignOp::BitOr => BinOp::BitOr,
                                AssignOp::BitXor => BinOp::BitXor,
                                AssignOp::Shl => BinOp::Shl,
                                AssignOp::Shr => BinOp::Shr,
                                AssignOp::Assign => unreachable!(),
                            },
                            &left,
                            &right,
                            &stmt.span,
                        );
                    }
                    if op != AssignOp::Assign {
                        self.index_writeback(place, &stmt.span);
                    }
                    self.place_written(place);
                    if let ExprKind::Path(path) = self.arena().exprs[place.0 as usize].kind {
                        let segs = self.arena().paths[path.0 as usize]
                            .segments
                            .as_slice(&self.arena().segments);
                        if segs.len() == 1 {
                            if let Some(&slot) = self.state.names.get(&segs[0].name) {
                                self.initialize(slot, true);
                                self.state.callables[slot] = self.value_callables(value);
                            }
                        }
                    }
                }
            }
            StmtKind::Expr { expr, .. } => {
                let outer = self.discarded_expression.replace(expr);
                self.expression(expr, None);
                self.discarded_expression = outer;
            }
            StmtKind::Defer { body, ret } => self.register_defer(id, body, ret),
            StmtKind::Yield => {}
            StmtKind::SourceMacro { .. } => self.error(
                DiagnosticCode::InvalidExpression,
                "源码宏必须先完成展开",
                stmt.span.clone(),
            ),
        }
    }
    pub(super) fn condition(&mut self, id: ExprId) -> State {
        let expr = &self.arena().exprs[id.0 as usize];
        match expr.kind {
            ExprKind::Binary {
                op: BinOp::And,
                lhs,
                rhs,
            } => {
                let base = self.state.clone();
                let left_failed = self.condition(lhs);
                let right_failed = self.condition(rhs);
                let success = self.state.clone();
                self.merge(&base, &[left_failed, right_failed]);
                let failed = self.state.clone();
                self.state = success;
                self.expressions.push((id, Ty::Bool));
                failed
            }
            ExprKind::Block { stmts, tail: None } if stmts.len == 1 => {
                let stmt =
                    &self.arena().stmts[stmts.as_slice(&self.arena().stmt_ids)[0].0 as usize];
                if let StmtKind::Let {
                    pat,
                    init: Some(init),
                    ..
                } = stmt.kind
                {
                    let ty = self.expression(init, None);
                    let failed = self.state.clone();
                    self.bind(pat, &ty, true, Some(true));
                    self.expressions.push((id, Ty::Bool));
                    failed
                } else {
                    self.expression(id, Some(&Ty::Bool));
                    self.state.clone()
                }
            }
            _ => {
                self.expression(id, Some(&Ty::Bool));
                self.state.clone()
            }
        }
    }
    pub(super) fn branch(
        &mut self,
        cond: ExprId,
        yes: ExprId,
        no: Option<ExprId>,
        expected: Option<&Ty>,
        value_used: bool,
    ) -> Ty {
        let base = self.state.clone();
        let failed_condition = self.condition(cond);
        let left = self.expression(yes, expected);
        let success = self.state.clone();
        self.state = failed_condition;
        let right = no.map_or(Ty::Unit, |id| self.expression(id, expected));
        let failure = self.state.clone();
        self.merge(&base, &[success, failure]);
        if no.is_none() {
            if value_used {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "取值的 if 必须有 else",
                    self.arena().exprs[cond.0 as usize].span.clone(),
                );
            }
            Ty::Unit
        } else {
            self.join(&left, &right, &self.arena().exprs[cond.0 as usize].span)
        }
    }
    pub(super) fn matching(
        &mut self,
        scrutinee: ExprId,
        arms: AstRange<MatchArm>,
        expected: Option<&Ty>,
    ) -> Ty {
        let ty = self.expression(scrutinee, None);
        let ty = self.pattern_type(&ty);
        let base = self.state.clone();
        let mut states = Vec::new();
        let mut result = Ty::Never;
        let mut coverage = Vec::new();
        let mut fallthrough = base.clone();
        let mut patterns_valid = true;
        for (offset, arm) in arms.as_slice(&self.arena().match_arms).iter().enumerate() {
            if !self.model.modules[self.module]
                .configured
                .match_arm_active(arms.start as usize + offset)
            {
                continue;
            }
            self.state = fallthrough.clone();
            let errors_before_pattern = self.errors.len();
            let irrefutable = self.bind(arm.pat, &ty, true, None);
            patterns_valid &= self.errors.len() == errors_before_pattern;
            let guard_failed = if let Some(guard) = arm.guard {
                self.condition(guard)
            } else {
                self.state.clone()
            };
            let body = self.expression(arm.body, expected);
            result = self.join(&body, &result, &arm.span);
            if self.state.reachable {
                states.push(self.state.clone());
            }
            if arm.guard.is_none() {
                if irrefutable {
                    fallthrough.reachable = false;
                }
            } else if irrefutable {
                fallthrough.initialized = guard_failed.initialized;
                fallthrough.cleanup_paths = guard_failed.cleanup_paths;
                fallthrough.reachable = guard_failed.reachable;
            } else {
                self.merge(&fallthrough.clone(), &[fallthrough.clone(), guard_failed]);
                fallthrough = self.state.clone();
            }
            coverage.push((arm.pat, arm.guard.is_some()));
        }
        if patterns_valid {
            match patterns::exhaustive(self.model, self.module, &self.resolve(&ty), &coverage) {
                Ok(true) => {}
                Ok(false) => self.error(
                    DiagnosticCode::InvalidPattern,
                    "match 未穷尽",
                    self.arena().exprs[scrutinee.0 as usize].span.clone(),
                ),
                Err(errors) => self.errors.extend(errors),
            }
        }
        self.merge(&base, &states);
        result
    }
    pub(super) fn looping(
        &mut self,
        body: ExprId,
        cond: Option<ExprId>,
        iter: Option<(PatId, ExprId)>,
    ) -> Ty {
        let base = self.state.clone();
        let mut natural_exit = cond.map(|cond| self.condition(cond));
        if let Some((pat, iter)) = iter {
            let ty = self.expression(iter, None);
            natural_exit = Some(self.state.clone());
            let elem = match ty.deref() {
                Ty::Array(t, _) | Ty::Slice(t) => (**t).clone(),
                Ty::Range => Ty::int(),
                _ => self.user_iterator(iter, &ty),
            };
            self.bind(pat, &elem, true, Some(false));
        }
        self.loops.push(LoopState {
            values: Vec::new(),
            exits: Vec::new(),
            value_allowed: cond.is_none() && iter.is_none(),
            cleanup_floor: self.defers.len(),
        });
        self.expression(body, None);
        let loop_state = self.loops.pop().expect("循环上下文已建立");
        let mut exits = loop_state.exits;
        if let Some(state) = natural_exit {
            exits.push(state);
        }
        self.merge(&base, &exits);
        let mut result = if loop_state.value_allowed {
            Ty::Never
        } else {
            Ty::Unit
        };
        for ty in loop_state.values {
            result = self.join(&ty, &result, &self.arena().exprs[body.0 as usize].span);
        }
        result
    }
}
