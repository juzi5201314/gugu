use super::*;
use std::ops::Range;

fn range(range: &Range<u32>, length: usize) -> bool {
    range.start <= range.end && range.end as usize <= length
}

impl Module {
    pub(super) fn verify_owner(&self, owner: &Owner) -> Result<(), Diagnostic> {
        if !self.definition(owner.definition)
            || owner.body != ExprId(0)
            || owner.expressions.is_empty()
            || owner.expression_types.len() != owner.expressions.len()
            || owner.expression_inputs.len() != owner.expressions.len()
            || owner.expression_inputs.iter().any(|&ty| !self.ty(ty))
            || owner.expression_types.iter().any(|&ty| !self.ty(ty))
            || owner
                .parameters
                .iter()
                .any(|id| id.index() >= owner.patterns.len())
        {
            return Err(invalid("owner 入口、参数或表达式类型侧表不完整"));
        }
        for (index, scope) in owner.scopes.iter().enumerate() {
            if !self.location(&scope.location)
                || if index == 0 {
                    scope.parent.is_some() || scope.kind != ScopeKind::Function
                } else {
                    scope.parent.is_none_or(|parent| parent.index() >= index)
                }
            {
                return Err(invalid("词法作用域不构成 owner 内的前序树"));
            }
        }
        let mut incoming = vec![false; owner.expressions.len()];
        incoming[0] = true;
        for (index, expression) in owner.expressions.iter().enumerate() {
            if !self.location(&expression.location)
                || expression.scope.index() >= owner.scopes.len()
                || expression.effects.0 & !Effects::KNOWN != 0
                || !range(&expression.adjustments, owner.adjustments.len())
                || !self.expression_edges(owner, index, &expression.kind, &mut incoming)
            {
                return Err(invalid("表达式位置、作用域、调整或操作数不合法"));
            }
            if !self.adjustment_chain(owner, index) {
                return Err(invalid("表达式调整链与输入或结果类型不一致"));
            }
            match &expression.kind {
                ExprKind::Field { base, index }
                    if self
                        .field_count(owner.expression_types[base.index()])
                        .is_none_or(|count| *index as usize >= count) =>
                {
                    return Err(invalid("字段投影超出接收者类型的字段域"));
                }
                ExprKind::Construct { variant, fields } => {
                    let count = self
                        .variant_fields(owner.expression_inputs[index], *variant)
                        .ok_or_else(|| invalid("构造操作引用未知变体"))?;
                    if owner.fields[fields.start as usize..fields.end as usize]
                        .iter()
                        .any(|field| field.field as usize >= count)
                    {
                        return Err(invalid("构造字段超出变体字段域"));
                    }
                }
                ExprKind::Exit {
                    target, cleanup, ..
                }
                | ExprKind::TryExit {
                    target, cleanup, ..
                } if !self.exit_scopes(owner, expression.scope, *target, cleanup) => {
                    return Err(invalid("退出清理没有完整覆盖当前作用域链"));
                }
                _ => {}
            }
        }
        if incoming.contains(&false) {
            return Err(invalid("owner 包含没有语义父节点的表达式"));
        }
        for statement in &owner.statements {
            if !self.location(&statement.location)
                || statement.scope.index() >= owner.scopes.len()
                || !self.statement(owner, &statement.kind)
            {
                return Err(invalid("语句引用不完整"));
            }
        }
        for (index, pattern) in owner.patterns.iter().enumerate() {
            if !self.location(&pattern.location)
                || !self.ty(pattern.ty)
                || !self.pattern(owner, index, &pattern.kind)
            {
                return Err(invalid("模式引用或绑定槽不完整"));
            }
        }
        for local in &owner.locals {
            if !self.ty(local.ty) || !self.location(&local.location) || local.storage & !7 != 0 {
                return Err(invalid("局部槽类型、位置或存储标志不合法"));
            }
        }
        self.verify_plans(owner)?;
        self.verify_cleanup(owner)
    }

    fn expression_edges(
        &self,
        owner: &Owner,
        index: usize,
        kind: &ExprKind,
        incoming: &mut [bool],
    ) -> bool {
        let mut edge = |id: ExprId| {
            if id.index() <= index || id.index() >= incoming.len() {
                return false;
            }
            incoming[id.index()] = true;
            true
        };
        let ids = |list: &Range<u32>| range(list, owner.expression_ids.len());
        let expressions =
            |list: &Range<u32>| &owner.expression_ids[list.start as usize..list.end as usize];
        let pat = |id: PatternId| id.index() < owner.patterns.len();
        let dispatch = |id: Option<u32>| id.is_none_or(|id| (id as usize) < owner.dispatches.len());
        let target = |target: ExitTarget| match target {
            ExitTarget::Return => true,
            ExitTarget::Break(id) | ExitTarget::Continue(id) => owner
                .scopes
                .get(id.index())
                .is_some_and(|scope| scope.kind == ScopeKind::Loop),
            ExitTarget::Try(id) => owner
                .scopes
                .get(id.index())
                .is_some_and(|scope| scope.kind == ScopeKind::Try),
        };
        let cleanup = |list: &Range<u32>| {
            range(list, owner.scope_ids.len())
                && owner.scope_ids[list.start as usize..list.end as usize]
                    .iter()
                    .all(|id| id.index() < owner.scopes.len())
        };
        match kind {
            ExprKind::Resolved(res) => match res {
                Res::Def(id) => self.definition(*id),
                Res::Local(id) => id.index() < owner.locals.len(),
                Res::Primitive(id) => self.ty(*id),
                Res::Builtin(_) => true,
                Res::Associated {
                    definition,
                    self_ty,
                    interface,
                } => {
                    self.definition(*definition)
                        && self.ty(*self_ty)
                        && interface
                            .as_ref()
                            .is_none_or(|interface| self.trait_ref(interface))
                }
            },
            ExprKind::Literal(_) => true,
            ExprKind::Tuple(list) | ExprKind::Array(list) => {
                ids(list) && expressions(list).iter().copied().all(&mut edge)
            }
            ExprKind::Repeat { value, .. } => edge(*value),
            ExprKind::Construct { fields, .. } => {
                range(fields, owner.fields.len())
                    && owner.fields[fields.start as usize..fields.end as usize]
                        .iter()
                        .all(|field| edge(field.value))
            }
            ExprKind::Block {
                statements,
                tail,
                end_plan,
            } => {
                if !range(statements, owner.statement_ids.len())
                    || end_plan.is_some_and(|plan| (plan as usize) >= owner.cleanup_plans.len())
                {
                    return false;
                }
                for id in &owner.statement_ids[statements.start as usize..statements.end as usize] {
                    let Some(statement) = owner.statements.get(id.index()) else {
                        return false;
                    };
                    let valid = match &statement.kind {
                        StatementKind::Let {
                            value, otherwise, ..
                        } => value.is_none_or(&mut edge) && otherwise.is_none_or(&mut edge),
                        StatementKind::Static { .. } | StatementKind::Yield => true,
                        StatementKind::Assign { place, value, .. } => edge(*place) && edge(*value),
                        StatementKind::Defer(action) => owner
                            .cleanup
                            .get(*action as usize)
                            .is_some_and(|action| edge(action.body)),
                        StatementKind::Expression(value) => edge(*value),
                    };
                    if !valid {
                        return false;
                    }
                }
                tail.is_none_or(&mut edge)
            }
            ExprKind::If {
                condition,
                then_value,
                else_value,
            } => edge(*condition) && edge(*then_value) && else_value.is_none_or(&mut edge),
            ExprKind::Match { value, arms } => {
                edge(*value)
                    && range(arms, owner.arms.len())
                    && owner.arms[arms.start as usize..arms.end as usize]
                        .iter()
                        .all(|arm| {
                            pat(arm.pattern) && arm.guard.is_none_or(&mut edge) && edge(arm.body)
                        })
            }
            ExprKind::Loop { body } => edge(*body),
            ExprKind::While { condition, body } => edge(*condition) && edge(*body),
            ExprKind::For {
                pattern,
                value,
                body,
                into_iter,
                next,
            } => {
                pat(*pattern)
                    && edge(*value)
                    && edge(*body)
                    && dispatch(*into_iter)
                    && dispatch(*next)
            }
            ExprKind::Try { body, from_value } => edge(*body) && dispatch(*from_value),
            ExprKind::TryExit {
                value,
                branch,
                from_error,
                target: exit,
                cleanup: scopes,
                plan,
            } => {
                edge(*value)
                    && dispatch(*branch)
                    && dispatch(*from_error)
                    && target(*exit)
                    && cleanup(scopes)
                    && (*plan as usize) < owner.cleanup_plans.len()
            }
            ExprKind::Select { arms } => {
                range(arms, owner.select_arms.len())
                    && owner.select_arms[arms.start as usize..arms.end as usize]
                        .iter()
                        .all(|arm| match arm {
                            SelectArm::Send {
                                channel,
                                value,
                                body,
                            } => edge(*channel) && edge(*value) && edge(*body),
                            SelectArm::Recv {
                                channel,
                                pattern,
                                body,
                            } => edge(*channel) && pat(*pattern) && edge(*body),
                            SelectArm::Wait {
                                join,
                                pattern,
                                body,
                            } => edge(*join) && pat(*pattern) && edge(*body),
                            SelectArm::Default { body } => edge(*body),
                        })
            }
            ExprKind::Closure { definition } | ExprKind::Spawn { definition } => {
                self.definition(*definition)
                    && self
                        .owners
                        .iter()
                        .any(|child| child.definition == *definition)
            }
            ExprKind::Call {
                target,
                receiver,
                arguments,
            }
            | ExprKind::SpawnCall {
                target,
                receiver,
                arguments,
            } => {
                (match target {
                    CallTarget::Value(value) => edge(*value),
                    CallTarget::Dispatch(id) => dispatch(Some(*id)),
                    CallTarget::Builtin(_) => true,
                    CallTarget::Constructor { ty, .. } => self.ty(*ty),
                }) && receiver.is_none_or(&mut edge)
                    && ids(arguments)
                    && expressions(arguments).iter().copied().all(&mut edge)
            }
            ExprKind::Intrinsic {
                arguments, types, ..
            } => {
                ids(arguments)
                    && expressions(arguments).iter().copied().all(&mut edge)
                    && types.iter().all(|&ty| self.ty(ty))
            }
            ExprKind::Field { base, .. } => edge(*base),
            ExprKind::Index {
                base,
                index,
                read,
                write,
            } => edge(*base) && edge(*index) && dispatch(*read) && dispatch(*write),
            ExprKind::Slice { base, start, end } => {
                edge(*base) && start.is_none_or(&mut edge) && end.is_none_or(&mut edge)
            }
            ExprKind::Unary { value, .. } | ExprKind::Comptime { value } => edge(*value),
            ExprKind::Binary {
                left,
                right,
                dispatch: call,
                ..
            } => edge(*left) && edge(*right) && dispatch(*call),
            ExprKind::Range { start, end } => edge(*start) && edge(*end),
            ExprKind::Assembly(plan) => owner
                .assembly
                .get(*plan as usize)
                .is_some_and(|plan| plan.operands.iter().all(|operand| edge(operand.value))),
            ExprKind::Exit {
                target: exit,
                value,
                cleanup: scopes,
                plan,
            } => {
                target(*exit)
                    && value.is_none_or(&mut edge)
                    && cleanup(scopes)
                    && (*plan as usize) < owner.cleanup_plans.len()
            }
            ExprKind::String { parts } => {
                range(parts, owner.string_parts.len())
                    && owner.string_parts[parts.start as usize..parts.end as usize]
                        .iter()
                        .all(|part| match part {
                            StringPart::Text(_) => true,
                            StringPart::Value {
                                expression,
                                format,
                                dispatch: call,
                            } => {
                                edge(*expression)
                                    && dispatch(*call)
                                    && format.width.iter().chain(&format.precision).all(|count| {
                                        match count {
                                            FormatCount::Fixed(value) => *value <= i64::MAX as u64,
                                            FormatCount::Value(value) => {
                                                edge(*value)
                                                    && self.types[owner.expression_types
                                                        [value.index()]
                                                    .index()]
                                                        == (Type::Int {
                                                            signed: true,
                                                            bits: 64,
                                                        })
                                            }
                                        }
                                    })
                            }
                        })
            }
            ExprKind::LetCondition { pattern, value } => pat(*pattern) && edge(*value),
        }
    }

    fn statement(&self, owner: &Owner, kind: &StatementKind) -> bool {
        let expr = |id: ExprId| id.index() < owner.expressions.len();
        match kind {
            StatementKind::Let {
                pattern,
                value,
                otherwise,
            } => {
                pattern.index() < owner.patterns.len()
                    && value.is_none_or(expr)
                    && otherwise.is_none_or(expr)
            }
            StatementKind::Static { local, definition } => {
                local.index() < owner.locals.len() && self.definition(*definition)
            }
            StatementKind::Assign {
                place,
                value,
                dispatch,
                ..
            } => {
                expr(*place)
                    && expr(*value)
                    && dispatch.is_none_or(|id| (id as usize) < owner.dispatches.len())
            }
            StatementKind::Defer(id) => (*id as usize) < owner.cleanup.len(),
            StatementKind::Yield => true,
            StatementKind::Expression(value) => expr(*value),
        }
    }

    fn pattern(&self, owner: &Owner, index: usize, kind: &PatternKind) -> bool {
        let child = |id: PatternId| id.index() > index && id.index() < owner.patterns.len();
        let children = |list: &Range<u32>| {
            range(list, owner.pattern_ids.len())
                && owner.pattern_ids[list.start as usize..list.end as usize]
                    .iter()
                    .copied()
                    .all(child)
        };
        let local = |id: LocalId| id.index() < owner.locals.len();
        match kind {
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::Range { .. } => true,
            PatternKind::Bind(id) => local(*id),
            PatternKind::Ref(id) => child(*id),
            PatternKind::Tuple(ids) | PatternKind::Or(ids) => children(ids),
            PatternKind::Array {
                prefix,
                rest,
                suffix,
                ..
            } => children(prefix) && rest.is_none_or(local) && children(suffix),
            PatternKind::Construct { fields, .. } => {
                range(fields, owner.pattern_fields.len())
                    && owner.pattern_fields[fields.start as usize..fields.end as usize]
                        .iter()
                        .all(|field| child(field.pattern))
            }
            PatternKind::At { local: id, pattern } => local(*id) && child(*pattern),
        }
    }

    fn verify_plans(&self, owner: &Owner) -> Result<(), Diagnostic> {
        let expr = |id: ExprId| id.index() < owner.expressions.len();
        for capture in &owner.captures {
            if capture.local.index() >= owner.locals.len()
                || self.definitions[owner.definition.index()].parent != Some(capture.owner)
                || self
                    .owners
                    .iter()
                    .find(|parent| parent.definition == capture.owner)
                    .is_none_or(|parent| {
                        parent
                            .locals
                            .get(capture.source.index())
                            .is_none_or(|source| {
                                source.ty != owner.locals[capture.local.index()].ty
                            })
                    })
            {
                return Err(invalid("捕获引用不属于有效 owner/local 槽"));
            }
        }
        for cleanup in &owner.cleanup {
            if cleanup.statement.index() >= owner.statements.len()
                || !expr(cleanup.body)
                || cleanup.scope.index() >= owner.scopes.len()
                || cleanup
                    .captures
                    .iter()
                    .any(|id| id.index() >= owner.locals.len())
            {
                return Err(invalid("清理注册缺少 body、作用域或捕获槽"));
            }
        }
        for adjustment in &owner.adjustments {
            if matches!(adjustment, Adjustment::NeverTo(ty) | Adjustment::Erase(ty) | Adjustment::Opaque(ty) | Adjustment::ArrayToSlice(ty) | Adjustment::Instantiate(ty) if !self.ty(*ty))
            {
                return Err(invalid("调整引用无效类型"));
            }
        }
        for dispatch in &owner.dispatches {
            if dispatch.function.is_some_and(|id| !self.definition(id))
                || dispatch
                    .implementation
                    .is_some_and(|id| !self.definition(id))
                || dispatch
                    .interface
                    .as_ref()
                    .is_some_and(|interface| !self.trait_ref(interface))
                || !self.ty(dispatch.self_ty)
                || !self.ty(dispatch.signature)
                || dispatch.dynamic
                    && (dispatch.function.is_some()
                        || dispatch.interface.is_none()
                        || dispatch.member.is_none())
            {
                return Err(invalid("派发记录没有唯一已解析目标"));
            }
        }
        for check in &owner.checks {
            let valid = match check.kind {
                CheckKind::Division { ty, divisor } => self.ty(ty) && expr(divisor),
                CheckKind::Shift { ty, amount } => self.ty(ty) && expr(amount),
                CheckKind::FloatToInt { value, bits, .. } => {
                    expr(value) && matches!(bits, 8 | 16 | 32 | 64 | 128)
                }
                CheckKind::UnicodeScalar { value } => expr(value),
                CheckKind::Bounds { .. } | CheckKind::Utf8Boundary => true,
            };
            if !expr(check.expression) || !valid {
                return Err(invalid("运行时检查引用未知表达式或类型"));
            }
        }
        for call in &owner.variadic_calls {
            if !expr(call.expression) || !self.ty(call.element) {
                return Err(invalid("参数包计划引用未知调用或元素类型"));
            }
        }
        for call in &owner.foreign_calls {
            if !expr(call.expression)
                || !matches!(
                    owner.expressions[call.expression.index()].kind,
                    ExprKind::Call { .. } | ExprKind::SpawnCall { .. }
                )
            {
                return Err(invalid("外部调用效应没有实际调用节点"));
            }
        }
        for borrow in &owner.borrow_constraints {
            if !expr(borrow.expression) || !self.ty(borrow.base) || !self.ty(borrow.target) {
                return Err(invalid("引用约束没有完整的源与目标类型"));
            }
        }
        for assembly in &owner.assembly {
            if assembly.clobbers >> 34 != 0
                || assembly
                    .operands
                    .iter()
                    .any(|operand| !expr(operand.value) || operand.register.index >= 16)
            {
                return Err(invalid("汇编计划包含无效寄存器或操作数"));
            }
        }
        Ok(())
    }
}

impl Module {
    fn field_count(&self, mut ty: TypeId) -> Option<usize> {
        while let Type::Ref(inner) = self.types[ty.index()] {
            ty = inner;
        }
        match &self.types[ty.index()] {
            Type::Tuple(fields) => Some(fields.len()),
            Type::Named { definition, .. } => self
                .aggregates
                .iter()
                .find(|aggregate| aggregate.definition == *definition)?
                .variants
                .first()
                .map(|variant| variant.fields.len()),
            _ => None,
        }
    }
    fn variant_fields(&self, ty: TypeId, variant: u32) -> Option<usize> {
        match &self.types[ty.index()] {
            Type::Named { definition, .. } => self
                .aggregates
                .iter()
                .find(|aggregate| aggregate.definition == *definition)?
                .variants
                .get(variant as usize)
                .map(|variant| variant.fields.len()),
            Type::Option(_) => match variant {
                0 => Some(1),
                1 => Some(0),
                _ => None,
            },
            Type::Result(..) if variant < 2 => Some(1),
            _ => None,
        }
    }
    fn exit_scopes(
        &self,
        owner: &Owner,
        from: ScopeId,
        target: ExitTarget,
        scopes: &Range<u32>,
    ) -> bool {
        if !range(scopes, owner.scope_ids.len()) {
            return false;
        }
        let mut cursor = Some(from);
        let list = &owner.scope_ids[scopes.start as usize..scopes.end as usize];
        for &scope in list {
            if cursor != Some(scope)
                || matches!(target, ExitTarget::Continue(target) if target == scope)
            {
                return false;
            }
            cursor = if matches!(target, ExitTarget::Break(target) | ExitTarget::Try(target) if target == scope)
            {
                None
            } else {
                owner.scopes[scope.index()].parent
            };
        }
        match target {
            ExitTarget::Return => cursor.is_none(),
            ExitTarget::Break(target) | ExitTarget::Try(target) => {
                cursor.is_none() && list.last() == Some(&target)
            }
            ExitTarget::Continue(target) => cursor == Some(target),
        }
    }

    fn adjustment_chain(&self, owner: &Owner, index: usize) -> bool {
        let mut current = owner.expression_inputs[index];
        let expression = &owner.expressions[index];
        for adjustment in &owner.adjustments
            [expression.adjustments.start as usize..expression.adjustments.end as usize]
        {
            current = match *adjustment {
                Adjustment::Dereference => match self.types[current.index()] {
                    Type::Ref(inner) => inner,
                    _ => return false,
                },
                Adjustment::ArrayToSlice(target) => {
                    if !self.ty(target) {
                        return false;
                    }
                    let mut array = current;
                    while let Type::Ref(inner) = self.types[array.index()] {
                        array = inner;
                    }
                    let (Type::Array(element, _), Type::Ref(slice)) =
                        (&self.types[array.index()], &self.types[target.index()])
                    else {
                        return false;
                    };
                    if !matches!(self.types[slice.index()], Type::Slice(wanted) if *element == wanted)
                    {
                        return false;
                    }
                    target
                }
                Adjustment::NeverTo(target) => {
                    if !self.ty(target) || self.types[current.index()] != Type::Never {
                        return false;
                    }
                    target
                }
                Adjustment::Erase(target) => {
                    if !self.ty(target)
                        || !matches!(
                            self.types[target.index()],
                            Type::Dyn(_) | Type::Function { .. }
                        )
                    {
                        return false;
                    }
                    target
                }
                Adjustment::Opaque(target) => {
                    if !self.ty(target)
                        || !matches!(self.types[target.index()], Type::Opaque { .. })
                    {
                        return false;
                    }
                    target
                }
                Adjustment::Instantiate(target) => {
                    if !self.ty(target) {
                        return false;
                    }
                    let (
                        Type::Callable {
                            definition: source, ..
                        },
                        Type::Callable {
                            definition: target_definition,
                            ..
                        },
                    ) = (&self.types[current.index()], &self.types[target.index()])
                    else {
                        return false;
                    };
                    if source != target_definition {
                        return false;
                    }
                    target
                }
            };
        }
        current == owner.expression_types[index]
    }
}
