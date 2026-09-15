use super::values::Computed;
use super::{Builder, Diagnostic, Storage, TypeKind, invalid};
use crate::frontend::gir::body::{Access as GirAccess, LocalId, Place, Projection};
use crate::frontend::gir::placement::PlacementKind;
use crate::lir::body::{
    Access, AliasClass, Conversion, InstId, IntOp, Op, Origin, Provenance, RuntimeCall, Symbol,
    Type, ValueId, ValueType, id,
};
use crate::lir::invalid_resource;

impl Builder<'_> {
    pub(super) fn offset(&mut self, pointer: ValueId, offset: u64) -> ValueId {
        if offset == 0 {
            return pointer;
        }
        let offset = self.constant(offset, Type::I64);
        self.dynamic_offset(pointer, offset)
    }

    pub(super) fn dynamic_offset(&mut self, pointer: ValueId, offset: ValueId) -> ValueId {
        let mut kind = self.machine_type(pointer);
        if kind.provenance == Some(Provenance::GcHeap) {
            kind.provenance = Some(Provenance::GcInterior);
        }
        self.emit_one(
            Op::PtrOffset,
            &[pointer, offset],
            kind,
            Origin::Derived(pointer),
        )
    }

    fn alias(&self, pointer: ValueId) -> AliasClass {
        let mut current = pointer;
        loop {
            match self.body.values[current.index()].origin {
                Origin::Stack(slot) => return AliasClass::Stack(slot),
                Origin::Derived(base) => current = base,
                _ => break,
            }
        }
        match self.machine_type(pointer).provenance {
            Some(
                Provenance::GcHeap
                | Provenance::GcInterior
                | Provenance::SharedHandle
                | Provenance::CompressedRef
                | Provenance::Stack,
            ) => AliasClass::Heap,
            _ => AliasClass::Foreign,
        }
    }

    pub(super) fn load(
        &mut self,
        pointer: ValueId,
        kind: ValueType,
        align: u32,
        volatile: bool,
    ) -> ValueId {
        let alias = if volatile {
            AliasClass::Volatile
        } else {
            self.alias(pointer)
        };
        self.emit_one(
            Op::Load(Access {
                alias,
                align,
                volatile,
            }),
            &[pointer],
            kind,
            Origin::None,
        )
    }

    pub(super) fn store(&mut self, pointer: ValueId, value: ValueId, align: u32, volatile: bool) {
        let heap = self
            .machine_type(pointer)
            .provenance
            .is_some_and(Provenance::managed);
        let managed = self
            .machine_type(value)
            .provenance
            .is_some_and(Provenance::managed);
        let old = (heap && managed)
            .then(|| self.load(pointer, self.machine_type(value), align, volatile));
        let store = InstId(id(self.body.instructions.len()));
        let alias = if volatile {
            AliasClass::Volatile
        } else {
            self.alias(pointer)
        };
        self.emit(
            Op::Store(Access {
                alias,
                align,
                volatile,
            }),
            &[pointer, value],
            &[],
        );
        if let Some(old) = old {
            self.emit(Op::GcWriteBarrier { store }, &[pointer, old, value], &[]);
        }
    }

    pub(super) fn address(&mut self, place: Place) -> Result<(ValueId, u32), Diagnostic> {
        let mut ty = self.local_ty(place.local);
        let mut address = match self.storage[place.local.index()].clone() {
            Storage::Stack { address, .. } | Storage::Capture { address } => Some(address),
            Storage::Heap { variable, .. } => Some(self.read(variable)?),
            Storage::Values(_) => None,
        };
        let mut variant = 0;
        for projection in self.gir.projections_of(place) {
            match projection {
                Projection::Deref => {
                    let inner = match self.kind(ty) {
                        TypeKind::Reference(inner) | TypeKind::Pointer(inner) => *inner,
                        // static ref 的 generic GIR 仍使用其值类型标注；绑定后的符号地址已经是 storage 地址。
                        _ if address.is_some() => continue,
                        _ => return Err(invalid("Deref 的具体类型不是指针")),
                    };
                    let pointer = if let Some(pointer) = address {
                        let provenance = if matches!(self.kind(ty), TypeKind::Pointer(_)) {
                            Provenance::Raw
                        } else {
                            Provenance::GcInterior
                        };
                        self.load(pointer, ValueType::pointer(provenance), 8, false)
                    } else {
                        self.local_values(place.local)?[0]
                    };
                    address = Some(pointer);
                    ty = inner;
                }
                Projection::Field { index, .. } | Projection::TupleField { index, .. } => {
                    if let TypeKind::Reference(inner) = self.kind(ty) {
                        let inner = *inner;
                        address = Some(if let Some(pointer) = address {
                            self.load(
                                pointer,
                                ValueType::pointer(Provenance::GcInterior),
                                8,
                                false,
                            )
                        } else {
                            self.local_values(place.local)?[0]
                        });
                        ty = inner;
                    }
                    let fields = match self.kind(ty) {
                        TypeKind::Aggregate { variants, .. } => {
                            variants.get(usize::try_from(variant).expect("variant 适配宿主"))
                        }
                        _ => None,
                    }
                    .ok_or_else(|| invalid("字段投影没有具体聚合布局"))?;
                    let field = fields
                        .get(usize::try_from(*index).expect("字段编号适配宿主"))
                        .ok_or_else(|| invalid("字段投影越界"))?
                        .clone();
                    let base = address.ok_or_else(|| invalid("聚合字段没有 storage"))?;
                    address = Some(self.offset(base, field.offset));
                    ty = field.ty;
                }
                Projection::Index(local) => {
                    let index = self.local_values(*local)?[0];
                    let (base, element) = self.index_base(place.local, address, ty)?;
                    let size = self
                        .layout(element)
                        .layout
                        .ok_or_else(|| invalid("下标元素没有布局"))?
                        .size;
                    let size = self.constant(size, Type::I64);
                    let index = self.integer_resize(index, Type::I64, false);
                    let offset = self.emit_one(
                        Op::Integer(IntOp::Mul),
                        &[index, size],
                        ValueType::scalar(Type::I64),
                        Origin::None,
                    );
                    address = Some(self.dynamic_offset(base, offset));
                    ty = element;
                }
                Projection::ConstantIndex { offset, from_end } => {
                    let length = self
                        .static_length(ty)
                        .ok_or_else(|| invalid("固定下标没有数组长度"))?;
                    let index = if *from_end {
                        length
                            .checked_sub(*offset)
                            .ok_or_else(|| invalid("反向下标越界"))?
                    } else {
                        *offset
                    };
                    let (base, element) = self.index_base(place.local, address, ty)?;
                    let size = self
                        .layout(element)
                        .layout
                        .ok_or_else(|| invalid("数组元素没有布局"))?
                        .size;
                    address = Some(
                        self.offset(
                            base,
                            index
                                .checked_mul(size)
                                .ok_or_else(|| invalid("下标字节偏移溢出"))?,
                        ),
                    );
                    ty = element;
                }
                Projection::Subslice { from, .. } => {
                    let (base, element) = self.index_base(place.local, address, ty)?;
                    let size = self
                        .layout(element)
                        .layout
                        .ok_or_else(|| invalid("子切片元素没有布局"))?
                        .size;
                    address = Some(
                        self.offset(
                            base,
                            from.checked_mul(size)
                                .ok_or_else(|| invalid("子切片偏移溢出"))?,
                        ),
                    );
                }
                Projection::Downcast(index) => variant = *index,
                Projection::OpaqueCast(target) => ty = target.0,
            }
        }
        address
            .map(|address| (address, ty))
            .ok_or_else(|| invalid("需要地址的 local 没有物化 storage"))
    }

    fn index_base(
        &mut self,
        local: LocalId,
        address: Option<ValueId>,
        ty: u32,
    ) -> Result<(ValueId, u32), Diagnostic> {
        match self.kind(ty).clone() {
            TypeKind::Array { element, .. } | TypeKind::Slice(element) => {
                Ok((address.ok_or_else(|| invalid("数组没有地址"))?, element))
            }
            TypeKind::Reference(inner) if matches!(self.kind(inner), TypeKind::Slice(_)) => {
                let TypeKind::Slice(element) = self.kind(inner) else {
                    unreachable!()
                };
                let element = *element;
                let pointer = if let Some(address) = address {
                    self.load(
                        address,
                        ValueType::pointer(Provenance::GcInterior),
                        8,
                        false,
                    )
                } else {
                    self.local_values(local)?[0]
                };
                Ok((pointer, element))
            }
            TypeKind::Reference(inner) => {
                let pointer = if let Some(address) = address {
                    self.load(
                        address,
                        ValueType::pointer(Provenance::GcInterior),
                        8,
                        false,
                    )
                } else {
                    self.local_values(local)?[0]
                };
                self.index_base(local, Some(pointer), inner)
            }
            _ => Err(invalid("下标投影不是数组或切片")),
        }
    }

    pub(super) fn static_length(&self, ty: u32) -> Option<u64> {
        match self.kind(ty) {
            TypeKind::Array { count, .. } => Some(*count),
            TypeKind::Reference(inner) => self.static_length(*inner),
            _ => None,
        }
    }

    pub(super) fn allocate(
        &mut self,
        ty: u32,
        bytes: u64,
        placement: PlacementKind,
        region: Option<u32>,
    ) -> Result<ValueId, Diagnostic> {
        // 资源值只能由 Resource placement 管理，不能进入任何 managed heap/region。
        if self.layout(ty).passing.has_resource() != (placement == PlacementKind::Resource) {
            return Err(invalid_resource("资源值必须使用 Resource placement"));
        }
        let layout = self.layout(ty);
        let descriptor = layout.key;
        let align = u32::try_from(
            layout
                .layout
                .ok_or_else(|| invalid("分配类型没有具体布局"))?
                .align,
        )
        .map_err(|_| invalid("分配对齐越界"))?;
        self.allocate_descriptor(descriptor, align, bytes, placement, region)
    }

    /// 按稳定 descriptor 与对齐做一次 managed 分配。
    ///
    /// 闭包环境没有对应的语言类型，只能用它自己的稳定 descriptor；`region` 为 `Some` 时走
    /// `RegionAlloc`，否则按 placement 走 `GcAlloc`。
    pub(super) fn allocate_descriptor(
        &mut self,
        descriptor: [u8; 32],
        align: u32,
        bytes: u64,
        placement: PlacementKind,
        region: Option<u32>,
    ) -> Result<ValueId, Diagnostic> {
        let bytes_value = self.constant(bytes, Type::I64);
        let allocation = self.next_allocation;
        self.next_allocation += 1;
        let op = match region {
            Some(region) if placement == PlacementKind::TurnRegion => Op::RegionAlloc {
                region,
                descriptor,
                align,
            },
            _ => Op::GcAlloc {
                descriptor,
                align,
                placement,
            },
        };
        let pointer = self.emit_one(
            op,
            &[bytes_value],
            ValueType::pointer(Provenance::GcHeap),
            Origin::Allocation(allocation),
        );
        let zero = self.constant(0, Type::I8);
        self.emit(Op::Memset { bytes }, &[pointer, zero], &[]);
        Ok(pointer)
    }

    pub(super) fn descriptor(&mut self, ty: u32) -> ValueId {
        self.emit_one(
            Op::SymbolAddr(Symbol::TypeDescriptor(self.layout(ty).key)),
            &[],
            ValueType::pointer(Provenance::Metadata),
            Origin::Symbol,
        )
    }

    pub(super) fn copy_memory(
        &mut self,
        destination: ValueId,
        source: ValueId,
        ty: u32,
    ) -> Result<(), Diagnostic> {
        let layout = self
            .layout(ty)
            .layout
            .ok_or_else(|| invalid("内存复制缺少布局"))?;
        if layout.size == 0 {
            return Ok(());
        }
        if self
            .machine_type(destination)
            .provenance
            .is_some_and(Provenance::managed)
            && !self.roots(ty).is_empty()
        {
            let descriptor = self.descriptor(ty);
            self.runtime(
                RuntimeCall::ValueTransfer,
                &[destination, source, descriptor],
                &[],
            )?;
        } else {
            self.emit(
                Op::Memmove { bytes: layout.size },
                &[destination, source],
                &[],
            );
        }
        Ok(())
    }

    pub(super) fn read_place(&mut self, place: Place) -> Result<Computed, Diagnostic> {
        if place.is_local() && matches!(self.storage[place.local.index()], Storage::Values(_)) {
            return Ok(Computed::Values {
                ty: self.local_ty(place.local),
                values: self.local_values(place.local)?,
            });
        }
        let (address, ty) = self.address(place)?;
        if let Some(lanes) = self.scalar_lanes(ty) {
            let align = self
                .layout(ty)
                .layout
                .ok_or_else(|| invalid("load 类型缺少布局"))?
                .align;
            let values = lanes
                .into_iter()
                .map(|(offset, kind)| {
                    let address = self.offset(address, offset);
                    self.load(
                        address,
                        kind,
                        u32::try_from(align.min(kind.ty.bytes().expect("标量大小")))
                            .expect("标量对齐"),
                        false,
                    )
                })
                .collect();
            Ok(Computed::Values { ty, values })
        } else {
            Ok(Computed::Address { ty, address })
        }
    }

    pub(super) fn write_place(&mut self, place: Place, value: Computed) -> Result<(), Diagnostic> {
        if self.gir.projections_of(place).iter().any(|projection| {
            matches!(
                projection,
                Projection::Field {
                    access: GirAccess::ScopedRead,
                    ..
                }
            )
        }) {
            return Err(invalid("ScopedRead 投影不能写入"));
        }
        if place.is_local()
            && let Storage::Values(variables) = self.storage[place.local.index()].clone()
        {
            let values = self.computed_values(value)?;
            if variables.len() != values.len() {
                return Err(invalid("SSA 赋值的机器分量不匹配"));
            }
            for (variable, value) in variables.zip(values) {
                self.define(variable, value);
            }
            return Ok(());
        }
        let (destination, ty) = self.address(place)?;
        match value {
            Computed::Address { address, .. } => self.copy_memory(destination, address, ty),
            Computed::Values { values, .. } => self.store_abi(destination, ty, &values),
        }
    }

    pub(super) fn integer_resize(&mut self, value: ValueId, target: Type, signed: bool) -> ValueId {
        let source = self.machine_type(value).ty;
        if source == target {
            return value;
        }
        let conversion = if source.bytes() > target.bytes() {
            Conversion::Truncate
        } else if signed {
            Conversion::SignExtend
        } else {
            Conversion::ZeroExtend
        };
        self.emit_one(
            Op::Convert(conversion),
            &[value],
            ValueType::scalar(target),
            Origin::None,
        )
    }
}
