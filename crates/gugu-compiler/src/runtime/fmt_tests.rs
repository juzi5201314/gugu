use super::fmt::*;
use crate::frontend::string::{ParsedCount, parse_format};

fn spec(text: &str) -> ResolvedSpec {
    parse_format(text)
        .expect("测试格式说明合法")
        .try_map(|count| match count {
            ParsedCount::Fixed(value) => Ok::<u64, ()>(value),
            ParsedCount::Name(_) => Err(()),
        })
        .expect("测试不使用动态计数")
}

fn render(text: &str, write: impl FnOnce(&mut Formatter)) -> String {
    let mut out = Formatter::new(spec(text));
    write(&mut out);
    out.finish()
}

#[test]
fn integers_follow_sign_alternate_zero_and_radix_rules() {
    assert_eq!(render("", |out| format_int(out, 42)), "42");
    assert_eq!(render("+", |out| format_int(out, 42)), "+42");
    assert_eq!(render(" ", |out| format_int(out, 42)), " 42");
    assert_eq!(render("", |out| format_int(out, -42)), "-42");
    assert_eq!(render("08x", |out| format_int(out, 255)), "000000ff");
    assert_eq!(render("#010b", |out| format_int(out, 5)), "0b00000101");
    assert_eq!(render("#o", |out| format_int(out, 8)), "0o10");
    assert_eq!(render("X", |out| format_int(out, 255)), "FF");
    assert_eq!(render("x", |out| format_int(out, -1)), "ffffffffffffffff");
    assert_eq!(render("6", |out| format_int(out, 7)), "     7");
    assert_eq!(render("<6", |out| format_int(out, 7)), "7     ");
    assert_eq!(render("*^7", |out| format_int(out, 7)), "***7***");
    assert_eq!(render("+05", |out| format_int(out, 7)), "+0007");
}

#[test]
fn floats_follow_precision_exponent_and_non_finite_rules() {
    assert_eq!(render("", |out| format_float(out, 1.5)), "1.5");
    assert_eq!(render("", |out| format_float(out, 1.0)), "1");
    assert_eq!(render("?", |out| format_float(out, 1.0)), "1.0");
    assert_eq!(render(".2", |out| format_float(out, 3.14159)), "3.14");
    assert_eq!(render("+.1", |out| format_float(out, 2.0)), "+2.0");
    assert_eq!(render("e", |out| format_float(out, 1234.5)), "1.2345e3");
    assert_eq!(render(".2E", |out| format_float(out, 1234.5)), "1.23E3");
    assert_eq!(render("08.2", |out| format_float(out, -1.5)), "-0001.50");
    assert_eq!(
        render("06", |out| format_float(out, f64::INFINITY)),
        "   inf"
    );
    assert_eq!(
        render("", |out| format_float(out, f64::NEG_INFINITY)),
        "-inf"
    );
    assert_eq!(render("", |out| format_float(out, f64::NAN)), "NaN");
}

#[test]
fn text_values_pad_truncate_and_escape() {
    assert_eq!(render("", |out| format_str(out, "gugu")), "gugu");
    assert_eq!(render(".2", |out| format_str(out, "gugu")), "gu");
    assert_eq!(render(">6", |out| format_str(out, "gugu")), "  gugu");
    assert_eq!(render("6", |out| format_str(out, "gugu")), "gugu  ");
    assert_eq!(
        render("?", |out| format_str(out, "a\"b\n")),
        "\"a\\\"b\\n\""
    );
    assert_eq!(render("", |out| format_char(out, '中')), "中");
    assert_eq!(render("?", |out| format_char(out, '\'')), "'\\''");
    assert_eq!(render("7", |out| format_bool(out, true)), "true   ");
    assert_eq!(render("", |out| format_bool(out, false)), "false");
}

#[test]
fn structured_debug_builders_render_compact_and_pretty() {
    let list = render("?", |out| {
        out.debug_list()
            .entry(|out| format_int(out, 1))
            .entry(|out| format_int(out, 2))
            .finish();
    });
    assert_eq!(list, "[1, 2]");
    let tuple = render("?", |out| {
        out.debug_tuple("Some")
            .entry(|out| format_str(out, "x"))
            .finish();
    });
    assert_eq!(tuple, "Some(\"x\")");
    let map = render("?", |out| {
        out.debug_map()
            .field(|out| format_int(out, 1), |out| format_bool(out, true))
            .finish();
    });
    assert_eq!(map, "{1: true}");
    assert_eq!(render("?", |out| out.debug_map().finish()), "{}");
    let unit = render("?", |out| out.debug_struct("Bare").finish());
    assert_eq!(unit, "Bare");
    let record = render("?", |out| {
        out.debug_struct("Point")
            .field(|out| out.write_str("x"), |out| format_int(out, 1))
            .field(|out| out.write_str("y"), |out| format_int(out, 2))
            .finish();
    });
    assert_eq!(record, "Point { x: 1, y: 2 }");
    let pretty = render("#?", |out| {
        out.debug_struct("Point")
            .field(|out| out.write_str("x"), |out| format_int(out, 1))
            .field(
                |out| out.write_str("tags"),
                |out| {
                    out.debug_list().entry(|out| format_int(out, 2)).finish();
                },
            )
            .finish();
    });
    assert_eq!(
        pretty,
        "Point {\n    x: 1,\n    tags: [\n        2,\n    ],\n}"
    );
}

#[test]
fn negative_dynamic_counts_are_faults() {
    let spec = parse_format("width$.precision$")
        .expect("动态计数合法")
        .try_map(|_| Ok::<i64, ()>(3))
        .expect("映射成功");
    assert!(resolve_counts(spec.clone()).is_ok());
    let negative = spec.try_map(|_| Ok::<i64, ()>(-1)).expect("映射成功");
    assert_eq!(resolve_counts(negative), Err(FormatFault::NegativeCount));
}
