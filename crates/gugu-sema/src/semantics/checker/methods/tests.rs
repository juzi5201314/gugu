use crate::semantics::{Model, comptime::EarlyConstTable};
use crate::{Diagnostic, DiagnosticCode};

fn check(source: &str) -> Vec<Diagnostic> {
    let sources = crate::SourceMap::new(vec![
        crate::source::SourceSnapshot::from_str("main.gg", source).unwrap(),
    ])
    .unwrap();
    let cfg = crate::cfg::CfgContext::target_only(crate::TargetName::X86_64Linux);
    let modules = crate::modules::parse_modules(&sources, "", &cfg, &Default::default()).unwrap();
    let names = crate::names::analyze_with_prefix(
        "tests/constructor-generic-kinds@1",
        &Default::default(),
        &modules,
        None,
    )
    .unwrap();
    let paths = Default::default();
    let model = Model::new(&modules, &names, &paths).unwrap();
    super::super::check(&model, &EarlyConstTable::default(), None, cfg.target())
        .err()
        .unwrap_or_default()
}

#[test]
fn explicit_constructor_rejects_values_for_type_parameters_in_every_variant_shape() {
    for variant in ["Unit", "Tuple(1)", "Rec { value: 1 }"] {
        let errors = check(&format!(
            "enum E[T] {{ Unit, Tuple(int), Rec {{ value: int }} }}\nfn main() {{ _ = E[3]::{variant} }}"
        ));
        assert!(
            errors
                .iter()
                .any(|error| error.code() == DiagnosticCode::InvalidType
                    && error.message() == "类型与 comptime 泛型实参种类不符"),
            "{variant}: {errors:?}"
        );
    }
}

#[test]
fn explicit_constructor_rejects_types_for_comptime_parameters_in_every_variant_shape() {
    for variant in ["Unit", "Tuple(1)", "Rec { value: 1 }"] {
        let errors = check(&format!(
            "enum E[comptime N: int] {{ Unit, Tuple(int), Rec {{ value: int }} }}\nfn main() {{ _ = E[string]::{variant} }}"
        ));
        assert!(
            errors
                .iter()
                .any(|error| error.code() == DiagnosticCode::InvalidType
                    && error.message() == "类型与 comptime 泛型实参种类不符"),
            "{variant}: {errors:?}"
        );
    }
}

#[test]
fn explicit_constructor_preserves_type_and_comptime_arguments_and_context_inference() {
    let errors = check(
        "enum E[T, comptime N: int] { Unit, Tuple([T; N]), Rec { value: T } }\n\
         fn make[T, comptime N: int](value: [T; N]) E[T, N] = E[T, N]::Tuple(value)\n\
         fn main() { _ = E[int, 2]::Unit\n _ = make::[int, 2]([1, 2])\n\
         _ = E[int, 2]::Rec { value: 3 }\n\
         let inferred: E[int, 2] = E::Tuple([1, 2])\n _ = inferred }",
    );
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn explicit_constructor_rejects_missing_or_extra_generic_arguments() {
    for arguments in ["int", "int, 2, 3"] {
        let errors = check(&format!(
            "enum E[T, comptime N: int] {{ Unit }}\nfn main() {{ _ = E[{arguments}]::Unit }}"
        ));
        assert!(
            errors
                .iter()
                .any(|error| error.code() == DiagnosticCode::InvalidType
                    && error.message().contains("类型实参数量不符")),
            "{arguments}: {errors:?}"
        );
    }
}
