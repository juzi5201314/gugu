//! 源码表 → 模块表：按逻辑路径定模块路径，逐模块词法、语法与 cfg 配置。

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Diagnostic, DiagnosticCode, ExpansionId, ParsedModule, SourceFileId, SourceMap, Span, cfg, lex,
    parse, source::SourceSnapshot,
};

/// 解析源码表中的全部模块；任一模块失败时汇总全部诊断返回。
pub fn parse_modules(
    source_map: &SourceMap,
    source_root: &str,
    cfg: &cfg::CfgContext,
    builtin_paths: &BTreeSet<String>,
) -> Result<Vec<ParsedModule>, Vec<Diagnostic>> {
    let mut modules = Vec::with_capacity(source_map.snapshots().len());
    let mut diagnostics = Vec::new();
    let mut occupied = BTreeMap::new();
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

/// 把内建逻辑路径映射成模块路径：去掉扩展名并把分隔符换成 `.`。
///
/// 内建逻辑路径已经是 package 相对路径（例如 `std/runtime/platform.gg`），不受用户
/// `source_root` 约束。
pub fn logical_to_module_path(logical: &str) -> Option<String> {
    logical
        .strip_suffix(".gg")
        .map(|path| path.replace('/', "."))
}

/// 返回内建源单元对应的模块路径。
fn builtin_module_path(
    snapshot: &SourceSnapshot,
    builtin_paths: &BTreeSet<String>,
) -> Option<String> {
    let logical = snapshot.logical_path();
    if !builtin_paths.contains(logical) {
        return None;
    }
    logical_to_module_path(logical)
}

/// 解析单个模块；harness manifest 等生成源码也走同一词法/语法/cfg 管线。
pub fn parse_module(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    file: SourceFileId,
    path: String,
    cfg: &cfg::CfgContext,
    builtin: bool,
) -> Result<ParsedModule, Vec<Diagnostic>> {
    parse_module_in_expansion(
        snapshot,
        source_map,
        file,
        ExpansionId::ROOT,
        path,
        cfg,
        builtin,
    )
}

/// 在指定源码宏展开上下文中解析模块；生成模块的 span 归到该展开记录，
/// 诊断可经 `reanchor_errors` 回锚到生成调用点。
pub fn parse_module_in_expansion(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    file: SourceFileId,
    expansion: ExpansionId,
    path: String,
    cfg: &cfg::CfgContext,
    builtin: bool,
) -> Result<ParsedModule, Vec<Diagnostic>> {
    let lexed = lex::lex_in_expansion(snapshot, source_map, file, expansion);
    if !lexed.diagnostics.is_empty() || lexed.buffer.has_error_tokens() {
        return Err(lexed.diagnostics);
    }
    let mut tokens = lexed.buffer;
    let parsed =
        parse::parse_in_expansion(snapshot.content(), source_map, file, expansion, &mut tokens);
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

/// 在源码表上取 span，并把 `SpanError` 映射为 `SpanOutOfBounds` 诊断。
///
/// 诊断契约闸门直接调用本函数验证 E0007 的归属；公开编译输入无法触达该分支，
/// 因为现有调用点都请求已注册文件的 `(0, 0)`。
pub fn source_map_span(
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
