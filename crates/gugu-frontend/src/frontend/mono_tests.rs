//! 需要完整 `Compiler` 的单态化回归。

#[test]
fn empty_package_has_empty_mono_world() {
    let compilation = crate::Compiler::shared().compile_full_for_test(
        crate::CompileRequest::empty_package(crate::TargetName::X86_64Linux),
    );
    assert!(compilation.is_success());
    assert!(compilation.image_plan().is_none());
}
