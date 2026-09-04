use std::path::PathBuf;

use crate::{
    diagnostics::{Diagnostic, DiagnosticCode},
    source::{ExpansionId, SourceFileId, SourceMap, SourceSnapshot, Span},
};

mod ast;
mod attr;
mod intern;
mod lex;
mod parse;
mod string;
mod token;

use ast::{AstArena, AstFile};
pub(crate) use lex::lex;
#[cfg(test)]
pub(crate) use parse::{dump_ast, has_main_fn, parent_before_child, parse};
#[cfg(not(test))]
pub(crate) use parse::{has_main_fn, parse};
pub(crate) use token::TokenBuffer;

pub(crate) enum SourceInput<'a> {
    EmptyPackage,
    File {
        snapshot: &'a SourceSnapshot,
        source_map: &'a SourceMap,
        require_main: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct FrontendOutput {
    pub(crate) path: Option<PathBuf>,
    pub(crate) has_main: bool,
    pub(crate) source_len: u32,
    pub(crate) tokens: TokenBuffer,
    pub(crate) ast: Option<ParsedAst>,
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedAst {
    pub(crate) file: AstFile,
    pub(crate) arena: AstArena,
}

pub(crate) fn bootstrap(input: SourceInput<'_>) -> Result<FrontendOutput, Vec<Diagnostic>> {
    match input {
        SourceInput::EmptyPackage => Ok(FrontendOutput {
            path: None,
            has_main: false,
            source_len: 0,
            tokens: TokenBuffer::default(),
            ast: None,
        }),
        SourceInput::File {
            snapshot,
            source_map,
            require_main,
        } => check_file(snapshot, source_map, require_main),
    }
}

fn check_file(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    require_main: bool,
) -> Result<FrontendOutput, Vec<Diagnostic>> {
    let file = source_map.file_id(snapshot.logical_path()).ok_or_else(|| {
        vec![Diagnostic::error(
            DiagnosticCode::MalformedSource,
            format!(
                "前端快照逻辑路径 `{}` 未在当前源码表登记",
                snapshot.logical_path()
            ),
            None,
        )]
    })?;
    let lexed = lex(snapshot, source_map, file);
    if !lexed.diagnostics.is_empty() || lexed.buffer.has_error_tokens() {
        let mut diagnostics = lexed.diagnostics;
        if diagnostics.is_empty() {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::LexInvalidToken,
                "词法分析产生了错误记号",
                None,
            ));
        }
        return Err(diagnostics);
    }
    let parsed = parse(snapshot.content(), source_map, file, &lexed.buffer);
    if !parsed.diagnostics.is_empty() {
        return Err(parsed.diagnostics);
    }
    let has_main = has_main_fn(&parsed.file, &parsed.arena, &lexed.buffer.intern);
    if require_main && !has_main {
        let span = source_map_span(source_map, file, 0, snapshot.content().len().min(1))?;
        return Err(vec![Diagnostic::error(
            DiagnosticCode::MissingMain,
            "单文件入口必须包含 `fn main() { ... }` 或 `fn main() = ...`",
            Some(span),
        )]);
    }
    Ok(FrontendOutput {
        path: Some(snapshot.path().to_path_buf()),
        has_main,
        source_len: snapshot.content().len() as u32,
        tokens: lexed.buffer,
        ast: Some(ParsedAst {
            file: parsed.file,
            arena: parsed.arena,
        }),
    })
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

#[cfg(test)]
mod lex_tests;
#[cfg(test)]
mod parse_tests;
