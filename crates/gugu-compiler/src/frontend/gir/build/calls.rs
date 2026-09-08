use super::*;
use crate::frontend::semantics::foreign::ForeignEffect;
use crate::frontend::semantics::model::MemoryIntrinsic;

impl Builder<'_> {
    pub(super) fn emit_call_expr(
        &mut self,
        id: ExprId,
        target: hir::CallTarget,
        receiver: Option<ExprId>,
        arguments: Range<u32>,
        spawn: bool,
    ) -> Result<Option<LocalId>, Diagnostic> {
        if let hir::CallTarget::Builtin(builtin) = target {
            return self.emit_builtin_call(id, builtin, receiver, arguments, spawn);
        }
        if let hir::CallTarget::Constructor { ty, variant } = target {
            return self.emit_constructor(id, ty, variant, arguments);
        }
        let (callee, mut args) = self.prepare_call(target, receiver, arguments)?;
        if self.terminated() {
            return Ok(None);
        }
        if spawn {
            let first = match &callee {
                Callee::Value(operand) => operand.clone(),
                _ => {
                    let local = self.temp(self.primitives.unit);
                    copy_of(local)
                }
            };
            args.insert(0, first);
            let dest = self.temp(self.expr_ty(id));
            self.assign(
                Place::local(dest),
                Rvalue::Intrinsic {
                    op: IntrinsicOp::Spawn(SpawnTarget::Callee(callee)),
                    operands: args,
                    types: Vec::new(),
                },
            );
            self.flags |= BodyFlags::SUSPEND;
            self.set_value(id, dest);
            return Ok(Some(dest));
        }
        if let Some(result) = self.emit_lang_call(id, &callee, &args)? {
            return Ok(Some(result));
        }
        self.emit_check_ops(id)?;
        self.emit_call(id, callee, args)
    }

    fn prepare_call(
        &mut self,
        target: hir::CallTarget,
        receiver: Option<ExprId>,
        arguments: Range<u32>,
    ) -> Result<(Callee, Vec<Operand>), Diagnostic> {
        let callee = match target {
            hir::CallTarget::Value(value) => {
                let Some(local) = self.emit_expr(value)? else {
                    return Ok((
                        Callee::Value(copy_of(self.temp(self.primitives.unit))),
                        Vec::new(),
                    ));
                };
                Callee::Value(copy_of(local))
            }
            hir::CallTarget::Dispatch(dispatch) => {
                if self.owner.dispatches[dispatch as usize].dynamic {
                    Callee::Dynamic(dispatch)
                } else {
                    Callee::Dispatch(dispatch)
                }
            }
            hir::CallTarget::Builtin(builtin) => Callee::Builtin(builtin),
            hir::CallTarget::Constructor { .. } => {
                return Ok((Callee::Builtin(hir::Builtin::Some), Vec::new()));
            }
        };
        let mut args = Vec::new();
        if let Some(receiver) = receiver {
            let Some(local) = self.emit_expr(receiver)? else {
                return Ok((callee, args));
            };
            args.push(copy_of(local));
        }
        for argument in expr_range(self.owner, &arguments) {
            let Some(local) = self.emit_expr(argument)? else {
                return Ok((callee, args));
            };
            args.push(copy_of(local));
        }
        Ok((callee, args))
    }

    fn emit_constructor(
        &mut self,
        id: ExprId,
        ty: TypeId,
        variant: u32,
        arguments: Range<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let mut operands = Vec::new();
        for argument in expr_range(self.owner, &arguments) {
            let Some(local) = self.emit_expr(argument)? else {
                return Ok(None);
            };
            operands.push(copy_of(local));
        }
        let dest = self.temp(self.expr_ty(id));
        self.assign(
            Place::local(dest),
            Rvalue::Aggregate {
                kind: AggregateKind::Adt { ty, variant },
                operands,
            },
        );
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_call(
        &mut self,
        id: ExprId,
        callee: Callee,
        args: Vec<Operand>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let dest = self.temp(self.expr_ty(id));
        let normal = self.fresh(false);
        let unwind = self.intern_plan(self.current_unwind(id), CleanupChain::Unwind)?;
        let call_kind = self.call_kind(id);
        self.terminate(Terminator::Call {
            callee,
            args,
            destination: Place::local(dest),
            normal,
            unwind: Some(unwind),
            call_kind,
            site: crate::frontend::mono::instantiate::CallSite::Expression(id.0),
        });
        self.switch_to(normal);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn call_dispatch(
        &mut self,
        at: ExprId,
        dispatch: u32,
        args: Vec<Operand>,
    ) -> Result<LocalId, Diagnostic> {
        let dest = self.temp(self.expr_ty(at));
        let normal = self.fresh(false);
        let unwind = self.intern_plan(self.current_unwind(at), CleanupChain::Unwind)?;
        self.terminate(Terminator::Call {
            callee: Callee::Dispatch(dispatch),
            args,
            destination: Place::local(dest),
            normal,
            unwind: Some(unwind),
            call_kind: CallKind::Managed,
            site: crate::frontend::mono::instantiate::CallSite::Dispatch(dispatch),
        });
        self.switch_to(normal);
        Ok(dest)
    }

    fn call_kind(&self, id: ExprId) -> CallKind {
        self.owner
            .foreign_calls
            .iter()
            .find(|call| call.expression == id)
            .map(|call| match call.effect {
                ForeignEffect::Bridge => CallKind::ForeignBridge,
                ForeignEffect::DirtyCpu => CallKind::ForeignBridgeDirtyCpu,
                ForeignEffect::Leaf { stack } => CallKind::ForeignLeaf { stack },
            })
            .unwrap_or(CallKind::Managed)
    }

    fn emit_builtin_call(
        &mut self,
        id: ExprId,
        builtin: hir::Builtin,
        receiver: Option<ExprId>,
        arguments: Range<u32>,
        spawn: bool,
    ) -> Result<Option<LocalId>, Diagnostic> {
        if spawn {
            return self.emit_call_expr(
                id,
                hir::CallTarget::Value(receiver.unwrap_or(id)),
                receiver,
                arguments,
                true,
            );
        }
        match builtin {
            hir::Builtin::Panic => self.emit_panic(id, arguments),
            hir::Builtin::ChanSend => self.emit_chan_send(id, arguments),
            hir::Builtin::ChanRecv => self.emit_chan_recv(id, arguments),
            hir::Builtin::JoinWait => self.emit_join_wait(id, arguments),
            hir::Builtin::Some | hir::Builtin::Ok | hir::Builtin::Err => {
                self.emit_ctor_builtin(id, builtin, arguments)
            }
            hir::Builtin::None => {
                self.emit_builtin_value(id, builtin)?;
                self.value_of(id)
            }
            _ => {
                let types = Vec::new();
                self.emit_intrinsic(id, builtin, arguments, types, None)
            }
        }
    }

    fn emit_ctor_builtin(
        &mut self,
        id: ExprId,
        builtin: hir::Builtin,
        arguments: Range<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let mut operands = Vec::new();
        for argument in expr_range(self.owner, &arguments) {
            let Some(local) = self.emit_expr(argument)? else {
                return Ok(None);
            };
            operands.push(copy_of(local));
        }
        let variant = match builtin {
            hir::Builtin::None => 0,
            hir::Builtin::Some | hir::Builtin::Ok => 1,
            hir::Builtin::Err => 0,
            _ => 0,
        };
        let dest = self.temp(self.expr_ty(id));
        self.assign(
            Place::local(dest),
            Rvalue::Aggregate {
                kind: AggregateKind::Adt {
                    ty: self.expr_ty(id),
                    variant,
                },
                operands,
            },
        );
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_intrinsic(
        &mut self,
        id: ExprId,
        operation: hir::Builtin,
        arguments: Range<u32>,
        types: Vec<TypeId>,
        field: Option<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        if matches!(operation, hir::Builtin::Panic) {
            return self.emit_panic(id, arguments);
        }
        let mut operands = Vec::new();
        for argument in expr_range(self.owner, &arguments) {
            let Some(local) = self.emit_expr(argument)? else {
                return Ok(None);
            };
            operands.push(copy_of(local));
        }
        self.emit_check_ops(id)?;
        if let hir::Builtin::Memory(memory) = operation {
            return self.emit_memory(id, memory, operands);
        }
        let dest = self.temp(self.expr_ty(id));
        let rvalue = match operation {
            hir::Builtin::SizeOf => intrinsic(IntrinsicOp::SizeOf, operands, types),
            hir::Builtin::AlignOf => intrinsic(IntrinsicOp::AlignOf, operands, types),
            hir::Builtin::OffsetOf => intrinsic(
                IntrinsicOp::OffsetOf {
                    field: field.unwrap_or(0),
                },
                operands,
                types,
            ),
            hir::Builtin::TypeId | hir::Builtin::TypeAsInt => {
                intrinsic(IntrinsicOp::TypeId, operands, types)
            }
            hir::Builtin::TypeIdCount => {
                Rvalue::Use(self.const_operand(self.expr_ty(id), ConstValue::Integer(0)))
            }
            hir::Builtin::TypeName => intrinsic(IntrinsicOp::TypeName, operands, types),
            hir::Builtin::Is => intrinsic(IntrinsicOp::Is, operands, types),
            hir::Builtin::Downcast => intrinsic(IntrinsicOp::Downcast, operands, types),
            hir::Builtin::DowncastCopy => intrinsic(IntrinsicOp::DowncastCopy, operands, types),
            hir::Builtin::Chan => intrinsic(IntrinsicOp::ChanNew, operands, types),
            hir::Builtin::ChanClose => intrinsic(IntrinsicOp::ChanClose, operands, types),
            hir::Builtin::Len => {
                if let Some(Operand::Copy(place)) = operands.first() {
                    Rvalue::Len(*place)
                } else {
                    intrinsic(IntrinsicOp::TypeId, operands, types)
                }
            }
            _ => intrinsic(IntrinsicOp::TypeId, operands, types),
        };
        self.assign(Place::local(dest), rvalue);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    fn emit_memory(
        &mut self,
        id: ExprId,
        memory: MemoryIntrinsic,
        operands: Vec<Operand>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        if matches!(memory, MemoryIntrinsic::Unreachable) {
            self.terminate(Terminator::Unreachable);
            return Ok(None);
        }
        if matches!(
            memory,
            MemoryIntrinsic::VolatileLoad | MemoryIntrinsic::VolatileStore
        ) {
            let dest = self.temp(self.expr_ty(id));
            let (op, value) = if matches!(memory, MemoryIntrinsic::VolatileLoad) {
                (VolatileOp::Load, None)
            } else {
                (VolatileOp::Store, operands.get(1).cloned())
            };
            self.push_stmt(StatementKind::Volatile {
                op,
                pointer: operands.first().cloned().unwrap_or_else(|| copy_of(dest)),
                value,
                destination: Some(Place::local(dest)),
            });
            self.set_value(id, dest);
            return Ok(Some(dest));
        }
        let dest = self.temp(self.expr_ty(id));
        let op = match memory {
            MemoryIntrinsic::AddrOf => {
                if let Some(Operand::Copy(place)) = operands.first() {
                    self.assign(Place::local(dest), Rvalue::RawAddress(*place));
                    self.set_value(id, dest);
                    return Ok(Some(dest));
                }
                IntrinsicOp::PtrRead
            }
            MemoryIntrinsic::PtrRead => IntrinsicOp::PtrRead,
            MemoryIntrinsic::PtrWrite => IntrinsicOp::PtrWrite,
            MemoryIntrinsic::ReadUnaligned => IntrinsicOp::ReadUnaligned,
            MemoryIntrinsic::WriteUnaligned => IntrinsicOp::WriteUnaligned,
            MemoryIntrinsic::UninitAsPtr => IntrinsicOp::UninitAsPtr,
            MemoryIntrinsic::UninitWrite => IntrinsicOp::UninitWrite,
            MemoryIntrinsic::Transmute
            | MemoryIntrinsic::PointerCast
            | MemoryIntrinsic::ScalarCast => {
                let kind = if matches!(memory, MemoryIntrinsic::Transmute) {
                    CastKind::Transmute
                } else {
                    CastKind::Pointer
                };
                self.assign(
                    Place::local(dest),
                    Rvalue::Cast {
                        kind,
                        operand: operands.first().cloned().unwrap_or_else(|| copy_of(dest)),
                        ty: self.expr_ty(id),
                    },
                );
                self.set_value(id, dest);
                return Ok(Some(dest));
            }
            MemoryIntrinsic::AssumeInit => {
                self.assign(
                    Place::local(dest),
                    Rvalue::Cast {
                        kind: CastKind::AssumeInit,
                        operand: operands.first().cloned().unwrap_or_else(|| copy_of(dest)),
                        ty: self.expr_ty(id),
                    },
                );
                self.set_value(id, dest);
                return Ok(Some(dest));
            }
            MemoryIntrinsic::Uninit | MemoryIntrinsic::UninitNew => {
                let operand = self.const_operand(self.primitives.unit, ConstValue::Unit);
                self.assign(
                    Place::local(dest),
                    Rvalue::Cast {
                        kind: CastKind::MaybeUninit,
                        operand,
                        ty: self.expr_ty(id),
                    },
                );
                self.set_value(id, dest);
                return Ok(Some(dest));
            }
            _ => IntrinsicOp::PtrRead,
        };
        self.assign(
            Place::local(dest),
            Rvalue::Intrinsic {
                op,
                operands,
                types: vec![self.expr_ty(id)],
            },
        );
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    fn emit_panic(
        &mut self,
        id: ExprId,
        arguments: Range<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let payload = if let Some(argument) = expr_range(self.owner, &arguments).into_iter().next()
        {
            match self.emit_expr(argument)? {
                Some(local) => copy_of(local),
                None => return Ok(None),
            }
        } else {
            self.panic_string("panic")
        };
        let unwind = self.intern_plan(self.current_unwind(id), CleanupChain::Unwind)?;
        self.terminate(Terminator::Panic { payload, unwind });
        Ok(None)
    }
}

fn intrinsic(op: IntrinsicOp, operands: Vec<Operand>, types: Vec<TypeId>) -> Rvalue {
    Rvalue::Intrinsic {
        op,
        operands,
        types,
    }
}
