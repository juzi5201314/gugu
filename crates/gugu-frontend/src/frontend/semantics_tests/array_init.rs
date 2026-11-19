use crate::{Diagnostic, DiagnosticCode, QueryEngine, SourceMap, SourceSnapshot, TargetName};
use gugu_sema::{cfg, hir, modules, names, semantics};

fn check(source: &str, queries: &QueryEngine) -> Result<hir::Validated, Vec<Diagnostic>> {
    let mut sources =
        SourceMap::new(vec![SourceSnapshot::from_str("main.gg", source).unwrap()]).unwrap();
    let resources = crate::testing::resources();
    let builtins: Vec<_> = resources
        .source_blobs()
        .iter()
        .filter(|blob| !gugu_sema::is_probe_builtin_source(&blob.logical_path))
        .collect();
    let builtin_paths = builtins
        .iter()
        .map(|blob| blob.logical_path.clone())
        .collect();
    sources
        .prepend_builtin_prefix(
            builtins
                .iter()
                .map(|blob| SourceSnapshot::from_str(&blob.logical_path, &blob.source).unwrap()),
        )
        .unwrap();
    let prefix = gugu_sema::sema_prefix::SemaPrefix::decode(
        resources.prebuilt().unwrap(),
        builtins.len(),
        &sources,
    )?;
    let cfg = cfg::CfgContext::new(
        TargetName::X86_64Linux,
        [],
        [],
        false,
        false,
        Default::default(),
    );
    let mut modules = modules::parse_modules(&sources, "", &cfg, &builtin_paths)?;
    modules.sort_by_key(|module| (!prefix.covers_module(&module.path), module.path.clone()));
    prefix.verify_installed(&modules)?;
    let names = names::analyze_with_prefix(
        "test/array-init",
        &Default::default(),
        &modules,
        Some(&prefix),
    )?;
    semantics::check(
        &modules,
        &names,
        &sources,
        &cfg,
        None,
        queries,
        &resources.runtime_paths().map(str::to_owned).collect(),
        Some(&prefix),
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
fn concrete_array_init_loop_carries_first_write_plan() {
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
}

#[test]
fn symbolic_array_init_loop_uses_declared_const_identity() {
    let source = "const n: int = 99\nfn f[T, comptime n: int](x: T) [T; n] { let a: [T; n]\n for i in 0..n { a[i] = x }\n a }";
    let hir = check(source, &QueryEngine::new()).unwrap();
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
            } => initializations.first().map(|plan| plan.length.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(length, hir::ArrayLength::Param(_)));
    accepts(source, length);
}

#[test]
fn empty_array_init_loop_is_vacuously_complete() {
    accepts(
        "fn f() [int; 0] { let a: [int; 0]\n for i in 0..0 { a[i] = i }\n a }",
        hir::ArrayLength::Known(0),
    );
}

#[test]
fn complete_array_assignment_needs_no_init_loop_plan() {
    let hir = check(
        "fn f() [int; 2] { let a = [1, 2]\n a }",
        &QueryEngine::new(),
    )
    .unwrap();
    assert!(plans(&hir).is_empty());
}

#[test]
fn initialized_captured_arrays_keep_overwrite_semantics() {
    let hir = check("fn f() [int; 2] { let a = [1, 2]\n let update = fn() { for i in 0..2 { a[i] = i } }\n update()\n a }", &QueryEngine::new()).unwrap();
    assert!(plans(&hir).is_empty());
}

#[test]
fn independent_array_stores_get_independent_init_plans() {
    let hir = check("fn f() ([int; 2], [int; 2]) { let a: [int; 2]\n let b: [int; 2]\n for i in 0..2 { a[i] = i\n b[i] = i + 1 }\n (a, b) }", &QueryEngine::new()).unwrap();
    let plans = plans(&hir);
    assert_eq!(plans.len(), 2);
    assert_ne!(plans[0].array, plans[1].array);
    assert_eq!(plans[0].index, plans[1].index);
}

#[test]
fn array_init_loop_plan_survives_cached_hir() {
    let queries = QueryEngine::new();
    let source = "fn f() [int; 2] { let a: [int; 2]\n for i in 0..2 { a[i] = i }\n a }";
    let cold = check(source, &queries).unwrap();
    let warm = check(source, &queries).unwrap();
    assert_eq!(cold, warm);
}

#[test]
fn cfg_removed_stores_do_not_count_toward_init_loop_proofs() {
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
}

macro_rules! rejects {
    ($($name:ident: $source:literal;)*) => {$ (
        #[test]
        fn $name() {
            let errors = check($source, &QueryEngine::new()).unwrap_err();
            assert!(errors.iter().any(|error| error.code() == DiagnosticCode::InvalidDeclaration
                && error.message() == "读取未初始化的局部槽"), "{errors:?}");
        }
    )*};
}

rejects! {
    only_cfg_removed_store: "fn f() [int; 2] { let a: [int; 2]\n for i in 0..2 { #[cfg(false)] a[i] = i }\n a }";
    reused_array_outside_outer_for: "fn f() { let a: [int; 2]\n for round in 0..2 { for i in 0..2 { a[i] = i } } }";
    reused_array_outside_outer_while: "fn f() { let a: [int; 2]\n let round = 0\n while round < 2 { for i in 0..2 { a[i] = i }\n round += 1 } }";
    maybe_initialized_array: "fn f(flag: bool) { let a: [int; 2]\n if flag { a = [1, 2] }\n for i in 0..2 { a[i] = i } }";
    nonzero_start: "fn f() [int; 3] { let a: [int; 3]\n for i in 1..3 { a[i] = i }\n a }";
    wrong_end: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..2 { a[i] = i }\n a }";
    loop_break: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n break }\n a }";
    loop_continue: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { continue\n a[i] = i }\n a }";
    loop_return: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { if i == 1 { return [0; 3] }\n a[i] = i }\n a }";
    loop_try_exit: "fn f(x: Option[int]) Option[[int; 3]] { let a: [int; 3]\n for i in 0..3 { a[i] = x? }\n Some(a) }";
    rhs_reads_array: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = a[0] }\n a }";
    borrows_array: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n _ = &a }\n a }";
    conditional_store: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { if i == 1 { a[i] = i } }\n a }";
    two_stores: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n a[i] = i }\n a }";
    modified_loop_variable: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n i = 2 }\n a }";
    borrowed_loop_variable: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { let p = &i\n *p = 2\n a[i] = i }\n a }";
    mutating_loop_variable_method: "trait Change { fn change(self: &Self) }\nimpl Change for int { fn change(self: &Self) { *self = 2 } }\nfn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n i.change() }\n a }";
    captured_array: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i] = i\n let read = fn() = a\n _ = read }\n a }";
    shadowed_loop_variable: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { let i = 0\n a[i] = i }\n a }";
    shadowed_array: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { let a = [0; 3]\n a[i] = i }\n a }";
    wrong_symbolic_parameter: "fn f[T, comptime n: int, comptime m: int](x: T) [T; n] { let a: [T; n]\n for i in 0..m { a[i] = x }\n a }";
    shadowed_const_length: "const N: int = 3\nfn f() [int; N] { let a: [int; N]\n let N = 1\n for i in 0..N { a[i] = i }\n a }";
    indirect_index: "fn f() [int; 3] { let a: [int; 3]\n for i in 0..3 { a[i + 0] = i }\n a }";
    arbitrary_index_store: "fn f() [int; 3] { let a: [int; 3]\n a[0] = 1\n a }";
}
