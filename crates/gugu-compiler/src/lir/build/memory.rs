use super::values::Computed;
use super::{Builder, Diagnostic, Storage, TypeKind, invalid};
use crate::frontend::gir::body::{Access as GirAccess, LocalId, Place, Projection};
use crate::frontend::gir::placement::PlacementKind;
use crate::lir::body::{
    Access, AliasClass, Condition, Conversion, InstId, IntOp, Op, Origin, Provenance, RuntimeCall,
    Symbol, Type, ValueId, ValueType, id,
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
        let provenance = self.machine_type(pointer).provenance;
        let heap = provenance.is_some_and(Provenance::managed);
        let shared = provenance == Some(Provenance::SharedHandle);
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
            if shared {
                // 共享字段写入的屏障记录必须绑定当前 guard：它让 GC 在 payload 搬迁期间知道
                // 哪一次写入属于哪个 token，而不是把 handle 当作 direct pointer 记账。
                // 结构性保证：SharedHandle 地址只能由 `shared_address` 派生，而它要求 guard 已打开。
                let token = self
                    .active_shared_token()
                    .expect("shared 字段写入必然在 access guard 内");
                self.emit(Op::SharedFieldBarrier { store, token }, &[], &[]);
            } else {
                self.emit(Op::GcWriteBarrier { store }, &[pointer, old, value], &[]);
            }
        }
    }

    pub(super) fn address(&mut self, place: Place) -> Result<(ValueId, u32), Diagnostic> {
        if self.is_shared(place.local) {
            // handle 不是地址：把 shared place 直接当 direct pointer 会让字段访问绕过 guard
            // 与代际校验，因此这里必须拒绝，而不是返回一个看起来像地址的值。
            return Err(invalid("shared place 的地址只能在 access guard 内派生"));
        }
        self.address_mode(place, false)
    }

    /// 在已打开的 guard 内解析 shared place 的字段地址；派生值保持 handle provenance。
    pub(super) fn shared_address(&mut self, place: Place) -> Result<(ValueId, u32), Diagnostic> {
        if self.active_shared_token().is_none() {
            return Err(invalid("shared place 的地址只能在 access guard 内派生"));
        }
        self.address_mode(place, true)
    }

    fn address_mode(&mut self, place: Place, shared: bool) -> Result<(ValueId, u32), Diagnostic> {
        let mut ty = self.local_ty(place.local);
        let mut address = if shared {
            Some(self.shared_handle(place.local)?)
        } else {
            match self.storage[place.local.index()].clone() {
                Storage::Stack { address, .. } | Storage::Capture { address } => Some(address),
                Storage::Heap { variable, .. } => Some(self.read(variable)?),
                Storage::Values(_) => None,
            }
        };
        let mut variant = 0;
        for projection in self.gir.projections_of(place) {
            match projection {
                Projection::Deref => {
                    if shared {
                        return Err(invalid(
                            "shared payload 不保存 direct pointer，不能在 guard 内解引用",
                        ));
                    }
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
                        if shared {
                            return Err(invalid(
                                "shared payload 不保存 direct pointer，不能穿透引用字段",
                            ));
                        }
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
        self.allocate_descriptor(descriptor, align, bytes, placement, region, false)
    }

    /// 按稳定 descriptor 与对齐做一次 managed 分配。
    ///
    /// 闭包环境没有对应的语言类型，只能用它自己的稳定 descriptor；`region` 为 `Some` 时走
    /// `RegionAlloc`，否则按 placement 走 `GcAlloc`。`compressed` 只对 LocalHeap 的闭包环境
    /// 为真：runtime 据此写 `COMPRESSED_REF` 对象头，GC 按头把 capture 槽当压缩字扫描。
    pub(super) fn allocate_descriptor(
        &mut self,
        descriptor: [u8; 32],
        align: u32,
        bytes: u64,
        placement: PlacementKind,
        region: Option<u32>,
        compressed: bool,
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
                compressed,
            },
        };
        let pointer = self.emit_one(
            op,
            &[bytes_value],
            ValueType::pointer(Provenance::GcHeap),
            Origin::Allocation(allocation),
        );
        if placement == PlacementKind::SharedHeap {
            // fresh payload 的唯一 use 必须是解析：解析结果才是可发布、可跨 owner 传递的身份。
            // payload 字节由运行时在建立记录时零初始化，因此共享路径不再补 `Memset`。
            return Ok(self.emit_one(
                Op::ResolveSharedHandle,
                &[pointer],
                ValueType::pointer(Provenance::SharedHandle),
                Origin::Derived(pointer),
            ));
        }
        let zero = self.constant(0, Type::I8);
        self.emit(Op::Memset { bytes }, &[pointer, zero], &[]);
        Ok(pointer)
    }

    /// 把完整指针编码成 cage 压缩字：`offset | generation << 32`，空指针归零。
    ///
    /// 只用现有整数域 op，不新增 `EncodeCompressedRef`：压缩字是 `pointer(Raw)` 值，不是
    /// GC 根。cage id 恒 0（真实编译只预留一个 cage），位移折进 generation 常量；generation
    /// 取 `CAGE_GENERATION_MIN`，与镜像启动值相同。抽 cage id / 校验 / bounds 的机器码序列
    /// 由后端 `Legalize`/`SelectInstructions` 展开。
    pub(super) fn encode_compressed_word(&mut self, pointer: ValueId) -> ValueId {
        use crate::runtime::compression_schema::{
            CAGE_GENERATION_MIN, CAGE_GENERATION_SHIFT, CAGE_OFFSET_MASK,
        };
        let bits = self.emit_one(
            Op::Convert(Conversion::PointerToInt),
            &[pointer],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        let mask = self.constant(CAGE_OFFSET_MASK, Type::I64);
        let offset = self.emit_one(
            Op::Integer(IntOp::And),
            &[bits, mask],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        let generation = self.constant(
            u64::from(CAGE_GENERATION_MIN) << CAGE_GENERATION_SHIFT,
            Type::I64,
        );
        let word = self.emit_one(
            Op::Integer(IntOp::Or),
            &[offset, generation],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        // 空引用存 CAGE_NULL_WORD：完整指针为 0 时把 word64 归零后再转指针。
        let zero = self.constant(0, Type::I64);
        let is_null = self.emit_one(
            Op::Compare {
                condition: Condition::Eq,
                signed: false,
            },
            &[bits, zero],
            ValueType::scalar(Type::I8),
            Origin::None,
        );
        let word = self.emit_one(
            Op::Select,
            &[is_null, zero, word],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        self.emit_one(
            Op::Convert(Conversion::IntToPointer),
            &[word],
            ValueType::pointer(Provenance::Raw),
            Origin::None,
        )
    }

    /// 把压缩字写入环境 capture 槽并显式登记写屏障。
    ///
    /// 槽内容按对象头表示是压缩引用，但压缩字本身是 `pointer(Raw)`，`store()` 的自动屏障
    /// 只看 value provenance 不会触发，因此这里按 hybrid barrier 的形状手写一次：
    /// 先读旧值再 Store，屏障操作数与 store 的目标/新值严格一致。
    pub(super) fn store_compressed_slot(&mut self, slot: ValueId, word: ValueId) {
        let old = self.load(slot, ValueType::pointer(Provenance::Raw), 8, false);
        let store = InstId(id(self.body.instructions.len()));
        let alias = self.alias(slot);
        self.emit(
            Op::Store(Access {
                alias,
                align: 8,
                volatile: false,
            }),
            &[slot, word],
            &[],
        );
        self.emit(Op::GcWriteBarrier { store }, &[slot, old, word], &[]);
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
        if self.machine_type(destination).provenance == Some(Provenance::SharedHandle)
            && !self.roots(ty).is_empty()
        {
            // 带 managed 根的聚合整块复制会退化成 runtime 调用，而 guard 不允许跨调用；
            // 共享 payload 的 managed 字段必须逐字段写入，由 `SharedFieldBarrier` 记账。
            return Err(invalid(
                "共享 payload 的 managed 聚合写入必须逐字段进行，不能整块复制",
            ));
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
        if self.is_shared(place.local) {
            return self.read_shared_place(place);
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
        if self.is_shared(place.local) {
            return self.write_shared_place(place, value);
        }
        let (destination, ty) = self.address(place)?;
        match value {
            Computed::Address { address, .. } => self.copy_memory(destination, address, ty),
            Computed::Values { values, .. } => self.store_abi(destination, ty, &values),
        }
    }

    /// 在 access guard 内读取一个 shared place。
    ///
    /// 标量车道在 guard 内直接载入；聚合读取必须在 `SharedAccessEnd` 之前物化到栈副本，
    /// 因为从 handle 派生的地址不允许活过 guard。
    fn read_shared_place(&mut self, place: Place) -> Result<Computed, Diagnostic> {
        let handle = self.shared_handle(place.local)?;
        self.begin_shared(handle);
        let (address, ty) = self.shared_address(place)?;
        let computed = match self.scalar_lanes(ty) {
            Some(lanes) => {
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
                Computed::Values { ty, values }
            }
            None => {
                let size = self
                    .layout(ty)
                    .layout
                    .ok_or_else(|| invalid("聚合读取缺少布局"))?
                    .size;
                let temporary = self.temporary(ty)?;
                if size != 0 {
                    self.emit(Op::Memmove { bytes: size }, &[temporary, address], &[]);
                }
                Computed::Address {
                    ty,
                    address: temporary,
                }
            }
        };
        self.end_shared()?;
        Ok(computed)
    }

    /// 在 access guard 内写入一个 shared place；字段屏障由 `store` 绑定同一个 token。
    fn write_shared_place(&mut self, place: Place, value: Computed) -> Result<(), Diagnostic> {
        let handle = self.shared_handle(place.local)?;
        self.begin_shared(handle);
        let (destination, ty) = self.shared_address(place)?;
        match value {
            Computed::Address { address, .. } => self.copy_memory(destination, address, ty)?,
            Computed::Values { values, .. } => self.store_abi(destination, ty, &values)?,
        }
        self.end_shared()?;
        Ok(())
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
