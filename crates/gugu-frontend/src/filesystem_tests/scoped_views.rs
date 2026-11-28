use super::{CompileRequest, Compiler, TargetName, harness::Harness};

macro_rules! rejects_scoped {
    ($($name:ident => $fixture:literal),* $(,)?) => {$(
        #[test]
        fn $name() {
            let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
                "main.gg", include_str!($fixture), TargetName::X86_64Linux,
            ));
            let diagnostics = format!("{:?}", output.diagnostics().items());
            assert!(!output.is_success(), "scoped callback 必须拒绝 {}", stringify!($name));
            assert!(diagnostics.to_ascii_lowercase().contains("scoped"), "{diagnostics}");
        }
    )*};
}

rejects_scoped! {
    scoped_callback_rejects_external_slot => "fixtures/scoped_views_external.gg",
    scoped_callback_rejects_reference_return => "fixtures/scoped_views_return.gg",
    scoped_callback_rejects_helper_escape => "fixtures/scoped_views_helper.gg",
    scoped_callback_rejects_dynamic_escape => "fixtures/scoped_views_dynamic.gg",
    scoped_callback_rejects_view_write => "fixtures/scoped_views_write.gg",
    scoped_callback_rejects_async_capture => "fixtures/scoped_views_async.gg",
    scoped_callback_rejects_transitive_yield => "fixtures/scoped_views_yield.gg",
    scoped_callback_rejects_raw_transmute => "fixtures/scoped_views_raw.gg",
    scoped_callback_rejects_aggregate_escape => "fixtures/scoped_views_aggregate.gg",
    scoped_callback_rejects_merged_view_copy => "fixtures/scoped_views_merged_copy.gg",
}

#[test]
fn scoped_callbacks_allow_reads_and_semantic_value_copies() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/scoped_views_reads.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = Harness::new(&output);
    assert_eq!(
        harness.invoke("main.gg", "reads", &[], &mut vec![0; 65536]),
        [1]
    );
}

#[test]
fn scoped_callback_allows_large_value_copy_return() {
    let output = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/scoped_views_large_copy.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = Harness::new(&output);
    assert_eq!(
        harness.invoke("main.gg", "copy_large", &[], &mut vec![0; 65536]),
        [1]
    );
}
