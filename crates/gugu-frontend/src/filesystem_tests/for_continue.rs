use super::{CompileRequest, Compiler, TargetName, harness::Harness};

fn check_for_continue(target: TargetName) {
    let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/for_continue.gg"),
        target,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    let mut harness = Harness::new(&compiled);
    let mut memory = vec![0; 65536];
    for (entry, argument, expected) in [
        ("range_count", 5, 5),
        ("range_count", 0, 0),
        ("range_skip", 5, 4),
        ("array_skip", 2, 8),
        ("slice_case", 2, 8),
        ("nested", 3, 44),
        ("range_cleanup", 5, 55),
        ("array_cleanup", 2, 40),
        ("while_continue", 5, 4),
        ("range_break", 5, 2),
    ] {
        assert_eq!(
            harness.invoke("main.gg", entry, &[argument], &mut memory),
            [expected],
            "{target:?}: {entry}({argument})"
        );
    }
}

#[test]
fn for_continue_advances_and_cleans_up_on_x64() {
    check_for_continue(TargetName::X86_64Linux);
}

#[test]
fn for_continue_advances_and_cleans_up_on_a64() {
    check_for_continue(TargetName::Aarch64Macos);
}
