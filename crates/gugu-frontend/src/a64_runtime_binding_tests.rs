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
fn aarch64_leaf_extern_imports_build_for_linux_and_macos() {
    let source = "#[ffi(leaf)]\nextern \"C\" fn getpid() i32\nfn main() { let _ = getpid() }";
    for target in [TargetName::Aarch64Linux, TargetName::Aarch64Macos] {
        let compilation =
            Compiler::shared().compile(CompileRequest::single_file("main.gg", source, target));
        assert!(
            compilation.is_success(),
            "{target}: {:?}",
            compilation.diagnostics().items()
        );
        assert!(compilation.image_plan().is_some());
    }
}
