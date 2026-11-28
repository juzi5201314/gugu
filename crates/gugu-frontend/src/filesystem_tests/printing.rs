use super::{CompileRequest, Compiler, TargetName, harness::Harness};
use crate::runtime::RuntimeMachineOp;

#[expect(clippy::ptr_arg, reason = "签名需与 machine_hook 一致")]
pub(super) fn capture(
    harness: &mut Harness<'_>,
    op: RuntimeMachineOp,
    args: &[u64],
    memory: &mut Vec<u8>,
) -> Vec<u64> {
    assert_eq!(op, RuntimeMachineOp::ReportWrite);
    let written = if let Some((fd, length, result)) = harness.script.pop_front() {
        assert_eq!((args[0], args[2]), (fd, length));
        result
    } else {
        args[2]
    };
    let count = if written <= args[2] {
        written as usize
    } else {
        0
    };
    let start = args[1] as usize;
    harness.calls.push((
        args[0],
        memory[start..start + count]
            .iter()
            .map(|byte| u64::from(*byte))
            .collect(),
    ));
    vec![written]
}

pub(super) fn output(source: &str, script: &[(u64, u64, u64)]) -> Vec<(u64, Vec<u8>)> {
    output_with_heap(source, script, 262144)
}

pub(super) fn output_with_heap(
    source: &str,
    script: &[(u64, u64, u64)],
    heap_bytes: usize,
) -> Vec<(u64, Vec<u8>)> {
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
    assert!(compiled.image_plan().is_none());
    invoke_compiled(&compiled, "main", script, heap_bytes)
}

pub(super) fn invoke_compiled(
    compiled: &super::Compilation,
    entry: &str,
    script: &[(u64, u64, u64)],
    heap_bytes: usize,
) -> Vec<(u64, Vec<u8>)> {
    let mut harness = Harness::new(compiled);
    harness.machine_hook = Some(capture);
    harness.script.extend(script.iter().copied());
    harness.invoke("main.gg", entry, &[], &mut vec![0; heap_bytes]);
    assert!(harness.script.is_empty());
    harness
        .calls
        .into_iter()
        .map(|(fd, bytes)| (fd, bytes.into_iter().map(|byte| byte as u8).collect()))
        .collect()
}

#[test]
fn hello_fixture_uses_standard_output_on_linux_and_windows() {
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
            "main.gg",
            include_str!("../../../../ci/e2e/fixtures/hello.gg"),
            target,
        ));
        assert!(
            compiled.is_success(),
            "{target}: {:?}",
            compiled.diagnostics().items()
        );
        assert_eq!(
            invoke_compiled(&compiled, "main", &[], 262144),
            vec![(
                1,
                include_bytes!("../../../../ci/e2e/fixtures/hello.stdout").to_vec()
            )],
            "{target} 必须沿标准输出边界写出同一字节序列"
        );
    }
}

#[test]
fn print_and_write_retry_contracts() {
    let source = r#"
use std.io.{print, println, eprint, eprintln}

type Table[K] = Vec[K]
type Numbers = Table[int]
fn append[T](xs: Table[T], x: T) Vec[T] { xs.push(x)
    xs }
fn case_generic_alias() {
    let xs: Numbers = Vec[int]::new()
    let ys: Table[int] = append(xs, 42)
    match ys.get(0) { Some(n) => println(n), None => println(0) }
}

fn case_hello_world() { println("hello, gugu") }

fn case_runtime_floats() {
    let single: f32 = 0.1
    println("float=", 1.5, "/", single, "/", f"{single:.2}")
}

struct Word {}
impl Print for Word { fn print(self: &Self, out: &Formatter) { out.write_str("word") } }
fn case_mixed_values() { println("i + 1 = ", 1, true, Word {}) }

fn case_empty_and_stderr() { print(); eprint(); println(); eprintln("err"); eprint("tail") }

fn case_interpolated() {
    let i = 3
    print(f"i = {i}")
}

fn case_short_writes() { eprintln("éx") }

fn case_print_abc() { print("abc") }

fn main() {
    case_generic_alias()
    case_hello_world()
    case_runtime_floats()
    case_mixed_values()
    case_empty_and_stderr()
    case_interpolated()
    case_short_writes()
    case_print_abc()
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
    assert!(compiled.image_plan().is_none());

    assert_eq!(
        invoke_compiled(&compiled, "case_generic_alias", &[], 262144),
        vec![(1, b"42\n".to_vec())]
    );
    assert_eq!(
        invoke_compiled(&compiled, "case_hello_world", &[], 262144),
        vec![(1, b"hello, gugu\n".to_vec())]
    );
    assert_eq!(
        invoke_compiled(&compiled, "case_runtime_floats", &[], 262144),
        vec![(1, b"float=1.5/0.1/0.10\n".to_vec())]
    );
    assert_eq!(
        invoke_compiled(&compiled, "case_mixed_values", &[], 262144),
        vec![(1, b"i + 1 = 1trueword\n".to_vec())]
    );
    assert_eq!(
        invoke_compiled(&compiled, "case_empty_and_stderr", &[], 262144),
        vec![
            (1, b"\n".to_vec()),
            (2, b"err\n".to_vec()),
            (2, b"tail".to_vec())
        ]
    );
    assert_eq!(
        invoke_compiled(&compiled, "case_interpolated", &[], 262144),
        vec![(1, b"i = 3".to_vec())]
    );
    assert_eq!(
        invoke_compiled(
            &compiled,
            "case_short_writes",
            &[(2, 4, 1), (2, 3, 2), (2, 1, 1)],
            262144
        ),
        vec![(2, vec![0xc3]), (2, vec![0xa9, b'x']), (2, vec![b'\n'])]
    );
    assert_eq!(
        invoke_compiled(&compiled, "case_print_abc", &[(1, 3, 1), (1, 2, 0)], 262144),
        vec![(1, b"a".to_vec()), (1, vec![])]
    );
    assert_eq!(
        invoke_compiled(&compiled, "case_print_abc", &[(1, 3, u64::MAX)], 262144),
        vec![(1, vec![])]
    );
}

#[test]
fn aggregate_printing_and_debug_formatting() {
    let actual = output(include_str!("fixtures/aggregate_printing.gg"), &[]);
    let expected = [
        "(1, \"a\")\n",
        "[1, 2, 3]\n",
        "Some(3)|None|Ok(1)|Err(\"bad\")\n",
        "()|[]|Ok(())\n",
        "(Some(3), [1, 2, 3])|(1, \"a\")\n",
        "(Point { x: 7 })|[Point { x: 7 }]|Some(Point { x: 7 })|Ok(Point { x: 7 })\n",
        "(\n    1,\n    \"a\",\n)\n",
    ];
    assert_eq!(
        actual,
        expected
            .into_iter()
            .map(|text| (1, text.as_bytes().to_vec()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn aggregate_printing_rejects_missing_format_traits_at_use() {
    for (declaration, value, expression) in [
        (
            "struct Point { x: int }",
            "Point { x: 1 }",
            "println(value)",
        ),
        (
            "struct Point { x: int }",
            "Point { x: 1 }",
            "println((value,))",
        ),
        (
            "struct Point { x: int }",
            "Point { x: 1 }",
            "_ = f\"{value}\"",
        ),
        ("enum Shape { Dot }", "Shape::Dot", "println(value)"),
    ] {
        let source = format!(
            "use std.io.{{println}}\n{declaration}\nfn main() {{\n    let value = {value}\n    {expression}\n}}"
        );
        let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
            "main.gg",
            &source,
            TargetName::X86_64Linux,
        ));
        assert!(!compiled.is_success());
        let diagnostics = compiled.diagnostics().items();
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code() == crate::DiagnosticCode::InvalidType
                    && (diagnostic.message().contains("Print")
                        || diagnostic.message().contains("Debug"))
                    && diagnostic.span().is_some_and(|span| span.line() == 5)
            }),
            "{expression}: {diagnostics:?}"
        );
        assert!(diagnostics.iter().all(|diagnostic| {
            !diagnostic.message().contains("内部") && !diagnostic.message().contains("已检查接口")
        }));
    }
}

#[test]
fn missing_print_diagnostic_identifies_the_callee_instead_of_eof() {
    let source = include_str!("fixtures/missing_print_diagnostic.gg");
    let compiled = Compiler::shared().compile_sema_for_test(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(!compiled.is_success());
    let errors = compiled.diagnostics().items();
    let error = errors
        .iter()
        .find(|error| {
            error.code() == crate::DiagnosticCode::InvalidType
                && error.message().contains("不满足 trait Print")
        })
        .unwrap_or_else(|| panic!("Point 没有 Print 实现：{errors:?}"));
    let span = error.span().expect("调用错误必须携带源范围");
    let start = source.find("println(value)").unwrap();
    assert_eq!((span.line(), span.column()), (5, 5));
    assert_eq!(span.start() as usize, start);
    assert_eq!(span.end() as usize, start + "println".len());
}

#[test]
fn aggregate_printing_keeps_builtin_impls_authoritative() {
    for ty in ["(int,)", "[int; 1]"] {
        let source = format!(
            "impl Print for {ty} {{ fn print(self: &Self, out: &Formatter) {{}} }}\nfn main() {{}}"
        );
        let compiled = Compiler::shared().compile_sema_for_test(CompileRequest::single_file(
            "main.gg",
            &source,
            TargetName::X86_64Linux,
        ));
        assert!(!compiled.is_success());
        assert!(
            compiled.diagnostics().items().iter().any(|diagnostic| {
                diagnostic.code() == crate::DiagnosticCode::InvalidDeclaration
                    && diagnostic
                        .message()
                        .contains("不能覆盖编译器提供的语言实现")
            }),
            "{ty}: {:?}",
            compiled.diagnostics().items()
        );
    }
}

fn formatting_fixture_oracle() -> String {
    use super::formatting::oracle;
    use gugu_sema::string::fmt;
    let mut expected = format!(
        "values: {} {} {} {} {}\nstrings: {:?}\npadding: {:*>6}|{:<6}|{:^7}|{:#x}\n",
        42, -7, true, 'Z', "hello", "line\n\"\\\t", 42, 42, 42, 42
    );
    macro_rules! floats {
        ($ty:ty, $label:literal) => {{
            let v: $ty = 0.1;
            let large: $ty = 1e21;
            let zero: $ty = -0.0;
            let nan = <$ty>::NAN;
            let inf = <$ty>::INFINITY;
            expected.push_str(&format!(
                "{}: {v}|{large}|{zero}|{nan}|{inf}|{v:.3}|{v:e}\n",
                $label
            ));
        }};
    }
    floats!(f64, "f64");
    floats!(f32, "f32");
    expected.push_str("print: ");
    expected.push_str(&oracle("", |f| {
        f.write_str("Widget(");
        fmt::format_int(f, 7);
        f.write_str(")");
    }));
    expected.push('\n');
    for (label, spec) in [("debug", "?"), ("pretty", "#?")] {
        expected.push_str(label);
        expected.push_str(": ");
        expected.push_str(&oracle(spec, |f| {
            f.debug_struct("Record")
                .field(|f| f.write_str("number"), |f| fmt::format_float(f, 0.1))
                .field(|f| f.write_str("text"), |f| fmt::format_str(f, "field\n"))
                .finish();
        }));
        expected.push('\n');
    }
    expected
}

#[test]
fn linux_formatting_fixture_matches_oracle_and_interpreter() {
    let expected = formatting_fixture_oracle();
    assert_eq!(
        include_str!("../../../../ci/e2e/fixtures/formatting.stdout"),
        expected
    );
    let calls = output(
        include_str!("../../../../ci/e2e/fixtures/formatting.gg"),
        &[],
    );
    let mut actual = Vec::new();
    for (fd, bytes) in calls {
        assert_eq!(fd, 1);
        actual.extend(bytes);
    }
    assert_eq!(actual, expected.as_bytes());
}

#[test]
fn print_overview_example_compiles_to_linux_elf() {
    let compiled = super::docs_examples::compile_overview_image();
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    assert!(
        compiled
            .image_plan()
            .unwrap()
            .linux_image()
            .starts_with(b"\x7fELF")
    );
}
