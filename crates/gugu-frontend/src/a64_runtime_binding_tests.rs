use crate::{CompileRequest, Compiler, TargetName, backend, runtime};

#[test]
fn a64_catalog_uses_canonical_frozen_runtime_definitions_not_names_or_aliases() {
    let compilation = Compiler::new().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        "fn main() {}",
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let lir = compilation.lir.as_ref().unwrap();
    let hir = compilation.hir.as_ref().unwrap();
    let bindings =
        backend::x64::glue::RuntimeGlueBindings::resolve_defs(runtime::GLUE_DEFS, hir, lir)
            .unwrap();
    let catalog = backend::a64::runtime::Catalog::from_resolved(&bindings, lir).unwrap();
    let (name, expected) = lir
        .bodies()
        .iter()
        .find_map(|body| {
            bindings
                .glue_of_instance(&body.instance)
                .map(|name| (name, body))
        })
        .unwrap();
    let function = catalog.function(name).unwrap();
    assert_eq!(function, expected);
    assert!(lir.bodies().contains(function));
    assert!(catalog.function("not_a_runtime_function").is_err());
    assert!(catalog.function("context_switch").is_err());
    let mut changed = function.clone();
    changed.name.push('!');
    let error = backend::a64::codegen::build_runtime(
        backend::a64::codegen::BuildRequest {
            bodies: &[changed],
            entry: function.instance,
            platform: backend::a64::foreign::Platform::Linux,
            bindings: &backend::a64::glue::RuntimeBindings {
                morestack_or_poll: "morestack".into(),
                safepoint_slow: "poll".into(),
                foreign: None,
            },
            definitions: &[],
            imports: &[],
            universe: None,
        },
        &catalog,
    )
    .unwrap_err();
    assert!(error.to_string().contains("frozen body differ"));
}

#[test]
fn aarch64_asm_reports_target_unsupported_before_x86_register_validation() {
    use crate::{CompileRequest, Compiler, DiagnosticCode, TargetName};
    let source = r#"
        global_asm("add x0, x0, x0")
        fn main() {
            let x = 5
            let y: int
            unsafe { asm("add x0, x0, x0", in("x0") x, lateout("x0") y) }
        }
    "#;
    for target in [TargetName::Aarch64Linux, TargetName::Aarch64Macos] {
        let result = Compiler::shared()
            .compile_sema_for_test(CompileRequest::single_file("main.gg", source, target));
        assert!(!result.is_success());
        let diagnostics = result.diagnostics().items();
        assert!(
            diagnostics
                .iter()
                .any(|error| error.code() == DiagnosticCode::InvalidExpression
                    && error.message() == format!("目标 {target} 暂不支持 asm")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|error| error.code() == DiagnosticCode::InvalidExpression
                    && error.message() == format!("目标 {target} 暂不支持 global_asm")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .all(|error| !error.message().contains("x86-64-v1")
                    && !error.message().contains("LIR pass")),
            "{diagnostics:?}"
        );
    }
}
