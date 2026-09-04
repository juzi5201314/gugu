use std::path::PathBuf;

use crate::{
    diagnostics::{Diagnostic, DiagnosticCode},
    source::{ExpansionId, SourceFileId, SourceMap, SourceSnapshot, Span},
};

mod attr;
mod intern;
mod lex;
mod string;
mod token;

pub(crate) use lex::lex;
pub(crate) use token::{TokenBuffer, TokenKind};

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
}

pub(crate) fn bootstrap(input: SourceInput<'_>) -> Result<FrontendOutput, Vec<Diagnostic>> {
    match input {
        SourceInput::EmptyPackage => Ok(FrontendOutput {
            path: None,
            has_main: false,
            source_len: 0,
            tokens: TokenBuffer::default(),
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
    let has_main = find_main(&lexed.buffer);
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

fn find_main(buffer: &TokenBuffer) -> bool {
    let tokens = &buffer.tokens;
    let mut index = 0;
    while index + 3 < tokens.len() {
        if tokens[index].kind == TokenKind::KwFn
            && tokens[index + 1].kind == TokenKind::Ident
            && tokens[index + 1]
                .symbol
                .is_some_and(|symbol| buffer.intern.get_str(symbol) == "main")
            && tokens[index + 2].kind == TokenKind::LParen
        {
            if let Some(close) = matching_paren(tokens, index + 2)
                && matches!(
                    tokens.get(close + 1).map(|token| token.kind),
                    Some(TokenKind::LBrace | TokenKind::Eq)
                )
            {
                return true;
            }
            return false;
        }
        index += 1;
    }
    false
}

fn matching_paren(tokens: &[token::Token], open: usize) -> Option<usize> {
    let mut depth = 0_u32;
    for (offset, token) in tokens[open..].iter().enumerate() {
        match token.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            TokenKind::Eof => return None,
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod lex_tests;
