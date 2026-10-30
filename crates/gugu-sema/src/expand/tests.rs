use crate::{DiagnosticCode, TargetName};

#[test]
fn cached_macro_replays_resource_cost_without_caching_action_exhaustion() {
    let sources = crate::SourceMap::new(vec![
        crate::source::SourceSnapshot::from_str(
            "main.gg",
            "fn main() { _ = comptime source { std.syntax.parse_expr(\"1\") } }",
        )
        .unwrap(),
    ])
    .unwrap();
    let cfg = crate::cfg::CfgContext::new(
        TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    let modules = crate::modules::parse_modules(&sources, "", &cfg, &Default::default()).unwrap();
    let names =
        crate::names::analyze_with_prefix("tests/macros@1", &Default::default(), &modules, None)
            .unwrap();
    let model =
        crate::semantics::model::Model::new(&modules, &names, &std::collections::BTreeSet::new())
            .unwrap();
    let calls = super::collect(&modules);
    let queries = crate::query::QueryEngine::new();
    let host = super::ExpandHost { queries: &queries };
    let run = |budget: &mut super::ExpansionBudget| {
        super::evaluate_macro(
            &model, &sources, &calls[0], &host, 1, &[0; 32], &cfg, &queries, budget,
        )
    };
    let mut exhausted = super::ExpansionBudget {
        fuel_limit: 0,
        ..Default::default()
    };
    assert!(run(&mut exhausted).is_err());
    let mut available = super::ExpansionBudget::default();
    assert_eq!(
        run(&mut available)
            .expect("action 预算失败不能污染宏求值缓存")
            .text,
        "1"
    );
    let mut exhausted_again = super::ExpansionBudget {
        heap_limit: 0,
        ..Default::default()
    };
    let errors = match run(&mut exhausted_again) {
        Err(errors) => errors,
        Ok(_) => panic!("缓存命中仍必须扣除 heap 预算"),
    };
    assert!(
        errors
            .iter()
            .any(|error| error.code() == DiagnosticCode::ExpansionLimit)
    );
}
