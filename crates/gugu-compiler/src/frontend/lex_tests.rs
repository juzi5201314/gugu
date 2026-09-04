use crate::{
    diagnostics::DiagnosticCode,
    source::{SourceMap, SourceSnapshot},
};

use super::lex::lex;
use super::token::TokenKind;

fn lex_source(source: &str) -> (Vec<TokenKind>, Vec<DiagnosticCode>) {
    let snapshot = SourceSnapshot::from_str("lex.gg", source).expect("utf-8 fixture");
    let map = SourceMap::new(vec![snapshot.clone()]).expect("unique path");
    let file = map.file_id("lex.gg").expect("registered");
    let lexed = lex(&snapshot, &map, file);
    let kinds = lexed.buffer.tokens.iter().map(|token| token.kind).collect();
    let codes = lexed
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code())
        .collect();
    (kinds, codes)
}

fn kinds_without_eof(source: &str) -> Vec<TokenKind> {
    let (mut kinds, diagnostics) = lex_source(source);
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics for `{source}`: {diagnostics:?}"
    );
    assert_eq!(kinds.pop(), Some(TokenKind::Eof));
    kinds
}

#[test]
fn longest_match_punctuation() {
    assert_eq!(
        kinds_without_eof("&& || .. :: != += <<= >>= => ..."),
        [
            TokenKind::AndAnd,
            TokenKind::OrOr,
            TokenKind::DotDot,
            TokenKind::PathSep,
            TokenKind::Ne,
            TokenKind::PlusEq,
            TokenKind::ShlEq,
            TokenKind::ShrEq,
            TokenKind::FatArrow,
            TokenKind::DotDotDot,
        ]
    );
}

#[test]
fn whitespace_splits_operators() {
    assert_eq!(
        kinds_without_eof("& & | |"),
        [TokenKind::And, TokenKind::And, TokenKind::Or, TokenKind::Or,]
    );
    assert_eq!(
        kinds_without_eof("&&x"),
        [TokenKind::AndAnd, TokenKind::Ident]
    );
}

#[test]
fn keywords_and_contextual_idents() {
    assert_eq!(kinds_without_eof("fn"), [TokenKind::KwFn]);
    assert_eq!(kinds_without_eof("source"), [TokenKind::Ident]);
    assert_eq!(
        kinds_without_eof("self Self"),
        [TokenKind::Ident, TokenKind::Ident]
    );
}

#[test]
fn integers_and_floats() {
    assert_eq!(
        kinds_without_eof("42 0xFF 0b1010 0o755 1_000"),
        [
            TokenKind::Int,
            TokenKind::Int,
            TokenKind::Int,
            TokenKind::Int,
            TokenKind::Int,
        ]
    );
    assert_eq!(
        kinds_without_eof("0.0 3.14 1e-9 5.0"),
        [
            TokenKind::Float,
            TokenKind::Float,
            TokenKind::Float,
            TokenKind::Float,
        ]
    );
    assert_eq!(kinds_without_eof(".5"), [TokenKind::Dot, TokenKind::Int]);
    assert_eq!(kinds_without_eof("5."), [TokenKind::Int, TokenKind::Dot]);
}

#[test]
fn invalid_numbers() {
    let (_, codes) = lex_source("08");
    assert_eq!(codes, [DiagnosticCode::LexInvalidNumeric]);
    let (_, codes) = lex_source("1foo");
    assert_eq!(codes, [DiagnosticCode::LexInvalidNumeric]);
    let (_, codes) = lex_source("0x_1");
    assert_eq!(codes, [DiagnosticCode::LexInvalidNumeric]);
}

#[test]
fn strings_chars_and_raw() {
    assert_eq!(
        kinds_without_eof(r#""hi" 'A' b"xy" b'x' c"ok" raw"a" raw"""a"b""""#),
        [
            TokenKind::String,
            TokenKind::Char,
            TokenKind::ByteString,
            TokenKind::ByteChar,
            TokenKind::CString,
            TokenKind::RawString,
            TokenKind::RawString,
        ]
    );
}

#[test]
fn nested_block_comment_is_trivia() {
    let snapshot =
        SourceSnapshot::from_str("lex.gg", "a /* outer /* inner */ still */ b").expect("utf-8");
    let map = SourceMap::new(vec![snapshot.clone()]).expect("unique");
    let file = map.file_id("lex.gg").expect("file");
    let lexed = lex(&snapshot, &map, file);
    assert!(lexed.diagnostics.is_empty());
    let ident_b = &lexed.buffer.tokens[1];
    assert_eq!(ident_b.kind, TokenKind::Ident);
    assert!(
        lexed
            .buffer
            .leading_trivia(ident_b)
            .iter()
            .any(|trivia| trivia.kind == super::token::TriviaKind::BlockComment)
    );
}

#[test]
fn line_continue_trivia_keeps_newline() {
    let snapshot = SourceSnapshot::from_str("lex.gg", "let y =\n    xs").expect("utf-8");
    let map = SourceMap::new(vec![snapshot.clone()]).expect("unique");
    let file = map.file_id("lex.gg").expect("file");
    let lexed = lex(&snapshot, &map, file);
    let eq = lexed
        .buffer
        .tokens
        .iter()
        .find(|token| token.kind == TokenKind::Eq)
        .unwrap();
    assert!(eq.kind.continues_line());
    let xs = lexed
        .buffer
        .tokens
        .iter()
        .find(|token| token.kind == TokenKind::Ident && token.text(snapshot.content()) == "xs")
        .unwrap();
    assert!(
        lexed
            .buffer
            .leading_trivia(xs)
            .iter()
            .any(|trivia| trivia.kind == super::token::TriviaKind::Newline)
    );
}

#[test]
fn fstring_interpolation_tokens() {
    assert_eq!(
        kinds_without_eof(r#"f"hello {name}!""#),
        [
            TokenKind::FStringStart,
            TokenKind::FStringText,
            TokenKind::FStringInterpOpen,
            TokenKind::Ident,
            TokenKind::FStringInterpClose,
            TokenKind::FStringText,
            TokenKind::FStringEnd,
        ]
    );
    assert_eq!(
        kinds_without_eof(r#"f"{id:08x}""#),
        [
            TokenKind::FStringStart,
            TokenKind::FStringInterpOpen,
            TokenKind::Ident,
            TokenKind::FormatSpec,
            TokenKind::FStringInterpClose,
            TokenKind::FStringEnd,
        ]
    );
}

#[test]
fn nested_fstring_is_lex_error() {
    let (_, codes) = lex_source(r#"f"{f"x"}""#);
    assert!(codes.contains(&DiagnosticCode::LexInvalidToken));
}

#[test]
fn unknown_attribute_and_cfg_tokens() {
    let (_, codes) = lex_source("#[unknown] fn f() {}");
    assert_eq!(codes, [DiagnosticCode::LexUnknownAttribute]);
    assert_eq!(
        kinds_without_eof(r#"#[cfg(os = "linux")]"#),
        [
            TokenKind::Hash,
            TokenKind::LBracket,
            TokenKind::Ident,
            TokenKind::LParen,
            TokenKind::Ident,
            TokenKind::Eq,
            TokenKind::String,
            TokenKind::RParen,
            TokenKind::RBracket,
        ]
    );
    let (_, codes) = lex_source("#[repr(foo)]");
    assert_eq!(codes, [DiagnosticCode::LexInvalidAttributeArg]);
    let (_, codes) = lex_source("#[derive(Debug)]");
    assert_eq!(codes, [DiagnosticCode::LexInvalidAttributeArg]);
    let (_, codes) = lex_source("#[cfg(1)]");
    assert_eq!(codes, [DiagnosticCode::LexInvalidAttributeArg]);
}

#[test]
fn invalid_escapes_and_c_string_nul() {
    let (_, codes) = lex_source(r#""\q""#);
    assert_eq!(codes, [DiagnosticCode::LexInvalidEscape]);
    let (_, codes) = lex_source(r#"'\u{D800}'"#);
    assert_eq!(codes, [DiagnosticCode::LexInvalidUnicodeScalar]);
    let (_, codes) = lex_source("c\"a\\0b\"");
    assert_eq!(codes, [DiagnosticCode::LexCStringNul]);
    let (_, codes) = lex_source("b'\\u{1F600}'");
    assert_eq!(codes, [DiagnosticCode::LexInvalidByteChar]);
}

#[test]
fn fixture_literals_and_attributes() {
    let source = include_str!("fixtures/literals.gg");
    let (kinds, codes) = lex_source(source);
    assert!(codes.is_empty(), "{codes:?}");
    assert!(kinds.contains(&TokenKind::Float));
    assert!(kinds.contains(&TokenKind::RawString));
    let source = include_str!("fixtures/attributes.gg");
    let (_, codes) = lex_source(source);
    assert!(codes.is_empty(), "{codes:?}");
}

#[test]
fn lex_errors_never_reach_image_plan() {
    use crate::{CompileRequest, Compiler, TargetName};
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "broken.gg",
        r#""unterminated"#,
        TargetName::X86_64Linux,
    ));
    assert!(!compilation.is_success());
    assert!(compilation.image_plan().is_none());
    assert_eq!(
        compilation.diagnostics().items()[0].code(),
        DiagnosticCode::LexUnterminated
    );
}
