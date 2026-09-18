//! `render_human` 的渲染契约：头行、位置行、片段标注、裁剪、颜色与退化形态。
//!
//! 期望字符串是渲染契约本身（格式化的可观察行为），因此逐字节断言而不是子串扫描；
//! 只有超长行裁剪这类大输入用局部断言避免把整个窗口写进测试。

use std::path::Path;

use crate::{
    Diagnostic, DiagnosticCode, Severity, SourceFileId, SourceMap, SourceSnapshot, Span,
    source::ExpansionId,
};

/// 建立单文件源码表并返回其文件 ID。
fn source_map(path: &str, source: &str) -> (SourceMap, SourceFileId) {
    let map = SourceMap::new(vec![
        SourceSnapshot::from_str(path, source).expect("快照合法"),
    ])
    .expect("逻辑路径唯一");
    let file = map.file_id(path).expect("文件已登记");
    (map, file)
}

/// 用已登记文件的字节范围构造错误诊断。
fn error(
    map: &SourceMap,
    file: SourceFileId,
    start: usize,
    end: usize,
    message: &str,
) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::ParseUnexpected,
        message,
        Some(
            map.span(file, start, end, ExpansionId::ROOT)
                .expect("span 合法"),
        ),
    )
}

#[test]
fn renders_header_location_and_caret() {
    let (map, file) = source_map("src/main.gg", "fn main() {\n    let x = ;\n}\n");
    // 第 2 行的 `;` 位于字节 24..25、第 13 列。
    let diagnostic = error(&map, file, 24, 25, "此处需要表达式");
    assert_eq!(
        diagnostic.render_human(&map, false),
        "error[E0020]: 此处需要表达式\n --> src/main.gg:2:13\n  |\n2 |     let x = ;\n  |             ^"
    );
}

#[test]
fn color_paints_header_gutter_and_caret() {
    let (map, file) = source_map("src/main.gg", "fn main() {\n    let x = ;\n}\n");
    let rendered = error(&map, file, 24, 25, "此处需要表达式").render_human(&map, true);
    assert!(rendered.starts_with("\x1b[1;31merror[E0020]: 此处需要表达式\x1b[0m\n"));
    assert!(rendered.contains("\x1b[1;34m --> \x1b[0msrc/main.gg:2:13\n"));
    assert!(rendered.contains("\x1b[1;34m  |\x1b[0m\n"));
    assert!(rendered.contains("\x1b[1;34m2 | \x1b[0m    let x = ;\n"));
    assert!(rendered.contains("\x1b[1;34m  | \x1b[0m            \x1b[1;31m^\x1b[0m"));
}

#[test]
fn warning_and_note_use_signal_colors() {
    let (map, file) = source_map("w.gg", "fn main() {}\n");
    let span = map.span(file, 3, 7, ExpansionId::ROOT).expect("span 合法");
    let warning = Diagnostic::new(
        Severity::Warning,
        DiagnosticCode::LargeCopy,
        "按值传递超过 64 字节的结构体",
        Some(span.clone()),
        u32::MAX,
    );
    let note = Diagnostic::new(
        Severity::Note,
        DiagnosticCode::LargeCopy,
        "首次声明在这里",
        Some(span),
        u32::MAX,
    );
    let warning_color = warning.render_human(&map, true);
    let note_color = note.render_human(&map, true);
    assert!(
        warning_color.starts_with("\x1b[1;33mwarning[E0056]"),
        "{warning_color}"
    );
    assert!(
        warning_color.contains("\x1b[1;33m^^^^\x1b[0m"),
        "{warning_color}"
    );
    assert!(
        note_color.starts_with("\x1b[1;36mnote[E0056]"),
        "{note_color}"
    );
    assert!(note_color.contains("\x1b[1;36m^^^^\x1b[0m"), "{note_color}");
    // 关闭颜色时不出现任何转义序列。
    assert!(!warning.render_human(&map, false).contains('\x1b'));
    assert!(!note.render_human(&map, false).contains('\x1b'));
}

#[test]
fn multiline_span_underlines_every_covered_line() {
    // 首行从标注起点到行尾、中间行整行、末行到标注终点。
    let (map, file) = source_map("m.gg", "let a = foo(\n    bar,\n    baz);\nnext\n");
    let diagnostic = error(&map, file, 8, 29, "多行标注");
    assert_eq!(
        diagnostic.render_human(&map, false),
        "error[E0020]: 多行标注\n --> m.gg:1:9\n  |\n1 | let a = foo(\n  |         ^^^^\n2 |     bar,\n  | ^^^^^^^^\n3 |     baz);\n  | ^^^^^^^"
    );
}

#[test]
fn long_spans_keep_head_and_tail() {
    let mut source = String::new();
    for index in 1..=20 {
        source.push_str(&format!("line {index}\n"));
    }
    let (map, file) = source_map("big.gg", &source);
    let diagnostic = error(&map, file, 0, source.len(), "超长片段");
    assert_eq!(
        diagnostic.render_human(&map, false),
        "error[E0020]: 超长片段\n  --> big.gg:1:1\n   |\n 1 | line 1\n   | ^^^^^^\n 2 | line 2\n   | ^^^^^^\n 3 | line 3\n   | ^^^^^^\n 4 | line 4\n   | ^^^^^^\n   ...\n17 | line 17\n   | ^^^^^^^\n18 | line 18\n   | ^^^^^^^\n19 | line 19\n   | ^^^^^^^\n20 | line 20\n   | ^^^^^^^"
    );
}

#[test]
fn tabs_expand_to_tab_stops() {
    // 制表符展开到 4 列制表位；插入符按展开后的显示列对齐。
    let (map, file) = source_map("t.gg", "fn main() {\n\tlet x = 1;\n}\n");
    // 第 2 行的 `let` 位于字节 13..16、第 2 列（标量列），显示列 4。
    let diagnostic = error(&map, file, 13, 16, "制表符");
    assert_eq!(
        diagnostic.render_human(&map, false),
        "error[E0020]: 制表符\n --> t.gg:2:2\n  |\n2 |     let x = 1;\n  |     ^^^"
    );
}

#[test]
fn wide_characters_align_by_display_width() {
    // `你好` 各占两列；插入符必须落在显示列 15 而不是标量列 13。
    let (map, file) = source_map("w.gg", "let s = \"你好\" @\n");
    let diagnostic = error(&map, file, 17, 18, "此处需要表达式");
    assert_eq!(
        diagnostic.render_human(&map, false),
        "error[E0020]: 此处需要表达式\n --> w.gg:1:14\n  |\n1 | let s = \"你好\" @\n  |                ^"
    );
}

#[test]
fn spans_outside_the_source_table_render_without_snippet() {
    let (map, _file) = source_map("a.gg", "fn main() {}\n");
    let diagnostic = Diagnostic::error(
        DiagnosticCode::SourceRead,
        "无法读取源文件 `b.gg`",
        Some(Span::detached(Path::new("b.gg"), 0, 0)),
    );
    assert_eq!(
        diagnostic.render_human(&map, false),
        "error[E0001]: 无法读取源文件 `b.gg`\n --> b.gg:1:1"
    );
}

#[test]
fn diagnostics_without_spans_render_the_header_only() {
    let (map, _file) = source_map("a.gg", "fn main() {}\n");
    let diagnostic = Diagnostic::error(DiagnosticCode::MissingMain, "缺少入口", None);
    assert_eq!(
        diagnostic.render_human(&map, false),
        "error[E0002]: 缺少入口"
    );
}

#[test]
fn empty_spans_underline_a_single_column() {
    let (map, file) = source_map("e.gg", "abc\ndef\n");
    let diagnostic = error(&map, file, 1, 1, "空 span");
    assert_eq!(
        diagnostic.render_human(&map, false),
        "error[E0020]: 空 span\n --> e.gg:1:2\n  |\n1 | abc\n  |  ^"
    );
}

#[test]
fn long_lines_are_clipped_around_the_span() {
    let line = format!("{}@{}", "x".repeat(300), "y".repeat(300));
    let (map, file) = source_map("l.gg", &format!("{line}\n"));
    let diagnostic = error(&map, file, 300, 301, "超长行");
    let rendered = diagnostic.render_human(&map, false);
    // 标注前保留 60 字符、后保留 120 字符，两侧以 `...` 标记裁剪。
    assert!(
        rendered.contains(&format!("...{}@", "x".repeat(60))),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!("{}...", "y".repeat(120))),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!("  | {}^", " ".repeat(63))),
        "{rendered}"
    );
    assert!(!rendered.contains(&"x".repeat(61)), "{rendered}");
}

#[test]
fn crlf_terminators_are_not_displayed() {
    let (map, file) = source_map("c.gg", "first\r\nsecond\r\n");
    let diagnostic = error(&map, file, 7, 9, "CRLF");
    let rendered = diagnostic.render_human(&map, false);
    assert!(rendered.ends_with("2 | second\n  | ^^"), "{rendered}");
    assert!(!rendered.contains('\r'), "{rendered}");
}

#[test]
fn gutter_width_follows_the_line_number() {
    let mut source = String::new();
    for index in 1..=105 {
        source.push_str(&format!("line {index}\n"));
    }
    let (map, file) = source_map("h.gg", &source);
    let at = source.find("line 100").expect("存在第 100 行");
    let diagnostic = error(&map, file, at, at + 4, "宽行号");
    let rendered = diagnostic.render_human(&map, false);
    assert!(
        rendered.starts_with("error[E0020]: 宽行号\n   --> h.gg:100:1\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("\n100 | line 100\n    | ^^^^"),
        "{rendered}"
    );
}

#[test]
fn message_line_breaks_are_flattened() {
    let (map, _file) = source_map("a.gg", "fn main() {}\n");
    let diagnostic = Diagnostic::error(
        DiagnosticCode::MacroBoundaryError,
        "宏脚本返回: \n坏消息\r文本",
        None,
    );
    assert_eq!(
        diagnostic.render_human(&map, false),
        "error[E0051]: 宏脚本返回:  坏消息 文本"
    );
}
