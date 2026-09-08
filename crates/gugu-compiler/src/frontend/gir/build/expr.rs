use super::*;
use crate::frontend::ast::{BinOp, UnOp};

impl Builder<'_> {
    pub(super) fn emit_expr(&mut self, id: ExprId) -> Result<Option<LocalId>, Diagnostic> {
        if self.terminated() {
            return Ok(None);
        }
        if let Some(local) = self.expression_locals[id.index()] {
            return Ok(Some(local));
        }
        if self.expression_places[id.index()].is_some() {
            return self.value_of(id);
        }
        let kind = self.owner.expressions[id.index()].kind.clone();
        match kind {
            hir::ExprKind::Resolved(res) => self.emit_resolved(id, res)?,
            hir::ExprKind::Literal(literal) => self.emit_literal(id, literal)?,
            hir::ExprKind::Tuple(range) => self.emit_aggregate(id, AggregateKind::Tuple, range)?,
            hir::ExprKind::Array(range) => {
                self.emit_aggregate(id, AggregateKind::Array(self.expr_ty(id)), range)?;
            }
            hir::ExprKind::Repeat { value, count } => self.emit_repeat(id, value, count)?,
            hir::ExprKind::Construct { variant, fields } => {
                self.emit_construct(id, variant, fields)?
            }
            hir::ExprKind::Block {
                statements,
                tail,
                end_plan,
            } => return self.emit_block(id, statements, tail, end_plan),
            hir::ExprKind::If {
                condition,
                then_value,
                else_value,
            } => return self.emit_if(id, condition, then_value, else_value),
            hir::ExprKind::Match { value, arms } => return self.emit_match(id, value, arms),
            hir::ExprKind::Loop { body } => return self.emit_loop(id, None, body),
            hir::ExprKind::While { condition, body } => {
                return self.emit_loop(id, Some(condition), body);
            }
            hir::ExprKind::For {
                pattern,
                value,
                body,
                into_iter,
                next,
            } => return self.emit_for(id, pattern, value, body, into_iter, next),
            hir::ExprKind::Try { body, from_value } => return self.emit_try(id, body, from_value),
            hir::ExprKind::TryExit {
                value,
                branch,
                from_error,
                target,
                plan,
                ..
            } => return self.emit_try_exit(id, value, branch, from_error, target, plan),
            hir::ExprKind::Select { arms } => return self.emit_select(id, arms),
            hir::ExprKind::Closure { definition } => self.emit_closure(id, definition, false)?,
            hir::ExprKind::Spawn { definition } => self.emit_closure(id, definition, true)?,
            hir::ExprKind::Call {
                target,
                receiver,
                arguments,
            } => return self.emit_call_expr(id, target, receiver, arguments, false),
            hir::ExprKind::SpawnCall {
                target,
                receiver,
                arguments,
            } => return self.emit_call_expr(id, target, receiver, arguments, true),
            hir::ExprKind::Intrinsic {
                operation,
                arguments,
                types,
                field,
            } => return self.emit_intrinsic(id, operation, arguments, types, field),
            hir::ExprKind::Field { base, index } => self.emit_field(id, base, index)?,
            hir::ExprKind::Index {
                base,
                index,
                read,
                write,
            } => self.emit_index(id, base, index, read, write)?,
            hir::ExprKind::Slice { base, start, end } => self.emit_slice(id, base, start, end)?,
            hir::ExprKind::Unary { operation, value } => self.emit_unary(id, operation, value)?,
            hir::ExprKind::Binary {
                operation,
                left,
                right,
                dispatch,
            } => return self.emit_binary(id, operation, left, right, dispatch),
            hir::ExprKind::Range { start, end } => self.emit_range(id, start, end)?,
            hir::ExprKind::Comptime { value } => {
                return self.emit_expr(value).map(|local| {
                    if let Some(local) = local {
                        self.set_value(id, local);
                    }
                    local
                });
            }
            hir::ExprKind::Assembly(index) => self.emit_asm(id, index)?,
            hir::ExprKind::Exit {
                target,
                value,
                plan,
                ..
            } => {
                return self.emit_exit(id, target, value, plan);
            }
            hir::ExprKind::String { parts } => self.emit_string(id, parts)?,
            hir::ExprKind::LetCondition { pattern, value } => {
                return self.emit_let_condition(id, pattern, value);
            }
        }
        self.apply_adjustments(id)?;
        self.value_of(id)
    }

    fn emit_resolved(&mut self, id: ExprId, res: hir::Res) -> Result<(), Diagnostic> {
        match res {
            hir::Res::Local(local) => {
                self.set_place(id, Place::local(self.hir_to_gir[local.index()]));
            }
            hir::Res::Def(definition) => self.emit_definition(id, definition)?,
            hir::Res::Primitive(ty) => {
                let local = self.intrinsic_temp(
                    IntrinsicOp::TypeId,
                    Vec::new(),
                    vec![ty],
                    self.expr_ty(id),
                );
                self.set_value(id, local);
            }
            hir::Res::Builtin(builtin) => self.emit_builtin_value(id, builtin)?,
            hir::Res::Associated { definition, .. } => self.emit_definition(id, definition)?,
        }
        Ok(())
    }

    fn emit_definition(&mut self, id: ExprId, definition: hir::DefId) -> Result<(), Diagnostic> {
        let kind = self.module.definitions[definition.index()].kind;
        if matches!(
            kind,
            hir::DefinitionKind::Static | hir::DefinitionKind::LocalStatic
        ) {
            let local = self.intrinsic_temp(
                IntrinsicOp::StaticRef(definition),
                Vec::new(),
                Vec::new(),
                self.expr_ty(id),
            );
            let place = self.project(Place::local(local), Projection::Deref);
            self.set_place(id, place);
            return Ok(());
        }
        if matches!(
            kind,
            hir::DefinitionKind::Function
                | hir::DefinitionKind::Closure
                | hir::DefinitionKind::Async
        ) {
            let local = self.temp(self.expr_ty(id));
            self.assign(
                Place::local(local),
                Rvalue::FunctionValue(MonoCandidate {
                    definition,
                    signature: self.expr_ty(id),
                }),
            );
            self.set_value(id, local);
            return Ok(());
        }
        let local = self.temp(self.expr_ty(id));
        let constant = self.intern_const(self.expr_ty(id), ConstValue::Definition(definition));
        self.assign(
            Place::local(local),
            Rvalue::Use(Operand::Constant(constant)),
        );
        self.set_value(id, local);
        Ok(())
    }

    pub(super) fn emit_builtin_value(
        &mut self,
        id: ExprId,
        builtin: hir::Builtin,
    ) -> Result<(), Diagnostic> {
        match builtin {
            hir::Builtin::None => {
                let local = self.temp(self.expr_ty(id));
                self.assign(
                    Place::local(local),
                    Rvalue::Aggregate {
                        kind: AggregateKind::Adt {
                            ty: self.expr_ty(id),
                            variant: 0,
                        },
                        operands: Vec::new(),
                    },
                );
                self.set_value(id, local);
            }
            _ => {
                let local = self.temp(self.expr_ty(id));
                self.assign_unit(local);
                self.set_value(id, local);
            }
        }
        Ok(())
    }

    fn emit_literal(&mut self, id: ExprId, literal: hir::Literal) -> Result<(), Diagnostic> {
        let value = match literal {
            hir::Literal::Integer(value) => ConstValue::Integer(value),
            hir::Literal::Float(value) => ConstValue::Float(value),
            hir::Literal::Bool(value) => ConstValue::Bool(value),
            hir::Literal::Char(value) => ConstValue::Char(value),
            hir::Literal::String(value) => ConstValue::String(value),
            hir::Literal::Bytes(value) => ConstValue::Bytes(value),
            hir::Literal::CString(value) => ConstValue::CString(value),
        };
        let local = self.temp(self.expr_ty(id));
        let operand = self.const_operand(self.expr_ty(id), value);
        self.assign(Place::local(local), Rvalue::Use(operand));
        self.set_value(id, local);
        Ok(())
    }

    fn emit_aggregate(
        &mut self,
        id: ExprId,
        kind: AggregateKind,
        range: Range<u32>,
    ) -> Result<(), Diagnostic> {
        let mut operands = Vec::new();
        for child in expr_range(self.owner, &range) {
            let Some(local) = self.emit_expr(child)? else {
                return Ok(());
            };
            operands.push(copy_of(local));
        }
        let local = self.temp(self.expr_ty(id));
        self.assign(Place::local(local), Rvalue::Aggregate { kind, operands });
        self.set_value(id, local);
        Ok(())
    }

    fn emit_repeat(&mut self, id: ExprId, value: ExprId, count: u64) -> Result<(), Diagnostic> {
        let Some(local) = self.emit_expr(value)? else {
            return Ok(());
        };
        let dest = self.temp(self.expr_ty(id));
        self.assign(
            Place::local(dest),
            Rvalue::Repeat {
                operand: copy_of(local),
                count,
            },
        );
        self.set_value(id, dest);
        Ok(())
    }

    fn emit_construct(
        &mut self,
        id: ExprId,
        variant: u32,
        fields: Range<u32>,
    ) -> Result<(), Diagnostic> {
        let mut operands = Vec::new();
        for field in &self.owner.fields[fields.start as usize..fields.end as usize] {
            let Some(local) = self.emit_expr(field.value)? else {
                return Ok(());
            };
            operands.push(copy_of(local));
        }
        let local = self.temp(self.expr_ty(id));
        self.assign(
            Place::local(local),
            Rvalue::Aggregate {
                kind: AggregateKind::Adt {
                    ty: self.expr_ty(id),
                    variant,
                },
                operands,
            },
        );
        self.set_value(id, local);
        Ok(())
    }

    fn emit_unary(&mut self, id: ExprId, operation: UnOp, value: ExprId) -> Result<(), Diagnostic> {
        let Some(local) = self.emit_expr(value)? else {
            return Ok(());
        };
        match operation {
            UnOp::Ref => {
                let dest = self.temp(self.expr_ty(id));
                let place = self.expression_places[value.index()].unwrap_or(Place::local(local));
                self.assign(Place::local(dest), Rvalue::Ref(place));
                self.set_value(id, dest);
            }
            UnOp::Deref => {
                let place = self.project(Place::local(local), Projection::Deref);
                self.set_place(id, place);
            }
            UnOp::Neg => self.assign_unary(id, UnaryOp::Neg, local),
            UnOp::Not | UnOp::BitNot => self.assign_unary(id, UnaryOp::Not, local),
        }
        Ok(())
    }

    fn assign_unary(&mut self, id: ExprId, op: UnaryOp, local: LocalId) {
        let dest = self.temp(self.expr_ty(id));
        self.assign(
            Place::local(dest),
            Rvalue::UnaryOp {
                op,
                operand: copy_of(local),
            },
        );
        self.set_value(id, dest);
    }

    fn emit_binary(
        &mut self,
        id: ExprId,
        operation: BinOp,
        left: ExprId,
        right: ExprId,
        dispatch: Option<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        if matches!(operation, BinOp::And | BinOp::Or) {
            return self.emit_short_circuit(id, left, right, operation == BinOp::And);
        }
        let Some(left_local) = self.emit_expr(left)? else {
            return Ok(None);
        };
        let Some(right_local) = self.emit_expr(right)? else {
            return Ok(None);
        };
        if let Some(dispatch) = dispatch {
            return self.emit_dispatch_op(id, dispatch, left_local, right_local);
        }
        self.emit_check_ops(id)?;
        let dest = self.temp(self.expr_ty(id));
        let rvalue = binary_rvalue(operation, copy_of(left_local), copy_of(right_local));
        self.assign(Place::local(dest), rvalue);
        self.set_value(id, dest);
        self.apply_adjustments(id)?;
        self.value_of(id)
    }

    fn emit_dispatch_op(
        &mut self,
        id: ExprId,
        dispatch: u32,
        left: LocalId,
        right: LocalId,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let dest = self.temp(self.expr_ty(id));
        let normal = self.fresh(false);
        let unwind = self.intern_plan(self.current_unwind(id), CleanupChain::Unwind)?;
        self.terminate(Terminator::Call {
            callee: Callee::Dispatch(dispatch),
            args: vec![copy_of(left), copy_of(right)],
            destination: Place::local(dest),
            normal,
            unwind: Some(unwind),
            call_kind: CallKind::Managed,
            site: crate::frontend::mono::instantiate::CallSite::Dispatch(dispatch),
        });
        self.switch_to(normal);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    fn emit_range(&mut self, id: ExprId, start: ExprId, end: ExprId) -> Result<(), Diagnostic> {
        let Some(start) = self.emit_expr(start)? else {
            return Ok(());
        };
        let Some(end) = self.emit_expr(end)? else {
            return Ok(());
        };
        let dest = self.temp(self.expr_ty(id));
        self.assign(
            Place::local(dest),
            Rvalue::Aggregate {
                kind: AggregateKind::Range,
                operands: vec![copy_of(start), copy_of(end)],
            },
        );
        self.set_value(id, dest);
        Ok(())
    }

    fn emit_string(&mut self, id: ExprId, parts: Range<u32>) -> Result<(), Diagnostic> {
        let mut operands = Vec::new();
        for part in &self.owner.string_parts[parts.start as usize..parts.end as usize] {
            if let hir::StringPart::Value { expression, .. } = part {
                let Some(local) = self.emit_expr(*expression)? else {
                    return Ok(());
                };
                operands.push(copy_of(local));
            }
        }
        let dest = self.temp(self.expr_ty(id));
        self.assign(
            Place::local(dest),
            Rvalue::Intrinsic {
                op: IntrinsicOp::Format {
                    parts: parts.clone(),
                },
                operands,
                types: Vec::new(),
            },
        );
        self.set_value(id, dest);
        Ok(())
    }

    fn emit_asm(&mut self, id: ExprId, index: u32) -> Result<(), Diagnostic> {
        let mut operands = Vec::new();
        if let Some(assembly) = self.owner.assembly.get(index as usize) {
            for operand in &assembly.operands {
                let Some(local) = self.emit_expr(operand.value)? else {
                    return Ok(());
                };
                operands.push(copy_of(local));
            }
        }
        let dest = self.temp(self.expr_ty(id));
        self.assign(
            Place::local(dest),
            Rvalue::Intrinsic {
                op: IntrinsicOp::Asm(index),
                operands,
                types: Vec::new(),
            },
        );
        self.set_value(id, dest);
        Ok(())
    }

    fn emit_closure(
        &mut self,
        id: ExprId,
        definition: hir::DefId,
        spawn: bool,
    ) -> Result<(), Diagnostic> {
        let dest = self.temp(self.expr_ty(id));
        if spawn {
            self.assign(
                Place::local(dest),
                Rvalue::Intrinsic {
                    op: IntrinsicOp::Spawn(SpawnTarget::Body(definition)),
                    operands: Vec::new(),
                    types: Vec::new(),
                },
            );
            self.flags |= BodyFlags::SUSPEND;
        } else {
            self.assign(
                Place::local(dest),
                Rvalue::Aggregate {
                    kind: AggregateKind::Closure(definition),
                    operands: Vec::new(),
                },
            );
        }
        self.set_value(id, dest);
        Ok(())
    }

    fn apply_adjustments(&mut self, id: ExprId) -> Result<(), Diagnostic> {
        let adjustments = self.owner.expressions[id.index()].adjustments.clone();
        let Some(mut local) = self.expression_locals[id.index()] else {
            if self.expression_places[id.index()].is_some() {
                return Ok(());
            }
            return Ok(());
        };
        for adjustment in
            &self.owner.adjustments[adjustments.start as usize..adjustments.end as usize]
        {
            local = self.apply_adjustment(id, local, adjustment)?;
        }
        Ok(())
    }

    fn apply_adjustment(
        &mut self,
        id: ExprId,
        local: LocalId,
        adjustment: &hir::Adjustment,
    ) -> Result<LocalId, Diagnostic> {
        match adjustment {
            hir::Adjustment::Dereference => {
                let place = self.project(Place::local(local), Projection::Deref);
                self.set_place(id, place);
                self.require_value(id)
            }
            hir::Adjustment::ArrayToSlice(ty) => self.cast(id, local, CastKind::ArrayToSlice, *ty),
            hir::Adjustment::NeverTo(ty) => self.cast(id, local, CastKind::NeverTo, *ty),
            hir::Adjustment::Erase(ty) => {
                let dest = self.temp(*ty);
                self.assign(
                    Place::local(dest),
                    Rvalue::DynErase {
                        operand: copy_of(local),
                        ty: *ty,
                    },
                );
                self.set_value(id, dest);
                Ok(dest)
            }
            hir::Adjustment::Opaque(ty) => self.cast(id, local, CastKind::Opaque, *ty),
            hir::Adjustment::Instantiate(ty) => self.cast(id, local, CastKind::Instantiate, *ty),
        }
    }

    fn cast(
        &mut self,
        id: ExprId,
        local: LocalId,
        kind: CastKind,
        ty: TypeId,
    ) -> Result<LocalId, Diagnostic> {
        let dest = self.temp(ty);
        self.assign(
            Place::local(dest),
            Rvalue::Cast {
                kind,
                operand: copy_of(local),
                ty,
            },
        );
        self.set_value(id, dest);
        Ok(dest)
    }
}

fn binary_rvalue(operation: BinOp, left: Operand, right: Operand) -> Rvalue {
    match operation {
        BinOp::Add => Rvalue::BinaryOp {
            op: BinaryOp::Add,
            left,
            right,
        },
        BinOp::Sub => Rvalue::BinaryOp {
            op: BinaryOp::Sub,
            left,
            right,
        },
        BinOp::Mul => Rvalue::BinaryOp {
            op: BinaryOp::Mul,
            left,
            right,
        },
        BinOp::Div => Rvalue::BinaryOp {
            op: BinaryOp::Div,
            left,
            right,
        },
        BinOp::Rem => Rvalue::BinaryOp {
            op: BinaryOp::Rem,
            left,
            right,
        },
        BinOp::BitAnd => Rvalue::BinaryOp {
            op: BinaryOp::BitAnd,
            left,
            right,
        },
        BinOp::BitOr => Rvalue::BinaryOp {
            op: BinaryOp::BitOr,
            left,
            right,
        },
        BinOp::BitXor => Rvalue::BinaryOp {
            op: BinaryOp::BitXor,
            left,
            right,
        },
        BinOp::Shl => Rvalue::BinaryOp {
            op: BinaryOp::Shl,
            left,
            right,
        },
        BinOp::Shr => Rvalue::BinaryOp {
            op: BinaryOp::Shr,
            left,
            right,
        },
        BinOp::Eq => Rvalue::Compare {
            op: CompareOp::Eq,
            left,
            right,
        },
        BinOp::Ne => Rvalue::Compare {
            op: CompareOp::Ne,
            left,
            right,
        },
        BinOp::Lt => Rvalue::Compare {
            op: CompareOp::Lt,
            left,
            right,
        },
        BinOp::Le => Rvalue::Compare {
            op: CompareOp::Le,
            left,
            right,
        },
        BinOp::Gt => Rvalue::Compare {
            op: CompareOp::Gt,
            left,
            right,
        },
        BinOp::Ge => Rvalue::Compare {
            op: CompareOp::Ge,
            left,
            right,
        },
        BinOp::And | BinOp::Or => unreachable!("短路运算已单独 lowering"),
    }
}
