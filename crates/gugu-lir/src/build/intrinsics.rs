use super::values::Computed;
use super::{Builder, Diagnostic, Storage, TypeKind, invalid};
use crate::body::{
    Condition, Conversion, IntOp, Op, Origin, Provenance, RuntimeCall, Symbol, Type, ValueId,
    ValueType, id, range,
};
use crate::gir::placement::PlacementKind;
use crate::invalid_resource;
use crate::runtime::{
    COROUTINE_PACK_ALIGN, COROUTINE_PACK_BYTES, COROUTINE_PACK_FLOAT_LANES,
    COROUTINE_PACK_INTEGER_LANES, COROUTINE_PACK_TRACE_NONE, coroutine_pack_float_offset,
    coroutine_pack_integer_offset, coroutine_pack_trace_code, coroutine_pack_trace_shift,
};
use crate::{gir::body as g, hir};

impl Builder<'_> {
    pub(super) fn intrinsic(
        &mut self,
        op: &g::IntrinsicOp,
        operands: &[g::Operand],
        types: &[hir::TypeId],
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        match op {
            g::IntrinsicOp::StringBytes | g::IntrinsicOp::StringFromUtf8Unchecked => {
                let input = self.operand(&operands[0])?;
                let values = match input {
                    Computed::Values { values, .. } => values,
                    Computed::Address { address, .. } => {
                        let pointer =
                            self.load(address, ValueType::pointer(Provenance::GcHeap), 8, false);
                        let length_address = self.offset(address, 8);
                        let length =
                            self.load(length_address, ValueType::scalar(Type::I64), 8, false);
                        vec![pointer, length]
                    }
                };
                if matches!(op, g::IntrinsicOp::StringBytes) {
                    Ok(Computed::Values { ty, values })
                } else {
                    let address = self.temporary(ty)?;
                    self.store(address, values[0], 8, false);
                    let length_address = self.offset(address, 8);
                    self.store(length_address, values[1], 8, false);
                    Ok(Computed::Address { ty, address })
                }
            }
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
                    let key = self.layout(input.0).key;
                    let value = self.type_id_value(&key)?;
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
                // 局部类型已被具体化改写为地址类型；块尺寸必须取声明的值类型（`types[0]`），
                // 否则每个静态都会得到「一个指针宽」的块。
                let declared = *types
                    .first()
                    .ok_or_else(|| invalid("静态引用缺少声明的值类型"))?;
                let symbol = self.static_block(*definition, declared.0)?;
                if self.has_managed_fields(declared.0) {
                    return Err(invalid("managed 静态值的发布路径尚未接线"));
                }
                let block = self.emit_one(
                    Op::SymbolAddr(symbol),
                    &[],
                    ValueType::pointer(Provenance::Foreign),
                    Origin::Symbol,
                );
                // 块的第一个字是 initialized flags，值槽紧跟其后。
                Ok(Computed::Values {
                    ty,
                    values: vec![self.offset(block, 8)],
                })
            }
            g::IntrinsicOp::StaticInitialized(definition) => {
                let declared = *types
                    .first()
                    .ok_or_else(|| invalid("静态初始化位读取缺少声明的值类型"))?;
                let symbol = self.static_block(*definition, declared.0)?;
                let block = self.emit_one(
                    Op::SymbolAddr(symbol),
                    &[],
                    ValueType::pointer(Provenance::Foreign),
                    Origin::Symbol,
                );
                // 位字是块首的 8 字节；读点守卫据此决定是否先跑初始化器。
                Ok(Computed::Values {
                    ty,
                    values: vec![self.load(block, ValueType::scalar(Type::I64), 8, false)],
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
            g::IntrinsicOp::ArenaNew { sync } => {
                let capacity = self.operand(&operands[0])?;
                let capacity = self.computed_values(capacity)?[0];
                let capacity = self.integer_resize(capacity, Type::I64, true);
                let shared = self.constant(u64::from(*sync), Type::I64);
                // glue 约定返回 `uint`（data 段尾 control-16），这里落成句柄
                // `_region` 字段要求的 GcInterior 引用车道。
                let word = self.runtime(
                    RuntimeCall::ArenaNew,
                    &[capacity, shared],
                    &[ValueType::scalar(Type::I64)],
                )?[0];
                let pointer = self.arena_word_to_ref(word)?;
                Ok(Computed::Values {
                    ty,
                    values: vec![pointer],
                })
            }
            g::IntrinsicOp::ArenaAlloc { sync } => {
                let element = types
                    .first()
                    .ok_or_else(|| invalid("arena alloc 缺少元素类型"))?
                    .0;
                let (key, layout) = {
                    let info = self.layout(element);
                    if info.passing.has_resource() {
                        return Err(invalid("arena 不接受含 resource 字段的类型"));
                    }
                    (
                        info.key,
                        info.layout
                            .ok_or_else(|| invalid("arena alloc 的元素类型缺少布局"))?,
                    )
                };
                let receiver = self.operand(&operands[0])?;
                let address = self.computed_address(receiver)?;
                let region = self.load(
                    address,
                    ValueType::pointer(Provenance::GcInterior),
                    8,
                    false,
                );
                let region = self.pointer_bits(region);
                let kind = self.type_id_value(&key)?;
                let kind = self.integer_resize(kind, Type::I64, false);
                let bytes = self.constant(layout.size, Type::I64);
                let granular = self.constant(layout.align, Type::I64);
                let call = if *sync {
                    RuntimeCall::ArenaAllocSync
                } else {
                    RuntimeCall::ArenaAlloc
                };
                let word = self.runtime(
                    call,
                    &[region, kind, bytes, granular],
                    &[ValueType::scalar(Type::I64)],
                )?[0];
                let pointer = self.arena_word_to_ref(word)?;
                let value = self.operand(&operands[1])?;
                self.store_computed(pointer, element, value)?;
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
                self.channel_ctor(op, operands, ty)
            }
            g::IntrinsicOp::ChanTrySend | g::IntrinsicOp::ChanTryRecv => {
                self.channel_try(op, operands, ty)
            }
            g::IntrinsicOp::Spawn(g::SpawnTarget::Body(definition)) => {
                self.closure(*definition, ty, true)
            }
            g::IntrinsicOp::Spawn(g::SpawnTarget::Callee(callee)) => {
                self.spawn_callee(callee, operands, ty)
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

    /// 静态块的规范符：块尺寸取声明的值类型（具体编号）。
    ///
    /// `StaticRef`、`StaticInitialized` 与初始化器发布必须生成同一个符号：尺寸出现分歧时
    /// 链接期只能猜。线程/协程局部项按初始化域标记；块本身始终是可写数据。
    pub(super) fn static_block(
        &self,
        definition: hir::DefId,
        value_ty: u32,
    ) -> Result<Symbol, Diagnostic> {
        let declaration = &self.module.definitions[definition.index()];
        let layout = self
            .layout(value_ty)
            .layout
            .ok_or_else(|| invalid("static 缺少具体布局"))?;
        let thread_local = self.module.initialization.iter().any(|init| {
            init.definition == definition
                && matches!(
                    init.domain,
                    hir::StorageDomain::Coroutine | hir::StorageDomain::OsThread
                )
        });
        Ok(Symbol::Global {
            key: declaration.key,
            thread_local,
            bytes: layout.size,
        })
    }

    /// glue 返回的 `uint` 槽位地址落成受管引用：I64 → 裸指针 → GcInterior，
    /// 与 `(&T)(p)` 的 RawToReference 是同一条转换通道。
    fn arena_word_to_ref(&mut self, word: ValueId) -> Result<ValueId, Diagnostic> {
        let raw = self.int_to_raw(word);
        Ok(self.emit_one(
            Op::Convert(crate::body::Conversion::RawToReference),
            &[raw],
            ValueType::pointer(Provenance::GcInterior),
            Origin::Derived(raw),
        ))
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
        // 与 `concat` 同理：切片/字符串是 COW 聚合，glue 绑定需要显式 (data, len) 车道。
        let mut args = self.explicit_abi_values(base)?;
        // 调用形状固定为 (data, len, start, end)：缺省起点归一为 0，缺省终点归一为 len，
        // `utf8_boundary` glue 因此绑定到一个固定四参数的 Gugu 函数。
        for (index, endpoint) in [start, end].into_iter().enumerate() {
            match endpoint {
                Some(endpoint) => {
                    let local = self.gir.expression_locals[endpoint.index()]
                        .ok_or_else(|| invalid("字符串检查缺少已求值端点"))?;
                    args.extend(self.local_values(local)?);
                }
                None if index == 0 => args.push(self.constant(0, Type::I64)),
                None => args.push(args[1]),
            }
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
        // 零长度动态数组仍必须分配一个非空对象：收集器以 header 里的字节数区分
        // 「有效对象」与陈旧指针，0 字节对象会被当成坏指针；对象尺寸钳到 8 字节，
        // 逻辑长度仍保留 0。
        let zero = self.constant(0, Type::I64);
        let empty = self.compare_value(Condition::Eq, false, bytes, zero);
        let minimum = self.constant(8, Type::I64);
        let bytes = self.emit_one(
            Op::Select,
            &[empty, minimum, bytes],
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
                compressed: false,
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
        let owner = self
            .module
            .owners
            .iter()
            .find(|owner| owner.definition == definition)
            .ok_or_else(|| invalid("协程没有冻结 owner"))?;
        let mut lanes = Vec::new();
        if !owner.captures.is_empty() {
            lanes.push(environment);
        }
        let result_kind = self.spawn_result_kind(ty)?;
        self.spawn_call(code, &lanes, ty, result_kind)
    }

    fn channel_ctor(
        &mut self,
        op: &g::IntrinsicOp,
        operands: &[g::Operand],
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        if matches!(op, g::IntrinsicOp::ChanClose) {
            let handle =
                self.operand(operands.first().ok_or_else(|| invalid("close 缺少通道"))?)?;
            let handle = self.uint_bits(handle)?;
            self.runtime(RuntimeCall::ChannelClose, &[handle], &[])?;
            return Ok(Computed::Values {
                ty,
                values: Vec::new(),
            });
        }
        let element = match self.kind(ty) {
            TypeKind::Channel(element) => *element,
            _ => return Err(invalid("channel 构造的结果不是 chan")),
        };
        let capacity = self.operand(operands.first().ok_or_else(|| invalid("chan 缺少容量"))?)?;
        let capacity = self.uint_bits(capacity)?;
        let desc = self.descriptor(element);
        let desc = self.raw_pointer(desc);
        let handle = self.runtime(
            RuntimeCall::ChannelNew,
            &[capacity, desc],
            &[ValueType::scalar(Type::I64)],
        )?;
        Ok(Computed::Values { ty, values: handle })
    }

    fn channel_try(
        &mut self,
        op: &g::IntrinsicOp,
        operands: &[g::Operand],
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let send = matches!(op, g::IntrinsicOp::ChanTrySend);
        let target = if send {
            RuntimeCall::ChannelTrySend
        } else {
            RuntimeCall::ChannelTryRecv
        };
        let handle = operands.first().ok_or_else(|| invalid("try 缺少通道"))?;
        let payload_ty = if send { None } else { self.ok_payload(ty)? };
        let payload = match payload_ty {
            Some(inner)
                if self
                    .layout(inner)
                    .layout
                    .is_some_and(|layout| layout.size != 0) =>
            {
                Some(self.temporary(inner)?)
            }
            _ => None,
        };
        let status = if send {
            self.channel_transfer(target, handle, operands.get(1), None)?
        } else {
            let handle_value = self.operand(handle)?;
            let handle_bits = self.uint_bits(handle_value)?;
            let pointer = if let Some(address) = payload {
                self.raw_pointer(address)
            } else {
                let zero = self.constant(0, Type::I64);
                self.int_to_raw(zero)
            };
            let desc = if let Some(inner) = payload_ty {
                let desc = self.descriptor(inner);
                self.raw_pointer(desc)
            } else {
                let zero = self.constant(0, Type::I64);
                self.int_to_raw(zero)
            };
            self.runtime(
                target,
                &[handle_bits, pointer, desc],
                &[ValueType::scalar(Type::I8)],
            )?[0]
        };
        let address = self.finish_try(ty, payload_ty, payload, status, send)?;
        Ok(Computed::Address { ty, address })
    }

    /// `try_*` 的三态：1 成功，0 Full/Empty，2 Closed。
    fn finish_try(
        &mut self,
        ty: u32,
        payload_ty: Option<u32>,
        payload: Option<ValueId>,
        status: ValueId,
        send: bool,
    ) -> Result<ValueId, Diagnostic> {
        let address = self.temporary(ty)?;
        self.zero_object(address, ty)?;
        let success = self.constant(1, Type::I8);
        let ok = self.compare_value(crate::body::Condition::Eq, false, status, success);
        let closed_code = self.constant(2, Type::I8);
        let closed = self.compare_value(crate::body::Condition::Eq, false, status, closed_code);
        let ok_tag = self.constant(0, Type::I8);
        let err_tag = self.constant(1, Type::I8);
        let tag = self.emit_one(
            Op::Select,
            &[ok, ok_tag, err_tag],
            ValueType::scalar(Type::I8),
            Origin::None,
        );
        self.store(address, tag, 1, false);
        if !send {
            if let (Some(payload), Some(payload_ty)) = (payload, payload_ty) {
                let offset = self
                    .fields(ty, 0, 1)
                    .ok()
                    .and_then(|fields| fields.first().copied())
                    .map(|(offset, _)| offset)
                    .unwrap_or(0);
                let destination = self.offset(address, offset);
                self.copy_memory(destination, payload, payload_ty)?;
            }
        }
        let err_at = self
            .fields(ty, 1, 1)
            .ok()
            .and_then(|fields| fields.first().copied())
            .map(|(offset, _)| offset)
            .unwrap_or(1);
        let closed_tag = self.constant(1, Type::I8);
        let empty_tag = self.constant(0, Type::I8);
        let inner = self.emit_one(
            Op::Select,
            &[closed, closed_tag, empty_tag],
            ValueType::scalar(Type::I8),
            Origin::None,
        );
        let err_slot = self.offset(address, err_at);
        self.store(err_slot, inner, 1, false);
        Ok(address)
    }

    fn spawn_callee(
        &mut self,
        callee: &g::Callee,
        operands: &[g::Operand],
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let instance = match callee {
            g::Callee::Value(operand) => {
                let value = self.operand(operand)?;
                let source = value.ty();
                let TypeKind::FunctionItem { definition, .. } = self.kind(source) else {
                    return Err(invalid("spawn 目标必须有明确代码身份"));
                };
                self.function_key(*definition, source)?
            }
            g::Callee::Dispatch(dispatch) => {
                let function = self.owner.dispatches
                    [usize::try_from(*dispatch).expect("dispatch 编号")]
                .function
                .ok_or_else(|| invalid("spawn dispatch 缺少函数"))?;
                self.instance
                    .functions
                    .iter()
                    .find(|callee| callee.definition == function.0)
                    .ok_or_else(|| invalid("spawn 函数没有闭合实例"))?
                    .instance
            }
            _ => return Err(invalid("spawn 目标必须有明确代码身份")),
        };
        let code = self.emit_one(
            Op::SymbolAddr(Symbol::Instance(instance)),
            &[],
            ValueType::pointer(Provenance::Code),
            Origin::Symbol,
        );
        let mut lanes = Vec::new();
        for operand in operands.iter().skip(1) {
            let value = self.operand(operand)?;
            lanes.extend(self.computed_values(value)?);
        }
        // 子协程结果字在 ABI 上是一个字：结果类型含根车道时它指向带描述符的载荷对象，因此
        // 当结果类型含根车道时，结束后的结果字必须按该车道的根种类登记，供 Join 取走前扫描。
        let result_kind = self.spawn_result_kind(ty)?;
        self.spawn_call(code, &lanes, ty, result_kind)
    }

    /// spawn 结果字里根的 lane map 编码；结果类型不含根车道时为 0。
    ///
    /// 复用帧存储的同一套分类（`Builder::roots`）：结果字是载荷对象的引用，取该类型的第一个根
    /// 车道种类即可（Direct 的判别值是 0，所以这里必须用 lane map 编码 `n + 1` 才不会与
    /// 「不含根」混淆），载荷对象内部形状由它自带的描述符决定。
    fn spawn_result_kind(&self, ty: u32) -> Result<u64, Diagnostic> {
        // 协程句柄的具体类型直接给出结果载荷类型；其它形态退回按 Result 形聚合体拆解。
        let payload = match self.kind(ty) {
            TypeKind::Join(payload) => *payload,
            _ => match self.ok_payload(ty)? {
                Some(payload) => payload,
                None => return Ok(0),
            },
        };
        Ok(self
            .roots(payload)
            .first()
            .and_then(|(_, provenance)| provenance.root_kind())
            .map_or(0, coroutine_pack_trace_code))
    }

    /// `spawn(code, env, trace, result_kind)`：把内部 ABI 车道放进 9 个整数槽和 8 个浮点槽。
    fn spawn_call(
        &mut self,
        code: ValueId,
        lanes: &[ValueId],
        ty: u32,
        result_kind: u64,
    ) -> Result<Computed, Diagnostic> {
        let blob = self.temporary_bytes(
            u64::try_from(COROUTINE_PACK_BYTES).expect("参数包字节数"),
            u64::try_from(COROUTINE_PACK_ALIGN).expect("参数包对齐"),
            [0; 32],
            Vec::new(),
        )?;
        let zero = self.constant(0, Type::I64);
        for index in 0..(COROUTINE_PACK_BYTES / 8) {
            let slot = self.offset(
                blob,
                u64::try_from(coroutine_pack_integer_offset(index)).expect("车道偏移"),
            );
            self.store(slot, zero, 8, false);
        }
        let mut gpr = 0usize;
        let mut xmm = 0usize;
        let mut trace = COROUTINE_PACK_TRACE_NONE;
        for lane in lanes {
            let kind = self.machine_type(*lane);
            if matches!(kind.ty, Type::F32 | Type::F64) {
                if xmm >= COROUTINE_PACK_FLOAT_LANES {
                    return Err(invalid("spawn 浮点参数超过参数包浮点车道数"));
                }
                let bits = if kind.ty == Type::F64 {
                    *lane
                } else {
                    self.emit_one(
                        Op::Convert(Conversion::FloatResize),
                        &[*lane],
                        ValueType::scalar(Type::F64),
                        Origin::None,
                    )
                };
                let slot = self.offset(
                    blob,
                    u64::try_from(coroutine_pack_float_offset(xmm)).expect("车道偏移"),
                );
                self.store(slot, bits, 8, false);
                trace |= self.lane_trace(&kind, COROUTINE_PACK_INTEGER_LANES + xmm);
                xmm += 1;
            } else {
                if gpr >= COROUTINE_PACK_INTEGER_LANES {
                    return Err(invalid("spawn 整数参数超过参数包车道数"));
                }
                let bits = if kind.ty == Type::Ptr {
                    self.emit_one(
                        Op::Convert(Conversion::PointerToInt),
                        &[*lane],
                        ValueType::scalar(Type::I64),
                        Origin::None,
                    )
                } else {
                    *lane
                };
                let slot = self.offset(
                    blob,
                    u64::try_from(coroutine_pack_integer_offset(gpr)).expect("车道偏移"),
                );
                self.store(slot, bits, 8, false);
                trace |= self.lane_trace(&kind, gpr);
                gpr += 1;
            }
        }
        let code = self.raw_pointer(code);
        let blob = self.raw_pointer(blob);
        let trace = self.constant(trace, Type::I64);
        let result_kind = self.constant(result_kind, Type::I64);
        let handle = self.runtime(
            RuntimeCall::Spawn,
            &[code, blob, trace, result_kind],
            &[ValueType::scalar(Type::I64)],
        )?;
        Ok(Computed::Values { ty, values: handle })
    }

    /// 参数包一个车道的 lane map 位：只有受管值才置位，种类沿用 `Provenance::root_kind`。
    fn lane_trace(&self, kind: &ValueType, lane: usize) -> u64 {
        let Some(root_kind) = kind.provenance.and_then(|it| it.root_kind()) else {
            return COROUTINE_PACK_TRACE_NONE;
        };
        coroutine_pack_trace_code(root_kind) << coroutine_pack_trace_shift(lane)
    }

    /// 闭包环境的 placement：由 `EscapeAndPlacement` 的分配点表按当前语句点查。
    ///
    /// 只认「TurnRegion 且带 region 编号」的记录；没有记录或记录不带 region 时回到 LocalHeap，
    /// 保持完整 managed 语义。这两个字段由 placement verifier 保持自洽。
    fn environment_placement(&self) -> (PlacementKind, Option<u32>) {
        self.world
            .placement
            .allocs
            .iter()
            .find(|alloc| {
                alloc.body == self.concrete.generic_body && alloc.statement == self.statement
            })
            .map_or((PlacementKind::LocalHeap, None), |alloc| {
                match alloc.region {
                    Some(region) if alloc.kind == PlacementKind::TurnRegion => {
                        (PlacementKind::TurnRegion, Some(region))
                    }
                    _ => (alloc.kind, None),
                }
            })
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
        let (placement, region) = self.environment_placement();
        let shared = placement == PlacementKind::SharedHeap;
        // 压缩判定与世界扫描同源：TurnRegion / Shared 环境不压缩，capture 槽保持全指针。
        let compressed = !shared && self.compressed_environment(definition)?;
        let representation = if shared {
            crate::mono::storage::StorageRepresentation::SharedHandle
        } else if compressed {
            crate::mono::storage::StorageRepresentation::Compressed
        } else {
            crate::mono::storage::StorageRepresentation::Direct
        };
        let mut slots = Vec::with_capacity(owner.captures.len());
        let mut keys = Vec::with_capacity(owner.captures.len());
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
            let ty = self.local_ty(local_id);
            if self.layout(ty).passing.has_resource() {
                return Err(invalid_resource(
                    "Resource 值不能捕获到 managed closure environment",
                ));
            }
            keys.push(self.layout(ty).key);
            slots.push(self.capture_slot(local_id, ty, shared, placement)?);
        }
        // descriptor 是冻结类型表里的真实记录：环境类型在实例闭合期间按同一份规范字节登记，
        // 这里只按键查表，不再自造哈希。
        let shape = crate::mono::storage::environment_shape(
            self.module.definitions[definition.index()].key,
            &keys,
            representation,
        );
        let descriptor = shape.key();
        if self.mono.universe.type_id(&descriptor).is_none() {
            return Err(invalid("闭包环境类型不在冻结类型表"));
        }
        let bytes = u64::try_from(keys.len()).expect("捕获数量适配 u64") * 8;
        self.body.environments.push(crate::body::Environment {
            descriptor,
            bytes,
            // 共享环境的槽是 stable handle，环境自身的移动由 handle 与 forwarding grace 负责，
            // 这里不登记 LocalHeap 的 managed 根；压缩环境的槽是压缩字，按压缩引用种类登记；
            // 其余环境保存 direct cell 地址。
            roots: if shared {
                (0..slots.len())
                    .map(|index| (index as u64 * 8, Provenance::SharedHandle))
                    .collect()
            } else {
                slots
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        (
                            u64::try_from(index).expect("捕获编号") * 8,
                            if compressed {
                                Provenance::CompressedRef
                            } else {
                                self.machine_type(*value).provenance.expect("捕获槽为指针")
                            },
                        )
                    })
                    .collect()
            },
        });
        let environment =
            self.allocate_descriptor(descriptor, 8, bytes, placement, region, compressed)?;
        let payload = if shared {
            self.begin_shared(environment)
        } else {
            environment
        };
        for (index, slot) in slots.into_iter().enumerate() {
            let destination = self.offset(payload, u64::try_from(index).expect("捕获编号") * 8);
            if compressed && !shared {
                let word = self.encode_compressed_word(slot);
                self.store_compressed_slot(destination, word);
            } else {
                self.store(destination, slot, 8, false);
            }
        }
        if shared {
            self.end_shared()?;
        }
        Ok(environment)
    }

    /// 一个捕获在环境槽里的值：direct/compressed 环境存 cell 地址，shared 环境存 stable handle。
    ///
    /// 共享环境的每一个捕获都必须指向**同一个提升后的 cell**：父函数与全部闭包都通过它读写，
    /// 因此源槽还不是 SharedHeap cell 时在这里就地提升（分配 cell、写入当前值、把父函数后续
    /// 访问改到 cell 上）。非共享环境只保存 cell 地址，因此源槽必须是 managed storage，不能是
    /// 当前 frame 的栈地址。
    fn capture_slot(
        &mut self,
        local: g::LocalId,
        ty: u32,
        shared: bool,
        placement: PlacementKind,
    ) -> Result<ValueId, Diagnostic> {
        if shared {
            return self.promote_capture_cell(local, ty);
        }
        match self.storage[local.index()] {
            Storage::Heap { variable, .. } => self.read(variable),
            Storage::Stack { .. } => {
                if placement == PlacementKind::Stack {
                    Ok(self.address(g::Place::local(local))?.0)
                } else {
                    Err(invalid(
                        "逃逸闭包环境不能保存当前 frame 的栈地址：捕获槽必须物化到 managed storage",
                    ))
                }
            }
            Storage::Capture { address } => Ok(address),
            Storage::Values(_) => Err(invalid("捕获槽没有可寻址存储")),
        }
    }

    /// 把捕获槽提升为 SharedHeap cell，返回它的 stable handle。
    fn promote_capture_cell(&mut self, local: g::LocalId, ty: u32) -> Result<ValueId, Diagnostic> {
        if let Storage::Heap {
            variable,
            placement: PlacementKind::SharedHeap,
        } = self.storage[local.index()]
        {
            return self.read(variable);
        }
        let layout = self
            .layout(ty)
            .layout
            .ok_or_else(|| invalid("共享捕获缺少布局"))?;
        let variable = self.shared_promotions[local.index()]
            .ok_or_else(|| invalid("共享捕获源槽缺少提升变量"))?;
        let value = self.read_place(g::Place::local(local))?;
        let handle = self.allocate(ty, layout.size.max(8), PlacementKind::SharedHeap, None)?;
        self.define(variable, handle);
        self.storage[local.index()] = Storage::Heap {
            variable,
            placement: PlacementKind::SharedHeap,
        };
        self.write_shared_place(g::Place::local(local), value)?;
        Ok(handle)
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
            Op::SymbolAddr(Symbol::FunctionAdapter {
                target: key,
                has_environment: capturing,
            }),
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
            if constraint.direction == crate::semantics::assembly::Direction::In {
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
        let naked = assembly.context == crate::semantics::assembly::AssemblyContext::Naked;
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
