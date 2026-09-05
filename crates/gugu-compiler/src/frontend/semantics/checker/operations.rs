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
        let left = self.expression(
            lhs,
            if matches!(
                self.arena().exprs[lhs.0 as usize].kind,
                ExprKind::Literal(_) | ExprKind::Unary { op: UnOp::Neg, .. }
            ) {
                hint.as_ref()
            } else {
                None
            },
        );
        let before = self.state.clone();
        let right = self.expression(
            rhs,
            if matches!(op, BinOp::Shl | BinOp::Shr)
                || left == Ty::Never
                || !(self.is_number(&left)
                    || self.builtin_comparable(&left)
                    || matches!(self.resolve(&left), Ty::Var(_)))
            {
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
        if !self.is_number(left)
            && !self.builtin_comparable(left)
            && !matches!(op, BinOp::And | BinOp::Or)
        {
            return self.trait_operator(id, op, left, right, span);
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
    pub(super) fn user_index(&mut self, id: ExprId, ty: &Ty, write: bool, span: &Span) -> Ty {
        let name = if write { "index_set" } else { "index" };
        let Some(method) = self.language_method(id, ty.deref(), "Index", Vec::new(), name, span)
        else {
            return Ty::Error;
        };
        let (parameters, ret) = method.signature.signature().expect("Index 方法签名");
        if write {
            parameters[2].clone()
        } else {
            ret.clone()
        }
    }

    pub(super) fn compound_trait(
        &mut self,
        id: ExprId,
        op: AssignOp,
        left: &Ty,
        right: &Ty,
        span: &Span,
    ) {
        let (interface, method) = match op {
            AssignOp::Add => ("AddAssign", "add_assign"),
            AssignOp::Sub => ("SubAssign", "sub_assign"),
            AssignOp::Mul => ("MulAssign", "mul_assign"),
            AssignOp::Div => ("DivAssign", "div_assign"),
            AssignOp::Rem => ("RemAssign", "rem_assign"),
            AssignOp::BitAnd => ("BitAndAssign", "bitand_assign"),
            AssignOp::BitOr => ("BitOrAssign", "bitor_assign"),
            AssignOp::BitXor => ("BitXorAssign", "bitxor_assign"),
            AssignOp::Shl => ("ShlAssign", "shl_assign"),
            AssignOp::Shr => ("ShrAssign", "shr_assign"),
            AssignOp::Assign => unreachable!("直接赋值不走操作符 trait"),
        };
        let right = self.pattern_type(right);
        if let Some(method) = self.language_method(
            id,
            &self.resolve(left),
            interface,
            vec![right.clone()],
            method,
            span,
        ) {
            let (params, ret) = method.signature.signature().expect("复合赋值签名");
            self.unify(&right, &params[1], span);
            self.unify(ret, &Ty::Unit, span);
        }
    }

    pub(super) fn index_writeback(&mut self, id: ExprId, span: &Span) {
        let target = self
            .dispatches
            .iter()
            .find(|dispatch| {
                dispatch.expression == id
                    && dispatch.interface.as_ref().is_some_and(|interface| {
                        self.model.traits.interfaces[interface.id].name == "Index"
                    })
            })
            .map(|dispatch| dispatch.self_ty.clone());
        if let Some(target) = target {
            self.user_index(id, &target, true, span);
        }
    }

    pub(super) fn language_method(
        &mut self,
        id: ExprId,
        ty: &Ty,
        name: &str,
        arguments: Vec<Ty>,
        method: &str,
        span: &Span,
    ) -> Option<super::super::traits::Method> {
        let interface = super::super::traits::TraitRef {
            id: self
                .model
                .traits
                .interfaces
                .iter()
                .position(|t| t.definition.is_none() && t.name == name)
                .expect("语言 trait 已注册"),
            arguments,
        };
        let selected = self
            .model
            .assumptions_at(self.module, span)
            .and_then(|assumptions| {
                self.model
                    .method(self.module, ty, Some(&interface), method, &assumptions)
            });
        let selected = match selected {
            Ok(Some(method)) => method,
            Ok(None) => {
                self.error(
                    DiagnosticCode::InvalidType,
                    format!("没有适用的 {name} 实现"),
                    span.clone(),
                );
                return None;
            }
            Err(error) => {
                self.error(error.code(), error.message(), span.clone());
                return None;
            }
        };
        if let Some(callable) = selected.callable {
            if let Some(def) = self.model.function_definition(callable) {
                if !self.dependencies.contains(&def) {
                    self.dependencies.push(def);
                }
            }
        }
        self.dispatches.push(super::super::output::Dispatch {
            expression: id,
            callable: selected.callable,
            implementation: selected.implementation,
            interface: selected.interface.clone(),
            member: selected.member,
            self_ty: ty.clone(),
            signature: selected.signature.clone(),
            dereferences: 0,
            borrow: selected.receiver
                && selected
                    .signature
                    .signature()
                    .is_some_and(|(params, _)| matches!(params.first(), Some(Ty::Ref(_)))),
            implicit_receiver: selected.receiver,
        });
        Some(selected)
    }
    pub(super) fn try_parts(&mut self, ty: &Ty, span: &Span) -> (Ty, Ty) {
        let ty = self.resolve(ty);
        match &ty {
            Ty::Option(value) => return ((**value).clone(), Ty::Unit),
            Ty::Result(value, error) => return ((**value).clone(), (**error).clone()),
            Ty::Error => return (Ty::Error, Ty::Error),
            _ => {}
        }
        let interface = super::super::traits::TraitRef {
            id: self
                .model
                .traits
                .interfaces
                .iter()
                .position(|interface| interface.definition.is_none() && interface.name == "Try")
                .expect("Try 已登记"),
            arguments: Vec::new(),
        };
        let result = self
            .model
            .assumptions_at(self.module, span)
            .and_then(|assumptions| {
                self.model.require_trait(&ty, &interface, &assumptions)?;
                let value = self.model.normalize(
                    &Ty::Projection(Box::new(ty.clone()), interface.clone(), "Value".into()),
                    &assumptions,
                )?;
                let error = self.model.normalize(
                    &Ty::Projection(Box::new(ty.clone()), interface, "Error".into()),
                    &assumptions,
                )?;
                Ok((value, error))
            });
        match result {
            Ok(parts) => parts,
            Err(error) => {
                self.error(error.code(), error.message(), span.clone());
                (Ty::Error, Ty::Error)
            }
        }
    }

    pub(super) fn user_iterator(&mut self, expression: ExprId, ty: &Ty) -> Ty {
        let span = &self.arena().exprs[expression.0 as usize].span;
        let Some(into_iter) =
            self.language_method(expression, ty, "IntoIter", Vec::new(), "into_iter", span)
        else {
            return Ty::Error;
        };
        let (_, iterator) = into_iter.signature.signature().expect("IntoIter 签名");
        let Some(next) =
            self.language_method(expression, iterator, "Iter", Vec::new(), "next", span)
        else {
            return Ty::Error;
        };
        let (_, result) = next.signature.signature().expect("Iter 签名");
        let Ty::Option(element) = result else {
            unreachable!("Iter 契约已验证")
        };
        (**element).clone()
    }
}
