//! unsafe 契约属于声明身份；擦除签名不能把不安全调用伪装成安全调用。
use super::super::ast::{ExprId, ExprKind, ItemKind};
use super::model::{Model, Ty};
use super::output::{CheckKind, RuntimeCheck};
use super::traits::Member;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Property {
    Bit,
    Managed,
}

impl Model<'_> {
    pub(super) fn callable_is_unsafe(&self, ty: &Ty) -> bool {
        matches!(ty, Ty::Callable(id, _, _) if self.modules[id.module].arena.fns[id.function as usize].unsafety)
    }

    pub(super) fn member_is_unsafe(&self, member: &Member) -> bool {
        member.definition.is_some_and(|definition| {
            let arena = &self.modules[definition.module].arena;
            matches!(arena.items[definition.item.0 as usize].kind, ItemKind::Function(id) if arena.fns[id.0 as usize].unsafety)
        })
    }

    pub(crate) fn is_union(&self, ty: &Ty) -> bool {
        let Ty::Named(index, _) = ty.deref() else {
            return false;
        };
        let definition = self.nominal[*index].definition;
        matches!(
            self.modules[definition.module].arena.items[definition.item.0 as usize].kind,
            ItemKind::Union { .. }
        )
    }

    pub(crate) fn is_bit_type(&self, ty: &Ty, hidden: &[Option<Ty>]) -> Option<bool> {
        self.type_property(ty, hidden, Property::Bit)
    }

    pub(crate) fn has_managed_value(&self, ty: &Ty, hidden: &[Option<Ty>]) -> Option<bool> {
        self.type_property(ty, hidden, Property::Managed)
    }

    pub(crate) fn check_proven(
        &self,
        module: usize,
        check: &RuntimeCheck,
        expressions: &[(ExprId, Ty)],
    ) -> bool {
        let ty = |id: ExprId| {
            expressions
                .binary_search_by_key(&id.0, |(id, _)| id.0)
                .ok()
                .map(|index| &expressions[index].1)
        };
        match check.kind {
            CheckKind::IntegerDivision { divisor, .. } => self
                .constant_int(module, divisor)
                .is_ok_and(|value| value != 0),
            CheckKind::Shift { amount, .. } => {
                matches!(ty(amount), Some(Ty::Int { signed: false, .. }))
                    || self
                        .constant_int(module, amount)
                        .is_ok_and(|value| value >= 0)
            }
            CheckKind::UnicodeScalar { value } => self
                .constant_int(module, value)
                .ok()
                .and_then(|value| u32::try_from(value).ok())
                .and_then(char::from_u32)
                .is_some(),
            CheckKind::FloatToInt {
                signed,
                bits,
                value,
            } => {
                let Some(source) = ty(value) else {
                    return false;
                };
                let value = match self.constant_value(module, value, source) {
                    Ok(super::model::ConstantValue::Float(bits)) => f64::from_bits(bits),
                    Ok(super::model::ConstantValue::Int(value)) => {
                        if *source == Ty::Float(32) {
                            f64::from(value as f32)
                        } else {
                            value as f64
                        }
                    }
                    _ => return false,
                };
                let bound = 2_f64.powi(i32::from(bits) - i32::from(signed));
                value.is_finite()
                    && value.trunc() >= if signed { -bound } else { 0.0 }
                    && value.trunc() < bound
            }
            CheckKind::Bounds { slice: false } => {
                let arena = &self.modules[module].arena;
                let ExprKind::Index {
                    base,
                    index: super::super::ast::IndexKind::Expr(index),
                } = arena.exprs[check.expression.0 as usize].kind
                else {
                    return false;
                };
                let Some(Ty::Array(_, len)) = ty(base).map(Ty::deref) else {
                    return false;
                };
                self.constant_int(module, index)
                    .is_ok_and(|value| value >= 0 && value < i128::from(*len))
            }
            _ => false,
        }
    }

    // 调用前由布局形成检查排除无限大小类型；未知泛型留下待实例化的义务。
    fn type_property(&self, ty: &Ty, hidden: &[Option<Ty>], property: Property) -> Option<bool> {
        match ty {
            Ty::Error | Ty::Var(_) | Ty::Param(_) | Ty::Projection(..) => None,
            Ty::Opaque(id, arguments) => {
                let ty = hidden[*id as usize].as_ref()?;
                let concrete = super::model::substitute(ty, &self.opaque_bindings(*id, arguments));
                self.type_property(&concrete, hidden, property)
            }
            Ty::String => Some(property == Property::Managed),
            Ty::Ref(_)
            | Ty::Slice(_)
            | Ty::Chan(_)
            | Ty::Join(_)
            | Ty::Function(..)
            | Ty::Callable(..)
            | Ty::Dyn(_) => Some(false),
            Ty::Array(_, 0) => Some(property == Property::Bit),
            Ty::Array(inner, _) | Ty::Option(inner) | Ty::MaybeUninit(inner) => {
                self.type_property(inner, hidden, property)
            }
            Ty::Tuple(types) => self.aggregate_property(types.iter(), hidden, property),
            Ty::Result(value, error) => {
                self.aggregate_property([&**value, &**error].into_iter(), hidden, property)
            }
            Ty::Named(..) => {
                let variants = self.variants(ty).expect("名义类型已形成");
                self.aggregate_property(
                    variants
                        .iter()
                        .flat_map(|variant| variant.fields.iter().map(|field| &field.ty)),
                    hidden,
                    property,
                )
            }
            Ty::Never
            | Ty::Unit
            | Ty::Bool
            | Ty::Char
            | Ty::Int { .. }
            | Ty::Float(_)
            | Ty::Ptr(_)
            | Ty::TypeId
            | Ty::Range => Some(property == Property::Bit),
        }
    }

    fn aggregate_property<'t>(
        &self,
        types: impl Iterator<Item = &'t Ty>,
        hidden: &[Option<Ty>],
        property: Property,
    ) -> Option<bool> {
        let decisive = property == Property::Managed;
        let mut unknown = false;
        for ty in types {
            match self.type_property(ty, hidden, property) {
                Some(value) if value == decisive => return Some(decisive),
                None => unknown = true,
                _ => {}
            }
        }
        if unknown { None } else { Some(!decisive) }
    }
}
