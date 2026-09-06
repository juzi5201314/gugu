//! 对齐证明保留累计偏移；不能逐投影截断对齐，否则会丢失后续字段抵消偏移的信息。
use super::super::semantics::borrow::{BorrowCheck, Projection};
use super::*;

impl Layouts<'_, '_> {
    pub(super) fn check_borrow(&mut self, check: &BorrowCheck) -> Result<(), Diagnostic> {
        let (Some(base), Some(target)) = (self.layout(&check.base)?, self.layout(&check.target)?)
        else {
            return Ok(());
        };
        // 所有布局对齐都是二的幂；模目标对齐记录偏移，不计算可能溢出的绝对地址。
        debug_assert!(base.align.is_power_of_two() && target.align.is_power_of_two());
        let mask = target.align - 1;
        let mut offset = 0u64;
        let mut aligned = base.align >= target.align;
        let mut ty = check.base.clone();
        for projection in &check.projection {
            match projection {
                Projection::Field(index) => {
                    let Some((field, field_offset)) = self.project_field(&ty, *index)? else {
                        return Ok(());
                    };
                    offset = offset.wrapping_add(field_offset) & mask;
                    ty = field;
                }
                Projection::Index(index) => {
                    let Ty::Array(element, _) = ty else {
                        return Err(invalid("引用投影的下标要求固定数组"));
                    };
                    let Some(layout) = self.layout(&element)? else {
                        return Ok(());
                    };
                    if let Some(index) = index {
                        offset =
                            offset.wrapping_add((*index as u64).wrapping_mul(layout.size)) & mask;
                    } else {
                        aligned &= layout.size & mask == 0;
                    }
                    ty = *element;
                }
            }
        }
        if !aligned || offset != 0 {
            return Err(invalid(
                "字段地址不能保证引用所需的自然对齐；使用 addr_of 与未对齐访问 intrinsic",
            ));
        }
        Ok(())
    }

    fn project_field(&mut self, ty: &Ty, index: usize) -> Result<Option<(Ty, u64)>, Diagnostic> {
        match ty {
            Ty::Tuple(fields) => self.field_offset(fields.iter(), index, AggregateKind::Ordered),
            Ty::Named(id, _) => {
                let nominal = &self.model.nominal[*id];
                let item = &self.model.modules[nominal.definition.module].arena.items
                    [nominal.definition.item.0 as usize];
                let kind = match item.kind {
                    ItemKind::Union { .. } => AggregateKind::Union,
                    ItemKind::Struct { .. } if nominal.repr.packed() => AggregateKind::Packed,
                    ItemKind::Struct { .. } => AggregateKind::Ordered,
                    _ => return Err(invalid("引用投影要求结构体或 union 字段")),
                };
                let variants = self.model.variants(ty).expect("名义类型字段");
                self.field_offset(
                    variants[0].fields.iter().map(|field| &field.ty),
                    index,
                    kind,
                )
            }
            Ty::Param(_) | Ty::Projection(..) | Ty::Opaque(..) => Ok(None),
            _ => Err(invalid("引用投影没有可布局字段")),
        }
    }

    fn field_offset<'t>(
        &mut self,
        fields: impl Iterator<Item = &'t Ty>,
        index: usize,
        kind: AggregateKind,
    ) -> Result<Option<(Ty, u64)>, Diagnostic> {
        let mut prefix = Layout { size: 0, align: 1 };
        for (position, ty) in fields.enumerate() {
            let Some(layout) = self.layout(ty)? else {
                return Ok(None);
            };
            let offset = aggregate_field(&mut prefix, layout, kind)?;
            if position == index {
                return Ok(Some((ty.clone(), offset)));
            }
        }
        Err(invalid("引用投影字段超出类型范围"))
    }
}
