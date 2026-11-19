use super::{CompileRequest, Compiler, TargetName, printing::output};

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
fn early_calls_bind_explicit_and_inferred_const_generics() {
    let actual: Vec<_> = output(
        r#"
use std.io.{println}
const N: int = 9
fn number[comptime N: int]() int { N }
fn forward[comptime N: int]() int { number::[N]() }
fn length[comptime K: int](a: [int; K]) int { K }
const EXPLICIT: int = number::[3]()
const INFERRED: int = length([1, 2])
const EMPTY: int = length([1; 0])
const FORWARDED: int = forward::[4]()
fn main() { println(EXPLICIT)
    println(INFERRED)
    println(EMPTY)
    println(FORWARDED)
}
"#,
        &[],
    )
    .into_iter()
    .flat_map(|(_, bytes)| bytes)
    .collect();
    assert_eq!(actual, b"3\n2\n0\n4\n");
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
