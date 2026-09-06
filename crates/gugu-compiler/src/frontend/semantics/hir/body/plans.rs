use super::*;
use crate::frontend::semantics::output;

impl BodyBuilder<'_, '_, '_, '_> {
    fn dispatch_indices(&self, source: ast::ExprId) -> &[usize] {
        let order = &self.facts.dispatch_order;
        let start = order
            .partition_point(|&index| self.facts.body.dispatches[index].expression.0 < source.0);
        let end = order
            .partition_point(|&index| self.facts.body.dispatches[index].expression.0 <= source.0);
        &order[start..end]
    }
    pub(super) fn selected_dispatch(
        &mut self,
        source: ast::ExprId,
        interface: Option<&str>,
        member: Option<&str>,
    ) -> Result<Option<u32>, Diagnostic> {
        let index = self
            .dispatch_indices(source)
            .iter()
            .copied()
            .find(|&index| {
                let dispatch = &self.facts.body.dispatches[index];
                let shape = dispatch
                    .interface
                    .as_ref()
                    .map(|interface| &self.compiler.model.traits.interfaces[interface.id]);
                interface.is_none_or(|name| shape.is_some_and(|shape| shape.name == name))
                    && member.is_none_or(|name| {
                        shape.zip(dispatch.member).is_some_and(|(shape, member)| {
                            shape
                                .members
                                .keys()
                                .nth(member as usize)
                                .is_some_and(|member| member == name)
                        })
                    })
            });
        index.map(|index| self.lower_dispatch(index)).transpose()
    }
    pub(super) fn assignment_dispatch(
        &mut self,
        source: ast::ExprId,
    ) -> Result<Option<u32>, Diagnostic> {
        let index = self
            .dispatch_indices(source)
            .iter()
            .copied()
            .find(|&index| {
                self.facts.body.dispatches[index]
                    .interface
                    .as_ref()
                    .is_some_and(|interface| {
                        let interface = &self.compiler.model.traits.interfaces[interface.id];
                        interface.definition.is_none() && interface.name.ends_with("Assign")
                    })
            });
        index.map(|index| self.lower_dispatch(index)).transpose()
    }
    fn lower_dispatch(&mut self, index: usize) -> Result<u32, Diagnostic> {
        if let Some(id) = self.dispatch_map[index] {
            return Ok(id);
        }
        let selected = &self.facts.body.dispatches[index];
        let id = checked_id(self.output.dispatches.len())?;
        let dispatch = hir::Dispatch {
            function: selected
                .callable
                .map(|function| self.compiler.identities.function(function)),
            implementation: selected
                .implementation
                .map(|definition| self.compiler.identities.item(definition)),
            interface: selected
                .interface
                .as_ref()
                .map(|interface| self.compiler.trait_ref(interface, self.output.definition))
                .transpose()?,
            member: selected.member,
            dereferences: selected.dereferences,
            borrow: selected.borrow,
            implicit_receiver: selected.implicit_receiver,
            self_ty: self.type_id(&selected.self_ty)?,
            signature: self.type_id(&selected.signature)?,
            dynamic: selected.dynamic,
        };
        self.output.dispatches.push(dispatch);
        self.dispatch_map[index] = Some(id);
        Ok(id)
    }
    pub(super) fn mapped(&self, source: ast::ExprId) -> Result<hir::ExprId, Diagnostic> {
        self.expression_map[source.0 as usize]
            .ok_or_else(|| self.error("已检查计划的操作数没有 owner-local HIR 身份"))
    }

    pub(super) fn lower_plans(&mut self) -> Result<(), Diagnostic> {
        for check in &self.facts.body.runtime_checks {
            let Some(expression) = self.expression_map[check.expression.0 as usize] else {
                continue;
            };
            let kind = match &check.kind {
                output::CheckKind::IntegerDivision { ty, divisor } => hir::CheckKind::Division {
                    ty: self.type_id(ty)?,
                    divisor: self.mapped(*divisor)?,
                },
                output::CheckKind::Shift { ty, amount } => hir::CheckKind::Shift {
                    ty: self.type_id(ty)?,
                    amount: self.mapped(*amount)?,
                },
                output::CheckKind::Bounds { slice } => hir::CheckKind::Bounds { slice: *slice },
                output::CheckKind::Utf8Boundary => hir::CheckKind::Utf8Boundary,
                output::CheckKind::FloatToInt {
                    signed,
                    bits,
                    value,
                } => hir::CheckKind::FloatToInt {
                    signed: *signed,
                    bits: *bits,
                    value: self.mapped(*value)?,
                },
                output::CheckKind::UnicodeScalar { value } => hir::CheckKind::UnicodeScalar {
                    value: self.mapped(*value)?,
                },
            };
            self.output.checks.push(hir::RuntimeCheck {
                expression,
                kind,
                proof: None,
            });
            self.expressions[expression.index()]
                .as_mut()
                .expect("计划在表达式形成后转换")
                .effects
                .0 |= hir::Effects::PANIC;
        }
        self.output.checks.sort_unstable();
        self.output.checks.dedup();
        for adjustment in &self.facts.body.adjustments {
            let Some(expression) = self.expression_map[adjustment.expression.0 as usize] else {
                continue;
            };
            let target = self.type_id(&adjustment.target)?;
            let operation = match adjustment.kind {
                output::AdjustmentKind::Erase => hir::Adjustment::Erase(target),
                output::AdjustmentKind::Opaque => hir::Adjustment::Opaque(target),
                output::AdjustmentKind::ArrayToSlice => hir::Adjustment::ArrayToSlice(target),
            };
            if !self.adjustments[expression.index()].contains(&operation) {
                self.adjustments[expression.index()].push(operation);
            }
            self.output.expression_types[expression.index()] = target;
            if adjustment.kind != output::AdjustmentKind::Opaque {
                self.expressions[expression.index()]
                    .as_mut()
                    .expect("调整表达式已形成")
                    .effects
                    .0 |= hir::Effects::ALLOCATE | hir::Effects::SAFEPOINT;
            }
        }
        for borrow in &self.facts.body.borrow_checks {
            let Some(expression) = self.expression_map[borrow.expression.0 as usize] else {
                continue;
            };
            let constraint = hir::BorrowConstraint {
                expression,
                base: self.type_id(&borrow.base)?,
                projection: borrow.projection.clone(),
                target: self.type_id(&borrow.target)?,
            };
            if !self.output.borrow_constraints.contains(&constraint) {
                self.output.borrow_constraints.push(constraint);
            }
        }
        Ok(())
    }

    pub(super) fn call_plans(
        &mut self,
        source: ast::ExprId,
        callee: ast::ExprId,
        call: hir::ExprId,
    ) -> Result<u32, Diagnostic> {
        for plan in &self.facts.body.variadic_calls {
            if plan.callee != callee {
                continue;
            }
            let variadic = hir::VariadicCall {
                expression: call,
                fixed_count: checked_id(plan.fixed_count)?,
                element: self.type_id(&plan.element)?,
                heterogeneous: plan.heterogeneous,
            };
            if !self.output.variadic_calls.contains(&variadic) {
                self.output.variadic_calls.push(variadic);
            }
        }
        if let Some(plan) = self
            .facts
            .body
            .foreign_calls
            .iter()
            .find(|plan| plan.expression == source)
        {
            self.output.foreign_calls.push(hir::ForeignCall {
                expression: call,
                effect: plan.effect,
            });
            return Ok(hir::Effects::FOREIGN
                | match plan.effect {
                    crate::frontend::semantics::foreign::ForeignEffect::Leaf { .. } => 0,
                    crate::frontend::semantics::foreign::ForeignEffect::Bridge
                    | crate::frontend::semantics::foreign::ForeignEffect::DirtyCpu => {
                        hir::Effects::SAFEPOINT | hir::Effects::SUSPEND
                    }
                });
        }
        Ok(hir::Effects::PANIC
            | hir::Effects::ALLOCATE
            | hir::Effects::SAFEPOINT
            | hir::Effects::SUSPEND)
    }

    pub(super) fn nested(
        &mut self,
        source: ast::ExprId,
        function: Option<ast::FnId>,
        body: ast::ExprId,
    ) -> Result<hir::DefId, Diagnostic> {
        let definition = if let Some(function) = function {
            self.compiler.identities.function(CallableId {
                module: self.module,
                function: function.0,
            })
        } else {
            self.compiler.identities.asynchronous[self.module][source.0 as usize]
                .ok_or_else(|| self.error("async body 没有稳定定义"))?
        };
        let mut inherited = Vec::new();
        for plan in &self.facts.body.captures {
            if plan.expression != source {
                continue;
            }
            for capture in &plan.captures {
                let local = self.slots[capture.slot]
                    .ok_or_else(|| self.error("闭包捕获槽不在外围 owner 可见"))?;
                inherited.push(Inherited {
                    slot: capture.slot,
                    owner: self.output.definition,
                    local,
                    read: capture.read_before_write,
                    written: capture.written,
                    coroutine: plan.coroutine,
                });
            }
        }
        self.compiler.lower_owner(
            self.facts,
            definition,
            self.module,
            function,
            body,
            inherited,
        )?;
        Ok(definition)
    }
}
