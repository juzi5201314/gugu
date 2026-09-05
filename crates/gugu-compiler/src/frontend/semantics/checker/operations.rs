use super::super::output::{CheckKind, RuntimeCheck};
use super::*;

impl Checker<'_, '_> {
    pub(super) fn record_check(&mut self, expression: ExprId, kind: CheckKind) {
        self.runtime_checks.push(RuntimeCheck { expression, kind });
    }
    pub(super) fn binary(
        &mut self,
        id: ExprId,
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
        expected: Option<&Ty>,
    ) -> Ty {
        let hint = if matches!(op, BinOp::And | BinOp::Or) {
            Some(Ty::Bool)
        } else {
            expected
                .filter(|_| {
                    !matches!(
                        op,
                        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                    )
                })
                .cloned()
        };
        let left = self.expression(lhs, hint.as_ref());
        let before = self.state.clone();
        let right = self.expression(
            rhs,
            if matches!(op, BinOp::Shl | BinOp::Shr) || left == Ty::Never {
                None
            } else {
                Some(&left)
            },
        );
        if matches!(op, BinOp::And | BinOp::Or) {
            let after = self.state.clone();
            self.merge(&before, &[before.clone(), after]);
        }
        let span = &self.arena().exprs[id.0 as usize].span;
        if matches!(op, BinOp::And | BinOp::Or) {
            self.unify(&left, &Ty::Bool, span);
            self.unify(&right, &Ty::Bool, span);
            return if left == Ty::Never {
                Ty::Never
            } else {
                Ty::Bool
            };
        }
        self.operation_type(id, op, &left, &right, span)
    }

    pub(super) fn operation_type(
        &mut self,
        id: ExprId,
        op: BinOp,
        left: &Ty,
        right: &Ty,
        span: &Span,
    ) -> Ty {
        if *left == Ty::Never || *right == Ty::Never {
            return Ty::Never;
        }
        if matches!(op, BinOp::Shl | BinOp::Shr) {
            if self.is_integer(left) && self.is_integer(right) {
                self.record_check(id, CheckKind::Shift { ty: left.clone() });
                return left.clone();
            }
            self.error(
                DiagnosticCode::InvalidExpression,
                "移位要求两个整数操作数",
                span.clone(),
            );
            Ty::Error
        } else {
            let ty = self.binary_type(op, left, right, span);
            if matches!(op, BinOp::Div | BinOp::Rem) && self.is_integer(&ty) {
                self.record_check(id, CheckKind::IntegerDivision { ty: ty.clone() });
            }
            ty
        }
    }
}
