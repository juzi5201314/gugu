use super::{Builder, Diagnostic, Storage, TypeKind, invalid};
use crate::body::{IntOp, Op, Origin, Provenance, SlotId, Type, ValueId, ValueType};
use crate::gir::body::{Place, Projection};

impl Builder<'_> {
    fn root_init_address(&mut self, slot: SlotId) -> ValueId {
        self.emit_one(
            Op::RootInitAddr(slot),
            &[],
            ValueType::pointer(Provenance::Raw),
            Origin::None,
        )
    }

    pub(super) fn root_init_bytes(&mut self, slot: SlotId, initialized: bool) {
        let bytes = self.body.stack_slots[slot.index()].roots.len() as u64;
        if bytes != 0 {
            let address = self.root_init_address(slot);
            let value = self.constant(u64::from(initialized), Type::I8);
            self.emit(Op::Memset { bytes }, &[address, value], &[]);
        }
    }

    pub(super) fn initialize_array_projection(
        &mut self,
        place: Place,
        pointer: ValueId,
        ty: u32,
    ) -> Result<(), Diagnostic> {
        if self.roots(ty).is_empty() {
            return Ok(());
        }
        let Storage::Stack { slot, .. } = self.storage[place.local.index()] else {
            return Ok(());
        };
        let TypeKind::Array { element, .. } = *self.kind(self.local_ty(place.local)) else {
            return Err(invalid("初始化数组缺少具体数组类型"));
        };
        let projections = self.gir.projections_of(place);
        let Some(Projection::Index(index)) = projections.first() else {
            return Ok(());
        };
        let Some(offset) = self.element_field_offset(element, &projections[1..]) else {
            return Ok(());
        };
        let stride = self.roots(element).len() as u64;
        let index = self.local_values(*index)?[0];
        let index = self.integer_resize(index, Type::I64, false);
        let stride = self.constant(stride, Type::I64);
        let offset_index = self.emit_one(
            Op::Integer(IntOp::Mul),
            &[index, stride],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        let base = self.root_init_address(slot);
        let base = self.emit_one(
            Op::PtrOffset,
            &[base, offset_index],
            ValueType::pointer(Provenance::Raw),
            Origin::Derived(base),
        );
        self.dynamic_root = Some(base);
        let result = self.root_init_value(slot, offset, ty, pointer, None);
        self.dynamic_root = None;
        result
    }

    fn element_field_offset(&self, mut ty: u32, projections: &[Projection]) -> Option<u64> {
        let mut offset = 0;
        let mut variant = 0;
        for projection in projections {
            match projection {
                Projection::Field { index, .. } | Projection::TupleField { index, .. } => {
                    let TypeKind::Aggregate { variants, .. } = self.kind(ty) else {
                        return None;
                    };
                    let field = variants.get(variant)?.get(*index as usize)?;
                    offset += field.offset;
                    ty = field.ty;
                    variant = 0;
                }
                Projection::Downcast(index) => variant = *index as usize,
                Projection::OpaqueCast(target) => ty = target.0,
                _ => return None,
            }
        }
        Some(offset)
    }
}
