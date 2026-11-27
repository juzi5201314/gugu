use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};

#[test]
fn shared_closure_uses_guarded_payload_and_real_runtime_entries() {
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_full_for_test(
        CompileRequest::single_file(
            "main.gg",
            include_str!("fixtures/shared_handle.gg"),
            TargetName::X86_64Linux,
        ),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    assert!(
        output
            .image_plan()
            .unwrap()
            .shared_heap_demand()
            .alloc_sites
            > 0,
        "夹具必须产生共享存储"
    );
    let lir = output.dump_lir().unwrap();
    for entry in [
        "gc_resolve_shared_handle",
        "gc_shared_access_begin",
        "gc_shared_access_end",
    ] {
        assert!(
            lir.contains(&format!("fn {entry} ")),
            "缺少共享入口 {entry}"
        );
    }
    assert!(
        output
            .image_plan()
            .unwrap()
            .shared_heap_demand()
            .alloc_sites
            > 0
    );
}

#[test]
fn pin_after_channel_publication_keeps_scope_cleanup() {
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_full_for_test(
        CompileRequest::single_file(
            "main.gg",
            include_str!("fixtures/shared_pin.gg"),
            TargetName::X86_64Linux,
        ),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let demand = output.image_plan().unwrap().local_heap_demand();
    assert!(demand.pin_sites > 0);
    assert!(demand.unpin_sites >= demand.pin_sites);
}
