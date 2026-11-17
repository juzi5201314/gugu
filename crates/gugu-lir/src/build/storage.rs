use super::{Builder, Diagnostic, Storage, TypeKind, invalid};
use crate::body::{
    Lifetime, Op, Origin, Provenance, RootInit, SlotId, StackSlot, Type, ValueId, ValueType, id,
};
use crate::gir::body::{
    AggregateKind, LocalId, LocalKind, MonoCandidate, Place, Projection, Rvalue, StatementKind,
};
use crate::gir::placement::{ExportFlags, PlacementKind};
use crate::hir;

impl Builder<'_> {
    pub(super) fn prepare_storage(&mut self) -> Result<(), Diagnostic> {
        let shared_captures = self.shared_capture_targets()?;
        let promotions = self.shared_capture_sources()?;
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
                        kind: crate::gir::body::CastKind::ArrayToSlice,
                        operand:
                            crate::gir::body::Operand::Copy(place)
                            | crate::gir::body::Operand::MoveInternal(place),
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
            // 共享环境的捕获在实例侧由 stable handle 承载：handle 变量必须在建 block 前登记，
            // 因此这里直接把这些 local 的 storage 定为 SharedHeap 的 Handle。
            if shared_captures.contains(&index) {
                let variable =
                    self.variable(id(index), 0, ValueType::pointer(Provenance::SharedHandle));
                self.storage.push(Storage::Heap {
                    variable,
                    placement: PlacementKind::SharedHeap,
                });
                continue;
            }
            if promotions.contains(&index) {
                self.shared_promotions[index] =
                    Some(self.variable(id(index), 0, ValueType::pointer(Provenance::SharedHandle)));
            }
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
            let placement = crate::gir::placement::record_of(
                &self.world.placement,
                self.concrete.generic_body,
                LocalId(id(index)),
            );
            let escape = local.pinned_storage
                || placement.is_some_and(|record| {
                    record.export
                        & (ExportFlags::ESCAPE | ExportFlags::PUBLISH | ExportFlags::UNKNOWN)
                        != 0
                        && addressed[index]
                });
            if escape {
                let placement = if self.layout(ty).passing.has_resource() {
                    PlacementKind::Resource
                } else {
                    placement.map_or(PlacementKind::LocalHeap, |record| match record.kind {
                        PlacementKind::Stack => PlacementKind::LocalHeap,
                        kind => kind,
                    })
                };
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
                let ty = self.local_ty(LocalId(id(index)));
                if self.layout(ty).passing.has_resource() {
                    self.lifetime(slot, true);
                    let zero = self.constant(0, Type::I8);
                    let bytes = self.layout(ty).layout.expect("资源值布局").size;
                    self.emit(Op::Memset { bytes }, &[address, zero], &[]);
                }
            }
        }
        if let Some(environment) = self.environment {
            let shared = self.shared_environment(self.owner.definition)?;
            let environment = if shared {
                self.begin_shared(environment)
            } else {
                environment
            };
            // 压缩判定与构造侧（capture_environment）使用同一世界扫描：构造侧 encode 过的
            // 槽在实例侧必须按压缩字读出。压缩字是 pointer(Raw)，解码结果是普通 GcHeap
            // 引用，可以直接进入 Capture storage 与后续读写。
            let compressed = !shared && self.compressed_environment(self.owner.definition)?;
            let mut offset = 0u64;
            for capture in &self.owner.captures {
                let local = self
                    .gir
                    .locals
                    .iter()
                    .position(|local| local.hir_local == Some(capture.local))
                    .ok_or_else(|| invalid("capture 没有具体 local"))?;
                if shared {
                    // 共享环境每槽一个 stable handle：handle 变量在 `prepare_storage` 已经登记
                    // （建 block 之后不能再新增变量），这里只绑定它。之后的读写都经 access guard
                    // 解析到同一个 cell，因此父函数与全部闭包看到的是同一个值。
                    let variable = match self.storage[local] {
                        Storage::Heap {
                            variable,
                            placement: PlacementKind::SharedHeap,
                        } => variable,
                        _ => return Err(invalid("共享捕获槽缺少 handle 变量")),
                    };
                    let slot = self.offset(environment, offset);
                    let handle =
                        self.load(slot, ValueType::pointer(Provenance::SharedHandle), 8, false);
                    self.define(variable, handle);
                    offset += 8;
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
            }
        }
        Ok(())
    }

    pub(super) fn live(&mut self, local: LocalId) -> Result<(), Diagnostic> {
        match self.storage[local.index()].clone() {
            Storage::Stack { slot, .. } => {
                if !self.layout(self.local_ty(local)).passing.has_resource() {
                    self.lifetime(slot, true);
                }
            }
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
            // 并集根表只能登记活动变体；位字段不能因覆盖同一偏移被误作指针。
            if let Storage::Stack { slot, .. } = self.storage[local.index()] {
                self.root_init_value(slot, 0, ty, destination, None)?;
            }
        }
        Ok(())
    }

    pub(super) fn dead(&mut self, local: LocalId) {
        if local.0 != 0
            && !self.layout(self.local_ty(local)).passing.has_resource()
            && let Storage::Stack { slot, .. } = self.storage[local.index()]
        {
            self.lifetime(slot, false);
        }
    }

    pub(super) fn release_stack_resources(&mut self) -> Result<(), Diagnostic> {
        for index in 1..self.storage.len() {
            let ty = self.local_ty(LocalId(id(index)));
            if !self.layout(ty).passing.has_resource() {
                continue;
            }
            if let Storage::Stack { slot, address } = self.storage[index] {
                let descriptor = self.descriptor(ty);
                self.runtime(
                    crate::body::RuntimeCall::ResourceRelease,
                    &[address, descriptor],
                    &[],
                )?;
                self.lifetime(slot, false);
            }
        }
        Ok(())
    }

    pub(super) fn lifetime(&mut self, slot: SlotId, live: bool) {
        self.body.lifetimes.push(Lifetime {
            slot,
            block: self.current,
            position: id(self.blocks[self.current.index()].instructions.len()),
            live,
        });
        // 建立新值或结束占用都清空该槽的逐根初始化位：新值的根字尚未写入，旧值的位不能继承。
        self.root_init_reset(slot);
    }

    /// 逐根初始化位复位：把该槽 root 表的全部位写成未初始化。
    fn root_init_reset(&mut self, slot: SlotId) {
        let count = self.body.stack_slots[slot.index()].roots.len();
        for index in 0..count {
            self.root_init(slot, u32::try_from(index).expect("根下标适配 u32"), false);
        }
    }

    pub(super) fn root_init_value(
        &mut self,
        slot: SlotId,
        offset: u64,
        ty: u32,
        pointer: crate::body::ValueId,
        variant: Option<u32>,
    ) -> Result<(), Diagnostic> {
        let bytes = self
            .layout(ty)
            .layout
            .ok_or_else(|| invalid("根初始化类型缺少布局"))?
            .size;
        let roots = self.body.stack_slots[slot.index()].roots.clone();
        for (index, (root, _)) in roots.iter().enumerate() {
            if *root >= offset && *root < offset + bytes {
                self.root_init(slot, u32::try_from(index).expect("根编号适配 u32"), false);
            }
        }
        self.root_init_active(slot, offset, ty, pointer, variant)
    }

    fn root_init_active(
        &mut self,
        slot: SlotId,
        offset: u64,
        ty: u32,
        pointer: crate::body::ValueId,
        variant: Option<u32>,
    ) -> Result<(), Diagnostic> {
        if self.roots(ty).is_empty() {
            return Ok(());
        }
        match self.kind(ty).clone() {
            TypeKind::Aggregate {
                tag_bytes,
                variants,
                ..
            } => {
                if tag_bytes != 0 && variants.len() > 1 {
                    if let Some(index) = variant {
                        // 构造现场已知 tag：只按该变体的字段收窄，不为其它
                        // 变体发运行时分支，也不置位。
                        let fields = variants
                            .get(usize::try_from(index).expect("变体编号适配宿主"))
                            .ok_or_else(|| invalid("静态变体编号越界"))?
                            .clone();
                        for field in fields {
                            let child = self.offset(pointer, field.offset);
                            self.root_init_active(
                                slot,
                                offset + field.offset,
                                field.ty,
                                child,
                                None,
                            )?;
                        }
                        return Ok(());
                    }
                    let tag_ty = integer_type(u16::from(tag_bytes) * 8);
                    let tag = self.load(pointer, ValueType::scalar(tag_ty), 1, false);
                    let done = self.fresh(self.source.clone(), false);
                    for (index, fields) in variants.into_iter().enumerate() {
                        if fields.iter().all(|field| self.roots(field.ty).is_empty()) {
                            continue;
                        }
                        let selected = self.fresh(self.source.clone(), false);
                        let next = self.fresh(self.source.clone(), false);
                        let expected =
                            self.constant(u64::try_from(index).expect("变体编号适配 u64"), tag_ty);
                        let condition =
                            self.compare_value(crate::body::Condition::Eq, false, tag, expected);
                        self.branch(condition, selected, next);
                        self.current = selected;
                        for field in fields {
                            let child = self.offset(pointer, field.offset);
                            self.root_init_active(
                                slot,
                                offset + field.offset,
                                field.ty,
                                child,
                                None,
                            )?;
                        }
                        self.jump(done);
                        self.current = next;
                    }
                    self.jump(done);
                    self.current = done;
                } else {
                    for field in variants.into_iter().flatten() {
                        let child = self.offset(pointer, field.offset);
                        self.root_init_active(
                            slot,
                            offset + field.offset,
                            field.ty,
                            child,
                            None,
                        )?;
                    }
                }
            }
            TypeKind::Array { element, count } if !self.roots(element).is_empty() => {
                let stride = self.layout(element).layout.expect("数组元素有布局").size;
                for index in 0..count {
                    let child = self.offset(pointer, index * stride);
                    self.root_init_active(
                        slot,
                        offset + index * stride,
                        element,
                        child,
                        None,
                    )?;
                }
            }
            _ => {
                let wanted = self.roots(ty);
                let roots = self.body.stack_slots[slot.index()].roots.clone();
                for (index, (root, provenance)) in roots.iter().enumerate() {
                    if wanted
                        .iter()
                        .any(|(at, kind)| offset + at == *root && kind == provenance)
                    {
                        self.root_init(slot, u32::try_from(index).expect("根编号适配 u32"), true);
                    }
                }
            }
        }
        Ok(())
    }

    fn root_init(&mut self, slot: SlotId, bit: u32, initialized: bool) {
        self.body.root_inits.push(RootInit {
            slot,
            block: self.current,
            position: id(self.blocks[self.current.index()].instructions.len()),
            bit,
            initialized,
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
            TypeKind::Formatter | TypeKind::Hasher => one(ValueType::scalar(Type::I64)),
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
            // 通道与 Join 是 runtime 表索引，不是可扫描的堆指针。
            TypeKind::Channel(_) | TypeKind::Join(_) => one(ValueType::scalar(Type::I64)),
            TypeKind::Panic => one(ValueType::pointer(Provenance::GcHeap)),
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
            TypeKind::Panic | TypeKind::String | TypeKind::Dynamic => {
                roots.push((base, Provenance::GcHeap))
            }
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

impl Builder<'_> {
    /// 实例侧（闭包本体）由共享环境稳定 handle 承载的捕获槽 local 下标。
    ///
    /// 表示判定与构造侧共用 `shared_environment`，两侧不会出现一个按 handle、一个按 direct
    /// pointer 的裂脑。
    fn shared_capture_targets(&self) -> Result<std::collections::BTreeSet<usize>, Diagnostic> {
        let mut targets = std::collections::BTreeSet::new();
        if self.owner.captures.is_empty() || !self.shared_environment(self.owner.definition)? {
            return Ok(targets);
        }
        for capture in &self.owner.captures {
            if let Some(local) = self
                .gir
                .locals
                .iter()
                .position(|local| local.hir_local == Some(capture.local))
            {
                targets.insert(local);
            }
        }
        Ok(targets)
    }

    /// 本 body 创建的闭包中，被共享环境捕获的源槽 local 下标。
    ///
    /// 这些源槽在闭包构造点被提升成 SharedHeap cell（父函数与全部闭包共享同一个 cell），
    /// 提升用的 handle 变量在这里预先登记。
    fn shared_capture_sources(&self) -> Result<std::collections::BTreeSet<usize>, Diagnostic> {
        let mut sources = std::collections::BTreeSet::new();
        for statement in &self.gir.statements {
            let definition = match &statement.kind {
                StatementKind::Assign(
                    _,
                    Rvalue::Aggregate {
                        kind:
                            AggregateKind::Closure(definition) | AggregateKind::Coroutine(definition),
                        ..
                    },
                )
                | StatementKind::Assign(
                    _,
                    Rvalue::FunctionValue(MonoCandidate { definition, .. }),
                ) => *definition,
                _ => continue,
            };
            if !self.shared_environment(definition)? {
                continue;
            }
            let Some(owner) = self
                .module
                .owners
                .iter()
                .find(|owner| owner.definition == definition)
            else {
                continue;
            };
            for capture in &owner.captures {
                if capture.owner != self.owner.definition {
                    continue;
                }
                if let Some(local) = self
                    .gir
                    .locals
                    .iter()
                    .position(|local| local.hir_local == Some(capture.source))
                {
                    sources.insert(local);
                }
            }
        }
        Ok(sources)
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
