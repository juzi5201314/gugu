//! 语言级赋值展开为 ValueAction / ResourceAction / CowSnapshot / Assign。
use super::*;
use crate::frontend::gir::passing::{self, PassingClass, PassingTable};

/// LocalId 稠密对应 locals；只有部分初始化的聚合才保存投影前缀。
/// 前缀引用稳定的投影池范围，身份比较使用范围内的实际投影。
pub(super) enum PlaceInit {
    Unwritten,
    Whole,
    Partial(Vec<Place>),
}

impl Builder<'_> {
    pub(super) fn copy_value(&mut self, dest: Place, src: Place, ty: TypeId) {
        self.copy_value_inner(dest, src, ty, true);
    }

    fn copy_value_inner(&mut self, dest: Place, src: Place, ty: TypeId, release_dest: bool) {
        if self.terminated() {
            return;
        }
        let class = self.passing().class(ty);
        if class.is_pure_bits() {
            self.copy_bits(dest, src, ty);
            return;
        }
        if class.is_unknown() {
            self.copy_unknown(dest, src, ty, release_dest);
            return;
        }
        if class.is_pure_identity() {
            self.copy_identity(dest, src, ty);
            return;
        }
        if class.is_pure_cow() {
            self.copy_cow(dest, src, ty);
            return;
        }
        if class.is_pure_resource() {
            self.copy_resource(dest, src, ty, release_dest);
            return;
        }
        if self.copy_fields(dest, src, ty, release_dest) {
            return;
        }
        self.copy_mixed(dest, src, ty, class, release_dest);
    }

    fn copy_bits(&mut self, dest: Place, src: Place, ty: TypeId) {
        if let Some(size) = self.passing().size(ty) {
            if size > passing::LARGE_COPY_BYTES {
                let location = self.blocks[self.current.index()].source.location.clone();
                self.large_copies.push(passing::site(location, size, ty));
            }
            if size == 0 {
                self.assign(dest, Rvalue::Use(Operand::Copy(src)));
                return;
            }
        }
        self.value_action(ValueActionKind::Copy, src, ty);
        self.assign(dest, Rvalue::ValueCopy(src));
    }

    fn copy_identity(&mut self, dest: Place, src: Place, ty: TypeId) {
        self.value_action(ValueActionKind::Copy, src, ty);
        self.assign(dest, Rvalue::Use(Operand::Copy(src)));
    }

    fn copy_cow(&mut self, dest: Place, src: Place, ty: TypeId) {
        self.value_action(ValueActionKind::Copy, src, ty);
        self.assign(dest, Rvalue::CowSnapshot(src));
    }

    fn copy_resource(&mut self, dest: Place, src: Place, ty: TypeId, release_dest: bool) {
        self.resource_action(ResourceActionKind::AcquireLease, src, ty);
        if release_dest {
            self.release_if_live(dest, ty);
        }
        self.assign(dest, Rvalue::Use(Operand::Copy(src)));
    }

    fn copy_unknown(&mut self, dest: Place, src: Place, ty: TypeId, release_dest: bool) {
        self.resource_action(ResourceActionKind::AcquireLease, src, ty);
        if release_dest {
            self.release_if_live(dest, ty);
        }
        self.value_action(ValueActionKind::Copy, src, ty);
        self.assign(dest, Rvalue::CowSnapshot(src));
    }

    fn copy_mixed(
        &mut self,
        dest: Place,
        src: Place,
        ty: TypeId,
        class: PassingClass,
        release_dest: bool,
    ) {
        if class.has_resource() {
            self.resource_action(ResourceActionKind::AcquireLease, src, ty);
        }
        if release_dest {
            self.release_if_live(dest, ty);
        }
        self.value_action(ValueActionKind::Copy, src, ty);
        if class.has_cow() {
            self.assign(dest, Rvalue::CowSnapshot(src));
        } else {
            self.assign(dest, Rvalue::Use(Operand::Copy(src)));
        }
    }

    fn copy_fields(&mut self, dest: Place, src: Place, ty: TypeId, release_dest: bool) -> bool {
        if let Some(fields) = passing::struct_fields(self.module, ty) {
            let fields: Vec<_> = fields
                .iter()
                .enumerate()
                .map(|(index, field)| (index as u32, field.ty))
                .collect();
            for (index, field_ty) in fields {
                let dest = self.project(
                    dest,
                    Projection::Field {
                        index,
                        field_ty,
                        access: Access::Normal,
                    },
                );
                let src = self.project(
                    src,
                    Projection::Field {
                        index,
                        field_ty,
                        access: Access::Normal,
                    },
                );
                let release_field = release_dest && self.passing().class(field_ty).has_resource();
                self.copy_value_inner(dest, src, field_ty, release_field);
            }
            if !self.terminated() {
                self.mark_written(dest);
            }
            return true;
        }
        if let Some(fields) = passing::tuple_fields(self.module, ty) {
            let fields: Vec<_> = fields.iter().copied().enumerate().collect();
            for (index, field_ty) in fields {
                let dest = self.project(
                    dest,
                    Projection::TupleField {
                        index: index as u32,
                        field_ty,
                    },
                );
                let src = self.project(
                    src,
                    Projection::TupleField {
                        index: index as u32,
                        field_ty,
                    },
                );
                let release_field = release_dest && self.passing().class(field_ty).has_resource();
                self.copy_value_inner(dest, src, field_ty, release_field);
            }
            if !self.terminated() {
                self.mark_written(dest);
            }
            return true;
        }
        false
    }

    fn release_if_live(&mut self, dest: Place, ty: TypeId) {
        if self.is_written(dest) && self.locals[dest.local.index()].kind != LocalKind::Return {
            self.resource_action(ResourceActionKind::ReleaseLease, dest, ty);
        }
    }

    pub(super) fn release_local(&mut self, local: LocalId) {
        let ty = self.locals[local.index()].ty;
        let class = self.passing().class(ty);
        if !class.has_resource() && !class.is_unknown() {
            return;
        }
        if self.locals[local.index()].kind == LocalKind::Return {
            return;
        }
        self.release_resources(Place::local(local), ty);
    }

    fn release_resources(&mut self, place: Place, ty: TypeId) {
        let class = self.passing().class(ty);
        if !class.has_resource() && !class.is_unknown() {
            return;
        }
        if class.has_resource()
            && let Some(fields) = passing::struct_fields(self.module, ty)
        {
            let fields: Vec<_> = fields
                .iter()
                .enumerate()
                .map(|(index, field)| (index as u32, field.ty))
                .collect();
            for (index, field_ty) in fields {
                let field = self.project(
                    place,
                    Projection::Field {
                        index,
                        field_ty,
                        access: Access::Normal,
                    },
                );
                self.release_resources(field, field_ty);
            }
            return;
        }
        if class.has_resource()
            && let Some(fields) = passing::tuple_fields(self.module, ty)
        {
            let fields: Vec<_> = fields.iter().copied().enumerate().collect();
            for (index, field_ty) in fields {
                let field = self.project(
                    place,
                    Projection::TupleField {
                        index: index as u32,
                        field_ty,
                    },
                );
                self.release_resources(field, field_ty);
            }
            return;
        }
        if self.is_written(place) {
            self.resource_action(ResourceActionKind::ReleaseLease, place, ty);
        }
    }

    fn value_action(&mut self, action: ValueActionKind, place: Place, descriptor: TypeId) {
        self.push_stmt(StatementKind::ValueAction {
            action,
            place,
            descriptor,
        });
    }

    fn resource_action(&mut self, action: ResourceActionKind, place: Place, descriptor: TypeId) {
        self.push_stmt(StatementKind::ResourceAction {
            action,
            place,
            descriptor,
        });
    }

    pub(super) fn mark_written(&mut self, dest: Place) {
        debug_assert_eq!(self.written.len(), self.locals.len());
        debug_assert!(dest.local.index() < self.locals.len());
        debug_assert!(dest.range().end <= self.projections.len());
        let path = &self.projections[dest.range()];
        let state = &mut self.written[dest.local.index()];
        if path.is_empty() {
            *state = PlaceInit::Whole;
            return;
        }
        match state {
            PlaceInit::Whole => {}
            PlaceInit::Unwritten => *state = PlaceInit::Partial(vec![dest]),
            PlaceInit::Partial(places) => {
                if places
                    .iter()
                    .any(|place| path.starts_with(&self.projections[place.range()]))
                {
                    return;
                }
                places.retain(|place| !self.projections[place.range()].starts_with(path));
                places.push(dest);
            }
        }
    }

    fn is_written(&self, place: Place) -> bool {
        debug_assert_eq!(self.written.len(), self.locals.len());
        debug_assert!(place.local.index() < self.locals.len());
        debug_assert!(place.range().end <= self.projections.len());
        match &self.written[place.local.index()] {
            PlaceInit::Unwritten => false,
            PlaceInit::Whole => true,
            PlaceInit::Partial(places) => places.iter().any(|written| {
                self.projections[place.range()].starts_with(&self.projections[written.range()])
            }),
        }
    }

    pub(super) fn passing(&self) -> &PassingTable {
        &self.passing
    }

    pub(super) fn pass_arg(&mut self, id: ExprId, local: LocalId) -> Operand {
        let ty = self.owner.expression_types[id.index()];
        let dest = self.temp(ty);
        self.copy_value(Place::local(dest), Place::local(local), ty);
        Operand::MoveInternal(Place::local(dest))
    }

    pub(super) fn place_ty(&self, place: Place) -> TypeId {
        let mut ty = self.locals[place.local.index()].ty;
        for projection in &self.projections[place.range()] {
            ty = match projection {
                Projection::Field { field_ty, .. }
                | Projection::TupleField { field_ty, .. }
                | Projection::OpaqueCast(field_ty) => *field_ty,
                Projection::Deref => deref_ty(self.module, ty).unwrap_or(ty),
                Projection::Index(_) | Projection::ConstantIndex { .. } => {
                    let base = deref_ty(self.module, ty).unwrap_or(ty);
                    element_ty(self.module, base).unwrap_or(ty)
                }
                Projection::Subslice { .. } => element_ty(self.module, ty).unwrap_or(ty),
                Projection::Downcast(_) => ty,
            };
        }
        ty
    }
}

fn deref_ty(module: &hir::Module, ty: TypeId) -> Option<TypeId> {
    match module.types.get(ty.index())? {
        hir::Type::Ref(inner) | hir::Type::Ptr(inner) => Some(*inner),
        _ => None,
    }
}

fn element_ty(module: &hir::Module, ty: TypeId) -> Option<TypeId> {
    match module.types.get(ty.index())? {
        hir::Type::Array(elem, _) | hir::Type::Slice(elem) => Some(*elem),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(builder: &mut Builder<'_>, place: Place, index: usize) -> Place {
        let ty = builder.place_ty(place);
        let field_ty = passing::struct_fields(builder.module, ty).unwrap()[index].ty;
        builder.project(
            place,
            Projection::Field {
                index: u32::try_from(index).unwrap(),
                field_ty,
                access: Access::Normal,
            },
        )
    }

    #[test]
    fn resource_scope_release_tracks_partial_places() {
        let (hir, _) = crate::frontend::gir::tests::compile_gir(include_str!(
            "../fixtures/resource_fields.gg"
        ));
        let module = hir.module();
        let owner = module
            .owners
            .iter()
            .find(|owner| module.definitions[owner.definition.index()].name == "clone_outer")
            .unwrap();
        let mut builder = Builder::new(module, owner).unwrap();
        let local = |name: &str| {
            owner
                .locals
                .iter()
                .position(|local| local.name == name)
                .unwrap()
        };
        let output = builder.hir_to_gir[local("output")];
        let source = builder.hir_to_gir[local("source")];
        let destination_inner = field(&mut builder, Place::local(output), 1);
        let destination = field(&mut builder, destination_inner, 1);
        let source_inner = field(&mut builder, Place::local(source), 1);
        let source = field(&mut builder, source_inner, 1);
        let ty = builder.place_ty(source);
        builder.copy_value(destination, source, ty);
        let repeated_inner = field(&mut builder, Place::local(output), 1);
        let repeated = field(&mut builder, repeated_inner, 1);
        assert_ne!(destination.projections, repeated.projections);
        builder.copy_value(repeated, source, ty);
        builder.release_local(output);
        let released: Vec<_> = builder.blocks[builder.current.index()]
            .statements
            .iter()
            .filter_map(|statement| match statement.kind {
                StatementKind::ResourceAction {
                    action: ResourceActionKind::ReleaseLease,
                    place,
                    ..
                } => Some(place),
                _ => None,
            })
            .collect();
        assert_eq!(released.len(), 2, "一次覆盖释放旧值，一次退出释放新值");
        for place in released {
            assert_eq!(place.local, output);
            assert_eq!(
                builder.projections[place.range()],
                builder.projections[destination.range()]
            );
        }
    }
}
