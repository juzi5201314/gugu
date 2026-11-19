use super::printing::output;

fn stdout(body: &str) -> String {
    let source = format!("use std.io.{{println}}\nfn main() {{\n{body}\n}}");
    let bytes: Vec<u8> = output(&source, &[])
        .into_iter()
        .filter(|(fd, _)| *fd == 1)
        .flat_map(|(_, bytes)| bytes)
        .collect();
    String::from_utf8(bytes).expect("UTF-8 输出")
}

#[test]
fn string_growth_methods() {
    assert_eq!(
        stdout(
            r#"let s = string::with_capacity(8)
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
    println(s.len(), " [", s, "]")"#
        ),
        "true 0\naé中😀xyz 13 true false\n0 []\n"
    );
}

#[test]
fn string_editing_methods() {
    assert_eq!(
        stdout(
            r#"let s = "héllo"
    s.insert(1, '中')
    println(s)
    println(s.remove(1), " ", s)
    s.replace_range(1..3, "e")
    println(s)
    s.truncate(3)
    println(s)"#
        ),
        "h中éllo\n中 héllo\nhello\nhel\n"
    );
}

#[test]
fn string_reading_methods() {
    assert_eq!(
        stdout(
            r#"let s = "aé中"
    match s.byte_at(1) { Some(b) => println(b), None => println("none") }
    match s.byte_at(6) { Some(b) => println(b), None => println("none") }
    match s.char_at(2) { Some(c) => println(c), None => println("none") }
    match s.char_at(3) { Some(c) => println(c), None => println("none") }
    for c in s.chars() { println(c) }
    println(s.bytes_view().len())"#
        ),
        "195\nnone\n中\nnone\na\né\n中\n6\n"
    );
}

#[test]
fn string_mutation_does_not_alias_copies() {
    assert_eq!(
        stdout(
            r#"let original = "gu"
    let copy = original
    copy.push_string("gu")
    let chars = copy.chars()
    copy.clear()
    let rest = 0
    while let Some(_) = chars.next() { rest = rest + 1 }
    println(original, " ", copy.len(), " ", rest)"#
        ),
        "gu 0 4\n"
    );
}

#[test]
fn string_edits_at_the_end() {
    assert_eq!(
        stdout(
            r#"let s = "aé"
    s.insert(s.len(), '!')
    println(s, " [", s[s.len()..], "]")
    println(s.remove(s.len() - 1), " ", s)
    s.replace_range(1..s.len(), "bc")
    s.truncate(s.len())
    println(s)"#
        ),
        "aé! []\n! aé\nabc\n"
    );
}
