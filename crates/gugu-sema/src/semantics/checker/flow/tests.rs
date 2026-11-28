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
        "tests/discarded-if@1",
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
fn discarded_if_let_chain_accepts_different_tail_types() {
    let errors = check(
        "enum Maybe { Good(int), Bad(string) }\n\
         fn choose(v: Maybe) { if let Maybe::Good(x) = v { x } else if let Maybe::Bad(e) = v { e } }\n\
         fn main() {}",
    );
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn value_used_if_let_chain_still_requires_final_else() {
    let errors = check(
        "enum Maybe { Good(int), Bad(string) }\n\
         fn choose(v: Maybe) int { if let Maybe::Good(x) = v { x } else if let Maybe::Bad(_) = v { 0 } }\n\
         fn main() {}",
    );
    assert!(
        errors
            .iter()
            .any(|error| error.code() == DiagnosticCode::InvalidExpression
                && error.message() == "取值的 if 必须有 else"),
        "{errors:?}"
    );
}

#[test]
fn value_used_if_still_checks_tail_types() {
    let errors = check("fn main() { let n = if true { 1 } else { \"tail\" }\n _ = n }");
    assert!(
        errors
            .iter()
            .any(|error| error.code() == DiagnosticCode::InvalidExpression),
        "{errors:?}"
    );
}
