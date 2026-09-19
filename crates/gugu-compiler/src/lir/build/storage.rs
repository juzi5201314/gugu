use super::{Builder, Diagnostic, Storage, TypeKind, invalid};
use crate::frontend::gir::body::{LocalId, LocalKind, Place, Projection, Rvalue, StatementKind};
use crate::frontend::gir::placement::{ExportFlags, PlacementKind};
use crate::frontend::hir;
use crate::lir::body::{
    Lifetime, Op, Origin, Provenance, SlotId, StackSlot, Type, ValueId, ValueType, id,
};

impl Builder<'_> {
    pub(super) fn prepare_storage(&mut self) -> Result<(), Diagnostic> {
        let mut addressed: Vec<_> = self
            .gir
            .locals
            .iter()
            .map(|local| local.address_taken || local.pinned_storage)
            .collect();
        for statement in &self.gir.statements {
            match &statement.kind {
                StatementKind::Assign(_, Rvalue::Ref(place) | Rvalue::RawAddress(place)) => {
                    mark_address(&mut addressed, self.gir.projections_of(*place), *place)
                }
                StatementKind::Assign(_, Rvalue::StackSlotAddress(local)) => {
                    addressed[local.index()] = true
                }
                StatementKind::ScopedViewBegin { source, .. } => {
                    mark_address(&mut addressed, self.gir.projections_of(*source), *source)
                }
                StatementKind::Assign(
                    _,
                    Rvalue::Cast {
                        kind: crate::frontend::gir::body::CastKind::ArrayToSlice,
                        operand:
                            crate::frontend::gir::body::Operand::Copy(place)
                            | crate::frontend::gir::body::Operand::MoveInternal(place),
                        ..
                    },
                ) => mark_address(&mut addressed, self.gir.projections_of(*place), *place),
                _ => {}
            }
            if let StatementKind::Assign(place, _) | StatementKind::SetDiscriminant { place, .. } =
                &statement.kind
                && !place.is_local()
            {
                mark_address(&mut addressed, self.gir.projections_of(*place), *place);
            }
        }
        for case in &self.gir.select_cases {
            if let Some(destination) = case.destination {
                addressed[destination.local.index()] = true;
            }
        }
        for (index, local) in self.gir.locals.iter().enumerate() {
            let ty = local.ty.0;
            let lanes = self.scalar_lanes(ty);
            if !addressed[index]
                && let Some(lanes) = lanes
            {
                let start = id(self.variables.len());
                for (offset, kind) in lanes {
                    self.variable(id(index), offset, kind);
                }
                self.storage
                    .push(Storage::Values(start..id(self.variables.len())));
                continue;
            }
            let placement = self.world.placement.records.iter().find(|record| {
                record.body == self.concrete.generic_body && record.local == id(index)
            });
            let escape = local.pinned_storage
                || placement.is_some_and(|record| {
                    record.export
                        & (ExportFlags::ESCAPE | ExportFlags::PUBLISH | ExportFlags::UNKNOWN)
                        != 0
                        && addressed[index]
                });
            if escape {
                let placement =
                    placement.map_or(PlacementKind::LocalHeap, |record| match record.kind {
                        PlacementKind::Stack => PlacementKind::LocalHeap,
                        kind => kind,
                    });
                // SharedHeap 的 Heap storage 保存 stable handle；其余 placement 保存 direct pointer。
                // 两种车道严格区分，因此 handle 不可能被当作地址送进 LocalHeap 的地址 API。
                let kind = if placement == PlacementKind::SharedHeap {
                    ValueType::pointer(Provenance::SharedHandle)
                } else {
                    ValueType::pointer(Provenance::GcHeap)
                };
                let variable = self.variable(id(index), 0, kind);
                self.storage.push(Storage::Heap {
                    variable,
                    placement,
                });
            } else {
                let layout = self
                    .layout(ty)
                    .layout
                    .ok_or_else(|| invalid("storage 布局尚未具体化"))?;
                let slot = SlotId(id(self.body.stack_slots.len()));
                self.body.stack_slots.push(StackSlot {
                    local: id(index),
                    bytes: layout.size,
                    align: u32::try_from(layout.align)
                        .map_err(|_| invalid("stack slot 对齐越界"))?,
                    descriptor: self.layout(ty).key,
                    roots: self.roots(ty),
                });
                // 地址在 entry 物化；此时还没有 block，因此不创建临时指令。
                self.storage.push(Storage::Stack {
                    slot,
                    address: ValueId(u32::MAX),
                });
            }
        }
        Ok(())
    }

    pub(super) fn prepare_entry(&mut self) -> Result<(), Diagnostic> {
        let result_ty = self.gir.signature.result.0;
        if self.indirect_abi(result_ty) {
            let layout = self
                .layout(result_ty)
                .layout
                .ok_or_else(|| invalid("返回值没有布局"))?;
            self.body.signature.sret = Some((
                layout.size,
                u32::try_from(layout.align).map_err(|_| invalid("返回对齐越界"))?,
                self.layout(result_ty).key,
            ));
            self.sret = Some(
                self.entry_parameter(ValueType::pointer(Provenance::Foreign), (u32::MAX - 1, 0)),
            );
        } else {
            self.body.signature.results = self
                .abi_lanes(result_ty)
                .into_iter()
                .map(|(_, _, kind)| kind)
                .collect();
        }
        if !self.owner.captures.is_empty() {
            let kind = if self.shared_environment(self.owner.definition)? {
                ValueType::pointer(Provenance::SharedHandle)
            } else {
                ValueType::pointer(Provenance::GcHeap)
            };
            self.environment = Some(self.entry_parameter(kind, (u32::MAX - 2, 0)));
        }
        for index in 0..self.gir.locals.len() {
            if self.gir.locals[index].kind != LocalKind::Argument {
                continue;
            }
            let ty = self.gir.locals[index].ty.0;
            let values = if self.indirect_abi(ty) {
                self.body.signature.by_value.push((
                    id(self.body.signature.parameters.len()),
                    self.layout(ty).key,
                    self.layout(ty).layout.expect("参数布局").size,
                ));
                vec![self.entry_parameter(ValueType::pointer(Provenance::Foreign), (id(index), 0))]
            } else {
                self.abi_lanes(ty)
                    .into_iter()
                    .map(|(offset, _, kind)| self.entry_parameter(kind, (id(index), offset)))
                    .collect()
            };
            if let Storage::Values(variables) = &self.storage[index] {
                for (variable, &value) in variables.clone().zip(&values) {
                    self.define(variable, value);
                }
            }
            self.entry_arguments[index] = Some(values);
        }
        self.emit(Op::StackCheck, &[], &[]);
        for index in 0..self.storage.len() {
            if let Storage::Stack { slot, .. } = self.storage[index] {
                let address = self.emit_one(
                    Op::StackAddr(slot),
                    &[],
                    ValueType::pointer(Provenance::Stack),
                    Origin::Stack(slot),
                );
                self.storage[index] = Storage::Stack { slot, address };
            }
        }
        if let Some(environment) = self.environment {
            let shared = self.shared_environment(self.owner.definition)?;
            // 压缩判定与构造侧（capture_environment）使用同一世界扫描：构造侧 encode 过的
            // 槽在实例侧必须按压缩字读出。压缩字是 pointer(Raw)，解码结果是普通 GcHeap
            // 引用，可以直接进入 Capture storage 与后续读写。
            let compressed = !shared && self.compressed_environment(self.owner.definition)?;
            let mut offset = 0u64;
            let mut loaded = Vec::with_capacity(self.owner.captures.len());
            // 共享环境只开一个 guard 覆盖全部捕获读取：同一个 handle 的多次解析不该产生多个
            // guard；读出的值先留在 SSA，guard 结束后再写进各自的栈副本。
            if shared {
                self.begin_shared(environment);
            }
            for capture in &self.owner.captures {
                let local = self
                    .gir
                    .locals
                    .iter()
                    .position(|local| local.hir_local == Some(capture.local))
                    .ok_or_else(|| invalid("capture 没有具体 local"))?;
                let ty = self.local_ty(LocalId(id(local)));
                if shared {
                    // 共享环境按值捕获：每个捕获按 ABI 车道顺序占用连续 8-byte 槽，读出时先复制
                    // 到栈副本，再把副本地址当作 capture storage，因此 guard 内不会留下地址。
                    let mut values = Vec::new();
                    for (index, kind) in self.abi_lanes(ty).into_iter().enumerate() {
                        let source = self.offset(
                            environment,
                            offset + u64::try_from(index).expect("车道编号") * 8,
                        );
                        values.push(self.load(source, kind.2, 8, false));
                    }
                    offset += u64::try_from(values.len()).expect("车道数适配 u64") * 8;
                    loaded.push((local, ty, values));
                    continue;
                }
                let address = self.offset(environment, offset);
                let address = if compressed {
                    let word = self.load(address, ValueType::pointer(Provenance::Raw), 8, false);
                    self.emit_one(
                        Op::DecodeCompressedRef,
                        &[word],
                        ValueType::pointer(Provenance::GcHeap),
                        Origin::Derived(word),
                    )
                } else {
                    self.load(address, ValueType::pointer(Provenance::GcHeap), 8, false)
                };
                self.storage[local] = Storage::Capture { address };
                offset += 8;
            }
            if shared {
                self.end_shared()?;
                for (local, ty, values) in loaded {
                    let temporary = self.temporary(ty)?;
                    self.store_abi(temporary, ty, &values)?;
                    self.storage[local] = Storage::Capture { address: temporary };
                }
            }
        }
        Ok(())
    }

    pub(super) fn live(&mut self, local: LocalId) -> Result<(), Diagnostic> {
        match self.storage[local.index()].clone() {
            Storage::Stack { slot, .. } => self.lifetime(slot, true),
            Storage::Heap {
                variable,
                placement,
            } => {
                let ty = self.local_ty(local);
                let layout = self
                    .layout(ty)
                    .layout
                    .ok_or_else(|| invalid("managed 槽缺少布局"))?;
                let address = self.allocate(ty, layout.size, placement, None)?;
                self.define(variable, address);
            }
            Storage::Capture { .. } | Storage::Values(_) => {}
        }
        if let Some(arguments) = self.entry_arguments[local.index()].take()
            && !matches!(self.storage[local.index()], Storage::Values(_))
        {
            let ty = self.local_ty(local);
            let destination = self.address(Place::local(local))?.0;
            if self.indirect_abi(ty) {
                self.copy_memory(destination, arguments[0], ty)?;
            } else {
                self.store_abi(destination, ty, &arguments)?;
            }
        }
        Ok(())
    }

    pub(super) fn dead(&mut self, local: LocalId) {
        if local.0 != 0
            && let Storage::Stack { slot, .. } = self.storage[local.index()]
        {
            self.lifetime(slot, false);
        }
    }

    pub(super) fn lifetime(&mut self, slot: SlotId, live: bool) {
        self.body.lifetimes.push(Lifetime {
            slot,
            block: self.current,
            position: id(self.blocks[self.current.index()].instructions.len()),
            live,
        });
    }

    pub(super) fn scalar_lanes(&self, ty: u32) -> Option<Vec<(u64, ValueType)>> {
        let one = |kind| Some(vec![(0, kind)]);
        match self.kind(ty) {
            TypeKind::Never
            | TypeKind::Unit
            | TypeKind::FunctionItem {
                capturing: false, ..
            } => Some(Vec::new()),
            TypeKind::Bool => one(ValueType::scalar(Type::I8)),
            TypeKind::Char | TypeKind::TypeId => one(ValueType::scalar(Type::I32)),
            TypeKind::Int { bits: 128, .. } => Some(vec![
                (0, ValueType::scalar(Type::I64)),
                (8, ValueType::scalar(Type::I64)),
            ]),
            TypeKind::Int { bits, .. } => one(ValueType::scalar(integer_type(*bits))),
            TypeKind::Float(32) => one(ValueType::scalar(Type::F32)),
            TypeKind::Float(64) => one(ValueType::scalar(Type::F64)),
            TypeKind::Reference(inner) if matches!(self.kind(*inner), TypeKind::Slice(_)) => {
                Some(vec![
                    (0, ValueType::pointer(Provenance::GcInterior)),
                    (8, ValueType::scalar(Type::I64)),
                ])
            }
            TypeKind::Reference(_) => one(ValueType::pointer(Provenance::GcInterior)),
            TypeKind::Pointer(_) => one(ValueType::pointer(Provenance::Raw)),
            TypeKind::FunctionItem {
                definition,
                capturing: true,
                ..
            } => one(ValueType::pointer(
                if self.closure_is_shared(hir::DefId(*definition)) {
                    Provenance::SharedHandle
                } else {
                    Provenance::GcHeap
                },
            )),
            TypeKind::Channel(_) | TypeKind::Join(_) | TypeKind::Panic => {
                one(ValueType::pointer(Provenance::GcHeap))
            }
            TypeKind::MaybeUninit(_) => None,
            _ => None,
        }
    }

    pub(super) fn indirect_abi(&self, ty: u32) -> bool {
        let value = self.layout(ty);
        value.layout.is_some_and(|layout| layout.size > 16)
            || value.passing.has_cow()
            || value.passing.has_resource()
    }

    pub(super) fn abi_lanes(&self, ty: u32) -> Vec<(u64, u64, ValueType)> {
        if let Some(lanes) = self.scalar_lanes(ty) {
            return lanes
                .into_iter()
                .map(|(offset, kind)| (offset, kind.ty.bytes().expect("普通值具有大小"), kind))
                .collect();
        }
        if matches!(self.kind(ty), TypeKind::Function { .. }) {
            return vec![
                (0, 8, ValueType::pointer(Provenance::Code)),
                (8, 8, ValueType::pointer(Provenance::GcHeap)),
            ];
        }
        if matches!(self.kind(ty), TypeKind::Dynamic) {
            return vec![
                (0, 8, ValueType::pointer(Provenance::GcHeap)),
                (8, 8, ValueType::pointer(Provenance::Metadata)),
            ];
        }
        let size = self.layout(ty).layout.expect("具体 ABI 布局").size;
        let roots = self.roots(ty);
        (0..size)
            .step_by(8)
            .map(|offset| {
                let bytes = (size - offset).min(8);
                let kind = roots
                    .iter()
                    .find(|(root, _)| *root == offset)
                    .map_or(ValueType::scalar(Type::I64), |(_, provenance)| {
                        ValueType::pointer(*provenance)
                    });
                (offset, bytes, kind)
            })
            .collect()
    }

    pub(super) fn roots(&self, ty: u32) -> Vec<(u64, Provenance)> {
        let mut roots = Vec::new();
        self.collect_roots(ty, 0, &mut roots);
        roots.sort_unstable();
        roots.dedup();
        roots
    }

    fn collect_roots(&self, ty: u32, base: u64, roots: &mut Vec<(u64, Provenance)>) {
        match self.kind(ty) {
            TypeKind::Reference(_) => roots.push((base, Provenance::GcInterior)),
            TypeKind::FunctionItem {
                definition,
                capturing: true,
                ..
            } => roots.push((
                base,
                if self.closure_is_shared(hir::DefId(*definition)) {
                    Provenance::SharedHandle
                } else {
                    Provenance::GcHeap
                },
            )),
            TypeKind::Channel(_)
            | TypeKind::Join(_)
            | TypeKind::Panic
            | TypeKind::String
            | TypeKind::Dynamic => roots.push((base, Provenance::GcHeap)),
            TypeKind::Function { .. } => roots.push((base + 8, Provenance::GcHeap)),
            TypeKind::Array { element, count } => {
                let size = self.layout(*element).layout.expect("元素布局").size;
                if !self.roots(*element).is_empty() {
                    for index in 0..*count {
                        self.collect_roots(*element, base + index * size, roots);
                    }
                }
            }
            TypeKind::Aggregate { variants, .. } => {
                for field in variants.iter().flatten() {
                    self.collect_roots(field.ty, base + field.offset, roots);
                }
            }
            TypeKind::MaybeUninit(_) => {}
            _ => {}
        }
    }
}

fn mark_address(addressed: &mut [bool], projections: &[Projection], place: Place) {
    if !matches!(projections.first(), Some(Projection::Deref)) {
        addressed[place.local.index()] = true;
    }
}

pub(super) fn integer_type(bits: u16) -> Type {
    match bits {
        8 => Type::I8,
        16 => Type::I16,
        32 => Type::I32,
        64 => Type::I64,
        _ => unreachable!("已经具体化的整数位宽: {bits}"),
    }
}
