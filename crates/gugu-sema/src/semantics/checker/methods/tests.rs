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

#[test]
fn inferred_inherent_impl_bounds_are_checked_after_receiver_inference() {
    let declaration = "trait Key {}\nimpl Key for int {}\n\
        struct Map[K, V] {}\nimpl[K: Key, V] Map[K, V] {\n\
        fn new() Self = Map[K, V] {}\nfn insert(self: &Self, key: K, value: V) {}\n}";
    for body in [
        "let m = Map::new()\n m.insert(1, true)",
        "let m: Map[int, bool] = Map::new()\n _ = m",
    ] {
        let errors = check(&format!("{declaration}\nfn main() {{ {body} }}"));
        assert!(errors.is_empty(), "{errors:?}");
    }
    let errors = check(&format!(
        "{declaration}\nfn main() {{ let m = Map::new()\n m.insert(true, 1) }}"
    ));
    assert!(!errors.is_empty());
}

#[test]
fn inferred_operator_impl_bounds_are_verified_after_numeric_inference() {
    let declaration = "struct Cell[T] {}\n\
        impl[T] Cell[T] { fn new() Self = Cell[T] {}\n\
        fn set(self: &Self, value: T) {} }\n\
        impl[T: Ord] Add[Cell[T]] for Cell[T] {\n\
        type Output = Cell[T]\n fn add(self: Self, rhs: Self) Self = self\n}";
    let errors = check(&format!(
        "{declaration}\nfn main() {{ let value = Cell::new()\n\
         _ = value + value\n let typed: Cell[u64] = value\n _ = typed }}"
    ));
    assert!(errors.is_empty(), "{errors:?}");
    let errors = check(&format!(
        "{declaration}\nfn main() {{ let value = Cell::new()\n\
         _ = value + value\n value.set(1.5) }}"
    ));
    assert!(
        errors
            .iter()
            .any(|error| error.message().contains("不满足 trait Ord")),
        "{errors:?}"
    );
}

#[test]
fn erroneous_receivers_do_not_report_cascading_missing_methods() {
    let errors = check("fn main() { let m = missing()\n m.insert(1, 2) }");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].message().contains("missing"));
}
