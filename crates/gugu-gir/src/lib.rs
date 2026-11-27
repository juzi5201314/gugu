#![forbid(unsafe_code)]

//! GIR 层：冻结 HIR 之后的 generic GIR、单态化、late comptime、全程序分析、放置、GC 元数据
//! 与 runtime 契约交叉视图，以及从源码到 GIR world 的前端入口 [`bootstrap_with`]。
//!
//! 内建块物化（[`prebuild`]）与安装（`prefix`）也在本层：它们的表覆盖 sema 层与 GIR 层。
//! 只依赖 `gugu-base`、`gugu-runtime-abi` 与 `gugu-sema`；LIR、后端与镜像在上层。

use std::{collections::BTreeSet, path::PathBuf};

use gugu_base::{canonical, diagnostics, hash, query, source, target};
use gugu_runtime_abi as runtime;

use diagnostics::{Diagnostic, DiagnosticCode, Severity};
use query::QueryEngine;
use source::{ExpansionId, SourceMap, SourceSnapshot};
use target::TargetName;

pub mod analysis;
pub mod contract;
pub mod functions;
pub mod gc;
pub mod gir;
mod harness;
pub mod late;
pub mod mono;
mod pipeline;
pub mod prebuild;
mod prefix;
#[cfg(all(test, feature = "runtime-gc-probes"))]
mod probe_source_tests;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;

#[cfg(all(any(test, feature = "test-support"), feature = "slow-tests"))]
use gugu_sema::modules::logical_to_module_path;
use gugu_sema::modules::parse_modules;
pub use gugu_sema::modules::source_map_span;
pub use gugu_sema::{
    BUILTIN_PACKAGE_IDENTITY, ParsedModule, ast, attr, cfg, docs, expand, format, hir,
    is_probe_builtin_source, names, sema_prefix, semantics, types,
};

use ast::ItemKind;

pub enum SourceInput<'a> {
    EmptyPackage,
    Sources {
        source_map: &'a mut SourceMap,
        entry: &'a str,
        source_root: &'a str,
        package_identity: &'a str,
        require_main: bool,
        cfg: &'a cfg::CfgContext,
        external_packages: &'a BTreeSet<String>,
        embed: Option<&'a gugu_sema::semantics::comptime::embed::EmbedHost>,
    },
}

#[derive(Clone, Debug)]
pub struct FrontendOutput {
    pub path: Option<PathBuf>,
    pub source_len: u64,
    pub token_count: usize,
    pub item_count: usize,
    pub node_count: u64,
    pub modules: Vec<ParsedModule>,
    pub names: names::NameResolution,
    pub types: Vec<types::Layout>,
    pub comptime_registry: (u32, [u8; 32]),
    pub expansion_inputs: expand::ExpansionInputs,
    #[cfg(any(test, feature = "test-support"))]
    pub semantics: semantics::CheckedSemantics,
    pub hir: hir::Validated,
    pub analysis: analysis::AnalysisWorldV1,
    /// 从内建函数表安装的实例；后续层按同一集合安装，测试据此观察安装覆盖面。
    pub installed: functions::InstalledFunctions,
    pub mono: mono::MonoWorldV1,
    pub gir: gir::GirWorldV1,
    pub gir_stats: gir::pass::GirPassStats,
    /// 公共 API 文档表；`gugu doc` 的消费产物。
    pub docs: docs::DocWorld,
    pub lints: Vec<Diagnostic>,
}

impl FrontendOutput {
    /// Frontend action 的摘要行：入口、模块/字节/记号/AST/定义/导入/类型布局计数。
    pub fn detail(&self) -> String {
        match &self.path {
            Some(path) => format!(
                "{}，{} 个模块 / {} 字节，{} 个记号，{} 个 AST 项 / {} 个节点，{} 个定义 / {} 个导入，{} 个类型布局",
                path.display(),
                self.modules.len(),
                self.source_len,
                self.token_count,
                self.item_count,
                self.node_count,
                self.names.definitions.len(),
                self.names.imports.len(),
                self.types.len(),
            ),
            None => "空模块".to_owned(),
        }
    }
}

/// 内建块的加载方式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrefixMode {
    /// 生产路径：按 `prebuilt` 物化表安装内建块。
    Install,
    /// 测试路径：忽略物化表，内建块与用户模块一起现算。
    ///
    /// 只用于比对两条加载路径的编号无关指纹；它不是源码回退——`Install` 模式下缺少物化表
    /// 仍然是 compiler invariant 失败。
    #[cfg(all(any(test, feature = "test-support"), feature = "slow-tests"))]
    Derive,
}

/// 前端入口：解析、展开、名称解析与语义检查。
///
/// 生产入口是 [`bootstrap_with`]（会话按物化表安装内建块）；这个包装只给测试用，
/// 让不关心内建块加载方式的用例少传一个参数。
#[cfg(any(test, feature = "test-support"))]
pub fn bootstrap(
    input: SourceInput<'_>,
    queries: &crate::query::QueryEngine,
    resources: crate::runtime::RuntimeResources<'_>,
) -> Result<FrontendOutput, Vec<Diagnostic>> {
    bootstrap_with(input, queries, resources, PrefixMode::Install)
}

/// 带内建块加载方式的前端入口。
pub fn bootstrap_with(
    input: SourceInput<'_>,
    queries: &crate::query::QueryEngine,
    resources: crate::runtime::RuntimeResources<'_>,
    mode: PrefixMode,
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
            #[cfg(any(test, feature = "test-support"))]
            semantics: semantics::CheckedSemantics::default(),
            hir: hir::Validated::freeze(hir::Module::default())
                .map_err(|error| vec![error])?
                .0,
            analysis: analysis::empty_world(),
            installed: functions::InstalledFunctions::default(),
            mono: mono::empty_world(),
            gir: gir::empty_world(),
            gir_stats: gir::pass::GirPassStats::default(),
            docs: docs::DocWorld::default(),
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
            embed,
        } => check_sources(
            source_map,
            entry,
            source_root,
            package_identity,
            require_main,
            cfg,
            external_packages,
            embed,
            queries,
            resources,
            mode,
        ),
    }
}

/// 内建源单元是否进入入口为 `entry` 的本次编译。
pub fn installs_builtin_source(logical_path: &str, entry: &str) -> bool {
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
    // 内建源单元分两段：物化表覆盖的源按 artifact manifest 顺序占据文件 ID 前缀，探针源这类
    // 未覆盖的源追加在物化块之后。组内保持 manifest 顺序（稳定排序），不能按路径重排：
    // 物化表里的位置以「物化源占前缀」为前提，顺序不能与构建期不同。
    snapshots.sort_by_key(|snapshot| is_probe_builtin_source(snapshot.logical_path()));
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
    embed: Option<&gugu_sema::semantics::comptime::embed::EmbedHost>,
    queries: &crate::query::QueryEngine,
    resources: crate::runtime::RuntimeResources<'_>,
    mode: PrefixMode,
) -> Result<FrontendOutput, Vec<Diagnostic>> {
    // 物化表按 target 派生：目标不一致时内建块的裁剪与物化位置都会错位，必须在这里失败。
    if resources.target_name() != cfg.target().name() {
        return Err(vec![Diagnostic::error(
            DiagnosticCode::BackendInvariant,
            format!(
                "runtime artifact 目标 `{}` 与本次编译目标 `{}` 不一致",
                resources.target_name(),
                cfg.target().name()
            ),
            None,
        )]);
    }
    let (builtin_paths, runtime_paths) = install_builtin_sources(source_map, resources, entry)?;
    // 物化表决定编号前缀：内建块的位置与顺序都按它安装，缺表或校验失败即 compiler invariant。
    // 解码要在源安装之后：表里的源码位置按当前源码表重建。
    let prefix = match mode {
        PrefixMode::Install => {
            let materialized_sources = resources
                .source_blobs()
                .iter()
                .filter(|blob| !is_probe_builtin_source(&blob.logical_path))
                .count();
            Some(prefix::BuiltinPrefix::decode(
                resources.prebuilt(),
                resources.target_name(),
                materialized_sources,
                source_map,
            )?)
        }
        #[cfg(all(any(test, feature = "test-support"), feature = "slow-tests"))]
        PrefixMode::Derive => None,
    };
    // 强制现算的测试路径没有物化表：用「非探针内建源」判定物化归属，模块位次与生产路径一致。
    let probe_modules = match mode {
        PrefixMode::Install => BTreeSet::new(),
        #[cfg(all(any(test, feature = "test-support"), feature = "slow-tests"))]
        PrefixMode::Derive => probe_module_paths(resources),
    };
    let mut modules = parse_modules(source_map, source_root, cfg, &builtin_paths)?;
    // 模块表定序：物化覆盖的内建模块（artifact 顺序）→ 未覆盖的内建模块 → 用户模块。
    modules.sort_by(|left, right| {
        (
            module_rank(
                left,
                prefix.as_ref().map(prefix::BuiltinPrefix::sema),
                &probe_modules,
            ),
            &left.path,
        )
            .cmp(&(
                module_rank(
                    right,
                    prefix.as_ref().map(prefix::BuiltinPrefix::sema),
                    &probe_modules,
                ),
                &right.path,
            ))
    });
    if let Some(prefix) = prefix.as_ref() {
        prefix.sema().verify_installed(&modules)?;
    }
    // 宏展开必须先于入口判定：`comptime source` 生成的 `fn main` 与源码 main 同等有效。
    let expansion_inputs = expand_modules(
        &mut modules,
        source_map,
        cfg,
        queries,
        package_identity,
        external_packages,
        prefix.as_ref().map(prefix::BuiltinPrefix::sema),
        embed,
    )?;
    // harness 域：收集测试/bench 与 doctest，生成 `__gugu_harness` 模块并把入口指向它。
    // doctest 模块按展开登记，其诊断回锚到文档围栏所在行。
    let harness_entry = harness::apply(&mut modules, source_map, cfg)
        .map_err(|errors| expand::reanchor_errors(errors, source_map))?;
    let entry_file = match harness_entry {
        Some(file) => file,
        None => source_map.file_id(entry).ok_or_else(|| {
            vec![Diagnostic::error(
                DiagnosticCode::ModuleNotFound,
                format!("target 入口 `{entry}` 未进入源码模块表"),
                None,
            )]
        })?,
    };
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
    let (names, derived) = derive_world(
        &modules,
        source_map,
        cfg,
        package_identity,
        external_packages,
        entry_function,
        queries,
        &runtime_paths,
        prefix.as_ref(),
        embed,
    )?;
    let mut lints = semantics::must_use_lints(&modules, &derived.checked, source_map)
        .map_err(|errors| expand::reanchor_errors(errors, source_map))?;
    lints.extend(
        gir::large_copy_lints(&modules, &derived.gir, source_map)
            .map_err(|errors| expand::reanchor_errors(errors, source_map))?,
    );
    lints.extend(docs::missing_docs_lints(&modules, source_map));
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
        derived.installed,
        derived.mono,
        derived.gir,
        derived.gir_stats,
        lints,
    ))
}

/// 展开 `comptime source` 宏，返回可重放的展开输入。
///
/// 入口判定必须看到宏生成的项，因此这一步与派生分开：调用方先展开、再定入口、最后派生。
#[expect(
    clippy::too_many_arguments,
    reason = "宏展开需要源码、cfg、query 与 package 上下文"
)]
fn expand_modules(
    modules: &mut [ParsedModule],
    source_map: &mut SourceMap,
    cfg: &cfg::CfgContext,
    queries: &crate::query::QueryEngine,
    package_identity: &str,
    external_packages: &BTreeSet<String>,
    prefix: Option<&sema_prefix::SemaPrefix>,
    embed: Option<&gugu_sema::semantics::comptime::embed::EmbedHost>,
) -> Result<expand::ExpansionInputs, Vec<Diagnostic>> {
    expand::run(
        modules,
        source_map,
        cfg,
        queries,
        package_identity,
        external_packages,
        prefix,
        embed,
    )
    .map_err(|errors| expand::reanchor_errors(errors, source_map))
}

/// 模块表定序：物化覆盖的内建模块 → 未覆盖的内建模块 → 用户模块。
///
/// 物化表里的模块下标以「物化模块占前缀」为前提；探针源这类未物化的内建模块追加在物化块
/// 之后，用户模块再追加在其后。`Install` 模式的物化归属由物化表给出，`Derive` 模式没有
/// 物化表，改由「非探针内建源」判定，两条路径的模块位次因此一致。
fn module_rank(
    module: &ParsedModule,
    prefix: Option<&sema_prefix::SemaPrefix>,
    probe_modules: &BTreeSet<String>,
) -> u8 {
    if !module.builtin {
        2
    } else if prefix.map_or_else(
        || !probe_modules.contains(&module.path),
        |prefix| prefix.covers_module(&module.path),
    ) {
        0
    } else {
        1
    }
}

/// 探针内建源对应的模块路径集合。
#[cfg(all(any(test, feature = "test-support"), feature = "slow-tests"))]
fn probe_module_paths(resources: crate::runtime::RuntimeResources<'_>) -> BTreeSet<String> {
    resources
        .source_blobs()
        .iter()
        .filter(|blob| is_probe_builtin_source(&blob.logical_path))
        .filter_map(|blob| logical_to_module_path(&blob.logical_path))
        .collect()
}

/// 名称解析与语义检查的公共段：已展开的模块 → 冻结 world。
///
/// 用户编译与构建期物化走同一段管线，因此内建块的编号与指纹在两条路径上同源。
#[expect(
    clippy::too_many_arguments,
    reason = "派生入口需要源码映射、cfg、入口与 query 引擎"
)]
fn derive_world(
    modules: &[ParsedModule],
    source_map: &mut SourceMap,
    cfg: &cfg::CfgContext,
    package_identity: &str,
    external_packages: &BTreeSet<String>,
    entry_function: Option<semantics::model::CallableId>,
    queries: &crate::query::QueryEngine,
    runtime_paths: &BTreeSet<String>,
    prefix: Option<&prefix::BuiltinPrefix<'_>>,
    embed: Option<&gugu_sema::semantics::comptime::embed::EmbedHost>,
) -> Result<(names::NameResolution, pipeline::DerivedWorld), Vec<Diagnostic>> {
    let sema_prefix = prefix.map(prefix::BuiltinPrefix::sema);
    let names =
        names::analyze_with_prefix(package_identity, external_packages, modules, sema_prefix)?;
    let sema = semantics::check(
        modules,
        &names,
        source_map,
        cfg,
        entry_function,
        queries,
        runtime_paths,
        sema_prefix,
        embed.cloned(),
    )
    .map_err(|errors| expand::reanchor_errors(errors, source_map))?;
    let derived = pipeline::derive(sema, source_map, cfg, queries, prefix)
        .map_err(|errors| expand::reanchor_errors(errors, source_map))?;
    Ok((names, derived))
}

/// 内建块派生结果：构建期物化的输入，每张表都是 canonical 编码的字节。
///
/// 编号前缀就是 artifact 的 source manifest 顺序；用户模块在用户编译里追加在它之后，
/// 因此这里的每个下标在用户编译中仍然有效。
pub struct BuiltinDerivation {
    pub module_order: Vec<String>,
    pub names: Vec<u8>,
    pub identities: Vec<u8>,
    pub checked: Vec<u8>,
    pub early_constants: Vec<u8>,
    pub hir: Vec<u8>,
    pub gir_bodies: Vec<Vec<u8>>,
    pub record_types: Vec<u8>,
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
pub fn derive_builtin(
    source_map: &mut SourceMap,
    queries: &crate::query::QueryEngine,
    resources: crate::runtime::RuntimeResources<'_>,
    cfg: &cfg::CfgContext,
) -> Result<BuiltinDerivation, Vec<Diagnostic>> {
    let (builtin_paths, runtime_paths) = install_builtin_sources(source_map, resources, "")?;
    let mut modules = parse_modules(source_map, "", cfg, &builtin_paths)?;
    modules.sort_by(|left, right| left.path.cmp(&right.path));
    // 内建块没有入口判定，但展开必须发生：物化表要在与用户编译相同的展开后世界上形成。
    expand_modules(
        &mut modules,
        source_map,
        cfg,
        queries,
        BUILTIN_PACKAGE_IDENTITY,
        &BTreeSet::new(),
        None,
        None,
    )?;
    let module_order = modules.iter().map(|module| module.path.clone()).collect();
    let (names, derived) = derive_world(
        &modules,
        source_map,
        cfg,
        BUILTIN_PACKAGE_IDENTITY,
        &BTreeSet::new(),
        None,
        queries,
        &runtime_paths,
        None,
        None,
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
    installed: functions::InstalledFunctions,
    mono: mono::MonoWorldV1,
    gir: gir::GirWorldV1,
    gir_stats: gir::pass::GirPassStats,
    lints: Vec<Diagnostic>,
) -> FrontendOutput {
    #[cfg(not(any(test, feature = "test-support")))]
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
        docs: docs::world(&modules, source_map),
        modules,
        names,
        types,
        comptime_registry,
        expansion_inputs,
        #[cfg(any(test, feature = "test-support"))]
        semantics,
        hir,
        analysis,
        installed,
        mono,
        gir,
        gir_stats,
        lints,
    }
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

#[cfg(all(test, feature = "slow-tests"))]
mod stage10_tests;
