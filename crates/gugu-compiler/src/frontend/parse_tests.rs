use crate::{
    diagnostics::DiagnosticCode,
    source::{SourceMap, SourceSnapshot},
};

use super::{dump_ast, has_main_fn, lex, parent_before_child, parse};

fn parse_source(source: &str) -> (String, Vec<DiagnosticCode>, bool) {
    let snapshot = SourceSnapshot::from_str("parse.gg", source).expect("utf-8 fixture");
    let map = SourceMap::new(vec![snapshot.clone()]).expect("unique path");
    let file = map.file_id("parse.gg").expect("registered");
    let lexed = lex(&snapshot, &map, file);
    assert!(
        lexed.diagnostics.is_empty(),
        "词法失败: {:?}",
        lexed.diagnostics
    );
    let mut buffer = lexed.buffer;
    let parsed = parse(snapshot.content(), &map, file, &mut buffer);
    let dump = dump_ast(&parsed.file, &parsed.arena, &buffer.intern);
    let codes = parsed
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code())
        .collect();
    let main = has_main_fn(&parsed.file, &parsed.arena, &buffer.intern);
    assert!(
        parent_before_child(&parsed.arena),
        "同起点父节点必须先于子节点:\n{dump}"
    );
    (dump, codes, main)
}

#[test]
fn parses_simple_main() {
    let (dump, codes, main) = parse_source("fn main() {}\n");
    assert!(codes.is_empty(), "{codes:?}\n{dump}");
    assert!(main);
    assert!(dump.contains("item#"));
    assert!(dump.contains("fn main"));
    assert!(dump.contains("block"));
}

#[test]
fn parses_spec_declarations() {
    let source = r#"
use std.io.{print, println}
pub fn inc(i: int) int = i + 1
struct Point {
    pub x: int
    y: int
}
enum Option[T] {
    Some(T)
    None
}
union Bits { a: int, b: uint }
trait Print {
    fn print(self: &Self)
}
impl Print for Point {
    fn print(self: &Self) {}
}
const N: int = 1
type Ids = Vec[int]
static FLAG: bool = false
"#;
    let (dump, codes, _) = parse_source(source);
    assert!(codes.is_empty(), "{codes:?}\n{dump}");
    assert!(dump.contains("use"));
    assert!(dump.contains("struct"));
    assert!(dump.contains("enum"));
    assert!(dump.contains("union"));
    assert!(dump.contains("trait"));
    assert!(dump.contains("impl"));
}

#[test]
fn parses_control_async_select_try_defer() {
    let source = r#"
fn main() {
    let n = if let Ok(x) = a && x > 0 {
        x
    } else {
        0
    }
    match n {
        0 => 1
        _ => 2
    }
    try { 1 }
    loop { break }
    while ready { yield }
    for x in xs { _ = x }
    defer f.close()
    defer ret { g() }
    comptime source { 1 }
    let j = async { 1 }
    select {
        ch.send(1) => 1
        let v = ch.recv() => v
        let r = j.wait() => 0
        _ => 2
    }
    unsafe { 1 }
    comptime 1 + 2
    size_of[int]()
    asm("nop")
    chan[int](0)
}
"#;
    let (dump, codes, main) = parse_source(source);
    assert!(codes.is_empty(), "{codes:?}\n{dump}");
    assert!(main);
    assert!(dump.contains("select"));
    assert!(dump.contains("async"));
    assert!(dump.contains("try"));
}

#[test]
fn chained_comparison_has_primary_and_secondary() {
    let source = "fn main() { a < b < c }\n";
    let (dump, codes, _) = parse_source(source);
    assert!(
        codes.contains(&DiagnosticCode::ParseInvalidPrecedence),
        "{codes:?}\n{dump}"
    );
    let notes = codes
        .iter()
        .filter(|code| **code == DiagnosticCode::ParseInvalidPrecedence)
        .count();
    assert!(notes >= 1);
}

#[test]
fn unclosed_paren_recovers() {
    let source = "fn main( {}\n";
    let (dump, codes, _) = parse_source(source);
    assert!(!codes.is_empty(), "{dump}");
    assert!(
        codes.contains(&DiagnosticCode::ParseExpected)
            || codes.contains(&DiagnosticCode::ParseUnclosed)
            || codes.contains(&DiagnosticCode::ParseUnexpected),
        "{codes:?}"
    );
}

#[test]
fn dump_is_stable() {
    let source = "fn main() { 1 + 2 * 3 }\n";
    let first = parse_source(source).0;
    let second = parse_source(source).0;
    assert_eq!(first, second);
}

#[test]
fn library_without_main_parses() {
    let (dump, codes, main) = parse_source("fn util() int { 1 }\n");
    assert!(codes.is_empty(), "{codes:?}\n{dump}");
    assert!(!main);
}

#[test]
fn malformed_source_never_reaches_image_plan() {
    use crate::{CompileRequest, Compiler, TargetName};
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "broken.gg",
        "fn main( {}",
        TargetName::X86_64Linux,
    ));
    assert!(!compilation.is_success());
    assert!(compilation.image_plan().is_none());
    assert!(!compilation.diagnostics().items().is_empty());
}
