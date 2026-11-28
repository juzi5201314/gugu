use super::{CompileRequest, Compiler, TargetName, harness::Harness, named};

#[test]
fn unicode_full_casing_and_segment_boundaries_execute_in_lir() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/unicode.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = Harness::new(&output);
    let mut memory = vec![0; 262144];
    for (entry, input, args, expected) in [
        ("lower_probe", "", &[][..], ""),
        (
            "lower_probe",
            "ABC İ ΟΣ ΟΣΑ ΟΣ\u{301}",
            &[],
            "abc i\u{307} ος οσα ος\u{301}",
        ),
        ("upper_probe", "Straße ﬃ iı", &[], "STRASSE FFI II"),
        ("fold_probe", "Straße ẞ Σς İ", &[], "strasse ss σσ i\u{307}"),
        ("segment_probe", "", &[0][..], ""),
        (
            "segment_probe",
            "a\u{301}🇨🇳🇺🇸👩\u{200d}👩\u{200d}👧\u{200d}👦\r\n",
            &[0],
            "[a\u{301}][🇨🇳][🇺🇸][👩\u{200d}👩\u{200d}👧\u{200d}👦][\r\n]",
        ),
        ("segment_probe", "각क्क", &[0], "[각][क्क]"),
        (
            "segment_probe",
            "can't 12,345.6 אב\"גד a_b",
            &[1],
            "[can't][ ][12,345.6][ ][אב\"גד][ ][a_b]",
        ),
        (
            "segment_probe",
            "a\u{301} b\r\nc",
            &[1],
            "[a\u{301}][ ][b][\r\n][c]",
        ),
        ("segment_probe", "", &[2], "[]"),
        (
            "segment_probe",
            "a\r\nb\u{b}c\u{c}d\u{85}e\u{2028}f\u{2029}g\r",
            &[2],
            "[a\r\n][b\u{b}][c\u{c}][d\u{85}][e\u{2028}][f\u{2029}][g\r]",
        ),
    ] {
        let result = harness.text(named(&output, entry), input, args, &mut memory);
        let start = result[0] as usize;
        assert_eq!(
            &memory[start..start + result[1] as usize],
            expected.as_bytes(),
            "{entry}({input:?}, {args:?})"
        );
    }
}

fn check_boundaries(vectors: &str, kind: u64, shard: usize) {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/unicode_boundaries.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = Harness::new(&output);
    let mut memory = vec![0; 262144];
    for (index, line) in vectors.lines().enumerate() {
        let body = line.split('#').next().unwrap().trim();
        if body.is_empty() || index % 4 != shard {
            continue;
        }
        let mut input = String::new();
        let mut boundaries = Vec::new();
        for token in body.split_whitespace() {
            match token {
                "÷" => {
                    boundaries.push(input.len());
                }
                "×" => {}
                scalar => {
                    let scalar = char::from_u32(u32::from_str_radix(scalar, 16).unwrap()).unwrap();
                    input.push(scalar);
                }
            }
        }
        for boundary in boundaries.windows(2) {
            let result = harness.text(
                named(&output, "boundary_probe"),
                &input[boundary[0]..],
                &[kind],
                &mut memory,
            );
            assert_eq!(
                result,
                [u64::try_from(boundary[1] - boundary[0]).unwrap()],
                "Unicode 17 第 {} 行：{line}，起点 {}",
                index + 1,
                boundary[0]
            );
        }
    }
}

macro_rules! boundary_shards {
    ($fixture:literal, $kind:literal, $($name:ident: $shard:literal),+ $(,)?) => {
        $(#[test]
        fn $name() {
            check_boundaries(include_str!($fixture), $kind, $shard);
        })+
    };
}

boundary_shards!("fixtures/GraphemeBreakTest.txt", 0,
    unicode_grapheme_conformance_0: 0, unicode_grapheme_conformance_1: 1,
    unicode_grapheme_conformance_2: 2, unicode_grapheme_conformance_3: 3,
);
boundary_shards!("fixtures/WordBreakTest.txt", 1,
    unicode_word_conformance_0: 0, unicode_word_conformance_1: 1,
    unicode_word_conformance_2: 2, unicode_word_conformance_3: 3,
);
