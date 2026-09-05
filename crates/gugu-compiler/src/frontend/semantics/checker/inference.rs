//! 字面量在约束收集完毕后才选择宽度，避免用默认 int 产生隐式转换。
use super::*;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum NumberKind {
    Any,
    Integer,
    Signed,
    Float,
}

impl Checker<'_, '_> {
    pub(super) fn number_kind(&self, ty: &Ty) -> NumberKind {
        match self.resolve(ty) {
            Ty::Var(id) => self.number_kinds[id as usize],
            Ty::Int { signed: true, .. } => NumberKind::Signed,
            Ty::Int { signed: false, .. } => NumberKind::Integer,
            Ty::Float(_) => NumberKind::Float,
            _ => NumberKind::Any,
        }
    }

    pub(super) fn is_integer(&self, ty: &Ty) -> bool {
        matches!(
            self.number_kind(ty),
            NumberKind::Integer | NumberKind::Signed
        )
    }

    pub(super) fn is_number(&self, ty: &Ty) -> bool {
        self.number_kind(ty) != NumberKind::Any
    }

    pub(super) fn builtin_comparable(&self, ty: &Ty) -> bool {
        match self.resolve(ty) {
            Ty::Bool | Ty::Char | Ty::String | Ty::Unit => true,
            Ty::Tuple(ts) => ts.iter().all(|t| self.builtin_comparable(t)),
            Ty::Array(t, _) => self.builtin_comparable(&t),
            ty => self.is_number(&ty),
        }
    }

    pub(super) fn require_signed(&mut self, ty: &Ty, span: &Span) -> bool {
        match self.resolve(ty) {
            Ty::Var(id) if self.is_integer(ty) => {
                self.number_kinds[id as usize] = NumberKind::Signed;
                true
            }
            Ty::Int { signed: true, .. } | Ty::Float(_) => true,
            Ty::Var(_) if self.number_kind(ty) == NumberKind::Float => true,
            Ty::Error => true,
            _ => {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "负号要求有符号整数或浮点数",
                    span.clone(),
                );
                false
            }
        }
    }

    pub(super) fn number_literal(
        &mut self,
        lit: LitKind,
        negative: bool,
        expected: Option<&Ty>,
        span: &Span,
    ) -> Ty {
        let ty = self.fresh();
        let Ty::Var(id) = ty else { unreachable!() };
        self.number_kinds[id as usize] = if matches!(lit, LitKind::Float { .. }) {
            NumberKind::Float
        } else if negative {
            NumberKind::Signed
        } else {
            NumberKind::Integer
        };
        let result =
            expected.map_or_else(|| ty.clone(), |expected| self.unify(&ty, expected, span));
        self.literals.push((ty, lit, negative, span.clone()));
        result
    }

    pub(super) fn constrain_number(&mut self, id: u32, ty: &Ty, span: &Span) -> bool {
        let left = self.number_kinds[id as usize];
        let right = self.number_kind(ty);
        if left == NumberKind::Any {
            return true;
        }
        let merged = match (left, right) {
            (left, NumberKind::Any) if matches!(ty, Ty::Var(_)) => Some(left),
            (NumberKind::Integer, NumberKind::Signed)
            | (NumberKind::Signed, NumberKind::Integer) => Some(NumberKind::Signed),
            (left, right) if left == right => Some(left),
            _ => None,
        };
        if let Some(kind) = merged {
            if let Ty::Var(other) = ty {
                self.number_kinds[*other as usize] = kind;
            } else if left == NumberKind::Signed && matches!(ty, Ty::Int { signed: false, .. }) {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "负整数不能推断为无符号类型",
                    span.clone(),
                );
                return false;
            }
            true
        } else {
            self.error(
                DiagnosticCode::InvalidExpression,
                "数值字面量不能隐式转换到目标类型",
                span.clone(),
            );
            false
        }
    }

    pub(super) fn pattern_type(&mut self, ty: &Ty) -> Ty {
        match self.resolve(ty) {
            Ty::Var(id) => match self.number_kinds[id as usize] {
                NumberKind::Integer | NumberKind::Signed => {
                    self.vars[id as usize] = Some(Ty::int())
                }
                NumberKind::Float => self.vars[id as usize] = Some(Ty::Float(64)),
                NumberKind::Any => {}
            },
            Ty::Ref(t) | Ty::Ptr(t) | Ty::Slice(t) | Ty::Array(t, _) | Ty::Option(t) => {
                self.pattern_type(&t);
            }
            Ty::Tuple(ts) | Ty::Named(_, ts) => {
                for t in ts {
                    self.pattern_type(&t);
                }
            }
            Ty::Result(t, e) => {
                self.pattern_type(&t);
                self.pattern_type(&e);
            }
            _ => {}
        }
        self.resolve(ty)
    }
    pub(super) fn finish_inference(&mut self) {
        for index in 0..self.vars.len() {
            if self.vars[index].is_none() {
                self.vars[index] = match self.number_kinds[index] {
                    NumberKind::Integer | NumberKind::Signed => Some(Ty::int()),
                    NumberKind::Float => Some(Ty::Float(64)),
                    NumberKind::Any => None,
                };
            }
        }
        for (ty, lit, negative, span) in std::mem::take(&mut self.literals) {
            if matches!(lit, LitKind::Int { .. })
                && super::super::numeric::literal_ordinal(
                    self.arena(),
                    lit,
                    negative,
                    &self.resolve(&ty),
                )
                .is_none()
            {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "整数字面量超出目标类型范围",
                    span,
                );
            }
        }
        let mut patterns = std::mem::take(&mut self.pattern_plans);
        for pattern in &mut patterns {
            pattern.ty = self.resolve(&pattern.ty);
        }
        self.pattern_plans = patterns;
        let mut checks = std::mem::take(&mut self.runtime_checks);
        for check in &mut checks {
            match &mut check.kind {
                super::super::output::CheckKind::IntegerDivision { ty }
                | super::super::output::CheckKind::Shift { ty } => *ty = self.resolve(ty),
                _ => {}
            }
        }
        self.runtime_checks = checks;
    }
}
