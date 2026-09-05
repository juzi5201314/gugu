use std::{collections::BTreeSet, path::PathBuf};

use crate::{
    diagnostics::{Diagnostic, DiagnosticCode},
    source::{ExpansionId, SourceFileId, SourceMap, SourceSnapshot, Span},
};

mod ast;
mod attr;
pub(crate) mod cfg;
pub(crate) mod format;
mod intern;
mod lex;
mod names;
mod parse;
mod semantics;
mod string;
mod token;
mod types;

use ast::{AstArena, AstFile, ItemKind};
pub(crate) use lex::lex;
#[cfg(not(test))]
pub(crate) use parse::parse;
#[cfg(test)]
pub(crate) use parse::{dump_ast, has_main_fn, parent_before_child, parse};
pub(crate) use semantics::CheckedSemantics;
pub(crate) use token::TokenBuffer;

pub(crate) enum SourceInput<'a> {
    EmptyPackage,
    Sources {
        source_map: &'a SourceMap,
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
    pub(crate) has_main: bool,
    pub(crate) source_len: u64,
    pub(crate) token_count: usize,
    pub(crate) item_count: usize,
    pub(crate) node_count: u64,
    pub(crate) modules: Vec<ParsedModule>,
    pub(crate) names: names::NameResolution,
    pub(crate) types: Vec<types::Layout>,
    pub(crate) semantics: semantics::CheckedSemantics,
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedModule {
    pub(crate) path: String,
    pub(crate) file: AstFile,
    pub(crate) arena: AstArena,
    pub(crate) tokens: TokenBuffer,
    pub(crate) configured: cfg::ConfiguredAst,
}

pub(crate) fn bootstrap(
    input: SourceInput<'_>,
    queries: &crate::query::QueryEngine,
) -> Result<FrontendOutput, Vec<Diagnostic>> {
    match input {
        SourceInput::EmptyPackage => Ok(FrontendOutput {
            path: None,
            has_main: false,
            source_len: 0,
            token_count: 0,
            item_count: 0,
            node_count: 0,
            modules: Vec::new(),
            names: names::NameResolution::default(),
            types: Vec::new(),
            semantics: semantics::CheckedSemantics::default(),
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
        ),
    }
}

fn check_sources(
    source_map: &SourceMap,
    entry: &str,
    source_root: &str,
    package_identity: &str,
    require_main: bool,
    cfg: &cfg::CfgContext,
    external_packages: &BTreeSet<String>,
    queries: &crate::query::QueryEngine,
) -> Result<FrontendOutput, Vec<Diagnostic>> {
    let mut modules = parse_modules(source_map, source_root, cfg)?;
    modules.sort_by(|left, right| left.path.cmp(&right.path));
    let entry_file = source_map.file_id(entry).ok_or_else(|| {
        vec![Diagnostic::error(
            DiagnosticCode::ModuleNotFound,
            format!("target 入口 `{entry}` 未进入源码模块表"),
            None,
        )]
    })?;
    let entry_module = modules
        .iter()
        .find(|module| module.file.source == entry_file);
    let has_main = entry_module.is_some_and(has_active_main);
    if require_main && !has_main {
        let span = source_map_span(source_map, entry_file, 0, 0)?;
        return Err(vec![Diagnostic::error(
            DiagnosticCode::MissingMain,
            "target 入口在 cfg 裁项后必须包含 `fn main() { ... }` 或 `fn main() = ...`",
            Some(span),
        )]);
    }
    let names = names::analyze(package_identity, external_packages, &modules)?;
    let (semantics, types) = semantics::check(&modules, &names, source_map, cfg, queries)?;
    let mut output = frontend_output(entry, has_main, source_map, modules, names, types);
    output.semantics = semantics;
    Ok(output)
}

fn parse_modules(
    source_map: &SourceMap,
    source_root: &str,
    cfg: &cfg::CfgContext,
) -> Result<Vec<ParsedModule>, Vec<Diagnostic>> {
    let mut modules = Vec::with_capacity(source_map.snapshots().len());
    let mut diagnostics = Vec::new();
    let mut occupied = std::collections::BTreeMap::new();
    for snapshot in source_map.snapshots() {
        let file = source_map
            .file_id(snapshot.logical_path())
            .expect("源码表快照必须有稳定文件 ID");
        let path = match module_path(snapshot, source_root, source_map, file) {
            Ok(path) => path,
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                continue;
            }
        };
        if let Some(previous) = occupied.insert(path.clone(), snapshot.logical_path()) {
            diagnostics.push(module_path_conflict(
                snapshot, source_map, file, &path, previous,
            ));
            continue;
        }
        match parse_module(snapshot, source_map, file, path, cfg) {
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

fn parse_module(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    file: SourceFileId,
    path: String,
    cfg: &cfg::CfgContext,
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

fn frontend_output(
    entry: &str,
    has_main: bool,
    source_map: &SourceMap,
    modules: Vec<ParsedModule>,
    names: names::NameResolution,
    types: Vec<types::Layout>,
) -> FrontendOutput {
    FrontendOutput {
        path: Some(PathBuf::from(entry)),
        has_main,
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
        semantics: semantics::CheckedSemantics::default(),
    }
}

fn source_map_span(
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

fn has_active_main(module: &ParsedModule) -> bool {
    module
        .file
        .items
        .as_slice(&module.arena.item_ids)
        .iter()
        .copied()
        .filter(|&item| module.configured.item_active(item))
        .any(|item| match module.arena.items[item.0 as usize].kind {
            ItemKind::Function(function) => {
                let declaration = &module.arena.fns[function.0 as usize];
                declaration
                    .name
                    .is_some_and(|name| module.tokens.intern.get_str(name) == "main")
                    && matches!(declaration.body, ast::FnBody::Block(_) | ast::FnBody::Eq(_))
            }
            _ => false,
        })
}

#[cfg(test)]
mod lex_tests;
#[cfg(test)]
mod parse_tests;
#[cfg(test)]
mod stage10_tests;
