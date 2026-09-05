//! 捕获记录引用原槽；构造闭包与执行其 body 使用分离的数据流状态。
use super::super::{
    model::CallableId,
    output::{ADDRESS_TAKEN, CAPTURED, CROSS_COROUTINE, CapturePlan, CapturedSlot},
};
use super::*;

pub(super) struct CaptureFrame {
    slots: Vec<Option<CapturedSlot>>,
    coroutine: bool,
}

struct SavedExecution {
    state: State,
    return_ty: Ty,
    loops: Vec<LoopState>,
    tries: Vec<TryState>,
    defers: Vec<defer::Deferred>,
    in_cleanup: bool,
    unsafe_depth: usize,
    discarded_expression: Option<ExprId>,
    callable_bounds: BTreeMap<String, Ty>,
    dependencies: Vec<DefRef>,
}

impl Checker<'_, '_> {
    fn enter_capture(&mut self, coroutine: bool) -> SavedExecution {
        let saved = SavedExecution {
            state: self.state.clone(),
            return_ty: std::mem::replace(&mut self.return_ty, Ty::Unit),
            loops: std::mem::take(&mut self.loops),
            tries: std::mem::take(&mut self.tries),
            defers: std::mem::take(&mut self.defers),
            in_cleanup: std::mem::replace(&mut self.in_cleanup, false),
            unsafe_depth: std::mem::replace(&mut self.unsafe_depth, 0),
            discarded_expression: self.discarded_expression.take(),
            dependencies: std::mem::take(&mut self.dependencies),
            callable_bounds: self.callable_bounds.clone(),
        };
        // 外层槽编号稠密且上界为当前 slots.len()；每个槽至多一份捕获记录。
        self.capture_frames.push(CaptureFrame {
            slots: vec![None; self.slots.len()],
            coroutine,
        });
        self.state.initialized.fill(false);
        self.state.cleanup_paths.clear();
        self.state.reachable = true;
        saved
    }

    fn leave_capture(
        &mut self,
        saved: SavedExecution,
        expression: ExprId,
        function: Option<CallableId>,
        signature: Ty,
    ) {
        let frame = self.capture_frames.pop().expect("捕获 frame 成对进入退出");
        let captures = frame.slots.into_iter().flatten().collect();
        let mut dependencies = std::mem::replace(&mut self.dependencies, saved.dependencies);
        dependencies.sort_by_key(|def| (def.module, def.item.0));
        dependencies.dedup();
        self.capture_plans.push(CapturePlan {
            expression,
            function,
            signature,
            captures,
            coroutine: frame.coroutine,
            dependencies,
        });
        self.state = saved.state;
        self.return_ty = saved.return_ty;
        self.loops = saved.loops;
        self.tries = saved.tries;
        self.defers = saved.defers;
        self.in_cleanup = saved.in_cleanup;
        self.unsafe_depth = saved.unsafe_depth;
        self.discarded_expression = saved.discarded_expression;
        self.callable_bounds = saved.callable_bounds;
    }

    pub(super) fn closure(
        &mut self,
        expression: ExprId,
        function: FnId,
        expected: Option<&Ty>,
    ) -> Ty {
        let saved = self.enter_capture(false);
        let signature = self.function(function, expected);
        let id = CallableId {
            module: self.module,
            function: function.0,
        };
        self.leave_capture(saved, expression, Some(id), signature.clone());
        let arguments = self
            .model
            .parameters_at(self.module, &self.arena().fns[function.0 as usize].span)
            .into_values()
            .collect();
        Ty::Callable(id, arguments, Box::new(signature))
    }

    pub(super) fn launch(&mut self, expression: ExprId, body: ExprId, expected: Option<&Ty>) -> Ty {
        let expected_result = match expected {
            Some(Ty::Join(ty)) => Some(&**ty),
            _ => None,
        };
        if let ExprKind::Call {
            callee,
            type_args,
            args,
        } = self.arena().exprs[body.0 as usize].kind
        {
            // 目标和参数在父协程求值；这里只检查调用签名，不执行 callee。
            let result = self.call(callee, type_args, args, expected_result);
            self.expressions.push((body, result.clone()));
            return Ty::Join(Box::new(result));
        }
        let saved = self.enter_capture(true);
        self.return_ty = expected_result.cloned().unwrap_or_else(|| self.fresh());
        let result = self.return_ty.clone();
        self.expression(body, Some(&result));
        self.run_cleanups(0, true);
        let result = self.resolve(&result);
        self.leave_capture(
            saved,
            expression,
            None,
            Ty::Function(vec![], Box::new(result.clone())),
        );
        let required: Vec<_> = self
            .capture_plans
            .last()
            .expect("async 捕获计划已建立")
            .captures
            .iter()
            .filter(|capture| capture.read_before_write)
            .map(|capture| capture.slot)
            .collect();
        for slot in required {
            let captured = self.capture_slot(slot, true);
            if !captured && self.state.reachable && self.state.initialized.get(slot) != Some(&true)
            {
                self.error(
                    DiagnosticCode::InvalidDeclaration,
                    "启动协程时捕获槽尚未初始化",
                    self.arena().exprs[expression.0 as usize].span.clone(),
                );
            }
        }
        Ty::Join(Box::new(result))
    }

    pub(super) fn capture_slot(&mut self, slot: usize, read: bool) -> bool {
        let Some(frame) = self.capture_frames.last() else {
            return false;
        };
        debug_assert!(slot < self.slots.len(), "捕获编号必须属于现有连续槽");
        if slot >= frame.slots.len() {
            return false;
        }
        let read_before_write =
            read && self.state.reachable && self.state.initialized.get(slot) != Some(&true);
        let innermost = self.capture_frames.len() - 1;
        for (index, frame) in self.capture_frames.iter_mut().enumerate() {
            if let Some(entry) = frame.slots.get_mut(slot) {
                let capture = entry.get_or_insert(CapturedSlot {
                    slot,
                    read_before_write: false,
                    written: false,
                });
                if index == innermost {
                    capture.read_before_write |= read_before_write;
                    capture.written |= !read;
                }
                self.slots[slot].storage |= CAPTURED;
                if frame.coroutine {
                    self.slots[slot].storage |= CROSS_COROUTINE;
                }
            }
        }
        true
    }

    pub(super) fn record_callable_value(&mut self, expression: ExprId) {
        self.expression_callables.remove(&expression.0);
        let values = self.value_callables(expression);
        if !values.is_empty() {
            self.expression_callables.insert(expression.0, values);
        }
    }

    pub(super) fn value_callables(&self, expression: ExprId) -> Vec<CallableId> {
        let mut values = Vec::new();
        self.collect_callables(expression, &mut values);
        values.sort_unstable();
        values.dedup();
        values
    }

    fn collect_callables(&self, expression: ExprId, output: &mut Vec<CallableId>) {
        // 仅记录可能携带 callable 的稀疏表达式，保存求值时的槽身份，避免后续遮蔽改写来源。
        if let Some(values) = self.expression_callables.get(&expression.0) {
            output.extend_from_slice(values);
            return;
        }
        match self.arena().exprs[expression.0 as usize].kind {
            ExprKind::Closure(function) => output.push(CallableId {
                module: self.module,
                function: function.0,
            }),
            ExprKind::Path(path) => {
                let segments = self.arena().paths[path.0 as usize]
                    .segments
                    .as_slice(&self.arena().segments);
                if let Some(segment) = segments.first()
                    && let Some(&slot) = self.state.names.get(&segment.name)
                {
                    if let Some(values) = self.state.callables.get(slot) {
                        output.extend_from_slice(values);
                    }
                } else if let Ok(def) = self
                    .model
                    .resolve(self.module, &self.model.path(self.module, path))
                    && let ItemKind::Function(function) =
                        self.model.modules[def.module].arena.items[def.item.0 as usize].kind
                {
                    output.push(CallableId {
                        module: def.module,
                        function: function.0,
                    });
                }
            }
            ExprKind::Paren(inner)
            | ExprKind::Unary {
                expr: inner,
                op: UnOp::Ref | UnOp::Deref,
            }
            | ExprKind::TypeApp { base: inner, .. }
            | ExprKind::Field { base: inner, .. }
            | ExprKind::TupleField { base: inner, .. }
            | ExprKind::Index { base: inner, .. } => self.collect_callables(inner, output),
            ExprKind::Array(items) | ExprKind::Tuple(items) => {
                for &item in items.as_slice(&self.arena().expr_ids) {
                    self.collect_callables(item, output);
                }
            }
            ExprKind::Repeat { elem, .. } => self.collect_callables(elem, output),
            ExprKind::Block {
                tail: Some(tail), ..
            } => self.collect_callables(tail, output),
            ExprKind::If {
                then_block,
                else_branch,
                ..
            } => {
                self.collect_callables(then_block, output);
                if let Some(branch) = else_branch {
                    self.collect_callables(branch, output);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms.as_slice(&self.arena().match_arms) {
                    self.collect_callables(arm.body, output);
                }
            }
            ExprKind::Struct { fields, .. } => {
                for field in fields.as_slice(&self.arena().field_exprs) {
                    if let Some(value) = field.value {
                        self.collect_callables(value, output);
                    } else if let Some(&slot) = self.state.names.get(&field.name) {
                        output.extend_from_slice(&self.state.callables[slot]);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn require_value_captures(&mut self, expression: ExprId) {
        let span = self.arena().exprs[expression.0 as usize].span.clone();
        for value in self.value_callables(expression) {
            self.require_captures(value, &span);
        }
    }

    pub(super) fn require_captures(&mut self, callable: CallableId, span: &Span) {
        let mut pending = vec![callable];
        let mut visited = Vec::new();
        while let Some(callable) = pending.pop() {
            if visited.contains(&callable) {
                continue;
            }
            visited.push(callable);
            let required: Vec<_> = self
                .capture_plans
                .iter()
                .filter(|plan| plan.function == Some(callable))
                .flat_map(|plan| {
                    plan.captures
                        .iter()
                        .filter(|capture| capture.read_before_write)
                        .map(|capture| capture.slot)
                })
                .collect();
            for slot in required {
                let captured = self.capture_slot(slot, true);
                if !captured
                    && self.state.reachable
                    && self.state.initialized.get(slot) != Some(&true)
                {
                    self.error(
                        DiagnosticCode::InvalidDeclaration,
                        "调用闭包时捕获槽尚未初始化",
                        span.clone(),
                    );
                }
                if let Some(values) = self.state.callables.get(slot) {
                    pending.extend_from_slice(values);
                }
            }
        }
    }

    pub(super) fn address_taken(&mut self, expression: ExprId) {
        if let Some(slot) = self.place_root(expression) {
            self.slots[slot].storage |= ADDRESS_TAKEN;
        }
    }

    pub(super) fn place_written(&mut self, expression: ExprId) {
        if let Some(slot) = self.place_root(expression) {
            self.capture_slot(slot, false);
        }
    }

    fn place_root(&self, expression: ExprId) -> Option<usize> {
        match self.arena().exprs[expression.0 as usize].kind {
            ExprKind::Path(path) => {
                let first = self.arena().paths[path.0 as usize]
                    .segments
                    .as_slice(&self.arena().segments)
                    .first()?;
                self.state.names.get(&first.name).copied()
            }
            ExprKind::Field { base, .. }
            | ExprKind::TupleField { base, .. }
            | ExprKind::Index { base, .. } => self.place_root(base),
            ExprKind::Paren(inner) => self.place_root(inner),
            _ => None,
        }
    }
}
