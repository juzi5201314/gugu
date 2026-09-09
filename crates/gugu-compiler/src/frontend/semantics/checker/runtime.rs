//! runtime publish 原语按规范路径登记，目标位置与句柄值在同一类型约束下检查。
use super::super::model::RuntimeIntrinsic;
use super::super::output::RuntimeOperation;
use super::*;

impl Checker<'_, '_> {
    pub(super) fn runtime_call(
        &mut self,
        callee: ExprId,
        path: PathId,
        type_args: AstRange<GenericArg>,
        args: &[ExprId],
        expected: Option<&Ty>,
    ) -> Option<Ty> {
        let segments = self.arena().paths[usize::try_from(path.0).expect("路径下标")]
            .segments
            .as_slice(&self.arena().segments);
        if segments
            .first()
            .is_some_and(|segment| self.state.names.contains_key(&segment.name))
        {
            return None;
        }
        let path = self
            .model
            .external_path(self.module, &self.model.path(self.module, path))?;
        let kind = RuntimeIntrinsic::from_path(&path)?;
        let type_args = if type_args.len != 0 {
            type_args
        } else {
            segments
                .iter()
                .rev()
                .find(|segment| segment.args.len != 0)
                .map_or(AstRange::empty(), |segment| segment.args)
        };
        let span = &self.arena().exprs[usize::try_from(callee.0).expect("表达式下标")].span;
        if type_args.len > 1 {
            self.error(
                DiagnosticCode::InvalidExpression,
                "运行时 publish 原语的类型实参数量不符",
                span.clone(),
            );
            return Some(Ty::Error);
        }
        if args.len() != 2 {
            self.error(
                DiagnosticCode::InvalidExpression,
                "运行时 publish 原语需要目标位置与值",
                span.clone(),
            );
            return Some(Ty::Error);
        }
        let destination_ty = self.place(args[0], false);
        let ty = if let Some(&argument) = type_args.as_slice(&self.arena().generic_args).first() {
            match self.model.form_argument(self.module, argument) {
                Ok(ty) => ty,
                Err(error) => {
                    self.errors.push(error);
                    return Some(Ty::Error);
                }
            }
        } else {
            destination_ty.clone()
        };
        self.unify(&destination_ty, &ty, span);
        let value_ty = self.expression(args[1], Some(&destination_ty));
        self.unify(&value_ty, &ty, span);
        if !matches!(
            self.normalized(&ty),
            Ty::Ref(_)
                | Ty::Slice(_)
                | Ty::Chan(_)
                | Ty::Join(_)
                | Ty::Function(..)
                | Ty::Callable(..)
                | Ty::Dyn(_)
                | Ty::Error
                | Ty::Var(_)
                | Ty::Param(_)
                | Ty::Projection(..)
                | Ty::Opaque(..)
        ) {
            self.error(
                DiagnosticCode::InvalidExpression,
                "runtime publish 原语的值必须是受管句柄类型（&T、切片、chan、Join、函数值或 dyn）",
                span.clone(),
            );
        }
        if let Some(expected) = expected {
            self.unify(&Ty::Unit, expected, span);
        }
        self.require_value_captures(callee);
        for &argument in args {
            self.require_value_captures(argument);
        }
        self.runtime_operations.push(RuntimeOperation {
            expression: callee,
            kind,
            ty,
            arguments: args.to_vec(),
        });
        Some(Ty::Unit)
    }
}
