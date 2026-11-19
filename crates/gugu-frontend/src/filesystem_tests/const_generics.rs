use super::{CompileRequest, Compiler, TargetName, printing::output};

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
