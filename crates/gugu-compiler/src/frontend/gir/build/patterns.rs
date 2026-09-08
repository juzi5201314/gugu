use super::*;

impl Builder<'_> {
    pub(super) fn emit_match(
        &mut self,
        id: ExprId,
        value: ExprId,
        arms: Range<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let Some(scrutinee) = self.emit_expr(value)? else {
            return Ok(None);
        };
        let dest = self.temp(self.expr_ty(id));
        let join = self.fresh(false);
        let mut current_fail = self.current;
        let place = Place::local(scrutinee);
        let arm_ids: Vec<u32> = (arms.start..arms.end).collect();
        for (row, arm_index) in arm_ids.into_iter().enumerate() {
            let arm = self.owner.arms[arm_index as usize].clone();
            let ok = self.fresh(false);
            let fail = self.fresh(false);
            self.switch_to(current_fail);
            self.emit_pattern_test(place, arm.pattern, ok, fail)?;
            self.switch_to(ok);
            if let Some(guard) = arm.guard {
                let Some(cond) = self.emit_expr(guard)? else {
                    current_fail = fail;
                    continue;
                };
                let taken = self.fresh(false);
                self.terminate(Terminator::SwitchInt {
                    value: copy_of(cond),
                    targets: vec![(1, taken)],
                    otherwise: fail,
                });
                self.switch_to(taken);
            }
            self.bind_pattern(place, arm.pattern)?;
            self.match_leaves.push((self.current, row as u32));
            if let Some(value) = self.emit_expr(arm.body)? {
                self.copy_value(Place::local(dest), Place::local(value), self.expr_ty(id));
                self.goto(join);
            }
            current_fail = fail;
        }
        self.switch_to(current_fail);
        if !self.terminated() {
            self.terminate(Terminator::Unreachable);
        }
        self.switch_to(join);
        if self.terminated() {
            return Ok(None);
        }
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_let_condition(
        &mut self,
        id: ExprId,
        pattern: hir::PatternId,
        value: ExprId,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let Some(local) = self.emit_expr(value)? else {
            return Ok(None);
        };
        let dest = self.temp(self.primitives.bool_ty);
        let ok = self.fresh(false);
        let fail = self.fresh(false);
        let join = self.fresh(false);
        self.emit_pattern_test(Place::local(local), pattern, ok, fail)?;
        self.switch_to(ok);
        self.bind_pattern(Place::local(local), pattern)?;
        self.assign_bool(dest, true);
        self.goto(join);
        self.switch_to(fail);
        self.assign_bool(dest, false);
        self.goto(join);
        self.switch_to(join);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_pattern_test(
        &mut self,
        place: Place,
        pattern: hir::PatternId,
        ok: BlockId,
        fail: BlockId,
    ) -> Result<(), Diagnostic> {
        match &self.owner.patterns[pattern.index()].kind.clone() {
            hir::PatternKind::Wildcard | hir::PatternKind::Bind(_) => self.goto(ok),
            hir::PatternKind::At { pattern, .. } => {
                self.emit_pattern_test(place, *pattern, ok, fail)?;
            }
            hir::PatternKind::Ref(inner) => {
                let inner_place = self.project(place, Projection::Deref);
                self.emit_pattern_test(inner_place, *inner, ok, fail)?;
            }
            hir::PatternKind::Literal(literal) => {
                self.test_literal(place, literal, ok, fail)?;
            }
            hir::PatternKind::Range { start, end } => {
                self.test_range(place, start, end, ok, fail)?
            }
            hir::PatternKind::Tuple(range) => {
                self.test_tuple(place, range.clone(), ok, fail)?;
            }
            hir::PatternKind::Construct { variant, fields } => {
                self.test_construct(place, *variant, fields.clone(), ok, fail)?;
            }
            hir::PatternKind::Array {
                prefix,
                rest,
                has_rest,
                suffix,
            } => self.test_array(
                place,
                prefix.clone(),
                *rest,
                *has_rest,
                suffix.clone(),
                ok,
                fail,
            )?,
            hir::PatternKind::Or(range) => self.test_or(place, range.clone(), ok, fail)?,
        }
        Ok(())
    }

    fn test_literal(
        &mut self,
        place: Place,
        literal: &hir::Literal,
        ok: BlockId,
        fail: BlockId,
    ) -> Result<(), Diagnostic> {
        let ty = self.locals[place.local.index()].ty;
        let expected = self.const_operand(ty, literal_value(literal));
        let cond = self.temp(self.primitives.bool_ty);
        self.assign(
            Place::local(cond),
            Rvalue::Compare {
                op: CompareOp::Eq,
                left: Operand::Copy(place),
                right: expected,
            },
        );
        self.terminate(Terminator::SwitchInt {
            value: copy_of(cond),
            targets: vec![(1, ok)],
            otherwise: fail,
        });
        Ok(())
    }

    fn test_range(
        &mut self,
        place: Place,
        start: &hir::Literal,
        end: &hir::Literal,
        ok: BlockId,
        fail: BlockId,
    ) -> Result<(), Diagnostic> {
        let ty = self.locals[place.local.index()].ty;
        let ge = self.temp(self.primitives.bool_ty);
        let start = self.const_operand(ty, literal_value(start));
        self.assign(
            Place::local(ge),
            Rvalue::Compare {
                op: CompareOp::Ge,
                left: Operand::Copy(place),
                right: start,
            },
        );
        let mid = self.fresh(false);
        self.terminate(Terminator::SwitchInt {
            value: copy_of(ge),
            targets: vec![(1, mid)],
            otherwise: fail,
        });
        self.switch_to(mid);
        let lt = self.temp(self.primitives.bool_ty);
        let end = self.const_operand(ty, literal_value(end));
        self.assign(
            Place::local(lt),
            Rvalue::Compare {
                op: CompareOp::Lt,
                left: Operand::Copy(place),
                right: end,
            },
        );
        self.terminate(Terminator::SwitchInt {
            value: copy_of(lt),
            targets: vec![(1, ok)],
            otherwise: fail,
        });
        Ok(())
    }

    fn test_tuple(
        &mut self,
        place: Place,
        range: Range<u32>,
        ok: BlockId,
        fail: BlockId,
    ) -> Result<(), Diagnostic> {
        let patterns = self.owner.pattern_ids[range.start as usize..range.end as usize].to_vec();
        self.test_fields(
            place,
            &patterns,
            |index, ty| Projection::TupleField {
                index,
                field_ty: ty,
            },
            ok,
            fail,
        )
    }

    fn test_construct(
        &mut self,
        place: Place,
        variant: u32,
        fields: Range<u32>,
        ok: BlockId,
        fail: BlockId,
    ) -> Result<(), Diagnostic> {
        let disc = self.temp(self.int_ty());
        self.assign(Place::local(disc), Rvalue::Discriminant(place));
        let payload = self.fresh(false);
        self.terminate(Terminator::SwitchInt {
            value: copy_of(disc),
            targets: vec![(u128::from(variant), payload)],
            otherwise: fail,
        });
        self.switch_to(payload);
        let downcast = self.project(place, Projection::Downcast(variant));
        let patterns: Vec<_> = self.owner.pattern_fields
            [fields.start as usize..fields.end as usize]
            .iter()
            .map(|field| field.pattern)
            .collect();
        self.test_fields(
            downcast,
            &patterns,
            |index, ty| Projection::Field {
                index,
                field_ty: ty,
                access: Access::Normal,
            },
            ok,
            fail,
        )
    }

    fn test_fields(
        &mut self,
        place: Place,
        patterns: &[hir::PatternId],
        projection: impl Fn(u32, TypeId) -> Projection,
        ok: BlockId,
        fail: BlockId,
    ) -> Result<(), Diagnostic> {
        if patterns.is_empty() {
            self.goto(ok);
            return Ok(());
        }
        let mut next_ok = ok;
        for (offset, pattern) in patterns.iter().enumerate().rev() {
            let current = if offset == 0 {
                self.current
            } else {
                let block = self.fresh(false);
                self.switch_to(block);
                block
            };
            let ty = self.owner.patterns[pattern.index()].ty;
            let field = self.project(place, projection(offset as u32, ty));
            if offset == 0 {
                self.switch_to(current);
            }
            let after = if offset + 1 == patterns.len() {
                next_ok
            } else {
                // 下一字段的测试入口即刚才创建的块；正序时由下一轮写入。
                next_ok
            };
            if offset + 1 == patterns.len() {
                self.emit_pattern_test(field, *pattern, ok, fail)?;
            } else {
                let cont = self.fresh(false);
                self.emit_pattern_test(field, *pattern, cont, fail)?;
                next_ok = cont;
                let _ = after;
            }
        }
        Ok(())
    }

    fn test_array(
        &mut self,
        place: Place,
        prefix: Range<u32>,
        _rest: Option<hir::LocalId>,
        _has_rest: bool,
        suffix: Range<u32>,
        ok: BlockId,
        fail: BlockId,
    ) -> Result<(), Diagnostic> {
        let mut patterns =
            self.owner.pattern_ids[prefix.start as usize..prefix.end as usize].to_vec();
        patterns.extend(
            self.owner.pattern_ids[suffix.start as usize..suffix.end as usize]
                .iter()
                .copied(),
        );
        self.test_fields(
            place,
            &patterns,
            |index, _| Projection::ConstantIndex {
                offset: u64::from(index),
                from_end: false,
            },
            ok,
            fail,
        )
    }

    fn test_or(
        &mut self,
        place: Place,
        range: Range<u32>,
        ok: BlockId,
        fail: BlockId,
    ) -> Result<(), Diagnostic> {
        let alts = self.owner.pattern_ids[range.start as usize..range.end as usize].to_vec();
        let mut current = self.current;
        for (index, alt) in alts.iter().enumerate() {
            let next_fail = if index + 1 == alts.len() {
                fail
            } else {
                self.fresh(false)
            };
            self.switch_to(current);
            self.emit_pattern_test(place, *alt, ok, next_fail)?;
            current = next_fail;
        }
        Ok(())
    }

    pub(super) fn bind_pattern(
        &mut self,
        place: Place,
        pattern: hir::PatternId,
    ) -> Result<(), Diagnostic> {
        match &self.owner.patterns[pattern.index()].kind.clone() {
            hir::PatternKind::Wildcard
            | hir::PatternKind::Literal(_)
            | hir::PatternKind::Range { .. } => {}
            hir::PatternKind::Bind(local) => {
                let dest = self.hir_to_gir[local.index()];
                self.copy_value(
                    Place::local(dest),
                    place,
                    self.owner.locals[local.index()].ty,
                );
            }
            hir::PatternKind::At { local, pattern } => {
                let dest = self.hir_to_gir[local.index()];
                self.copy_value(
                    Place::local(dest),
                    place,
                    self.owner.locals[local.index()].ty,
                );
                self.bind_pattern(place, *pattern)?;
            }
            hir::PatternKind::Ref(inner) => {
                let inner_place = self.project(place, Projection::Deref);
                self.bind_pattern(inner_place, *inner)?;
            }
            hir::PatternKind::Tuple(range) => {
                for (index, inner) in self.owner.pattern_ids
                    [range.start as usize..range.end as usize]
                    .iter()
                    .enumerate()
                {
                    let ty = self.owner.patterns[inner.index()].ty;
                    let field = self.project(
                        place,
                        Projection::TupleField {
                            index: index as u32,
                            field_ty: ty,
                        },
                    );
                    self.bind_pattern(field, *inner)?;
                }
            }
            hir::PatternKind::Construct { variant, fields } => {
                let downcast = self.project(place, Projection::Downcast(*variant));
                for (index, field) in self.owner.pattern_fields
                    [fields.start as usize..fields.end as usize]
                    .iter()
                    .enumerate()
                {
                    let ty = self.owner.patterns[field.pattern.index()].ty;
                    let projected = self.project(
                        downcast,
                        Projection::Field {
                            index: index as u32,
                            field_ty: ty,
                            access: Access::Normal,
                        },
                    );
                    self.bind_pattern(projected, field.pattern)?;
                }
            }
            hir::PatternKind::Array {
                prefix,
                rest,
                suffix,
                ..
            } => self.bind_array(place, prefix.clone(), *rest, suffix.clone())?,
            hir::PatternKind::Or(range) => {
                if let Some(first) = self.owner.pattern_ids.get(range.start as usize) {
                    self.bind_pattern(place, *first)?;
                }
            }
        }
        Ok(())
    }

    fn bind_array(
        &mut self,
        place: Place,
        prefix: Range<u32>,
        rest: Option<hir::LocalId>,
        suffix: Range<u32>,
    ) -> Result<(), Diagnostic> {
        for (index, inner) in self.owner.pattern_ids[prefix.start as usize..prefix.end as usize]
            .iter()
            .enumerate()
        {
            let field = self.project(
                place,
                Projection::ConstantIndex {
                    offset: index as u64,
                    from_end: false,
                },
            );
            self.bind_pattern(field, *inner)?;
        }
        if let Some(local) = rest {
            let dest = self.hir_to_gir[local.index()];
            self.assign(Place::local(dest), Rvalue::Use(Operand::Copy(place)));
        }
        for (index, inner) in self.owner.pattern_ids[suffix.start as usize..suffix.end as usize]
            .iter()
            .enumerate()
        {
            let field = self.project(
                place,
                Projection::ConstantIndex {
                    offset: index as u64,
                    from_end: true,
                },
            );
            self.bind_pattern(field, *inner)?;
        }
        Ok(())
    }
}

fn literal_value(literal: &hir::Literal) -> ConstValue {
    match literal {
        hir::Literal::Integer(value) => ConstValue::Integer(*value),
        hir::Literal::Float(value) => ConstValue::Float(*value),
        hir::Literal::Bool(value) => ConstValue::Bool(*value),
        hir::Literal::Char(value) => ConstValue::Char(*value),
        hir::Literal::String(value) => ConstValue::String(value.clone()),
        hir::Literal::Bytes(value) => ConstValue::Bytes(value.clone()),
        hir::Literal::CString(value) => ConstValue::CString(value.clone()),
    }
}
