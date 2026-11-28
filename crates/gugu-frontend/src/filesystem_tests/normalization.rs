use super::{CompileRequest, Compiler, TargetName, harness::Harness, named};

const VECTORS: &[u8] = include_bytes!("../../../../ci/e2e/fixtures/NormalizationTest.0.utf8");

#[test]
fn string_normalization_in_lir() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        r#"use std.text.{NormalizationForm}
fn normalize_probe(input: &string, form: int) string {
    let kind = match form {
        0 => NormalizationForm::NFC,
        1 => NormalizationForm::NFD,
        2 => NormalizationForm::NFKC,
        _ => NormalizationForm::NFKD,
    }
    input.normalize(kind)
}
fn main() {
    let input = "é"
    _ = normalize_probe(&input, 0)
}"#,
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = Harness::new(&output);
    let body = named(&output, "normalize_probe");
    let mut memory = vec![0; 262144];
    let mut check = |input: &str, form, expected: &str| {
        let result = harness.text(body, input, &[form], &mut memory);
        let start = result[0] as usize;
        let actual = &memory[start..start + result[1] as usize];
        assert_eq!(actual, expected.as_bytes(), "form {form}, input {input:?}");
    };

    let mut data = VECTORS;
    for _ in 0..6 {
        let row: [&str; 5] = std::array::from_fn(|_| {
            let (&length, rest) = data.split_first().unwrap();
            let (text, rest) = rest.split_at(length as usize);
            data = rest;
            std::str::from_utf8(text).unwrap()
        });
        check(row[0], 0, row[1]);
        check(row[0], 1, row[2]);
    }
    for (input, form, expected) in [
        ("", 0, ""),
        ("a\0z", 3, "a\0z"),
        ("aé", 1, "ae\u{301}"),
        ("\u{FB01}", 0, "\u{FB01}"),
        ("\u{FB01}", 2, "fi"),
        ("\u{FDFA}", 3, "صلى الله عليه وسلم"),
        ("\u{1100}\u{1161}\u{11A8}", 0, "각"),
        ("각", 1, "\u{1100}\u{1161}\u{11A8}"),
        ("\u{301}\u{323}\u{300}", 1, "\u{323}\u{301}\u{300}"),
        ("\u{1100}\u{301}\u{1161}", 0, "\u{1100}\u{301}\u{1161}"),
        ("\u{958}", 0, "\u{915}\u{93C}"),
    ] {
        check(input, form, expected);
    }
}
