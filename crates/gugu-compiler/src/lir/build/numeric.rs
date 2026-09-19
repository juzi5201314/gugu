use super::values::Computed;
use super::{Builder, Diagnostic, TypeKind, invalid};
use crate::frontend::gir::body::{BinaryOp, CheckOpKind, CompareOp, Operand, UnaryOp};
use crate::lir::body::{
    Condition, FloatOp, IntOp, Op, Origin, RuntimeCall, Type, ValueId, ValueType,
};

impl Builder<'_> {
    pub(super) fn binary(
        &mut self,
        op: BinaryOp,
        left: &Operand,
        right: &Operand,
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let left = self.operand(left)?;
        let right = self.operand(right)?;
        let left = self.computed_values(left)?;
        let right = self.computed_values(right)?;
        if op == BinaryOp::Add && matches!(self.kind(ty), TypeKind::String) {
            let mut args = left;
            args.extend(right);
            return self.runtime_result(RuntimeCall::Concat, args, ty);
        }
        if left.len() == 2 && matches!(self.kind(ty), TypeKind::Int { bits: 128, .. }) {
            return self.wide_binary(op, &left, &right, ty);
        }
        let (&left, &right) = left
            .first()
            .zip(right.first())
            .ok_or_else(|| invalid("算术操作缺少机器值"))?;
        let kind = self.machine_type(left);
        let signed = matches!(self.kind(ty), TypeKind::Int { signed: true, .. });
        let op = if matches!(kind.ty, Type::F32 | Type::F64) {
            Op::Float(match op {
                BinaryOp::Add => FloatOp::Add,
                BinaryOp::Sub => FloatOp::Sub,
                BinaryOp::Mul => FloatOp::Mul,
                BinaryOp::Div => FloatOp::Div,
                _ => return Err(invalid("浮点操作不在封闭指令集中")),
            })
        } else {
            Op::Integer(integer_op(op, signed))
        };
        let right = if self.machine_type(right).ty != kind.ty {
            self.integer_resize(right, kind.ty, false)
        } else {
            right
        };
        let value = self.emit_one(op, &[left, right], kind, Origin::None);
        Ok(Computed::Values {
            ty,
            values: vec![value],
        })
    }

    pub(super) fn unary(
        &mut self,
        op: UnaryOp,
        operand: &Operand,
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let operand = self.operand(operand)?;
        let values = self.computed_values(operand)?;
        if values.len() == 2 && matches!(self.kind(ty), TypeKind::Int { bits: 128, .. }) {
            if op == UnaryOp::Not {
                let values = values
                    .into_iter()
                    .map(|value| {
                        self.emit_one(
                            Op::Integer(IntOp::Not),
                            &[value],
                            ValueType::scalar(Type::I64),
                            Origin::None,
                        )
                    })
                    .collect();
                return Ok(Computed::Values { ty, values });
            }
            let zero = self.constant(0, Type::I64);
            let low = self.emit(
                Op::Integer(IntOp::SubBorrow),
                &[zero, values[0], zero],
                &[
                    (ValueType::scalar(Type::I64), Origin::None),
                    (ValueType::scalar(Type::I64), Origin::None),
                ],
            );
            let high = self.emit(
                Op::Integer(IntOp::SubBorrow),
                &[zero, values[1], low[1]],
                &[
                    (ValueType::scalar(Type::I64), Origin::None),
                    (ValueType::scalar(Type::I64), Origin::None),
                ],
            );
            return Ok(Computed::Values {
                ty,
                values: vec![low[0], high[0]],
            });
        }
        let value = *values.first().ok_or_else(|| invalid("一元操作缺少值"))?;
        let kind = self.machine_type(value);
        let op = match (op, kind.ty) {
            (UnaryOp::Neg, Type::F32 | Type::F64) => Op::Float(FloatOp::Neg),
            (UnaryOp::Neg, _) => Op::Integer(IntOp::Neg),
            (UnaryOp::Not, _) if matches!(self.kind(ty), TypeKind::Bool) => {
                let zero = self.constant(0, Type::I8);
                let value = self.compare_value(Condition::Eq, false, value, zero);
                return Ok(Computed::Values {
                    ty,
                    values: vec![value],
                });
            }
            (UnaryOp::Not, _) => Op::Integer(IntOp::Not),
        };
        Ok(Computed::Values {
            ty,
            values: vec![self.emit_one(op, &[value], kind, Origin::None)],
        })
    }

    pub(super) fn compare(
        &mut self,
        op: CompareOp,
        left: &Operand,
        right: &Operand,
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let lhs = self.operand(left)?;
        let rhs = self.operand(right)?;
        let signed = matches!(self.kind(lhs.ty()), TypeKind::Int { signed: true, .. });
        let lhs = self.computed_values(lhs)?;
        let rhs = self.computed_values(rhs)?;
        let condition = condition(op);
        let value = if lhs.len() == 2 && rhs.len() == 2 {
            self.wide_compare(condition, signed, &lhs, &rhs)
        } else {
            let (&lhs, &rhs) = lhs
                .first()
                .zip(rhs.first())
                .ok_or_else(|| invalid("比较缺少机器值"))?;
            self.compare_value(condition, signed, lhs, rhs)
        };
        Ok(Computed::Values {
            ty,
            values: vec![value],
        })
    }

    pub(super) fn compare_value(
        &mut self,
        condition: Condition,
        signed: bool,
        left: ValueId,
        right: ValueId,
    ) -> ValueId {
        self.emit_one(
            Op::Compare { condition, signed },
            &[left, right],
            ValueType::scalar(Type::I8),
            Origin::None,
        )
    }

    fn boolean(&mut self, op: IntOp, left: ValueId, right: ValueId) -> ValueId {
        self.emit_one(
            Op::Integer(op),
            &[left, right],
            ValueType::scalar(Type::I8),
            Origin::None,
        )
    }

    fn wide_compare(
        &mut self,
        condition: Condition,
        signed: bool,
        left: &[ValueId],
        right: &[ValueId],
    ) -> ValueId {
        let high_equal = self.compare_value(Condition::Eq, false, left[1], right[1]);
        let low = self.compare_value(condition, false, left[0], right[0]);
        if condition == Condition::Eq {
            return self.boolean(IntOp::And, high_equal, low);
        }
        if condition == Condition::Ne {
            let high = self.compare_value(Condition::Ne, false, left[1], right[1]);
            return self.boolean(IntOp::Or, high, low);
        }
        let high_condition = if matches!(condition, Condition::Lt | Condition::Le) {
            Condition::Lt
        } else {
            Condition::Gt
        };
        let high = self.compare_value(high_condition, signed, left[1], right[1]);
        let low = self.boolean(IntOp::And, high_equal, low);
        self.boolean(IntOp::Or, high, low)
    }

    fn wide_binary(
        &mut self,
        op: BinaryOp,
        left: &[ValueId],
        right: &[ValueId],
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let kind = ValueType::scalar(Type::I64);
        let signed = matches!(self.kind(ty), TypeKind::Int { signed: true, .. });
        let values = match op {
            BinaryOp::Add | BinaryOp::Sub => {
                let op = if op == BinaryOp::Add {
                    IntOp::AddCarry
                } else {
                    IntOp::SubBorrow
                };
                let zero = self.constant(0, Type::I64);
                let low = self.emit(
                    Op::Integer(op),
                    &[left[0], right[0], zero],
                    &[(kind, Origin::None), (kind, Origin::None)],
                );
                let high = self.emit(
                    Op::Integer(op),
                    &[left[1], right[1], low[1]],
                    &[(kind, Origin::None), (kind, Origin::None)],
                );
                vec![low[0], high[0]]
            }
            BinaryOp::Mul => {
                let low = self.emit(
                    Op::Integer(IntOp::MulWide),
                    &[left[0], right[0]],
                    &[(kind, Origin::None), (kind, Origin::None)],
                );
                let a = self.emit_one(
                    Op::Integer(IntOp::Mul),
                    &[left[1], right[0]],
                    kind,
                    Origin::None,
                );
                let b = self.emit_one(
                    Op::Integer(IntOp::Mul),
                    &[left[0], right[1]],
                    kind,
                    Origin::None,
                );
                let high = self.emit_one(Op::Integer(IntOp::Add), &[low[1], a], kind, Origin::None);
                vec![
                    low[0],
                    self.emit_one(Op::Integer(IntOp::Add), &[high, b], kind, Origin::None),
                ]
            }
            BinaryOp::Div | BinaryOp::Rem => self.runtime(
                RuntimeCall::WideDiv {
                    signed,
                    remainder: op == BinaryOp::Rem,
                },
                &[left[0], left[1], right[0], right[1]],
                &[kind, kind],
            )?,
            BinaryOp::Shl | BinaryOp::Shr => self.wide_shift(op, left, right[0], signed),
            _ => (0..2)
                .map(|index| {
                    self.emit_one(
                        Op::Integer(integer_op(op, signed)),
                        &[left[index], right[index]],
                        kind,
                        Origin::None,
                    )
                })
                .collect(),
        };
        Ok(Computed::Values { ty, values })
    }

    fn wide_shift(
        &mut self,
        op: BinaryOp,
        value: &[ValueId],
        amount: ValueId,
        signed: bool,
    ) -> Vec<ValueId> {
        let kind = ValueType::scalar(Type::I64);
        let amount = self.integer_resize(amount, Type::I64, false);
        let zero = self.constant(0, Type::I64);
        let sixty_four = self.constant(64, Type::I64);
        let mask = self.constant(63, Type::I64);
        let small = self.compare_value(Condition::Lt, false, amount, sixty_four);
        let count = self.emit_one(Op::Integer(IntOp::And), &[amount, mask], kind, Origin::None);
        let reverse = self.emit_one(
            Op::Integer(IntOp::Sub),
            &[sixty_four, count],
            kind,
            Origin::None,
        );
        let reverse = self.emit_one(
            Op::Integer(IntOp::And),
            &[reverse, mask],
            kind,
            Origin::None,
        );
        let no_shift = self.compare_value(Condition::Eq, false, count, zero);
        let (primary, carry, first, second) = if op == BinaryOp::Shl {
            (IntOp::Shl, IntOp::ShrUnsigned, value[0], value[1])
        } else {
            (
                if signed {
                    IntOp::ShrSigned
                } else {
                    IntOp::ShrUnsigned
                },
                IntOp::Shl,
                value[1],
                value[0],
            )
        };
        let a = self.emit_one(Op::Integer(primary), &[first, count], kind, Origin::None);
        let b = self.emit_one(
            Op::Integer(if op == BinaryOp::Shl {
                IntOp::Shl
            } else {
                IntOp::ShrUnsigned
            }),
            &[second, count],
            kind,
            Origin::None,
        );
        let carry = self.emit_one(Op::Integer(carry), &[first, reverse], kind, Origin::None);
        let carry = self.emit_one(Op::Select, &[no_shift, zero, carry], kind, Origin::None);
        let b = self.emit_one(Op::Integer(IntOp::Or), &[b, carry], kind, Origin::None);
        let fill = if op == BinaryOp::Shr && signed {
            self.emit_one(
                Op::Integer(IntOp::ShrSigned),
                &[first, mask],
                kind,
                Origin::None,
            )
        } else {
            zero
        };
        let outer = self.emit_one(Op::Select, &[small, a, fill], kind, Origin::None);
        let inner = self.emit_one(Op::Select, &[small, b, a], kind, Origin::None);
        if op == BinaryOp::Shl {
            vec![outer, inner]
        } else {
            vec![inner, outer]
        }
    }

    pub(super) fn checked(
        &mut self,
        check: u32,
        kind: &CheckOpKind,
        operands: &[Operand],
        ty: u32,
    ) -> Result<Computed, Diagnostic> {
        let value = match kind {
            CheckOpKind::Division { .. } => {
                let operand = self.operand(&operands[0])?;
                let values = self.computed_values(operand)?;
                let mut value = values[0];
                if values.len() == 2 {
                    value = self.emit_one(
                        Op::Integer(IntOp::Or),
                        &values,
                        ValueType::scalar(Type::I64),
                        Origin::None,
                    );
                }
                let zero = self.constant(0, self.machine_type(value).ty);
                self.compare_value(Condition::Ne, false, value, zero)
            }
            CheckOpKind::Shift { ty: amount_ty } => {
                let signed = match self.kind(amount_ty.0) {
                    TypeKind::Int { signed, .. } => *signed,
                    _ => return Err(invalid("移位检查没有整型移位量")),
                };
                let operand = self.operand(&operands[0])?;
                let values = self.computed_values(operand)?;
                if !signed {
                    // 无符号移位量不可能为负：检查恒真，量化成常量真值。
                    self.constant(1, Type::I8)
                } else {
                    // 有符号移位量必须非负；128 位量按高字符号判定。
                    let (value, ty) = if values.len() == 2 {
                        (values[1], Type::I64)
                    } else {
                        (values[0], self.machine_type(values[0]).ty)
                    };
                    let zero = self.constant(0, ty);
                    self.compare_value(Condition::Ge, true, value, zero)
                }
            }
            CheckOpKind::Bounds { slice } => self.bounds(check, operands, *slice)?,
            CheckOpKind::UnicodeScalar => {
                let value = self.operand(&operands[0])?;
                let value = self.computed_values(value)?[0];
                let machine = self.machine_type(value).ty;
                let limit = self.constant(0x110000, machine);
                let start = self.constant(0xd800, machine);
                let end = self.constant(0xe000, machine);
                let bounded = self.compare_value(Condition::Lt, false, value, limit);
                let below = self.compare_value(Condition::Lt, false, value, start);
                let above = self.compare_value(Condition::Ge, false, value, end);
                let scalar = self.boolean(IntOp::Or, below, above);
                self.boolean(IntOp::And, bounded, scalar)
            }
            CheckOpKind::FloatToInt { signed, bits } => {
                self.float_bounds(&operands[0], *signed, *bits)?
            }
            CheckOpKind::Utf8Boundary => self.utf8_check(check)?,
        };
        Ok(Computed::Values {
            ty,
            values: vec![value],
        })
    }

    fn bounds(
        &mut self,
        check: u32,
        operands: &[Operand],
        slice: bool,
    ) -> Result<ValueId, Diagnostic> {
        let base = self.operand(&operands[0])?;
        let length = self.length(base)?;
        if !slice {
            let index = self.operand(&operands[1])?;
            let index = self.computed_values(index)?[0];
            let index = self.integer_resize(index, Type::I64, false);
            return Ok(self.compare_value(Condition::Lt, false, index, length));
        }
        let expression = &self.owner.expressions[self.owner.checks
            [usize::try_from(check).expect("检查编号")]
        .expression
        .index()];
        let crate::frontend::hir::ExprKind::Slice { start, end, .. } = expression.kind else {
            return Err(invalid("切片检查缺少已检查的端点信息"));
        };
        let mut index = 1;
        let start = if start.is_some() {
            let value = self.operand(&operands[index])?;
            index += 1;
            self.computed_values(value)?[0]
        } else {
            self.constant(0, Type::I64)
        };
        let end = if end.is_some() {
            let value = self.operand(&operands[index])?;
            self.computed_values(value)?[0]
        } else {
            length
        };
        let start = self.integer_resize(start, Type::I64, false);
        let end = self.integer_resize(end, Type::I64, false);
        let ordered = self.compare_value(Condition::Le, false, start, end);
        let bounded = self.compare_value(Condition::Le, false, end, length);
        Ok(self.boolean(IntOp::And, ordered, bounded))
    }

    fn float_bounds(
        &mut self,
        operand: &Operand,
        signed: bool,
        bits: u16,
    ) -> Result<ValueId, Diagnostic> {
        let value = self.operand(operand)?;
        let value = self.computed_values(value)?[0];
        let ty = self.machine_type(value);
        let upper = 2f64.powi(i32::from(bits) - i32::from(signed));
        let lower = if signed { -upper } else { 0.0 };
        let encode = |number: f64| {
            if ty.ty == Type::F32 {
                // 此处构造目标浮点格式的范围常量，按 IEEE 规则舍入。
                u64::from((number as f32).to_bits())
            } else {
                number.to_bits()
            }
        };
        let low = self.emit_one(Op::FConst(encode(lower)), &[], ty, Origin::None);
        let high = self.emit_one(Op::FConst(encode(upper)), &[], ty, Origin::None);
        let low = self.compare_value(Condition::Ge, false, value, low);
        let high = self.compare_value(Condition::Lt, false, value, high);
        Ok(self.boolean(IntOp::And, low, high))
    }
}

fn condition(op: CompareOp) -> Condition {
    match op {
        CompareOp::Eq => Condition::Eq,
        CompareOp::Ne => Condition::Ne,
        CompareOp::Lt => Condition::Lt,
        CompareOp::Le => Condition::Le,
        CompareOp::Gt => Condition::Gt,
        CompareOp::Ge => Condition::Ge,
    }
}
fn integer_op(op: BinaryOp, signed: bool) -> IntOp {
    match op {
        BinaryOp::Add => IntOp::Add,
        BinaryOp::Sub => IntOp::Sub,
        BinaryOp::Mul => IntOp::Mul,
        BinaryOp::Div => {
            if signed {
                IntOp::DivSigned
            } else {
                IntOp::DivUnsigned
            }
        }
        BinaryOp::Rem => {
            if signed {
                IntOp::RemSigned
            } else {
                IntOp::RemUnsigned
            }
        }
        BinaryOp::BitAnd => IntOp::And,
        BinaryOp::BitOr => IntOp::Or,
        BinaryOp::BitXor => IntOp::Xor,
        BinaryOp::Shl => IntOp::Shl,
        BinaryOp::Shr => {
            if signed {
                IntOp::ShrSigned
            } else {
                IntOp::ShrUnsigned
            }
        }
    }
}
