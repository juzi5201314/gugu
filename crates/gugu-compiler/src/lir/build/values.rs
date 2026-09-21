use super::storage::integer_type;
use super::{Builder, Diagnostic, Storage, TypeKind, invalid};
use crate::frontend::gir::body::{ConstValue, LocalId, Operand};
use crate::lir::body::{
    Data, IntOp, Op, Origin, Provenance, SlotId, StackSlot, Symbol, Type, ValueId, ValueType, id,
};

#[derive(Clone)]
pub(super) enum Computed {
    Values { ty: u32, values: Vec<ValueId> },
    Address { ty: u32, address: ValueId },
}
impl Computed {
    pub(super) fn ty(&self) -> u32 {
        match self {
            Self::Values { ty, .. } | Self::Address { ty, .. } => *ty,
        }
    }
}

impl Builder<'_> {
    pub(super) fn local_values(&mut self, local: LocalId) -> Result<Vec<ValueId>, Diagnostic> {
        let Storage::Values(variables) = self.storage[local.index()].clone() else {
            let value = self.read_place(crate::frontend::gir::body::Place::local(local))?;
            return self.computed_values(value);
        };
        variables.map(|variable| self.read(variable)).collect()
    }

    pub(super) fn operand(&mut self, operand: &Operand) -> Result<Computed, Diagnostic> {
        match operand {
            Operand::Copy(place) | Operand::MoveInternal(place) => self.read_place(*place),
            Operand::Constant(constant) => {
                let constant = &self.gir.constants[constant.index()];
                self.literal(&constant.value, constant.ty.0)
            }
            Operand::Function(candidate) => {
                self.function_value(candidate.definition.0, candidate.signature.0)
            }
            Operand::LateConstRef { .. } => Err(invalid("LIR 不能消费未物化 late 引用")),
        }
    }

    pub(super) fn computed_values(
        &mut self,
        computed: Computed,
    ) -> Result<Vec<ValueId>, Diagnostic> {
        match computed {
            Computed::Values { values, .. } => Ok(values),
            Computed::Address { address, ty } => {
                if self.indirect_abi(ty) {
                    Ok(vec![address])
                } else {
                    self.load_abi(address, ty)
                }
            }
        }
    }

    pub(super) fn computed_address(&mut self, computed: Computed) -> Result<ValueId, Diagnostic> {
        match computed {
            Computed::Address { address, .. } => Ok(address),
            Computed::Values { ty, values } => {
                let address = self.temporary(ty)?;
                self.store_abi(address, ty, &values)?;
                Ok(address)
            }
        }
    }

    pub(super) fn temporary(&mut self, ty: u32) -> Result<ValueId, Diagnostic> {
        let layout = self
            .layout(ty)
            .layout
            .ok_or_else(|| invalid("临时值没有具体布局"))?;
        self.temporary_bytes(
            layout.size,
            layout.align,
            self.layout(ty).key,
            self.roots(ty),
        )
    }

    /// 按字节数物化栈临时；用于打包聚合等布局不等于单个语言类型的情况。
    pub(super) fn temporary_bytes(
        &mut self,
        bytes: u64,
        align: u64,
        descriptor: [u8; 32],
        roots: Vec<(u64, Provenance)>,
    ) -> Result<ValueId, Diagnostic> {
        let slot = SlotId(id(self.body.stack_slots.len()));
        self.body.stack_slots.push(StackSlot {
            local: u32::MAX,
            bytes,
            align: u32::try_from(align).map_err(|_| invalid("临时值对齐越界"))?,
            descriptor,
            roots,
        });
        let pointer = self.emit_one(
            Op::StackAddr(slot),
            &[],
            ValueType::pointer(Provenance::Stack),
            Origin::Stack(slot),
        );
        self.lifetime(slot, true);
        Ok(pointer)
    }

    pub(super) fn load_abi(
        &mut self,
        address: ValueId,
        ty: u32,
    ) -> Result<Vec<ValueId>, Diagnostic> {
        let align = self
            .layout(ty)
            .layout
            .ok_or_else(|| invalid("ABI load 缺少布局"))?
            .align;
        let mut values = Vec::new();
        for (offset, bytes, kind) in self.abi_lanes(ty) {
            let pointer = self.offset(address, offset);
            if kind.ty.bytes() == Some(bytes) {
                values.push(self.load(
                    pointer,
                    kind,
                    u32::try_from(align.min(bytes)).expect("ABI 对齐"),
                    false,
                ));
            } else {
                values.push(self.load_bits(pointer, bytes));
            }
        }
        Ok(values)
    }

    pub(super) fn store_abi(
        &mut self,
        address: ValueId,
        ty: u32,
        values: &[ValueId],
    ) -> Result<(), Diagnostic> {
        let lanes = self.abi_lanes(ty);
        if lanes.len() != values.len() {
            return Err(invalid("ABI store 分量数量不匹配"));
        }
        let align = self
            .layout(ty)
            .layout
            .ok_or_else(|| invalid("ABI store 缺少布局"))?
            .align;
        for ((offset, bytes, _), value) in lanes.into_iter().zip(values) {
            let pointer = self.offset(address, offset);
            if self.machine_type(*value).ty.bytes() == Some(bytes) {
                self.store(
                    pointer,
                    *value,
                    u32::try_from(align.min(bytes)).expect("ABI 对齐"),
                    false,
                );
            } else {
                self.store_bits(pointer, *value, bytes);
            }
        }
        Ok(())
    }

    fn load_bits(&mut self, address: ValueId, bytes: u64) -> ValueId {
        let mut result = self.constant(0, Type::I64);
        let mut offset = 0;
        while offset < bytes {
            let chunk = chunk(bytes - offset);
            let pointer = self.offset(address, offset);
            let value = self.load(
                pointer,
                ValueType::scalar(integer_type(u16::try_from(chunk * 8).expect("piece 位宽"))),
                1,
                false,
            );
            let mut value = self.integer_resize(value, Type::I64, false);
            if offset != 0 {
                let shift = self.constant(offset * 8, Type::I64);
                value = self.emit_one(
                    Op::Integer(IntOp::Shl),
                    &[value, shift],
                    ValueType::scalar(Type::I64),
                    Origin::None,
                );
            }
            result = self.emit_one(
                Op::Integer(IntOp::Or),
                &[result, value],
                ValueType::scalar(Type::I64),
                Origin::None,
            );
            offset += chunk;
        }
        result
    }

    fn store_bits(&mut self, address: ValueId, value: ValueId, bytes: u64) {
        let mut offset = 0;
        while offset < bytes {
            let chunk = chunk(bytes - offset);
            let mut value = value;
            if offset != 0 {
                let shift = self.constant(offset * 8, Type::I64);
                value = self.emit_one(
                    Op::Integer(IntOp::ShrUnsigned),
                    &[value, shift],
                    ValueType::scalar(Type::I64),
                    Origin::None,
                );
            }
            let value = self.integer_resize(
                value,
                integer_type(u16::try_from(chunk * 8).expect("piece 位宽")),
                false,
            );
            let pointer = self.offset(address, offset);
            self.store(pointer, value, 1, false);
            offset += chunk;
        }
    }

    pub(super) fn literal(&mut self, value: &ConstValue, ty: u32) -> Result<Computed, Diagnostic> {
        let values = match value {
            ConstValue::Unit | ConstValue::Never => Vec::new(),
            ConstValue::Bool(value) => vec![self.constant(u64::from(*value), Type::I8)],
            ConstValue::Char(value) => vec![self.constant(u64::from(u32::from(*value)), Type::I32)],
            ConstValue::Integer(value) => {
                let lanes = self
                    .scalar_lanes(ty)
                    .ok_or_else(|| invalid("整数常量类型没有标量布局"))?;
                let bytes = value.to_le_bytes();
                lanes
                    .into_iter()
                    .map(|(offset, kind)| {
                        let offset = usize::try_from(offset).expect("常量偏移");
                        let mut word = [0; 8];
                        let count =
                            usize::try_from(kind.ty.bytes().expect("整数大小")).expect("整数宽度");
                        word[..count].copy_from_slice(&bytes[offset..offset + count]);
                        self.emit_one(
                            Op::IConst(u64::from_le_bytes(word)),
                            &[],
                            kind,
                            Origin::None,
                        )
                    })
                    .collect()
            }
            ConstValue::Float(bits) => {
                let (kind, bits) = if matches!(self.kind(ty), TypeKind::Float(32)) {
                    // 语言规定 f64 字面量向 f32 进行 IEEE 舍入，不作整数式截断。
                    (
                        Type::F32,
                        u64::from((f64::from_bits(*bits) as f32).to_bits()),
                    )
                } else {
                    (Type::F64, *bits)
                };
                vec![self.emit_one(Op::FConst(bits), &[], ValueType::scalar(kind), Origin::None)]
            }
            ConstValue::Type(key) => vec![self.emit_one(
                Op::SymbolAddr(Symbol::TypeId(*key)),
                &[],
                ValueType::scalar(Type::I32),
                Origin::None,
            )],
            ConstValue::String(text) => {
                let pointer = self.data(text.as_bytes().to_vec(), Provenance::GcHeap);
                let length = self.constant(
                    u64::try_from(text.len()).expect("字符串长度适配 u64"),
                    Type::I64,
                );
                let address = self.temporary(ty)?;
                self.store(address, pointer, 8, false);
                let end = self.offset(address, 8);
                self.store(end, length, 8, false);
                return Ok(Computed::Address { ty, address });
            }
            ConstValue::Bytes(bytes) => {
                return Ok(Computed::Address {
                    ty,
                    address: self.data(bytes.clone(), Provenance::Metadata),
                });
            }
            ConstValue::CString(bytes) => {
                crate::runtime::cstring::require_terminated(bytes)
                    .map_err(|error| invalid(error.message()))?;
                vec![self.data(bytes.clone(), Provenance::Raw)]
            }
            ConstValue::Aggregate(values) => {
                let address = self.temporary(ty)?;
                let fields = self.fields(ty, 0, values.len())?;
                for ((offset, field_ty), value) in fields.into_iter().zip(values) {
                    let value = self.literal(value, field_ty)?;
                    let pointer = self.offset(address, offset);
                    self.store_computed(pointer, field_ty, value)?;
                }
                return Ok(Computed::Address { ty, address });
            }
            ConstValue::Definition(_) => return Err(invalid("LIR 不能重新求值常量定义")),
        };
        Ok(Computed::Values { ty, values })
    }

    pub(super) fn store_computed(
        &mut self,
        pointer: ValueId,
        ty: u32,
        value: Computed,
    ) -> Result<(), Diagnostic> {
        match value {
            Computed::Values { values, .. } => self.store_abi(pointer, ty, &values),
            Computed::Address { address, .. } => self.copy_memory(pointer, address, ty),
        }
    }

    pub(super) fn data(&mut self, bytes: Vec<u8>, provenance: Provenance) -> ValueId {
        let index = self
            .body
            .data
            .iter()
            .position(|data| data.align == 1 && data.bytes == bytes)
            .unwrap_or_else(|| {
                let index = self.body.data.len();
                self.body.data.push(Data { bytes, align: 1 });
                index
            });
        self.emit_one(
            Op::SymbolAddr(Symbol::Data(id(index))),
            &[],
            ValueType::pointer(provenance),
            Origin::Symbol,
        )
    }

    pub(super) fn fields(
        &self,
        ty: u32,
        variant: u32,
        count: usize,
    ) -> Result<Vec<(u64, u32)>, Diagnostic> {
        match self.kind(ty) {
            TypeKind::Array { element, .. } => {
                let bytes = self
                    .layout(*element)
                    .layout
                    .ok_or_else(|| invalid("数组元素没有布局"))?
                    .size;
                Ok((0..count)
                    .map(|index| (u64::try_from(index).expect("数组下标") * bytes, *element))
                    .collect())
            }
            TypeKind::Aggregate { variants, .. } => Ok(variants
                .get(usize::try_from(variant).expect("variant 下标"))
                .ok_or_else(|| invalid("聚合 variant 越界"))?
                .iter()
                .map(|field| (field.offset, field.ty))
                .collect()),
            _ => Err(invalid("聚合构造没有具体字段布局")),
        }
    }

    pub(super) fn function_key(&self, definition: u32, ty: u32) -> Result<[u8; 32], Diagnostic> {
        let arguments = match self.kind(ty) {
            TypeKind::FunctionItem { arguments, .. } => arguments.as_slice(),
            _ => &[],
        };
        let mut candidates = self
            .instance
            .functions
            .iter()
            .filter(|callee| callee.definition == definition);
        let first = candidates
            .next()
            .ok_or_else(|| invalid("函数值不在实例闭包中"))?;
        if candidates.next().is_none() {
            return Ok(first.instance);
        }
        self.instance
            .functions
            .iter()
            .find(|callee| callee.definition == definition && callee.type_arguments == arguments)
            .map(|callee| callee.instance)
            .ok_or_else(|| invalid("函数值不能唯一绑定具体实例"))
    }

    pub(super) fn function_value(
        &mut self,
        definition: u32,
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        if matches!(
            self.kind(ty),
            TypeKind::FunctionItem {
                capturing: false,
                ..
            }
        ) {
            return Ok(Computed::Values {
                ty,
                values: Vec::new(),
            });
        }
        let key = self.function_key(definition, ty)?;
        let value = self.emit_one(
            Op::SymbolAddr(Symbol::Instance(key)),
            &[],
            ValueType::pointer(Provenance::Code),
            Origin::Symbol,
        );
        Ok(Computed::Values {
            ty,
            values: vec![value],
        })
    }
}

fn chunk(bytes: u64) -> u64 {
    if bytes >= 8 {
        8
    } else if bytes >= 4 {
        4
    } else if bytes >= 2 {
        2
    } else {
        1
    }
}
