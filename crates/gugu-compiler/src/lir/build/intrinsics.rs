use super::values::Computed;
use super::{Builder, Diagnostic, TypeKind, invalid};
use crate::frontend::gir::placement::PlacementKind;
use crate::frontend::{gir::body as g, hir, mono};
use crate::lir::body::{
    Conversion, IntOp, Op, Origin, Provenance, RuntimeCall, Symbol, Type, ValueId, ValueType, id,
    range,
};
use crate::lir::invalid_resource;

impl Builder<'_> {
    pub(super) fn intrinsic(
        &mut self,
        op: &g::IntrinsicOp,
        operands: &[g::Operand],
        types: &[hir::TypeId],
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        match op {
            g::IntrinsicOp::SizeOf | g::IntrinsicOp::AlignOf | g::IntrinsicOp::OffsetOf { .. } => {
                let input = types
                    .first()
                    .ok_or_else(|| invalid("布局 intrinsic 缺少具体类型"))?
                    .0;
                let layout = self
                    .layout(input)
                    .layout
                    .ok_or_else(|| invalid("布局 intrinsic 的类型尚未具体化"))?;
                let number = match op {
                    g::IntrinsicOp::SizeOf => layout.size,
                    g::IntrinsicOp::AlignOf => layout.align,
                    g::IntrinsicOp::OffsetOf { field } => {
                        self.fields(input, 0, 0)?
                            .get(usize::try_from(*field).expect("字段编号"))
                            .ok_or_else(|| invalid("offset_of 字段越界"))?
                            .0
                    }
                    _ => unreachable!(),
                };
                Ok(Computed::Values {
                    ty,
                    values: vec![self.constant(number, Type::I64)],
                })
            }
            g::IntrinsicOp::TypeId => {
                if let Some(input) = types.first() {
                    let value = self.emit_one(
                        Op::SymbolAddr(Symbol::TypeId(self.layout(input.0).key)),
                        &[],
                        ValueType::scalar(Type::I32),
                        Origin::None,
                    );
                    Ok(Computed::Values {
                        ty,
                        values: vec![value],
                    })
                } else {
                    let value = self.operand(
                        operands
                            .first()
                            .ok_or_else(|| invalid("TypeId 转整数缺少输入"))?,
                    )?;
                    let values = self.computed_values(value)?;
                    let target = self
                        .scalar_lanes(ty)
                        .ok_or_else(|| invalid("TypeId 结果没有标量类型"))?[0]
                        .1
                        .ty;
                    Ok(Computed::Values {
                        ty,
                        values: vec![self.integer_resize(values[0], target, false)],
                    })
                }
            }
            g::IntrinsicOp::TypeName => {
                if let Some(input) = types.first() {
                    let record = self.mono.universe.record(&self.layout(input.0).key)?;
                    self.literal(&g::ConstValue::String(record.name.clone()), ty)
                } else {
                    // 运行时 `TypeId.name()`：按稠密编号查 type section 的 name pool。
                    let id = self.operand(
                        operands
                            .first()
                            .ok_or_else(|| invalid("TypeId 名称缺少输入"))?,
                    )?;
                    let id = self.computed_values(id)?[0];
                    let id = self.integer_resize(id, Type::I64, false);
                    let records = self.emit_one(
                        Op::SymbolAddr(Symbol::TypeRecords),
                        &[],
                        ValueType::pointer(Provenance::Metadata),
                        Origin::Symbol,
                    );
                    let stride = self.constant(80, Type::I64);
                    let offset = self.emit_one(
                        Op::Integer(IntOp::Mul),
                        &[id, stride],
                        ValueType::scalar(Type::I64),
                        Origin::None,
                    );
                    let record = self.dynamic_offset(records, offset);
                    let name_offset = self.offset(record, 16);
                    let name_offset =
                        self.load(name_offset, ValueType::scalar(Type::I32), 4, false);
                    let name_offset = self.integer_resize(name_offset, Type::I64, false);
                    let name_len = self.offset(record, 20);
                    let name_len = self.load(name_len, ValueType::scalar(Type::I32), 4, false);
                    let name_len = self.integer_resize(name_len, Type::I64, false);
                    let names = self.emit_one(
                        Op::SymbolAddr(Symbol::TypeNames),
                        &[],
                        ValueType::pointer(Provenance::GcHeap),
                        Origin::Symbol,
                    );
                    let pointer = self.dynamic_offset(names, name_offset);
                    let address = self.temporary(ty)?;
                    self.store(address, pointer, 8, false);
                    let end = self.offset(address, 8);
                    self.store(end, name_len, 8, false);
                    Ok(Computed::Address { ty, address })
                }
            }
            g::IntrinsicOp::StaticRef(definition) => {
                let declaration = &self.module.definitions[definition.index()];
                let thread_local = self.module.initialization.iter().any(|init| {
                    init.definition == *definition
                        && matches!(
                            init.domain,
                            hir::StorageDomain::Coroutine | hir::StorageDomain::OsThread
                        )
                });
                let pointer = self.emit_one(
                    Op::SymbolAddr(Symbol::Global {
                        key: declaration.key,
                        thread_local,
                    }),
                    &[],
                    ValueType::pointer(Provenance::Foreign),
                    Origin::Symbol,
                );
                Ok(Computed::Values {
                    ty,
                    values: vec![pointer],
                })
            }
            g::IntrinsicOp::PtrRead | g::IntrinsicOp::ReadUnaligned => {
                let pointer = self.operand(&operands[0])?;
                let pointer = self.computed_values(pointer)?[0];
                if let Some(lanes) = self.scalar_lanes(ty) {
                    let values = lanes
                        .into_iter()
                        .map(|(offset, kind)| {
                            let pointer = self.offset(pointer, offset);
                            let align = if matches!(op, g::IntrinsicOp::ReadUnaligned) {
                                1
                            } else {
                                u32::try_from(kind.ty.bytes().expect("读宽度")).expect("读对齐")
                            };
                            self.load(pointer, kind, align, false)
                        })
                        .collect();
                    Ok(Computed::Values { ty, values })
                } else {
                    Ok(Computed::Address {
                        ty,
                        address: pointer,
                    })
                }
            }
            g::IntrinsicOp::PtrWrite | g::IntrinsicOp::WriteUnaligned => {
                let pointer = self.operand(&operands[0])?;
                let pointer = self.computed_values(pointer)?[0];
                let value = self.operand(&operands[1])?;
                if matches!(op, g::IntrinsicOp::WriteUnaligned) {
                    let values = self.computed_values(value)?;
                    for (index, value) in values.into_iter().enumerate() {
                        let pointer =
                            self.offset(pointer, u64::try_from(index).expect("piece 编号") * 8);
                        self.store(pointer, value, 1, false);
                    }
                } else {
                    self.store_computed(pointer, value.ty(), value)?;
                }
                Ok(Computed::Values {
                    ty,
                    values: Vec::new(),
                })
            }
            g::IntrinsicOp::UninitAsPtr | g::IntrinsicOp::UninitWrite => {
                let value = self.operand(&operands[0])?;
                let pointer = self.computed_address(value)?;
                if matches!(op, g::IntrinsicOp::UninitWrite) {
                    let value = self.operand(&operands[1])?;
                    self.store_computed(pointer, value.ty(), value)?;
                    return Ok(Computed::Values {
                        ty,
                        values: Vec::new(),
                    });
                }
                let target = self
                    .scalar_lanes(ty)
                    .ok_or_else(|| invalid("MaybeUninit 指针结果缺少机器类型"))?;
                let pointer = if target[0].1.provenance == Some(Provenance::Raw) {
                    self.emit_one(
                        Op::Convert(Conversion::PointerCast),
                        &[pointer],
                        target[0].1,
                        Origin::Derived(pointer),
                    )
                } else {
                    pointer
                };
                Ok(Computed::Values {
                    ty,
                    values: vec![pointer],
                })
            }
            g::IntrinsicOp::Subslice { .. } => self.subslice(operands, ty),
            g::IntrinsicOp::PackSlice => {
                let element = types
                    .first()
                    .ok_or_else(|| invalid("变参尾缺少元素类型"))?
                    .0;
                self.pack_slice(element, operands, ty)
            }
            g::IntrinsicOp::Is | g::IntrinsicOp::Downcast | g::IntrinsicOp::DowncastCopy => {
                let call = match op {
                    g::IntrinsicOp::Is => RuntimeCall::TypeIs,
                    g::IntrinsicOp::Downcast => RuntimeCall::Downcast,
                    _ => RuntimeCall::DowncastCopy,
                };
                let mut args = Vec::new();
                for operand in operands {
                    let value = self.operand(operand)?;
                    args.extend(self.computed_values(value)?);
                }
                for target in types {
                    args.push(self.descriptor(target.0));
                }
                self.runtime_result(call, args, ty)
            }
            g::IntrinsicOp::ChanNew | g::IntrinsicOp::ChanClose => {
                let target = if matches!(op, g::IntrinsicOp::ChanNew) {
                    RuntimeCall::ChannelNew
                } else {
                    RuntimeCall::ChannelClose
                };
                let mut args = Vec::new();
                for operand in operands {
                    let value = self.operand(operand)?;
                    args.extend(self.computed_values(value)?);
                }
                args.push(self.descriptor(ty));
                self.runtime_result(target, args, ty)
            }
            g::IntrinsicOp::Spawn(g::SpawnTarget::Body(definition)) => {
                self.closure(*definition, ty, true)
            }
            g::IntrinsicOp::Spawn(g::SpawnTarget::Callee(callee)) => {
                let mut args = Vec::new();
                let target = match callee {
                    g::Callee::Value(operand) => {
                        let value = self.operand(operand)?;
                        let source = value.ty();
                        if let TypeKind::FunctionItem { definition, .. } = self.kind(source) {
                            Some(self.function_key(*definition, source)?)
                        } else {
                            args.extend(self.computed_values(value)?);
                            None
                        }
                    }
                    g::Callee::Dispatch(dispatch) => {
                        let function = self.owner.dispatches
                            [usize::try_from(*dispatch).expect("dispatch 编号")]
                        .function
                        .ok_or_else(|| invalid("spawn dispatch 缺少函数"))?;
                        let instance = self
                            .instance
                            .functions
                            .iter()
                            .find(|callee| callee.definition == function.0)
                            .ok_or_else(|| invalid("spawn 函数没有闭合实例"))?;
                        Some(instance.instance)
                    }
                    _ => return Err(invalid("spawn 目标必须有明确代码身份")),
                };
                if let Some(target) = target {
                    args.push(self.emit_one(
                        Op::SymbolAddr(Symbol::Instance(target)),
                        &[],
                        ValueType::pointer(Provenance::Code),
                        Origin::Symbol,
                    ));
                }
                for operand in operands.iter().skip(1) {
                    let value = self.operand(operand)?;
                    args.extend(self.computed_values(value)?);
                }
                self.runtime_result(RuntimeCall::Spawn, args, ty)
            }
            g::IntrinsicOp::Format { parts } => {
                let metadata = serde_json::to_vec(&self.owner.string_parts[range(parts)])
                    .expect("格式计划可序列化");
                let mut args = vec![self.data(metadata, Provenance::Metadata)];
                for operand in operands {
                    let value = self.operand(operand)?;
                    let ty = value.ty();
                    args.push(self.computed_address(value)?);
                    args.push(self.descriptor(ty));
                }
                self.runtime_result(RuntimeCall::Format, args, ty)
            }
            g::IntrinsicOp::Asm(index) => self.inline_asm(*index, operands, ty),
            g::IntrinsicOp::DeferChainEmpty => {
                let value = self.emit_one(
                    Op::IConst(0),
                    &[],
                    ValueType::pointer(Provenance::GcHeap),
                    Origin::None,
                );
                Ok(Computed::Values {
                    ty,
                    values: vec![value],
                })
            }
            g::IntrinsicOp::DeferChainPush { action }
            | g::IntrinsicOp::DeferChainEnv { action } => {
                let target = if matches!(op, g::IntrinsicOp::DeferChainPush { .. }) {
                    RuntimeCall::DeferPush
                } else {
                    RuntimeCall::DeferEnvironment
                };
                let mut args = vec![self.constant(u64::from(*action), Type::I32)];
                for operand in operands {
                    let value = self.operand(operand)?;
                    args.extend(self.computed_values(value)?);
                }
                self.runtime_result(target, args, ty)
            }
            g::IntrinsicOp::DeferChainAction | g::IntrinsicOp::DeferChainPop => {
                let target = if matches!(op, g::IntrinsicOp::DeferChainAction) {
                    RuntimeCall::DeferAction
                } else {
                    RuntimeCall::DeferPop
                };
                let mut args = Vec::new();
                for operand in operands {
                    let value = self.operand(operand)?;
                    args.extend(self.computed_values(value)?);
                }
                self.runtime_result(target, args, ty)
            }
        }
    }

    pub(super) fn runtime_result(
        &mut self,
        call: RuntimeCall,
        mut args: Vec<ValueId>,
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        if self.indirect_abi(ty) {
            let address = self.temporary(ty)?;
            args.insert(0, address);
            self.runtime(call, &args, &[])?;
            Ok(Computed::Address { ty, address })
        } else {
            let mut kinds: Vec<_> = self
                .abi_lanes(ty)
                .into_iter()
                .map(|(_, _, kind)| kind)
                .collect();
            if matches!(call, RuntimeCall::DeferPush | RuntimeCall::DeferPop) {
                kinds = vec![ValueType::pointer(Provenance::GcHeap)];
            }
            let values = self.runtime(call, &args, &kinds)?;
            Ok(Computed::Values { ty, values })
        }
    }

    pub(super) fn length(&mut self, value: Computed) -> Result<ValueId, Diagnostic> {
        let ty = value.ty();
        if let Some(length) = self.static_length(ty) {
            return Ok(self.constant(length, Type::I64));
        }
        match value {
            Computed::Values { values, .. } if values.len() == 2 => Ok(values[1]),
            Computed::Address { address, .. } => {
                let length = self.offset(address, 8);
                Ok(self.load(length, ValueType::scalar(Type::I64), 8, false))
            }
            _ => Err(invalid("len 输入没有固定长度或 fat pointer")),
        }
    }

    /// 齐次变参尾：把 operands 顺序写入栈槽，返回 `&[T]` 胖指针。
    fn pack_slice(
        &mut self,
        element: u32,
        operands: &[g::Operand],
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let layout = self
            .layout(element)
            .layout
            .ok_or_else(|| invalid("变参尾元素缺少布局"))?;
        let count = u64::try_from(operands.len()).expect("实参数量适配 u64");
        let bytes = layout
            .size
            .checked_mul(count)
            .ok_or_else(|| invalid("变参尾字节数溢出"))?;
        let mut roots = Vec::new();
        for (offset, provenance) in self.roots(element) {
            for index in 0..count {
                roots.push((index * layout.size + offset, provenance));
            }
        }
        let address = self.temporary_bytes(bytes, layout.align, self.layout(element).key, roots)?;
        for (index, operand) in operands.iter().enumerate() {
            let value = self.operand(operand)?;
            let pointer = self.offset(
                address,
                u64::try_from(index).expect("实参编号") * layout.size,
            );
            self.store_computed(pointer, element, value)?;
        }
        let length = self.constant(count, Type::I64);
        Ok(Computed::Values {
            ty,
            values: vec![address, length],
        })
    }

    fn subslice(&mut self, operands: &[g::Operand], ty: u32) -> Result<Computed, Diagnostic> {
        let base = self.operand(&operands[0])?;
        let source_ty = base.ty();
        let pointer = match base {
            Computed::Values { values, .. } => values[0],
            Computed::Address { address, .. }
                if matches!(self.kind(source_ty), TypeKind::Array { .. }) =>
            {
                address
            }
            Computed::Address { address, .. } => self.load(
                address,
                ValueType::pointer(Provenance::GcInterior),
                8,
                false,
            ),
        };
        let start = self.operand(&operands[1])?;
        let start = self.computed_values(start)?[0];
        let end = self.operand(&operands[2])?;
        let end = self.computed_values(end)?[0];
        let start = self.integer_resize(start, Type::I64, false);
        let end = self.integer_resize(end, Type::I64, false);
        let element_size = self.slice_element_size(source_ty)?;
        let size = self.constant(element_size, Type::I64);
        let offset = self.emit_one(
            Op::Integer(IntOp::Mul),
            &[start, size],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        let pointer = self.dynamic_offset(pointer, offset);
        let length = self.emit_one(
            Op::Integer(IntOp::Sub),
            &[end, start],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        Ok(Computed::Values {
            ty,
            values: vec![pointer, length],
        })
    }

    fn slice_element_size(&self, ty: u32) -> Result<u64, Diagnostic> {
        match self.kind(ty) {
            TypeKind::String => Ok(1),
            TypeKind::Array { element, .. } | TypeKind::Slice(element) => Ok(self
                .layout(*element)
                .layout
                .ok_or_else(|| invalid("切片元素缺少布局"))?
                .size),
            TypeKind::Reference(inner) => self.slice_element_size(*inner),
            _ => Err(invalid("子切片输入不是序列")),
        }
    }

    pub(super) fn utf8_check(&mut self, check: u32) -> Result<ValueId, Diagnostic> {
        let expression = self.owner.checks[usize::try_from(check).expect("检查编号")].expression;
        let hir::ExprKind::Slice { base, start, end } =
            self.owner.expressions[expression.index()].kind
        else {
            return Err(invalid("UTF-8 检查缺少切片表达式"));
        };
        let local = self.gir.expression_locals[base.index()]
            .ok_or_else(|| invalid("字符串检查缺少已求值 base"))?;
        let base = self.read_place(g::Place::local(local))?;
        let mut args = self.computed_values(base)?;
        for endpoint in [start, end].into_iter().flatten() {
            let local = self.gir.expression_locals[endpoint.index()]
                .ok_or_else(|| invalid("字符串检查缺少已求值端点"))?;
            args.extend(self.local_values(local)?);
        }
        Ok(self.runtime(
            RuntimeCall::Utf8Boundary,
            &args,
            &[ValueType::scalar(Type::I8)],
        )?[0])
    }

    pub(super) fn allocate_array(
        &mut self,
        element: u32,
        length: &g::Operand,
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let value = self.operand(length)?;
        let length = self.computed_values(value)?[0];
        let size = self
            .layout(element)
            .layout
            .ok_or_else(|| invalid("动态数组缺少元素布局"))?
            .size;
        let size = self.constant(size, Type::I64);
        let bytes = self.emit_one(
            Op::Integer(IntOp::Mul),
            &[length, size],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        let descriptor = self.layout(element).key;
        let align = u32::try_from(self.layout(element).layout.expect("元素布局").align)
            .map_err(|_| invalid("数组对齐越界"))?;
        let allocation = self.next_allocation;
        self.next_allocation += 1;
        let placement = if self.layout(element).passing.has_resource() {
            PlacementKind::Resource
        } else {
            PlacementKind::LocalHeap
        };
        let pointer = self.emit_one(
            Op::GcAlloc {
                descriptor,
                align,
                placement,
            },
            &[bytes],
            ValueType::pointer(Provenance::GcHeap),
            Origin::Allocation(allocation),
        );
        Ok(Computed::Values {
            ty,
            values: vec![pointer, length],
        })
    }

    pub(super) fn closure(
        &mut self,
        definition: hir::DefId,
        ty: u32,
        spawn: bool,
    ) -> Result<Computed, Diagnostic> {
        let environment = self.capture_environment(definition)?;
        if !spawn {
            return Ok(Computed::Values {
                ty,
                values: if self.layout(ty).layout.expect("闭包布局").size == 0 {
                    Vec::new()
                } else {
                    vec![environment]
                },
            });
        }
        let instance = self
            .instance
            .functions
            .iter()
            .find(|callee| callee.definition == definition.0)
            .ok_or_else(|| invalid("协程缺少闭合实例"))?;
        let code = self.emit_one(
            Op::SymbolAddr(Symbol::Instance(instance.instance)),
            &[],
            ValueType::pointer(Provenance::Code),
            Origin::Symbol,
        );
        self.runtime_result(RuntimeCall::Spawn, vec![code, environment], ty)
    }

    pub(super) fn capture_environment(
        &mut self,
        definition: hir::DefId,
    ) -> Result<ValueId, Diagnostic> {
        let owner = self
            .module
            .owners
            .iter()
            .find(|owner| owner.definition == definition)
            .ok_or_else(|| invalid("闭包没有冻结 owner"))?;
        if owner.captures.is_empty() {
            return Ok(self.emit_one(
                Op::IConst(0),
                &[],
                ValueType::pointer(Provenance::GcHeap),
                Origin::None,
            ));
        }
        let mut captures = Vec::with_capacity(owner.captures.len());
        for capture in &owner.captures {
            let local = if capture.owner == self.owner.definition {
                capture.source
            } else {
                self.owner
                    .captures
                    .iter()
                    .find(|outer| outer.owner == capture.owner && outer.source == capture.source)
                    .ok_or_else(|| invalid("嵌套 capture 没有传递槽"))?
                    .local
            };
            let local = self
                .gir
                .locals
                .iter()
                .position(|candidate| candidate.hir_local == Some(local))
                .ok_or_else(|| invalid("capture 源槽没有 GIR 身份"))?;
            let local_id = g::LocalId(id(local));
            if self.layout(self.local_ty(local_id)).passing.has_resource() {
                return Err(invalid_resource(
                    "Resource 值不能捕获到 managed closure environment",
                ));
            }
            captures.push(self.address(g::Place::local(local_id))?.0);
        }
        let bytes = u64::try_from(captures.len()).expect("捕获数量") * 8;
        let descriptor = mono::keys::hash_domain(
            "gugu-capture-environment-v1",
            &self.module.definitions[definition.index()].key,
        );
        self.body.environments.push(crate::lir::body::Environment {
            descriptor,
            bytes,
            roots: captures
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    (
                        u64::try_from(index).expect("捕获编号") * 8,
                        self.machine_type(*value).provenance.expect("捕获槽为指针"),
                    )
                })
                .collect(),
        });
        let size = self.constant(bytes, Type::I64);
        let allocation = self.next_allocation;
        self.next_allocation += 1;
        let environment = self.emit_one(
            Op::GcAlloc {
                descriptor,
                align: 8,
                placement: PlacementKind::LocalHeap,
            },
            &[size],
            ValueType::pointer(Provenance::GcHeap),
            Origin::Allocation(allocation),
        );
        let zero = self.constant(0, Type::I8);
        self.emit(Op::Memset { bytes }, &[environment, zero], &[]);
        for (index, pointer) in captures.into_iter().enumerate() {
            let destination = self.offset(environment, u64::try_from(index).expect("捕获编号") * 8);
            self.store(destination, pointer, 8, false);
        }
        Ok(environment)
    }

    pub(super) fn erase_function(
        &mut self,
        value: Computed,
        source_ty: u32,
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let TypeKind::FunctionItem {
            definition,
            capturing,
            ..
        } = self.kind(source_ty)
        else {
            return Err(invalid("函数擦除没有函数项"));
        };
        let (definition, capturing) = (*definition, *capturing);
        let key = self.function_key(definition, source_ty)?;
        let code = self.emit_one(
            Op::SymbolAddr(Symbol::Instance(key)),
            &[],
            ValueType::pointer(Provenance::Code),
            Origin::Symbol,
        );
        let environment = if capturing {
            self.computed_values(value)?[0]
        } else {
            self.emit_one(
                Op::IConst(0),
                &[],
                ValueType::pointer(Provenance::GcHeap),
                Origin::None,
            )
        };
        Ok(Computed::Values {
            ty,
            values: vec![code, environment],
        })
    }

    fn inline_asm(
        &mut self,
        index: u32,
        operands: &[g::Operand],
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let assembly = &self.owner.assembly[usize::try_from(index).expect("汇编编号")];
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        for (operand, constraint) in operands.iter().zip(&assembly.operands) {
            if constraint.direction == crate::frontend::semantics::assembly::Direction::In {
                let value = self.operand(operand)?;
                inputs.extend(self.computed_values(value)?);
            } else {
                let g::Operand::Copy(place) = operand else {
                    return Err(invalid("汇编输出没有 place"));
                };
                let output_ty = self.local_ty(place.local);
                outputs.push((*place, output_ty));
            }
        }
        let mut kinds: Vec<_> = outputs
            .iter()
            .flat_map(|(_, ty)| {
                self.abi_lanes(*ty)
                    .into_iter()
                    .map(|(_, _, kind)| (kind, Origin::None))
            })
            .collect();
        // 裸函数体只有这段汇编：返回值由模板按 ABI 写入返回寄存器。
        let naked =
            assembly.context == crate::frontend::semantics::assembly::AssemblyContext::Naked;
        if naked {
            let result = self.gir.signature.result.0;
            for (_, _, kind) in self.abi_lanes(result) {
                kinds.push((kind, Origin::None));
            }
        }
        let values = self.emit(Op::InlineAsm(index), &inputs, &kinds);
        let mut offset = 0;
        for (place, output_ty) in outputs {
            let count = self.abi_lanes(output_ty).len();
            self.write_place(
                place,
                Computed::Values {
                    ty: output_ty,
                    values: values[offset..offset + count].to_vec(),
                },
            )?;
            offset += count;
        }
        Ok(Computed::Values {
            ty,
            values: if naked {
                values[offset..].to_vec()
            } else {
                Vec::new()
            },
        })
    }
}
