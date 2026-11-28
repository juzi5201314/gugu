//! 从已检查语义形成独立 HIR；所有名称与类型模型访问终止在此转换边界。
use super::{
    model::{CallableId, Model, Ty},
    output::CheckedSemantics,
};
use crate::{Diagnostic, DiagnosticCode, SourceMap};
use crate::{ast, hir, names::NameResolution};
use std::collections::{BTreeMap, BTreeSet};
mod body;
mod declarations;
pub mod identity;
mod query;
pub use query::LOWER_SCHEMA;
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
    fragment: Option<&'a crate::hir::Validated>,
    type_index: Option<&'a BTreeMap<hir::Type, hir::TypeId>>,
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

#[expect(
    clippy::too_many_arguments,
    reason = "HIR lowering 需要语义结果、依赖指纹与 query 上下文"
)]
pub(super) fn lower(
    model: &Model<'_>,
    names: &NameResolution,
    checked: &CheckedSemantics,
    sources: &SourceMap,
    entry: Option<CallableId>,
    dependency: &crate::query::DependencyFingerprint,
    queries: &crate::query::QueryEngine,
    prefix: Option<&crate::sema_prefix::SemaPrefix>,
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
    prefix: Option<&crate::sema_prefix::SemaPrefix>,
) -> Result<identity::Identities, Diagnostic> {
    identity::collect(model, names, checked, sources, prefix).map(|(_, identities, _)| identities)
}

impl<'m, 'a> Builder<'m, 'a> {
    fn new(
        model: &'m Model<'a>,
        identities: &'m Identities,
        definitions: gugu_base::shared_table::SharedTable<hir::Definition>,
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
            types: Default::default(),
            aggregates: Default::default(),
            interfaces: Default::default(),
            implementations: Default::default(),
            opaques: Default::default(),
            owners: Vec::new(),
            initialization: Default::default(),
            linkage: Default::default(),
            entry,
            input_fingerprint: checked.input_fingerprint,
        };
        let type_intern = BTreeMap::new();
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
        if let Some(&id) = self
            .installed
            .type_index
            .and_then(|index| index.get(&formed))
        {
            return Ok(id);
        }
        let id = hir::TypeId(checked_id(self.output.types.len())?);
        self.type_intern.insert(formed.clone(), id);
        self.output.types.push(formed);
        Ok(id)
    }

    fn definition_mut(&mut self, definition: hir::DefId) -> &mut hir::Definition {
        self.output
            .definitions
            .get_mut(definition.index())
            .expect("只修改会话追加的 HIR 定义")
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
            .suffix_mut()
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

/// 一个类型直接引用的类型、接口与定义；未列出的变体不带引用。
fn type_references(module: &hir::Module, id: hir::TypeId) -> (Vec<hir::TypeId>, Vec<hir::DefId>) {
    use hir::Type;
    let mut types = Vec::new();
    let mut definitions = Vec::new();
    match &module.types[id.index()] {
        Type::Ref(inner)
        | Type::Ptr(inner)
        | Type::Slice(inner)
        | Type::Option(inner)
        | Type::Chan(inner)
        | Type::Join(inner)
        | Type::MaybeUninit(inner) => types.push(*inner),
        Type::Array(inner, length) => {
            types.push(*inner);
            if let hir::ArrayLength::Param((owner, _)) = length {
                definitions.push(*owner);
            }
        }
        Type::Const(hir::ArrayLength::Param((owner, _))) => definitions.push(*owner),
        Type::Tuple(items) => types.extend(items.iter().copied()),
        Type::Function { parameters, result } => {
            types.extend(parameters.iter().copied());
            types.push(*result);
        }
        Type::Callable {
            definition,
            arguments,
            signature,
        } => {
            definitions.push(*definition);
            types.extend(arguments.iter().copied());
            types.push(*signature);
        }
        Type::Named {
            definition,
            arguments,
        }
        | Type::Opaque {
            definition,
            arguments,
        } => {
            definitions.push(*definition);
            types.extend(arguments.iter().copied());
        }
        Type::Projection {
            self_ty, interface, ..
        } => {
            types.push(*self_ty);
            definitions.push(interface.definition);
            types.extend(interface.arguments.iter().copied());
        }
        Type::Dyn(interfaces) => {
            for reference in interfaces {
                definitions.push(reference.definition);
                types.extend(reference.arguments.iter().copied());
            }
        }
        Type::Result(left, right) => {
            types.push(*left);
            types.push(*right);
        }
        _ => {}
    }
    (types, definitions)
}

/// owner 的单元依赖指纹：只覆盖该 owner 降级真正读到的内容。
///
/// 读集由 owner 自身的带类型表（表达式类型、局部、模式、分派、调整、检查、可变参数调用、借用约束）
/// 出发，按 `hir::Type` 展开传递闭包，并把闭包里出现的定义签名一并纳入。`gir::build` 对模块表的读取
/// 只有 `definitions[owner.definition].signature` 与由它展开的两处 `module.types` 查表，读集因此完整。
pub fn owner_dependency_fingerprint(module: &hir::Module, owner: &hir::Owner) -> [u8; 32] {
    let mut content = owner.clone();
    content.input_fingerprint = [0; 32];
    let mut hash = blake3::Hasher::new_derive_key("gugu-owner-dependency-v1");
    hash.update(&crate::canonical::to_vec(&content).expect("Owner 可编码"));
    let mut types = BTreeSet::new();
    let mut definitions = BTreeSet::new();
    types.extend(owner.expression_types.iter().copied());
    types.extend(owner.expression_inputs.iter().copied());
    types.extend(owner.patterns.iter().map(|pattern| pattern.ty));
    types.extend(owner.locals.iter().map(|local| local.ty));
    for dispatch in &owner.dispatches {
        types.insert(dispatch.self_ty);
        types.insert(dispatch.signature);
        types.extend(dispatch.arguments.iter().copied());
        definitions.extend(dispatch.function);
        definitions.extend(dispatch.implementation);
    }
    for adjustment in &owner.adjustments {
        types.extend(match adjustment {
            hir::Adjustment::Dereference => None,
            hir::Adjustment::ArrayToSlice(ty)
            | hir::Adjustment::NeverTo(ty)
            | hir::Adjustment::Erase(ty)
            | hir::Adjustment::Opaque(ty)
            | hir::Adjustment::Instantiate(ty) => Some(*ty),
        });
    }
    for check in &owner.checks {
        types.extend(match &check.kind {
            hir::CheckKind::Division { ty, .. } | hir::CheckKind::Shift { ty, .. } => Some(*ty),
            _ => None,
        });
    }
    types.extend(owner.variadic_calls.iter().map(|call| call.element));
    for constraint in &owner.borrow_constraints {
        types.insert(constraint.base);
        types.insert(constraint.target);
    }
    definitions.extend(owner.captures.iter().map(|capture| capture.owner));
    definitions.insert(owner.definition);
    let mut pending_types = types.iter().copied().collect::<Vec<_>>();
    let mut pending_definitions = definitions.iter().copied().collect::<Vec<_>>();
    while !pending_types.is_empty() || !pending_definitions.is_empty() {
        while let Some(id) = pending_types.pop() {
            let (more_types, more_definitions) = type_references(module, id);
            for ty in more_types {
                if types.insert(ty) {
                    pending_types.push(ty);
                }
            }
            for definition in more_definitions {
                if definitions.insert(definition) {
                    pending_definitions.push(definition);
                }
            }
        }
        while let Some(id) = pending_definitions.pop() {
            let definition = &module.definitions[id.index()];
            hash.update(&definition.key);
            hash.update(format!("{:?}", definition.kind).as_bytes());
            if let Some(signature) = definition.signature
                && types.insert(signature)
            {
                pending_types.push(signature);
            }
        }
    }
    for id in &types {
        hash.update(&crate::canonical::to_vec(&module.types[id.index()]).expect("类型可编码"));
    }
    *hash.finalize().as_bytes()
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
