use crate::{CompileRequest, Compiler, TargetName};
pub(super) fn accepts(source: &str) -> bool {
    let c = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    if !c.is_success() {
        eprintln!("{:?}", c.diagnostics().items());
    }
    c.is_success()
}

/// 逐条断言的语义用例：一条断言一次整体编译（约 1 秒），聚合会让单个测试超过 3 秒上限，
/// 因此一行一个测试。`true` 表示该源码必须被接受，`false` 表示必须被拒绝。
#[cfg(feature = "slow-tests")]
macro_rules! accept_cases {
    ($($name:ident: $accept:literal, $source:literal;)*) => {
        $(
            #[test]
            fn $name() {
                assert_eq!(accepts($source), $accept);
            }
        )*
    };
}

#[cfg(feature = "slow-tests")]
pub(super) use accept_cases;
#[cfg(feature = "slow-tests")]
accept_cases! {
    initialization_merges_only_reachable_predecessors_00: true,
        "fn choose(b: bool) int { let x: int\n if b { x = 1 } else { return 2 }\n x }\nfn main() {}";
    initialization_merges_only_reachable_predecessors_01: false,
        "fn choose(b: bool) int { let x: int\n if b { x = 1 }\n x }\nfn main() {}";
    initialization_merges_only_reachable_predecessors_02: true,
        "fn main() { let x = 1\n let x = x + 1\n _ = x }";
    initialization_merges_only_reachable_predecessors_03: false, "fn main() { let x: int\n _ = x }";
}
#[cfg(feature = "slow-tests")]
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
#[cfg(feature = "slow-tests")]
#[test]
fn operators_and_loop_exit_types_are_checked() {
    assert!(!accepts("fn main() { let x = true + false }"));
    assert!(!accepts(
        "fn main() { let x = if true { 1 } else { false } }"
    ));
    assert!(accepts("fn f() int { loop { break 3 } }\nfn main() {}"));
    assert!(!accepts("fn main() { while true { break 3 } }"));
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    initializers_reject_cycles_and_separate_lazy_domains_00: true,
        "static A: int = B + 1\nstatic B: int = 2\nfn main() {}";
    initializers_reject_cycles_and_separate_lazy_domains_01: false,
        "static A: int = B\nstatic B: int = A\nfn main() {}";
    initializers_reject_cycles_and_separate_lazy_domains_02: false,
        "static A: int = f()\nfn f() int = A\nfn main() {}";
    initializers_reject_cycles_and_separate_lazy_domains_03: true,
        "#[coroutine_local] static A: int = A\n#[os_thread_local] static B: int = B\nfn main() {}";
    initializers_reject_cycles_and_separate_lazy_domains_04: false,
        "#[coroutine_local] static A: int = 1\nstatic B: int = A\nfn main() {}";
    initializers_reject_cycles_and_separate_lazy_domains_05: false, "fn main(_: int) {}";
}

#[cfg(feature = "slow-tests")]
#[test]
fn local_static_does_not_capture_automatic_slots() {
    assert!(accepts(
        "fn f() int { static count: int = 0\n count += 1\n count }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn f(arg: int) int { static count: int = arg\n count }\nfn main() {}"
    ));
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    literal_bounds_and_signed_patterns_00: true,
        "fn f(v: i8) int { match v { -128 => 0, _ => 1 } }\nfn main() { let n: i8 = -128\n _ = n }";
    literal_bounds_and_signed_patterns_01: false, "fn main() { let n: i8 = 128 }";
    literal_bounds_and_signed_patterns_02: false, "fn main() { let n: i8 = -129 }";
    literal_bounds_and_signed_patterns_03: true,
        "fn main() { let n: u128 = 340282366920938463463374607431768211455\n _ = n }";
    literal_bounds_and_signed_patterns_04: false,
        "fn main() { let n: u128 = 340282366920938463463374607431768211456 }";
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    try_failures_join_initialization_before_the_expression_00: true,
        "fn f(o: Option[int]) Option[int] { let x = o?\n Some(x) }\nfn main() {}";
    try_failures_join_initialization_before_the_expression_01: false,
        "fn f(o: Option[int]) int { let x: int\n let result: Option[int] = try { let y = o?\n x = y\n y }\n x }\nfn main() {}";
    try_failures_join_initialization_before_the_expression_02: true,
        "fn f(o: Option[int]) Option[int] { let result = try { o? + 1 }\n result }\nfn main() {}";
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    deferred_blocks_read_at_exit_and_calls_capture_at_registration_00: true,
        "fn sink(v: int) {}\nfn main() { let x: int\n defer { sink(x) }\n x = 7 }";
    deferred_blocks_read_at_exit_and_calls_capture_at_registration_01: false,
        "fn sink(v: int) {}\nfn main() { let x: int\n defer sink(x)\n x = 7 }";
    deferred_blocks_read_at_exit_and_calls_capture_at_registration_02: false,
        "fn main() { defer { return } }";
    deferred_blocks_read_at_exit_and_calls_capture_at_registration_03: true,
        "fn main() { defer panic(\"延迟\")\n let value = 3\n _ = value }";
    deferred_blocks_read_at_exit_and_calls_capture_at_registration_04: false,
        "fn sink(v: int) {}\nfn f(b: bool) { let x: int\n defer { sink(x) }\n if b { return }\n x = 7 }\nfn main() {}";
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    places_preserve_references_and_reject_constants_00: true,
        "fn f(p: &int) int { *p = 2\n *p }\nfn main() { let x = 0\n _ = f(&x) }";
    places_preserve_references_and_reject_constants_01: false, "const N = 1\nfn main() { N = 2 }";
    places_preserve_references_and_reject_constants_02: false, "fn main() { let p = &1 }";
    places_preserve_references_and_reject_constants_03: true, "fn main() { let a = [1, 2]\n a[0] = 3 }";
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    numeric_inference_has_no_implicit_scalar_conversion_00: true,
        "fn f(x: i8) bool { 1 == x }\nfn main() { let n = 1\n let x: i8 = n + 1\n _ = x }";
    numeric_inference_has_no_implicit_scalar_conversion_01: false,
        "fn f(x: i8, y: int) int { x + y }\nfn main() {}";
    numeric_inference_has_no_implicit_scalar_conversion_02: true,
        "fn main() { let x = int(1.5)\n let y = byte(x)\n _ = y }";
    numeric_inference_has_no_implicit_scalar_conversion_03: false, "fn main() { let x = bool(1) }";
    numeric_inference_has_no_implicit_scalar_conversion_04: false,
        "struct S {}\nfn main() { _ = (S {},) == (S {},) }";
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    patterns_cover_references_slices_ranges_and_alternatives_00: true,
        "fn f(xs: &[bool]) int { match xs { [] => 0, [true, ..] => 1, [false, ..] => 2 } }\nfn main() {}";
    patterns_cover_references_slices_ranges_and_alternatives_01: true,
        "fn f(c: char) int { match c { '\\0'..'\\u{10ffff}' => 0, '\\u{10ffff}' => 1 } }\nfn main() {}";
    patterns_cover_references_slices_ranges_and_alternatives_02: false,
        "fn f(v: int) int { match v { 2..2 => 0, _ => 1 } }\nfn main() {}";
    patterns_cover_references_slices_ranges_and_alternatives_03: false,
        "fn f(t: (int, int)) { let (x, x) = t }\nfn main() {}";
    patterns_cover_references_slices_ranges_and_alternatives_04: false,
        "fn f(o: Option[int]) int { match o { Some(x) | None => 0 } }\nfn main() {}";
    patterns_cover_references_slices_ranges_and_alternatives_05: true,
        "fn f(v: int) int { match v { x @ 0..10 | x @ 10..20 => x, _ => 0 } }\nfn main() {}";
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    rest_patterns_and_product_coverage_preserve_types_00: false,
        "fn f(xs: &[bool]) &[bool] { let rest @ .. = xs\n rest }\nfn main() {}";
    rest_patterns_and_product_coverage_preserve_types_01: true,
        "fn f(xs: &[bool]) &[bool] { let [rest @ ..] = xs\n rest }\nfn main() {}";
    rest_patterns_and_product_coverage_preserve_types_02: true,
        "fn f(xs: [bool; 4]) [bool; 2] { let [_, rest @ .., _] = xs\n rest }\nfn main() {}";
    rest_patterns_and_product_coverage_preserve_types_03: true,
        "fn f(x: &bool) &bool { match x { same @ true | same @ false => same } }\nfn main() {}";
    rest_patterns_and_product_coverage_preserve_types_04: false,
        "fn f(pair: (bool, bool)) int { match pair { (true, true) => 1, (false, false) => 0 } }\nfn main() {}";
    rest_patterns_and_product_coverage_preserve_types_05: true,
        "fn main() { let n = match 1 { 1 => 2, _ => 3 }\n _ = n }";
    rest_patterns_and_product_coverage_preserve_types_06: true,
        "fn main() { if let Some(1) = Some(2) {} }";
    rest_patterns_and_product_coverage_preserve_types_07: true,
        "fn f(b: bool) { let n: int\n match b { _ if { n = 1\n false } => (), _ => { _ = n } } }\nfn main() {}";
}

#[cfg(feature = "slow-tests")]
#[test]
fn pattern_matrix_fixture_compiles() {
    assert!(accepts(include_str!("../fixtures/patterns.gg")));
}

#[cfg(feature = "slow-tests")]
#[test]
fn let_else_and_refutability_follow_spec() {
    // let-else 接受可驳模式，失败分支发散
    assert!(accepts(
        "fn f(r: Result[int, string]) int { let Ok(v) = r else { return 0 }\n v }\nfn main() {}"
    ));
    // let-else 失败分支必须发散
    let queries = crate::QueryEngine::new();
    let errors = frontend(
        &[(
            "main.gg",
            "fn f(r: Result[int, string]) int { let Ok(v) = r else { 0 }\n v }\nfn main() {}",
        )],
        &queries,
    )
    .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.code() == crate::DiagnosticCode::InvalidLetElse)
    );
    // 不可驳模式不能用于 let-else
    let errors = frontend(
        &[(
            "main.gg",
            "fn f(p: (int, int)) int { let (a, b) = p else { return 0 }\n a }\nfn main() {}",
        )],
        &queries,
    )
    .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.code() == crate::DiagnosticCode::InvalidPattern)
    );
    // 普通 let 拒绝可驳模式
    let errors = frontend(
        &[(
            "main.gg",
            "fn f(o: Option[int]) int { let Some(x) = o\n x }\nfn main() {}",
        )],
        &queries,
    )
    .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.code() == crate::DiagnosticCode::InvalidPattern)
    );
    // let-else 不是 let 链
    assert!(!accepts(
        "fn f(a: Result[int, string], b: Result[int, string]) int { let Ok(x) = a && let Ok(y) = b else { return 0 }\n x }\nfn main() {}"
    ));
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    struct_patterns_follow_record_shape_rules_00: true,
        "struct Point { x: int, y: int }\nfn f(p: Point) int { let Point { x, y } = p\n x + y }\nfn main() {}";
    struct_patterns_follow_record_shape_rules_01: true,
        "struct Point { x: int, y: int }\nfn f(p: Point) int { let Point { x: a, y } = p\n a + y }\nfn main() {}";
    struct_patterns_follow_record_shape_rules_02: true,
        "struct Point { x: int, y: int }\nfn f(p: Point) int { let Point { x, .. } = p\n x }\nfn main() {}";
    // 省略字段必须写 ..
    struct_patterns_follow_record_shape_rules_03: false,
        "struct Point { x: int, y: int }\nfn f(p: Point) int { let Point { x } = p\n x }\nfn main() {}";
    // 字段必须存在
    struct_patterns_follow_record_shape_rules_04: false,
        "struct Point { x: int, y: int }\nfn f(p: Point) int { let Point { z, .. } = p\n z }\nfn main() {}";
    // 同一字段不能出现两次
    struct_patterns_follow_record_shape_rules_05: false,
        "struct Point { x: int, y: int }\nfn f(p: Point) int { let Point { x, x } = p\n x }\nfn main() {}";
}

#[cfg(feature = "slow-tests")]
#[test]
fn struct_patterns_respect_private_fields() {
    let queries = crate::QueryEngine::new();
    let lib =
        "pub struct Counter { value: int }\npub fn make(v: int) Counter = Counter { value: v }";
    // 模块外不能点名私有字段
    let errors = frontend(
        &[
            (
                "main.gg",
                "use lib.{Counter, make}\nfn peek(c: Counter) int { let Counter { value } = c\n value }\nfn main() { _ = peek(make(1)) }",
            ),
            ("lib.gg", lib),
        ],
        &queries,
    )
    .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.code() == crate::DiagnosticCode::InvalidPattern)
    );
    // 但可以由 .. 整体忽略
    assert!(frontend(
        &[
            (
                "main.gg",
                "use lib.{Counter, make}\nfn peek(c: Counter) int { let Counter { .. } = c\n 0 }\nfn main() { _ = peek(make(1)) }",
            ),
            ("lib.gg", lib),
        ],
        &queries,
    )
    .is_ok());
}

#[cfg(feature = "slow-tests")]
#[test]
fn array_or_and_range_patterns_validate_shapes() {
    // 固定数组精确模式长度必须一致，rest 可以吃掉剩余
    assert!(!accepts(
        "fn f(xs: [int; 4]) int { let [a, b] = xs\n a }\nfn main() {}"
    ));
    assert!(accepts(
        "fn f(xs: [int; 4]) int { let [a, b, ..] = xs\n a }\nfn main() {}"
    ));
    // or 两侧必须绑定同一组名字
    assert!(!accepts(
        "fn f(o: Option[int]) int { match o { Some(x) | Some(y) => 0, None => 1 } }\nfn main() {}"
    ));
    // string 字面量不进模式
    assert!(!accepts(
        "fn f(s: string) int { match s { \"hi\" => 0, _ => 1 } }\nfn main() {}"
    ));
    // 范围端点类型必须与被匹配类型相同
    assert!(!accepts(
        "fn f(c: char) int { match c { 0..'a' => 0, _ => 1 } }\nfn main() {}"
    ));
    // 下界必须小于上界
    assert!(!accepts(
        "fn f(v: int) int { match v { 3..2 => 0, _ => 1 } }\nfn main() {}"
    ));
    // 范围模式没有 ..=
    assert!(!accepts(
        "fn f(v: int) int { match v { 0..=10 => 0, _ => 1 } }\nfn main() {}"
    ));
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    condition_bindings_and_loop_initialization_follow_execution_00: true,
        "fn f(o: Option[int]) int { if let Some(x) = o && x > 0 { x } else { 0 } }\nfn main() {}";
    condition_bindings_and_loop_initialization_follow_execution_01: false,
        "fn main() { if let x = 1 {} }";
    condition_bindings_and_loop_initialization_follow_execution_02: false,
        "fn f(o: Option[int]) { if let Some(x) = o {} else { _ = x } }\nfn main() {}";
    condition_bindings_and_loop_initialization_follow_execution_03: true,
        "fn main() { let x: int\n while { x = 1\n false } {}\n _ = x }";
    condition_bindings_and_loop_initialization_follow_execution_04: false,
        "fn main() { let x: int\n while false { x = 1 }\n _ = x }";
}

#[cfg(feature = "slow-tests")]
accept_cases! {
    function_defer_outlives_nested_blocks_00: true,
        "fn sink(x: int) {}\nfn main() { let x: int\n { defer ret { sink(x) } }\n x = 1 }";
    function_defer_outlives_nested_blocks_01: false,
        "fn sink(x: int) {}\nfn main() { let x: int\n { defer ret { sink(x) } } }";
    function_defer_outlives_nested_blocks_02: false,
        "fn main() { defer { let x: bool = 1 }\n loop {} }";
    function_defer_outlives_nested_blocks_03: true,
        "fn sink(x: int) {}\nfn f(flag: bool) { let x: int\n if flag { defer ret { sink(x) }\n x = 1 } }\nfn main() {}";
    function_defer_outlives_nested_blocks_04: true,
        "fn sink(x: int) {}\nfn f(flag: bool) { if flag { let x = 1\n defer ret { sink(x) } } }\nfn main() {}";
}

pub(super) fn frontend(
    sources: &[(&str, &str)],
    queries: &crate::QueryEngine,
) -> Result<super::super::FrontendOutput, Vec<crate::Diagnostic>> {
    let mut sources = crate::SourceMap::new(
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
            source_map: &mut sources,
            entry: "main.gg",
            source_root: "",
            package_identity: "tests/semantics@1.0.0",
            require_main: true,
            cfg: &cfg,
            external_packages: &Default::default(),
        },
        queries,
        crate::testing::resources(),
    )
}

#[cfg(feature = "slow-tests")]
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
        // 运行时源自身也带 `pub const`（它们是初始化实例，但不是本用例的观察对象）。
        .filter(|(name, _)| name.len() == 1)
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

#[cfg(feature = "slow-tests")]
#[test]
fn cached_constraint_failures_preserve_notes_and_current_source_table() {
    let source = "trait Marker {}\nstruct Box[T] { value: T }\nimpl[T] Marker for Box[T] {}\nimpl !Marker for Box[bool] {}\nfn use_marker[T: Marker](value: T) {}\nfn main() { let value: Box[bool] = Box { value: true }\n use_marker(value) }";
    let queries = crate::QueryEngine::new();
    let cold = frontend(&[("main.gg", source)], &queries).unwrap_err();
    let warm = frontend(&[("main.gg", source)], &queries).unwrap_err();
    assert!(
        cold.iter()
            .any(|diagnostic| diagnostic.severity() == crate::Severity::Note)
    );
    assert_eq!(
        cold.iter()
            .map(crate::Diagnostic::render_text)
            .collect::<Vec<_>>(),
        warm.iter()
            .map(crate::Diagnostic::render_text)
            .collect::<Vec<_>>()
    );
    assert_ne!(
        cold[0].span().unwrap().table(),
        warm[0].span().unwrap().table()
    );
}

#[cfg(feature = "slow-tests")]
#[test]
fn runtime_checks_survive_queries_and_reach_the_backend_plan() {
    use super::output::CheckKind;
    let queries = crate::QueryEngine::new();
    let source = "fn f(x: int, n: u8, a: &[int], s: string, d: float) { _ = x / 0\n _ = x << n\n _ = a[x]\n _ = s[x..]\n _ = int(d)\n _ = char(x)\n unsafe { _ = a[x] } }\nfn main() { let data = [1, 2]\n f(1, 2, &data, \"s\", 1.0) }";
    let cold = frontend(&[("main.gg", source)], &queries).unwrap();
    let warm = frontend(&[("main.gg", source)], &queries).unwrap();
    assert_eq!(cold.semantics, warm.semantics);
    // 证明链冷热一致；无符号移位量的类型边界可证明非负，未知切片边界仍需检查。
    assert_eq!(cold.analysis, warm.analysis);
    assert_eq!(cold.hir.module().owners, warm.hir.module().owners);
    let user_module = warm
        .modules
        .iter()
        .position(|module| {
            warm.hir.module().sources
                [usize::try_from(module.file.source.as_u32()).expect("source下标")]
            .path
                == "main.gg"
        })
        .expect("被测输入模块");
    let checks: Vec<_> = warm
        .semantics
        .bodies
        .iter()
        .filter(|body| body.definition.module == user_module)
        .flat_map(|body| body.runtime_checks.iter().map(|check| &check.kind))
        .collect();
    let mut proofs = Vec::new();
    for (owner_index, owner) in warm.hir.module().owners.iter().enumerate() {
        for check in &owner.checks {
            let location = &owner.expressions[check.expression.index()].location;
            if warm.hir.module().sources[usize::try_from(location.source).expect("source下标")].path
                != "main.gg"
            {
                continue;
            }
            let key = crate::frontend::analysis::RuntimeCheckKey {
                owner_index: owner_index as u32,
                expression: check.expression,
                kind: check.kind.clone(),
            };
            proofs.push(warm.analysis.proof_status(&key));
        }
    }
    assert_eq!(proofs.len(), checks.len());
    assert_eq!(proofs[1], crate::frontend::analysis::ProofStatus::Proved);
    assert!(
        proofs[2..]
            .iter()
            .all(|proof| *proof == crate::frontend::analysis::ProofStatus::Unknown)
    );
    assert_eq!(proofs[0], crate::frontend::analysis::ProofStatus::Disproved);
    let [
        CheckKind::IntegerDivision {
            ty: division_ty,
            divisor,
        },
        CheckKind::Shift {
            ty: shift_ty,
            amount,
        },
        CheckKind::Bounds { slice: false },
        CheckKind::Bounds { slice: true },
        CheckKind::Utf8Boundary,
        CheckKind::FloatToInt {
            signed: true,
            bits: 64,
            value: float,
        },
        CheckKind::UnicodeScalar { value: scalar },
    ] = checks.as_slice()
    else {
        panic!("所有有风险操作都必须保留对应检查，unsafe 下标不生成边界检查");
    };
    assert_eq!(division_ty, &super::model::Ty::int());
    assert_eq!(
        shift_ty,
        &super::model::Ty::Int {
            signed: false,
            bits: 8
        },
        "移位检查携带移位量类型，用于判定有符号性"
    );
    let model = super::model::Model::new(
        &warm.modules,
        &warm.names,
        &std::collections::BTreeSet::new(),
    )
    .unwrap();
    assert_eq!(model.constant_int(user_module, *divisor).unwrap(), 0);
    let operand_type = |expression| {
        &warm
            .semantics
            .bodies
            .iter()
            .filter(|body| body.definition.module == user_module)
            .flat_map(|body| &body.expressions)
            .find(|(id, _)| *id == expression)
            .unwrap()
            .1
    };
    assert_eq!(
        operand_type(*amount),
        &super::model::Ty::Int {
            signed: false,
            bits: 8
        }
    );
    assert_eq!(operand_type(*float), &super::model::Ty::Float(64));
    assert_eq!(operand_type(*scalar), &super::model::Ty::int());
    let compiler = Compiler::shared();
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

#[cfg(feature = "slow-tests")]
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
