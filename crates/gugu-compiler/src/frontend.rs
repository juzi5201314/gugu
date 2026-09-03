use std::path::PathBuf;

use crate::{
    diagnostics::{Diagnostic, DiagnosticCode},
    source::{ExpansionId, SourceMap, SourceSnapshot},
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
    let file = source_map
        .file_id(snapshot.logical_path())
        .expect("frontend snapshot is registered");
    if source.len() > u32::MAX as usize {
        return Err(Diagnostic::error(
            DiagnosticCode::MalformedSource,
            "bootstrap 源文件超过 u32 字节范围",
            Some(
                source_map
                    .span(file, 0, 0, ExpansionId::ROOT)
                    .expect("valid empty span"),
            ),
        ));
    }
    if let Some(offset) = source.as_bytes().iter().position(|byte| *byte == 0) {
        return Err(Diagnostic::error(
            DiagnosticCode::MalformedSource,
            "源文件不能包含 NUL 字节",
            Some(
                source_map
                    .span(file, offset, offset + 1, ExpansionId::ROOT)
                    .expect("NUL byte is within snapshot"),
            ),
        ));
    }

    let Some(main_offset) = find_main_declaration(source) else {
        return Err(Diagnostic::error(
            DiagnosticCode::MissingMain,
            "单文件入口必须包含 `fn main() { ... }`",
            Some(
                source_map
                    .span(file, 0, source.len().min(1), ExpansionId::ROOT)
                    .expect("valid missing-main span"),
            ),
        ));
    };
    let declaration_end = main_offset + "fn main()".len();
    let body_start = skip_whitespace(source, declaration_end);
    if source.as_bytes().get(body_start) != Some(&b'{') {
        return Err(Diagnostic::error(
            DiagnosticCode::MalformedSource,
            "`fn main()` 后必须是函数体",
            Some(
                source_map
                    .span(file, main_offset, declaration_end, ExpansionId::ROOT)
                    .expect("valid main declaration span"),
            ),
        ));
    }
    if find_matching_brace(source, body_start).is_none() {
        return Err(Diagnostic::error(
            DiagnosticCode::MalformedSource,
            "main 函数体的花括号不匹配",
            Some(
                source_map
                    .span(file, body_start, body_start + 1, ExpansionId::ROOT)
                    .expect("opening brace is within snapshot"),
            ),
        ));
    }

    Ok(FrontendOutput {
        path: Some(snapshot.path().to_path_buf()),
        has_main: true,
        source_len: source.len() as u32,
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
