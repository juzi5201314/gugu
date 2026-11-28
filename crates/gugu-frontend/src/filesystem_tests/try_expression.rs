use super::{CompileRequest, Compiler, TargetName, harness::Harness};

#[test]
fn try_expressions_wrap_success_and_preserve_failure_exits() {
    let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/try_expression.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    let mut harness = Harness::new(&compiled);
    let mut memory = vec![0; 65536];
    for (entry, arguments, expected) in [
        ("result_case", &[0][..], 8),
        ("result_case", &[1][..], 3),
        ("option_case", &[0][..], 8),
        ("option_case", &[1][..], 0),
        ("unit_and_plain_success", &[][..], 11),
        ("nested_exit", &[0][..], 8),
        ("nested_exit", &[1][..], 42),
        ("cleanup", &[0][..], 82),
        ("cleanup", &[1][..], 31),
        ("user_try", &[0][..], 8),
        ("user_try", &[1][..], 3),
        ("return_through_try", &[0][..], 2),
        ("return_through_try", &[1][..], 9),
        ("never_try", &[][..], 9),
    ] {
        assert_eq!(
            harness.invoke("main.gg", entry, arguments, &mut memory),
            [expected],
            "{entry}({arguments:?})"
        );
    }
}
