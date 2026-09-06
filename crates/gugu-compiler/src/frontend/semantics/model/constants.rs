use super::super::super::ast::{BinOp, ExprId, ExprKind, ItemKind, LitKind, UnOp};
use super::super::traits::MemberKind;
use super::{DefRef, Model, Ty};
use crate::Diagnostic;
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in super::super) enum ConstantValue {
    Int(i128),
    Float(u64),
    Bool(bool),
    String(String),
}

impl Model<'_> {
    pub(crate) fn constant_type(&self, module: usize, expr: ExprId) -> Result<Ty, Diagnostic> {
        self.constant_type_inner(module, expr, &mut Vec::new())
    }

    fn constant_type_inner(
        &self,
        module: usize,
        expr: ExprId,
        stack: &mut Vec<DefRef>,
    ) -> Result<Ty, Diagnostic> {
        match self.modules[module].arena.exprs[usize::try_from(expr.0).expect("表达式下标")].kind
        {
            ExprKind::Literal(LitKind::Int { .. }) => Ok(Ty::int()),
            ExprKind::Literal(LitKind::Char { .. }) => Ok(Ty::Char),
            ExprKind::Literal(LitKind::ByteChar { .. }) => Ok(Ty::Int {
                signed: false,
                bits: 8,
            }),
            ExprKind::Literal(LitKind::Bool(_)) => Ok(Ty::Bool),
            ExprKind::Literal(LitKind::Float { .. }) => Ok(Ty::Float(64)),
            ExprKind::Literal(LitKind::String { .. } | LitKind::RawString { .. }) => Ok(Ty::String),
            ExprKind::Paren(inner) | ExprKind::Unary { expr: inner, .. } => {
                self.constant_type_inner(module, inner, stack)
            }
            ExprKind::Binary { lhs, rhs, op } => {
                let left = self.constant_type_inner(module, lhs, stack)?;
                let right = self.constant_type_inner(module, rhs, stack)?;
                if left != right {
                    return Err(self.error(module, "常量操作数类型不一致"));
                }
                Ok(
                    if matches!(
                        op,
                        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                    ) {
                        Ty::Bool
                    } else {
                        left
                    },
                )
            }
            ExprKind::Path(path) => {
                let def = match self
                    .constant_member(module, path)?
                    .and_then(|member| member.definition)
                {
                    Some(def) => def,
                    None => self.resolve(module, &self.path(module, path))?,
                };
                if stack.contains(&def) {
                    return Err(self.error(module, "常量类型推断形成循环"));
                }
                match self.modules[def.module].arena.items
                    [usize::try_from(def.item.0).expect("项下标")]
                .kind
                {
                    ItemKind::Const { ty: Some(ty), .. } | ItemKind::Static { ty, .. } => {
                        self.form(def.module, ty)
                    }
                    ItemKind::Const {
                        value: Some(value), ..
                    } => {
                        stack.push(def);
                        let ty = self.constant_type_inner(def.module, value, stack);
                        stack.pop();
                        ty
                    }
                    _ => Err(self.error(module, "端点不是常量")),
                }
            }
            _ => Err(self.error(module, "无法形成常量类型")),
        }
    }
    pub(crate) fn constant_int(
        &self,
        module: usize,
        expression: ExprId,
    ) -> Result<i128, Diagnostic> {
        match self.constant_value(module, expression, &Ty::int())? {
            ConstantValue::Int(value) => Ok(value),
            _ => Err(self.error(module, "需要整数常量")),
        }
    }
    pub(in super::super) fn constant_value(
        &self,
        module: usize,
        expression: ExprId,
        ty: &Ty,
    ) -> Result<ConstantValue, Diagnostic> {
        self.constant_value_inner(module, expression, ty, &mut Vec::new())
    }
    fn constant_value_inner(
        &self,
        module: usize,
        expression: ExprId,
        ty: &Ty,
        stack: &mut Vec<DefRef>,
    ) -> Result<ConstantValue, Diagnostic> {
        let arena = &self.modules[module].arena;
        let fail = || self.error(module, "需要可求值的编译期表达式");
        match arena.exprs[expression.0 as usize].kind {
            ExprKind::Literal(LitKind::Bool(value)) => Ok(ConstantValue::Bool(value)),
            ExprKind::Literal(LitKind::String { text } | LitKind::RawString { text }) => {
                Ok(ConstantValue::String(
                    super::super::super::string::decode_string(self.name(module, text))
                        .into_owned(),
                ))
            }
            ExprKind::Literal(LitKind::Float { digits, exp10 }) => {
                let value = format!("{}e{exp10}", self.name(module, digits))
                    .parse::<f64>()
                    .map_err(|_| fail())?;
                Ok(ConstantValue::Float(if *ty == Ty::Float(32) {
                    f64::from(value as f32).to_bits()
                } else {
                    value.to_bits()
                }))
            }
            ExprKind::Literal(LitKind::Int { limbs, .. }) => {
                let mut value = 0i128;
                for &limb in limbs.as_slice(&arena.int_limbs).iter().rev() {
                    value = value
                        .checked_mul(1i128 << 32)
                        .and_then(|value| value.checked_add(i128::from(limb)))
                        .ok_or_else(fail)?;
                }
                Ok(ConstantValue::Int(value))
            }
            ExprKind::Literal(LitKind::Char { value, .. }) => {
                Ok(ConstantValue::Int(i128::from(value as u32)))
            }
            ExprKind::Literal(LitKind::ByteChar { value, .. }) => {
                Ok(ConstantValue::Int(i128::from(value)))
            }
            ExprKind::Paren(inner) | ExprKind::Comptime(inner) => {
                self.constant_value_inner(module, inner, ty, stack)
            }
            ExprKind::Unary { op, expr } => {
                match (op, self.constant_value_inner(module, expr, ty, stack)?) {
                    (UnOp::Not, ConstantValue::Bool(value)) => Ok(ConstantValue::Bool(!value)),
                    (UnOp::Neg, ConstantValue::Int(value)) => value
                        .checked_neg()
                        .map(|value| in_type(ConstantValue::Int(value), ty))
                        .ok_or_else(fail),
                    (UnOp::BitNot, ConstantValue::Int(value)) => {
                        Ok(in_type(ConstantValue::Int(!value), ty))
                    }
                    (UnOp::Neg, ConstantValue::Float(bits)) => {
                        Ok(ConstantValue::Float((-f64::from_bits(bits)).to_bits()))
                    }
                    _ => Err(fail()),
                }
            }
            ExprKind::Binary { lhs, rhs, op } => {
                let left = self.constant_value_inner(module, lhs, ty, stack)?;
                if matches!(
                    (&left, op),
                    (ConstantValue::Bool(false), BinOp::And)
                        | (ConstantValue::Bool(true), BinOp::Or)
                ) {
                    return Ok(left);
                }
                let right = self.constant_value_inner(module, rhs, ty, stack)?;
                evaluate(op, left, right)
                    .map(|value| in_type(value, ty))
                    .ok_or_else(fail)
            }
            ExprKind::Path(path) => {
                let member = self.constant_member(module, path)?;
                if let Some(member) = &member {
                    if let MemberKind::Const {
                        value: Some(value), ..
                    } = &member.kind
                    {
                        return Ok(value.clone());
                    }
                }
                let definition = match member.and_then(|member| member.definition) {
                    Some(def) => def,
                    None => self.resolve(module, &self.path(module, path))?,
                };
                if stack.contains(&definition) {
                    return Err(self.trait_error(definition, "关联常量形成循环"));
                }
                let ItemKind::Const {
                    ty: declared_ty,
                    value: Some(value),
                } = self.modules[definition.module].arena.items[definition.item.0 as usize].kind
                else {
                    return Err(fail());
                };
                stack.push(definition);
                let declared_ty = match declared_ty {
                    Some(ty) => self.form(definition.module, ty)?,
                    None => self.constant_type(definition.module, value)?,
                };
                let result =
                    self.constant_value_inner(definition.module, value, &declared_ty, stack);
                stack.pop();
                result
            }
            _ => Err(fail()),
        }
    }
}
fn in_type(value: ConstantValue, ty: &Ty) -> ConstantValue {
    match (value, ty) {
        (ConstantValue::Float(bits), Ty::Float(32)) => {
            ConstantValue::Float(f64::from(f64::from_bits(bits) as f32).to_bits())
        }
        (ConstantValue::Int(value), Ty::Int { signed, bits }) if *bits < 128 => {
            debug_assert!(*bits > 0, "整数类型至少占一位");
            let value = if *signed {
                (value << (128 - bits)) >> (128 - bits)
            } else {
                value & ((1i128 << bits) - 1)
            };
            ConstantValue::Int(value)
        }
        (value, _) => value,
    }
}
fn evaluate(op: BinOp, a: ConstantValue, b: ConstantValue) -> Option<ConstantValue> {
    use ConstantValue::{Bool, Float, Int, String};
    Some(match (a, b) {
        (Int(a), Int(b)) => match op {
            BinOp::Add => Int(a.checked_add(b)?),
            BinOp::Sub => Int(a.checked_sub(b)?),
            BinOp::Mul => Int(a.checked_mul(b)?),
            BinOp::Div => Int(a.checked_div(b)?),
            BinOp::Rem => Int(a.checked_rem(b)?),
            BinOp::BitAnd => Int(a & b),
            BinOp::BitOr => Int(a | b),
            BinOp::BitXor => Int(a ^ b),
            BinOp::Eq => Bool(a == b),
            BinOp::Ne => Bool(a != b),
            BinOp::Lt => Bool(a < b),
            BinOp::Le => Bool(a <= b),
            BinOp::Gt => Bool(a > b),
            BinOp::Ge => Bool(a >= b),
            _ => return None,
        },
        (Bool(a), Bool(b)) => match op {
            BinOp::And => Bool(a && b),
            BinOp::Or => Bool(a || b),
            BinOp::Eq => Bool(a == b),
            BinOp::Ne => Bool(a != b),
            _ => return None,
        },
        (String(mut a), String(b)) => match op {
            BinOp::Add => {
                a.push_str(&b);
                String(a)
            }
            BinOp::Eq => Bool(a == b),
            BinOp::Ne => Bool(a != b),
            _ => return None,
        },
        (Float(a), Float(b)) => {
            let (a, b) = (f64::from_bits(a), f64::from_bits(b));
            match op {
                BinOp::Add => Float((a + b).to_bits()),
                BinOp::Sub => Float((a - b).to_bits()),
                BinOp::Mul => Float((a * b).to_bits()),
                BinOp::Div => Float((a / b).to_bits()),
                BinOp::Rem => Float((a % b).to_bits()),
                BinOp::Eq => Bool(a == b),
                BinOp::Ne => Bool(a != b),
                BinOp::Lt => Bool(a < b),
                BinOp::Le => Bool(a <= b),
                BinOp::Gt => Bool(a > b),
                BinOp::Ge => Bool(a >= b),
                _ => return None,
            }
        }
        _ => return None,
    })
}
