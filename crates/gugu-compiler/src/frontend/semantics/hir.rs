//! 从已检查语义形成独立 HIR；所有名称与类型模型访问终止在此转换边界。
use super::{
    model::{CallableId, Model, Ty},
    output::CheckedSemantics,
};
use crate::frontend::{ast, hir, names::NameResolution};
use crate::{Diagnostic, DiagnosticCode, SourceMap};
use std::collections::BTreeMap;
mod body;
mod declarations;
mod identity;
mod query;
#[cfg(test)]
mod tests;
mod types;

use identity::{Identities, Origin};

#[derive(Clone)]
struct PendingParameter {
    name: String,
    kind: PendingParameterKind,
}
#[derive(Clone, Copy)]
enum PendingParameterKind {
    Type { pack: bool },
    Comptime { module: usize, ty: ast::TyId },
}

struct Builder<'m, 'a> {
    model: &'m Model<'a>,
    checked: &'m CheckedSemantics,
    sources: &'m SourceMap,
    identities: Identities,
    output: hir::Module,
    parameters: Vec<Vec<PendingParameter>>,
    type_intern: BTreeMap<hir::Type, hir::TypeId>,
    type_cache: Vec<BTreeMap<Ty, hir::TypeId>>,
}

pub(super) fn lower(
    model: &Model<'_>,
    names: &NameResolution,
    checked: &CheckedSemantics,
    sources: &SourceMap,
    entry: Option<CallableId>,
    dependency: &crate::query::DependencyFingerprint,
    queries: &crate::QueryEngine,
) -> Result<hir::Validated, Vec<Diagnostic>> {
    query::lower(model, names, checked, sources, entry, dependency, queries)
}

impl<'m, 'a> Builder<'m, 'a> {
    fn new(
        model: &'m Model<'a>,
        names: &'m NameResolution,
        checked: &'m CheckedSemantics,
        sources: &'m SourceMap,
        entry: Option<CallableId>,
    ) -> Result<Self, Diagnostic> {
        let (definitions, identities) = identity::collect(model, names, checked, sources)?;
        let entry = entry.map(|entry| identities.function(entry));
        let expansions = sources
            .expansions()
            .iter()
            .map(|expansion| {
                Ok(hir::Expansion {
                    parent: expansion.parent().as_u32(),
                    call: identity::location(sources, expansion.macro_call())?,
                    definition: identity::location(sources, expansion.macro_definition())?,
                    source: expansion.generated_source().as_u32(),
                    slot: expansion.fragment_kind(),
                    hash: expansion.source_hash(),
                })
            })
            .collect::<Result<Vec<_>, Diagnostic>>()?;
        let parameters = vec![Vec::new(); definitions.len()];
        let type_cache = vec![BTreeMap::new(); definitions.len()];
        let output = hir::Module {
            sources: sources
                .snapshots()
                .iter()
                .map(|source| hir::Source {
                    path: source.logical_path().to_owned(),
                    hash: source.content_hash(),
                    length: source.content().len() as u32,
                })
                .collect(),
            expansions,
            definitions,
            types: Vec::new(),
            aggregates: Vec::new(),
            interfaces: Vec::new(),
            implementations: Vec::new(),
            opaques: Vec::new(),
            owners: Vec::new(),
            initialization: Vec::new(),
            linkage: Vec::new(),
            entry,
            input_fingerprint: checked.input_fingerprint,
        };
        Ok(Self {
            model,
            checked,
            sources,
            identities,
            output,
            parameters,
            type_intern: BTreeMap::new(),
            type_cache,
        })
    }

    fn build(mut self) -> Result<hir::Module, Diagnostic> {
        self.collect_parameters()?;
        self.lower_declarations()?;
        self.lower_owners()?;
        self.output.owners.sort_by_key(|owner| owner.definition);
        self.output.initialization = self
            .checked
            .initialization
            .iter()
            .map(|initialization| hir::Initialization {
                definition: self.identities.item(initialization.definition),
                domain: match initialization.kind {
                    super::initialization::InitKind::Constant => hir::StorageDomain::Constant,
                    super::initialization::InitKind::Process => hir::StorageDomain::Process,
                    super::initialization::InitKind::Coroutine => hir::StorageDomain::Coroutine,
                    super::initialization::InitKind::OsThread => hir::StorageDomain::OsThread,
                },
            })
            .collect();
        self.lower_linkage();
        Ok(self.output)
    }

    fn lower_linkage(&mut self) {
        for linkage in &self.checked.linkage {
            let function = match self.model.modules[linkage.definition.module].arena.items
                [linkage.definition.item.0 as usize]
                .kind
            {
                ast::ItemKind::Function(function) => self.model.foreign_definition_at(CallableId {
                    module: linkage.definition.module,
                    function: function.0,
                }),
                _ => None,
            };
            self.output.linkage.push(hir::Linkage {
                definition: self.identities.item(linkage.definition),
                export_name: linkage.export_name.clone(),
                import_name: linkage.import_name.clone(),
                section: linkage.section.clone(),
                used: linkage.used,
                foreign: function.and_then(|function| function.effect),
                naked: function.is_some_and(|function| function.naked()),
            });
        }
        for function in &self.checked.foreign_definitions {
            let definition = self.identities.function(function.callable);
            if self
                .output
                .linkage
                .iter()
                .any(|linkage| linkage.definition == definition)
            {
                continue;
            }
            self.output.linkage.push(hir::Linkage {
                definition,
                export_name: None,
                import_name: None,
                section: None,
                used: false,
                foreign: function.effect,
                naked: function.naked(),
            });
        }
        self.output
            .linkage
            .sort_by_key(|linkage| linkage.definition);
    }

    fn error(&self, owner: hir::DefId, message: &str) -> Diagnostic {
        let span = self.output.definitions[owner.index()]
            .location
            .as_ref()
            .and_then(|location| {
                self.sources
                    .span(
                        crate::SourceFileId::new(location.source),
                        location.start as usize,
                        location.end as usize,
                        crate::ExpansionId::new(location.expansion),
                    )
                    .ok()
            });
        Diagnostic::error(DiagnosticCode::InvalidType, message, span)
    }
}

fn checked_id(index: usize) -> Result<u32, Diagnostic> {
    if index >= u32::MAX as usize {
        return Err(Diagnostic::error(
            DiagnosticCode::ParseImplementationLimit,
            "HIR arena 达到 u32 编号上界",
            None,
        ));
    }
    debug_assert!(index < u32::MAX as usize);
    Ok(index as u32)
}
