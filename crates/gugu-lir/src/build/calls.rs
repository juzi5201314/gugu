use super::values::Computed;
use super::{Builder, Diagnostic, Storage, TypeKind, invalid};
use crate::body::{
    AtomicOp, BlockId, Call, CallTarget, Condition, Conversion, Definition, IntOp, Memory, Op,
    Origin, Provenance, RuntimeCall, Safepoint, SafepointId, Terminator, Type, ValueId, ValueType,
    id,
};
use crate::{gir::body as g, mono};

impl Builder<'_> {
    /// 初始化器把结果发布进静态块：先写值槽，再 release 置 initialized flags。
    ///
    /// 值槽地址就是 `StaticRef` 返回的地址（块首 +8），位字是块首本身。收集器只在位字已发布
    /// 时扫描值槽，因此绝不会读到半初始化的内容；初始化器自己构造完整值再发布，中途 panic
    /// 时位字保持 0。聚合结果按字节从自己的地址复制，多车道标量按车道依次落槽。
    pub(super) fn publish_static(&mut self, value: Computed) -> Result<(), Diagnostic> {
        let definition = crate::hir::DefId(self.instance.definition);
        // 初始化器正文的签名结果就是静态值类型，且已经过具体化改写。
        let value_ty = self.gir.signature.result.0;
        let symbol = self.static_block(definition, value_ty)?;
        let layout = self
            .layout(value_ty)
            .layout
            .ok_or_else(|| invalid("静态初始化器缺少结果布局"))?;
        let block = self.emit_one(
            Op::SymbolAddr(symbol),
            &[],
            ValueType::pointer(Provenance::Foreign),
            Origin::Symbol,
        );
        let slot = self.offset(block, 8);
        if layout.size != 0 {
            if let Computed::Address { address, .. } = value {
                self.emit(Op::Memcpy { bytes: layout.size }, &[slot, address], &[]);
            } else {
                for (index, lane) in self.computed_values(value)?.into_iter().enumerate() {
                    let destination =
                        self.offset(slot, u64::try_from(index).expect("车道编号") * 8);
                    self.store(destination, lane, 8, false);
                }
            }
        }
        let published = self.constant(1, Type::I64);
        self.emit(
            Op::Atomic {
                op: AtomicOp::Store,
                ordering: g::MemoryOrdering::Release,
                failure: None,
                align: 8,
            },
            &[block, published],
            &[],
        );
        Ok(())
    }

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
            foreign_arguments: Vec::new(),
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
                if self.instance.kind == crate::mono::keys::MonoKind::StaticInit {
                    self.publish_static(value.clone())?;
                }
                let values = if let Some(sret) = self.sret {
                    let source = self.computed_address(value)?;
                    let ty = self.gir.signature.result.0;
                    self.copy_memory(sret, source, ty)?;
                    if self.layout(ty).passing.has_resource() {
                        let descriptor = self.descriptor(ty);
                        self.runtime(RuntimeCall::ValueForget, &[source, descriptor], &[])?;
                    }
                    Vec::new()
                } else {
                    self.computed_values(value)?
                };
                self.release_stack_resources()?;
                if let Storage::Stack { slot, .. } = self.storage[0] {
                    self.lifetime(slot, false);
                }
                let values = self.arguments(&values);
                let memory = self.blocks[self.current.index()].memory;
                self.blocks[self.current.index()].terminator =
                    Some(Terminator::Return { values, memory });
            }
            g::Terminator::ResumePanic => {
                self.release_stack_resources()?;
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
            g::Terminator::Panic {
                payload,
                location,
                unwind,
            } => {
                let payload = self.operand(payload)?;
                let mut args = self.explicit_abi_values(payload)?;
                if let Some(message) = args.first().copied()
                    && self
                        .machine_type(message)
                        .provenance
                        .is_some_and(Provenance::managed)
                {
                    args[0] = self.emit_one(
                        Op::Convert(crate::body::Conversion::PointerCast),
                        &[message],
                        ValueType::pointer(Provenance::Raw),
                        Origin::Derived(message),
                    );
                }
                /* #[track_caller] owner 的 panic 报告调用者位置：转发签名末尾三个
                隐藏实参（file 字符串按 ABI 展开为 ptr+len），普通 owner 折叠为
                编译期常量。*/
                if let Some([file, line, column]) = self.caller_location_params() {
                    args.extend(self.local_values(file)?);
                    args.extend(self.local_values(line)?);
                    args.extend(self.local_values(column)?);
                } else {
                    let source = self
                        .sources
                        .snapshot(crate::SourceFileId::new(location.source))
                        .ok_or_else(|| invalid("panic 源位置引用未知源码快照"))?;
                    let position = source
                        .line_column(usize::try_from(location.start).expect("源码偏移适配宿主"))
                        .map_err(|_| invalid("panic 源位置不在源码快照边界内"))?;
                    let path =
                        self.data(source.logical_path().as_bytes().to_vec(), Provenance::Raw);
                    args.push(path);
                    args.push(self.constant(
                        u64::try_from(source.logical_path().len()).expect("源码路径长度适配 u64"),
                        Type::I64,
                    ));
                    args.push(self.constant(u64::from(position.line), Type::I64));
                    args.push(self.constant(u64::from(position.column), Type::I64));
                }
                let call = Call {
                    target: CallTarget::Runtime(RuntimeCall::Panic),
                    kind: g::CallKind::Managed,
                    parameters: args.iter().map(|value| self.machine_type(*value)).collect(),
                    results: vec![ValueType::scalar(Type::I64)],
                    may_unwind: true,
                    may_suspend: false,
                    may_allocate: false,
                    captures_arguments: false,
                    by_value: Vec::new(),
                    sret: None,
                    foreign_arguments: Vec::new(),
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

    pub(super) fn branch(&mut self, condition: ValueId, yes: BlockId, no: BlockId) {
        let yes = self.edge(yes, false);
        let no = self.edge(no, false);
        self.blocks[self.current.index()].terminator =
            Some(Terminator::Branch { condition, yes, no });
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "调用 lowering 需要 callee、实参、目标 place 与正常/展开出口"
    )]
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
        let foreign = kind != g::CallKind::Managed;
        let mut arguments = Vec::new();
        let mut by_value = Vec::new();
        let mut foreign_arguments = Vec::new();
        let indirect_result = if foreign {
            self.c_layout(result_ty).memory
        } else {
            self.indirect_abi(result_ty)
        };
        let output = if indirect_result {
            let output = self.temporary(result_ty)?;
            arguments.push(output);
            Some(output)
        } else {
            None
        };
        let target = self.callee(callee, site, &mut arguments)?;
        for operand in operands {
            let value = self.operand(operand)?;
            if foreign {
                self.c_argument(value, &mut arguments, &mut foreign_arguments, &mut by_value)?;
                continue;
            }
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
            let lanes = if foreign {
                self.c_layout(result_ty).lanes
            } else {
                self.abi_lanes(result_ty)
            };
            lanes.into_iter().map(|(_, _, kind)| kind).collect()
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
            foreign_arguments,
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
        let result = if let Some(address) = output {
            Computed::Address {
                ty: result_ty,
                address,
            }
        } else if foreign {
            self.c_result(result_ty, &values)?
        } else {
            Computed::Values {
                ty: result_ty,
                values,
            }
        };
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
                .index
                .instance(&key)
                .map(|position| &self.mono.instances[position])
                && let Some(linkage) = self.module.linkage.iter().find(|linkage| {
                    linkage.definition.0 == instance.definition
                        && linkage.foreign.is_some()
                        && !linkage.naked
                })
                && let Some(name) = &linkage.export_name
            {
                return Ok(CallTarget::External {
                    key: self.module.definitions[instance.definition as usize].key,
                    name: name.clone(),
                });
            }
            if let Some(instance) = self
                .index
                .instance(&key)
                .map(|position| &self.mono.instances[position])
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
                                .as_ref()
                                .or(linkage.export_name.as_ref())
                                .cloned()
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
            g::Callee::StaticInit(definition) => {
                // 每个静态定义只有一个初始化器实例；读点守卫的 site 通常已命中，
                // 这里按定义唯一绑定，用于 site 未登记的内部读点。
                let mut candidates = self
                    .instance
                    .functions
                    .iter()
                    .filter(|callee| callee.definition == definition.0);
                match (candidates.next(), candidates.next()) {
                    (Some(binding), None) => Ok(CallTarget::Instance(binding.instance)),
                    _ => Err(invalid("静态初始化器不能唯一绑定实例")),
                }
            }
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
        match reason {
            g::SuspendReason::Yield => {
                self.runtime(RuntimeCall::Yield, &[], &[])?;
                self.jump(self.target(resume));
            }
            g::SuspendReason::ChanSend { channel, value } => {
                let status =
                    self.channel_transfer(RuntimeCall::ChannelSend, channel, Some(value), None)?;
                let resume_block = self.target(resume);
                let cancel = self.target(cancelled.ok_or_else(|| invalid("send 缺少关闭路径"))?);
                self.branch(status, resume_block, cancel);
            }
            g::SuspendReason::ChanRecv { channel } => {
                let place = destination.ok_or_else(|| invalid("recv 缺少结果槽"))?;
                self.channel_result(RuntimeCall::ChannelReceive, channel, place)?;
                self.jump(self.target(resume));
            }
            g::SuspendReason::JoinWait { join } => {
                let place = destination.ok_or_else(|| invalid("wait 缺少结果槽"))?;
                self.channel_result(RuntimeCall::JoinWait, join, place)?;
                self.jump(self.target(resume));
            }
        }
        Ok(())
    }

    /// `channel_send` / `try_*` 的固定车道：句柄、载荷地址、类型描述符，返回 `byte` 状态。
    ///
    /// 状态 1 表示成功，0 表示 Full/Empty 或取消，2 表示 Closed。
    pub(super) fn channel_transfer(
        &mut self,
        target: RuntimeCall,
        handle: &g::Operand,
        payload: Option<&g::Operand>,
        out: Option<ValueId>,
    ) -> Result<ValueId, Diagnostic> {
        let handle_value = self.operand(handle)?;
        let handle = self.uint_bits(handle_value)?;
        let (pointer, desc) = if let Some(payload) = payload {
            let value = self.operand(payload)?;
            let ty = value.ty();
            let address = self.computed_address(value)?;
            let pointer = self.raw_pointer(address);
            // 发送描述符是载荷类型的受管车道表：环槽登记与固定访客都按它取根。
            let desc = self.payload_root_table(ty);
            (pointer, desc)
        } else if let Some(out) = out {
            let pointer = self.raw_pointer(out);
            let zero = self.constant(0, Type::I64);
            (pointer, self.int_to_raw(zero))
        } else {
            let zero = self.constant(0, Type::I64);
            let null = self.int_to_raw(zero);
            (null, null)
        };
        let status = self.runtime(
            target,
            &[handle, pointer, desc],
            &[ValueType::scalar(Type::I8)],
        )?;
        Ok(status[0])
    }

    /// 接收或 `Join.wait`：runtime 把成功载荷写入临时槽，编译器再装成 `Result`。
    fn channel_result(
        &mut self,
        target: RuntimeCall,
        handle: &g::Operand,
        place: g::Place,
    ) -> Result<(), Diagnostic> {
        let result_ty = self.local_ty(place.local);
        let payload_ty = self.ok_payload(result_ty)?;
        let payload = match payload_ty {
            Some(ty)
                if self
                    .layout(ty)
                    .layout
                    .is_some_and(|layout| layout.size != 0) =>
            {
                Some(self.temporary(ty)?)
            }
            _ => None,
        };
        let handle_value = self.operand(handle)?;
        let handle_bits = self.uint_bits(handle_value)?;
        let pointer = if let Some(address) = payload {
            self.raw_pointer(address)
        } else {
            let zero = self.constant(0, Type::I64);
            self.int_to_raw(zero)
        };
        let desc = if let Some(ty) = payload_ty {
            let desc = self.descriptor(ty);
            self.raw_pointer(desc)
        } else {
            let zero = self.constant(0, Type::I64);
            self.int_to_raw(zero)
        };
        let status = self.runtime(
            target,
            &[handle_bits, pointer, desc],
            &[ValueType::scalar(Type::I8)],
        )?;
        // `Join.wait` 的 panic 完成：状态 2 时 Panic 句柄由单独车道取回（非 panic 返回 0）。
        let panic = if matches!(target, RuntimeCall::JoinWait) {
            Some(
                self.runtime(
                    RuntimeCall::JoinPanic,
                    &[handle_bits],
                    &[ValueType::scalar(Type::I64)],
                )?[0],
            )
        } else {
            None
        };
        let address = self.materialize_status(result_ty, payload_ty, payload, status[0], panic)?;
        self.write_place(
            place,
            Computed::Address {
                ty: result_ty,
                address,
            },
        )?;
        Ok(())
    }

    /// `Result` 的 Ok 变体载荷；Ok 没有字段时返回 `None`。
    pub(super) fn ok_payload(&self, result_ty: u32) -> Result<Option<u32>, Diagnostic> {
        let TypeKind::Aggregate { variants, .. } = self.kind(result_ty) else {
            return Ok(Some(result_ty));
        };
        Ok(variants
            .first()
            .and_then(|fields| fields.first())
            .map(|field| field.ty))
    }

    /// 状态 1 装 Ok，其余装 Err；`panic` 非空时把 panic 句柄写入 Err 载荷字段。
    pub(super) fn materialize_status(
        &mut self,
        result_ty: u32,
        payload_ty: Option<u32>,
        payload: Option<ValueId>,
        status: ValueId,
        panic: Option<ValueId>,
    ) -> Result<ValueId, Diagnostic> {
        let address = self.temporary(result_ty)?;
        self.zero_object(address, result_ty)?;
        let success = self.constant(1, Type::I8);
        let ok = self.compare_value(Condition::Eq, false, status, success);
        let tag_bytes = match self.kind(result_ty) {
            TypeKind::Aggregate { tag_bytes, .. } if *tag_bytes != 0 => Some(*tag_bytes),
            _ => None,
        };
        if let Some(tag_bytes) = tag_bytes {
            let width = super::storage::integer_type(u16::from(tag_bytes) * 8);
            let ok_tag = self.constant(0, width);
            let err_tag = self.constant(1, width);
            let tag = self.emit_one(
                Op::Select,
                &[ok, ok_tag, err_tag],
                ValueType::scalar(width),
                Origin::None,
            );
            self.store(address, tag, u32::from(tag_bytes), false);
        }
        if let (Some(payload), Some(payload_ty)) = (payload, payload_ty) {
            let fields = self.fields(result_ty, 0, 1)?;
            let (offset, _) = fields
                .first()
                .copied()
                .ok_or_else(|| invalid("Result 缺少 Ok 载荷"))?;
            let destination = self.offset(address, offset);
            self.copy_memory(destination, payload, payload_ty)?;
        }
        if let Some(panic) = panic {
            // panic 完成时 runtime 不写正常载荷，临时槽是未初始化字节：Err 分支写 Panic 句柄，
            // Ok 分支保持刚复制的载荷原样（把读回的值原样写回即不生效的写）。
            let fields = self.fields(result_ty, 1, 1)?;
            let (offset, _) = fields
                .first()
                .copied()
                .ok_or_else(|| invalid("Result 缺少 Err 载荷"))?;
            let slot = self.offset(address, offset);
            let current = self.load(slot, ValueType::scalar(Type::I64), 8, false);
            let merged = self.emit_one(
                Op::Select,
                &[ok, current, panic],
                ValueType::scalar(Type::I64),
                Origin::None,
            );
            self.store(slot, merged, 8, false);
        }
        Ok(address)
    }

    /// 把结果对象清零，避免判别字的高位残留导致 match 落入不可达陷阱。
    pub(super) fn zero_object(&mut self, address: ValueId, ty: u32) -> Result<(), Diagnostic> {
        let size = self
            .layout(ty)
            .layout
            .ok_or_else(|| invalid("结果缺少布局"))?
            .size;
        let zero = self.constant(0, Type::I64);
        let mut offset = 0u64;
        while offset + 8 <= size {
            let slot = self.offset(address, offset);
            self.store(slot, zero, 8, false);
            offset += 8;
        }
        if offset < size {
            let slot = self.offset(address, offset);
            let narrow = self.constant(0, Type::I8);
            self.store(slot, narrow, 1, false);
        }
        Ok(())
    }

    /// 指针降成裸指针，整数先变成裸指针。glue 形参统一是 `*()`。
    pub(super) fn raw_pointer(&mut self, value: ValueId) -> ValueId {
        let kind = self.machine_type(value);
        if kind.provenance == Some(Provenance::Raw) && kind.ty == Type::Ptr {
            return value;
        }
        if kind.ty == Type::Ptr {
            return self.emit_one(
                Op::Convert(Conversion::PointerCast),
                &[value],
                ValueType::pointer(Provenance::Raw),
                Origin::Derived(value),
            );
        }
        self.int_to_raw(value)
    }

    pub(super) fn int_to_raw(&mut self, value: ValueId) -> ValueId {
        self.emit_one(
            Op::Convert(Conversion::IntToPointer),
            &[value],
            ValueType::pointer(Provenance::Raw),
            Origin::None,
        )
    }

    pub(super) fn pointer_bits(&mut self, pointer: ValueId) -> ValueId {
        if self.machine_type(pointer).ty == Type::Ptr {
            self.emit_one(
                Op::Convert(Conversion::PointerToInt),
                &[pointer],
                ValueType::scalar(Type::I64),
                Origin::None,
            )
        } else {
            pointer
        }
    }

    /// 通道/Join 句柄或指针收成 `uint` 车道。
    pub(super) fn uint_bits(&mut self, value: Computed) -> Result<ValueId, Diagnostic> {
        let mut values = self.computed_values(value)?;
        let bits = values.pop().ok_or_else(|| invalid("句柄没有机器车道"))?;
        if self.machine_type(bits).ty == Type::Ptr {
            Ok(self.emit_one(
                Op::Convert(Conversion::PointerToInt),
                &[bits],
                ValueType::scalar(Type::I64),
                Origin::None,
            ))
        } else {
            Ok(bits)
        }
    }

    fn select(
        &mut self,
        cases: std::ops::Range<u32>,
        index: g::LocalId,
        ready: g::BlockId,
        suspend: Option<g::BlockId>,
        cancelled: Option<g::BlockId>,
    ) -> Result<(), Diagnostic> {
        let count = cases.end - cases.start;
        let records = if count == 0 {
            let zero = self.constant(0, Type::I64);
            self.int_to_raw(zero)
        } else {
            let bytes = u64::from(count) * SELECT_RECORD_BYTES;
            let base = self.temporary_bytes(bytes, 8, [0; 32], Vec::new())?;
            for (slot, case) in self.gir.select_cases[crate::body::range(&cases)]
                .iter()
                .enumerate()
            {
                self.store_select_record(base, slot, case)?;
            }
            self.raw_pointer(base)
        };
        let has_default = self.constant(u64::from(suspend.is_none()), Type::I8);
        let count_value = self.constant(u64::from(count), Type::I64);
        let values = self.runtime(
            RuntimeCall::SelectCommit {
                cases: count,
                has_default: suspend.is_none(),
            },
            &[records, count_value, has_default],
            &[ValueType::scalar(Type::I64), ValueType::scalar(Type::I64)],
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

    /// 一条 select case 占 8 个机器字：操作、句柄、载荷、载荷描述符、结果对象、元素描述符、字段偏移、关闭标记。
    fn store_select_record(
        &mut self,
        base: ValueId,
        slot: usize,
        case: &g::SelectCase,
    ) -> Result<(), Diagnostic> {
        let origin = u64::try_from(slot).expect("select 下标") * SELECT_RECORD_BYTES;
        let (op, handle, payload) = match &case.operation {
            g::SelectOperation::Send { channel, value } => (0, channel, Some(value.clone())),
            g::SelectOperation::Recv { channel } => (1, channel, None),
            g::SelectOperation::Wait { join } => (2, join, None),
        };
        let op_word = self.constant(op, Type::I64);
        self.store_record_word(base, origin, op_word);
        let handle_value = self.operand(handle)?;
        let handle_bits = self.uint_bits(handle_value)?;
        self.store_record_word(base, origin + 8, handle_bits);
        if let Some(payload) = payload {
            let value = self.operand(&payload)?;
            let ty = value.ty();
            let address = self.computed_address(value)?;
            let address_bits = self.pointer_bits(address);
            self.store_record_word(base, origin + 16, address_bits);
            // 发送载荷的描述符是受管车道表：select 提交路径按它登记环槽。
            let desc = self.payload_root_table(ty);
            let desc_bits = self.pointer_bits(desc);
            self.store_record_word(base, origin + 24, desc_bits);
        } else {
            let zero = self.constant(0, Type::I64);
            self.store_record_word(base, origin + 16, zero);
            let zero = self.constant(0, Type::I64);
            self.store_record_word(base, origin + 24, zero);
        }
        if let Some(destination) = case.destination {
            let (pointer, ty) = self.address(destination)?;
            let element = self.ok_payload(ty)?.unwrap_or(ty);
            let field = self
                .fields(ty, 0, 1)
                .ok()
                .and_then(|fields| fields.first().copied())
                .map(|(offset, _)| offset)
                .unwrap_or(0);
            let result_bits = self.pointer_bits(pointer);
            self.store_record_word(base, origin + 32, result_bits);
            let desc = self.descriptor(element);
            let desc_bits = self.pointer_bits(desc);
            self.store_record_word(base, origin + 40, desc_bits);
            let field_word = self.constant(field, Type::I64);
            self.store_record_word(base, origin + 48, field_word);
            let closed = self.constant(u64::from(op == 1), Type::I64);
            self.store_record_word(base, origin + 56, closed);
        } else {
            for word in [32, 40, 48, 56] {
                let zero = self.constant(0, Type::I64);
                self.store_record_word(base, origin + word, zero);
            }
        }
        Ok(())
    }

    fn store_record_word(&mut self, base: ValueId, offset: u64, value: ValueId) {
        let slot = self.offset(base, offset);
        self.store(slot, value, 8, false);
    }

    /// `#[track_caller]` owner 的末尾三个 Argument local 是调用点隐藏位置实参
    /// `(file, line, column)`；非 track_caller owner 返回 `None`，panic 退到常量路径。
    fn caller_location_params(&self) -> Option<[g::LocalId; 3]> {
        if self.gir.flags & g::BodyFlags::TRACK_CALLER == 0 {
            return None;
        }
        let mut hidden: Vec<g::LocalId> = self
            .gir
            .locals
            .iter()
            .enumerate()
            .filter(|(_, local)| local.kind == g::LocalKind::Argument)
            .map(|(index, _)| g::LocalId(index as u32))
            .rev()
            .take(3)
            .collect();
        hidden.reverse();
        <[g::LocalId; 3]>::try_from(hidden).ok()
    }
}

/// select case 记录的字节数：8 个 `uint`。
const SELECT_RECORD_BYTES: u64 = 64;
