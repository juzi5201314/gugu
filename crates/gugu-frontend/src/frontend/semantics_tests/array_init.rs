use crate::{Diagnostic, DiagnosticCode, QueryEngine, SourceMap, SourceSnapshot, TargetName};
use gugu_sema::{cfg, hir, modules, names, semantics};
use std::collections::BTreeSet;
use std::sync::LazyLock;

struct BuiltinContext {
    snapshots: Vec<SourceSnapshot>,
    paths: BTreeSet<String>,
    runtime_paths: BTreeSet<String>,
}

fn builtin_context() -> &'static BuiltinContext {
    static CONTEXT: LazyLock<BuiltinContext> = LazyLock::new(|| {
        let resources = crate::testing::resources();
        let builtins: Vec<_> = resources
            .source_blobs()
            .iter()
            .filter(|blob| !gugu_sema::is_probe_builtin_source(&blob.logical_path))
            .collect();
        let snapshots = builtins
            .iter()
            .map(|blob| SourceSnapshot::from_str(&blob.logical_path, &blob.source).unwrap())
            .collect();
        let paths = builtins
            .iter()
            .map(|blob| blob.logical_path.clone())
            .collect();
        let runtime_paths = resources.runtime_paths().map(str::to_owned).collect();
        BuiltinContext {
            snapshots,
            paths,
            runtime_paths,
        }
    });
    &CONTEXT
}

fn check(source: &str, queries: &QueryEngine) -> Result<hir::Validated, Vec<Diagnostic>> {
    let mut sources =
        SourceMap::new(vec![SourceSnapshot::from_str("main.gg", source).unwrap()]).unwrap();
    let resources = crate::testing::resources();
    let builtins = builtin_context();
    sources
        .prepend_builtin_prefix(builtins.snapshots.iter().cloned())
        .unwrap();
    let prefix = gugu_gir::BuiltinPrefix::decode(
        resources.prebuilt(),
        resources.target_name(),
        builtins.snapshots.len(),
        &sources,
    )?;
    let sema_prefix = prefix.sema();
    let cfg = cfg::CfgContext::new(
        TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    let mut modules = modules::parse_modules(&sources, "", &cfg, &builtins.paths)?;
    modules.sort_by_key(|module| {
        (
            !sema_prefix.covers_module(&module.path),
            module.path.clone(),
        )
    });
    sema_prefix.verify_installed(&modules)?;
    let names = names::analyze_with_prefix(
        "test/array-init",
        &Default::default(),
        &modules,
        Some(sema_prefix),
    )?;
    semantics::check(
        &modules,
        &names,
        &sources,
        &cfg,
        None,
        queries,
        &builtins.runtime_paths,
        Some(sema_prefix),
        None,
    )
    .map(|world| world.hir)
}

fn accepts(source: &str, length: hir::ArrayLength) -> hir::Validated {
    let hir = check(source, &QueryEngine::new()).unwrap();
    let plans = plans(&hir);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].length, length);
    hir
}

fn plans(hir: &hir::Validated) -> Vec<&hir::ArrayInitLoop> {
    hir.module()
        .owners
        .iter()
        .flat_map(|owner| &owner.expressions)
        .filter_map(|expression| match &expression.kind {
            hir::ExprKind::For {
                initializations, ..
            } => Some(initializations.as_slice()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn array_init_loop_positive_contracts() {
    // 1. 具体长度数组初始化循环携带首写计划，且非法局部槽被冻结校验拒绝。
    let hir = accepts(
        "const N: int = 3\nconst M: int = 3\nfn f() [int; N] { let a: [int; N]\n for i in 0..M { let x = i + 1\n a[i] = x\n _ = x }\n a }",
        hir::ArrayLength::Known(3),
    );
    let mut invalid = hir.module().clone();
    for owner in &mut invalid.owners {
        for expression in &mut owner.expressions {
            if let hir::ExprKind::For {
                initializations, ..
            } = &mut expression.kind
            {
                for plan in initializations {
                    plan.array = hir::LocalId(u32::MAX);
                }
            }
        }
    }
    assert!(hir::Validated::freeze(invalid).is_err());

    // 2. 符号长度数组初始化循环使用声明常量参数身份。
    let symbolic = "const n: int = 99\nfn f[T, comptime n: int](x: T) [T; n] { let a: [T; n]\n for i in 0..n { a[i] = x }\n a }";
    let hir = check(symbolic, &QueryEngine::new()).unwrap();
    let owner = hir
        .module()
        .owners
        .iter()
        .find(|owner| hir.module().definitions[owner.definition.index()].name == "f")
        .unwrap();
    let length = owner
        .expressions
        .iter()
        .find_map(|expression| match &expression.kind {
            hir::ExprKind::For {
                initializations, ..
            } => initializations.first().map(|plan| plan.length),
            _ => None,
        })
        .unwrap();
    assert!(matches!(length, hir::ArrayLength::Param(_)));
    let symbolic_plans = plans(&hir);
    assert_eq!(symbolic_plans.len(), 1);
    assert_eq!(symbolic_plans[0].length, length);

    // 3. 空数组、完整赋值、已初始化捕获、多数组独立计划与缓存复用。
    accepts(
        "fn f() [int; 0] { let a: [int; 0]\n for i in 0..0 { a[i] = i }\n a }",
        hir::ArrayLength::Known(0),
    );
    assert!(
        plans(
            &check(
                "fn f() [int; 2] { let a = [1, 2]\n a }",
                &QueryEngine::new(),
            )
            .unwrap()
        )
        .is_empty()
    );
    assert!(
        plans(
            &check(
                "fn f() [int; 2] { let a = [1, 2]\n let update = fn() { for i in 0..2 { a[i] = i } }\n update()\n a }",
                &QueryEngine::new(),
            )
            .unwrap()
        )
        .is_empty()
    );
    let independent = check(
        "fn f() ([int; 2], [int; 2]) { let a: [int; 2]\n let b: [int; 2]\n for i in 0..2 { a[i] = i\n b[i] = i + 1 }\n (a, b) }",
        &QueryEngine::new(),
    )
    .unwrap();
    let independent_plans = plans(&independent);
    assert_eq!(independent_plans.len(), 2);
    assert_ne!(independent_plans[0].array, independent_plans[1].array);
    assert_eq!(independent_plans[0].index, independent_plans[1].index);
}

#[test]
fn array_init_loop_positive_cache_cfg_and_diverging_loops() {
    let queries = QueryEngine::new();
    let cached_source = "fn f() [int; 2] { let a: [int; 2]\n for i in 0..2 { a[i] = i }\n a }";
    let cold = check(cached_source, &queries).unwrap();
    let warm = check(cached_source, &queries).unwrap();
    assert_eq!(cold, warm);

    // 4. cfg 裁除写入与发散前序循环不干扰初始化循环证明。
    for statements in [
        "a[i] = i\n #[cfg(false)] a[i] = 9",
        "#[cfg(false)] a[i] = 9\n a[i] = i",
    ] {
        accepts(
            &format!(
                "fn f() [int; 2] {{ let a: [int; 2]\n for i in 0..2 {{ {statements} }}\n a }}"
            ),
            hir::ArrayLength::Known(2),
        );
    }
    for earlier_loop in [
        "for j in 0..2 { if flag { a = [1, 2]\n return a } }",
        "while flag { a = [1, 2]\n return a }",
        "for j in 0..2 { if flag { loop { a = [1, 2]\n return a } } }",
    ] {
        accepts(
            &format!(
                "fn f(flag: bool) [int; 2] {{ let a: [int; 2]\n {earlier_loop}\n for i in 0..2 {{ a[i] = i }}\n a }}"
            ),
            hir::ArrayLength::Known(2),
        );
    }
}

fn assert_uninit_local_errors(cases: &[(&str, &str)]) {
    for &(name, source) in cases {
        let errors = check(source, &QueryEngine::new())
            .expect_err(&format!("{name} 必须触发未初始化局部槽错误"));
        assert!(
            errors
                .iter()
                .any(|error| error.code() == DiagnosticCode::InvalidDeclaration
                    && error.message() == "读取未初始化的局部槽"),
            "{name}: {errors:?}"
        );
    }
}

#[test]
fn array_init_loop_negative_control_and_reuse_diagnostics() {
    assert_uninit_local_errors(&[
        (
            "previous_loop_fallthrough_write",
            "fn f(flag: bool) { let a: [int; 2]\n for j in 0..2 { if flag { a = [1, 2] } }\n for i in 0..2 { a[i] = i } }",
        ),
        (
            "previous_loop_continue_write",
            "fn f(flag: bool) { let a: [int; 2]\n for j in 0..2 { if flag { a = [1, 2]\n continue } }\n for i in 0..2 { a[i] = i } }",
        ),
        (
            "previous_loop_break_write",
            "fn f(flag: bool) { let a: [int; 2]\n for j in 0..2 { if flag { a = [1, 2]\n break } }\n for i in 0..2 { a[i] = i } }",
        ),
        (
            "only_cfg_removed_store",
            "fn f() [int; 2] { let a: [int; 2]\n for i in 0..2 { #[cfg(false)] a[i] = i }\n a }",
        ),
        (
            "reused_array_outside_outer_for",
            "fn f() { let a: [int; 2]\n for round in 0..2 { for i in 0..2 { a[i] = i } } }",
        ),
        (
            "reused_array_outside_outer_while",
            "fn f() { let a: [int; 2]\n let round = 0\n while round < 2 { for i in 0..2 { a[i] = i }\n round += 1 } }",
        ),
        (
            "maybe_initialized_array",
            "fn f(flag: bool) { let a: [int; 2]\n if flag { a = [1, 2] }\n for i in 0..2 { a[i] = i } }",
        ),
        (
            "nonzero_start",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 1..3 { a[i] = i }\n a }",
        ),
        (
            "wrong_end",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..2 { a[i] = i }\n a }",
        ),
        (
            "loop_break",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n break }\n a }",
        ),
        (
            "loop_continue",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { continue\n a[i] = i }\n a }",
        ),
        (
            "loop_return",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { if i == 1 { return [0; 3] }\n a[i] = i }\n a }",
        ),
        (
            "loop_try_exit",
            "fn f(x: Option[int]) Option[[int; 3]] { let a: [int; 3]\n for i in 0..3 { a[i] = x? }\n Some(a) }",
        ),
    ]);
}

#[test]
fn array_init_loop_negative_alias_and_shadow_diagnostics() {
    assert_uninit_local_errors(&[
        (
            "rhs_reads_array",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = a[0] }\n a }",
        ),
        (
            "borrows_array",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n _ = &a }\n a }",
        ),
        (
            "conditional_store",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { if i == 1 { a[i] = i } }\n a }",
        ),
        (
            "two_stores",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n a[i] = i }\n a }",
        ),
        (
            "modified_loop_variable",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n i = 2 }\n a }",
        ),
        (
            "borrowed_loop_variable",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { let p = &i\n *p = 2\n a[i] = i }\n a }",
        ),
        (
            "mutating_loop_variable_method",
            "trait Change { fn change(self: &Self) }\nimpl Change for int { fn change(self: &Self) { *self = 2 } }\nfn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n i.change() }\n a }",
        ),
        (
            "captured_array",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n let read = fn() = a\n _ = read }\n a }",
        ),
        (
            "shadowed_loop_variable",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { let i = 0\n a[i] = i }\n a }",
        ),
        (
            "shadowed_array",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { let a = [0; 3]\n a[i] = i }\n a }",
        ),
        (
            "wrong_symbolic_parameter",
            "fn f[T, comptime n: int, comptime m: int](x: T) [T; n] { let a: [T; n]\n for i in 0..m { a[i] = x }\n a }",
        ),
        (
            "shadowed_const_length",
            "const N: int = 3\nfn f() [int; N] { let a: [int; N]\n let N = 1\n for i in 0..N { a[i] = i }\n a }",
        ),
        (
            "indirect_index",
            "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i + 0] = i }\n a }",
        ),
        (
            "arbitrary_index_store",
            "fn f() [int; 3] { let a: [int; 3]\n a[0] = 1\n a }",
        ),
    ]);
}
