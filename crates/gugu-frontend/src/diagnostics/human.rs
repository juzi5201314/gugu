//! rustc 风格的人读诊断渲染：源码片段、插入符标注与 ANSI 颜色。
//!
//! 渲染只读 [`SourceMap`] 中已注册的源码快照：span 不属于该源码表或文件未登记时，
//! 退化为头行加位置行。片段按显示宽度对齐（CJK 等宽字符按两列计），制表符展开到
//! 4 列制表位；超长行围绕标注裁剪、超长 span 保留首尾并省略中间，省略处打印 `...`。
//! 返回值不含尾随换行，块之间的空行由调用方插入。

use std::fmt::Write as _;

use unicode_width::UnicodeWidthChar;

use super::{Diagnostic, Severity};
use crate::source::{SourceMap, Span};

/// 片段最多显示的行数；超过时保留首尾并省略中间。
const SNIPPET_MAX_LINES: usize = 12;
/// 省略中间行时保留的首尾行数。
const SNIPPET_EDGE_LINES: usize = 4;
/// 制表符展开到下一个 4 列制表位。
const SNIPPET_TAB_WIDTH: usize = 4;
/// 单行超过该字符数时围绕标注裁剪。
const SNIPPET_LINE_CLIP_THRESHOLD: usize = 200;
/// 裁剪时在标注前保留的字符数。
const SNIPPET_LINE_CLIP_LEFT: usize = 60;
/// 裁剪时在标注后保留的字符数。
const SNIPPET_LINE_CLIP_RIGHT: usize = 120;

impl Diagnostic {
    /// 将诊断渲染为 rustc 风格的人读文本。
    ///
    /// 首行为 `级别[代码]: 消息`，有主源范围时追加 `--> 文件:行:列`；能从 `sources`
    /// 取回快照时再追加带行号的源码片段和 `^` 标注。`color` 为真时按级别着色：
    /// 错误红色、警告黄色、附注青色，位置行与行号栏蓝色。返回值不含尾随换行。
    pub fn render_human(&self, sources: &SourceMap, color: bool) -> String {
        let style = Style::of(self.severity, color);
        let mut output = String::with_capacity(160);
        let mut header = String::with_capacity(self.message.len() + 16);
        let _ = write!(header, "{}[{}]: ", self.severity, self.code);
        push_single_line(&mut header, &self.message);
        style.accent(&mut output, &header);
        let Some(span) = self.span.as_ref() else {
            return output;
        };
        let snippet = Snippet::build(span, sources);
        let gutter_width = snippet.as_ref().map_or(1, |snippet| snippet.width);
        output.push('\n');
        let mut prefix = " ".repeat(gutter_width);
        prefix.push_str("--> ");
        style.gutter(&mut output, &prefix);
        let _ = write!(
            output,
            "{}:{}:{}",
            span.path().display(),
            span.line(),
            span.column()
        );
        if let Some(snippet) = &snippet {
            snippet.push(&mut output, &style);
        }
        output
    }
}

/// 把消息折叠为单行：换行与回车替换为空格，避免破坏头行结构。
fn push_single_line(output: &mut String, message: &str) {
    for character in message.chars() {
        match character {
            '\n' | '\r' => output.push(' '),
            other => output.push(other),
        }
    }
}

/// 一条诊断的 ANSI 样式；关闭颜色时全部为空串。
#[derive(Clone, Copy)]
struct Style {
    /// 级别色，用于头行与插入符。
    accent: &'static str,
    /// 位置行、行号与竖线。
    gutter: &'static str,
    /// 复位序列。
    reset: &'static str,
}

impl Style {
    /// 按级别取样式；`color` 为假时全部透明。
    const fn of(severity: Severity, color: bool) -> Self {
        if !color {
            return Self {
                accent: "",
                gutter: "",
                reset: "",
            };
        }
        let accent = match severity {
            Severity::Error => "\x1b[1;31m",
            Severity::Warning => "\x1b[1;33m",
            Severity::Note => "\x1b[1;36m",
        };
        Self {
            accent,
            gutter: "\x1b[1;34m",
            reset: "\x1b[0m",
        }
    }

    /// 以级别色追加文本。
    fn accent(&self, output: &mut String, text: &str) {
        push_styled(output, self.accent, self.reset, text);
    }

    /// 以 gutter 色追加文本。
    fn gutter(&self, output: &mut String, text: &str) {
        push_styled(output, self.gutter, self.reset, text);
    }
}

fn push_styled(output: &mut String, style: &str, reset: &str, text: &str) {
    if style.is_empty() {
        output.push_str(text);
        return;
    }
    output.push_str(style);
    output.push_str(text);
    output.push_str(reset);
}

/// 一条诊断的源码片段：行号栏、源码行与插入符标注。
struct Snippet {
    /// 行号栏宽度（最大显示行号的位数）。
    width: usize,
    rows: Vec<SnippetRow>,
}

/// 片段中的一行：源码行或省略标记。
enum SnippetRow {
    /// 被省略的中间行。
    Elision,
    Line {
        /// 1 基行号。
        number: u32,
        /// 制表符展开并裁剪后的行文本。
        text: String,
        /// 标注起点的显示列（含左裁剪的 `...` 宽度）。
        underline_start: usize,
        /// 标注宽度，至少 1 列。
        underline_width: usize,
    },
}

impl Snippet {
    /// 为 `span` 构建片段；span 不属于 `sources` 的源码表或文件未登记时返回 `None`。
    ///
    /// 行号栏宽度取最大显示行号的位数；跨行 span 超过 [`SNIPPET_MAX_LINES`] 时
    /// 只保留首尾 [`SNIPPET_EDGE_LINES`] 行，中间省略。
    fn build(span: &Span, sources: &SourceMap) -> Option<Self> {
        if span.table() != sources.table() {
            return None;
        }
        let snapshot = sources.snapshot(span.file())?;
        let content = snapshot.content();
        let line_starts = snapshot.line_starts();
        let start = (span.start() as usize).min(content.len());
        let end = (span.end() as usize).min(content.len()).max(start);
        let first = line_index(line_starts, start);
        let last = if end > start {
            line_index(line_starts, end - 1)
        } else {
            first
        };
        let line_count = last - first + 1;
        let mut rows = Vec::with_capacity(line_count.min(SNIPPET_MAX_LINES));
        if line_count <= SNIPPET_MAX_LINES {
            for line in first..=last {
                rows.push(line_row(content, line_starts, line, start, end));
            }
        } else {
            for line in first..first + SNIPPET_EDGE_LINES {
                rows.push(line_row(content, line_starts, line, start, end));
            }
            rows.push(SnippetRow::Elision);
            for line in last + 1 - SNIPPET_EDGE_LINES..=last {
                rows.push(line_row(content, line_starts, line, start, end));
            }
        }
        Some(Self {
            width: (last + 1)
                .checked_ilog10()
                .map_or(1, |digits| digits as usize + 1),
            rows,
        })
    }

    /// 把片段逐行追加到输出：先打印引导 gutter 行，再按 ` 行号 | 文本` 与插入符标注
    /// 成对输出每一行。
    fn push(&self, output: &mut String, style: &Style) {
        output.push('\n');
        let mut intro = " ".repeat(self.width + 1);
        intro.push('|');
        style.gutter(output, &intro);
        for row in &self.rows {
            output.push('\n');
            match row {
                SnippetRow::Elision => {
                    let mut gutter = " ".repeat(self.width + 1);
                    gutter.push_str("...");
                    style.gutter(output, &gutter);
                }
                SnippetRow::Line {
                    number,
                    text,
                    underline_start,
                    underline_width,
                } => {
                    let mut gutter = String::with_capacity(self.width + 4);
                    let _ = write!(gutter, "{number:>width$} | ", width = self.width);
                    style.gutter(output, &gutter);
                    output.push_str(text);
                    output.push('\n');
                    let mut gutter = " ".repeat(self.width + 1);
                    gutter.push_str("| ");
                    style.gutter(output, &gutter);
                    for _ in 0..*underline_start {
                        output.push(' ');
                    }
                    style.accent(output, &"^".repeat(*underline_width));
                }
            }
        }
    }
}

/// 构建一行片段：展开制表符、裁剪窗口并计算标注列。
fn line_row(
    content: &str,
    line_starts: &[u32],
    line: usize,
    start: usize,
    end: usize,
) -> SnippetRow {
    let line_start = line_starts[line] as usize;
    let line_end = line_starts
        .get(line + 1)
        .map_or(content.len(), |next| *next as usize);
    let mut text_end = line_end;
    let bytes = content.as_bytes();
    if text_end > line_start && bytes[text_end - 1] == b'\n' {
        text_end -= 1;
    }
    if text_end > line_start && bytes[text_end - 1] == b'\r' {
        text_end -= 1;
    }
    let raw = &content[line_start..text_end];
    // 标注字节范围；空 span 或只覆盖行终止符时退化为一个列的插入符位置。
    let from_byte = start.clamp(line_start, text_end) - line_start;
    let to_byte = (end.clamp(line_start, text_end) - line_start).max(from_byte);
    // 逐字符展开：制表符按制表位、控制字符显示为空格、其余按显示宽度累计。
    // 错误路径，逐行小分配可以接受；窗口索引全部按字符（Cell）计。
    let mut cells = Vec::with_capacity(raw.len());
    let mut column = 0;
    for (byte, character) in raw.char_indices() {
        let width = match character {
            '\t' => SNIPPET_TAB_WIDTH - column % SNIPPET_TAB_WIDTH,
            other if other.is_control() => 1,
            other => other.width().unwrap_or(0),
        };
        cells.push(Cell {
            byte,
            column,
            width,
            display: if character.is_control() && character != '\t' {
                ' '
            } else {
                character
            },
        });
        column += width;
    }
    let total_width = column;
    let start_index = cells.partition_point(|cell| cell.byte < from_byte);
    let end_index = cells
        .partition_point(|cell| cell.byte < to_byte)
        .max(start_index);
    let (window_from, window_to) = if cells.len() <= SNIPPET_LINE_CLIP_THRESHOLD {
        (0, cells.len())
    } else {
        (
            start_index.saturating_sub(SNIPPET_LINE_CLIP_LEFT),
            (end_index + SNIPPET_LINE_CLIP_RIGHT).min(cells.len()),
        )
    };
    let clip_left = window_from > 0;
    let clip_right = window_to < cells.len();
    let window_column = cells
        .get(window_from)
        .map_or(total_width, |cell| cell.column);
    let window_end_column = cells.get(window_to).map_or(total_width, |cell| cell.column);
    let mut text = String::with_capacity(raw.len());
    let base = if clip_left {
        text.push_str("...");
        3
    } else {
        0
    };
    for cell in &cells[window_from..window_to] {
        if cell.display == '\t' {
            text.push_str(&" ".repeat(cell.width));
        } else {
            text.push(cell.display);
        }
    }
    if clip_right {
        text.push_str("...");
    }
    let visible_end = base + window_end_column.saturating_sub(window_column);
    let underline_start =
        base + column_at(&cells, from_byte, total_width).saturating_sub(window_column);
    let underline_end = (base
        + column_at(&cells, to_byte, total_width).saturating_sub(window_column))
    .min(visible_end.max(underline_start + 1));
    SnippetRow::Line {
        number: u32::try_from(line + 1).expect("行号适配 u32"),
        text,
        underline_start,
        underline_width: underline_end.saturating_sub(underline_start).max(1),
    }
}

/// 展开后的一个字符：字节偏移、显示列、显示宽度与实际显示的字符。
struct Cell {
    byte: usize,
    column: usize,
    width: usize,
    display: char,
}

/// 返回字节偏移处的显示列；偏移不是字符起点时返回 `fallback`。
fn column_at(cells: &[Cell], byte: usize, fallback: usize) -> usize {
    let index = cells.partition_point(|cell| cell.byte < byte);
    match cells.get(index) {
        Some(cell) if cell.byte == byte => cell.column,
        _ => fallback,
    }
}

/// 返回字节偏移所在行的稠密下标。
fn line_index(line_starts: &[u32], offset: usize) -> usize {
    line_starts
        .partition_point(|&start| start as usize <= offset)
        .saturating_sub(1)
}

#[cfg(test)]
#[path = "human_tests.rs"]
mod tests;
