use super::{CompileRequest, Compiler, TargetName, printing::output};

#[test]
fn dependent_value_parameter_instances_use_values_and_survive_cache_hits() {
    let compile = |n| {
        let source = format!(
            "fn f(comptime n: int) [int; n] {{ [0; n] }}\nfn main() {{ _ = f(2)\n _ = f({n}) }}"
        );
        let compiled = Compiler::shared().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(
            compiled.is_success(),
            "{:?}",
            compiled.diagnostics().items()
        );
        compiled
    };
    let same = compile(2);
    let distinct = compile(3);
    let warm = compile(3);
    assert_eq!(
        same.image_plan().unwrap().mono_instance_count() + 1,
        distinct.image_plan().unwrap().mono_instance_count()
    );
    assert_eq!(
        distinct.image_plan().unwrap().mono_graph_fingerprint(),
        warm.image_plan().unwrap().mono_graph_fingerprint()
    );
}

#[test]
fn dependent_value_parameter_cannot_be_mutated_or_borrowed() {
    for statement in ["n = 1", "_ = &n"] {
        let source = format!(
            "fn f(comptime n: int) [int; n] {{ {statement}\n [0; n] }}\nfn main() {{ _ = f(2) }}"
        );
        let compiled = Compiler::shared().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(!compiled.is_success());
        assert!(
            compiled
                .diagnostics()
                .items()
                .iter()
                .any(|diagnostic| diagnostic.message().contains("不能赋值或借用")),
            "{:?}",
            compiled.diagnostics().items()
        );
    }
}

#[test]
fn dependent_value_parameters_execute_runtime_and_early_arrays() {
    let bytes: Vec<_> = output(
        r#"
use std.io.{println}
fn repeat[T](comptime n: int, x: T) [T; n] {
    let a: [T; n]
    for i in 0..n { a[i] = x }
    a
}
const A: [int; 2] = repeat(2, 7)
const B: [int; 3] = repeat::[int](3, 8)
fn main() {
    let a = repeat(2, 5)
    let b = repeat::[int](3, 6)
    let empty: [int; 0] = repeat(0, 9)
    _ = empty
    println(a[1])
    println(b[2])
    println(A[1])
    println(B[2])
}
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(bytes, b"5\n6\n7\n8\n");
}

#[test]
fn dependent_value_parameters_reject_nonconstant_arguments() {
    let compiled = Compiler::shared().compile(CompileRequest::single_file("main.gg",
        "fn repeat(comptime n: int) [int; n] { [0; n] }\nfn f(n: int) { _ = repeat(n) }\nfn main() {}", TargetName::X86_64Linux));
    assert!(!compiled.is_success());
    assert!(
        compiled
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.message().contains("需要编译期已知值")),
        "{:?}",
        compiled.diagnostics().items()
    );
}

#[test]
fn dependent_value_parameters_reject_type_arithmetic() {
    let compiled = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        "fn f(comptime n: int) [int; n + 1] { [0; n + 1] }\nfn main() { _ = f(2) }",
        TargetName::X86_64Linux,
    ));
    assert!(!compiled.is_success());
    assert!(
        compiled
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.message().contains("算术")),
        "{:?}",
        compiled.diagnostics().items()
    );
}

#[test]
fn imported_result_constructor_names_do_not_evaluate_function_const_arguments() {
    let compiled = compile_imported_result_constructors(
        r#"
use helpers.{shadow as Ok, shadow as Err, genuine as User}
const ANSWER: int = comptime source {
    let n = 0
    let ok = Ok::[{ n += 1
        n
    }](4)
    let err = Err::[{ n += 10
        n
    }](5)
    let left = match ok { Ok(value) => value, Err(_) => 0 }
    let right = match err { Err(value) => value, Ok(_) => 0 }
    if left != 4 || right != 5 || n != 0 { panic("constructor evaluated imported function const arguments") }
    let called = User::[{ n += 2
        n
    }](3)
    if called != 5 || n != 2 { panic("user function must evaluate its const arguments") }
    std.syntax.parse_source("450")
}
fn main() { _ = ANSWER
    let ordinary: Result[int, int] = Ok::[panic("generic argument must not run")](7)
    _ = ordinary
}
"#,
    );
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
}

#[test]
fn imported_result_constructor_names_validate_constructor_arity_first() {
    for constructor in ["Ok", "Err"] {
        for arguments in ["", "1, 2"] {
            let source = format!(
                r#"
use helpers.{{shadow as {constructor}}}
const ANSWER: int = comptime source {{
    _ = {constructor}::[panic("function const argument must not run")]({arguments})
    std.syntax.parse_source("0")
}}
fn main() {{ _ = ANSWER }}
"#
            );
            let compiled = compile_imported_result_constructors(&source);
            assert!(!compiled.is_success());
            assert!(
                compiled
                    .diagnostics()
                    .items()
                    .iter()
                    .any(|diagnostic| diagnostic.message().contains("构造器实参数量不符")),
                "{:?}",
                compiled.diagnostics().items()
            );
        }
    }
}

fn compile_imported_result_constructors(source: &str) -> crate::Compilation {
    let root = tempfile::tempdir().unwrap();
    let src = root.path().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(
        root.path().join("gugu.toml"),
        "[package]\nname = \"constructors\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    std::fs::write(src.join("main.gg"), source).unwrap();
    std::fs::write(src.join("helpers.gg"), "pub fn shadow[comptime N: int](value: int) int { panic(\"imported function must not run\") }\npub fn genuine[comptime N: int](value: int) int { N + value }\n").unwrap();
    let project = crate::Project::discover(root.path()).unwrap();
    let package = project.current_package().unwrap();
    let target = package
        .targets()
        .iter()
        .find(|target| target.kind() == crate::TargetKind::Bin)
        .unwrap();
    Compiler::shared().compile(CompileRequest::project_target(
        package,
        target,
        "constructors@0.0.0 (path+.)",
        TargetName::X86_64Linux,
        vec!["default".to_owned()],
        Default::default(),
    ))
}

#[test]
fn early_const_arguments_run_before_value_arguments() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
fn combine[comptime N: int](value: int) int { N * 10 + value }
const ANSWER: int = comptime source {
    let n = 0
    let value = combine::[{ n = 1
        n
    }]({ n = 2
        n
    })
    std.syntax.parse_source(f"{value} * 10 + {n}")
}
fn main() { println(ANSWER) }
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"122\n");
}

#[test]
fn invalid_early_const_arguments_fail_before_value_arguments() {
    for argument in ["-1", "true"] {
        let source = format!(
            r#"
fn choose[comptime N: int](value: int) int {{ N }}
const ANSWER: int = comptime source {{
    let value = choose::[{argument}](panic("value argument evaluated first"))
    std.syntax.parse_source(f"{{value}}")
}}
fn main() {{ _ = ANSWER }}
"#
        );
        let compiled = Compiler::shared().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(!compiled.is_success());
        assert!(
            compiled
                .diagnostics()
                .items()
                .iter()
                .any(|diagnostic| diagnostic
                    .message()
                    .contains("comptime 泛型实参需要非负 int 常量")),
            "{:?}",
            compiled.diagnostics().items()
        );
    }
}

#[test]
fn explicit_const_argument_assignments_update_live_caller_locals() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
fn number[comptime N: int]() int { N }
fn increment(value: int) int { value + 1 }
fn nested() int { number::[increment(number::[2]())]() }
const TOP_LEVEL: int = number::[increment(2)]()
const NESTED: int = nested()
const ANSWER: int = comptime source {
    let n = 1
    let value = number::[{ n += 1
        n
    }]()
    std.syntax.parse_source(f"{value} * 10 + {n}")
}
fn main() { println(ANSWER)
    println(TOP_LEVEL)
    println(NESTED)
}
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"22\n3\n3\n");
}

#[test]
fn forwarded_const_parameters_shadow_module_constants_in_all_phases() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
const N: int = 9
fn number[comptime N: int]() int { N }
fn forward[comptime N: int]() int { number::[N]() }
const EARLY: int = forward::[3]()
fn main() { println(forward::[3]())
    println(EARLY)
}
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"3\n3\n");
}

#[test]
fn source_macros_reject_surplus_explicit_generic_arguments() {
    for call in [
        "number::[2, 3]()",
        "typed::[int, 2, bool]()",
        "plain::[2]()",
        "typed::[int]()",
    ] {
        let source = format!(
            r#"
fn number[comptime N: int]() int {{ N }}
fn typed[T, comptime N: int]() int {{ N }}
fn plain() int {{ 1 }}
const ANSWER: int = comptime source {{
    let value = {call}
    std.syntax.parse_source(f"{{value}}")
}}
fn main() {{ _ = ANSWER }}
"#
        );
        let compiled = Compiler::shared().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(
            !compiled.is_success(),
            "surplus generic arguments were accepted: {call}"
        );
        assert!(
            compiled
                .diagnostics()
                .items()
                .iter()
                .any(|diagnostic| diagnostic.message().contains("comptime 泛型实参数量不符")),
            "{:?}",
            compiled.diagnostics().items()
        );
    }
}

#[test]
fn early_array_arguments_match_explicit_lengths_during_type_formation() {
    for (parameter, argument, count) in [
        ("[T; N]", "[1, 2]", 3),
        ("[T; N]", "[0; 0]", 1),
        ("[[T; N]; 1]", "[[1]]", 2),
        ("([T; N], bool)", "([1], true)", 2),
        ("[T; 2]", "[1]", 2),
    ] {
        let source = format!(
            "fn length[T, comptime N: int](a: {parameter}) int {{ N }}\nstruct Storage {{ data: [int; length::[int, {count}]({argument})] }}\nfn main() {{}}"
        );
        let compiled = Compiler::shared().compile(CompileRequest::single_file(
            "main.gg",
            &source,
            TargetName::X86_64Linux,
        ));
        assert!(
            !compiled.is_success(),
            "ill-typed array-length call was accepted"
        );
        assert!(
            compiled
                .diagnostics()
                .items()
                .iter()
                .any(|diagnostic| diagnostic
                    .message()
                    .contains("comptime 数组实参长度与参数长度不一致")),
            "{:?}",
            compiled.diagnostics().items()
        );
    }
}

#[test]
fn source_macro_const_arguments_preserve_script_locals_and_comptime_parameters() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
const M: int = 1
fn number[comptime N: int]() int { N }
fn forward(comptime M: int) int { number::[M]() }
const ANSWER: int = comptime source {
    let M = 3
    let local = number::[M]()
    let parameter = forward(4)
    std.syntax.parse_source(f"{local} * 10 + {parameter}")
}
fn main() { println(ANSWER) }
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"34\n");
}

#[test]
fn qualified_repeat_lengths_are_not_bare_const_parameters() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
use std.test as Mod
fn length[comptime Mod: int](x: int) int {
    let values = [x; Mod.FLAG_IGNORE]
    values.len()
}
fn main() { println(length::[4](7)) }
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"1\n");
}

fn requires_explicit_early_const_arguments(source: &str) {
    let compiled = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic
                .message()
                .contains("早期求值 const 泛型调用需要显式 ::[...] const 实参")),
        "{:?}",
        compiled.diagnostics().items()
    );
}

#[test]
fn early_const_calls_do_not_reform_parameter_lengths() {
    let source = "const SIZE: int = length([0])\nfn length[comptime N: int](a: [int; SIZE]) int { N }\nfn main() { _ = SIZE }";
    requires_explicit_early_const_arguments(source);
    let explicit = source.replace("length([0])", "length::[1]([0])");
    let compiled = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        &explicit,
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
}

#[test]
fn early_explicit_const_arguments_ignore_ordinary_caller_parameters() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
const M: int = 3
fn number[comptime N: int]() int { N }
fn caller(M: int) int { number::[M]() }
fn expression(M: int) int { number::[M + 1]() }
const ANSWER: int = caller(7)
const EXPRESSION: int = expression(7)
fn main() { println(ANSWER)
    println(EXPRESSION)
}
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"3\n4\n");
}

#[test]
fn early_const_calls_do_not_inspect_inactive_enum_fields() {
    let source = r#"
use std.io.{println}
enum Choice { A { data: [int; 2] }, B { data: [int; 3] } }
fn number[comptime N: int](choice: Choice) int { N }
const ANSWER: int = number::[4](Choice::A { data: [1, 2] })
fn main() { println(ANSWER) }
"#;
    let actual: Vec<_> = output(source, &[])
        .into_iter()
        .flat_map(|(_, bytes)| bytes)
        .collect();
    assert_eq!(actual, b"4\n");
    requires_explicit_early_const_arguments(&source.replace("number::[4]", "number"));
}

#[test]
fn early_empty_outer_arrays_require_explicit_inner_lengths() {
    let source = r#"
use std.io.{println}
fn inner[comptime N: int](a: [[int; N]; 0]) int { N }
const ANSWER: int = inner::[3]([[1; 3]; 0])
fn main() { println(ANSWER) }
"#;
    let actual: Vec<_> = output(source, &[])
        .into_iter()
        .flat_map(|(_, bytes)| bytes)
        .collect();
    assert_eq!(actual, b"3\n");
    requires_explicit_early_const_arguments(&source.replace("inner::[3]", "inner"));
    requires_explicit_early_const_arguments(
        "const N: int = 9\nfn length[comptime N: int](a: [int; N]) int { N }\nconst ANSWER: int = length([1, 2])\nfn main() { _ = ANSWER }",
    );
}

#[test]
fn early_explicit_const_arguments_allow_empty_and_multiple_type_pack_elements() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
fn number[comptime N: int, Ts...]() int { N }
fn typed[T, comptime N: int, Ts...]() int { N }
const EMPTY: int = number::[2]()
const MULTIPLE: int = number::[3, int, bool]()
const TYPED: int = typed::[bool, 4, int]()
fn main() { println(EMPTY)
    println(MULTIPLE)
    println(TYPED)
}
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"2\n3\n4\n");
    requires_explicit_early_const_arguments(
        "fn ambiguous[Ts..., comptime N: int]() int { N }\nconst ANSWER: int = ambiguous::[2, int]()\nfn main() { _ = ANSWER }",
    );
}

#[test]
fn late_repeats_use_concrete_instance_lengths() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
fn filled[comptime N: int](x: int) [int; N] { [x; N] }
const THREE: [int; 3] = filled::[3](type_id_count())
const EMPTY: [int; 0] = filled::[0](type_id_count())
fn main() {
    let three = THREE
    let empty = EMPTY
    println(three.len())
    println(empty.len())
    println(three[0] > 0)
}
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"3\n0\ntrue\n");
}

#[test]
fn explicit_const_method_arguments_survive_dispatch() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
struct Counter {}
impl Counter {
    fn count[comptime N: int](self: &Self) int { N }
}
fn forward[comptime K: int](c: &Counter) int { c.count::[K]() }
fn main() {
    let c = Counter {}
    println(c.count::[3]())
    println(c.count::[0]())
    println(forward::[4](&c))
}
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"3\n0\n4\n");
}

#[test]
fn early_calls_bind_only_explicit_const_generics() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
const N: int = 9
fn number[comptime N: int]() int { N }
fn forward[comptime N: int]() int { number::[N]() }
fn length[comptime K: int](a: [int; K]) int { K }
fn ignore[comptime K: int](a: [int; K]) int { 5 }
const EXPLICIT: int = number::[3]()
const ARRAY: int = length::[2]([1, 2])
const EMPTY: int = length::[0]([1; 0])
const FORWARDED: int = forward::[4]()
const IGNORED: int = ignore([1, 2, 3])
fn main() { println(EXPLICIT)
    println(ARRAY)
    println(EMPTY)
    println(FORWARDED)
    println(IGNORED)
}
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"3\n2\n0\n4\n5\n");
}

#[test]
fn const_generic_cached_callee_preserves_instance_identity() {
    let compiler = Compiler::new();
    let source = "fn length[comptime N: int](a: [int; N]) int { _ = a\n N }\nfn main() { _ = length([1, 2, 3]) }";
    let cold = compiler.compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(cold.is_success(), "{:?}", cold.diagnostics().items());
    let warm = compiler.compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(warm.is_success(), "{:?}", warm.diagnostics().items());
    assert_eq!(
        cold.image_plan().unwrap().mono_graph_fingerprint(),
        warm.image_plan().unwrap().mono_graph_fingerprint()
    );
}

#[test]
fn symbolic_array_patterns_require_unconditional_rest() {
    for pattern in ["[]", "[x]", "[x, ..]", "[.., x]"] {
        let source = format!(
            "fn empty[comptime N: int](a: [int; N]) bool {{ match a {{ {pattern} => true, _ => false }} }}\nfn main() {{ _ = empty([1, 2]) }}"
        );
        let compiled = Compiler::shared().compile(CompileRequest::single_file(
            "main.gg",
            &source,
            TargetName::X86_64Linux,
        ));
        assert!(
            compiled.diagnostics().items().iter().any(|d| d
                .message()
                .contains("符号长度数组模式只支持无前后缀的 rest")),
            "{pattern}: {:?}",
            compiled.diagnostics().items()
        );
    }
}

#[test]
fn generic_repeat_length_shadows_module_constant() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
const N: int = 1
fn size[comptime N: int](x: int) int {
    let a = [x; N]
    a.len()
}
fn main() { println(size::[3](7)) }
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"3\n");
}

#[test]
fn const_generic_arrays_execute_distinct_instances() {
    let source = r#"
use std.io.{println}
struct Block[T, comptime N: int] { data: [T; N] }
impl[T, comptime N: int] Block[T, N] {
    fn len(self: &Self) int { N }
}
fn first[T, comptime N: int](a: [T; N]) T { a[0] }
fn filled[comptime N: int](x: int) [int; N] {
    let a: [int; N] = [x; N]
    a
}
fn dimension[T, comptime N: int](a: [T; N]) int { _ = a
    N
}
fn forwarded[comptime K: int](x: int) [int; K] { filled::[K](x) }
fn select_size[comptime A: int, comptime B: int](a: [int; A], b: [int; B]) int {
    _ = a
    _ = b
    A
}
fn reversed[comptime A: int, comptime B: int](a: [int; A], b: [int; B]) int {
    select_size::[B, A](b, a)
}
const M: int = 4
fn main() {
    let small = Block[int, 2] { data: [7; 2] }
    let large = Block[int, M] { data: filled::[4](9) }
    println(small.len())
    println(large.len())
    println(first([1, 2, 3]))
    println(dimension([1, 2, 3]))
    println(filled::[0](5).len())
    println(first::[int, 3]([8, 9, 10]))
    println(forwarded::[M](6).len())
    println(reversed([1; 2], [1; 4]))
}
"#;
    let actual: Vec<_> = output(source, &[])
        .into_iter()
        .flat_map(|(fd, bytes)| {
            assert_eq!(fd, 1);
            bytes
        })
        .collect();
    assert_eq!(actual, b"2\n4\n1\n3\n0\n8\n4\n4\n");
}

#[test]
fn const_generic_enum_and_union_fields_accept_concrete_lengths() {
    let compiled = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        r#"
enum SizedChoice[T, comptime N: int] { Data([T; N]), Empty }
union Packed[comptime N: int] { data: [int; N] }
fn take(a: SizedChoice[int, 2], b: Packed[4]) { _ = a
    _ = b
}
fn main() {}
"#,
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
}

#[test]
fn const_generic_arrays_reject_invalid_lengths_and_kinds() {
    for source in [
        "struct A[comptime N: int] { a: [int; N] }\nfn main() { _ = A[-1] { a: [0; 1] } }",
        "struct A[comptime N: int] { a: [int; N] }\nfn main() { let n = 2\n _ = A[n] { a: [0; 2] } }",
        "struct A[comptime N: int] { a: [int; N] }\nfn main() { _ = A[int] { a: [0; 2] } }",
        "struct A[T] { a: T }\nfn main() { _ = A[2] { a: 2 } }",
        "fn first[comptime N: int](a: [int; N]) int { a[0] }\nfn main() { _ = first::[2]([1, 2, 3]) }",
        "fn f[T](x: T) T { x }\nfn main() { _ = f::[2](2) }",
        "struct A[comptime N: int] { a: N }\nfn main() {}",
        "struct A[comptime N: string] { a: [int; N] }\nfn main() {}",
    ] {
        let compiled = Compiler::shared().compile(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(!compiled.is_success(), "{source}");
        assert!(!compiled.diagnostics().items().is_empty());
    }
}

#[test]
fn const_generic_array_arithmetic_has_explicit_diagnostic() {
    let compiled = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        "struct A[comptime N: int] { a: [int; N + 1] }\nfn main() {}",
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled
            .diagnostics()
            .items()
            .iter()
            .any(|d| d.message().contains("不支持参数算术")),
        "{:?}",
        compiled.diagnostics().items()
    );
}
