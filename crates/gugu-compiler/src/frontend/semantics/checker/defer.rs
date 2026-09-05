use super::*;

#[derive(Clone)]
pub(super) struct Deferred {
    id: usize,
    body: ExprId,
    names: BTreeMap<Symbol, usize>,
    function_exit: bool,
    execute_body: bool,
}

impl Deferred {
    pub(super) const fn function_exit(&self) -> bool {
        self.function_exit
    }
}

impl Checker<'_, '_> {
    pub(super) fn register_defer(&mut self, statement: StmtId, body: ExprId, ret: bool) {
        let expression = &self.arena().exprs[body.0 as usize];
        if self.in_cleanup {
            self.error(
                DiagnosticCode::InvalidExpression,
                "defer 体不能再次注册 defer",
                expression.span.clone(),
            );
            return;
        }
        let id = self.cleanup_plan.len();
        match expression.kind {
            ExprKind::Call {
                callee,
                type_args,
                args,
            } => {
                let ty = self.call(callee, type_args, args, None);
                self.expressions.push((body, ty));
                self.defers.push(Deferred {
                    id,
                    body,
                    names: self.state.names.clone(),
                    function_exit: ret,
                    execute_body: false,
                });
            }
            ExprKind::Block { .. } => {
                // 注册时检查体内类型，未初始化读取留给真正离开作用域的路径。
                let saved = self.state.clone();
                let loops = std::mem::take(&mut self.loops);
                let tries = std::mem::take(&mut self.tries);
                self.state.reachable = false;
                self.in_cleanup = true;
                self.expression(body, None);
                self.in_cleanup = false;
                self.state = saved;
                self.loops = loops;
                self.tries = tries;
                self.defers.push(Deferred {
                    id,
                    body,
                    names: self.state.names.clone(),
                    function_exit: ret,
                    execute_body: true,
                });
            }
            _ => self.error(
                DiagnosticCode::InvalidExpression,
                "defer 必须注册调用或块",
                expression.span.clone(),
            ),
        }
        self.state.cleanup_paths.insert(
            id,
            CleanupPath {
                initialized: self.state.initialized.clone(),
                mandatory: true,
            },
        );
        self.cleanup_plan
            .push(super::super::output::CleanupRegistration {
                statement,
                body,
                function_exit: ret,
                captures: self.state.names.values().copied().collect(),
            });
    }

    pub(super) fn run_cleanups(&mut self, floor: usize, function_exit: bool) {
        if self.in_cleanup || !self.state.reachable {
            return;
        }
        let entries: Vec<_> = self.defers[floor..]
            .iter()
            .rev()
            .filter(|d| !d.function_exit)
            .chain(
                self.defers[floor..]
                    .iter()
                    .rev()
                    .filter(|d| function_exit && d.function_exit),
            )
            .cloned()
            .collect();
        let saved = self.state.clone();
        let loops = std::mem::take(&mut self.loops);
        let tries = std::mem::take(&mut self.tries);
        self.in_cleanup = true;
        for deferred in entries {
            let Some(path) = self.state.cleanup_paths.remove(&deferred.id) else {
                continue;
            };
            if deferred.execute_body {
                let initialized = std::mem::replace(&mut self.state.initialized, path.initialized);
                let pending = self.state.cleanup_paths.clone();
                self.state.names = deferred.names;
                self.expression(deferred.body, Some(&Ty::Unit));
                if !path.mandatory {
                    self.state.initialized = initialized;
                    self.state.cleanup_paths = pending;
                    self.state.reachable = true;
                }
            }
        }
        self.in_cleanup = false;
        self.state.names = saved.names;
        self.loops = loops;
        self.tries = tries;
    }
}
