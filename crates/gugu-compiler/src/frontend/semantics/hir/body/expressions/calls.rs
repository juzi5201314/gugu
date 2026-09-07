use super::*;
use crate::frontend::semantics::{model::MemoryIntrinsic, output::ReflectionKind};

impl BodyBuilder<'_, '_, '_, '_> {
    pub(super) fn call(
        &mut self,
        source: ast::ExprId,
        id: hir::ExprId,
        callee: ast::ExprId,
        _type_args: ast::AstRange<ast::GenericArg>,
        arguments: ast::AstRange<ast::ExprId>,
        spawn: bool,
    ) -> Result<(hir::ExprKind, u32), Diagnostic> {
        let arguments: Vec<_> = arguments
            .as_slice(&self.arena().expr_ids)
            .iter()
            .copied()
            .filter(|&argument| {
                self.compiler.model.modules[self.module]
                    .configured
                    .expr_active(argument)
            })
            .collect();
        if let Some(operation) = self
            .facts
            .body
            .memory_operations
            .iter()
            .find(|operation| operation.expression == callee)
        {
            let mut values = Vec::new();
            if matches!(
                operation.kind,
                MemoryIntrinsic::UninitAsPtr | MemoryIntrinsic::AssumeInit
            ) && operation.arguments.is_empty()
                || operation.kind == MemoryIntrinsic::UninitWrite && operation.arguments.len() == 1
            {
                values.push(self.receiver(callee)?);
            }
            for &argument in &operation.arguments {
                values.push(self.expression(argument)?);
            }
            let types = vec![
                self.type_id(&operation.value)?,
                self.type_id(&operation.result)?,
            ];
            self.expression_map[callee.0 as usize] = Some(id);
            let effects = memory_effects(operation.kind);
            return Ok((
                hir::ExprKind::Intrinsic {
                    operation: hir::Builtin::Memory(operation.kind),
                    arguments: self.expression_list(values)?,
                    types,
                    field: None,
                },
                effects,
            ));
        }
        if let Some(reflection) = self
            .facts
            .body
            .reflections
            .iter()
            .find(|reflection| reflection.expression == callee)
        {
            let (operation, ty) = match &reflection.kind {
                ReflectionKind::Is(ty) => (hir::Builtin::Is, Some(ty)),
                ReflectionKind::Downcast(ty) => (hir::Builtin::Downcast, Some(ty)),
                ReflectionKind::DowncastCopy(ty) => (hir::Builtin::DowncastCopy, Some(ty)),
                ReflectionKind::TypeName => (hir::Builtin::TypeName, None),
                ReflectionKind::TypeAsInt => (hir::Builtin::TypeAsInt, None),
                ReflectionKind::TypeId(_) | ReflectionKind::TypeIdCount => {
                    return Err(self.error("类型反射原语误用为实例调用"));
                }
            };
            let receiver = self.receiver(callee)?;
            let types = ty
                .map(|ty| self.type_id(ty))
                .transpose()?
                .into_iter()
                .collect();
            self.expression_map[callee.0 as usize] = Some(id);
            return Ok((
                hir::ExprKind::Intrinsic {
                    operation,
                    arguments: self.expression_list([receiver])?,
                    types,
                    field: None,
                },
                hir::Effects::READ,
            ));
        }
        if let ast::ExprKind::Path(path) = self.arena().exprs[callee.0 as usize].kind {
            let segments = self.arena().paths[path.0 as usize]
                .segments
                .as_slice(&self.arena().segments);
            let names = self.compiler.model.path(self.module, path);
            if names == ["panic"] {
                let values = arguments
                    .iter()
                    .map(|&argument| self.expression(argument))
                    .collect::<Result<Vec<_>, _>>()?;
                self.expression_map[callee.0 as usize] = Some(id);
                return Ok((
                    hir::ExprKind::Intrinsic {
                        operation: hir::Builtin::Panic,
                        arguments: self.expression_list(values)?,
                        types: Vec::new(),
                        field: None,
                    },
                    hir::Effects::PANIC,
                ));
            }
            if segments.len() == 2
                && let Some(&local) = self.names.get(names[0])
            {
                let receiver_ty = &self.facts.body.slots[self.local_sources[local.index()]];
                let operation = match (receiver_ty, names[1]) {
                    (Ty::Chan(_), "send") => Some(hir::Builtin::ChanSend),
                    (Ty::Chan(_), "recv") => Some(hir::Builtin::ChanRecv),
                    (Ty::Chan(_), "close") => Some(hir::Builtin::ChanClose),
                    (Ty::Join(_), "wait") => Some(hir::Builtin::JoinWait),
                    _ => None,
                };
                if let Some(operation) = operation {
                    let mut values = vec![self.receiver(callee)?];
                    for &argument in &arguments {
                        values.push(self.expression(argument)?);
                    }
                    self.expression_map[callee.0 as usize] = Some(id);
                    return Ok((
                        hir::ExprKind::Intrinsic {
                            operation,
                            arguments: self.expression_list(values)?,
                            types: Vec::new(),
                            field: None,
                        },
                        hir::Effects::SAFEPOINT
                            | hir::Effects::SUSPEND
                            | hir::Effects::READ
                            | hir::Effects::WRITE,
                    ));
                }
            }
            if let Some((ty, constructor)) =
                self.compiler
                    .model
                    .constructor(self.module, &names, Some(self.ty(source)?))?
            {
                let mut values = Vec::with_capacity(arguments.len());
                for (&argument, field) in arguments.iter().zip(&constructor.fields) {
                    let value = self.expression(argument)?;
                    self.coerce(value, self.ty(argument)?, &field.ty)?;
                    values.push(value);
                }
                self.expression_map[callee.0 as usize] = Some(id);
                if spawn {
                    let ty = self.type_id(&ty)?;
                    return Ok((
                        hir::ExprKind::SpawnCall {
                            target: hir::CallTarget::Constructor {
                                ty,
                                variant: checked_id(constructor.index)?,
                            },
                            receiver: None,
                            arguments: self.expression_list(values)?,
                        },
                        hir::Effects::ALLOCATE | hir::Effects::SAFEPOINT,
                    ));
                }
                let start = checked_id(self.output.fields.len())?;
                for (index, value) in values.into_iter().enumerate() {
                    self.output.fields.push(hir::FieldValue {
                        field: checked_id(index)?,
                        value,
                    });
                }
                return Ok((
                    hir::ExprKind::Construct {
                        variant: checked_id(constructor.index)?,
                        fields: start..checked_id(self.output.fields.len())?,
                    },
                    0,
                ));
            }
        }
        if let Some(receiver) = self.array_slice_len_receiver(callee, &arguments)? {
            self.expression_map[callee.0 as usize] = Some(id);
            return Ok((
                hir::ExprKind::Intrinsic {
                    operation: hir::Builtin::Len,
                    arguments: self.expression_list([receiver])?,
                    types: Vec::new(),
                    field: None,
                },
                hir::Effects::READ,
            ));
        }
        let selected = self.selected_dispatch(callee, None, None)?;
        let (target, receiver) = if let Some(dispatch) = selected {
            let receiver = if self.output.dispatches[dispatch as usize].implicit_receiver {
                Some(self.receiver(callee)?)
            } else {
                None
            };
            self.expression_map[callee.0 as usize] = Some(id);
            (hir::CallTarget::Dispatch(dispatch), receiver)
        } else {
            (hir::CallTarget::Value(self.expression(callee)?), None)
        };
        let mut values = Vec::with_capacity(arguments.len());
        for &argument in &arguments {
            values.push(self.expression(argument)?);
        }
        let effects = self.call_plans(source, callee, id)?;
        let arguments = self.expression_list(values)?;
        Ok((
            if spawn {
                hir::ExprKind::SpawnCall {
                    target,
                    receiver,
                    arguments,
                }
            } else {
                hir::ExprKind::Call {
                    target,
                    receiver,
                    arguments,
                }
            },
            effects,
        ))
    }

    pub(in super::super) fn coerce(
        &mut self,
        value: hir::ExprId,
        source: &Ty,
        target: &Ty,
    ) -> Result<(), Diagnostic> {
        if source == target {
            return Ok(());
        }
        let target_id = self.type_id(target)?;
        if *source == Ty::Never {
            self.adjustments[value.index()].push(hir::Adjustment::NeverTo(target_id));
        } else if matches!((source.deref(), target), (Ty::Array(_, _), Ty::Ref(inner)) if matches!(&**inner, Ty::Slice(_)))
        {
            self.adjustments[value.index()].push(hir::Adjustment::ArrayToSlice(target_id));
            self.expressions[value.index()]
                .as_mut()
                .expect("已形成实参")
                .effects
                .0 |= hir::Effects::ALLOCATE | hir::Effects::SAFEPOINT;
        } else {
            return Ok(());
        }
        self.output.expression_types[value.index()] = target_id;
        Ok(())
    }

    pub(super) fn intrinsic(
        &mut self,
        kind: ast::IntrinsicKind,
        types: ast::AstRange<ast::GenericArg>,
        arguments: ast::AstRange<ast::ExprId>,
        field: Option<crate::frontend::intern::Symbol>,
    ) -> Result<hir::ExprKind, Diagnostic> {
        let formed = types
            .as_slice(&self.arena().generic_args)
            .iter()
            .map(|&argument| self.compiler.model.form_argument(self.module, argument))
            .collect::<Result<Vec<_>, _>>()?;
        let field = field
            .map(|name| {
                let ty = formed
                    .first()
                    .ok_or_else(|| self.error("offset_of 没有类型参数"))?;
                self.compiler
                    .model
                    .find_field(ty, self.compiler.model.name(self.module, name))
                    .map(|(index, _, _)| checked_id(index))
                    .ok_or_else(|| self.error("offset_of 没有已解析字段"))?
            })
            .transpose()?;
        let types = formed
            .iter()
            .map(|ty| self.type_id(ty))
            .collect::<Result<Vec<_>, _>>()?;
        let operation = match kind {
            ast::IntrinsicKind::SizeOf => hir::Builtin::SizeOf,
            ast::IntrinsicKind::AlignOf => hir::Builtin::AlignOf,
            ast::IntrinsicKind::TypeId => hir::Builtin::TypeId,
            ast::IntrinsicKind::OffsetOf => hir::Builtin::OffsetOf,
            ast::IntrinsicKind::TypeIdCount => hir::Builtin::TypeIdCount,
            ast::IntrinsicKind::Chan => hir::Builtin::Chan,
        };
        Ok(hir::ExprKind::Intrinsic {
            operation,
            arguments: self.lower_arguments(arguments)?,
            types,
            field,
        })
    }

    pub(in super::super) fn assembly(
        &mut self,
        source: ast::ExprId,
    ) -> Result<hir::ExprKind, Diagnostic> {
        let plan = self
            .facts
            .body
            .assembly
            .iter()
            .find(|plan| plan.expression == source)
            .ok_or_else(|| self.error("汇编缺少已检查计划"))?;
        let mut operands = Vec::with_capacity(plan.operands.len());
        for operand in &plan.operands {
            operands.push(hir::AssemblyOperand {
                register: operand.register,
                direction: operand.direction,
                value: self.expression(operand.expression)?,
            });
        }
        let id = checked_id(self.output.assembly.len())?;
        self.output.assembly.push(hir::Assembly {
            template: plan.template.clone(),
            context: plan.context,
            operands,
            clobbers: plan.clobbers,
            stack_reserve: plan.stack_reserve,
        });
        Ok(hir::ExprKind::Assembly(id))
    }

    fn array_slice_len_receiver(
        &mut self,
        callee: ast::ExprId,
        arguments: &[ast::ExprId],
    ) -> Result<Option<hir::ExprId>, Diagnostic> {
        if !self.is_array_slice_len(callee, arguments) {
            return Ok(None);
        }
        if self.path_len_is_ufcs(callee) {
            let receiver = arguments
                .first()
                .copied()
                .ok_or_else(|| self.error("len 需要接收者实参"))?;
            return Ok(Some(self.expression(receiver)?));
        }
        Ok(Some(self.receiver(callee)?))
    }

    fn is_array_slice_len(&self, callee: ast::ExprId, arguments: &[ast::ExprId]) -> bool {
        match self.arena().exprs[callee.0 as usize].kind {
            ast::ExprKind::Field { base, name } => {
                self.is_len_name(name) && self.facts.ty(base).is_some_and(Self::is_len_self)
            }
            ast::ExprKind::Path(path) => self.path_is_array_slice_len(path, arguments),
            ast::ExprKind::TypeApp { base, .. } | ast::ExprKind::Paren(base) => {
                self.is_array_slice_len(base, arguments)
            }
            _ => false,
        }
    }

    fn path_len_is_ufcs(&self, callee: ast::ExprId) -> bool {
        let ast::ExprKind::Path(path) = self.arena().exprs[callee.0 as usize].kind else {
            return false;
        };
        self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments)
            .last()
            .is_some_and(|segment| segment.colon)
    }

    fn path_is_array_slice_len(&self, path: ast::PathId, arguments: &[ast::ExprId]) -> bool {
        let segments = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments);
        let Some(last) = segments.last() else {
            return false;
        };
        if !self.is_len_name(last.name) || segments.len() < 2 {
            return false;
        }
        if last.colon {
            return arguments
                .first()
                .and_then(|&argument| self.facts.ty(argument))
                .is_some_and(Self::is_len_self);
        }
        self.path_prefix_is_len_self(&segments[..segments.len() - 1])
    }

    fn path_prefix_is_len_self(&self, prefix: &[ast::PathSegment]) -> bool {
        let Some(first) = prefix.first() else {
            return false;
        };
        let first_name = self.compiler.model.name(self.module, first.name);
        let Some(&local) = self.names.get(first_name) else {
            return false;
        };
        let mut ty = self.facts.body.slots[self.local_sources[local.index()]].clone();
        for segment in &prefix[1..] {
            let name = self.compiler.model.name(self.module, segment.name);
            let Some((_, field, _)) = self.compiler.model.find_field(&ty, name) else {
                return false;
            };
            ty = field;
        }
        Self::is_len_self(&ty)
    }

    fn is_len_name(&self, name: crate::frontend::intern::Symbol) -> bool {
        self.compiler.model.name(self.module, name) == "len"
    }

    fn is_len_self(ty: &Ty) -> bool {
        matches!(ty.deref(), Ty::Array(_, _) | Ty::Slice(_))
    }
}

fn memory_effects(operation: MemoryIntrinsic) -> u32 {
    let memory = match operation {
        MemoryIntrinsic::PtrRead
        | MemoryIntrinsic::ReadUnaligned
        | MemoryIntrinsic::VolatileLoad
        | MemoryIntrinsic::AssumeInit => hir::Effects::READ,
        MemoryIntrinsic::PtrWrite
        | MemoryIntrinsic::WriteUnaligned
        | MemoryIntrinsic::VolatileStore
        | MemoryIntrinsic::UninitWrite => hir::Effects::WRITE,
        _ => 0,
    };
    memory
        | if matches!(
            operation,
            MemoryIntrinsic::AddrOf
                | MemoryIntrinsic::Uninit
                | MemoryIntrinsic::UninitNew
                | MemoryIntrinsic::UninitAsPtr
                | MemoryIntrinsic::UninitWrite
                | MemoryIntrinsic::ScalarCast
        ) {
            0
        } else {
            hir::Effects::UNSAFE
        }
}
