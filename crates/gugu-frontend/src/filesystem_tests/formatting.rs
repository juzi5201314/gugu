use super::{CompileRequest, Compiler, TargetName, harness::Harness};
use gugu_sema::string::{FormatSpec, ParsedCount, fmt, parse_format};

pub(super) fn oracle(spec: &str, value: impl FnOnce(&mut fmt::Formatter)) -> String {
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

fn invoke_with_heap(compiled: &super::Compilation, entry: &str) -> (String, usize) {
    let mut harness = Harness::new(compiled);
    let mut memory = vec![0; 262144];
    let result = harness.invoke("main.gg", entry, &[], &mut memory);
    let start = result[0] as usize;
    (
        String::from_utf8(memory[start..start + result[1] as usize].to_vec()).unwrap(),
        harness.heap,
    )
}

fn assert_panics(compiled: &super::Compilation, entry: &str) {
    let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        invoke_with_heap(compiled, entry);
    }))
    .expect_err(&format!("{entry} 必须触发 Runtime(Panic)"));
    let message = error
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| error.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        message.contains("Runtime(Panic)"),
        "{entry} 意外 panic 消息：{message}"
    );
}

#[test]
fn runtime_formatting_and_heterogeneous_pack_contracts() {
    let source = r#"
use std.fmt.{Formatter}

fn case_append_alloc() string {
    let out = Formatter::new()
    for _ in 0..64 { out.write_str("0123456789abcdef") }
    out.finish()
}

fn case_literals() string = f"hello {{world}}"
fn case_empty() string = f""

const SINGLE: f32 = 0.1
const SHORT: string = f"{SINGLE}"
const PRECISE: string = f"{SINGLE:.20?}"
const SUM: f32 = SINGLE + SINGLE
const NEGATIVE: f32 = -SINGLE
const OPERATIONS: string = f"{SUM}|{NEGATIVE}"
fn case_comptime_f32() string = SHORT + "|" + PRECISE + "|" + OPERATIONS

fn total_basic[Ts: Print...](...args: Ts) string {
    let out = Formatter::new()
    for a in args { a.print(&out) }
    out.finish()
}
fn case_pack_formatting() string = total_basic(1, true, "x")
fn case_pack_empty() string = total_basic()

fn total_slice[Ts: Print...](start: int, end: int, ...args: Ts) string {
    let out = Formatter::new()
    let text = "a界b"
    out.write_str(text[..0])
    for a in args {
        out.write_str(text[start..end])
        a.print(&out)
    }
    out.finish()
}
fn case_pack_slices_nonempty() string = total_slice(1, 4, 1, true)
fn case_pack_slices_empty() string = total_slice(3, 1)
fn case_pack_slice_bounds_panic() string = total_slice(3, 1, true)
fn case_pack_slice_utf8_panic() string = total_slice(2, 4, true)

fn first_pack[Ts: Print...](...args: Ts) string {
    for a in args { return f"{a}" }
    "empty"
}
fn case_pack_return() string = first_pack(1, true, "x")

fn total_capture[Ts: Print...](...args: Ts) string {
    let out = Formatter::new()
    for a in args {
        let text = f"{a}"
        let write = fn() { out.write_str(text) }
        write()
    }
    out.finish()
}
fn case_pack_capture() string = total_capture(1, true, "x")

struct Word {}
impl Print for Word { fn print(self: &Self, out: &Formatter) { out.write_str("word") } }
fn write_one[T: Print](value: T, out: &Formatter) { value.print(out) }
fn total_user[Ts: Print...](...args: Ts) string {
    let out = Formatter::new()
    for a in args {
        defer { out.write_str("!") }
        write_one(a, &out)
    }
    out.finish()
}
fn case_pack_user_dispatch() string = total_user(1, Word {}, true)

fn case_primitives() string {
    let n = -42
    let b = true
    let s = "hello"
    let c = '界'
    f"{n}|{b}|{s}|{c}"
}

fn total_elem_capture[Ts: Print...](...args: Ts) string {
    let out = Formatter::new()
    for a in args {
        let write = fn() {
            let inner = fn() { a.print(&out) }
            inner()
        }
        write()
    }
    out.finish()
}
fn case_pack_elem_capture() string = total_elem_capture(1, true, "x") + total_elem_capture()

fn count_pack[Ts: Print...](...args: Ts) int {
    let count = 0
    for _ in args { count = count + 1 }
    count
}
fn case_pack_wildcard() string {
    let mixed = count_pack(1, true, "x")
    let empty = count_pack()
    f"{mixed}:{empty}"
}

fn collect_escaping[Ts: Print...](...args: Ts) string {
    let out = Formatter::new()
    let saved: [fn(); 3] = [fn() {}, fn() {}, fn() {}]
    let index = 0
    for a in args {
        let text = f"[{a}]"
        let write = fn() { a.print(&out); out.write_str(text) }
        write()
        saved[index] = write
        index = index + 1
    }
    out.write_str("|")
    for i in 0..index { saved[i]() }
    out.finish()
}
fn case_pack_escaping() string = collect_escaping(1, true, "x") + collect_escaping()

fn sum_callables[Fs: Fn() int...](...fs: Fs) int {
    let n = 0
    for f in fs { n = n + f() }
    n
}
fn one_fn() int = 1
fn single_fn[F: Fn() int](f: F) int = f()
fn case_pack_callable() string {
    let bias = 2
    let two = fn() int = bias
    let mixed = sum_callables(one_fn, two)
    let empty = sum_callables()
    let baseline = single_fn(two)
    f"{mixed}:{empty}:{baseline}"
}

fn case_specs_dynamic() string {
    let n = 42
    let s = "hi"
    let long = "abcdefgh"
    let width = 6
    f"{n:*>6}|{s:^6}|{n:#x}|{n:+06}|{n:<width$}|{long:.width$}"
}

fn case_never_returns() string {
    let out = Formatter::new()
    let text = f"prefix{ { out.write_str("evaluated"); return out.finish() } }suffix"
    text
}

struct Integral { non_negative: bool, prefix: string, digits: string }
impl LowerHex for Integral {
    fn lower_hex(self: &Self, out: &Formatter) {
        out.pad_integral(self.non_negative, self.prefix, self.digits)
    }
}
fn case_unicode_padding() string {
    let text = "界é🙂"
    let value = Integral { non_negative: false, prefix: "前", digits: "é🙂" }
    let empty = Integral { non_negative: true, prefix: "", digits: "" }
    f"{value:界>#8x}|{empty:界>#8x}|{text:界>6.2}"
}

fn case_never_panics() string = f"prefix{panic("test")}suffix"

fn case_int_boundaries() string {
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
}

fn case_wide_int_boundaries() string {
    let a: i128 = -170141183460469231731687303715884105728
    let b: u128 = 340282366920938463463374607431768211455
    f"{a}|{b}"
}

struct Record { value: int }
impl Print for Record {
    fn print(self: &Self, out: &Formatter) { out.write_str("Record("); out.write_str(f"{self.value}"); out.write_char(')') }
}
fn case_debug_user_print() string {
    let s = "hello\n\"world\u{85}\u{200b}"
    let r = Record { value: 7 }
    f"{s:?}|{r}"
}

fn case_neg_width_panic() string {
    let width = -1
    let n = 42
    f"{n:width$}"
}

fn main() {
    _ = case_append_alloc()
    _ = case_literals()
    _ = case_empty()
    _ = case_comptime_f32()
    _ = case_pack_formatting()
    _ = case_pack_empty()
    _ = case_pack_slices_nonempty()
    _ = case_pack_slices_empty()
    _ = case_pack_slice_bounds_panic()
    _ = case_pack_slice_utf8_panic()
    _ = case_pack_return()
    _ = case_pack_capture()
    _ = case_pack_user_dispatch()
    _ = case_primitives()
    _ = case_pack_elem_capture()
    _ = case_pack_wildcard()
    _ = case_pack_escaping()
    _ = case_pack_callable()
    _ = case_specs_dynamic()
    _ = case_never_returns()
    _ = case_unicode_padding()
    _ = case_never_panics()
    _ = case_int_boundaries()
    _ = case_wide_int_boundaries()
    _ = case_debug_user_print()
    _ = case_neg_width_panic()
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

    let (text, allocated) = invoke_with_heap(&compiled, "case_append_alloc");
    assert_eq!(text, "0123456789abcdef".repeat(64));
    assert!(allocated < 32768, "formatter allocated {allocated} bytes");

    assert_eq!(
        invoke_with_heap(&compiled, "case_literals").0,
        "hello {world}"
    );
    assert_eq!(invoke_with_heap(&compiled, "case_empty").0, "");
    assert_eq!(
        invoke_with_heap(&compiled, "case_comptime_f32").0,
        format!("{}|{:.20?}|{}|{}", 0.1f32, 0.1f32, 0.2f32, -0.1f32)
    );
    assert_eq!(
        invoke_with_heap(&compiled, "case_pack_formatting").0,
        "1truex"
    );
    assert_eq!(invoke_with_heap(&compiled, "case_pack_empty").0, "");
    assert_eq!(
        invoke_with_heap(&compiled, "case_pack_slices_nonempty").0,
        "界1界true"
    );
    assert_eq!(invoke_with_heap(&compiled, "case_pack_slices_empty").0, "");
    assert_panics(&compiled, "case_pack_slice_bounds_panic");
    assert_panics(&compiled, "case_pack_slice_utf8_panic");
    assert_eq!(invoke_with_heap(&compiled, "case_pack_return").0, "1");
    assert_eq!(invoke_with_heap(&compiled, "case_pack_capture").0, "1truex");
    assert_eq!(
        invoke_with_heap(&compiled, "case_pack_user_dispatch").0,
        "1!word!true!"
    );

    let expected_primitives = [
        oracle("", |f| fmt::format_int(f, -42)),
        oracle("", |f| fmt::format_bool(f, true)),
        oracle("", |f| fmt::format_str(f, "hello")),
        oracle("", |f| fmt::format_char(f, '界')),
    ]
    .join("|");
    assert_eq!(
        invoke_with_heap(&compiled, "case_primitives").0,
        expected_primitives
    );
    assert_eq!(
        invoke_with_heap(&compiled, "case_pack_elem_capture").0,
        "1truex"
    );
    assert_eq!(invoke_with_heap(&compiled, "case_pack_wildcard").0, "3:0");
    assert_eq!(
        invoke_with_heap(&compiled, "case_pack_escaping").0,
        "1[1]true[true]x[x]|1[1]true[true]x[x]|"
    );
    assert_eq!(invoke_with_heap(&compiled, "case_pack_callable").0, "3:0:2");

    let expected_specs = [
        oracle("*>6", |f| fmt::format_int(f, 42)),
        oracle("^6", |f| fmt::format_str(f, "hi")),
        oracle("#x", |f| fmt::format_int(f, 42)),
        oracle("+06", |f| fmt::format_int(f, 42)),
        oracle("<width$", |f| fmt::format_int(f, 42)),
        oracle(".width$", |f| fmt::format_str(f, "abcdefgh")),
    ]
    .join("|");
    assert_eq!(
        invoke_with_heap(&compiled, "case_specs_dynamic").0,
        expected_specs
    );
    assert_eq!(
        invoke_with_heap(&compiled, "case_never_returns").0,
        "evaluated"
    );

    let expected_padding = [
        oracle("界>#8x", |f| f.pad_integral(false, "前", "é🙂")),
        oracle("界>#8x", |f| f.pad_integral(true, "", "")),
        oracle("界>6.2", |f| f.pad("界é🙂")),
    ]
    .join("|");
    assert_eq!(
        invoke_with_heap(&compiled, "case_unicode_padding").0,
        expected_padding
    );
    assert_panics(&compiled, "case_never_panics");

    let expected_ints = format!(
        "-128|-32768|-2147483648|{}|255|65535|4294967295|{}|{}",
        i64::MIN,
        u64::MAX,
        oracle("#X", |f| fmt::format_int(f, -1))
    );
    assert_eq!(
        invoke_with_heap(&compiled, "case_int_boundaries").0,
        expected_ints
    );
    assert_eq!(
        invoke_with_heap(&compiled, "case_wide_int_boundaries").0,
        format!("{}|{}", i128::MIN, u128::MAX)
    );
    let debug = oracle("?", |f| fmt::format_str(f, "hello\n\"world\u{85}\u{200b}"));
    assert_eq!(
        invoke_with_heap(&compiled, "case_debug_user_print").0,
        format!("{debug}|Record(7)")
    );
    assert_panics(&compiled, "case_neg_width_panic");
}
