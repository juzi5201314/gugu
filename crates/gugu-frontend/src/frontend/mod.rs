use std::{collections::BTreeSet, path::PathBuf};

use crate::{
    diagnostics::{Diagnostic, DiagnosticCode},
    source::{ExpansionId, SourceFileId, SourceMap, SourceSnapshot, Span},
};

pub(crate) mod analysis;
mod ast;
pub(crate) mod attr;
pub(crate) mod cfg;
mod expand;
pub(crate) mod format;
pub(crate) mod gc;
pub(crate) mod gir;
pub(crate) mod hir;
mod intern;
pub(crate) mod late;
mod lex;
pub(crate) mod mono;
mod names;
mod parse;
pub mod prebuild;
#[cfg(all(test, feature = "runtime-gc-probes"))]
mod probe_source_tests;
pub(crate) mod semantics;
pub(crate) mod string;
mod token;
mod types;

use ast::{AstArena, AstFile, ItemKind};
pub(crate) use lex::lex;
pub(crate) use parse::parse;
#[cfg(test)]
#[allow(unused_imports, reason = "parse helper 仅由部分测试目标共享")]
pub(crate) use parse::{dump_ast, has_main_fn, parent_before_child};
pub(crate) use token::TokenBuffer;

pub(crate) enum SourceInput<'a> {
    EmptyPackage,
    Sources {
        source_map: &'a mut SourceMap,
        entry: &'a str,
        source_root: &'a str,
        package_identity: &'a str,
        require_main: bool,
        cfg: &'a cfg::CfgContext,
        external_packages: &'a BTreeSet<String>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct FrontendOutput {
    pub(crate) path: Option<PathBuf>,
    pub(crate) source_len: u64,
    pub(crate) token_count: usize,
    pub(crate) item_count: usize,
    pub(crate) node_count: u64,
    pub(crate) modules: Vec<ParsedModule>,
    pub(crate) names: names::NameResolution,
    pub(crate) types: Vec<types::Layout>,
    pub(crate) comptime_registry: (u32, [u8; 32]),
    pub(crate) expansion_inputs: expand::ExpansionInputs,
    #[cfg(test)]
    pub(crate) semantics: semantics::CheckedSemantics,
    pub(crate) hir: hir::Validated,
    pub(crate) analysis: analysis::AnalysisWorldV1,
    pub(crate) mono: mono::MonoWorldV1,
    pub(crate) gir: gir::GirWorldV1,
    pub(crate) gir_stats: gir::pass::GirPassStats,
    pub(crate) lints: Vec<Diagnostic>,
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedModule {
    pub(crate) path: String,
    pub(crate) file: AstFile,
    pub(crate) arena: AstArena,
    pub(crate) tokens: TokenBuffer,
    pub(crate) configured: cfg::ConfiguredAst,
    /// 该模块来自 compiler 内建的源树登记，而不是用户源码。
    ///
    /// `std.option` 等公开标准库模块是 package API。`std.prelude`、`std.runtime.*` 与
    /// `runtime.*` 仍是私有实现：用户源码不能声明 `std` 根，也不能导入这些私有模块。
    pub(crate) builtin: bool,
}

pub(crate) fn bootstrap(
    input: SourceInput<'_>,
    queries: &crate::query::QueryEngine,
    resources: crate::runtime::RuntimeResources<'_>,
) -> Result<FrontendOutput, Vec<Diagnostic>> {
    match input {
        SourceInput::EmptyPackage => Ok(FrontendOutput {
            path: None,
            source_len: 0,
            token_count: 0,
            item_count: 0,
            node_count: 0,
            modules: Vec::new(),
            names: names::NameResolution::default(),
            types: Vec::new(),
            comptime_registry: semantics::comptime::registry::EARLY_REGISTRY_IDENTITY,
            expansion_inputs: expand::ExpansionInputs::default(),
            #[cfg(test)]
            semantics: semantics::CheckedSemantics::default(),
            hir: hir::Validated::freeze(hir::Module::default())
                .map_err(|error| vec![error])?
                .0,
            analysis: analysis::empty_world(),
            mono: mono::empty_world(),
            gir: gir::empty_world(),
            gir_stats: gir::pass::GirPassStats::default(),
            lints: Vec::new(),
        }),
        SourceInput::Sources {
            source_map,
            entry,
            source_root,
            package_identity,
            require_main,
            cfg,
            external_packages,
        } => check_sources(
            source_map,
            entry,
            source_root,
            package_identity,
            require_main,
            cfg,
            external_packages,
            queries,
            resources,
        ),
    }
}

/// 探针入口程序所在的内建源前缀。
const ENTRY_ONLY_BUILTIN_SOURCES: &str = "std/runtime/fixtures/";

/// 探针专用内建源：探针库与探针入口程序。
///
/// 它们随 `runtime-gc-probes` 特性进入 artifact 源表，只供 `CompileInput::RuntimeGcProbe`
/// 的编译安装。普通编译一旦安装：探针库会把自身的大拷贝诊断混进用户诊断，探针入口程序会为
/// 本不属于该程序的 `main` 多派生一份世界，且其中 `#[cfg(runtime_gc_case = ...)]` 的选择
/// 函数在探针编译之外全部落空。
pub(crate) fn is_probe_builtin_source(logical_path: &str) -> bool {
    logical_path == "std/runtime/gc_probes.gg"
        || logical_path.starts_with(ENTRY_ONLY_BUILTIN_SOURCES)
}

/// 内建源单元是否进入入口为 `entry` 的本次编译。
pub(crate) fn installs_builtin_source(logical_path: &str, entry: &str) -> bool {
    !is_probe_builtin_source(logical_path) || is_probe_builtin_source(entry)
}

/// 把 build-time artifact 的 source blob 适配为本次 session 的 frontend 编号前缀。
///
/// source table 的唯一权威是嵌入 artifact；compiler 不再通过 runtime 模块的 `include_str!`
/// 持有第二份源码表。内建源占据文件 ID 前缀（artifact manifest 顺序），用户源码排序追加在后，
/// 物化表的每个下标因此在新会话里仍然指向同一个内建单元。
fn install_builtin_sources(
    source_map: &mut SourceMap,
    resources: crate::runtime::RuntimeResources<'_>,
    entry: &str,
) -> Result<(BTreeSet<String>, BTreeSet<String>), Vec<Diagnostic>> {
    let mut paths = BTreeSet::new();
    let mut runtime_paths = BTreeSet::new();
    let mut snapshots = Vec::new();
    for source in resources.source_blobs() {
        let logical = source.logical_path.as_str();
        if !installs_builtin_source(logical, entry) {
            continue;
        }
        let snapshot =
            SourceSnapshot::from_bytes(logical, source.source.as_bytes()).map_err(|error| {
                vec![Diagnostic::error(
                    DiagnosticCode::ModuleInvalidPath,
                    error.to_string(),
                    None,
                )]
            })?;
        snapshots.push(snapshot);
        paths.insert(logical.to_owned());
        if source.role == gugu_compiler_core::RuntimeSourceRole::Runtime {
            runtime_paths.insert(logical.to_owned());
        }
    }
    source_map
        .prepend_builtin_prefix(snapshots)
        .map_err(|error| {
            vec![Diagnostic::error(
                DiagnosticCode::ModuleInvalidPath,
                format!("内建源单元与 package 源码冲突：{error}"),
                None,
            )]
        })?;
    Ok((paths, runtime_paths))
}

#[expect(
    clippy::too_many_arguments,
    reason = "前端入口需要源码映射、根路径、包身份与 query 引擎"
)]
fn check_sources(
    source_map: &mut SourceMap,
    entry: &str,
    source_root: &str,
    package_identity: &str,
    require_main: bool,
    cfg: &cfg::CfgContext,
    external_packages: &BTreeSet<String>,
    queries: &crate::query::QueryEngine,
    resources: crate::runtime::RuntimeResources<'_>,
) -> Result<FrontendOutput, Vec<Diagnostic>> {
    let (builtin_paths, runtime_paths) = install_builtin_sources(source_map, resources, entry)?;
    let mut modules = parse_modules(source_map, source_root, cfg, &builtin_paths)?;
    // 内建块占据模块编号前缀：物化表里的 owner/定义下标都以「内建块在前」为前提。
    modules.sort_by(|left, right| (!left.builtin, &left.path).cmp(&(!right.builtin, &right.path)));
    let entry_file = source_map.file_id(entry).ok_or_else(|| {
        vec![Diagnostic::error(
            DiagnosticCode::ModuleNotFound,
            format!("target 入口 `{entry}` 未进入源码模块表"),
            None,
        )]
    })?;
    let entry_function = modules
        .iter()
        .enumerate()
        .find(|(_, module)| module.file.source == entry_file)
        .and_then(|(module, parsed)| {
            active_main(parsed).map(|function| semantics::model::CallableId {
                module,
                function: function.0,
            })
        });
    let has_main = entry_function.is_some();
    if require_main && !has_main {
        let span = source_map_span(source_map, entry_file, 0, 0)?;
        return Err(vec![Diagnostic::error(
            DiagnosticCode::MissingMain,
            "target 入口在 cfg 裁项与宏展开后必须包含 `fn main() { ... }` 或 `fn main() = ...`",
            Some(span),
        )]);
    }
    let (names, derived, expansion_inputs) = derive_world(
        &mut modules,
        source_map,
        cfg,
        package_identity,
        external_packages,
        entry_function,
        queries,
        &runtime_paths,
    )?;
    let mut lints = semantics::must_use_lints(&modules, &derived.checked, source_map)
        .map_err(|errors| expand::reanchor_errors(errors, source_map))?;
    lints.extend(
        gir::large_copy_lints(&modules, &derived.gir, source_map)
            .map_err(|errors| expand::reanchor_errors(errors, source_map))?,
    );
    Ok(frontend_output(
        entry,
        source_map,
        modules,
        names,
        derived.layouts,
        derived.registry_identity,
        expansion_inputs,
        derived.checked,
        derived.hir,
        derived.analysis,
        derived.mono,
        derived.gir,
        derived.gir_stats,
        lints,
    ))
}

/// 展开与语义检查的公共段：解析后的模块 → 名称解析 + 冻结 world。
///
/// 用户编译与构建期物化走同一段管线，因此内建块的编号与指纹在两条路径上同源。
#[expect(
    clippy::too_many_arguments,
    reason = "派生入口需要源码映射、cfg、入口与 query 引擎"
)]
fn derive_world(
    modules: &mut [ParsedModule],
    source_map: &mut SourceMap,
    cfg: &cfg::CfgContext,
    package_identity: &str,
    external_packages: &BTreeSet<String>,
    entry_function: Option<semantics::model::CallableId>,
    queries: &crate::query::QueryEngine,
    runtime_paths: &BTreeSet<String>,
) -> Result<
    (
        names::NameResolution,
        semantics::DerivedWorld,
        expand::ExpansionInputs,
    ),
    Vec<Diagnostic>,
> {
    let expansion_inputs = expand::run(
        modules,
        source_map,
        cfg,
        queries,
        package_identity,
        external_packages,
    )
    .map_err(|errors| expand::reanchor_errors(errors, source_map))?;
    let names = names::analyze(package_identity, external_packages, modules)?;
    let derived = semantics::check(
        modules,
        &names,
        source_map,
        cfg,
        entry_function,
        queries,
        runtime_paths,
    )
    .map_err(|errors| expand::reanchor_errors(errors, source_map))?;
    Ok((names, derived, expansion_inputs))
}

/// 内建块派生结果：构建期物化的输入，每张表都是 canonical 编码的字节。
///
/// 编号前缀就是 artifact 的 source manifest 顺序；用户模块在用户编译里追加在它之后，
/// 因此这里的每个下标在用户编译中仍然有效。
pub(crate) struct BuiltinDerivation {
    pub(crate) module_order: Vec<String>,
    pub(crate) names: Vec<u8>,
    pub(crate) identities: Vec<u8>,
    pub(crate) checked: Vec<u8>,
    pub(crate) early_constants: Vec<u8>,
    pub(crate) hir: Vec<u8>,
    pub(crate) gir_bodies: Vec<Vec<u8>>,
    pub(crate) record_types: Vec<u8>,
}

/// canonical 编码一张物化表；编码失败是 compiler invariant，不是用户诊断。
fn encode_table<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, Vec<Diagnostic>> {
    crate::canonical::to_vec(value).map_err(|error| {
        vec![Diagnostic::error(
            DiagnosticCode::BackendInvariant,
            format!("内建物化表编码失败：{error}"),
            None,
        )]
    })
}

/// 只派生内建块：入口取内建模块之一、关闭 `require_main`、按安装规则排除探针源。
pub(crate) fn derive_builtin(
    source_map: &mut SourceMap,
    queries: &crate::query::QueryEngine,
    resources: crate::runtime::RuntimeResources<'_>,
    cfg: &cfg::CfgContext,
) -> Result<BuiltinDerivation, Vec<Diagnostic>> {
    let (builtin_paths, runtime_paths) = install_builtin_sources(source_map, resources, "")?;
    let mut modules = parse_modules(source_map, "", cfg, &builtin_paths)?;
    modules.sort_by(|left, right| left.path.cmp(&right.path));
    let module_order = modules.iter().map(|module| module.path.clone()).collect();
    let (names, derived, _expansion_inputs) = derive_world(
        &mut modules,
        source_map,
        cfg,
        BUILTIN_PACKAGE_IDENTITY,
        &BTreeSet::new(),
        None,
        queries,
        &runtime_paths,
    )?;
    Ok(BuiltinDerivation {
        module_order,
        names: encode_table(&names)?,
        identities: encode_table(&derived.identities)?,
        checked: encode_table(&derived.checked)?,
        early_constants: encode_table(&derived.early)?,
        hir: encode_table(derived.hir.module())?,
        gir_bodies: derived
            .gir
            .bodies
            .iter()
            .map(encode_table)
            .collect::<Result<Vec<_>, _>>()?,
        record_types: encode_table(&derived.mono.contracts)?,
    })
}

/// 内建块的包身份：artifact 源表在用户 package 之外，必须与任何用户包身份区分。
const BUILTIN_PACKAGE_IDENTITY: &str = "builtin";

fn parse_modules(
    source_map: &SourceMap,
    source_root: &str,
    cfg: &cfg::CfgContext,
    builtin_paths: &BTreeSet<String>,
) -> Result<Vec<ParsedModule>, Vec<Diagnostic>> {
    let mut modules = Vec::with_capacity(source_map.snapshots().len());
    let mut diagnostics = Vec::new();
    let mut occupied = std::collections::BTreeMap::new();
    for snapshot in source_map.snapshots() {
        let file = source_map
            .file_id(snapshot.logical_path())
            .expect("源码表快照必须有稳定文件 ID");
        let builtin = builtin_paths.contains(snapshot.logical_path());
        let path = match builtin_module_path(snapshot, builtin_paths) {
            Some(path) => path,
            None => match module_path(snapshot, source_root, source_map, file) {
                Ok(path) => path,
                Err(diagnostic) => {
                    diagnostics.push(diagnostic);
                    continue;
                }
            },
        };
        if let Some(previous) = occupied.insert(path.clone(), snapshot.logical_path()) {
            diagnostics.push(module_path_conflict(
                snapshot, source_map, file, &path, previous,
            ));
            continue;
        }
        match parse_module(snapshot, source_map, file, path, cfg, builtin) {
            Ok(module) => modules.push(module),
            Err(errors) => diagnostics.extend(errors),
        }
    }
    if diagnostics.is_empty() {
        Ok(modules)
    } else {
        Err(diagnostics)
    }
}

/// 返回内建源单元对应的模块路径。
///
/// 内建逻辑路径已经是 package 相对路径（例如 `std/runtime/platform.gg`），直接去掉扩展名并
/// 把分隔符换成 `.`；它不受用户 `source_root` 约束。
fn builtin_module_path(
    snapshot: &SourceSnapshot,
    builtin_paths: &BTreeSet<String>,
) -> Option<String> {
    let logical = snapshot.logical_path();
    if !builtin_paths.contains(logical) {
        return None;
    }
    logical
        .strip_suffix(".gg")
        .map(|path| path.replace('/', "."))
}

fn parse_module(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    file: SourceFileId,
    path: String,
    cfg: &cfg::CfgContext,
    builtin: bool,
) -> Result<ParsedModule, Vec<Diagnostic>> {
    let lexed = lex(snapshot, source_map, file);
    if !lexed.diagnostics.is_empty() || lexed.buffer.has_error_tokens() {
        return Err(lexed.diagnostics);
    }
    let mut tokens = lexed.buffer;
    let parsed = parse(snapshot.content(), source_map, file, &mut tokens);
    if !parsed.diagnostics.is_empty() {
        return Err(parsed.diagnostics);
    }
    let configured = cfg::configure(
        snapshot,
        source_map,
        &parsed.file,
        &parsed.arena,
        &tokens,
        cfg,
    )?;
    Ok(ParsedModule {
        path,
        file: parsed.file,
        arena: parsed.arena,
        tokens,
        configured,
        builtin,
    })
}

fn module_path_conflict(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    file: SourceFileId,
    path: &str,
    previous: &str,
) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::ModuleInvalidPath,
        format!(
            "模块路径 `{path}` 同时由 `{previous}` 与 `{}` 声明",
            snapshot.logical_path()
        ),
        source_map_span(source_map, file, 0, 0).ok(),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "汇总所有前端阶段的 world 与诊断，参数即各阶段产物"
)]
fn frontend_output(
    entry: &str,
    source_map: &SourceMap,
    modules: Vec<ParsedModule>,
    names: names::NameResolution,
    types: Vec<types::Layout>,
    comptime_registry: (u32, [u8; 32]),
    expansion_inputs: expand::ExpansionInputs,
    semantics: semantics::CheckedSemantics,
    hir: hir::Validated,
    analysis: analysis::AnalysisWorldV1,
    mono: mono::MonoWorldV1,
    gir: gir::GirWorldV1,
    gir_stats: gir::pass::GirPassStats,
    lints: Vec<Diagnostic>,
) -> FrontendOutput {
    #[cfg(not(test))]
    drop(semantics);
    FrontendOutput {
        path: Some(PathBuf::from(entry)),
        source_len: source_map
            .snapshots()
            .iter()
            .map(|snapshot| snapshot.content().len() as u64)
            .sum(),
        token_count: modules
            .iter()
            .map(|module| module.tokens.tokens.len())
            .sum(),
        item_count: modules
            .iter()
            .map(|module| module.file.items.len as usize)
            .sum(),
        node_count: modules
            .iter()
            .map(|module| u64::from(module.arena.next_node))
            .sum(),
        modules,
        names,
        types,
        comptime_registry,
        expansion_inputs,
        #[cfg(test)]
        semantics,
        hir,
        analysis,
        mono,
        gir,
        gir_stats,
        lints,
    }
}

/// 在源码表上取 span，并把 `SpanError` 映射为 `SpanOutOfBounds` 诊断。
///
/// 诊断契约闸门直接调用本函数验证 E0007 的归属；公开编译输入无法触达该分支，
/// 因为现有调用点都请求已注册文件的 `(0, 0)`。
pub(crate) fn source_map_span(
    source_map: &SourceMap,
    file: SourceFileId,
    start: usize,
    end: usize,
) -> Result<Span, Vec<Diagnostic>> {
    source_map
        .span(file, start, end, ExpansionId::ROOT)
        .map_err(|error| {
            vec![Diagnostic::error(
                DiagnosticCode::SpanOutOfBounds,
                error.to_string(),
                None,
            )]
        })
}

fn module_path(
    snapshot: &SourceSnapshot,
    source_root: &str,
    source_map: &SourceMap,
    file: SourceFileId,
) -> Result<String, Diagnostic> {
    let logical = snapshot.logical_path();
    let relative = if source_root.is_empty() {
        logical
    } else {
        logical
            .strip_prefix(source_root)
            .and_then(|path| path.strip_prefix('/'))
            .ok_or_else(|| {
                Diagnostic::error(
                    DiagnosticCode::ModuleInvalidPath,
                    format!("源码 `{logical}` 不在 target 源码根 `{source_root}` 下"),
                    source_map_span(source_map, file, 0, 0).ok(),
                )
            })?
    };
    let path = relative.strip_suffix(".gg").ok_or_else(|| {
        Diagnostic::error(
            DiagnosticCode::ModuleInvalidPath,
            format!("模块源码 `{logical}` 必须使用 `.gg` 扩展名"),
            source_map_span(source_map, file, 0, 0).ok(),
        )
    })?;
    let mut segments = path.split('/').collect::<Vec<_>>();
    if segments.last() == Some(&"mod") {
        segments.pop();
    }
    if segments
        .iter()
        .any(|segment| !valid_module_segment(segment))
    {
        return Err(Diagnostic::error(
            DiagnosticCode::ModuleInvalidPath,
            format!("模块源码 `{logical}` 包含非法模块名"),
            source_map_span(source_map, file, 0, 0).ok(),
        ));
    }
    Ok(segments.join("."))
}

fn valid_module_segment(segment: &str) -> bool {
    let mut bytes = segment.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn active_main(module: &ParsedModule) -> Option<ast::FnId> {
    module
        .file
        .items
        .as_slice(&module.arena.item_ids)
        .iter()
        .copied()
        .filter(|&item| module.configured.item_active(item))
        .find_map(|item| match module.arena.items[item.0 as usize].kind {
            ItemKind::Function(function) => {
                let declaration = &module.arena.fns[function.0 as usize];
                (declaration
                    .name
                    .is_some_and(|name| module.tokens.intern.get_str(name) == "main")
                    && matches!(declaration.body, ast::FnBody::Block(_) | ast::FnBody::Eq(_)))
                .then_some(function)
            }
            _ => None,
        })
}

#[cfg(test)]
mod docs_code_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod lex_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod parse_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod stage10_tests;
