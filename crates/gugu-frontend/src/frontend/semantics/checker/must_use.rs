//! `unused_must_use`：丢掉 `Option`、`Result`、`#[must_use]` 类型或函数的值。
use super::super::output::{CheckedSemantics, MustUseSite};
use super::*;
use crate::diagnostics::{Diagnostic, Severity};
use crate::frontend::ParsedModule;
use crate::frontend::attr::{self, LintLevel};
use crate::source::{ExpansionId, SourceMap};

impl Checker<'_, '_> {
    pub(super) fn finish_must_use(&mut self) {
        for index in 0..self.slots.len() {
            let initialized = self.state.initialized.get(index) == Some(&true);
            if initialized && !self.slot_reads[index] && self.slot_type_must_use(index) {
                let slot = &self.slots[index];
                self.must_use_sites.push(MustUseSite {
                    start: slot.range[0],
                    end: slot.range[1],
                    expansion: 0,
                });
            }
        }
    }

    pub(super) fn note_discarded_must_use(&mut self, id: ExprId, ty: &Ty) {
        if self.value_must_use(id, ty) {
            let span = self.arena().exprs[id.0 as usize].span.clone();
            self.note_must_use(&span);
        }
    }

    pub(super) fn note_unbound_must_use(&mut self, before: usize, ty: &Ty, span: &Span) {
        if self.slots.len() == before && self.type_must_use(ty) {
            self.note_must_use(span);
        }
    }

    fn slot_type_must_use(&self, index: usize) -> bool {
        self.type_must_use(&self.slots[index].ty)
    }

    fn value_must_use(&self, id: ExprId, ty: &Ty) -> bool {
        self.type_must_use(ty) || self.call_must_use(id)
    }

    fn type_must_use(&self, ty: &Ty) -> bool {
        match self.resolve(ty) {
            Ty::Option(_) | Ty::Result(_, _) => true,
            Ty::Named(index, _) => self.nominal_must_use(index),
            _ => false,
        }
    }

    fn nominal_must_use(&self, index: usize) -> bool {
        let definition = self.model.nominal[index].definition;
        let item = &self.model.modules[definition.module].arena.items[definition.item.0 as usize];
        self.model
            .has_attribute(definition.module, item.attributes, "must_use")
    }

    fn call_must_use(&self, id: ExprId) -> bool {
        if self.item_must_use(id) {
            return true;
        }
        match self.arena().exprs[id.0 as usize].kind {
            ExprKind::Paren(inner) | ExprKind::TypeApp { base: inner, .. } => {
                self.call_must_use(inner)
            }
            ExprKind::Call { callee, .. } => self.call_must_use(callee),
            _ => false,
        }
    }

    fn item_must_use(&self, id: ExprId) -> bool {
        let ExprKind::Path(path) = self.arena().exprs[id.0 as usize].kind else {
            return false;
        };
        let Ok(definition) = self
            .model
            .resolve(self.module, &self.model.path(self.module, path))
        else {
            return false;
        };
        let item = &self.model.modules[definition.module].arena.items[definition.item.0 as usize];
        self.model
            .has_attribute(definition.module, item.attributes, "must_use")
    }

    fn note_must_use(&mut self, span: &Span) {
        self.must_use_sites.push(MustUseSite {
            start: span.start(),
            end: span.end(),
            expansion: span.expansion().as_u32(),
        });
    }
}

pub(crate) fn lints(
    modules: &[ParsedModule],
    semantics: &CheckedSemantics,
    sources: &SourceMap,
) -> Result<Vec<Diagnostic>, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    for body in &semantics.bodies {
        let Some(module) = modules.get(body.definition.module) else {
            continue;
        };
        for site in &body.must_use {
            if let Some(diagnostic) = lint_site(module, sources, site) {
                diagnostics.push(diagnostic);
            }
        }
    }
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity() == Severity::Error)
    {
        Err(diagnostics)
    } else {
        Ok(diagnostics)
    }
}

fn lint_site(module: &ParsedModule, sources: &SourceMap, site: &MustUseSite) -> Option<Diagnostic> {
    let file = module.file.source;
    let snapshot = sources.snapshot(file)?;
    let level = attr::lint_level(
        snapshot.content(),
        module.file.inner_attributes,
        &module.arena,
        &module.tokens,
        &module.configured,
        site.start,
        attr::UNUSED_MUST_USE,
    );
    if matches!(level, LintLevel::Allow) {
        return None;
    }
    let span = sources
        .span(
            file,
            site.start as usize,
            site.end as usize,
            ExpansionId::new(site.expansion),
        )
        .ok();
    Some(Diagnostic::new(
        match level {
            LintLevel::Warn => Severity::Warning,
            LintLevel::Deny | LintLevel::Forbid => Severity::Error,
            LintLevel::Allow => return None,
        },
        DiagnosticCode::UnusedMustUse,
        "丢掉了必须使用的值；用 `_ =` 显式丢弃，或读取该绑定",
        span,
        u32::MAX,
    ))
}
