use super::*;
use crate::semantics::foreign::ForeignEffect;
use crate::semantics::model::MemoryIntrinsic;

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
        if !spawn
            && let Some(result) = self.emit_builtin_dispatch(id, &target, receiver, &arguments)?
        {
            return Ok(Some(result));
        }
        let (callee, mut args) = self.prepare_call(id, &target, receiver, arguments)?;
        self.materialize_variadic(id, &target, &mut args)?;
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

    /// 变参调用：把尾部实参物化成被调者最后一个参数。
    fn materialize_variadic(
        &mut self,
        id: ExprId,
        target: &hir::CallTarget,
        args: &mut Vec<Operand>,
    ) -> Result<(), Diagnostic> {
        let Some(plan) = self
            .owner
            .variadic_calls
            .iter()
            .find(|plan| plan.expression == id)
            .cloned()
        else {
            return Ok(());
        };
        let fixed = usize::try_from(plan.fixed_count).expect("实参数量适配宿主");
        if fixed > args.len() {
            return Ok(());
        }
        let tail = args.split_off(fixed);
        let parameters = self.call_parameters(target);
        let Some(tail_ty) = parameters.last().copied() else {
            return Ok(());
        };
        let local = self.temp(tail_ty);
        if plan.heterogeneous {
            self.assign(
                Place::local(local),
                Rvalue::Aggregate {
                    kind: AggregateKind::Tuple,
                    operands: tail,
                },
            );
        } else {
            self.assign(
                Place::local(local),
                Rvalue::Intrinsic {
                    op: IntrinsicOp::PackSlice,
                    operands: tail,
                    types: vec![plan.element],
                },
            );
        }
        args.push(Operand::MoveInternal(Place::local(local)));
        Ok(())
    }

    /// 被调者签名参数表：函数项取表达式类型，方法取派发签名。
    fn call_parameters(&self, target: &hir::CallTarget) -> Vec<TypeId> {
        let ty = match target {
            hir::CallTarget::Value(expression) => self.owner.expression_types[expression.index()],
            hir::CallTarget::Dispatch(dispatch) => {
                self.owner.dispatches[*dispatch as usize].signature
            }
            _ => return Vec::new(),
        };
        let signature = match self.module.types.get(ty.index()) {
            Some(hir::Type::Function { .. }) => ty,
            Some(hir::Type::Callable { signature, .. }) => *signature,
            _ => return Vec::new(),
        };
        match self.module.types.get(signature.index()) {
            Some(hir::Type::Function { parameters, .. }) => parameters.clone(),
            _ => Vec::new(),
        }
    }

    fn prepare_call(
        &mut self,
        id: ExprId,
        target: &hir::CallTarget,
        receiver: Option<ExprId>,
        arguments: Range<u32>,
    ) -> Result<(Callee, Vec<Operand>), Diagnostic> {
        let callee = match target {
            hir::CallTarget::Value(value) => {
                let Some(local) = self.emit_expr(*value)? else {
                    return Ok((
                        Callee::Value(copy_of(self.temp(self.primitives.unit))),
                        Vec::new(),
                    ));
                };
                Callee::Value(copy_of(local))
            }
            hir::CallTarget::Dispatch(dispatch) => {
                if self.owner.dispatches[*dispatch as usize].dynamic {
                    Callee::Dynamic(*dispatch)
                } else {
                    Callee::Dispatch(*dispatch)
                }
            }
            hir::CallTarget::Builtin(builtin) => Callee::Builtin(*builtin),
            hir::CallTarget::Constructor { .. } => {
                return Ok((Callee::Builtin(hir::Builtin::Some), Vec::new()));
            }
        };
        let parameters = match target {
            hir::CallTarget::Dispatch(dispatch) => self.dispatch_parameters(*dispatch),
            _ => Vec::new(),
        };
        let dereferences = match target {
            hir::CallTarget::Dispatch(dispatch) => {
                self.owner.dispatches[*dispatch as usize].dereferences
            }
            _ => 0,
        };
        let mut args = Vec::new();
        if let Some(receiver) = receiver {
            let Some(local) = self.emit_expr(receiver)? else {
                return Ok((callee, args));
            };
            args.push(self.receiver_operand(
                receiver,
                local,
                dereferences,
                parameters.first().copied(),
            ));
        }
        let offset = usize::from(receiver.is_some());
        // 变参尾按元素/包展开，最后一个签名参数是打包类型，不参与逐参借用。
        let fixed = self
            .owner
            .variadic_calls
            .iter()
            .find(|plan| plan.expression == id)
            .map(|plan| usize::try_from(plan.fixed_count).expect("实参数量适配宿主"));
        for (index, argument) in expr_range(self.owner, &arguments).into_iter().enumerate() {
            let Some(local) = self.emit_expr(argument)? else {
                return Ok((callee, args));
            };
            let parameter = parameters
                .get(offset + index)
                .copied()
                .filter(|_| fixed.is_none_or(|fixed| offset + index < fixed));
            args.push(match parameter {
                Some(parameter) => self.dispatch_argument(parameter, local, Some(argument)),
                None => self.pass_arg(argument, local),
            });
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
            operands.push(self.pass_arg(argument, local));
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
            site: crate::mono::instantiate::CallSite::Expression(id.0),
        });
        self.switch_to(normal);
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    /// 派发签名参数表；签名必须已由语义检查物化为函数类型。
    pub(super) fn dispatch_parameters(&self, dispatch: u32) -> Vec<TypeId> {
        let signature = self.owner.dispatches[dispatch as usize].signature;
        match self.module.types.get(signature.index()) {
            Some(hir::Type::Function { parameters, .. }) => parameters.clone(),
            _ => Vec::new(),
        }
    }

    /// 方法接收者：按派发记录的解引用次数投影原槽，再按首参决定借用或复制。
    ///
    /// 引用形参必须借用投影后的 place。先把聚合复制进临时槽再借用，会让 `lock`
    /// 作用在副本上。值形参仍走复制。
    fn receiver_operand(
        &mut self,
        expression: ExprId,
        local: LocalId,
        dereferences: u32,
        parameter: Option<TypeId>,
    ) -> Operand {
        if !parameter.is_some_and(|ty| self.ref_type(ty)) {
            let local = self.dereference(local, dereferences);
            return match parameter {
                Some(parameter) => self.dispatch_argument(parameter, local, Some(expression)),
                None => self.pass_arg(expression, local),
            };
        }
        let mut place = self.expression_places[expression.index()].unwrap_or(Place::local(local));
        for _ in 0..dereferences {
            place = self.project(place, Projection::Deref);
        }
        if self.ref_type(self.place_ty(place)) {
            let ty = self.place_ty(place);
            let dest = self.temp(ty);
            self.copy_value(Place::local(dest), place, ty);
            return Operand::MoveInternal(Place::local(dest));
        }
        let dest = self.temp(parameter.expect("引用形参"));
        self.assign(Place::local(dest), Rvalue::Ref(place));
        Operand::MoveInternal(Place::local(dest))
    }

    fn ref_type(&self, ty: TypeId) -> bool {
        matches!(self.module.types.get(ty.index()), Some(hir::Type::Ref(_)))
    }

    /// 派发实参：签名参数为 `&T` 而实参不是引用时按语言规则自动借用。
    pub(super) fn dispatch_argument(
        &mut self,
        parameter: TypeId,
        local: LocalId,
        expression: Option<ExprId>,
    ) -> Operand {
        let reference = matches!(
            self.module.types.get(parameter.index()),
            Some(hir::Type::Ref(_))
        );
        let already = expression.is_some_and(|id| {
            matches!(
                self.module
                    .types
                    .get(self.owner.expression_types[id.index()].index()),
                Some(hir::Type::Ref(_))
            )
        });
        if !reference || already {
            return copy_of(local);
        }
        let place = expression
            .and_then(|id| self.expression_places[id.index()])
            .unwrap_or(Place::local(local));
        let dest = self.temp(parameter);
        self.assign(Place::local(dest), Rvalue::Ref(place));
        Operand::MoveInternal(Place::local(dest))
    }

    /// 派发接收者：按签名首参自动借用。
    pub(super) fn dispatch_receiver(
        &mut self,
        dispatch: u32,
        local: LocalId,
        expression: Option<ExprId>,
    ) -> Operand {
        match self.dispatch_parameters(dispatch).first().copied() {
            Some(parameter) => self.dispatch_argument(parameter, local, expression),
            None => copy_of(local),
        }
    }

    pub(super) fn call_dispatch(
        &mut self,
        at: ExprId,
        dispatch: u32,
        args: Vec<Operand>,
    ) -> Result<LocalId, Diagnostic> {
        let signature = self.owner.dispatches[dispatch as usize].signature;
        let result = match self.module.types.get(signature.index()) {
            Some(hir::Type::Function { result, .. }) => *result,
            _ => self.expr_ty(at),
        };
        let dest = self.temp(result);
        let normal = self.fresh(false);
        let unwind = self.intern_plan(self.current_unwind(at), CleanupChain::Unwind)?;
        self.terminate(Terminator::Call {
            callee: Callee::Dispatch(dispatch),
            args,
            destination: Place::local(dest),
            normal,
            unwind: Some(unwind),
            call_kind: CallKind::Managed,
            site: crate::mono::instantiate::CallSite::Dispatch(dispatch),
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
            hir::Builtin::ChanTrySend | hir::Builtin::ChanTryRecv => {
                self.emit_chan_try(id, builtin, arguments)
            }
            hir::Builtin::JoinWait => self.emit_join_wait(id, arguments),
            hir::Builtin::Some | hir::Builtin::Ok | hir::Builtin::Err => {
                self.emit_ctor_builtin(id, builtin, arguments)
            }
            hir::Builtin::None => {
                self.emit_builtin_value(id, builtin)?;
                self.value_of(id)
            }
            hir::Builtin::Runtime(kind) => self.emit_runtime_intrinsic(id, kind, arguments),
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
        if let hir::Builtin::Runtime(kind) = operation {
            return self.emit_runtime_intrinsic(id, kind, arguments);
        }
        if let hir::Builtin::Platform(kind) = operation {
            return self.emit_platform_call(id, platform_op(kind), arguments, types);
        }
        if let hir::Builtin::RuntimeMachine(kind) = operation {
            if kind.is_buffer() {
                return self.emit_buffer_intrinsic(id, kind, arguments);
            }
            if let Some(op) = runtime_machine_op(kind) {
                return self.emit_runtime_machine_call(id, op, arguments, types);
            }
            return self.emit_runtime_atomic_call(id, kind, arguments);
        }
        match operation {
            hir::Builtin::ChanSend => return self.emit_chan_send(id, arguments),
            hir::Builtin::ChanRecv => return self.emit_chan_recv(id, arguments),
            hir::Builtin::ChanTrySend | hir::Builtin::ChanTryRecv => {
                return self.emit_chan_try(id, operation, arguments);
            }
            hir::Builtin::JoinWait => return self.emit_join_wait(id, arguments),
            _ => {}
        }
        if matches!(operation, hir::Builtin::Memory(MemoryIntrinsic::AddrOf)) {
            // `addr_of` 的实参是位点：必须保留投射链并直接取该位点的地址。
            // 其余内存原语的实参是值，先物化成临时量再取值没有语义问题；
            // 但位点一旦物化，取到的就是副本的地址——写穿失效、地址也不再是对象本体。
            let mut operands = Vec::new();
            for argument in expr_range(self.owner, &arguments) {
                let place = self.emit_place(argument)?;
                operands.push(Operand::Copy(place));
            }
            self.emit_check_ops(id)?;
            return self.emit_memory(id, MemoryIntrinsic::AddrOf, operands);
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
            hir::Builtin::TypeIdCount => Rvalue::Use(Operand::LateConstRef { expression: id.0 }),
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
                    return Err(gir_error(
                        "len 缺少可投影的操作数",
                        Some(&self.source_of(id).location),
                    ));
                }
            }
            _ => {
                return Err(gir_error(
                    "该 intrinsic 没有对应的 GIR 操作",
                    Some(&self.source_of(id).location),
                ));
            }
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
            MemoryIntrinsic::ArenaNew { sync } => {
                let int = self.int_ty();
                let capacity = operands
                    .first()
                    .cloned()
                    .unwrap_or_else(|| self.const_operand(int, ConstValue::Integer(0)));
                self.assign(
                    Place::local(dest),
                    Rvalue::Intrinsic {
                        op: IntrinsicOp::ArenaNew { sync },
                        operands: vec![capacity],
                        types: vec![self.expr_ty(id)],
                    },
                );
                self.set_value(id, dest);
                return Ok(Some(dest));
            }
            MemoryIntrinsic::ArenaAlloc { sync } => {
                let ty = self.expr_ty(id);
                let hir::Type::Ref(inner) = self.module.types[ty.index()] else {
                    return Err(gir_error(
                        "arena alloc 的结果必须是引用类型",
                        Some(&self.source_of(id).location),
                    ));
                };
                if self.passing().class(inner).has_resource() {
                    return Err(gir_error(
                        "arena 不接受含 resource 字段的类型",
                        Some(&self.source_of(id).location),
                    ));
                }
                self.assign(
                    Place::local(dest),
                    Rvalue::Intrinsic {
                        op: IntrinsicOp::ArenaAlloc { sync },
                        operands,
                        types: vec![inner],
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
        let location = self.owner.expressions[id.index()].location.clone();
        self.terminate(Terminator::Panic {
            payload,
            location,
            unwind,
        });
        Ok(None)
    }
}

/// 把语义层的平台原语映射到契约操作；两者的枚举一一对应，映射是全覆盖的。
fn platform_op(kind: crate::semantics::model::PlatformIntrinsic) -> crate::runtime::PlatformOp {
    use crate::runtime::PlatformOp as Target;
    use crate::semantics::model::PlatformIntrinsic as Source;
    match kind {
        Source::ReserveAligned => Target::ReserveAligned,
        Source::Commit => Target::Commit,
        Source::Decommit => Target::Decommit,
        Source::Release => Target::Release,
        Source::ProtectGuard => Target::ProtectGuard,
        Source::Unprotect => Target::Unprotect,
        Source::Wait => Target::Wait,
        Source::Wake => Target::Wake,
        Source::Entropy => Target::Entropy,
        Source::Zero => Target::Zero,
        Source::SetDumpPolicy => Target::SetDumpPolicy,
        Source::LowMemoryHint => Target::LowMemoryHint,
        Source::HugePageHint => Target::HugePageHint,
    }
}

/// 把语义层的 runtime 机器原语映射到契约操作；两者的枚举一一对应，映射是全覆盖的。
fn runtime_machine_op(
    kind: crate::semantics::model::RuntimeMachineIntrinsic,
) -> Option<crate::runtime::RuntimeMachineOp> {
    use crate::runtime::RuntimeMachineOp as Target;
    use crate::semantics::model::RuntimeMachineIntrinsic as Source;
    Some(match kind {
        Source::ContextSwitch => Target::ContextSwitch,
        Source::ContextRestore => Target::ContextRestore,
        Source::CurrentSlot => Target::CurrentSlot,
        Source::ProcessExit => Target::ProcessExit,
        Source::ReportWrite => Target::ReportWrite,
        Source::GcConfig => Target::GcConfig,
        Source::GcEnter => Target::GcEnter,
        Source::FsStart => Target::FsStart,
        Source::FsPoll => Target::FsPoll,
        Source::NetCall => Target::NetCall,
        Source::ResolverSymbol => Target::ResolverSymbol,
        Source::StartupCall => Target::StartupCall,
        Source::EnvironmentSymbol => Target::EnvironmentSymbol,
        Source::ProcessCall => Target::ProcessCall,
        Source::ProcessWorker => Target::ProcessWorker,
        Source::MonotonicNanos => Target::MonotonicNanos,
        Source::WallNanos => Target::WallNanos,
        Source::ParallelismProbe => Target::ParallelismProbe,
        Source::SignalInstall => Target::SignalInstall,
        Source::StackReadWord => Target::StackReadWord,
        Source::StackCopyRaw => Target::StackCopyRaw,
        Source::CageControl => Target::CageControl,
        Source::CallbackControl => Target::CallbackControl,
        Source::CallbackQueue => Target::CallbackQueue,
        Source::ThreadId => Target::ThreadId,
        Source::AllocBytes | Source::StringBytes | Source::StringFromUtf8Unchecked => return None,
        Source::AtomicLoadAcquire
        | Source::AtomicStoreRelease
        | Source::AtomicFetchOrAcqRel
        | Source::AtomicFetchAndAcqRel
        | Source::AtomicCompareExchangeAcqRel => return None,
    })
}

fn intrinsic(op: IntrinsicOp, operands: Vec<Operand>, types: Vec<TypeId>) -> Rvalue {
    Rvalue::Intrinsic {
        op,
        operands,
        types,
    }
}
