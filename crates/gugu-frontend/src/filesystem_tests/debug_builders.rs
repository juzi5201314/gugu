use super::formatting::{oracle, render};
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

fn check(kind: &str) {
    let (constructor, entries) = match kind {
        "struct" => (
            "out.debug_struct(\"Record\")",
            "b.field(\"number\", &self.number).field(\"text\", &self.text)",
        ),
        "tuple" => (
            "out.debug_tuple(\"Record\")",
            "b.entry(&self.number).entry(&self.text)",
        ),
        "list" => (
            "out.debug_list()",
            "b.entry(&self.number).entry(&self.text)",
        ),
        "set" => ("out.debug_set()", "b.entry(&self.number).entry(&self.text)"),
        "map" => (
            "out.debug_map()",
            "let number = \"number\"\nlet text = \"text\"\nb.entry(&number, &self.number).entry(&text, &self.text)",
        ),
        _ => unreachable!(),
    };
    let source = format!(
        r#"
        struct Record {{ empty: bool, number: f64, text: string }}
        impl Debug for Record {{
            fn debug(self: &Self, out: &Formatter) {{
                let b = {constructor}
                if !self.empty {{ {entries} }}
                b.finish()
                b.finish()
            }}
        }}
        fn render() string {{
            let empty = Record {{ empty: true, number: 0.1, text: "x\n\"" }}
            let full = Record {{ empty: false, number: 0.1, text: "x\n\"" }}
            f"{{empty:?}}|{{empty:#?}}|{{full:?}}|{{full:#?}}|{{full:*>60?}}"
        }}
    "#
    );
    let expected = [
        ("?", true),
        ("#?", true),
        ("?", false),
        ("#?", false),
        ("*>60?", false),
    ]
    .map(|(spec, empty)| oracle(spec, |f| model(f, kind, empty)))
    .join("|");
    assert_eq!(render(&source), expected);
}

#[test]
fn debug_struct() {
    check("struct");
}
#[test]
fn debug_tuple() {
    check("tuple");
}
#[test]
fn debug_list() {
    check("list");
}
#[test]
fn debug_set() {
    check("set");
}
#[test]
fn debug_map() {
    check("map");
}

#[test]
fn nested_debug_builders() {
    let source = r#"
        struct Record { n: f64, s: string }
        impl Debug for Record {
            fn debug(self: &Self, out: &Formatter) {
                out.debug_struct("Record").field("n", &self.n).field("s", &self.s).finish()
            }
        }
        struct List { record: Record }
        impl Debug for List {
            fn debug(self: &Self, out: &Formatter) { out.debug_list().entry(&self.record).finish() }
        }
        struct Map { list: List }
        impl Debug for Map {
            fn debug(self: &Self, out: &Formatter) {
                let key = "key"
                out.debug_map().entry(&key, &self.list).finish()
            }
        }
        fn render() string {
            let value = Map { list: List { record: Record { n: 0.1, s: "hello" } } }
            f"{value:?}|{value:#?}"
        }
    "#;
    let expected = ["?", "#?"]
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
    assert_eq!(render(source), expected);
}

#[test]
#[should_panic(expected = "Runtime(Panic)")]
fn debug_entry_after_finish_panics() {
    render(
        r#"fn render() string {
        let out = Formatter::new()
        let b = out.debug_list()
        b.finish()
        let value = 1
        b.entry(&value)
        out.finish()
    }"#,
    );
}
