use super::{CompileRequest, Compiler, TargetName, formatting::oracle, harness::Harness};
use gugu_sema::string::fmt;

fn model(out: &mut fmt::Formatter, kind: &str, empty: bool) {
    let mut builder = match kind {
        "struct" => out.debug_struct("Record"),
        "tuple" => out.debug_tuple("Record"),
        "list" => out.debug_list(),
        "set" => out.debug_set(),
        "map" => out.debug_map(),
        _ => unreachable!(),
    };
    if !empty {
        if kind == "struct" {
            builder.field(|f| f.write_str("number"), |f| fmt::format_float(f, 0.1));
            builder.field(|f| f.write_str("text"), |f| fmt::format_str(f, "x\n\""));
        } else if kind == "map" {
            builder.field(
                |f| fmt::format_str(f, "number"),
                |f| fmt::format_float(f, 0.1),
            );
            builder.field(
                |f| fmt::format_str(f, "text"),
                |f| fmt::format_str(f, "x\n\""),
            );
        } else {
            builder.entry(|f| fmt::format_float(f, 0.1));
            builder.entry(|f| fmt::format_str(f, "x\n\""));
        }
    }
    builder.finish();
    builder.finish();
}

fn invoke_string(compiled: &super::Compilation, name: &str) -> String {
    let mut harness = Harness::new(compiled);
    let mut memory = vec![0; 262144];
    let result = harness.invoke("main.gg", name, &[], &mut memory);
    let start = result[0] as usize;
    String::from_utf8(memory[start..start + result[1] as usize].to_vec()).unwrap()
}

#[test]
fn debug_builders_contracts() {
    let source = r#"
struct RecordStruct { empty: bool, number: f64, text: string }
impl Debug for RecordStruct {
    fn debug(self: &Self, out: &Formatter) {
        let b = out.debug_struct("Record")
        if !self.empty { b.field("number", &self.number).field("text", &self.text) }
        b.finish()
        b.finish()
    }
}
fn render_struct() string {
    let empty = RecordStruct { empty: true, number: 0.1, text: "x\n\"" }
    let full = RecordStruct { empty: false, number: 0.1, text: "x\n\"" }
    f"{empty:?}|{empty:#?}|{full:?}|{full:#?}|{full:*>60?}"
}

struct RecordTuple { empty: bool, number: f64, text: string }
impl Debug for RecordTuple {
    fn debug(self: &Self, out: &Formatter) {
        let b = out.debug_tuple("Record")
        if !self.empty { b.entry(&self.number).entry(&self.text) }
        b.finish()
        b.finish()
    }
}
fn render_tuple() string {
    let empty = RecordTuple { empty: true, number: 0.1, text: "x\n\"" }
    let full = RecordTuple { empty: false, number: 0.1, text: "x\n\"" }
    f"{empty:?}|{empty:#?}|{full:?}|{full:#?}|{full:*>60?}"
}

struct RecordList { empty: bool, number: f64, text: string }
impl Debug for RecordList {
    fn debug(self: &Self, out: &Formatter) {
        let b = out.debug_list()
        if !self.empty { b.entry(&self.number).entry(&self.text) }
        b.finish()
        b.finish()
    }
}
fn render_list() string {
    let empty = RecordList { empty: true, number: 0.1, text: "x\n\"" }
    let full = RecordList { empty: false, number: 0.1, text: "x\n\"" }
    f"{empty:?}|{empty:#?}|{full:?}|{full:#?}|{full:*>60?}"
}

struct RecordSet { empty: bool, number: f64, text: string }
impl Debug for RecordSet {
    fn debug(self: &Self, out: &Formatter) {
        let b = out.debug_set()
        if !self.empty { b.entry(&self.number).entry(&self.text) }
        b.finish()
        b.finish()
    }
}
fn render_set() string {
    let empty = RecordSet { empty: true, number: 0.1, text: "x\n\"" }
    let full = RecordSet { empty: false, number: 0.1, text: "x\n\"" }
    f"{empty:?}|{empty:#?}|{full:?}|{full:#?}|{full:*>60?}"
}

struct RecordMap { empty: bool, number: f64, text: string }
impl Debug for RecordMap {
    fn debug(self: &Self, out: &Formatter) {
        let b = out.debug_map()
        if !self.empty {
            let number = "number"
            let text = "text"
            b.entry(&number, &self.number).entry(&text, &self.text)
        }
        b.finish()
        b.finish()
    }
}
fn render_map() string {
    let empty = RecordMap { empty: true, number: 0.1, text: "x\n\"" }
    let full = RecordMap { empty: false, number: 0.1, text: "x\n\"" }
    f"{empty:?}|{empty:#?}|{full:?}|{full:#?}|{full:*>60?}"
}

struct NestedRecord { n: f64, s: string }
impl Debug for NestedRecord {
    fn debug(self: &Self, out: &Formatter) {
        out.debug_struct("Record").field("n", &self.n).field("s", &self.s).finish()
    }
}
struct NestedList { record: NestedRecord }
impl Debug for NestedList {
    fn debug(self: &Self, out: &Formatter) { out.debug_list().entry(&self.record).finish() }
}
struct NestedMap { list: NestedList }
impl Debug for NestedMap {
    fn debug(self: &Self, out: &Formatter) {
        let key = "key"
        out.debug_map().entry(&key, &self.list).finish()
    }
}
fn render_nested() string {
    let value = NestedMap { list: NestedList { record: NestedRecord { n: 0.1, s: "hello" } } }
    f"{value:?}|{value:#?}"
}

fn entry_after_finish() string {
    let out = Formatter::new()
    let b = out.debug_list()
    b.finish()
    let value = 1
    b.entry(&value)
    out.finish()
}

fn main() {
    _ = render_struct()
    _ = render_tuple()
    _ = render_list()
    _ = render_set()
    _ = render_map()
    _ = render_nested()
    _ = entry_after_finish()
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

    for (kind, entry) in [
        ("struct", "render_struct"),
        ("tuple", "render_tuple"),
        ("list", "render_list"),
        ("set", "render_set"),
        ("map", "render_map"),
    ] {
        let expected = [
            ("?", true),
            ("#?", true),
            ("?", false),
            ("#?", false),
            ("*>60?", false),
        ]
        .map(|(spec, empty)| oracle(spec, |f| model(f, kind, empty)))
        .join("|");
        assert_eq!(invoke_string(&compiled, entry), expected, "{kind}");
    }

    let expected_nested = ["?", "#?"]
        .map(|spec| {
            oracle(spec, |f| {
                f.debug_map()
                    .field(
                        |f| fmt::format_str(f, "key"),
                        |f| {
                            f.debug_list()
                                .entry(|f| {
                                    f.debug_struct("Record")
                                        .field(|f| f.write_str("n"), |f| fmt::format_float(f, 0.1))
                                        .field(
                                            |f| f.write_str("s"),
                                            |f| fmt::format_str(f, "hello"),
                                        )
                                        .finish();
                                })
                                .finish();
                        },
                    )
                    .finish();
            })
        })
        .join("|");
    assert_eq!(invoke_string(&compiled, "render_nested"), expected_nested);

    let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        invoke_string(&compiled, "entry_after_finish");
    }))
    .expect_err("finish 后再调用 entry 必须触发 Runtime(Panic)");
    let message = error
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| error.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(message.contains("Runtime(Panic)"), "{message}");
}
