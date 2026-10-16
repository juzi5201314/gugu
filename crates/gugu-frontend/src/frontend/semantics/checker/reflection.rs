use super::super::super::ast::{AstRange, ExprId, GenericArg};
use super::super::model::Ty;
use super::super::output::{Reflection, ReflectionKind};
use super::super::traits::{Obligation, TraitRef};
use super::Checker;
use crate::DiagnosticCode;

impl Checker<'_, '_> {
    pub(super) fn reflection_method(
        &mut self,
        expression: ExprId,
        receiver: &Ty,
        name: &str,
        types: AstRange<GenericArg>,
        arguments: &[ExprId],
        expected: Option<&Ty>,
    ) -> Option<Ty> {
        let span = &self.arena().exprs[expression.0 as usize].span;
        let mut receiver = receiver;
        while let Ty::Ref(inner) = receiver {
            receiver = inner;
        }
        if *receiver == Ty::TypeId && matches!(name, "name" | "as_int") {
            if types.len != 0 {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "TypeId 方法不接收类型实参",
                    span.clone(),
                );
            }
            let (kind, result) = if name == "name" {
                (ReflectionKind::TypeName, Ty::String)
            } else {
                (ReflectionKind::TypeAsInt, Ty::int())
            };
            self.reflections.push(Reflection { expression, kind });
            return Some(self.invoke(
                expression,
                Ty::Function(Vec::new(), Box::new(result)),
                arguments.to_vec(),
                None,
                expected,
            ));
        }
        let any = self
            .model
            .traits
            .interfaces
            .iter()
            .position(|interface| interface.definition.is_none() && interface.name == "Any")
            .expect("Any 已登记");
        if !matches!(receiver, Ty::Dyn(interfaces) if interfaces.iter().any(|interface| interface.id == any))
            || !matches!(name, "is" | "downcast" | "downcast_copy")
        {
            return None;
        }
        let types = types.as_slice(&self.arena().generic_args);
        if types.len() > 1 {
            self.error(
                DiagnosticCode::InvalidExpression,
                "Any 恢复操作只接收一个目标类型",
                span.clone(),
            );
            return Some(Ty::Error);
        }
        let target = match types.first() {
            Some(argument) => match self.model.form_argument(self.module, *argument) {
                Ok(ty) => ty,
                Err(error) => {
                    self.errors.push(error);
                    return Some(Ty::Error);
                }
            },
            None => self.fresh(),
        };
        let (kind, result) = match name {
            "is" => (ReflectionKind::Is(target.clone()), Ty::Bool),
            "downcast" => (
                ReflectionKind::Downcast(target.clone()),
                Ty::Option(Box::new(Ty::Ref(Box::new(target.clone())))),
            ),
            "downcast_copy" => (
                ReflectionKind::DowncastCopy(target.clone()),
                Ty::Option(Box::new(target.clone())),
            ),
            _ => unreachable!("已筛选 Any 恢复方法"),
        };
        match self.model.assumptions_at(self.module, span) {
            Ok(assumptions) => self.trait_constraints.push((
                Obligation {
                    ty: target,
                    interface: TraitRef {
                        id: any,
                        arguments: Vec::new(),
                    },
                    span: span.clone(),
                },
                assumptions,
            )),
            Err(error) => self.errors.push(error),
        }
        self.reflections.push(Reflection { expression, kind });
        Some(self.invoke(
            expression,
            Ty::Function(Vec::new(), Box::new(result)),
            arguments.to_vec(),
            None,
            expected,
        ))
    }
}
