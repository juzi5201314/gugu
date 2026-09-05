use super::super::super::ast::{ExprId, FnId};
use super::super::model::{Model, Ty, substitute};
use super::super::opaque::{Bounds, has_unbound};
use super::Checker;
use crate::{DiagnosticCode, Span};
use std::collections::BTreeMap;

impl Checker<'_, '_> {
    pub(super) fn opaque_relation(
        &mut self,
        actual: &Ty,
        expected: &Ty,
        span: &Span,
        coercion: bool,
    ) -> Option<Ty> {
        if let Ty::Opaque(id, arguments) = expected {
            if self.model.can_bind_opaque(*id, self.module, span) {
                match self.model.opaque_bounds(*id, arguments, actual) {
                    Ok(bounds) => self.enforce_bounds(actual, bounds, span),
                    Err(error) => self.errors.push(error),
                }
                self.hidden_candidates
                    .push((*id, arguments.clone(), actual.clone()));
                return Some(expected.clone());
            }
        }
        if coercion && matches!(expected, Ty::Function(..)) && matches!(actual, Ty::Opaque(..)) {
            match self.model.opaque_function(actual) {
                Ok(Some(signature)) => return Some(self.relate(&signature, expected, span, true)),
                Ok(None) => {}
                Err(error) => {
                    self.errors.push(error);
                    return Some(Ty::Error);
                }
            }
        }
        if let Ty::Dyn(interfaces) = expected {
            if coercion {
                let assumptions = match self.model.assumptions_at(self.module, span) {
                    Ok(assumptions) => assumptions,
                    Err(error) => {
                        self.errors.push(error);
                        return Some(Ty::Error);
                    }
                };
                for interface in interfaces {
                    self.trait_constraints.push((
                        super::super::traits::Obligation {
                            ty: actual.clone(),
                            interface: interface.clone(),
                            span: span.clone(),
                        },
                        assumptions.clone(),
                    ));
                }
                return Some(expected.clone());
            }
        }
        None
    }
    pub(super) fn enforce_bounds(&mut self, actual: &Ty, mut bounds: Bounds, span: &Span) {
        let assumptions = match self.model.assumptions_at(self.module, span) {
            Ok(assumptions) => assumptions,
            Err(error) => {
                self.errors.push(error);
                return;
            }
        };
        self.model.expand_requirements(&mut bounds.traits);
        for mut obligation in bounds.traits {
            obligation.span = span.clone();
            self.trait_constraints
                .push((obligation, assumptions.clone()));
        }
        let callable = match actual {
            Ty::Param(name) => self
                .callable_bounds
                .get(name)
                .cloned()
                .unwrap_or_else(|| actual.clone()),
            Ty::Opaque(..) => match self.model.opaque_function(actual) {
                Ok(Some(signature)) => signature,
                Ok(None) => actual.clone(),
                Err(error) => {
                    self.errors.push(error);
                    Ty::Error
                }
            },
            _ => actual.clone(),
        };
        for signature in bounds.functions {
            self.callable_constraints
                .push((callable.clone(), signature, span.clone()));
        }
    }
    pub(super) fn instantiate_apits(
        &mut self,
        owner: super::super::model::CallableId,
        bindings: &mut BTreeMap<String, Ty>,
        span: &Span,
    ) {
        for id in self.model.apits(owner) {
            let value = self.fresh();
            bindings.insert(Model::apit_name(id), value);
        }
        for id in self.model.apits(owner) {
            let value = &bindings[&Model::apit_name(id)];
            let definition = &self.model.opaques.definitions[id as usize];
            match self
                .model
                .form_bounds(owner.module, definition.bounds, bindings, value)
            {
                Ok(bounds) => self.enforce_bounds(value, bounds, span),
                Err(error) => self.errors.push(error),
            }
        }
    }
    pub(super) fn register_apits(&mut self, function: FnId) {
        let owner = super::super::model::CallableId {
            module: self.module,
            function: function.0,
        };
        let span = &self.arena().fns[function.0 as usize].span;
        let context = self.model.parameters_at(self.module, span);
        for id in self.model.apits(owner) {
            let name = Model::apit_name(id);
            let definition = &self.model.opaques.definitions[id as usize];
            match self.model.form_bounds(
                self.module,
                definition.bounds,
                &context,
                &Ty::Param(name.clone()),
            ) {
                Ok(bounds) => {
                    for signature in bounds.functions {
                        if self
                            .callable_bounds
                            .get(&name)
                            .is_some_and(|previous| *previous != signature)
                        {
                            self.error(
                                DiagnosticCode::InvalidType,
                                "APIT 的 Fn 约束不一致",
                                span.clone(),
                            );
                        }
                        self.callable_bounds.insert(name.clone(), signature);
                    }
                }
                Err(error) => self.errors.push(error),
            }
        }
    }
    pub(super) fn record_erasure(&mut self, expression: ExprId, actual: &Ty, expected: &Ty) {
        let source = self.resolve(actual);
        let target = self.resolve(expected);
        if source != target
            && source != Ty::Never
            && matches!(target, Ty::Dyn(_) | Ty::Function(..))
        {
            self.erasures.push(super::super::output::Erasure {
                expression,
                source,
                target,
            });
        }
    }
    pub(super) fn finish_hidden(&mut self, hidden: &mut [Option<Ty>]) {
        for (id, arguments, ty) in std::mem::take(&mut self.hidden_candidates) {
            let context = self.model.opaque_context(id);
            let mut renames = BTreeMap::new();
            for ((_, formal), argument) in context.iter().zip(&arguments) {
                if let Ty::Param(name) = self.resolve(argument) {
                    renames.insert(name, formal.clone());
                }
            }
            let ty = substitute(&self.normalized(&ty), &renames);
            if has_unbound(&ty, &context) {
                self.error(
                    DiagnosticCode::InvalidType,
                    "隐藏类型捕获了别名声明之外的泛型参数",
                    self.model.opaque_span(id).clone(),
                );
                continue;
            }
            if let Some(previous) = &hidden[id as usize] {
                if *previous != ty {
                    self.error(
                        DiagnosticCode::InvalidType,
                        "同一不透明声明必须具有唯一隐藏类型",
                        self.model.opaque_span(id).clone(),
                    );
                }
            } else {
                hidden[id as usize] = Some(ty);
            }
        }
        let mut erasures = std::mem::take(&mut self.erasures);
        for erasure in &mut erasures {
            erasure.source = self.normalized(&erasure.source);
            erasure.target = self.normalized(&erasure.target);
        }
        self.erasures = erasures;
        let mut reflections = std::mem::take(&mut self.reflections);
        for reflection in &mut reflections {
            use super::super::output::ReflectionKind;
            if let ReflectionKind::Is(ty)
            | ReflectionKind::Downcast(ty)
            | ReflectionKind::DowncastCopy(ty)
            | ReflectionKind::TypeId(ty) = &mut reflection.kind
            {
                *ty = self.normalized(ty);
            }
        }
        self.reflections = reflections;
    }
}
