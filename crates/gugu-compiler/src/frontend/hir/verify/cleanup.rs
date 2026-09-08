//! 清理计划校验：注册表示、出口计划的动作序列与作用域归属必须与规范一致。
use super::*;

impl Module {
    pub(super) fn verify_cleanup(&self, owner: &Owner) -> Result<(), Diagnostic> {
        let has_chain = owner
            .cleanup
            .iter()
            .any(|cleanup| cleanup.registration == Registration::Chain);
        for cleanup in &owner.cleanup {
            let site = owner.statements[cleanup.statement.index()].scope;
            let expected_scope = if cleanup.function_exit {
                ScopeId(0)
            } else {
                site
            };
            let expected_registration = if cleanup.function_exit {
                registration_of(owner, site)
            } else {
                Registration::Static
            };
            if cleanup.scope != expected_scope
                || cleanup.registration != expected_registration
                || !plan_is(owner, cleanup.unwind_plan, ExitKind::Unwind(site))
            {
                return Err(invalid("清理注册的作用域、注册表示或 Unwind 计划不一致"));
            }
        }
        for (index, scope) in owner.scopes.iter().enumerate() {
            if !plan_is(
                owner,
                scope.unwind_plan,
                ExitKind::Unwind(ScopeId(index as u32)),
            ) {
                return Err(invalid("作用域入口没有对应的 Unwind 计划"));
            }
        }
        for plan in &owner.cleanup_plans {
            self.verify_plan(owner, plan, has_chain)?;
        }
        for expression in &owner.expressions {
            let valid = match &expression.kind {
                ExprKind::Exit { target, plan, .. } | ExprKind::TryExit { target, plan, .. } => {
                    plan_is(owner, *plan, exit_kind(*target))
                }
                ExprKind::Block { end_plan, .. } => match end_plan {
                    Some(plan) => plan_is(owner, *plan, ExitKind::BlockEnd(expression.scope)),
                    None => !owner
                        .cleanup
                        .iter()
                        .any(|cleanup| !cleanup.function_exit && cleanup.scope == expression.scope),
                },
                _ => true,
            };
            if !valid {
                return Err(invalid("控制流出口没有匹配的清理计划"));
            }
        }
        Ok(())
    }

    fn verify_plan(
        &self,
        owner: &Owner,
        plan: &CleanupPlan,
        has_chain: bool,
    ) -> Result<(), Diagnostic> {
        let scope_kind = |scope: ScopeId, kind: ScopeKind| {
            owner
                .scopes
                .get(scope.index())
                .is_some_and(|entry| entry.kind == kind)
        };
        let (valid_exit, destination) = match plan.exit {
            ExitKind::Return => (true, None),
            ExitKind::Unwind(scope) => (scope.index() < owner.scopes.len(), None),
            ExitKind::Continue(scope) => (scope_kind(scope, ScopeKind::Loop), Some(scope)),
            ExitKind::Break(scope) => (scope_kind(scope, ScopeKind::Loop), parent(owner, scope)),
            ExitKind::Try(scope) => (scope_kind(scope, ScopeKind::Try), parent(owner, scope)),
            ExitKind::BlockEnd(scope) => {
                (scope_kind(scope, ScopeKind::Block), parent(owner, scope))
            }
        };
        if !valid_exit
            || plan.destination != destination
            || plan.actions.start > plan.actions.end
            || plan.actions.end as usize > owner.cleanup_actions.len()
        {
            return Err(invalid("清理计划的出口、目标作用域或动作范围不合法"));
        }
        let actions =
            &owner.cleanup_actions[plan.actions.start as usize..plan.actions.end as usize];
        let function_segment = actions
            .iter()
            .position(|action| !is_block_action(owner, action))
            .unwrap_or(actions.len());
        let (block_actions, function_actions) = actions.split_at(function_segment);
        verify_block_actions(owner, plan.exit, block_actions)?;
        match plan.exit {
            ExitKind::Return | ExitKind::Unwind(_) => {
                verify_function_actions(owner, function_actions, has_chain)
            }
            _ if function_actions.is_empty() => Ok(()),
            _ => Err(invalid("局部出口计划包含函数出口动作")),
        }
    }
}

fn is_block_action(owner: &Owner, action: &CleanupAction) -> bool {
    matches!(action, CleanupAction::Action { cleanup, guard: Registration::Static }
        if owner.cleanup.get(*cleanup as usize).is_some_and(|cleanup| !cleanup.function_exit))
}

fn verify_block_actions(
    owner: &Owner,
    exit: ExitKind,
    actions: &[CleanupAction],
) -> Result<(), Diagnostic> {
    let mut previous: Option<(ScopeId, u32)> = None;
    for action in actions {
        let CleanupAction::Action { cleanup, .. } = action else {
            return Err(invalid("块动作段包含链消费"));
        };
        let scope = owner.cleanup[*cleanup as usize].scope;
        let in_domain = match exit {
            ExitKind::Return => true,
            ExitKind::Unwind(from) => is_ancestor_or_self(owner, scope, from),
            ExitKind::Break(target) | ExitKind::Try(target) => {
                is_ancestor_or_self(owner, target, scope)
            }
            ExitKind::Continue(target) => {
                scope != target && is_ancestor_or_self(owner, target, scope)
            }
            ExitKind::BlockEnd(target) => scope == target,
        };
        // 由内向外：后续动作的作用域是前一动作作用域的祖先或同层且注册更早。
        let ordered = previous.is_none_or(|(last_scope, last_cleanup)| {
            if last_scope == scope {
                *cleanup < last_cleanup
            } else {
                is_ancestor_or_self(owner, scope, last_scope) && scope != last_scope
            }
        });
        if !in_domain || !ordered {
            return Err(invalid("块清理动作不属于出口作用域链或顺序错误"));
        }
        previous = Some((scope, *cleanup));
    }
    Ok(())
}

/// 函数出口段：无链时为站点 LIFO；有链时每个站点前有 `DrainChain{Some(站点)}`，末尾 `DrainChain{None}`。
fn verify_function_actions(
    owner: &Owner,
    actions: &[CleanupAction],
    has_chain: bool,
) -> Result<(), Diagnostic> {
    let mut previous = None;
    let mut index = 0;
    while index < actions.len() {
        let site = if has_chain {
            let CleanupAction::DrainChain { until } = &actions[index] else {
                return Err(invalid("函数出口站点缺少前置链消费"));
            };
            let Some(site) = until else {
                return if index + 1 == actions.len() {
                    Ok(())
                } else {
                    Err(invalid("消费到链底后不能再有动作"))
                };
            };
            index += 1;
            Some(*site)
        } else {
            None
        };
        let Some(CleanupAction::Action { cleanup, guard }) = actions.get(index) else {
            return Err(invalid("链消费没有对应的函数出口站点"));
        };
        if site.is_some_and(|site| site != *cleanup) {
            return Err(invalid("链消费的站点与后续动作不一致"));
        }
        let entry = owner
            .cleanup
            .get(*cleanup as usize)
            .ok_or_else(|| invalid("函数出口动作引用未知注册"))?;
        if !entry.function_exit
            || entry.registration == Registration::Chain
            || *guard != entry.registration
            || previous.is_some_and(|last| *cleanup >= last)
        {
            return Err(invalid("函数出口动作的注册表示或顺序不合法"));
        }
        previous = Some(*cleanup);
        index += 1;
    }
    if has_chain {
        return Err(invalid("函数出口计划必须以消费到链底结束"));
    }
    Ok(())
}

fn plan_is(owner: &Owner, plan: u32, exit: ExitKind) -> bool {
    owner
        .cleanup_plans
        .get(plan as usize)
        .is_some_and(|plan| plan.exit == exit)
}

fn exit_kind(target: ExitTarget) -> ExitKind {
    match target {
        ExitTarget::Return => ExitKind::Return,
        ExitTarget::Break(scope) => ExitKind::Break(scope),
        ExitTarget::Continue(scope) => ExitKind::Continue(scope),
        ExitTarget::Try(scope) => ExitKind::Try(scope),
    }
}

fn parent(owner: &Owner, scope: ScopeId) -> Option<ScopeId> {
    owner
        .scopes
        .get(scope.index())
        .and_then(|scope| scope.parent)
}

fn is_ancestor_or_self(owner: &Owner, ancestor: ScopeId, mut scope: ScopeId) -> bool {
    loop {
        if scope == ancestor {
            return true;
        }
        match parent(owner, scope) {
            Some(next) => scope = next,
            None => return false,
        }
    }
}

fn registration_of(owner: &Owner, mut scope: ScopeId) -> Registration {
    let mut registration = Registration::Static;
    loop {
        match owner.scopes[scope.index()].kind {
            ScopeKind::Loop => return Registration::Chain,
            ScopeKind::Branch | ScopeKind::Try => registration = Registration::Flag,
            ScopeKind::Function | ScopeKind::Block => {}
        }
        match parent(owner, scope) {
            Some(next) => scope = next,
            None => return registration,
        }
    }
}
