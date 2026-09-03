use std::path::PathBuf;

use crate::{
    diagnostics::{Diagnostic, DiagnosticCode},
    source::{ExpansionId, SourceFileId, SourceMap, SourceSnapshot, Span},
};

pub(crate) enum SourceInput<'a> {
    EmptyPackage,
    SingleFile {
        snapshot: &'a SourceSnapshot,
        source_map: &'a SourceMap,
    },
    LibraryFile {
        snapshot: &'a SourceSnapshot,
    },
}
#[derive(Clone, Debug)]
pub(crate) struct FrontendOutput {
    pub(crate) path: Option<PathBuf>,
    pub(crate) has_main: bool,
    pub(crate) source_len: u32,
}

pub(crate) fn bootstrap(input: SourceInput<'_>) -> Result<FrontendOutput, Diagnostic> {
    match input {
        SourceInput::EmptyPackage => Ok(FrontendOutput {
            path: None,
            has_main: false,
            source_len: 0,
        }),
        SourceInput::SingleFile {
            snapshot,
            source_map,
        } => check_single_file(snapshot, source_map),
        SourceInput::LibraryFile { snapshot } => Ok(FrontendOutput {
            path: Some(snapshot.path().to_path_buf()),
            has_main: false,
            source_len: snapshot.content().len() as u32,
        }),
    }
}

fn check_single_file(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
) -> Result<FrontendOutput, Diagnostic> {
    let source = snapshot.content();
    // 快照在 load-sources 阶段已保证 u32 上限与路径唯一；这里只做入口结构检查。
    let file = source_map.file_id(snapshot.logical_path()).ok_or_else(|| {
        Diagnostic::error(
            DiagnosticCode::MalformedSource,
            format!(
                "前端快照逻辑路径 `{}` 未在当前源码表登记",
                snapshot.logical_path()
            ),
            None,
        )
    })?;
    let span = |start: usize, end: usize| source_map_span(source_map, file, start, end);

    if let Some(offset) = source.as_bytes().iter().position(|byte| *byte == 0) {
        return Err(Diagnostic::error(
            DiagnosticCode::MalformedSource,
            "源文件不能包含 NUL 字节",
            Some(span(offset, offset + 1)?),
        ));
    }

    let Some(main_offset) = find_main_declaration(source) else {
        return Err(Diagnostic::error(
            DiagnosticCode::MissingMain,
            "单文件入口必须包含 `fn main() { ... }`",
            Some(span(0, source.len().min(1))?),
        ));
    };
    let declaration_end = main_offset + "fn main()".len();
    let body_start = skip_whitespace(source, declaration_end);
    if source.as_bytes().get(body_start) != Some(&b'{') {
        return Err(Diagnostic::error(
            DiagnosticCode::MalformedSource,
            "`fn main()` 后必须是函数体",
            Some(span(main_offset, declaration_end)?),
        ));
    }
    if find_matching_brace(source, body_start).is_none() {
        return Err(Diagnostic::error(
            DiagnosticCode::MalformedSource,
            "main 函数体的花括号不匹配",
            Some(span(body_start, body_start + 1)?),
        ));
    }

    Ok(FrontendOutput {
        path: Some(snapshot.path().to_path_buf()),
        has_main: true,
        source_len: source.len() as u32,
    })
}

fn source_map_span(
    source_map: &SourceMap,
    file: SourceFileId,
    start: usize,
    end: usize,
) -> Result<Span, Diagnostic> {
    source_map
        .span(file, start, end, ExpansionId::ROOT)
        .map_err(|error| {
            Diagnostic::error(DiagnosticCode::SpanOutOfBounds, error.to_string(), None)
        })
}

fn find_main_declaration(source: &str) -> Option<usize> {
    let pattern = "fn main()";
    source.match_indices(pattern).find_map(|(offset, _)| {
        let previous_is_identifier = source[..offset]
            .chars()
            .next_back()
            .is_some_and(is_identifier_byte);
        let after = offset + pattern.len();
        let next_is_boundary = source
            .as_bytes()
            .get(after)
            .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'{');
        (!previous_is_identifier && next_is_boundary).then_some(offset)
    })
}

fn skip_whitespace(source: &str, mut offset: usize) -> usize {
    while source
        .as_bytes()
        .get(offset)
        .is_some_and(u8::is_ascii_whitespace)
    {
        offset += 1;
    }
    offset
}

fn find_matching_brace(source: &str, opening: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0_u32;
    let mut offset = opening;
    let mut in_string = false;
    let mut escaped = false;
    while let Some(byte) = bytes.get(offset).copied() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            offset += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
        } else if byte == b'{' {
            depth += 1;
        } else if byte == b'}' {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(offset);
            }
        }
        offset += 1;
    }
    None
}

fn is_identifier_byte(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}
