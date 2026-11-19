use super::values::Computed;
use super::{Builder, Diagnostic, TypeKind, invalid};
use crate::body::{Condition, IntOp, Op, Origin, RuntimeCall, Type, ValueId, ValueType};
use crate::gir::body::CompareOp;
use crate::gir::concrete::Field;

impl Builder<'_> {
    pub(super) fn comparison_order(
        &mut self,
        left: Computed,
        right: Computed,
    ) -> Result<ValueId, Diagnostic> {
        match self.kind(left.ty()).clone() {
            TypeKind::Unit => Ok(self.constant(0, Type::I64)),
            TypeKind::String => {
                let mut arguments = self.explicit_abi_values(left)?;
                arguments.extend(self.explicit_abi_values(right)?);
                Ok(self.runtime(
                    RuntimeCall::StringCompare,
                    &arguments,
                    &[ValueType::scalar(Type::I64)],
                )?[0])
            }
            TypeKind::Array { element, count } => {
                let left = self.computed_address(left)?;
                let right = self.computed_address(right)?;
                self.array_order(element, count, left, right)
            }
            TypeKind::Aggregate {
                tag_bytes,
                variants,
            } => {
                let left = self.computed_address(left)?;
                let right = self.computed_address(right)?;
                if tag_bytes == 0 {
                    self.fields_order(&variants[0], left, right)
                } else {
                    self.variant_order(tag_bytes, &variants, left, right)
                }
            }
            _ => self.scalar_order(left, right),
        }
    }

    fn scalar_order(&mut self, left: Computed, right: Computed) -> Result<ValueId, Diagnostic> {
        let less = self.scalar_comparison(CompareOp::Lt, left.clone(), right.clone())?;
        let greater = self.scalar_comparison(CompareOp::Gt, left, right)?;
        let less = self.integer_resize(less, Type::I64, false);
        let greater = self.integer_resize(greater, Type::I64, false);
        Ok(self.emit_one(
            Op::Integer(IntOp::Sub),
            &[greater, less],
            ValueType::scalar(Type::I64),
            Origin::None,
        ))
    }

    fn field_order(
        &mut self,
        field: &Field,
        left: ValueId,
        right: ValueId,
    ) -> Result<ValueId, Diagnostic> {
        let left = Computed::Address {
            ty: field.ty,
            address: self.offset(left, field.offset),
        };
        let right = Computed::Address {
            ty: field.ty,
            address: self.offset(right, field.offset),
        };
        self.comparison_order(left, right)
    }

    fn fields_order(
        &mut self,
        fields: &[Field],
        left: ValueId,
        right: ValueId,
    ) -> Result<ValueId, Diagnostic> {
        if fields.is_empty() {
            return Ok(self.constant(0, Type::I64));
        }
        let result = self.temporary_bytes(8, 8, [0; 32], Vec::new())?;
        let done = self.fresh(self.source.clone(), false);
        for (index, field) in fields.iter().enumerate() {
            let order = self.field_order(field, left, right)?;
            self.store(result, order, 8, false);
            if index + 1 < fields.len() {
                let next = self.fresh(self.source.clone(), false);
                let zero = self.constant(0, Type::I64);
                let equal = self.compare_value(Condition::Eq, true, order, zero);
                self.branch(equal, next, done);
                self.current = next;
            }
        }
        self.jump(done);
        self.current = done;
        Ok(self.load(result, ValueType::scalar(Type::I64), 8, false))
    }

    fn variant_order(
        &mut self,
        tag_bytes: u8,
        variants: &[Vec<Field>],
        left: ValueId,
        right: ValueId,
    ) -> Result<ValueId, Diagnostic> {
        if variants.is_empty() {
            return Err(invalid("无变体类型没有可比较值"));
        }
        let left_tag = self.load_bits(left, u64::from(tag_bytes));
        let right_tag = self.load_bits(right, u64::from(tag_bytes));
        let order = self.emit_one(
            Op::Integer(IntOp::Sub),
            &[left_tag, right_tag],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        let result = self.temporary_bytes(8, 8, [0; 32], Vec::new())?;
        self.store(result, order, 8, false);
        let same = self.fresh(self.source.clone(), false);
        let done = self.fresh(self.source.clone(), false);
        let equal = self.compare_value(Condition::Eq, false, left_tag, right_tag);
        self.branch(equal, same, done);
        self.current = same;
        for (index, fields) in variants.iter().enumerate() {
            let next = self.fresh(self.source.clone(), false);
            let tag = self.constant(index as u64, Type::I64);
            let equal = self.compare_value(Condition::Eq, false, left_tag, tag);
            let active = self.fresh(self.source.clone(), false);
            self.branch(equal, active, next);
            self.current = active;
            let order = self.fields_order(fields, left, right)?;
            self.store(result, order, 8, false);
            self.jump(done);
            self.current = next;
        }
        self.jump(done);
        self.current = done;
        Ok(self.load(result, ValueType::scalar(Type::I64), 8, false))
    }

    fn array_order(
        &mut self,
        element: u32,
        count: u64,
        left: ValueId,
        right: ValueId,
    ) -> Result<ValueId, Diagnostic> {
        let result = self.temporary_bytes(8, 8, [0; 32], Vec::new())?;
        let index_slot = self.temporary_bytes(8, 8, [0; 32], Vec::new())?;
        let zero = self.constant(0, Type::I64);
        self.store(result, zero, 8, false);
        self.store(index_slot, zero, 8, false);
        let header = self.fresh(self.source.clone(), false);
        let body = self.fresh(self.source.clone(), false);
        let done = self.fresh(self.source.clone(), false);
        self.jump(header);
        self.current = header;
        let index = self.load(index_slot, ValueType::scalar(Type::I64), 8, false);
        let count = self.constant(count, Type::I64);
        let more = self.compare_value(Condition::Lt, false, index, count);
        self.branch(more, body, done);
        self.current = body;
        let order = self.array_element_order(element, index, left, right)?;
        self.store(result, order, 8, false);
        let one = self.constant(1, Type::I64);
        let next = self.emit_one(
            Op::Integer(IntOp::Add),
            &[index, one],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        self.store(index_slot, next, 8, false);
        let equal = self.compare_value(Condition::Eq, true, order, zero);
        self.branch(equal, header, done);
        self.current = done;
        Ok(self.load(result, ValueType::scalar(Type::I64), 8, false))
    }

    fn array_element_order(
        &mut self,
        element: u32,
        index: ValueId,
        left: ValueId,
        right: ValueId,
    ) -> Result<ValueId, Diagnostic> {
        let stride = self
            .layout(element)
            .layout
            .ok_or_else(|| invalid("数组元素缺少布局"))?
            .size;
        let stride = self.constant(stride, Type::I64);
        let offset = self.emit_one(
            Op::Integer(IntOp::Mul),
            &[index, stride],
            ValueType::scalar(Type::I64),
            Origin::None,
        );
        let left = self.dynamic_offset(left, offset);
        let right = self.dynamic_offset(right, offset);
        self.field_order(
            &Field {
                ty: element,
                offset: 0,
            },
            left,
            right,
        )
    }
}
