use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};

fn verify(target: TargetName) {
    let output = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile_full_for_test(
        CompileRequest::single_file("main.gg", include_str!("../fixtures/pin.gg"), target),
    );
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let lir = output.dump_lir().unwrap();
    for name in ["pin_prepare", "pin_acquire", "pin_release"] {
        assert!(
            lir.contains(&format!("fn {name} ")),
            "缺少 pin 生命周期入口 {name}"
        );
    }
    let demand = output.image_plan().unwrap().local_heap_demand();
    assert!(demand.pin_sites > 0, "真实 pin_acquire 调用不能漏计");
    assert!(
        demand.unpin_sites >= demand.pin_sites,
        "正常与展开清理必须纳入统计"
    );
}

#[test]
fn source_pin_instantiates_nested_callback_and_cleanup() {
    verify(TargetName::X86_64Linux);
}

#[test]
fn source_pin_instantiates_windows_callback_and_cleanup() {
    verify(TargetName::X86_64Windows);
}
