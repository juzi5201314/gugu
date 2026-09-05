use crate::{CompileRequest, Compiler, TargetName};
fn accepts(source: &str) -> bool {
    let c = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    if !c.is_success() {
        eprintln!("{:?}", c.diagnostics().items());
    }
    c.is_success()
}
#[test]
fn initialization_merges_only_reachable_predecessors() {
    assert!(accepts(
        "fn choose(b: bool) int { let x: int\n if b { x = 1 } else { return 2 }\n x }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn choose(b: bool) int { let x: int\n if b { x = 1 }\n x }\nfn main() {}"
    ));
    assert!(accepts("fn main() { let x = 1\n let x = x + 1\n _ = x }"));
    assert!(!accepts("fn main() { let x: int\n _ = x }"));
}
#[test]
fn boolean_product_coverage_is_structural() {
    assert!(accepts(
        "fn f(t: (bool, bool)) int { match t { (true, _) => 1\n (false, true) => 2\n (false, false) => 3 } }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn f(t: (bool, bool)) int { match t { (true, _) => 1\n (false, true) => 2 } }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn f(b: bool) int { match b { true => 1, false if b => 2 } }\nfn main() {}"
    ));
}
#[test]
fn operators_and_loop_exit_types_are_checked() {
    assert!(!accepts("fn main() { let x = true + false }"));
    assert!(!accepts(
        "fn main() { let x = if true { 1 } else { false } }"
    ));
    assert!(accepts("fn f() int { loop { break 3 } }\nfn main() {}"));
    assert!(!accepts("fn main() { while true { break 3 } }"));
}

#[test]
fn initializers_reject_cycles_and_separate_lazy_domains() {
    assert!(accepts(
        "static A: int = B + 1\nstatic B: int = 2\nfn main() {}"
    ));
    assert!(!accepts(
        "static A: int = B\nstatic B: int = A\nfn main() {}"
    ));
    assert!(!accepts(
        "static A: int = f()\nfn f() int = A\nfn main() {}"
    ));
    assert!(accepts(
        "#[coroutine_local] static A: int = A\n#[os_thread_local] static B: int = B\nfn main() {}"
    ));
    assert!(!accepts(
        "#[coroutine_local] static A: int = 1\nstatic B: int = A\nfn main() {}"
    ));
    assert!(!accepts("fn main(_: int) {}"));
}

#[test]
fn local_static_does_not_capture_automatic_slots() {
    assert!(accepts(
        "fn f() int { static count: int = 0\n count += 1\n count }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn f(arg: int) int { static count: int = arg\n count }\nfn main() {}"
    ));
}

#[test]
fn literal_bounds_and_signed_patterns() {
    assert!(accepts(
        "fn f(v: i8) int { match v { -128 => 0, _ => 1 } }\nfn main() { let n: i8 = -128\n _ = n }"
    ));
    assert!(!accepts("fn main() { let n: i8 = 128 }"));
    assert!(!accepts("fn main() { let n: i8 = -129 }"));
    assert!(accepts(
        "fn main() { let n: u128 = 340282366920938463463374607431768211455\n _ = n }"
    ));
    assert!(!accepts(
        "fn main() { let n: u128 = 340282366920938463463374607431768211456 }"
    ));
}

#[test]
fn try_failures_join_initialization_before_the_expression() {
    assert!(accepts(
        "fn f(o: Option[int]) Option[int] { let x = o?\n Some(x) }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn f(o: Option[int]) int { let x: int\n let result: Option[int] = try { let y = o?\n x = y\n y }\n x }\nfn main() {}"
    ));
    assert!(accepts(
        "fn f(o: Option[int]) Option[int] { let result = try { o? + 1 }\n result }\nfn main() {}"
    ));
}

#[test]
fn deferred_blocks_read_at_exit_and_calls_capture_at_registration() {
    assert!(accepts(
        "fn sink(v: int) {}\nfn main() { let x: int\n defer { sink(x) }\n x = 7 }"
    ));
    assert!(!accepts(
        "fn sink(v: int) {}\nfn main() { let x: int\n defer sink(x)\n x = 7 }"
    ));
    assert!(!accepts("fn main() { defer { return } }"));
    assert!(accepts(
        "fn main() { defer panic(\"延迟\")\n let value = 3\n _ = value }"
    ));
    assert!(!accepts(
        "fn sink(v: int) {}\nfn f(b: bool) { let x: int\n defer { sink(x) }\n if b { return }\n x = 7 }\nfn main() {}"
    ));
}

#[test]
fn places_preserve_references_and_reject_constants() {
    assert!(accepts(
        "fn f(p: &int) int { *p = 2\n *p }\nfn main() { let x = 0\n _ = f(&x) }"
    ));
    assert!(!accepts("const N = 1\nfn main() { N = 2 }"));
    assert!(!accepts("fn main() { let p = &1 }"));
    assert!(accepts("fn main() { let a = [1, 2]\n a[0] = 3 }"));
}

#[test]
fn numeric_inference_has_no_implicit_scalar_conversion() {
    assert!(accepts(
        "fn f(x: i8) bool { 1 == x }\nfn main() { let n = 1\n let x: i8 = n + 1\n _ = x }"
    ));
    assert!(!accepts("fn f(x: i8, y: int) int { x + y }\nfn main() {}"));
    assert!(accepts(
        "fn main() { let x = int(1.5)\n let y = byte(x)\n _ = y }"
    ));
    assert!(!accepts("fn main() { let x = bool(1) }"));
    assert!(!accepts(
        "struct S {}\nfn main() { _ = (S {},) == (S {},) }"
    ));
}

#[test]
fn patterns_cover_references_slices_ranges_and_alternatives() {
    assert!(accepts(
        "fn f(xs: &[bool]) int { match xs { [] => 0, [true, ..] => 1, [false, ..] => 2 } }\nfn main() {}"
    ));
    assert!(accepts(
        "fn f(c: char) int { match c { '\\0'..'\\u{10ffff}' => 0, '\\u{10ffff}' => 1 } }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn f(v: int) int { match v { 2..2 => 0, _ => 1 } }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn f(t: (int, int)) { let (x, x) = t }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn f(o: Option[int]) int { match o { Some(x) | None => 0 } }\nfn main() {}"
    ));
    assert!(accepts(
        "fn f(v: int) int { match v { x @ 0..10 | x @ 10..20 => x, _ => 0 } }\nfn main() {}"
    ));
}

#[test]
fn rest_patterns_and_product_coverage_preserve_types() {
    assert!(!accepts(
        "fn f(xs: &[bool]) &[bool] { let rest @ .. = xs\n rest }\nfn main() {}"
    ));
    assert!(accepts(
        "fn f(xs: &[bool]) &[bool] { let [rest @ ..] = xs\n rest }\nfn main() {}"
    ));
    assert!(accepts(
        "fn f(xs: [bool; 4]) [bool; 2] { let [_, rest @ .., _] = xs\n rest }\nfn main() {}"
    ));
    assert!(accepts(
        "fn f(x: &bool) &bool { match x { same @ true | same @ false => same } }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn f(pair: (bool, bool)) int { match pair { (true, true) => 1, (false, false) => 0 } }\nfn main() {}"
    ));
    assert!(accepts(
        "fn main() { let n = match 1 { 1 => 2, _ => 3 }\n _ = n }"
    ));
    assert!(accepts("fn main() { if let Some(1) = Some(2) {} }"));
    assert!(accepts(
        "fn f(b: bool) { let n: int\n match b { _ if { n = 1\n false } => (), _ => { _ = n } } }\nfn main() {}"
    ));
}

#[test]
fn condition_bindings_and_loop_initialization_follow_execution() {
    assert!(accepts(
        "fn f(o: Option[int]) int { if let Some(x) = o && x > 0 { x } else { 0 } }\nfn main() {}"
    ));
    assert!(!accepts("fn main() { if let x = 1 {} }"));
    assert!(!accepts(
        "fn f(o: Option[int]) { if let Some(x) = o {} else { _ = x } }\nfn main() {}"
    ));
    assert!(accepts(
        "fn main() { let x: int\n while { x = 1\n false } {}\n _ = x }"
    ));
    assert!(!accepts(
        "fn main() { let x: int\n while false { x = 1 }\n _ = x }"
    ));
}

#[test]
fn function_defer_outlives_nested_blocks() {
    assert!(accepts(
        "fn sink(x: int) {}\nfn main() { let x: int\n { defer ret { sink(x) } }\n x = 1 }"
    ));
    assert!(!accepts(
        "fn sink(x: int) {}\nfn main() { let x: int\n { defer ret { sink(x) } } }"
    ));
    assert!(!accepts(
        "fn main() { defer { let x: bool = 1 }\n loop {} }"
    ));
    assert!(accepts(
        "fn sink(x: int) {}\nfn f(flag: bool) { let x: int\n if flag { defer ret { sink(x) }\n x = 1 } }\nfn main() {}"
    ));
    assert!(accepts(
        "fn sink(x: int) {}\nfn f(flag: bool) { if flag { let x = 1\n defer ret { sink(x) } } }\nfn main() {}"
    ));
}

fn frontend(
    sources: &[(&str, &str)],
    queries: &crate::QueryEngine,
) -> Result<super::super::FrontendOutput, Vec<crate::Diagnostic>> {
    let sources = crate::SourceMap::new(
        sources
            .iter()
            .map(|(path, source)| crate::SourceSnapshot::from_str(path, source).unwrap())
            .collect(),
    )
    .unwrap();
    let cfg = super::super::cfg::CfgContext::new(
        TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    super::super::bootstrap(
        super::super::SourceInput::Sources {
            source_map: &sources,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/semantics@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        queries,
    )
}

#[test]
fn declarations_enforce_visibility_and_initialization_domains() {
    let queries = crate::QueryEngine::new();
    let error = frontend(
        &[
            (
                "main.gg",
                "use model.{Secret}\nfn main() { _ = Secret { value: 1 } }",
            ),
            ("model.gg", "pub struct Secret { value: int }"),
        ],
        &queries,
    )
    .unwrap_err();
    assert!(
        error
            .iter()
            .any(|error| error.code() == crate::DiagnosticCode::InvalidExpression)
    );
    let checked = frontend(&[("main.gg", "static A: int = B\nstatic B: int = 1\n#[coroutine_local] static C: int = C\n#[os_thread_local] static D: int = D\nfn main() {}")], &queries).unwrap();
    let names: Vec<_> = checked
        .semantics
        .initialization
        .iter()
        .map(|init| {
            let module = &checked.modules[init.definition.module];
            (
                module.tokens.intern.get_str(
                    module.arena.items[init.definition.item.0 as usize]
                        .name
                        .unwrap(),
                ),
                init.kind,
            )
        })
        .collect();
    use super::initialization::InitKind;
    assert_eq!(
        names,
        [
            ("B", InitKind::Process),
            ("A", InitKind::Process),
            ("C", InitKind::Coroutine),
            ("D", InitKind::OsThread)
        ]
    );
    assert!(!accepts("type Broken = Missing\nfn main() {}"));
    assert!(!accepts("let forbidden = 1\nfn main() {}"));
}

#[test]
fn runtime_checks_survive_queries_and_reach_the_backend_plan() {
    use super::output::CheckKind;
    let queries = crate::QueryEngine::new();
    let source = "fn f(x: int, n: u8, a: &[int], s: string, d: float) { _ = x / 0\n _ = x << n\n _ = a[x]\n _ = s[x..]\n _ = int(d)\n _ = char(x)\n unsafe { _ = a[x] } }\nfn main() {}";
    let cold = frontend(&[("main.gg", source)], &queries).unwrap();
    let warm = frontend(&[("main.gg", source)], &queries).unwrap();
    assert_eq!(cold.semantics, warm.semantics);
    let checks: Vec<_> = warm
        .semantics
        .bodies
        .iter()
        .flat_map(|body| body.runtime_checks.iter().map(|check| &check.kind))
        .collect();
    assert_eq!(
        checks,
        [
            &CheckKind::IntegerDivision {
                ty: super::model::Ty::int()
            },
            &CheckKind::Shift {
                ty: super::model::Ty::int()
            },
            &CheckKind::Bounds { slice: false },
            &CheckKind::Bounds { slice: true },
            &CheckKind::Utf8Boundary,
            &CheckKind::FloatToInt {
                signed: true,
                bits: 64
            },
            &CheckKind::UnicodeScalar,
        ]
    );
    let compiler = Compiler::new();
    let first = compiler.compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { let x = 1 }",
        TargetName::X86_64Linux,
    ));
    let second = compiler.compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { let x = 2 }",
        TargetName::X86_64Linux,
    ));
    assert_ne!(
        first.image_plan().unwrap().semantic_fingerprint(),
        second.image_plan().unwrap().semantic_fingerprint()
    );
}

#[test]
fn failed_cached_queries_rebind_source_locations_and_stop_lowering() {
    let compiler = Compiler::new();
    let source = "fn main() { let x: int\n _ = x }";
    for _ in 0..2 {
        let failed = compiler.compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(failed.image_plan().is_none());
        let diagnostic = &failed.diagnostics().items()[0];
        assert_eq!(diagnostic.code(), crate::DiagnosticCode::InvalidDeclaration);
        let span = diagnostic.span().unwrap();
        let sources = failed.source_map();
        let offset = source.rfind('x').unwrap();
        assert_eq!(
            span,
            &sources
                .span(
                    sources.file_id("main.gg").unwrap(),
                    offset,
                    offset + 1,
                    crate::ExpansionId::ROOT
                )
                .unwrap()
        );
    }
}
