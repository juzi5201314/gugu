use std::collections::{BTreeMap, BTreeSet};

mod model;
mod prelude;

use model::is_reserved_name;
pub use model::{
    DefId, Definition, DefinitionKind, ModuleId, NameResolution, Namespace, ResolvedImport,
    ResolvedTarget, resolution_is_valid, resolved_target_key,
};

use crate::{
    diagnostics::{Diagnostic, DiagnosticCode},
    source::Span,
};

use super::{
    ParsedModule,
    ast::{ItemId, ItemKind, StructBody, UseTreeKind, VariantKind, Visibility},
};

#[derive(Clone)]
struct Candidate {
    module: ModuleId,
    parent: Option<usize>,
    name: Option<String>,
    kind: DefinitionKind,
    namespace: Option<Namespace>,
    visibility: Visibility,
    span: Span,
    def_path: Vec<u8>,
    stable_key: [u8; 32],
    module_binding: bool,
}

#[derive(Default)]
struct CandidateSet {
    values: Vec<Candidate>,
    disambiguators: BTreeMap<(ModuleId, Option<usize>, DefinitionKind, String), u32>,
}

#[derive(Clone)]
enum ModuleTarget {
    Local(ModuleId),
    External(String),
}

#[derive(Clone)]
struct PendingImport {
    module: ModuleId,
    target: ModuleTarget,
    item: Option<String>,
    alias: String,
    public: bool,
    span: Span,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum BindingTarget {
    Module(ModuleId),
    Def(DefId),
    External(String),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Binding {
    namespace: Namespace,
    target: BindingTarget,
}

type ModuleBindings = BTreeMap<(ModuleId, String), Vec<(Namespace, DefId, Visibility)>>;
/// 名称解析：`prefix` 为 `Some` 时按内建块物化表安装内建候选。
///
/// 物化覆盖的内建模块不参与候选收集，其定义与导入直接取自前缀；编号从物化前缀之后继续，
/// 未覆盖的内建模块（探针源）与用户模块按模块表顺序追加。跨块引用只走稳定键。
pub fn analyze_with_prefix(
    package_identity: &str,
    external_packages: &BTreeSet<String>,
    modules: &[ParsedModule],
    prefix: Option<&crate::sema_prefix::SemaPrefix>,
) -> Result<NameResolution, Vec<Diagnostic>> {
    let mut diagnostics = validate_modules(external_packages, modules);
    let candidates = collect_candidates(package_identity, modules, &mut diagnostics, prefix);
    let offset = prefix.map_or(0, |prefix| prefix.definition_count());
    let definitions = {
        let mut definitions = prefix
            .map(|prefix| prefix.names().definitions.clone())
            .unwrap_or_default();
        definitions.extend(assign_def_ids(&candidates.values, &mut diagnostics, offset));
        definitions
    };
    diagnose_definition_conflicts(&candidates.values, &mut diagnostics);
    let module_bindings = module_bindings(definitions.iter());
    let mut pending = collect_imports(external_packages, modules, &mut diagnostics, prefix);
    let implicit = prelude::implicit_imports(modules, &pending, &module_bindings);
    pending.extend(implicit);
    if let Some(import) = import_cycle(&pending, modules.len()) {
        diagnostics.push(Diagnostic::error(
            DiagnosticCode::ImportCycle,
            "模块 use 图形成循环",
            Some(pending[import].span.clone()),
        ));
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let reexports = prefix.map(prefix_reexports).unwrap_or_default();
    let mut imports = prefix
        .map(|prefix| prefix.names().imports.clone())
        .unwrap_or_default();
    imports.extend(resolve_imports(
        &pending,
        &module_bindings,
        modules,
        &reexports,
        &mut diagnostics,
    ));
    let definition_spans = prefix
        .map(|prefix| prefix.names().definition_spans.clone())
        .unwrap_or_default();
    let import_spans = prefix
        .map(|prefix| prefix.names().import_spans.clone())
        .unwrap_or_default();
    let resolution =
        NameResolution::with_session_spans(definitions, imports, definition_spans, import_spans);
    diagnose_import_conflicts(&resolution, &mut diagnostics);
    if diagnostics.is_empty() {
        debug_assert!(resolution_is_valid(&resolution));
        Ok(resolution)
    } else {
        Err(diagnostics)
    }
}

/// 内建块的公开再导出：层名即 `(模块, 别名)` 键，用户导入沿它解析到已解析的内建目标。
fn prefix_reexports(
    prefix: &crate::sema_prefix::SemaPrefix,
) -> BTreeMap<(ModuleId, String), Vec<Binding>> {
    let mut reexports = BTreeMap::<(ModuleId, String), Vec<Binding>>::new();
    for import in prefix.names().imports.iter().filter(|import| import.public) {
        reexports
            .entry((import.module, import.alias.clone()))
            .or_default()
            .push(Binding {
                namespace: import.namespace,
                target: match &import.target {
                    ResolvedTarget::Module(module) => BindingTarget::Module(*module),
                    ResolvedTarget::Def(definition) => BindingTarget::Def(*definition),
                    ResolvedTarget::External(path) => BindingTarget::External(path.clone()),
                },
            });
    }
    for bindings in reexports.values_mut() {
        bindings.sort();
        bindings.dedup();
    }
    reexports
}

fn validate_modules(
    external_packages: &BTreeSet<String>,
    modules: &[ParsedModule],
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut folded = BTreeMap::<String, &str>::new();
    let local_roots = modules
        .iter()
        .filter(|module| module.configured.module_active())
        .filter(|module| {
            module
                .package
                .as_ref()
                .is_none_or(|package| package.namespace.is_empty())
        })
        .filter_map(|module| module.path.split('.').next())
        .collect::<BTreeSet<_>>();
    for module in modules {
        if !module.builtin && module.local_path().split('.').next() == Some("std") {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::ReservedName,
                "用户源码不能声明保留模块根 `std`",
                Some(module.file.eof_span.clone()),
            ));
        }
        if module.configured.module_active() {
            let lowercase = module.path.to_ascii_lowercase();
            if let Some(previous) = folded.insert(lowercase, &module.path)
                && previous != module.path
            {
                diagnostics.push(Diagnostic::error(
                    DiagnosticCode::ModulePathCaseMismatch,
                    format!("模块路径 `{previous}` 与 `{}` 仅大小写不同", module.path),
                    Some(module.file.eof_span.clone()),
                ));
            }
        }
    }
    for alias in external_packages {
        if local_roots.contains(alias.as_str()) {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::ImportConflict,
                format!("依赖别名 `{alias}` 与当前 package 模块根冲突"),
                None,
            ));
        }
    }
    diagnostics
}

/// 物化覆盖的内建模块：候选与导入直接取自物化表，不在会话里重复收集。
fn materialized(module: &ParsedModule, prefix: Option<&crate::sema_prefix::SemaPrefix>) -> bool {
    module.builtin && prefix.is_some_and(|prefix| prefix.covers_module(&module.path))
}

fn collect_candidates(
    package_identity: &str,
    modules: &[ParsedModule],
    diagnostics: &mut Vec<Diagnostic>,
    prefix: Option<&crate::sema_prefix::SemaPrefix>,
) -> CandidateSet {
    let mut set = CandidateSet::default();
    for (index, module) in modules.iter().enumerate() {
        if !module.configured.module_active() || materialized(module, prefix) {
            continue;
        }
        let module_id = ModuleId(index as u32);
        for &item in module.file.items.as_slice(&module.arena.item_ids) {
            collect_item(
                package_identity,
                module_id,
                module,
                item,
                None,
                true,
                &mut set,
                diagnostics,
            );
        }
    }
    set
}

#[expect(
    clippy::too_many_arguments,
    reason = "递归定义收集显式传递稳定路径与所属作用域"
)]
fn collect_item(
    package_identity: &str,
    module_id: ModuleId,
    module: &ParsedModule,
    item_id: ItemId,
    parent: Option<usize>,
    module_binding: bool,
    set: &mut CandidateSet,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if !module.configured.item_active(item_id) {
        return;
    }
    let item = &module.arena.items[item_id.0 as usize];
    let Some(kind) = item_definition_kind(&item.kind) else {
        return;
    };
    let name = item
        .name
        .map(|symbol| module.tokens.intern.get_str(symbol).to_owned());
    // 预导入名保留给 `std` 自己的定义：内建源单元可以声明它们，用户源码不能。
    if module_binding && !module.builtin && name.as_deref().is_some_and(is_reserved_name) {
        diagnostics.push(Diagnostic::error(
            DiagnosticCode::ReservedName,
            format!("声明名 `{}` 是保留的预导入名称", name.as_deref().unwrap()),
            item.name_span.clone(),
        ));
    }
    let namespace = item_namespace(&item.kind);
    let candidate = push_candidate(
        package_identity,
        module_id,
        module,
        parent,
        name,
        kind,
        namespace,
        item.visibility,
        item.span.clone(),
        module_binding,
        set,
    );
    match item.kind {
        ItemKind::Struct { ref body, .. } => match body {
            StructBody::Newtype(field) => {
                collect_field(package_identity, module_id, module, field, candidate, set)
            }
            StructBody::Record(fields) => collect_fields(
                package_identity,
                module_id,
                module,
                fields.start as usize,
                fields.len as usize,
                candidate,
                set,
            ),
        },
        ItemKind::Union { fields, .. } => collect_fields(
            package_identity,
            module_id,
            module,
            fields.start as usize,
            fields.len as usize,
            candidate,
            set,
        ),
        ItemKind::Enum { variants, .. } => {
            collect_variants(
                package_identity,
                module_id,
                module,
                variants,
                candidate,
                set,
            );
        }
        ItemKind::Trait { items, .. } | ItemKind::Impl { items, .. } => {
            for &child in items.as_slice(&module.arena.item_ids) {
                collect_item(
                    package_identity,
                    module_id,
                    module,
                    child,
                    Some(candidate),
                    false,
                    set,
                    diagnostics,
                );
            }
        }
        ItemKind::ExternBlock { items, .. } => {
            for &child in items.as_slice(&module.arena.item_ids) {
                collect_item(
                    package_identity,
                    module_id,
                    module,
                    child,
                    Some(candidate),
                    true,
                    set,
                    diagnostics,
                );
            }
        }
        _ => {}
    }
}

fn collect_fields(
    package_identity: &str,
    module_id: ModuleId,
    module: &ParsedModule,
    start: usize,
    len: usize,
    parent: usize,
    set: &mut CandidateSet,
) {
    for index in start..start + len {
        if !module.configured.field_active(index) {
            continue;
        }
        let field = &module.arena.fields[index];
        collect_field(package_identity, module_id, module, field, parent, set);
    }
}

fn collect_field(
    package_identity: &str,
    module_id: ModuleId,
    module: &ParsedModule,
    field: &super::ast::Field,
    parent: usize,
    set: &mut CandidateSet,
) {
    let name = field
        .name
        .map(|symbol| module.tokens.intern.get_str(symbol).to_owned());
    push_candidate(
        package_identity,
        module_id,
        module,
        Some(parent),
        name,
        DefinitionKind::Field,
        Some(Namespace::Field),
        field.visibility,
        field.span.clone(),
        false,
        set,
    );
}

fn collect_variants(
    package_identity: &str,
    module_id: ModuleId,
    module: &ParsedModule,
    variants: super::ast::AstRange<super::ast::Variant>,
    parent: usize,
    set: &mut CandidateSet,
) {
    for index in variants.start as usize..(variants.start + variants.len) as usize {
        if !module.configured.variant_active(index) {
            continue;
        }
        let variant = &module.arena.variants[index];
        let candidate = push_candidate(
            package_identity,
            module_id,
            module,
            Some(parent),
            Some(module.tokens.intern.get_str(variant.name).to_owned()),
            DefinitionKind::Variant,
            Some(Namespace::Constructor),
            Visibility::Pub,
            variant.span.clone(),
            false,
            set,
        );
        if let VariantKind::Tuple(fields) | VariantKind::Struct(fields) = variant.kind {
            collect_fields(
                package_identity,
                module_id,
                module,
                fields.start as usize,
                fields.len as usize,
                candidate,
                set,
            );
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "定义候选构造需一次固定完整 DefPath 输入"
)]
fn push_candidate(
    package_identity: &str,
    module_id: ModuleId,
    module: &ParsedModule,
    parent: Option<usize>,
    name: Option<String>,
    kind: DefinitionKind,
    namespace: Option<Namespace>,
    visibility: Visibility,
    span: Span,
    module_binding: bool,
    set: &mut CandidateSet,
) -> usize {
    let identity = name.clone().unwrap_or_else(|| format!("@{}", span.start()));
    let ordinal = set
        .disambiguators
        .entry((module_id, parent, kind, identity.clone()))
        .and_modify(|value| *value += 1)
        .or_insert(0);
    // 内建块的稳定键由内建 package 身份给出，不能随编译它的用户 package 漂移：物化表按
    // `BUILTIN_PACKAGE_IDENTITY` 冻结，用户编译必须复现同一批键。
    let root = if module.builtin {
        super::BUILTIN_PACKAGE_IDENTITY
    } else {
        module
            .package
            .as_ref()
            .map_or(package_identity, |package| package.identity.as_str())
    };
    let mut def_path = parent
        .map(|index| set.values[index].def_path.clone())
        .unwrap_or_else(|| module_def_path(root, module.local_path()));
    push_path_component(&mut def_path, kind as u8, identity.as_bytes(), *ordinal);
    let stable_key = *blake3::hash(&def_path).as_bytes();
    let index = set.values.len();
    set.values.push(Candidate {
        module: module_id,
        parent,
        name,
        kind,
        namespace,
        visibility,
        span,
        def_path,
        stable_key,
        module_binding,
    });
    index
}

fn module_def_path(package_identity: &str, module: &str) -> Vec<u8> {
    let mut path = Vec::with_capacity(package_identity.len() + module.len() + 16);
    encode_bytes(&mut path, package_identity.as_bytes());
    encode_bytes(&mut path, module.as_bytes());
    path
}

fn push_path_component(path: &mut Vec<u8>, kind: u8, name: &[u8], ordinal: u32) {
    path.push(kind);
    encode_bytes(path, name);
    path.extend_from_slice(&ordinal.to_be_bytes());
}

fn encode_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}

fn item_definition_kind(kind: &ItemKind) -> Option<DefinitionKind> {
    Some(match kind {
        ItemKind::Use(_) | ItemKind::Error => return None,
        ItemKind::Function(_) => DefinitionKind::Function,
        ItemKind::Struct { .. } => DefinitionKind::Struct,
        ItemKind::Enum { .. } => DefinitionKind::Enum,
        ItemKind::Union { .. } => DefinitionKind::Union,
        ItemKind::TypeAlias { .. } => DefinitionKind::TypeAlias,
        ItemKind::Const { .. } => DefinitionKind::Const,
        ItemKind::Static { .. } => DefinitionKind::Static,
        ItemKind::Trait { .. } => DefinitionKind::Trait,
        ItemKind::Impl { .. } => DefinitionKind::Impl,
        ItemKind::ExternBlock { .. } => DefinitionKind::ExternBlock,
        ItemKind::GlobalAsm { .. } => DefinitionKind::GlobalAsm,
        ItemKind::SourceMacro { .. } => DefinitionKind::SourceMacro,
    })
}

fn item_namespace(kind: &ItemKind) -> Option<Namespace> {
    match kind {
        ItemKind::Function(_) | ItemKind::Const { .. } | ItemKind::Static { .. } => {
            Some(Namespace::Value)
        }
        ItemKind::Struct { .. }
        | ItemKind::Enum { .. }
        | ItemKind::Union { .. }
        | ItemKind::TypeAlias { .. }
        | ItemKind::Trait { .. } => Some(Namespace::Type),
        _ => None,
    }
}

fn diagnose_definition_conflicts(candidates: &[Candidate], diagnostics: &mut Vec<Diagnostic>) {
    let mut occupied = BTreeMap::<(ModuleId, Option<usize>, Namespace, String), usize>::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let (Some(namespace), Some(name)) = (candidate.namespace, candidate.name.as_ref()) else {
            continue;
        };
        let scope = if candidate.module_binding {
            None
        } else {
            candidate.parent
        };
        if let Some(previous) =
            occupied.insert((candidate.module, scope, namespace, name.clone()), index)
        {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::DuplicateDefinition,
                format!(
                    "同一命名空间重复声明 `{name}`；首次声明在字节 {}",
                    candidates[previous].span.start()
                ),
                Some(candidate.span.clone()),
            ));
            diagnostics.push(Diagnostic::note(
                DiagnosticCode::DuplicateDefinition,
                format!("`{name}` 的首次声明在这里"),
                Some(candidates[previous].span.clone()),
            ));
        }
    }
}

/// 按候选登记顺序分配稠密 `DefId`，编号从 `offset` 之后继续。
///
/// 内建模块在模块表里占据前缀，因此内建定义先由物化表占据编号前缀、用户定义追加在后：这正是
/// 物化表的编号前缀约定（见 `docs/src/internals/monomorphization-cache.md`）。跨块引用只走
/// 稳定键，负载路径把内建块整体替换为物化前缀，所以这里不做任何按键重排。
fn assign_def_ids(
    candidates: &[Candidate],
    diagnostics: &mut Vec<Diagnostic>,
    offset: u32,
) -> Vec<Definition> {
    // 键冲突检测需要按键有序的视图；它只读索引副本，不影响编号。
    let mut by_key = (0..candidates.len()).collect::<Vec<_>>();
    by_key.sort_unstable_by_key(|&index| candidates[index].stable_key);
    for pair in by_key.windows(2) {
        let left = &candidates[pair[0]];
        let right = &candidates[pair[1]];
        if left.stable_key == right.stable_key && left.def_path != right.def_path {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::DefinitionHashCollision,
                "两个不同 DefPath 产生相同 StableDefKey",
                Some(right.span.clone()),
            ));
        }
    }
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let id = offset
                .checked_add(u32::try_from(index).expect("定义数量必须落在 u32"))
                .expect("定义编号必须落在 u32");
            Definition {
                id: DefId(id),
                stable_key: candidate.stable_key,
                module: candidate.module,
                parent: candidate.parent.map(|parent| {
                    DefId(offset + u32::try_from(parent).expect("父定义必须落在 u32"))
                }),
                module_binding: candidate.module_binding,
                name: candidate.name.clone(),
                kind: candidate.kind,
                namespace: candidate.namespace,
                visibility: candidate.visibility,
                span: candidate.span.clone(),
            }
        })
        .collect()
}

fn module_bindings<'a>(definitions: impl IntoIterator<Item = &'a Definition>) -> ModuleBindings {
    let mut bindings = BTreeMap::<_, Vec<_>>::new();
    for definition in definitions {
        if !definition.module_binding {
            continue;
        }
        let (Some(namespace), Some(name)) = (definition.namespace, definition.name.as_ref()) else {
            continue;
        };
        bindings
            .entry((definition.module, name.clone()))
            .or_default()
            .push((namespace, definition.id, definition.visibility));
    }
    bindings
}

fn collect_imports(
    external_packages: &BTreeSet<String>,
    modules: &[ParsedModule],
    diagnostics: &mut Vec<Diagnostic>,
    prefix: Option<&crate::sema_prefix::SemaPrefix>,
) -> Vec<PendingImport> {
    let exact = modules
        .iter()
        .enumerate()
        .filter(|(_, module)| module.configured.module_active())
        .map(|(index, module)| (module.path.as_str(), ModuleId(index as u32)))
        .collect::<BTreeMap<_, _>>();
    let folded = modules
        .iter()
        .enumerate()
        .filter(|(_, module)| module.configured.module_active())
        .map(|(index, module)| (module.path.to_ascii_lowercase(), ModuleId(index as u32)))
        .collect::<BTreeMap<_, _>>();
    let builtin: BTreeSet<String> = modules
        .iter()
        .filter(|module| module.builtin && module.configured.module_active())
        .map(|module| module.path.clone())
        .collect();
    let mut pending = Vec::new();
    for (index, module) in modules.iter().enumerate() {
        if !module.configured.module_active() || materialized(module, prefix) {
            continue;
        }
        let owner = ModuleId(index as u32);
        for &item_id in module.file.items.as_slice(&module.arena.item_ids) {
            if !module.configured.item_active(item_id) {
                continue;
            }
            let item = &module.arena.items[item_id.0 as usize];
            let ItemKind::Use(tree) = item.kind else {
                continue;
            };
            let path_id = match tree {
                UseTreeKind::Path { path, .. } | UseTreeKind::Brace { path, .. } => path,
            };
            let path = path_text(module, path_id);
            let target = classify_module(
                &path,
                external_packages,
                &exact,
                &folded,
                &builtin,
                &module.path,
                module.package.as_deref(),
                item.span.clone(),
                diagnostics,
            );
            let Some(target) = target else {
                continue;
            };
            match tree {
                UseTreeKind::Path { alias, .. } => {
                    let alias = alias
                        .map(|symbol| module.tokens.intern.get_str(symbol).to_owned())
                        .or_else(|| path.last().cloned())
                        .unwrap_or_default();
                    pending.push(PendingImport {
                        module: owner,
                        target,
                        item: None,
                        alias,
                        public: item.visibility == Visibility::Pub,
                        span: item.span.clone(),
                    });
                }
                UseTreeKind::Brace { items, .. } => {
                    for index in items.start as usize..(items.start + items.len) as usize {
                        if !module.configured.use_item_active(index) {
                            continue;
                        }
                        let member = &module.arena.use_items[index];
                        let name = module.tokens.intern.get_str(member.name).to_owned();
                        let alias = member.alias.map_or_else(
                            || name.clone(),
                            |symbol| module.tokens.intern.get_str(symbol).to_owned(),
                        );
                        pending.push(PendingImport {
                            module: owner,
                            target: target.clone(),
                            item: Some(name),
                            alias,
                            public: item.visibility == Visibility::Pub,
                            span: member.span.clone(),
                        });
                    }
                }
            }
        }
    }
    pending.sort_by(|left, right| {
        (left.module, left.span.start(), &left.alias).cmp(&(
            right.module,
            right.span.start(),
            &right.alias,
        ))
    });
    pending
}

fn path_text(module: &ParsedModule, path_id: super::ast::PathId) -> Vec<String> {
    module.arena.paths[path_id.0 as usize]
        .segments
        .as_slice(&module.arena.segments)
        .iter()
        .map(|segment| module.tokens.intern.get_str(segment.name).to_owned())
        .collect()
}

pub(crate) fn private_builtin_module(path: &str) -> bool {
    path == "std.prelude"
        || path.starts_with("std.runtime.")
        || path == "runtime"
        || path.starts_with("runtime.")
}

#[expect(
    clippy::too_many_arguments,
    reason = "模块解析需要精确/折叠表、内建集合与导入者上下文"
)]
fn classify_module(
    path: &[String],
    external_packages: &BTreeSet<String>,
    exact: &BTreeMap<&str, ModuleId>,
    folded: &BTreeMap<String, ModuleId>,
    builtin: &BTreeSet<String>,
    importer: &str,
    package: Option<&crate::modules::PackageScope>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ModuleTarget> {
    let joined = path.join(".");
    // 公开的 `std.option` 等模块是 package API。`std.prelude`、`std.runtime.*` 与
    // `runtime.*` 仍是私有实现。未登记的 `std.*` 路径仍然按依赖别名处理。
    if let Some(&module) = exact.get(joined.as_str())
        && builtin.contains(&joined)
    {
        if private_builtin_module(&joined) && !importer.starts_with("std.") && importer != "std" {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::ReservedName,
                format!("`{joined}` 是 std 的私有实现模块，不是 package API"),
                Some(span),
            ));
            return None;
        }
        return Some(ModuleTarget::Local(module));
    }
    if let Some(package) = package
        && path.first().is_none_or(|root| root != "std")
    {
        let local = package.module_path(&joined);
        let dependency = path
            .first()
            .and_then(|alias| package.dependencies.get(alias));
        if dependency.is_some() && exact.contains_key(local.as_str()) {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::ImportConflict,
                format!("路径 `{joined}` 同时匹配本地模块与依赖别名"),
                Some(span),
            ));
            return None;
        }
        let resolved = if let Some((namespace, entry)) = dependency {
            if path.len() == 1 {
                entry.clone()
            } else {
                format!("{namespace}.{}", path[1..].join("."))
            }
        } else {
            local
        };
        if let Some(&module) = exact.get(resolved.as_str()) {
            return Some(ModuleTarget::Local(module));
        }
        let (code, message) = if folded.contains_key(&resolved.to_ascii_lowercase()) {
            (
                DiagnosticCode::ModulePathCaseMismatch,
                format!("模块路径 `{joined}` 的大小写与声明不一致"),
            )
        } else {
            (
                DiagnosticCode::ModuleNotFound,
                format!("找不到模块 `{joined}`"),
            )
        };
        diagnostics.push(Diagnostic::error(code, message, Some(span)));
        return None;
    }
    let external = path
        .first()
        .is_some_and(|root| root == "std" || external_packages.contains(root));
    if let Some(&module) = exact.get(joined.as_str()) {
        if external {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::ImportConflict,
                format!("路径 `{joined}` 同时匹配本地模块与依赖别名"),
                Some(span),
            ));
            return None;
        }
        return Some(ModuleTarget::Local(module));
    }
    if external {
        return Some(ModuleTarget::External(joined));
    }
    if let Some(module) = folded.get(&joined.to_ascii_lowercase()) {
        diagnostics.push(Diagnostic::error(
            DiagnosticCode::ModulePathCaseMismatch,
            format!(
                "模块路径 `{joined}` 的大小写与声明 `{}` 不一致",
                exact
                    .iter()
                    .find_map(|(path, id)| (*id == *module).then_some(*path))
                    .unwrap_or("<unknown>")
            ),
            Some(span),
        ));
    } else {
        diagnostics.push(Diagnostic::error(
            DiagnosticCode::ModuleNotFound,
            format!("找不到模块 `{joined}`"),
            Some(span),
        ));
    }
    None
}

fn import_cycle(imports: &[PendingImport], module_count: usize) -> Option<usize> {
    let mut edges = vec![Vec::new(); module_count];
    for (index, import) in imports.iter().enumerate() {
        if let ModuleTarget::Local(target) = import.target {
            edges[import.module.index()].push((target, index));
        }
    }
    let mut state = vec![0_u8; module_count];
    for module in 0..module_count {
        if state[module] == 0
            && let Some(edge) = visit_module(ModuleId(module as u32), &edges, &mut state)
        {
            return Some(edge);
        }
    }
    None
}

fn visit_module(
    module: ModuleId,
    edges: &[Vec<(ModuleId, usize)>],
    state: &mut [u8],
) -> Option<usize> {
    state[module.index()] = 1;
    for &(target, import) in &edges[module.index()] {
        match state[target.index()] {
            1 => return Some(import),
            0 => {
                if let Some(cycle) = visit_module(target, edges, state) {
                    return Some(cycle);
                }
            }
            _ => {}
        }
    }
    state[module.index()] = 2;
    None
}

fn resolve_imports(
    pending: &[PendingImport],
    definitions: &ModuleBindings,
    modules: &[ParsedModule],
    reexports: &BTreeMap<(ModuleId, String), Vec<Binding>>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<ResolvedImport> {
    let mut exports = BTreeMap::<(ModuleId, String), Vec<usize>>::new();
    for (index, import) in pending.iter().enumerate() {
        if import.public {
            exports
                .entry((import.module, import.alias.clone()))
                .or_default()
                .push(index);
        }
    }
    let mut state = vec![0_u8; pending.len()];
    let mut cache = vec![None; pending.len()];
    let mut resolved = Vec::new();
    for index in 0..pending.len() {
        match resolve_import(
            index,
            pending,
            definitions,
            &exports,
            reexports,
            modules,
            &mut state,
            &mut cache,
        ) {
            Ok(bindings) => {
                for binding in bindings {
                    resolved.push(ResolvedImport {
                        module: pending[index].module,
                        alias: pending[index].alias.clone(),
                        namespace: binding.namespace,
                        target: match binding.target {
                            BindingTarget::Module(module) => ResolvedTarget::Module(module),
                            BindingTarget::Def(definition) => ResolvedTarget::Def(definition),
                            BindingTarget::External(path) => ResolvedTarget::External(path),
                        },
                        public: pending[index].public,
                        span: pending[index].span.clone(),
                    });
                }
            }
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }
    resolved.sort_by(|left, right| {
        (
            left.module,
            left.span.start(),
            &left.alias,
            left.namespace,
            left.public,
            resolved_target_key(&left.target),
        )
            .cmp(&(
                right.module,
                right.span.start(),
                &right.alias,
                right.namespace,
                right.public,
                resolved_target_key(&right.target),
            ))
    });
    resolved
}

#[expect(
    clippy::too_many_arguments,
    reason = "解析单条导入需要待解队列、定义表、导出表、再导出表与状态位"
)]
fn resolve_import(
    index: usize,
    pending: &[PendingImport],
    definitions: &ModuleBindings,
    exports: &BTreeMap<(ModuleId, String), Vec<usize>>,
    reexports: &BTreeMap<(ModuleId, String), Vec<Binding>>,
    modules: &[ParsedModule],
    state: &mut [u8],
    cache: &mut [Option<Vec<Binding>>],
) -> Result<Vec<Binding>, Diagnostic> {
    if let Some(bindings) = &cache[index] {
        return Ok(bindings.clone());
    }
    if state[index] == 1 {
        return Err(Diagnostic::error(
            DiagnosticCode::ImportCycle,
            "pub use 再导出形成循环",
            Some(pending[index].span.clone()),
        ));
    }
    state[index] = 1;
    let import = &pending[index];
    let mut bindings = match (&import.target, import.item.as_ref()) {
        (ModuleTarget::External(path), None) => vec![Binding {
            namespace: Namespace::Module,
            target: BindingTarget::External(path.clone()),
        }],
        (ModuleTarget::External(path), Some(name)) => vec![Binding {
            namespace: Namespace::External,
            target: BindingTarget::External(format!("{path}.{name}")),
        }],
        (ModuleTarget::Local(module), None) => vec![Binding {
            namespace: Namespace::Module,
            target: BindingTarget::Module(*module),
        }],
        // 编译器拥有的标准项（原语、非预导入类型）没有源码声明，仍必须保留规范路径
        // 和导入别名；按规范路径还原为 External，导入别名不改变原语身份。
        (ModuleTarget::Local(module), Some(name))
            if modules[module.index()].builtin
                && crate::semantics::model::compiler_owned_item(&format!(
                    "{}.{name}",
                    modules[module.index()].path
                )) =>
        {
            vec![Binding {
                namespace: Namespace::External,
                target: BindingTarget::External(format!("{}.{name}", modules[module.index()].path)),
            }]
        }
        (ModuleTarget::Local(module), Some(name)) => resolve_local_item(
            import,
            *module,
            name,
            pending,
            definitions,
            exports,
            reexports,
            modules,
            state,
            cache,
        )?,
    };
    bindings.sort();
    bindings.dedup();
    state[index] = 2;
    cache[index] = Some(bindings.clone());
    Ok(bindings)
}

#[expect(
    clippy::too_many_arguments,
    reason = "本地导入解析需同时访问定义表、再导出图与 DFS 状态"
)]
fn resolve_local_item(
    import: &PendingImport,
    target_module: ModuleId,
    name: &str,
    pending: &[PendingImport],
    definitions: &ModuleBindings,
    exports: &BTreeMap<(ModuleId, String), Vec<usize>>,
    reexports: &BTreeMap<(ModuleId, String), Vec<Binding>>,
    modules: &[ParsedModule],
    state: &mut [u8],
    cache: &mut [Option<Vec<Binding>>],
) -> Result<Vec<Binding>, Diagnostic> {
    let mut bindings = Vec::new();
    let mut private = false;
    if let Some(local) = definitions.get(&(target_module, name.to_owned())) {
        for &(namespace, definition, visibility) in local {
            if import.module == target_module || visibility == Visibility::Pub {
                bindings.push(Binding {
                    namespace,
                    target: BindingTarget::Def(definition),
                });
            } else {
                private = true;
            }
        }
    }
    // 物化块的内建再导出已在构建期解析完成，这里直接取用已解析目标。
    if let Some(installed) = reexports.get(&(target_module, name.to_owned())) {
        bindings.extend(installed.iter().cloned());
    }
    if let Some(chained) = exports.get(&(target_module, name.to_owned())) {
        for &reexport in chained {
            bindings.extend(resolve_import(
                reexport,
                pending,
                definitions,
                exports,
                reexports,
                modules,
                state,
                cache,
            )?);
        }
    }
    if bindings.is_empty() {
        let child_path = if modules[target_module.index()].path.is_empty() {
            name.to_owned()
        } else {
            format!("{}.{}", modules[target_module.index()].path, name)
        };
        if let Some((index, _)) = modules
            .iter()
            .enumerate()
            .find(|(_, module)| module.path == child_path)
        {
            bindings.push(Binding {
                namespace: Namespace::Module,
                target: BindingTarget::Module(ModuleId(index as u32)),
            });
        }
    }
    if !bindings.is_empty() {
        return Ok(bindings);
    }
    let (code, message) = if private {
        (
            DiagnosticCode::PrivateImport,
            format!("不能跨模块导入私有项 `{name}`"),
        )
    } else {
        (
            DiagnosticCode::ImportNotFound,
            format!(
                "模块 `{}` 中不存在可导入项 `{name}`",
                modules[target_module.index()].path
            ),
        )
    };
    Err(Diagnostic::error(code, message, Some(import.span.clone())))
}

fn diagnose_import_conflicts(resolution: &NameResolution, diagnostics: &mut Vec<Diagnostic>) {
    let mut occupied = BTreeMap::<(ModuleId, Namespace, String), Span>::new();
    for (index, definition) in resolution.definitions.iter().enumerate() {
        let (Some(namespace), Some(name)) = (definition.namespace, definition.name.as_ref()) else {
            continue;
        };
        if definition.module_binding {
            occupied.insert(
                (definition.module, namespace, name.clone()),
                resolution.definition_span(index).clone(),
            );
        }
    }
    let mut external_aliases = BTreeSet::new();
    for (index, import) in resolution.imports.iter().enumerate() {
        if import.namespace == Namespace::External {
            if !external_aliases.insert((import.module, import.alias.clone()))
                || occupied
                    .keys()
                    .any(|(module, _, name)| *module == import.module && name == &import.alias)
            {
                push_import_conflict(import, resolution.import_span(index), diagnostics);
            }
            continue;
        }
        let key = (import.module, import.namespace, import.alias.clone());
        if occupied
            .insert(key, resolution.import_span(index).clone())
            .is_some()
        {
            push_import_conflict(import, resolution.import_span(index), diagnostics);
        }
    }
}

fn push_import_conflict(import: &ResolvedImport, span: &Span, diagnostics: &mut Vec<Diagnostic>) {
    diagnostics.push(Diagnostic::error(
        DiagnosticCode::ImportConflict,
        format!("导入别名 `{}` 与当前模块已有名称冲突", import.alias),
        Some(span.clone()),
    ));
}
