use super::{CompileRequest, Compiler, TargetName, printing::invoke_compiled};

fn run_stdout(compiled: &super::Compilation, entry: &str) -> Vec<u8> {
    invoke_compiled(compiled, entry, &[], 262144)
        .into_iter()
        .flat_map(|(fd, bytes)| {
            assert_eq!(fd, 1);
            bytes
        })
        .collect()
}

fn check_frontend_error(source: &str, needle: &str) {
    let compiled = Compiler::shared().compile_sema_for_test(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(!compiled.is_success(), "意外通过编译：{source}");
    assert!(
        compiled
            .diagnostics()
            .items()
            .iter()
            .any(|diagnostic| diagnostic.message().contains(needle)),
        "期望包含 `{needle}`，实际：{:?}",
        compiled.diagnostics().items()
    );
}

#[test]
fn const_generics_runtime_dependent_and_early_contracts() {
    let source = r#"
use std.io.{println}
use std.test as Mod

fn dep_g(comptime n: int) [int; n] { [9; n] }
fn dep_f(comptime n: int) [int; n] { dep_g(((n))) }
const EARLY_DEP: [int; 2] = dep_f(2)
fn case_dep_paren() {
    let a = dep_f(3)
    let empty: [int; 0] = dep_f(0)
    _ = empty
    println(EARLY_DEP[1])
    println(a[2])
}

struct DepMethod {}
impl DepMethod { fn f(self: &Self, comptime n: int) [int; n] { [7; n] } }
fn case_dep_method() {
    let s = DepMethod {}
    let a = s.f(2)
    let b = s.f(3)
    let empty: [int; 0] = s.f(0)
    _ = empty
    println(a[1])
    println(b[2])
}

fn dep_repeat[T](comptime n: int, x: T) [T; n] {
    let a: [T; n]
    for i in 0..n { a[i] = x }
    a
}
const REP_A: [int; 2] = dep_repeat(2, 7)
const REP_B: [int; 3] = dep_repeat::[int](3, 8)
fn case_dep_repeat() {
    let a = dep_repeat(2, 5)
    let b = dep_repeat::[int](3, 6)
    let empty: [int; 0] = dep_repeat(0, 9)
    _ = empty
    println(a[1])
    println(b[2])
    println(REP_A[1])
    println(REP_B[2])
}

fn combine_order[comptime N: int](value: int) int { N * 10 + value }
const ORDER_ANSWER: int = comptime source {
    let n = 0
    let value = combine_order::[{ n = 1
        n
    }]({ n = 2
        n
    })
    std.syntax.parse_source(f"{value} * 10 + {n}")
}
fn case_early_const_order() { println(ORDER_ANSWER) }

fn num_assign[comptime N: int]() int { N }
fn inc_assign(value: int) int { value + 1 }
fn nested_assign() int { num_assign::[inc_assign(num_assign::[2]())]() }
const TOP_LEVEL_ASSIGN: int = num_assign::[inc_assign(2)]()
const NESTED_ASSIGN: int = nested_assign()
const ASSIGN_ANSWER: int = comptime source {
    let n = 1
    let value = num_assign::[{ n += 1
        n
    }]()
    std.syntax.parse_source(f"{value} * 10 + {n}")
}
fn case_const_assign() {
    println(ASSIGN_ANSWER)
    println(TOP_LEVEL_ASSIGN)
    println(NESTED_ASSIGN)
}

const SHADOW_M: int = 1
fn forward_macro(comptime SHADOW_M: int) int { num_assign::[SHADOW_M]() }
const MACRO_SHADOW_ANSWER: int = comptime source {
    let SHADOW_M = 3
    let local = num_assign::[SHADOW_M]()
    let parameter = forward_macro(4)
    std.syntax.parse_source(f"{local} * 10 + {parameter}")
}
fn case_macro_locals() { println(MACRO_SHADOW_ANSWER) }

fn qualified_length[comptime Mod: int](x: int) int {
    let values = [x; Mod.FLAG_IGNORE]
    values.len()
}
fn case_qualified_repeat() { println(qualified_length::[4](7)) }

fn main() {
    case_dep_paren()
    case_dep_method()
    case_dep_repeat()
    case_early_const_order()
    case_const_assign()
    case_macro_locals()
    case_qualified_repeat()
}
"#;
    let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    assert_eq!(run_stdout(&compiled, "case_dep_paren"), b"9\n9\n");
    assert_eq!(run_stdout(&compiled, "case_dep_method"), b"7\n7\n");
    assert_eq!(run_stdout(&compiled, "case_dep_repeat"), b"5\n6\n7\n8\n");
    assert_eq!(run_stdout(&compiled, "case_early_const_order"), b"122\n");
    assert_eq!(run_stdout(&compiled, "case_const_assign"), b"22\n3\n3\n");
    assert_eq!(run_stdout(&compiled, "case_macro_locals"), b"34\n");
    assert_eq!(run_stdout(&compiled, "case_qualified_repeat"), b"1\n");
}

#[test]
fn const_generics_runtime_shadowing_packs_and_instances() {
    let source = r#"
use std.io.{println}

const N: int = 9
fn number[comptime N: int]() int { N }
fn forward_num[comptime N: int]() int { number::[N]() }
const EARLY_FORWARD: int = forward_num::[3]()
fn case_forwarded_shadow() {
    println(forward_num::[3]())
    println(EARLY_FORWARD)
}

const M_CALLER: int = 3
fn caller_ignore(M_CALLER: int) int { number::[M_CALLER]() }
fn expr_ignore(M_CALLER: int) int { number::[M_CALLER + 1]() }
const CALLER_ANS: int = caller_ignore(7)
const EXPR_ANS: int = expr_ignore(7)
fn case_ignore_caller_params() {
    println(CALLER_ANS)
    println(EXPR_ANS)
}

enum Choice { A { data: [int; 2] }, B { data: [int; 3] } }
fn choice_num[comptime N: int](choice: Choice) int { N }
const CHOICE_ANS: int = choice_num::[4](Choice::A { data: [1, 2] })
fn case_inactive_enum() { println(CHOICE_ANS) }

fn inner_empty[comptime N: int](a: [[int; N]; 0]) int { N }
const INNER_ANS: int = inner_empty::[3]([[1; 3]; 0])
fn case_empty_outer() { println(INNER_ANS) }

fn pack_num[comptime N: int, Ts...]() int { N }
fn pack_typed[T, comptime N: int, Ts...]() int { N }
const PACK_EMPTY: int = pack_num::[2]()
const PACK_MULTI: int = pack_num::[3, int, bool]()
const PACK_TYPED: int = pack_typed::[bool, 4, int]()
fn case_type_packs() {
    println(PACK_EMPTY)
    println(PACK_MULTI)
    println(PACK_TYPED)
}

fn filled_late[comptime N: int](x: int) [int; N] { [x; N] }
const LATE_THREE: [int; 3] = filled_late::[3](type_id_count())
const LATE_EMPTY: [int; 0] = filled_late::[0](type_id_count())
fn case_late_repeats() {
    let three = LATE_THREE
    let empty = LATE_EMPTY
    println(three.len())
    println(empty.len())
    println(three[0] > 0)
}

struct Counter {}
impl Counter { fn count[comptime N: int](self: &Self) int { N } }
fn forward_counter[comptime K: int](c: &Counter) int { c.count::[K]() }
fn case_const_method() {
    let c = Counter {}
    println(c.count::[3]())
    println(c.count::[0]())
    println(forward_counter::[4](&c))
}

fn arr_length[comptime K: int](a: [int; K]) int { K }
fn arr_ignore[comptime K: int](a: [int; K]) int { 5 }
const BIND_EXPLICIT: int = number::[3]()
const BIND_ARRAY: int = arr_length::[2]([1, 2])
const BIND_EMPTY: int = arr_length::[0]([1; 0])
const BIND_FORWARD: int = forward_num::[4]()
const BIND_IGNORED: int = arr_ignore([1, 2, 3])
fn case_early_bind_only_explicit() {
    println(BIND_EXPLICIT)
    println(BIND_ARRAY)
    println(BIND_EMPTY)
    println(BIND_FORWARD)
    println(BIND_IGNORED)
}

fn repeat_shadow[comptime N: int](x: int) int {
    let a = [x; N]
    a.len()
}
fn case_repeat_shadow() { println(repeat_shadow::[3](7)) }

struct Block[T, comptime N: int] { data: [T; N] }
impl[T, comptime N: int] Block[T, N] { fn len(self: &Self) int { N } }
fn first_elem[T, comptime N: int](a: [T; N]) T { a[0] }
fn filled_arr[comptime N: int](x: int) [int; N] {
    let a: [int; N] = [x; N]
    a
}
fn dimension[T, comptime N: int](a: [T; N]) int { _ = a
    N }
fn forwarded_arr[comptime K: int](x: int) [int; K] { filled_arr::[K](x) }
fn select_size[comptime A: int, comptime B: int](a: [int; A], b: [int; B]) int { _ = a
    _ = b
    A }
fn reversed[comptime A: int, comptime B: int](a: [int; A], b: [int; B]) int {
    select_size::[B, A](b, a)
}
const M_BLOCK: int = 4
fn case_distinct_instances() {
    let small = Block[int, 2] { data: [7; 2] }
    let large = Block[int, M_BLOCK] { data: filled_arr::[4](9) }
    println(small.len())
    println(large.len())
    println(first_elem([1, 2, 3]))
    println(dimension([1, 2, 3]))
    println(filled_arr::[0](5).len())
    println(first_elem::[int, 3]([8, 9, 10]))
    println(forwarded_arr::[M_BLOCK](6).len())
    println(reversed([1; 2], [1; 4]))
}

enum SizedChoice[T, comptime N: int] { Data([T; N]), Empty }
union Packed[comptime N: int] { data: [int; N] }
fn take_sized(a: SizedChoice[int, 2], b: Packed[4]) { _ = a
    _ = b }

fn main() {
    case_forwarded_shadow()
    case_ignore_caller_params()
    case_inactive_enum()
    case_empty_outer()
    case_type_packs()
    case_late_repeats()
    case_const_method()
    case_early_bind_only_explicit()
    case_repeat_shadow()
    case_distinct_instances()
}
"#;
    let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    assert_eq!(run_stdout(&compiled, "case_forwarded_shadow"), b"3\n3\n");
    assert_eq!(
        run_stdout(&compiled, "case_ignore_caller_params"),
        b"3\n4\n"
    );
    assert_eq!(run_stdout(&compiled, "case_inactive_enum"), b"4\n");
    assert_eq!(run_stdout(&compiled, "case_empty_outer"), b"3\n");
    assert_eq!(run_stdout(&compiled, "case_type_packs"), b"2\n3\n4\n");
    assert_eq!(run_stdout(&compiled, "case_late_repeats"), b"3\n0\ntrue\n");
    assert_eq!(run_stdout(&compiled, "case_const_method"), b"3\n0\n4\n");
    assert_eq!(
        run_stdout(&compiled, "case_early_bind_only_explicit"),
        b"3\n2\n0\n4\n5\n"
    );
    assert_eq!(run_stdout(&compiled, "case_repeat_shadow"), b"3\n");
    assert_eq!(
        run_stdout(&compiled, "case_distinct_instances"),
        b"2\n4\n1\n3\n0\n8\n4\n4\n"
    );
}

#[test]
fn const_generics_mono_instances_deduplicate() {
    let compile =
        |n| {
            let source = format!(
                "fn f(comptime n: int) [int; n] {{ [0; n] }}\nfn main() {{ _ = f(2)\n _ = f({n}) }}"
            );
            let compiled = Compiler::shared().compile_frontend_for_test(
                CompileRequest::single_file("main.gg", source, TargetName::X86_64Linux),
            );
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
        same.mono_instance_count().unwrap() + 1,
        distinct.mono_instance_count().unwrap()
    );
    assert_eq!(
        distinct.mono_graph_fingerprint().unwrap(),
        warm.mono_graph_fingerprint().unwrap()
    );
}

#[test]
fn const_generics_cold_warm_mono_cache_matches() {
    let compiler = Compiler::new();
    let source = "fn length[comptime N: int](a: [int; N]) int { _ = a\n N }\nfn main() { _ = length([1, 2, 3]) }";
    let cold = compiler.compile_frontend_for_test(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(cold.is_success(), "{:?}", cold.diagnostics().items());
    let warm = compiler.compile_frontend_for_test(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(warm.is_success(), "{:?}", warm.diagnostics().items());
    assert_eq!(
        cold.mono_graph_fingerprint().unwrap(),
        warm.mono_graph_fingerprint().unwrap()
    );
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
    Compiler::shared().compile_sema_for_test(CompileRequest::project_target(
        package,
        target,
        "constructors@0.0.0 (path+.)",
        TargetName::X86_64Linux,
        vec!["default".to_owned()],
        Default::default(),
    ))
}

#[test]
fn imported_result_constructor_const_argument_contracts() {
    let ok = compile_imported_result_constructors(
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
    assert!(ok.is_success(), "{:?}", ok.diagnostics().items());

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

#[test]
fn const_generics_negative_diagnostics_dependent_and_early() {
    check_frontend_error(
        r#"
fn take(comptime n: int, a: [int; ((n))]) int { a[0] }
const VALUE: int = comptime source {
    let value = take(2, [1, 2, 3])
    std.syntax.parse_source(f"{value}")
}
fn main() { _ = VALUE }
"#,
        "数组实参长度与参数长度不一致",
    );
    check_frontend_error(
        r#"
trait Change { fn change(self: &Self) }
impl Change for int { fn change(self: &Self) { *self = 1 } }
fn f(comptime n: int) [int; n] { n.change()
    [0; n] }
fn main() { _ = f(2) }
"#,
        "不能赋值或借用",
    );
    for statement in ["n = 1", "_ = &n", "_ = std.ptr.addr_of((n))"] {
        let source = format!(
            "fn f(comptime n: int) [int; n] {{ {statement}\n [0; n] }}\nfn main() {{ _ = f(2) }}"
        );
        check_frontend_error(&source, "不能赋值或借用");
    }
    check_frontend_error(
        "fn repeat(comptime n: int) [int; n] { [0; n] }\nfn f(n: int) { _ = repeat(n) }\nfn main() {}",
        "需要编译期已知值",
    );
    check_frontend_error(
        "fn f(comptime n: int) [int; n + 1] { [0; n + 1] }\nfn main() { _ = f(2) }",
        "算术",
    );
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
        check_frontend_error(&source, "comptime 泛型实参需要非负 int 常量");
    }
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
        check_frontend_error(&source, "comptime 泛型实参数量不符");
    }
}

#[test]
fn const_generics_negative_diagnostics_arrays_and_patterns() {
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
        check_frontend_error(&source, "comptime 数组实参长度与参数长度不一致");
    }

    let reform = "const SIZE: int = length([0])\nfn length[comptime N: int](a: [int; SIZE]) int { N }\nfn main() { _ = SIZE }";
    check_frontend_error(reform, "早期求值 const 泛型调用需要显式 ::[...] const 实参");
    let explicit = Compiler::shared().compile_sema_for_test(CompileRequest::single_file(
        "main.gg",
        reform.replace("length([0])", "length::[1]([0])"),
        TargetName::X86_64Linux,
    ));
    assert!(
        explicit.is_success(),
        "{:?}",
        explicit.diagnostics().items()
    );

    for source in [
        "enum Choice { A { data: [int; 2] }, B { data: [int; 3] } }\nfn number[comptime N: int](choice: Choice) int { N }\nconst ANSWER: int = number(Choice::A { data: [1, 2] })\nfn main() { _ = ANSWER }",
        "fn inner[comptime N: int](a: [[int; N]; 0]) int { N }\nconst ANSWER: int = inner([[1; 3]; 0])\nfn main() { _ = ANSWER }",
        "const N: int = 9\nfn length[comptime N: int](a: [int; N]) int { N }\nconst ANSWER: int = length([1, 2])\nfn main() { _ = ANSWER }",
        "fn ambiguous[Ts..., comptime N: int]() int { N }\nconst ANSWER: int = ambiguous::[2, int]()\nfn main() { _ = ANSWER }",
    ] {
        check_frontend_error(source, "早期求值 const 泛型调用需要显式 ::[...] const 实参");
    }

    for pattern in ["[]", "[x]", "[x, ..]", "[.., x]"] {
        let source = format!(
            "fn empty[comptime N: int](a: [int; N]) bool {{ match a {{ {pattern} => true, _ => false }} }}\nfn main() {{ _ = empty([1, 2]) }}"
        );
        check_frontend_error(&source, "符号长度数组模式只支持无前后缀的 rest");
    }

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
        let compiled = Compiler::shared().compile_sema_for_test(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(!compiled.is_success(), "{source}");
        assert!(!compiled.diagnostics().items().is_empty());
    }

    check_frontend_error(
        "struct A[comptime N: int] { a: [int; N + 1] }\nfn main() {}",
        "不支持参数算术",
    );
}
