//! 控制流出口的清理计划：出口按程序顺序登记请求，全部注册已知后统一物化动作序列。
use super::*;

/// 一次出口请求：出口种类、出发作用域以及请求时已注册的 action 数量。
pub(super) struct PendingPlan {
    exit: hir::ExitKind,
    from: hir::ScopeId,
    registered: usize,
}

impl BodyBuilder<'_, '_, '_, '_> {
    /// 登记一个出口计划；动作序列在 `finish` 时按全部注册物化。
    pub(super) fn request_plan(
        &mut self,
        exit: hir::ExitKind,
        from: hir::ScopeId,
    ) -> Result<u32, Diagnostic> {
        let id = checked_id(self.pending_plans.len())?;
        self.pending_plans.push(PendingPlan {
            exit,
            from,
            registered: self.output.cleanup.len(),
        });
        Ok(id)
    }

    /// 把 HIR 出口目标映射为计划种类。
    pub(super) fn exit_kind(target: hir::ExitTarget) -> hir::ExitKind {
        match target {
            hir::ExitTarget::Return => hir::ExitKind::Return,
            hir::ExitTarget::Break(scope) => hir::ExitKind::Break(scope),
            hir::ExitTarget::Continue(scope) => hir::ExitKind::Continue(scope),
            hir::ExitTarget::Try(scope) => hir::ExitKind::Try(scope),
        }
    }

    /// 注册点到函数作用域之间的控制结构决定函数出口 action 的注册表示。
    pub(super) fn registration_of(&self, mut scope: hir::ScopeId) -> hir::Registration {
        let mut registration = hir::Registration::Static;
        loop {
            match self.output.scopes[scope.index()].kind {
                hir::ScopeKind::Loop => return hir::Registration::Chain,
                hir::ScopeKind::Branch | hir::ScopeKind::Try => {
                    registration = hir::Registration::Flag;
                }
                hir::ScopeKind::Function | hir::ScopeKind::Block => {}
            }
            let Some(parent) = self.output.scopes[scope.index()].parent else {
                return registration;
            };
            scope = parent;
        }
    }

    /// 当前块作用域是否已注册块 action；决定 `Block.end_plan` 是否存在。
    pub(super) fn scope_has_block_actions(&self, scope: hir::ScopeId) -> bool {
        self.output
            .cleanup
            .iter()
            .any(|cleanup| !cleanup.function_exit && cleanup.scope == scope)
    }

    /// 物化全部计划：块 action 按作用域链由内向外 LIFO，函数出口 action 在函数边界 LIFO。
    pub(super) fn materialize_plans(&mut self) -> Result<(), Diagnostic> {
        let has_chain = self
            .output
            .cleanup
            .iter()
            .any(|cleanup| cleanup.registration == hir::Registration::Chain);
        let pending = std::mem::take(&mut self.pending_plans);
        for plan in pending {
            let start = checked_id(self.output.cleanup_actions.len())?;
            let scopes = self.exit_chain(plan.exit, plan.from);
            for scope in scopes {
                self.push_block_actions(scope, plan.registered);
            }
            if matches!(plan.exit, hir::ExitKind::Return | hir::ExitKind::Unwind(_)) {
                self.push_function_actions(plan.registered, has_chain);
            }
            let end = checked_id(self.output.cleanup_actions.len())?;
            self.output.cleanup_plans.push(hir::CleanupPlan {
                exit: plan.exit,
                actions: start..end,
                destination: self.plan_destination(plan.exit),
            });
        }
        Ok(())
    }

    fn exit_chain(&self, exit: hir::ExitKind, from: hir::ScopeId) -> Vec<hir::ScopeId> {
        let mut chain = Vec::new();
        if let hir::ExitKind::BlockEnd(scope) = exit {
            chain.push(scope);
            return chain;
        }
        let mut scope = from;
        loop {
            if matches!(exit, hir::ExitKind::Continue(target) if target == scope) {
                break;
            }
            chain.push(scope);
            if matches!(exit, hir::ExitKind::Break(target) | hir::ExitKind::Try(target) if target == scope)
            {
                break;
            }
            let Some(parent) = self.output.scopes[scope.index()].parent else {
                break;
            };
            scope = parent;
        }
        chain
    }

    fn push_block_actions(&mut self, scope: hir::ScopeId, registered: usize) {
        for (index, cleanup) in self.output.cleanup[..registered].iter().enumerate().rev() {
            if !cleanup.function_exit && cleanup.scope == scope {
                self.output
                    .cleanup_actions
                    .push(hir::CleanupAction::Action {
                        cleanup: index as u32,
                        guard: hir::Registration::Static,
                    });
            }
        }
    }

    fn push_function_actions(&mut self, registered: usize, has_chain: bool) {
        for (index, cleanup) in self.output.cleanup[..registered].iter().enumerate().rev() {
            if !cleanup.function_exit || cleanup.registration == hir::Registration::Chain {
                continue;
            }
            if has_chain {
                self.output
                    .cleanup_actions
                    .push(hir::CleanupAction::DrainChain {
                        until: Some(index as u32),
                    });
            }
            self.output
                .cleanup_actions
                .push(hir::CleanupAction::Action {
                    cleanup: index as u32,
                    guard: cleanup.registration,
                });
        }
        if has_chain {
            self.output
                .cleanup_actions
                .push(hir::CleanupAction::DrainChain { until: None });
        }
    }

    fn plan_destination(&self, exit: hir::ExitKind) -> Option<hir::ScopeId> {
        match exit {
            hir::ExitKind::Return | hir::ExitKind::Unwind(_) => None,
            hir::ExitKind::Continue(scope) => Some(scope),
            hir::ExitKind::Break(scope)
            | hir::ExitKind::Try(scope)
            | hir::ExitKind::BlockEnd(scope) => self.output.scopes[scope.index()].parent,
        }
    }
}
