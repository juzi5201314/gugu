use super::{CompileRequest, Compiler, TargetName, harness::Harness};
use gugu_sema::string::{FormatSpec, ParsedCount, fmt, parse_format};

fn oracle(spec: &str, value: impl FnOnce(&mut fmt::Formatter)) -> String {
    let spec: FormatSpec<u64> = parse_format(spec)
        .unwrap()
        .try_map(|count| {
            Ok::<_, ()>(match count {
                ParsedCount::Fixed(value) => value,
                ParsedCount::Name(_) => 6,
            })
        })
        .unwrap();
    let mut formatter = fmt::Formatter::new(spec);
    value(&mut formatter);
    formatter.finish()
}

fn render(source: &str) -> String {
    render_with_heap(source).0
}

fn render_with_heap(source: &str) -> (String, usize) {
    let source = format!("{source}\nfn main() {{ _ = render() }}");
    let output = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        &source,
        TargetName::X86_64Linux,
    ));
    assert!(output.is_success(), "{:?}", output.diagnostics().items());
    let mut harness = Harness::new(&output);
    let mut memory = vec![0; 262144];
    let result = harness.invoke("main.gg", "render", &[], &mut memory);
    let start = result[0] as usize;
    (
        String::from_utf8(memory[start..start + result[1] as usize].to_vec()).unwrap(),
        harness.heap,
    )
}

#[test]
fn runtime_format_append_allocation_is_linear() {
    let (text, allocated) = render_with_heap(
        r#"
        use std.fmt.{Formatter}
        fn render() string {
            let out = Formatter::new()
            for _ in 0..64 { out.write_str("0123456789abcdef") }
            out.finish()
        }
        "#,
    );
    assert_eq!(text, "0123456789abcdef".repeat(64));
    assert!(allocated < 32768, "formatter allocated {allocated} bytes");
}

#[test]
fn runtime_format_literals_and_empty() {
    assert_eq!(
        render(r#"fn render() string = f"hello {{world}}""#),
        "hello {world}"
    );
    assert_eq!(render(r#"fn render() string = f"""#), "");
}

#[test]
fn heterogeneous_pack_formatting() {
    assert_eq!(
        render(
            r#"
        use std.fmt.{Formatter}
        fn total[Ts: Print...](...args: Ts) string {
            let out = Formatter::new()
            for a in args { a.print(&out) }
            out.finish()
        }
        fn render() string = total(1, true, "x")
    "#
        ),
        "1truex"
    );
}

#[test]
fn heterogeneous_pack_empty() {
    assert_eq!(
        render(
            r#"
        use std.fmt.{Formatter}
        fn total[Ts: Print...](...args: Ts) string {
            let out = Formatter::new()
            for a in args { a.print(&out) }
            out.finish()
        }
        fn render() string = total()
    "#
        ),
        ""
    );
}

#[test]
fn heterogeneous_pack_return_exits_the_function() {
    assert_eq!(
        render(
            r#"
        fn first[Ts: Print...](...args: Ts) string {
            for a in args { return f"{a}" }
            "empty"
        }
        fn render() string = first(1, true, "x")
    "#
        ),
        "1"
    );
}

#[test]
fn heterogeneous_pack_user_dispatch_and_helpers() {
    assert_eq!(
        render(
            r#"
        use std.fmt.{Formatter}
        struct Word {}
        impl Print for Word { fn print(self: &Self, out: &Formatter) { out.write_str("word") } }
        fn write[T: Print](value: T, out: &Formatter) { value.print(out) }
        fn total[Ts: Print...](...args: Ts) string {
            let out = Formatter::new()
            for a in args {
                defer { out.write_str("!") }
                write(a, &out)
            }
            out.finish()
        }
        fn render() string = total(1, Word {}, true)
    "#
        ),
        "1!word!true!"
    );
}

#[test]
fn runtime_format_primitives() {
    let expected = [
        oracle("", |f| fmt::format_int(f, -42)),
        oracle("", |f| fmt::format_bool(f, true)),
        oracle("", |f| fmt::format_str(f, "hello")),
        oracle("", |f| fmt::format_char(f, '界')),
    ]
    .join("|");
    assert_eq!(
        render(
            r#"fn render() string {
                let n = -42
                let b = true
                let s = "hello"
                let c = '界'
                f"{n}|{b}|{s}|{c}"
            }"#
        ),
        expected
    );
}

#[test]
fn runtime_format_specs_and_dynamic_counts() {
    let expected = [
        oracle("*>6", |f| fmt::format_int(f, 42)),
        oracle("^6", |f| fmt::format_str(f, "hi")),
        oracle("#x", |f| fmt::format_int(f, 42)),
        oracle("+06", |f| fmt::format_int(f, 42)),
        oracle("<width$", |f| fmt::format_int(f, 42)),
        oracle(".width$", |f| fmt::format_str(f, "abcdefgh")),
    ]
    .join("|");
    assert_eq!(
        render(
            r#"fn render() string {
                let n = 42
                let s = "hi"
                let long = "abcdefgh"
                let width = 6
                f"{n:*>6}|{s:^6}|{n:#x}|{n:+06}|{n:<width$}|{long:.width$}"
            }"#
        ),
        expected
    );
}

#[test]
fn runtime_format_integer_boundaries() {
    let expected = format!(
        "-128|-32768|-2147483648|{}|255|65535|4294967295|{}|{}",
        i64::MIN,
        u64::MAX,
        oracle("#X", |f| fmt::format_int(f, -1))
    );
    assert_eq!(
        render(
            r#"fn render() string {
        let a: i8 = -128
        let b: i16 = -32768
        let c: i32 = -2147483648
        let d: int = -9223372036854775808
        let e: byte = 255
        let f: u16 = 65535
        let g: u32 = 4294967295
        let h: uint = 18446744073709551615
        let i: i8 = -1
        f"{a}|{b}|{c}|{d}|{e}|{f}|{g}|{h}|{i:#X}"
    }"#
        ),
        expected
    );
}

#[test]
fn runtime_format_wide_integer_boundaries() {
    assert_eq!(
        render(
            r#"fn render() string {
        let a: i128 = -170141183460469231731687303715884105728
        let b: u128 = 340282366920938463463374607431768211455
        f"{a}|{b}"
    }"#
        ),
        format!("{}|{}", i128::MIN, u128::MAX)
    );
}

#[test]
fn runtime_format_debug_and_user_print() {
    let debug = oracle("?", |f| fmt::format_str(f, "hello\n\"world\u{85}\u{200b}"));
    let actual = render(
        r#"
        struct Record { value: int }
        impl Print for Record {
            fn print(self: &Self, out: &Formatter) { out.write_str("Record("); out.write_str(f"{self.value}"); out.write_char(')') }
        }
        fn render() string {
            let s = "hello\n\"world\u{85}\u{200b}"
            let r = Record { value: 7 }
            f"{s:?}|{r}"
        }
    "#,
    );
    assert_eq!(actual, format!("{debug}|Record(7)"));
}

#[test]
#[should_panic(expected = "Runtime(Panic)")]
fn runtime_format_negative_dynamic_width_panics() {
    render(
        r#"fn render() string {
        let width = -1
        let n = 42
        f"{n:width$}"
    }"#,
    );
}
