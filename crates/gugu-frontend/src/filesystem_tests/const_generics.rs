use super::{CompileRequest, Compiler, TargetName, printing::output};

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
