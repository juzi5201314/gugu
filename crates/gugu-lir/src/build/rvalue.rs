use super::values::Computed;
use super::{Builder, Diagnostic, TypeKind, invalid};
use crate::body::{
    AtomicOp, Conversion, Op, Origin, Provenance, RuntimeCall, Type, ValueId, ValueType,
};
use crate::gir::body::{
    AggregateKind, AtomicOp as GirAtomic, CastKind, Operand, Place, ResourceActionKind, Rvalue,
    StatementKind, ValueActionKind, VolatileOp,
};
use crate::gir::placement::PlacementKind;
use std::num::NonZeroU32;

impl Builder<'_> {
    pub(super) fn statement(&mut self, statement: &StatementKind) -> Result<(), Diagnostic> {
        match statement {
            StatementKind::StorageLive(local) => self.live(*local)?,
            StatementKind::StorageDead(local) => self.dead(*local),
            StatementKind::Assign(place, value) => {
                let ty = if place.is_local() {
                    self.local_ty(place.local)
                } else {
                    self.address(*place)?.1
                };
                if matches!(
                    value,
                    Rvalue::Cast {
                        kind: CastKind::MaybeUninit,
                        ..
                    }
                ) {
                    return Ok(());
                }
                let value = self.rvalue(value, ty)?;
                self.write_place(*place, value)?;
            }
            StatementKind::SetDiscriminant { place, variant } => {
                let (pointer, ty) = self.address(*place)?;
                self.discriminant_store(pointer, ty, *variant)?;
            }
            StatementKind::ValueAction {
                action: ValueActionKind::Copy,
                ..
            } => {}
            StatementKind::ValueAction {
                action,
                place,
                descriptor,
            } => {
                if self.layout(descriptor.0).passing.is_pure_bits() {
                    return Ok(());
                }
                let pointer = self.address(*place)?.0;
                let descriptor = self.descriptor(descriptor.0);
                let call = match action {
                    ValueActionKind::Publish => RuntimeCall::ValuePublish,
                    ValueActionKind::Drop => RuntimeCall::ValueDrop,
                    ValueActionKind::Forget => RuntimeCall::ValueForget,
                    ValueActionKind::Copy => unreachable!(),
                };
                self.runtime(call, &[pointer, descriptor], &[])?;
            }
            StatementKind::ResourceAction {
                action,
                place,
                descriptor,
            } => {
                if !self.layout(descriptor.0).passing.has_resource() {
                    return Ok(());
                }
                let pointer = self.address(*place)?.0;
                let descriptor = self.descriptor(descriptor.0);
                let call = match action {
                    ResourceActionKind::AcquireLease => RuntimeCall::ResourceAcquire,
                    ResourceActionKind::ReleaseLease => RuntimeCall::ResourceRelease,
                    ResourceActionKind::Transfer => RuntimeCall::ResourceTransfer,
                    ResourceActionKind::Finalize => RuntimeCall::ResourceFinalize,
                };
                self.runtime(call, &[pointer, descriptor], &[])?;
            }
            StatementKind::GcWrite {
                destination, value, ..
            } => {
                let value = self.operand(value)?;
                self.write_place(*destination, value)?;
            }
            StatementKind::ScopedViewBegin {
                source,
                mode,
                token,
            } => {
                let address = self.address(*source)?.0;
                self.emit(
                    Op::ScopedViewBegin {
                        mode: *mode,
                        token: token.0,
                    },
                    &[address],
                    &[],
                );
            }
            StatementKind::ScopedViewEnd { token } => {
                self.emit(Op::ScopedViewEnd { token: token.0 }, &[], &[]);
            }
            StatementKind::PlatformCall {
                op,
                operands,
                destination,
            } => {
                let mut arguments = Vec::with_capacity(operands.len());
                for operand in operands {
                    let computed = self.operand(operand)?;
                    arguments.extend(self.computed_values(computed)?);
                }
                // 状态操作没有结果；查询返回一个标量，`entropy` 返回胖指针。
                let results: Vec<ValueType> = match destination {
                    Some(place) => self
                        .abi_lanes(self.local_ty(place.local))
                        .into_iter()
                        .map(|(_, _, kind)| kind)
                        .collect(),
                    None => Vec::new(),
                };
                let values = self.emit(
                    Op::PlatformCall(*op),
                    &arguments,
                    &results
                        .iter()
                        .map(|kind| (*kind, Origin::None))
                        .collect::<Vec<_>>(),
                );
                if let Some(place) = destination {
                    self.write_place(
                        *place,
                        Computed::Values {
                            ty: self.local_ty(place.local),
                            values,
                        },
                    )?;
                }
            }
            StatementKind::RuntimeMachineCall {
                op,
                operands,
                destination,
            } => {
                let mut arguments = Vec::with_capacity(operands.len());
                for operand in operands {
                    let computed = self.operand(operand)?;
                    arguments.extend(self.computed_values(computed)?);
                }
                // 机器原语的结果形状固定：`current_slot`/`report_write` 返回机器字，其余无结果。
                let results: Vec<ValueType> = match destination {
                    Some(place) => self
                        .abi_lanes(self.local_ty(place.local))
                        .into_iter()
                        .map(|(_, _, kind)| kind)
                        .collect(),
                    None => Vec::new(),
                };
                let values = self.emit(
                    Op::RuntimeMachineCall(*op),
                    &arguments,
                    &results
                        .iter()
                        .map(|kind| (*kind, Origin::None))
                        .collect::<Vec<_>>(),
                );
                if let Some(place) = destination {
                    self.write_place(
                        *place,
                        Computed::Values {
                            ty: self.local_ty(place.local),
                            values,
                        },
                    )?;
                }
            }
            StatementKind::SafepointPoll(_) => {
                self.emit(
                    Op::SafepointPoll {
                        interval: NonZeroU32::new(1).expect("常量非零"),
                    },
                    &[],
                    &[],
                );
            }
            StatementKind::StackCheck => {
                self.emit(Op::StackCheck, &[], &[]);
            }
            StatementKind::NoSafepointBegin(region) => {
                self.emit(Op::NoSafepointBegin(region.0), &[], &[]);
            }
            StatementKind::NoSafepointEnd(region) => {
                self.emit(Op::NoSafepointEnd(region.0), &[], &[]);
            }
            StatementKind::Atomic {
                op,
                ordering,
                pointer,
                operands,
                destination,
            } => {
                let mut args = Vec::new();
                if let Some(pointer) = pointer {
                    let value = self.operand(pointer)?;
                    args.extend(self.computed_values(value)?);
                }
                for operand in operands {
                    let value = self.operand(operand)?;
                    args.extend(self.computed_values(value)?);
                }
                let ty = destination.map(|place| self.local_ty(place.local));
                let mut results = ty.map_or_else(Vec::new, |ty| {
                    self.abi_lanes(ty)
                        .into_iter()
                        .map(|(_, _, kind)| (kind, Origin::None))
                        .collect()
                });
                if *op == GirAtomic::CompareExchange {
                    // CAS 的结果通道固定为 (前值, i8 成功标志)：目标聚合 ABI 把
                    // bool 槽升格成 8 字节，直接按不变量要求重定形。
                    let value = args
                        .get(1)
                        .map(|value| self.body.values[value.index()].kind)
                        .unwrap_or(ValueType::scalar(Type::I64));
                    results = vec![
                        (value, Origin::None),
                        (ValueType::scalar(Type::I8), Origin::None),
                    ];
                }
                let operation = match op {
                    GirAtomic::Load => AtomicOp::Load,
                    GirAtomic::Store => AtomicOp::Store,
                    GirAtomic::Rmw => AtomicOp::Exchange,
                    GirAtomic::FetchOr => AtomicOp::Or,
                    GirAtomic::FetchAnd => AtomicOp::And,
                    GirAtomic::CompareExchange => AtomicOp::CompareExchange,
                    GirAtomic::Fence => AtomicOp::Fence,
                };
                let width = if *op == GirAtomic::Store {
                    args.get(1)
                        .map(|value| self.body.values[value.index()].kind)
                } else {
                    results.first().map(|(kind, _)| *kind)
                };
                let align = width.and_then(|kind| kind.ty.bytes()).unwrap_or(1);
                let values = self.emit(
                    Op::Atomic {
                        op: operation,
                        ordering: *ordering,
                        failure: matches!(op, GirAtomic::CompareExchange)
                            .then_some(crate::gir::body::MemoryOrdering::Relaxed),
                        align: u32::try_from(align).expect("原子宽度"),
                    },
                    &args,
                    &results,
                );
                if let (Some(destination), Some(ty)) = (destination, ty) {
                    self.write_place(*destination, Computed::Values { ty, values })?;
                }
            }
            StatementKind::Volatile {
                op,
                pointer,
                value,
                destination,
            } => {
                let pointer = self.operand(pointer)?;
                let pointer = self.computed_values(pointer)?[0];
                match op {
                    VolatileOp::Load => {
                        let destination =
                            destination.ok_or_else(|| invalid("volatile load 缺少结果"))?;
                        let ty = self.local_ty(destination.local);
                        let lanes = self
                            .scalar_lanes(ty)
                            .ok_or_else(|| invalid("volatile load 需要机器标量"))?;
                        let values = lanes
                            .into_iter()
                            .map(|(offset, kind)| {
                                let pointer = self.offset(pointer, offset);
                                self.load(
                                    pointer,
                                    kind,
                                    u32::try_from(kind.ty.bytes().expect("volatile 宽度"))
                                        .expect("volatile 对齐"),
                                    true,
                                )
                            })
                            .collect();
                        self.write_place(destination, Computed::Values { ty, values })?;
                    }
                    VolatileOp::Store => {
                        let value = self.operand(
                            value
                                .as_ref()
                                .ok_or_else(|| invalid("volatile store 缺少值"))?,
                        )?;
                        let values = self.computed_values(value)?;
                        for (index, value) in values.into_iter().enumerate() {
                            let pointer =
                                self.offset(pointer, u64::try_from(index).expect("piece 编号") * 8);
                            self.store(
                                pointer,
                                value,
                                u32::try_from(
                                    self.machine_type(value).ty.bytes().expect("volatile 宽度"),
                                )
                                .expect("volatile 对齐"),
                                true,
                            );
                        }
                    }
                }
            }
            StatementKind::CoverageCounter(counter) => {
                self.emit(Op::CoverageCounter(*counter), &[], &[]);
            }
            StatementKind::Nop => {}
        }
        Ok(())
    }

    fn rvalue(&mut self, rvalue: &Rvalue, ty: u32) -> Result<Computed, Diagnostic> {
        match rvalue {
            Rvalue::Use(operand) => self.operand(operand),
            Rvalue::UnaryOp { op, operand } => self.unary(*op, operand, ty),
            Rvalue::BinaryOp { op, left, right } => self.binary(*op, left, right, ty),
            Rvalue::Compare { op, left, right } => self.compare(*op, left, right, ty),
            Rvalue::CheckedOp {
                check,
                kind,
                operands,
            } => self.checked(*check, kind, operands, ty),
            Rvalue::Aggregate {
                kind: AggregateKind::Closure(definition),
                operands,
            } => self.closure(*definition, ty, false, Some(operands)),
            Rvalue::Aggregate {
                kind: AggregateKind::Coroutine(definition),
                ..
            } => self.closure(*definition, ty, true, None),
            Rvalue::Aggregate { kind, operands } => self.aggregate(kind, operands, ty),
            Rvalue::Repeat { operand, count } => {
                let count = count.known().expect("具体 GIR 的数组长度已实例化");
                let address = self.temporary(ty)?;
                if count != 0 {
                    let value = self.operand(operand)?;
                    let source = self.computed_address(value)?;
                    let descriptor = self.descriptor(ty);
                    let count = self.constant(count, Type::I64);
                    self.runtime(
                        RuntimeCall::ValueRepeat,
                        &[address, source, count, descriptor],
                        &[],
                    )?;
                }
                Ok(Computed::Address { ty, address })
            }
            Rvalue::Discriminant(place) => {
                let (pointer, source_ty) = self.address(*place)?;
                let TypeKind::Aggregate { tag_bytes, .. } = self.kind(source_ty) else {
                    return Err(invalid("判别值读取要求 enum 布局"));
                };
                let tag = super::storage::integer_type(u16::from(*tag_bytes) * 8);
                let value = self.load(pointer, ValueType::scalar(tag), 1, false);
                let target = self
                    .scalar_lanes(ty)
                    .ok_or_else(|| invalid("判别结果缺少标量类型"))?[0]
                    .1
                    .ty;
                let value = self.integer_resize(value, target, false);
                Ok(Computed::Values {
                    ty,
                    values: vec![value],
                })
            }
            Rvalue::Len(place) => {
                let value = self.read_place(*place)?;
                let value = self.length(value)?;
                Ok(Computed::Values {
                    ty,
                    values: vec![value],
                })
            }
            Rvalue::Ref(place) | Rvalue::RawAddress(place) => {
                let pointer = self.address(*place)?.0;
                let pointer = if matches!(rvalue, Rvalue::RawAddress(_)) {
                    self.emit_one(
                        Op::Convert(Conversion::PointerCast),
                        &[pointer],
                        ValueType::pointer(Provenance::Raw),
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
            Rvalue::StackSlotAddress(local) => {
                let pointer = self.address(Place::local(*local))?.0;
                Ok(Computed::Values {
                    ty,
                    values: vec![pointer],
                })
            }
            Rvalue::Cast { kind, operand, .. } => self.cast(*kind, operand, ty),
            Rvalue::FunctionValue(candidate) => self.function_value(candidate.definition.0, ty),
            Rvalue::ValueCopy(place) => self.read_place(*place),
            Rvalue::CowSnapshot(place) => {
                let value = self.read_place(*place)?;
                if !self.layout(ty).passing.has_cow() {
                    return Ok(value);
                }
                let source = self.computed_address(value)?;
                let destination = self.temporary(ty)?;
                let descriptor = self.descriptor(ty);
                self.runtime(
                    RuntimeCall::CowSnapshot,
                    &[destination, source, descriptor],
                    &[],
                )?;
                Ok(Computed::Address {
                    ty,
                    address: destination,
                })
            }
            Rvalue::DynErase { operand, .. } => {
                let value = self.operand(operand)?;
                let source_ty = value.ty();
                // 函数擦除保留实例代码身份与已有环境，不能交给只收到类型描述符的 dyn 装箱入口。
                if matches!(self.kind(ty), TypeKind::Function { .. }) {
                    return self.erase_function(value, source_ty, ty);
                }
                let source = self.computed_address(value)?;
                let bytes = self
                    .layout(source_ty)
                    .layout
                    .ok_or_else(|| invalid("dyn 擦除载荷没有具体布局"))?
                    .size;
                let payload = if bytes == 0 {
                    // ZST 没有可观察载荷地址；fat pointer 仍需一个可管理的稳定指针身份。
                    self.allocate(source_ty, 0, PlacementKind::LocalHeap, None)?
                } else {
                    self.allocate(source_ty, bytes, PlacementKind::LocalHeap, None)?
                };
                self.copy_memory(payload, source, source_ty)?;
                let destination = self.temporary(ty)?;
                let concrete = self.layout(source_ty).key;
                let object = self.layout(ty).key;
                let root = self
                    .mono
                    .instances
                    .iter()
                    .flat_map(|instance| &instance.vtable_roots)
                    .find(|root| {
                        root.dyn_type == object
                            && gugu_base::hash::hash_domain("gugu-mono-v1", &root.self_type)
                                == concrete
                    })
                    .ok_or_else(|| invalid("dyn 擦除缺少 mono vtable root"))?;
                let vtable = self.emit_one(
                    Op::SymbolAddr(crate::body::Symbol::Vtable {
                        interface: root.interface,
                        concrete,
                    }),
                    &[],
                    ValueType::pointer(Provenance::Metadata),
                    Origin::Symbol,
                );
                self.store(destination, payload, 8, false);
                let slot = self.offset(destination, 8);
                self.store(slot, vtable, 8, false);
                Ok(Computed::Address {
                    ty,
                    address: destination,
                })
            }
            Rvalue::AllocObject {
                ty: object,
                operands,
            } => {
                let bytes = self
                    .layout(object.0)
                    .layout
                    .ok_or_else(|| invalid("对象分配缺少布局"))?
                    .size;
                let placement = if self.layout(object.0).passing.has_resource() {
                    PlacementKind::Resource
                } else {
                    PlacementKind::LocalHeap
                };
                let address = self.allocate(object.0, bytes, placement, None)?;
                self.aggregate_into(address, object.0, 0, None, operands)?;
                Ok(Computed::Values {
                    ty,
                    values: vec![address],
                })
            }
            Rvalue::AllocArray { element, length } => self.allocate_array(element.0, length, ty),
            Rvalue::Intrinsic {
                op,
                operands,
                types,
            } => self.intrinsic(op, operands, types, ty),
        }
    }

    fn aggregate(
        &mut self,
        kind: &AggregateKind,
        operands: &[Operand],
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let (variant, field) = match kind {
            AggregateKind::Adt { variant, .. } => (*variant, None),
            AggregateKind::Union { field, .. } => (0, Some(*field)),
            _ => (0, None),
        };
        let address = self.temporary(ty)?;
        self.aggregate_into(address, ty, variant, field, operands)?;
        Ok(Computed::Address { ty, address })
    }

    fn aggregate_into(
        &mut self,
        address: ValueId,
        ty: u32,
        variant: u32,
        field: Option<u32>,
        operands: &[Operand],
    ) -> Result<(), Diagnostic> {
        self.discriminant_store(address, ty, variant)?;
        let mut fields = self.fields(ty, variant, operands.len())?;
        if let Some(index) = field {
            let index = usize::try_from(index).expect("字段编号适配宿主");
            fields = vec![
                fields
                    .get(index)
                    .copied()
                    .ok_or_else(|| invalid("union 字段越界"))?,
            ];
        }
        if fields.len() != operands.len() {
            return Err(invalid("聚合操作数与具体字段不匹配"));
        }
        let tagged =
            matches!(self.kind(ty), TypeKind::Aggregate { tag_bytes, .. } if *tag_bytes != 0);
        for ((offset, field_ty), operand) in fields.into_iter().zip(operands) {
            let value = self.operand(operand)?;
            let pointer = self.offset(address, offset);
            self.store_computed(pointer, field_ty, value)?;
            if !tagged && let Some(slot) = self.value_slot(address) {
                self.root_init_value(slot, offset, field_ty, pointer, None)?;
            }
        }
        if tagged && let Some(slot) = self.value_slot(address) {
            self.root_init_value(slot, 0, ty, address, Some(variant))?;
        }
        Ok(())
    }

    fn discriminant_store(
        &mut self,
        address: ValueId,
        ty: u32,
        variant: u32,
    ) -> Result<(), Diagnostic> {
        if let TypeKind::Aggregate { tag_bytes, .. } = self.kind(ty)
            && *tag_bytes != 0
        {
            let width = super::storage::integer_type(u16::from(*tag_bytes) * 8);
            let value = self.constant(u64::from(variant), width);
            self.store(address, value, 1, false);
        }
        Ok(())
    }

    fn cast(&mut self, cast: CastKind, operand: &Operand, ty: u32) -> Result<Computed, Diagnostic> {
        let value = self.operand(operand)?;
        let source_ty = value.ty();
        if matches!(
            cast,
            CastKind::Opaque | CastKind::Instantiate | CastKind::AssumeInit | CastKind::NeverTo
        ) {
            return Ok(match value {
                Computed::Values { values, .. } => Computed::Values { ty, values },
                Computed::Address { address, .. } => Computed::Address { ty, address },
            });
        }
        if cast == CastKind::ArrayToSlice {
            let count = self
                .static_length(source_ty)
                .ok_or_else(|| invalid("数组转切片缺少固定长度"))?;
            let pointer = if matches!(self.kind(source_ty), TypeKind::Reference(_)) {
                self.computed_values(value)?[0]
            } else {
                self.computed_address(value)?
            };
            let count = self.constant(count, Type::I64);
            return Ok(Computed::Values {
                ty,
                values: vec![pointer, count],
            });
        }
        if matches!(self.kind(source_ty), TypeKind::FunctionItem { .. })
            && matches!(self.kind(ty), TypeKind::Function { .. })
        {
            return self.erase_function(value, source_ty, ty);
        }
        if cast == CastKind::Transmute {
            let address = self.computed_address(value)?;
            let values = self.load_abi(address, ty)?;
            return Ok(Computed::Values { ty, values });
        }
        let values = self.computed_values(value)?;
        let lanes = self
            .scalar_lanes(ty)
            .ok_or_else(|| invalid("标量转换缺少目标机器类型"))?;
        if values.len() == 2 || lanes.len() == 2 {
            return self.cast_wide(&values, source_ty, ty);
        }
        let source = *values.first().ok_or_else(|| invalid("转换缺少输入值"))?;
        let target = lanes[0].1;
        let input = self.machine_type(source);
        if input.ty == target.ty && input.provenance == target.provenance {
            return Ok(Computed::Values { ty, values });
        }
        let signed = matches!(self.kind(source_ty), TypeKind::Int { signed: true, .. });
        let conversion = match (input.ty, target.ty) {
            (Type::Ptr, Type::Ptr)
                if input.provenance == Some(Provenance::Raw)
                    && target.provenance != Some(Provenance::Raw) =>
            {
                Conversion::RawToReference
            }
            (Type::Ptr, Type::Ptr) => Conversion::PointerCast,
            (Type::Ptr, _) => Conversion::PointerToInt,
            (_, Type::Ptr) => Conversion::IntToPointer,
            (Type::F32 | Type::F64, Type::F32 | Type::F64) => Conversion::FloatResize,
            (Type::F32 | Type::F64, _) => Conversion::FloatToInt {
                signed: matches!(self.kind(ty), TypeKind::Int { signed: true, .. }),
            },
            (_, Type::F32 | Type::F64) => Conversion::IntToFloat { signed },
            _ if input.ty.bytes() > target.ty.bytes() => Conversion::Truncate,
            _ if signed => Conversion::SignExtend,
            _ => Conversion::ZeroExtend,
        };
        let origin = if input.ty == Type::Ptr {
            Origin::Derived(source)
        } else {
            Origin::None
        };
        let value = self.emit_one(Op::Convert(conversion), &[source], target, origin);
        Ok(Computed::Values {
            ty,
            values: vec![value],
        })
    }

    fn cast_wide(
        &mut self,
        values: &[ValueId],
        source_ty: u32,
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let lanes = self
            .scalar_lanes(ty)
            .ok_or_else(|| invalid("宽整数转换目标缺少机器类型"))?;
        let signed = matches!(self.kind(source_ty), TypeKind::Int { signed: true, .. });
        let mut output = vec![self.integer_resize(values[0], lanes[0].1.ty, signed)];
        if lanes.len() == 2 {
            output.push(if values.len() == 2 {
                values[1]
            } else if signed {
                let shift = self.constant(63, Type::I64);
                self.emit_one(
                    Op::Integer(crate::body::IntOp::ShrSigned),
                    &[output[0], shift],
                    ValueType::scalar(Type::I64),
                    Origin::None,
                )
            } else {
                self.constant(0, Type::I64)
            });
        }
        Ok(Computed::Values { ty, values: output })
    }
}
