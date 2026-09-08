//! 语言级赋值展开为 ValueAction / ResourceAction / CowSnapshot / Assign。
use super::*;
use crate::frontend::gir::passing::{self, PassingClass, PassingTable};

impl Builder<'_> {
    pub(super) fn copy_value(&mut self, dest: Place, src: Place, ty: TypeId) {
        if self.terminated() {
            return;
        }
        let class = self.passing().class(ty);
        if class.is_pure_bits() {
            self.copy_bits(dest, src, ty);
            return;
        }
        if class.is_unknown() {
            self.copy_unknown(dest, src, ty);
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
            self.copy_resource(dest, src, ty);
            return;
        }
        if self.copy_fields(dest, src, ty) {
            return;
        }
        self.copy_mixed(dest, src, ty, class);
    }

    fn copy_bits(&mut self, dest: Place, src: Place, ty: TypeId) {
        if let Some(size) = self.passing().size(ty) {
            if size > passing::LARGE_COPY_BYTES {
                let location = self.blocks[self.current.index()].source.location.clone();
                self.large_copies.push(passing::site(location, size, ty));
            }
            if size == 0 {
                self.assign(dest, Rvalue::Use(Operand::Copy(src)));
                self.mark_written(dest);
                return;
            }
        }
        self.value_action(ValueActionKind::Copy, src, ty);
        self.assign(dest, Rvalue::ValueCopy(src));
        self.mark_written(dest);
    }

    fn copy_identity(&mut self, dest: Place, src: Place, ty: TypeId) {
        self.value_action(ValueActionKind::Copy, src, ty);
        self.assign(dest, Rvalue::Use(Operand::Copy(src)));
        self.mark_written(dest);
    }

    fn copy_cow(&mut self, dest: Place, src: Place, ty: TypeId) {
        self.value_action(ValueActionKind::Copy, src, ty);
        self.assign(dest, Rvalue::CowSnapshot(src));
        self.mark_written(dest);
    }

    fn copy_resource(&mut self, dest: Place, src: Place, ty: TypeId) {
        self.release_if_live(dest, ty);
        self.resource_action(ResourceActionKind::AcquireLease, dest, ty);
        self.assign(dest, Rvalue::Use(Operand::Copy(src)));
        self.mark_written(dest);
    }

    fn copy_unknown(&mut self, dest: Place, src: Place, ty: TypeId) {
        self.release_if_live(dest, ty);
        self.value_action(ValueActionKind::Copy, src, ty);
        self.assign(dest, Rvalue::CowSnapshot(src));
        self.resource_action(ResourceActionKind::AcquireLease, dest, ty);
        self.mark_written(dest);
    }

    fn copy_mixed(&mut self, dest: Place, src: Place, ty: TypeId, class: PassingClass) {
        self.release_if_live(dest, ty);
        self.value_action(ValueActionKind::Copy, src, ty);
        if class.has_cow() {
            self.assign(dest, Rvalue::CowSnapshot(src));
        } else {
            self.assign(dest, Rvalue::Use(Operand::Copy(src)));
        }
        if class.has_resource() {
            self.resource_action(ResourceActionKind::AcquireLease, dest, ty);
        }
        self.mark_written(dest);
    }

    fn copy_fields(&mut self, dest: Place, src: Place, ty: TypeId) -> bool {
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
                self.copy_value(dest, src, field_ty);
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
                self.copy_value(dest, src, field_ty);
            }
            return true;
        }
        false
    }

    fn release_if_live(&mut self, dest: Place, ty: TypeId) {
        if dest.is_local()
            && self
                .written
                .get(dest.local.index())
                .copied()
                .unwrap_or(false)
            && self.locals[dest.local.index()].kind != LocalKind::Return
        {
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
        if !self.written.get(local.index()).copied().unwrap_or(false) {
            return;
        }
        self.resource_action(ResourceActionKind::ReleaseLease, Place::local(local), ty);
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

    fn mark_written(&mut self, dest: Place) {
        if dest.is_local()
            && let Some(slot) = self.written.get_mut(dest.local.index())
        {
            *slot = true;
        }
    }

    pub(super) fn passing(&self) -> &PassingTable {
        &self.passing
    }

    pub(super) fn pass_arg(&mut self, id: ExprId, local: LocalId) -> Operand {
        let ty = self.expr_ty(id);
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
                Projection::Index(_)
                | Projection::ConstantIndex { .. }
                | Projection::Subslice { .. } => element_ty(self.module, ty).unwrap_or(ty),
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
