//! 编译真实公开调用，不启动宿主程序或线程。
use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};

fn verify(target: TargetName) {
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_full_for_test(
        CompileRequest::single_file("main.gg", include_str!("fixtures/runtime_stats.gg"), target),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let lir = output.dump_lir().expect("统计 LIR");
    assert!(lir.contains("fn stats "));
    assert!(lir.contains("fn gc_heap_live_bytes "));
    assert!(lir.contains("MonotonicNanos"));
    assert!(output.image_plan().is_some());
}
#[test]
fn stack_access_intrinsics_are_private_even_through_aliases() {
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_full_for_test(
        CompileRequest::single_file(
            "main.gg",
            include_str!("fixtures/private_stack_access.gg"),
            TargetName::X86_64Linux,
        ),
    );
    assert!(!output.is_success());
    assert!(output.image_plan().is_none());
    assert!(output.diagnostics().items().iter().any(|diagnostic| {
        diagnostic
            .message()
            .contains("不存在可导入项 `stack_read_word`")
    }));
    let direct = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_full_for_test(
        CompileRequest::single_file(
            "main.gg",
            include_str!("fixtures/private_stack_direct.gg"),
            TargetName::X86_64Linux,
        ),
    );
    assert!(!direct.is_success());
    assert!(direct.image_plan().is_none());
    for name in ["stack_read_word", "stack_copy_raw"] {
        assert!(
            direct
                .diagnostics()
                .items()
                .iter()
                .any(|diagnostic| diagnostic.message().contains(name)
                    && diagnostic.message().contains("runtime 注入源")),
            "{:?}",
            direct.diagnostics().items()
        );
    }
}

#[test]
fn runtime_stats_public_api_links_linux() {
    verify(TargetName::X86_64Linux);
}
#[test]
fn runtime_stats_public_api_links_windows() {
    verify(TargetName::X86_64Windows);
}
