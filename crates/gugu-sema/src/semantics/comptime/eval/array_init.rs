use super::*;
use crate::ast::{TyId, TyKind};

impl Model<'_> {
    pub(super) fn uninitialized_array(
        &self,
        module: usize,
        ty: Option<TyId>,
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let Some(ty) = ty else {
            return Ok(ConstantValue::Unit);
        };
        let arena = &self.modules[module].arena;
        let TyKind::Array { len, .. } = arena.tys[ty.0 as usize].kind else {
            return Ok(ConstantValue::Unit);
        };
        let count = self.int_operand(module, len, state)?;
        let span = &arena.tys[ty.0 as usize].span;
        let count = usize::try_from(count)
            .map_err(|_| budget_error(span, &state.calls, "数组长度必须非负且适配宿主"))?;
        state.alloc((count as u64).saturating_mul(64), span)?;
        Ok(ConstantValue::Array(vec![ConstantValue::Unit; count]))
    }

    pub(super) fn assign_array_element(
        &self,
        module: usize,
        base: ExprId,
        index: ExprId,
        value: ConstantValue,
        state: &mut EvalState,
    ) -> Result<(), Diagnostic> {
        let arena = &self.modules[module].arena;
        let span = &arena.exprs[base.0 as usize].span;
        let fail = || {
            Diagnostic::error(
                DiagnosticCode::InvalidExpression,
                "comptime 数组赋值要求有效的局部数组和下标",
                Some(span.clone()),
            )
        };
        let ExprKind::Path(path) = arena.exprs[base.0 as usize].kind else {
            return Err(fail());
        };
        let names = self.path(module, path);
        let [name] = names.as_slice() else {
            return Err(fail());
        };
        let index = usize::try_from(
            self.int_operand(module, index, state)
                .map_err(|error| error.diagnostic(span))?,
        )
        .map_err(|_| fail())?;
        let Some(ConstantValue::Array(elements)) = state.slot_mut(name) else {
            return Err(fail());
        };
        let element = elements.get_mut(index).ok_or_else(fail)?;
        *element = value;
        Ok(())
    }
}
