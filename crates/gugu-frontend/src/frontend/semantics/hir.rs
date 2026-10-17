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
pub(crate) mod identity;
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

/// 物化内建块的安装信息。
///
/// `fragment` 是已通过 verifier 的 HIR 片段（物化路径才有）；`builtin` 是内建块的定义数，
/// 也就是编号前缀的边界——两条加载路径都按它把降级切成「内建块先完成、用户定义整体追加」。
#[derive(Clone, Copy)]
struct InstalledBlock<'a> {
    fragment: Option<&'a crate::frontend::hir::Validated>,
    builtin: usize,
}

struct Builder<'m, 'a> {
    model: &'m Model<'a>,
    checked: &'m CheckedSemantics,
    sources: &'m SourceMap,
    identities: &'m Identities,
    /// 物化内建块的安装信息：片段（可选）与内建块定义数（编号前缀边界）。
    installed: InstalledBlock<'m>,
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
    queries: &crate::query::QueryEngine,
    prefix: Option<&crate::frontend::prefix::BuiltinPrefix>,
) -> Result<(hir::Validated, crate::query::DependencyFingerprint), Vec<Diagnostic>> {
    query::lower(
        model, names, checked, sources, entry, dependency, queries, prefix,
    )
}

pub(super) fn identities(
    model: &Model<'_>,
    names: &NameResolution,
    checked: &CheckedSemantics,
    sources: &SourceMap,
    prefix: Option<&crate::frontend::prefix::BuiltinPrefix>,
) -> Result<identity::Identities, Diagnostic> {
    identity::collect(model, names, checked, sources, prefix).map(|(_, identities, _)| identities)
}

impl<'m, 'a> Builder<'m, 'a> {
    fn new(
        model: &'m Model<'a>,
        identities: &'m Identities,
        definitions: Vec<hir::Definition>,
        checked: &'m CheckedSemantics,
        sources: &'m SourceMap,
        entry: Option<CallableId>,
        installed: InstalledBlock<'m>,
    ) -> Result<Self, Diagnostic> {
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
        let mut output = hir::Module {
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
        let mut type_intern = BTreeMap::new();
        if let Some(installed) = installed.fragment {
            // 物化片段整段接管：类型表按原下标装回并预置驻留表，用户侧复用同一批 `TypeId`；
            // 各侧表直接沿用，只对用户定义做 lowering。
            let fragment = installed.module();
            output.types = fragment.types.clone();
            output.aggregates = fragment.aggregates.clone();
            output.interfaces = fragment.interfaces.clone();
            output.implementations = fragment.implementations.clone();
            output.opaques = fragment.opaques.clone();
            output.owners = fragment.owners.clone();
            // owner 输入身份必须在本次世界重算：片段里那份是构建期世界的编号。
            for owner in &mut output.owners {
                owner.input_fingerprint = owner_input_fingerprint(
                    &output.definitions[owner.definition.index()].key,
                    &checked.input_fingerprint,
                );
            }
            output.initialization = fragment.initialization.clone();
            output.linkage = fragment.linkage.clone();
            for (index, formed) in fragment.types.iter().enumerate() {
                type_intern.insert(formed.clone(), hir::TypeId(checked_id(index)?));
            }
        }
        Ok(Self {
            model,
            checked,
            sources,
            identities,
            installed,
            output,
            parameters,
            type_intern,
            type_cache,
        })
    }

    /// 物化片段覆盖的 `HIR DefId` 数量：用户编号从它开始。
    fn installed_definitions(&self) -> usize {
        self.installed
            .fragment
            .map_or(0, |installed| installed.module().definitions.len())
    }

    /// 内建块的定义数：编号前缀的边界。
    fn builtin_definitions(&self) -> usize {
        self.installed.builtin
    }

    fn intern_type(&mut self, formed: hir::Type) -> Result<hir::TypeId, Diagnostic> {
        if let Some(&id) = self.type_intern.get(&formed) {
            return Ok(id);
        }
        let id = hir::TypeId(checked_id(self.output.types.len())?);
        self.type_intern.insert(formed.clone(), id);
        self.output.types.push(formed);
        Ok(id)
    }

    fn intern_primitives(&mut self) -> Result<(), Diagnostic> {
        let unit = self.intern_type(hir::Type::Unit)?;
        self.intern_type(hir::Type::Never)?;
        self.intern_type(hir::Type::Bool)?;
        self.intern_type(hir::Type::String)?;
        self.intern_type(hir::Type::Ptr(unit))?;
        Ok(())
    }

    fn build(mut self) -> Result<hir::Module, Diagnostic> {
        self.collect_parameters()?;
        if !self.output.definitions.is_empty() {
            self.intern_primitives()?;
        }
        // 两段式降级：内建块先完成签名、侧表与 body，用户定义整体追加在后。类型驻留顺序因此
        // 只由「内建块 + 用户追加」决定——物化路径直接沿用前缀类型，现算路径按同一顺序重算，
        // 两条路径给出同一张类型表（I1 的编号前缀不变式）。
        let builtin = self.builtin_definitions();
        let total = self.identities.origins.len();
        debug_assert!(
            builtin <= total,
            "内建块定义数不得超过定义表长度：{builtin} > {total}"
        );
        // 物化路径里内建块整段来自前缀，这一段没有可降级的东西；现算路径必须按同一顺序补算。
        if self.installed_definitions() < builtin {
            self.lower_declarations(0..builtin)?;
            self.lower_owners(0..builtin)?;
        }
        self.lower_declarations(builtin..total)?;
        self.lower_owners(builtin..total)?;
        self.output.owners.sort_by_key(|owner| owner.definition);
        let installed = self.installed_definitions();
        self.output.initialization.extend(
            self.checked
                .initialization
                .iter()
                .filter(|initialization| {
                    self.identities.item(initialization.definition).index() >= installed
                })
                .map(|initialization| hir::Initialization {
                    definition: self.identities.item(initialization.definition),
                    domain: match initialization.kind {
                        super::initialization::InitKind::Constant => hir::StorageDomain::Constant,
                        super::initialization::InitKind::Process => hir::StorageDomain::Process,
                        super::initialization::InitKind::Coroutine => hir::StorageDomain::Coroutine,
                        super::initialization::InitKind::OsThread => hir::StorageDomain::OsThread,
                    },
                }),
        );
        self.lower_linkage();
        Ok(self.output)
    }

    fn lower_linkage(&mut self) {
        let installed = self.installed_definitions();
        for linkage in &self.checked.linkage {
            if self.identities.item(linkage.definition).index() < installed {
                continue;
            }
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
                root_kind: linkage.root_kind,
                foreign: function.and_then(|function| function.effect),
                naked: function.is_some_and(|function| function.naked()),
            });
        }
        for function in &self.checked.foreign_definitions {
            let definition = self.identities.function(function.callable);
            if definition.index() < installed
                || self
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
                root_kind: super::linkage::RootLinkageKind::ExecutableUsed,
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

/// owner 的 HIR 输入身份：定义稳定键 + 本次世界的类型检查身份。
///
/// 物化片段的 owner 体按内容照搬，但这份身份必须在**本次会话**重算——它是 owner 级缓存的输入
/// 身份，带着构建期世界的编号会让两条加载路径给出不同的结果。
pub(super) fn owner_input_fingerprint(key: &[u8; 32], checked: &[u8; 32]) -> [u8; 32] {
    *blake3::Hasher::new_derive_key("gugu-hir-owner-input-v1")
        .update(key)
        .update(checked)
        .finalize()
        .as_bytes()
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
