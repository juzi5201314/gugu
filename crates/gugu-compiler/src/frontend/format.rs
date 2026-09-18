use std::path::Path;

use super::lex;
use super::parse;
use super::token::{Token, TokenKind, Trivia, TriviaKind};
use crate::diagnostics::Diagnostic;
use crate::source::{SourceMap, SourceSnapshot};

/// `format_source` 失败：源码级诊断或快照级错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FormatError {
    /// 词法或语法诊断；附带源码表，使调用方能按同一渲染器打印片段。
    Diagnostic {
        /// 首个失败诊断。
        diagnostic: Diagnostic,
        /// 本次格式化的源码表。
        sources: Box<SourceMap>,
    },
    /// 快照或源码表构建失败：非法逻辑路径、BOM、非法 UTF-8 或路径重复。
    Source(String),
}

impl FormatError {
    /// 紧凑单行消息；用于 JSON 与 `json-diagnostic-short` 通道。
    pub fn message(&self) -> String {
        match self {
            Self::Diagnostic { diagnostic, .. } => diagnostic.render_text(),
            Self::Source(message) => message.clone(),
        }
    }

    /// 人读文本：诊断渲染为多行片段块，快照错误为单行 `error: ` 消息。
    pub fn render_human(&self, color: bool) -> String {
        match self {
            Self::Diagnostic {
                diagnostic,
                sources,
            } => diagnostic.render_human(sources.as_ref(), color),
            Self::Source(message) => format!("error: {message}"),
        }
    }
}

/// 格式化一个已通过词法和语法检查的 Gugu 源文件。
///
/// 记号和注释正文均从原始源码切片，字符串、字节串与汇编不经过解释。
pub fn format_source(path: impl AsRef<Path>, source: &str) -> Result<String, FormatError> {
    let snapshot = SourceSnapshot::from_str(path, source)
        .map_err(|error| FormatError::Source(error.to_string()))?;
    let map = SourceMap::new(vec![snapshot.clone()])
        .map_err(|error| FormatError::Source(error.to_string()))?;
    let file = map
        .file_id(snapshot.logical_path())
        .expect("源码表已登记文件");
    let lexed = lex(&snapshot, &map, file);
    if let Some(error) = lexed.diagnostics.first() {
        return Err(FormatError::Diagnostic {
            diagnostic: error.clone(),
            sources: Box::new(map),
        });
    }
    let mut buffer = lexed.buffer;
    let parsed = parse(snapshot.content(), &map, file, &mut buffer);
    if let Some(error) = parsed.diagnostics.first() {
        return Err(FormatError::Diagnostic {
            diagnostic: error.clone(),
            sources: Box::new(map),
        });
    }
    Ok(render(snapshot.content(), &buffer.tokens, &buffer.trivia))
}

fn render(source: &str, tokens: &[Token], trivia: &[Trivia]) -> String {
    let mut output = String::with_capacity(source.len() + 16);
    let mut indent = 0usize;
    let mut line_start = true;
    let mut previous = None;
    for token in tokens.iter().filter(|token| token.kind != TokenKind::Eof) {
        let start = usize::try_from(token.trivia_start).expect("trivia 下标可表示");
        let end = start + usize::try_from(token.trivia_len).expect("trivia 长度可表示");
        for item in &trivia[start..end] {
            let text = &source[usize::try_from(item.start).expect("偏移可表示")
                ..usize::try_from(item.end).expect("偏移可表示")];
            match item.kind {
                TriviaKind::LineComment | TriviaKind::DocComment | TriviaKind::InnerDocComment => {
                    if !line_start {
                        output.push(' ');
                    }
                    write_indent(&mut output, indent, line_start);
                    output.push_str(text);
                    output.push('\n');
                    line_start = true;
                }
                TriviaKind::BlockComment => {
                    if !line_start {
                        output.push(' ');
                    }
                    write_indent(&mut output, indent, line_start);
                    output.push_str(text);
                    line_start = false;
                }
                TriviaKind::Newline => {
                    if !line_start {
                        output.push('\n');
                        line_start = true;
                    }
                }
                TriviaKind::Whitespace => {}
            }
        }
        if token.kind == TokenKind::RBrace {
            indent = indent.saturating_sub(1);
            if !line_start {
                output.push('\n');
                line_start = true;
            }
        }
        if !line_start && needs_space(previous, token.kind) {
            output.push(' ');
        }
        write_indent(&mut output, indent, line_start);
        output.push_str(token.text(source));
        line_start = false;
        match token.kind {
            TokenKind::LBrace => {
                indent += 1;
                output.push('\n');
                line_start = true;
            }
            TokenKind::Semi => {
                output.push('\n');
                line_start = true;
            }
            TokenKind::Comma => output.push(' '),
            _ => {}
        }
        previous = Some(token.kind);
    }
    while output.ends_with([' ', '\n']) {
        output.pop();
    }
    output.push('\n');
    output
}

fn write_indent(output: &mut String, indent: usize, line_start: bool) {
    if line_start {
        output.push_str(&"    ".repeat(indent));
    }
}

fn needs_space(previous: Option<TokenKind>, current: TokenKind) -> bool {
    match (previous, current) {
        (
            Some(TokenKind::LParen | TokenKind::LBracket | TokenKind::Dot | TokenKind::PathSep),
            _,
        ) => false,
        (
            _,
            TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::RBrace
            | TokenKind::Comma
            | TokenKind::Dot
            | TokenKind::PathSep
            | TokenKind::Semi,
        ) => false,
        (_, TokenKind::LParen | TokenKind::LBracket) => false,
        (Some(TokenKind::Comma), _) => true,
        _ => true,
    }
}
