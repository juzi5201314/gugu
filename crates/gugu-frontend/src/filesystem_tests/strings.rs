use super::{CompileRequest, Compiler, TargetName, harness::Harness, printing::capture};

fn run_case(compiled: &super::Compilation, entry: &str) -> String {
    let mut harness = Harness::new(compiled);
    harness.machine_hook = Some(capture);
    harness.invoke("main.gg", entry, &[], &mut vec![0; 262144]);
    let bytes: Vec<u8> = harness
        .calls
        .into_iter()
        .filter(|(fd, _)| *fd == 1)
        .flat_map(|(_, bytes)| bytes.into_iter().map(|byte| byte as u8))
        .collect();
    String::from_utf8(bytes).expect("UTF-8 输出")
}

#[test]
fn string_methods_and_copy_isolation_contracts() {
    let source = r#"
use std.io.{println}

fn growth() {
    let s = string::with_capacity(8)
    println(s.is_empty(), " ", s.len())
    s.push('a')
    s.push('é')
    s.push('中')
    s.push('😀')
    s.push_string("xyz")
    s.reserve(4)
    s.shrink_to_fit()
    println(s, " ", s.len(), " ", s.cap() >= s.len(), " ", s.is_empty())
    s.clear()
    println(s.len(), " [", s, "]")
}

fn editing() {
    let s = "héllo"
    s.insert(1, '中')
    println(s)
    println(s.remove(1), " ", s)
    s.replace_range(1..3, "e")
    println(s)
    s.truncate(3)
    println(s)
}

fn reading() {
    let s = "aé中"
    match s.byte_at(1) { Some(b) => println(b), None => println("none") }
    match s.byte_at(6) { Some(b) => println(b), None => println("none") }
    match s.char_at(2) { Some(c) => println(c), None => println("none") }
    match s.char_at(3) { Some(c) => println(c), None => println("none") }
    for c in s.chars() { println(c) }
    println(s.bytes_view().len())
}

fn copy_isolation() {
    let original = "gu"
    let copy = original
    copy.push_string("gu")
    let chars = copy.chars()
    copy.clear()
    let rest = 0
    while let Some(_) = chars.next() { rest = rest + 1 }
    println(original, " ", copy.len(), " ", rest)
}

fn edits_at_end() {
    let s = "aé"
    s.insert(s.len(), '!')
    println(s, " [", s[s.len()..], "]")
    println(s.remove(s.len() - 1), " ", s)
    s.replace_range(1..s.len(), "bc")
    s.truncate(s.len())
    println(s)
}

fn main() {
    growth()
    editing()
    reading()
    copy_isolation()
    edits_at_end()
}
"#;
    let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    assert_eq!(
        run_case(&compiled, "growth"),
        "true 0\naé中😀xyz 13 true false\n0 []\n"
    );
    assert_eq!(
        run_case(&compiled, "editing"),
        "h中éllo\n中 héllo\nhello\nhel\n"
    );
    assert_eq!(
        run_case(&compiled, "reading"),
        "195\nnone\n中\nnone\na\né\n中\n6\n"
    );
    assert_eq!(run_case(&compiled, "copy_isolation"), "gu 0 4\n");
    assert_eq!(run_case(&compiled, "edits_at_end"), "aé! []\n! aé\nabc\n");
}
