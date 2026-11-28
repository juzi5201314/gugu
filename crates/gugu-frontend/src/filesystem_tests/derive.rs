use super::{CompileRequest, Compiler, TargetName, printing};

#[test]
fn derive_eq_clone_print_preserve_fields_and_variants() {
    let output = printing::output(include_str!("fixtures/derive.gg"), &[]);
    let text: Vec<_> = output.into_iter().flat_map(|(_, bytes)| bytes).collect();
    assert_eq!(text, include_bytes!("fixtures/derive.stdout"));
}

#[test]
fn derive_rejects_unsupported_fields_at_the_field() {
    for (source, field, interface) in [
        (
            "#[derive(Eq)]\nstruct Bad { value: float }\nfn main() {}",
            "value",
            "Eq",
        ),
        (
            "struct Missing {}\n#[derive(Clone)]\nstruct Bad { value: Missing }\nfn main() {}",
            "value",
            "Clone",
        ),
        (
            "struct Missing {}\n#[derive(Print)]\nenum Bad { Value(Missing) }\nfn main() {}",
            "0",
            "Print",
        ),
    ] {
        let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(!compiled.is_success());
        let diagnostic = compiled
            .diagnostics()
            .items()
            .iter()
            .find(|diagnostic| {
                diagnostic
                    .message()
                    .contains(&format!("字段 `{field}` 未实现 {interface}"))
            })
            .unwrap_or_else(|| panic!("{:?}", compiled.diagnostics().items()));
        let span = diagnostic.span().expect("派生失败必须定位到字段");
        assert_eq!(
            &source[span.start() as usize..span.end() as usize],
            if field == "0" {
                "Missing"
            } else if interface == "Eq" {
                "value: float"
            } else {
                "value: Missing"
            }
        );
    }
}
