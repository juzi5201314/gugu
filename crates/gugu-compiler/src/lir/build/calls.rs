use super::values::Computed;
use super::{Builder, Diagnostic, Storage, TypeKind, invalid};
use crate::frontend::{gir::body as g, mono};
use crate::lir::body::{
    BlockId, Call, CallTarget, Condition, Definition, IntOp, Memory, Op, Origin, RuntimeCall,
    Safepoint, SafepointId, Terminator, Type, ValueId, ValueType, id,
};

impl Builder<'_> {
    pub(super) fn runtime(
        &mut self,
        target: RuntimeCall,
        arguments: &[ValueId],
        results: &[ValueType],
    ) -> Result<Vec<ValueId>, Diagnostic> {
        let (may_unwind, may_suspend, may_allocate, captures_arguments) = target.effects();
        if may_unwind {
            return Err(invalid("可能 unwind 的 runtime 调用必须使用 Invoke"));
        }
        let call = Call {
            target: CallTarget::Runtime(target),
            kind: g::CallKind::Managed,
            parameters: arguments
                .iter()
                .map(|value| self.machine_type(*value))
                .collect(),
            results: results.to_vec(),
            may_unwind,
            may_suspend,
            may_allocate,
            captures_arguments,
            by_value: Vec::new(),
            sret: None,
            poll_free_leaf: false,
        };
        Ok(self.emit(
            Op::Call(call),
            arguments,
            &results
                .iter()
                .map(|kind| (*kind, Origin::None))
                .collect::<Vec<_>>(),
        ))
    }

    pub(super) fn terminator(&mut self, terminator: &g::Terminator) -> Result<(), Diagnostic> {
        match terminator {
            g::Terminator::Goto { target } => self.jump(self.target(*target)),
            g::Terminator::SwitchInt {
                value,
                targets,
                otherwise,
            } => {
                let value = self.operand(value)?;
                let values = self.computed_values(value)?;
                if values.len() == 2 {
                    for (case, target) in targets {
                        let bytes = case.to_le_bytes();
                        let low = self.constant(
                            u64::from_le_bytes(bytes[..8].try_into().expect("u128 低位")),
                            Type::I64,
                        );
                        let high = self.constant(
                            u64::from_le_bytes(bytes[8..].try_into().expect("u128 高位")),
                            Type::I64,
                        );
                        let low = self.compare_value(Condition::Eq, false, values[0], low);
                        let high = self.compare_value(Condition::Eq, false, values[1], high);
                        let condition = self.emit_one(
                            Op::Integer(IntOp::And),
                            &[low, high],
                            ValueType::scalar(Type::I8),
                            Origin::None,
                        );
                        let next = self.fresh(
                            self.source.clone(),
                            self.blocks[self.current.index()].cleanup,
                        );
                        self.branch(condition, self.target(*target), next);
                        self.current = next;
                    }
                    self.jump(self.target(*otherwise));
                } else {
                    let value = *values
                        .first()
                        .ok_or_else(|| invalid("SwitchInt 缺少机器值"))?;
                    let start = id(self.body.switch_cases.len());
                    for (case, target) in targets {
                        let case = u64::try_from(*case)
                            .map_err(|_| invalid("switch case 超过操作数宽度"))?;
                        let edge = self.edge(self.target(*target), false);
                        self.body.switch_cases.push((case, edge));
                    }
                    let cases = start..id(self.body.switch_cases.len());
                    let otherwise = self.edge(self.target(*otherwise), false);
                    self.blocks[self.current.index()].terminator = Some(Terminator::Switch {
                        value,
                        cases,
                        otherwise,
                    });
                }
            }
            g::Terminator::Return => {
                let value = self.read_place(g::Place::local(g::LocalId(0)))?;
                let values = if let Some(sret) = self.sret {
                    let source = self.computed_address(value)?;
                    self.copy_memory(sret, source, self.gir.signature.result.0)?;
                    Vec::new()
                } else {
                    self.computed_values(value)?
                };
                if let Storage::Stack { slot, .. } = self.storage[0] {
                    self.lifetime(slot, false);
                }
                let values = self.arguments(&values);
                let memory = self.blocks[self.current.index()].memory;
                self.blocks[self.current.index()].terminator =
                    Some(Terminator::Return { values, memory });
            }
            g::Terminator::ResumePanic => {
                let memory = self.blocks[self.current.index()].memory;
                self.blocks[self.current.index()].terminator =
                    Some(Terminator::ResumePanic { memory });
            }
            g::Terminator::Abort => {
                let memory = self.blocks[self.current.index()].memory;
                self.blocks[self.current.index()].terminator = Some(Terminator::Trap { memory });
            }
            g::Terminator::Unreachable => {
                let memory = self.blocks[self.current.index()].memory;
                self.blocks[self.current.index()].terminator =
                    Some(Terminator::Unreachable { memory });
            }
            g::Terminator::Call {
                callee,
                args,
                destination,
                normal,
                unwind,
                call_kind,
                site,
            } => {
                self.call(
                    callee,
                    args,
                    *destination,
                    *normal,
                    *unwind,
                    *call_kind,
                    *site,
                )?;
            }
            g::Terminator::Panic { payload, unwind } => {
                let payload = self.operand(payload)?;
                let args = self.computed_values(payload)?;
                let call = Call {
                    target: CallTarget::Runtime(RuntimeCall::Panic),
                    kind: g::CallKind::Managed,
                    parameters: args.iter().map(|value| self.machine_type(*value)).collect(),
                    results: Vec::new(),
                    may_unwind: true,
                    may_suspend: false,
                    may_allocate: false,
                    captures_arguments: false,
                    by_value: Vec::new(),
                    sret: None,
                    poll_free_leaf: false,
                };
                let normal = self.fresh(self.source.clone(), true);
                self.invoke(call, &args, normal, self.target(*unwind))?;
                self.current = normal;
                let memory = self.blocks[normal.index()].memory;
                self.blocks[normal.index()].terminator = Some(Terminator::Unreachable { memory });
            }
            g::Terminator::Suspend {
                reason,
                destination,
                resume,
                cancelled,
                ..
            } => self.suspend(reason, *destination, *resume, *cancelled)?,
            g::Terminator::SelectCommit {
                cases,
                index,
                ready,
                suspend,
                cancelled,
                ..
            } => self.select(cases.clone(), *index, *ready, *suspend, *cancelled)?,
        }
        Ok(())
    }

    fn branch(&mut self, condition: ValueId, yes: BlockId, no: BlockId) {
        let yes = self.edge(yes, false);
        let no = self.edge(no, false);
        self.blocks[self.current.index()].terminator =
            Some(Terminator::Branch { condition, yes, no });
    }

    fn call(
        &mut self,
        callee: &g::Callee,
        operands: &[g::Operand],
        destination: g::Place,
        normal: g::BlockId,
        unwind: Option<g::BlockId>,
        kind: g::CallKind,
        site: mono::instantiate::CallSite,
    ) -> Result<(), Diagnostic> {
        let result_ty = if destination.is_local() {
            self.local_ty(destination.local)
        } else {
            self.address(destination)?.1
        };
        let mut arguments = Vec::new();
        let mut by_value = Vec::new();
        let output = if self.indirect_abi(result_ty) {
            let output = self.temporary(result_ty)?;
            arguments.push(output);
            Some(output)
        } else {
            None
        };
        let target = self.callee(callee, site, &mut arguments)?;
        for operand in operands {
            let value = self.operand(operand)?;
            let ty = value.ty();
            if self.indirect_abi(ty) {
                by_value.push((
                    id(arguments.len()),
                    self.layout(ty).key,
                    self.layout(ty).layout.expect("参数布局").size,
                ));
            }
            arguments.extend(self.computed_values(value)?);
        }
        let results: Vec<_> = if output.is_some() {
            Vec::new()
        } else {
            self.abi_lanes(result_ty)
                .into_iter()
                .map(|(_, _, kind)| kind)
                .collect()
        };
        let sret = output.map(|_| {
            (
                0,
                self.layout(result_ty).layout.expect("sret 布局").size,
                self.layout(result_ty).key,
            )
        });
        let call = Call {
            target,
            kind,
            parameters: arguments
                .iter()
                .map(|value| self.machine_type(*value))
                .collect(),
            results: results.clone(),
            may_unwind: unwind.is_some(),
            may_suspend: false,
            may_allocate: matches!(kind, g::CallKind::Managed),
            captures_arguments: false,
            by_value,
            sret,
            poll_free_leaf: false,
        };
        let values = if let Some(unwind) = unwind {
            let continuation = self.fresh(
                self.source.clone(),
                self.blocks[self.current.index()].cleanup,
            );
            let values = self.invoke(call, &arguments, continuation, self.target(unwind))?;
            self.current = continuation;
            values
        } else {
            let op = if kind == g::CallKind::Managed {
                Op::Call(call)
            } else {
                Op::ForeignCall(call)
            };
            self.emit(
                op,
                &arguments,
                &results
                    .into_iter()
                    .map(|kind| (kind, Origin::None))
                    .collect::<Vec<_>>(),
            )
        };
        let result = output.map_or(
            Computed::Values {
                ty: result_ty,
                values,
            },
            |address| Computed::Address {
                ty: result_ty,
                address,
            },
        );
        self.write_place(destination, result)?;
        self.jump(self.target(normal));
        Ok(())
    }

    fn callee(
        &mut self,
        callee: &g::Callee,
        site: mono::instantiate::CallSite,
        arguments: &mut Vec<ValueId>,
    ) -> Result<CallTarget, Diagnostic> {
        if let Some((_, key)) = self
            .instance
            .call_targets
            .iter()
            .find(|(candidate, _)| *candidate == site)
        {
            let key = *key;
            if let Some(instance) = self
                .mono
                .instances
                .iter()
                .find(|instance| mono::digest_of(&instance.mono_key) == key)
                && let Some(owner) = self
                    .module
                    .owners
                    .iter()
                    .find(|owner| owner.definition.0 == instance.definition)
                && !owner.captures.is_empty()
            {
                let value = if let g::Callee::Value(value) = callee {
                    let value = self.operand(value)?;
                    self.computed_values(value)?.first().copied()
                } else {
                    None
                };
                arguments.push(match value {
                    Some(value) => value,
                    None => self.capture_environment(owner.definition)?,
                });
            }
            return Ok(CallTarget::Instance(key));
        }
        match callee {
            g::Callee::Value(operand) => {
                let value = self.operand(operand)?;
                let ty = value.ty();
                if let TypeKind::FunctionItem {
                    definition,
                    capturing,
                    ..
                } = self.kind(ty)
                {
                    let (definition, capturing) = (*definition, *capturing);
                    if let Some(linkage) = self.module.linkage.iter().find(|linkage| {
                        linkage.definition.0 == definition && linkage.foreign.is_some()
                    }) {
                        let definition = &self.module.definitions
                            [usize::try_from(definition).expect("定义编号")];
                        return Ok(CallTarget::External {
                            key: definition.key,
                            name: linkage
                                .import_name
                                .clone()
                                .unwrap_or_else(|| definition.name.clone()),
                        });
                    }
                    let key = self.function_key(definition, ty)?;
                    if capturing {
                        arguments.extend(self.computed_values(value)?);
                    }
                    Ok(CallTarget::Instance(key))
                } else {
                    arguments.extend(self.computed_values(value)?);
                    Ok(CallTarget::Indirect)
                }
            }
            g::Callee::Dynamic(dispatch) => {
                let dispatch =
                    &self.owner.dispatches[usize::try_from(*dispatch).expect("dispatch 编号")];
                Ok(CallTarget::Vtable {
                    slot: dispatch
                        .member
                        .ok_or_else(|| invalid("动态调用缺少 vtable 槽"))?,
                })
            }
            g::Callee::Dispatch(_) => Err(invalid("静态派发没有绑定具体实例")),
            g::Callee::Builtin(_) => Err(invalid("builtin 必须在 GIR 中展开为封闭操作")),
        }
    }

    fn invoke(
        &mut self,
        call: Call,
        arguments: &[ValueId],
        normal: BlockId,
        unwind: BlockId,
    ) -> Result<Vec<ValueId>, Diagnostic> {
        let block = self.current;
        let first = id(self.body.values.len());
        for (index, kind) in call.results.iter().enumerate() {
            self.value(
                *kind,
                Definition::Invoke {
                    block,
                    result: id(index),
                },
                Origin::None,
            );
        }
        let results = first..id(self.body.values.len());
        let output = self.value(
            ValueType::scalar(Type::Mem),
            Definition::Invoke {
                block,
                result: id(call.results.len()),
            },
            Origin::None,
        );
        let memory = Memory {
            input: self.blocks[block.index()].memory,
            output,
        };
        self.blocks[block.index()].memory = output;
        let normal_edge = self.edge(normal, false);
        let unwind_edge = self.edge(unwind, true);
        let mut parameters = Vec::with_capacity(call.results.len());
        for (index, (result, kind)) in results.clone().map(ValueId).zip(&call.results).enumerate() {
            let parameter = self.result_parameter(normal, *kind, id(index));
            self.fixed_edges.push((normal_edge, parameter, result));
            parameters.push(parameter);
        }
        let safepoint = call.safepoint_kind().map(|kind| {
            let id = SafepointId(id(self.body.safepoints.len()));
            self.body.safepoints.push(Safepoint {
                kind,
                block,
                instruction: None,
            });
            id
        });
        let arguments = self.arguments(arguments);
        self.blocks[block.index()].terminator = Some(Terminator::Invoke {
            call,
            arguments,
            results,
            memory,
            normal: normal_edge,
            unwind: unwind_edge,
            safepoint,
        });
        Ok(parameters)
    }

    fn suspend(
        &mut self,
        reason: &g::SuspendReason,
        destination: Option<g::Place>,
        resume: g::BlockId,
        cancelled: Option<g::BlockId>,
    ) -> Result<(), Diagnostic> {
        let (target, operands) = match reason {
            g::SuspendReason::ChanSend { channel, value } => {
                (RuntimeCall::ChannelSend, vec![channel, value])
            }
            g::SuspendReason::ChanRecv { channel } => (RuntimeCall::ChannelReceive, vec![channel]),
            g::SuspendReason::JoinWait { join } => (RuntimeCall::JoinWait, vec![join]),
            g::SuspendReason::Yield => (RuntimeCall::Yield, Vec::new()),
        };
        let mut args = Vec::new();
        for operand in operands {
            let value = self.operand(operand)?;
            args.extend(self.computed_values(value)?);
        }
        let result_ty = destination.map(|place| self.local_ty(place.local));
        let output = result_ty
            .filter(|ty| self.indirect_abi(*ty))
            .map(|ty| self.temporary(ty))
            .transpose()?;
        if let Some(output) = output {
            args.insert(0, output);
        }
        let result_kinds: Vec<_> = if cancelled.is_some() {
            vec![ValueType::scalar(Type::I8)]
        } else if let Some(ty) = result_ty.filter(|_| output.is_none()) {
            self.abi_lanes(ty)
                .into_iter()
                .map(|(_, _, kind)| kind)
                .collect()
        } else {
            Vec::new()
        };
        let values = self.runtime(target, &args, &result_kinds)?;
        if let (Some(destination), Some(ty)) = (destination, result_ty) {
            self.write_place(
                destination,
                output.map_or(
                    Computed::Values {
                        ty,
                        values: values.clone(),
                    },
                    |address| Computed::Address { ty, address },
                ),
            )?;
        }
        if let Some(cancelled) = cancelled {
            self.branch(values[0], self.target(resume), self.target(cancelled));
        } else {
            self.jump(self.target(resume));
        }
        Ok(())
    }

    fn select(
        &mut self,
        cases: std::ops::Range<u32>,
        index: g::LocalId,
        ready: g::BlockId,
        suspend: Option<g::BlockId>,
        cancelled: Option<g::BlockId>,
    ) -> Result<(), Diagnostic> {
        let mut args = Vec::new();
        for case in &self.gir.select_cases[crate::lir::body::range(&cases)] {
            let (tag, handle, value) = match &case.operation {
                g::SelectOperation::Send { channel, value } => (0, channel, Some(value)),
                g::SelectOperation::Recv { channel } => (1, channel, None),
                g::SelectOperation::Wait { join } => (2, join, None),
            };
            args.push(self.constant(tag, Type::I8));
            let handle = self.operand(handle)?;
            args.extend(self.computed_values(handle)?);
            if let Some(value) = value {
                let value = self.operand(value)?;
                let ty = value.ty();
                args.push(self.computed_address(value)?);
                args.push(self.descriptor(ty));
            }
            if let Some(destination) = case.destination {
                let (pointer, ty) = self.address(destination)?;
                args.push(pointer);
                args.push(self.descriptor(ty));
            }
        }
        let values = self.runtime(
            RuntimeCall::SelectCommit {
                cases: cases.end - cases.start,
                has_default: suspend.is_none(),
            },
            &args,
            &[ValueType::scalar(Type::I64), ValueType::scalar(Type::I8)],
        )?;
        self.write_place(
            g::Place::local(index),
            Computed::Values {
                ty: self.local_ty(index),
                values: vec![values[0]],
            },
        )?;
        let start = id(self.body.switch_cases.len());
        let ready_edge = self.edge(self.target(ready), false);
        self.body.switch_cases.push((0, ready_edge));
        if let Some(suspend) = suspend {
            let edge = self.edge(self.target(suspend), false);
            self.body.switch_cases.push((1, edge));
        }
        let cases = start..id(self.body.switch_cases.len());
        let otherwise = self.edge(self.target(cancelled.unwrap_or(ready)), false);
        self.blocks[self.current.index()].terminator = Some(Terminator::Switch {
            value: values[1],
            cases,
            otherwise,
        });
        Ok(())
    }
}
