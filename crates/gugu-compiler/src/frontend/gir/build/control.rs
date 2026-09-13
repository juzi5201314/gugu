use super::*;

impl Builder<'_> {
    pub(super) fn emit_block(
        &mut self,
        id: ExprId,
        statements: Range<u32>,
        tail: Option<ExprId>,
        end_plan: Option<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        for index in statements.start as usize..statements.end as usize {
            self.emit_stmt(self.owner.statement_ids[index])?;
            if self.terminated() {
                return Ok(None);
            }
        }
        let value = match tail {
            Some(tail) => self.emit_expr(tail)?,
            None => {
                let local = self.temp(self.expr_ty(id));
                self.assign_unit(local);
                Some(local)
            }
        };
        if self.terminated() {
            return Ok(None);
        }
        if let Some(plan) = end_plan {
            let cont = self.fresh(false);
            let cleanup = self.intern_cleanup(plan, CleanupChain::Normal, Some(cont))?;
            self.goto(cleanup);
            self.switch_to(cont);
        }
        if let Some(value) = value {
            self.set_value(id, value);
        }
        Ok(value)
    }

    pub(super) fn emit_stmt(&mut self, id: hir::StmtId) -> Result<(), Diagnostic> {
        let kind = self.owner.statements[id.index()].kind.clone();
        match kind {
            hir::StatementKind::Let {
                pattern,
                value,
                otherwise,
            } => self.emit_let(pattern, value, otherwise),
            hir::StatementKind::Assign {
                place,
                value,
                operation,
                dispatch,
            } => self.emit_assign(place, value, operation, dispatch),
            hir::StatementKind::Expression(value) => self.emit_expr(value).map(|_| ()),
            hir::StatementKind::Defer(action) => self.emit_defer(action),
            hir::StatementKind::Static { local, definition } => {
                self.emit_local_static(local, definition)
            }
            hir::StatementKind::Yield => self.emit_yield(),
        }
    }

    fn emit_let(
        &mut self,
        pattern: hir::PatternId,
        value: Option<ExprId>,
        otherwise: Option<ExprId>,
    ) -> Result<(), Diagnostic> {
        let Some(value) = value else {
            return Ok(());
        };
        let Some(local) = self.emit_expr(value)? else {
            return Ok(());
        };
        if let Some(otherwise) = otherwise {
            let ok = self.fresh(false);
            let fail = self.fresh(false);
            self.emit_pattern_test(Place::local(local), pattern, ok, fail)?;
            self.switch_to(fail);
            let _ = self.emit_expr(otherwise)?;
            self.switch_to(ok);
        }
        self.bind_pattern(Place::local(local), pattern)
    }

    fn emit_assign(
        &mut self,
        place: ExprId,
        value: ExprId,
        operation: crate::frontend::ast::AssignOp,
        dispatch: Option<u32>,
    ) -> Result<(), Diagnostic> {
        if let Some((write, base, index)) = self.index_write(place) {
            let base_place = self.emit_place(base)?;
            let Some(index_local) = self.emit_expr(index)? else {
                return Ok(());
            };
            if operation != crate::frontend::ast::AssignOp::Assign {
                // 复合赋值：读一次元素、更新、再经 `index_set` 写回。
                let Some(read) = self.index_read(place) else {
                    return Ok(());
                };
                let parameters = self.dispatch_parameters(read);
                let mut args = Vec::with_capacity(2);
                for (position, (local, expression)) in
                    [(base_place.local, base), (index_local, index)]
                        .into_iter()
                        .enumerate()
                {
                    args.push(match parameters.get(position).copied() {
                        Some(parameter) => {
                            self.dispatch_argument(parameter, local, Some(expression))
                        }
                        None => copy_of(local),
                    });
                }
                let mut current = self.call_dispatch(place, read, args)?;
                let Some(src) = self.emit_expr(value)? else {
                    return Ok(());
                };
                if let Some(op) = dispatch.and_then(|dispatch| self.builtin_binary(dispatch)) {
                    let ty = self.locals[current.index()].ty;
                    let dest = self.temp(ty);
                    self.assign(
                        Place::local(dest),
                        Rvalue::BinaryOp {
                            op,
                            left: copy_of(current),
                            right: copy_of(src),
                        },
                    );
                    current = dest;
                } else if let Some(dispatch) = dispatch {
                    let receiver = self.dispatch_receiver(dispatch, current, None);
                    self.call_dispatch(place, dispatch, vec![receiver, copy_of(src)])?;
                }
                let parameters = self.dispatch_parameters(write);
                let mut args = Vec::with_capacity(3);
                for (position, (local, expression)) in [
                    (base_place.local, base),
                    (index_local, index),
                    (current, value),
                ]
                .into_iter()
                .enumerate()
                {
                    args.push(match parameters.get(position).copied() {
                        Some(parameter) => {
                            self.dispatch_argument(parameter, local, Some(expression))
                        }
                        None => copy_of(local),
                    });
                }
                self.call_dispatch(place, write, args)?;
                return Ok(());
            }
            let Some(src) = self.emit_expr(value)? else {
                return Ok(());
            };
            let parameters = self.dispatch_parameters(write);
            let mut args = Vec::with_capacity(3);
            for (position, (local, expression)) in
                [(base_place.local, base), (index_local, index), (src, value)]
                    .into_iter()
                    .enumerate()
            {
                args.push(match parameters.get(position).copied() {
                    Some(parameter) => self.dispatch_argument(parameter, local, Some(expression)),
                    None => copy_of(local),
                });
            }
            self.call_dispatch(place, write, args)?;
            return Ok(());
        }
        let dest = self.emit_place(place)?;
        let Some(src) = self.emit_expr(value)? else {
            return Ok(());
        };
        if let Some(dispatch) = dispatch {
            // 内建复合赋值 impl 直接降成读改写。
            if let Some(op) = self.builtin_binary(dispatch) {
                self.assign(
                    dest,
                    Rvalue::BinaryOp {
                        op,
                        left: Operand::Copy(dest),
                        right: copy_of(src),
                    },
                );
                return Ok(());
            }
            let parameters = self.dispatch_parameters(dispatch);
            let mut args = Vec::with_capacity(2);
            for (position, (local, expression)) in
                [(dest.local, place), (src, value)].into_iter().enumerate()
            {
                args.push(match parameters.get(position).copied() {
                    Some(parameter) => self.dispatch_argument(parameter, local, Some(expression)),
                    None => copy_of(local),
                });
            }
            self.call_dispatch(place, dispatch, args)?;
            return Ok(());
        }
        match operation {
            crate::frontend::ast::AssignOp::Assign => {
                self.copy_value(dest, Place::local(src), self.place_ty(dest));
            }
            other => {
                let rvalue = compound_assign(other, Operand::Copy(dest), copy_of(src));
                self.assign(dest, rvalue);
            }
        }
        Ok(())
    }

    /// 用户下标写入：表达式是带 `index_set` 派发的下标。
    fn index_write(&self, place: ExprId) -> Option<(u32, ExprId, ExprId)> {
        match &self.owner.expressions[place.index()].kind {
            hir::ExprKind::Index {
                base,
                index,
                write: Some(dispatch),
                ..
            } => Some((*dispatch, *base, *index)),
            _ => None,
        }
    }

    /// 用户下标读取派发；`None` 表示内建数组/切片下标。
    fn index_read(&self, place: ExprId) -> Option<u32> {
        match &self.owner.expressions[place.index()].kind {
            hir::ExprKind::Index {
                read: Some(dispatch),
                ..
            } => Some(*dispatch),
            _ => None,
        }
    }

    fn emit_local_static(
        &mut self,
        local: hir::LocalId,
        definition: hir::DefId,
    ) -> Result<(), Diagnostic> {
        let dest = self.hir_to_gir[local.index()];
        let ptr = self.intrinsic_temp(
            IntrinsicOp::StaticRef(definition),
            Vec::new(),
            Vec::new(),
            self.owner.locals[local.index()].ty,
        );
        self.assign(
            Place::local(dest),
            Rvalue::Use(Operand::Copy(Place::local(ptr))),
        );
        Ok(())
    }

    pub(super) fn emit_if(
        &mut self,
        id: ExprId,
        condition: ExprId,
        then_value: ExprId,
        else_value: Option<ExprId>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let Some(cond) = self.emit_expr(condition)? else {
            return Ok(None);
        };
        let dest = self.temp(self.expr_ty(id));
        let then_block = self.fresh(false);
        let else_block = self.fresh(false);
        let join = self.fresh(false);
        self.terminate(Terminator::SwitchInt {
            value: copy_of(cond),
            targets: vec![(1, then_block)],
            otherwise: else_block,
        });
        self.switch_to(then_block);
        if let Some(value) = self.emit_expr(then_value)? {
            self.assign_copy(Place::local(dest), value);
            self.goto(join);
        }
        self.switch_to(else_block);
        if let Some(else_value) = else_value {
            if let Some(value) = self.emit_expr(else_value)? {
                self.assign_copy(Place::local(dest), value);
                self.goto(join);
            }
        } else {
            self.assign_unit(dest);
            self.goto(join);
        }
        self.switch_to(join);
        if self.terminated() {
            return Ok(None);
        }
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_short_circuit(
        &mut self,
        id: ExprId,
        left: ExprId,
        right: ExprId,
        is_and: bool,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let Some(left) = self.emit_expr(left)? else {
            return Ok(None);
        };
        let dest = self.temp(self.expr_ty(id));
        self.assign_copy(Place::local(dest), left);
        let rhs = self.fresh(false);
        let join = self.fresh(false);
        let (taken, otherwise) = if is_and { (rhs, join) } else { (join, rhs) };
        self.terminate(Terminator::SwitchInt {
            value: copy_of(left),
            targets: vec![(1, taken)],
            otherwise,
        });
        self.switch_to(rhs);
        if let Some(right) = self.emit_expr(right)? {
            self.assign_copy(Place::local(dest), right);
            self.goto(join);
        }
        self.switch_to(join);
        if self.terminated() {
            return Ok(None);
        }
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_loop(
        &mut self,
        id: ExprId,
        condition: Option<ExprId>,
        body: ExprId,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let header = self.fresh(false);
        let exit = self.fresh(false);
        let dest = if self.expr_ty(id) == self.primitives.never {
            None
        } else {
            Some(self.temp(self.expr_ty(id)))
        };
        let scope = self.owner.expressions[body.index()].scope;
        self.goto(header);
        self.switch_to(header);
        if let Some(condition) = condition {
            let Some(cond) = self.emit_expr(condition)? else {
                return Ok(None);
            };
            let body_block = self.fresh(false);
            self.terminate(Terminator::SwitchInt {
                value: copy_of(cond),
                targets: vec![(1, body_block)],
                otherwise: exit,
            });
            self.switch_to(body_block);
        }
        self.loops.push(LoopFrame {
            scope,
            header,
            exit,
            value: dest,
        });
        let _ = self.emit_expr(body)?;
        if !self.terminated() {
            self.goto(header);
        }
        self.loops.pop();
        self.switch_to(exit);
        if let Some(dest) = dest {
            self.set_value(id, dest);
            Ok(Some(dest))
        } else {
            Ok(None)
        }
    }

    pub(super) fn emit_for(
        &mut self,
        id: ExprId,
        pattern: hir::PatternId,
        value: ExprId,
        body: ExprId,
        into_iter: Option<u32>,
        next: Option<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        if let Some(result) = self.try_emit_range_for(id, pattern, value, body)? {
            return Ok(result);
        }
        let Some(iterable) = self.emit_expr(value)? else {
            return Ok(None);
        };
        if into_iter.is_none() && next.is_none() {
            return self.emit_slice_for(id, pattern, value, body);
        }
        let iter = if let Some(dispatch) = into_iter {
            let receiver = self.dispatch_receiver(dispatch, iterable, Some(value));
            self.call_dispatch(value, dispatch, vec![receiver])?
        } else {
            iterable
        };
        let header = self.fresh(false);
        let exit = self.fresh(false);
        let scope = self.owner.expressions[body.index()].scope;
        self.goto(header);
        self.switch_to(header);
        let item = if let Some(dispatch) = next {
            let receiver = self.dispatch_receiver(dispatch, iter, None);
            self.call_dispatch(value, dispatch, vec![receiver])?
        } else {
            iter
        };
        let disc = self.temp(self.int_ty());
        self.assign(Place::local(disc), Rvalue::Discriminant(Place::local(item)));
        let body_block = self.fresh(false);
        self.terminate(Terminator::SwitchInt {
            value: copy_of(disc),
            targets: vec![(0, body_block), (1, exit)],
            otherwise: exit,
        });
        self.switch_to(body_block);
        let payload = self.project(Place::local(item), Projection::Downcast(0));
        let payload = self.project(
            payload,
            Projection::Field {
                index: 0,
                field_ty: self.owner.patterns[pattern.index()].ty,
                access: Access::Normal,
            },
        );
        self.bind_pattern(payload, pattern)?;
        self.loops.push(LoopFrame {
            scope,
            header,
            exit,
            value: None,
        });
        let _ = self.emit_expr(body)?;
        if !self.terminated() {
            self.goto(header);
        }
        self.loops.pop();
        self.switch_to(exit);
        let dest = self.temp(self.expr_ty(id));
        self.assign_unit(dest);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    fn try_emit_range_for(
        &mut self,
        id: ExprId,
        pattern: hir::PatternId,
        value: ExprId,
        body: ExprId,
    ) -> Result<Option<Option<LocalId>>, Diagnostic> {
        let hir::PatternKind::Bind(local) = self.owner.patterns[pattern.index()].kind else {
            return Ok(None);
        };
        let hir::ExprKind::Range { start, end } = self.owner.expressions[value.index()].kind else {
            return Ok(None);
        };
        let Some(start_local) = self.emit_expr(start)? else {
            return Ok(Some(None));
        };
        let Some(end_local) = self.emit_expr(end)? else {
            return Ok(Some(None));
        };
        let iv = self.hir_to_gir[local.index()];
        self.assign_copy(Place::local(iv), start_local);
        let header = self.fresh(false);
        let exit = self.fresh(false);
        let body_block = self.fresh(false);
        self.goto(header);
        self.switch_to(header);
        let cond = self.temp(self.primitives.bool_ty);
        self.assign(
            Place::local(cond),
            Rvalue::Compare {
                op: CompareOp::Lt,
                left: copy_of(iv),
                right: copy_of(end_local),
            },
        );
        self.terminate(Terminator::SwitchInt {
            value: copy_of(cond),
            targets: vec![(1, body_block)],
            otherwise: exit,
        });
        self.switch_to(body_block);
        self.loops.push(LoopFrame {
            scope: self.owner.expressions[body.index()].scope,
            header,
            exit,
            value: None,
        });
        let _ = self.emit_expr(body)?;
        if !self.terminated() {
            let one =
                self.const_operand(self.owner.locals[local.index()].ty, ConstValue::Integer(1));
            self.assign(
                Place::local(iv),
                Rvalue::BinaryOp {
                    op: BinaryOp::Add,
                    left: copy_of(iv),
                    right: one,
                },
            );
            self.goto(header);
        }
        self.loops.pop();
        self.switch_to(exit);
        let dest = self.temp(self.expr_ty(id));
        self.assign_unit(dest);
        self.set_value(id, dest);
        Ok(Some(Some(dest)))
    }

    /// 数组与切片的原生 `for`：按索引推进，不经过用户迭代器协议。
    fn emit_slice_for(
        &mut self,
        id: ExprId,
        pattern: hir::PatternId,
        value: ExprId,
        body: ExprId,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let place = self.emit_place(value)?;
        let int = self.int_ty();
        let length = self.temp(int);
        self.assign(Place::local(length), Rvalue::Len(place));
        let index = self.temp(int);
        let zero = self.const_operand(int, ConstValue::Integer(0));
        self.assign(Place::local(index), Rvalue::Use(zero));
        let header = self.fresh(false);
        let exit = self.fresh(false);
        let body_block = self.fresh(false);
        self.goto(header);
        self.switch_to(header);
        let cond = self.temp(self.primitives.bool_ty);
        self.assign(
            Place::local(cond),
            Rvalue::Compare {
                op: CompareOp::Lt,
                left: copy_of(index),
                right: copy_of(length),
            },
        );
        self.terminate(Terminator::SwitchInt {
            value: copy_of(cond),
            targets: vec![(1, body_block)],
            otherwise: exit,
        });
        self.switch_to(body_block);
        let base = if matches!(
            self.module.types.get(self.place_ty(place).index()),
            Some(hir::Type::Ref(_))
        ) {
            self.project(place, Projection::Deref)
        } else {
            place
        };
        let element = self.project(base, Projection::Index(index));
        self.bind_pattern(element, pattern)?;
        self.loops.push(LoopFrame {
            scope: self.owner.expressions[body.index()].scope,
            header,
            exit,
            value: None,
        });
        let _ = self.emit_expr(body)?;
        if !self.terminated() {
            let one = self.const_operand(int, ConstValue::Integer(1));
            self.assign(
                Place::local(index),
                Rvalue::BinaryOp {
                    op: BinaryOp::Add,
                    left: copy_of(index),
                    right: one,
                },
            );
            self.goto(header);
        }
        self.loops.pop();
        self.switch_to(exit);
        let dest = self.temp(self.expr_ty(id));
        self.assign_unit(dest);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_try(
        &mut self,
        id: ExprId,
        body: ExprId,
        from_value: Option<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let dest = self.temp(self.expr_ty(id));
        let exit = self.fresh(false);
        self.tries.push(TryFrame {
            scope: self.expr_scope(id),
            exit,
            value: Some(dest),
        });
        let Some(value) = self.emit_expr(body)? else {
            self.tries.pop();
            self.switch_to(exit);
            self.set_value(id, dest);
            return Ok(Some(dest));
        };
        if let Some(dispatch) = from_value {
            let argument = self.dispatch_receiver(dispatch, value, None);
            let wrapped = self.call_dispatch(id, dispatch, vec![argument])?;
            self.assign_copy(Place::local(dest), wrapped);
        } else {
            self.assign_copy(Place::local(dest), value);
        }
        self.goto(exit);
        self.tries.pop();
        self.switch_to(exit);
        self.set_value(id, dest);
        Ok(Some(dest))
    }
    pub(super) fn emit_try_exit(
        &mut self,
        id: ExprId,
        value: ExprId,
        branch: Option<u32>,
        from_error: Option<u32>,
        target: hir::ExitTarget,
        plan: u32,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let Some(operand) = self.emit_expr(value)? else {
            return Ok(None);
        };
        // 用户 `Try` 先经 `branch` 取得 `Result[Value, Error]`；内建 Option/Result 直接检查自身。
        let source = match branch {
            Some(dispatch) => {
                let argument = self.dispatch_receiver(dispatch, operand, None);
                self.call_dispatch(id, dispatch, vec![argument])?
            }
            None => operand,
        };
        let disc = self.temp(self.int_ty());
        self.assign(
            Place::local(disc),
            Rvalue::Discriminant(Place::local(source)),
        );
        let ok = self.fresh(false);
        let err = self.fresh(false);
        self.terminate(Terminator::SwitchInt {
            value: copy_of(disc),
            targets: vec![(0, ok), (1, err)],
            otherwise: err,
        });
        self.switch_to(err);
        let error_ty = match from_error {
            Some(dispatch) => self.dispatch_parameters(dispatch).first().copied(),
            None => match self
                .module
                .types
                .get(self.locals[source.index()].ty.index())
            {
                Some(hir::Type::Result(_, error)) => Some(*error),
                _ => Some(self.primitives.unit),
            },
        };
        let error_ty = error_ty.ok_or_else(|| {
            Diagnostic::error(
                crate::diagnostics::DiagnosticCode::LirInvariant,
                "问号运算符缺少错误类型",
                None,
            )
        })?;
        let error_place = self.project(Place::local(source), Projection::Downcast(1));
        let error_place = self.project(
            error_place,
            Projection::Field {
                index: 0,
                field_ty: error_ty,
                access: Access::Normal,
            },
        );
        let error = self.temp(error_ty);
        self.copy_value(Place::local(error), error_place, error_ty);
        let outgoing = match from_error {
            Some(dispatch) => {
                let argument = self.dispatch_receiver(dispatch, error, None);
                self.call_dispatch(id, dispatch, vec![argument])?
            }
            None => error,
        };
        self.emit_exit_value(target, Some(outgoing), plan)?;
        self.switch_to(ok);
        let value_ty = self.expr_ty(id);
        let value_place = self.project(Place::local(source), Projection::Downcast(0));
        let value_place = self.project(
            value_place,
            Projection::Field {
                index: 0,
                field_ty: value_ty,
                access: Access::Normal,
            },
        );
        let dest = self.temp(value_ty);
        self.copy_value(Place::local(dest), value_place, value_ty);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_exit(
        &mut self,
        id: ExprId,
        target: hir::ExitTarget,
        value: Option<ExprId>,
        plan: u32,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let value = match value {
            Some(value) => self.emit_expr(value)?,
            None => None,
        };
        if self.terminated() {
            return Ok(None);
        }
        self.emit_exit_value(target, value, plan)?;
        let dest = self.temp(self.expr_ty(id));
        self.set_value(id, dest);
        Ok(None)
    }

    fn emit_exit_value(
        &mut self,
        target: hir::ExitTarget,
        value: Option<LocalId>,
        plan: u32,
    ) -> Result<(), Diagnostic> {
        let dest = match target {
            hir::ExitTarget::Return => {
                if let Some(value) = value {
                    self.assign_copy(Place::local(self.return_local), value);
                }
                Some(self.return_block)
            }
            hir::ExitTarget::Break(scope) => {
                if let Some(frame) = self.loops.iter().rev().find(|frame| frame.scope == scope)
                    && let (Some(slot), Some(value)) = (frame.value, value)
                {
                    self.assign_copy(Place::local(slot), value);
                }
                self.loops
                    .iter()
                    .rev()
                    .find(|frame| frame.scope == scope)
                    .map(|frame| frame.exit)
            }
            hir::ExitTarget::Continue(scope) => self
                .loops
                .iter()
                .rev()
                .find(|frame| frame.scope == scope)
                .map(|frame| frame.header),
            hir::ExitTarget::Try(scope) => {
                if let Some(frame) = self.tries.iter().rev().find(|frame| frame.scope == scope)
                    && let (Some(slot), Some(value)) = (frame.value, value)
                {
                    self.assign_copy(Place::local(slot), value);
                }
                self.tries
                    .iter()
                    .rev()
                    .find(|frame| frame.scope == scope)
                    .map(|frame| frame.exit)
            }
        };
        let cleanup = self.intern_cleanup(plan, CleanupChain::Normal, dest)?;
        self.goto(cleanup);
        Ok(())
    }
}

fn compound_assign(
    operation: crate::frontend::ast::AssignOp,
    left: Operand,
    right: Operand,
) -> Rvalue {
    let op = match operation {
        crate::frontend::ast::AssignOp::Add => BinaryOp::Add,
        crate::frontend::ast::AssignOp::Sub => BinaryOp::Sub,
        crate::frontend::ast::AssignOp::Mul => BinaryOp::Mul,
        crate::frontend::ast::AssignOp::Div => BinaryOp::Div,
        crate::frontend::ast::AssignOp::Rem => BinaryOp::Rem,
        crate::frontend::ast::AssignOp::BitAnd => BinaryOp::BitAnd,
        crate::frontend::ast::AssignOp::BitOr => BinaryOp::BitOr,
        crate::frontend::ast::AssignOp::BitXor => BinaryOp::BitXor,
        crate::frontend::ast::AssignOp::Shl => BinaryOp::Shl,
        crate::frontend::ast::AssignOp::Shr => BinaryOp::Shr,
        crate::frontend::ast::AssignOp::Assign => {
            return Rvalue::Use(right);
        }
    };
    Rvalue::BinaryOp { op, left, right }
}
