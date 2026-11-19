use super::*;

impl Module {
    pub(super) fn verify_array_init(
        &self,
        owner: &Owner,
        pattern: PatternId,
        body: ExprId,
        plan: &ArrayInitLoop,
    ) -> bool {
        let Some(array) = owner.locals.get(plan.array.index()) else {
            return false;
        };
        if owner.locals.get(plan.index.index()).is_none()
            || !matches!(self.types.get(array.ty.index()), Some(Type::Array(_, length)) if *length == plan.length)
            || !matches!(owner.patterns.get(pattern.index()).map(|pat| &pat.kind), Some(PatternKind::Bind(local)) if *local == plan.index)
        {
            return false;
        }
        let Some(ExprKind::Block { statements, .. }) =
            owner.expressions.get(body.index()).map(|expr| &expr.kind)
        else {
            return false;
        };
        let Some(statements) = owner
            .statement_ids
            .get(statements.start as usize..statements.end as usize)
        else {
            return false;
        };
        if !statements.contains(&plan.store) {
            return false;
        }
        let Some(StatementKind::Assign {
            place,
            operation: crate::ast::AssignOp::Assign,
            dispatch: None,
            ..
        }) = owner
            .statements
            .get(plan.store.index())
            .map(|stmt| &stmt.kind)
        else {
            return false;
        };
        let Some(ExprKind::Index { base, index, .. }) =
            owner.expressions.get(place.index()).map(|expr| &expr.kind)
        else {
            return false;
        };
        matches!(owner.expressions.get(base.index()).map(|expr| &expr.kind), Some(ExprKind::Resolved(Res::Local(local))) if *local == plan.array)
            && matches!(owner.expressions.get(index.index()).map(|expr| &expr.kind), Some(ExprKind::Resolved(Res::Local(local))) if *local == plan.index)
    }
}
