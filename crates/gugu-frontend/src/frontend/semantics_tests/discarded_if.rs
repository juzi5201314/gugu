use super::{accept_cases, accepts};

accept_cases! {
    incomplete_if_let_chain_discards_nonunit_tails: true,
        "fn main() { let v: Result[int, string] = Err(\"e\")\n if let Ok(x) = v { x } else if let Err(_) = v { 0 } }";
    incomplete_if_let_chain_discards_mixed_tails: true,
        "fn main() { let v: Result[int, string] = Err(\"e\")\n if let Ok(x) = v { x } else if let Err(e) = v { e } }";
    complete_statement_if_discards_mixed_tails: true,
        "fn choose(flag: bool) { _ = if flag { 1 } else { \"tail\" } }\nfn main() { choose(false) }";
    value_used_incomplete_if_let_chain_is_rejected: false,
        "fn main() { let v: Result[int, string] = Err(\"e\")\n let n = if let Ok(x) = v { x } else if let Err(_) = v { 0 }\n _ = n }";
    value_used_mixed_if_tails_are_rejected: false,
        "fn main() { let n = if true { 1 } else { \"tail\" }\n _ = n }";
}

#[test]
fn discarded_if_hir_preserves_unit_and_never_results() {
    use crate::frontend::hir;

    let output = super::frontend(
        &[(
            "main.gg",
            "fn unit(flag: bool) { _ = if flag { 1 } else { \"tail\" } }\nfn diverge(flag: bool) { _ = if flag { panic(\"a\") } else { panic(\"b\") } }\nfn main() {}",
        )],
        &crate::QueryEngine::new(),
    )
    .unwrap();
    let module = output.hir.module();
    for (name, expected) in [("unit", hir::Type::Unit), ("diverge", hir::Type::Never)] {
        let owner = module
            .owners
            .iter()
            .find(|owner| module.definitions[owner.definition.index()].name == name)
            .unwrap();
        let expression = owner
            .expressions
            .iter()
            .position(|expression| matches!(expression.kind, hir::ExprKind::If { .. }))
            .unwrap();
        assert_eq!(
            module.types[owner.expression_types[expression].index()],
            expected
        );
    }
}
