use super::super::foreign::{ForeignCall, ForeignDefinition, ForeignEffect};
use super::*;

impl Checker<'_, '_> {
    pub(super) fn check_foreign_call(&mut self, callee: ExprId, ty: &Ty) {
        let Ty::Callable(callable, _, _) = ty else {
            return;
        };
        let declaration = self.model.foreign_definition_at(*callable);
        let Some((declaration, mut effect)) = declaration
            .and_then(|declaration| declaration.effect.map(|effect| (declaration, effect)))
        else {
            if self.native_definition().is_some() {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "opaque native definition 不能调用 Gugu 函数",
                    self.arena().exprs[callee.0 as usize].span.clone(),
                );
            }
            return;
        };
        let expression = self.call_site.unwrap_or(callee);
        let expr = &self.arena().exprs[expression.0 as usize];
        match self
            .model
            .foreign_effect_attribute(self.module, expr.attributes)
        {
            Ok(Some(requested)) => {
                let valid = match requested {
                    ForeignEffect::Leaf { .. } => false,
                    ForeignEffect::Bridge => declaration.imported(),
                    ForeignEffect::DirtyCpu => {
                        declaration.imported()
                            || declaration.naked()
                            || declaration.effect == Some(ForeignEffect::DirtyCpu)
                    }
                };
                if valid {
                    effect = requested;
                } else {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "该 ffi 调用点覆盖不适用于目标声明；不能强制 leaf 或把受管定义当成 native",
                        expr.span.clone(),
                    );
                }
            }
            Ok(None) => {}
            Err(error) => self.errors.push(error),
        }
        if self.native_definition().is_some() && effect == ForeignEffect::Bridge {
            self.error(DiagnosticCode::InvalidExpression, "opaque native definition 不能建立受管回调桥；C 调用必须具有明确的 leaf 或 dirty 契约", expr.span.clone());
        }
        self.foreign_calls.push(ForeignCall {
            expression,
            callable: *callable,
            effect,
        });
    }

    pub(super) fn native_definition(&self) -> Option<&ForeignDefinition> {
        let function = self.current_function?;
        let definition = self
            .model
            .foreign_definition_at(super::super::model::CallableId {
                module: self.module,
                function: function.0,
            })?;
        (definition.naked() || definition.effect == Some(ForeignEffect::DirtyCpu))
            .then_some(definition)
    }
    pub(super) fn finish_native_checks(&mut self, function: FnId) {
        let callable = super::super::model::CallableId {
            module: self.module,
            function: function.0,
        };
        if self
            .model
            .foreign_definition_at(callable)
            .is_none_or(|definition| {
                !definition.naked() && definition.effect != Some(ForeignEffect::DirtyCpu)
            })
        {
            return;
        }
        let checks = std::mem::take(&mut self.runtime_checks);
        for check in checks {
            if !self
                .model
                .check_proven(self.module, &check, &self.expressions)
            {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "opaque native definition 不能包含无法消除的 panic 检查",
                    self.arena().exprs[check.expression.0 as usize].span.clone(),
                );
                self.runtime_checks.push(check);
            }
        }
    }
}
