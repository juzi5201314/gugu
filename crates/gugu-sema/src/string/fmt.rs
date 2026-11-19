//! `std.fmt` 的确定性参照模型。
//!
//! `Formatter` 只写入当前构建中的 string，不执行 I/O。已解析的格式说明由 f-string
//! 编译期给出；格式 trait 实现只能调用文本、char、padding 与结构化 debug 方法，
//! 不能读取或改变说明本身。

use crate::string::{Alignment, FormatKind, FormatSpec, Sign};

/// 格式化在语言里 panic 的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatFault {
    /// `name$` 引用的动态 width / precision 为负。
    NegativeCount,
}

/// 计数已经求值的格式说明。
pub type ResolvedSpec = FormatSpec<u64>;

/// 把动态 `int` 计数变成非负计数；负值是运行时 panic。
pub fn resolve_counts(spec: FormatSpec<i64>) -> Result<ResolvedSpec, FormatFault> {
    spec.try_map(|count| u64::try_from(count).map_err(|_| FormatFault::NegativeCount))
}

/// 一次插值的写入目标。
#[derive(Debug)]
pub struct Formatter {
    out: String,
    spec: ResolvedSpec,
}

impl Formatter {
    /// 按已解析说明建立空的写入目标。
    pub fn new(spec: ResolvedSpec) -> Self {
        Self {
            out: String::new(),
            spec,
        }
    }

    /// 取出已写入的文本。
    pub fn finish(self) -> String {
        self.out
    }

    /// 原样写入文本，不做 padding。
    pub fn write_str(&mut self, text: &str) {
        self.out.push_str(text);
    }

    /// 原样写入一个标量。
    pub fn write_char(&mut self, character: char) {
        self.out.push(character);
    }

    /// 文本写入：precision 按标量截断，width 按 fill/alignment 补齐，默认左对齐。
    pub fn pad(&mut self, text: &str) {
        let truncated: String = match self.spec.precision {
            Some(limit) => text.chars().take(clamp(limit)).collect(),
            None => text.to_owned(),
        };
        self.write_aligned(&truncated, Alignment::Left);
    }

    /// 整数类写入：符号、`#` 前缀、零填充与 width，默认右对齐。
    pub fn pad_integral(&mut self, non_negative: bool, prefix: &str, digits: &str) {
        let sign = match (non_negative, self.spec.sign) {
            (false, _) => "-",
            (true, Some(Sign::Plus)) => "+",
            (true, Some(Sign::Space)) => " ",
            (true, _) => "",
        };
        let prefix = if self.spec.alternate { prefix } else { "" };
        if self.spec.zero {
            let head = sign.chars().count() + prefix.chars().count();
            let width = self.spec.width.map_or(0, clamp).saturating_sub(head);
            let zeros = width.saturating_sub(digits.chars().count());
            self.out.push_str(sign);
            self.out.push_str(prefix);
            self.out.extend(std::iter::repeat_n('0', zeros));
            self.out.push_str(digits);
            return;
        }
        let body = format!("{sign}{prefix}{digits}");
        self.write_aligned(&body, Alignment::Right);
    }

    /// 结构化 debug：`Name(a, b)`。
    pub fn debug_tuple(&mut self, name: &str) -> DebugBuilder<'_> {
        DebugBuilder::new(self, name, DebugStyle::Tuple)
    }

    /// 结构化 debug：`[a, b]`。
    pub fn debug_list(&mut self) -> DebugBuilder<'_> {
        DebugBuilder::new(self, "", DebugStyle::List)
    }

    pub fn debug_set(&mut self) -> DebugBuilder<'_> {
        DebugBuilder::new(self, "", DebugStyle::Set)
    }

    /// 结构化 debug：`{k: v, ...}`。
    pub fn debug_map(&mut self) -> DebugBuilder<'_> {
        DebugBuilder::new(self, "", DebugStyle::Map)
    }

    /// 结构化 debug：`Name { field: value }`；没有字段时只写名字。
    pub fn debug_struct(&mut self, name: &str) -> DebugBuilder<'_> {
        DebugBuilder::new(self, name, DebugStyle::Struct)
    }

    fn write_aligned(&mut self, body: &str, default: Alignment) {
        let width = self.spec.width.map_or(0, clamp);
        let length = body.chars().count();
        let padding = width.saturating_sub(length);
        let (before, after) = match self.spec.alignment.unwrap_or(default) {
            Alignment::Left => (0, padding),
            Alignment::Right => (padding, 0),
            Alignment::Center => (padding / 2, padding - padding / 2),
        };
        self.out.extend(std::iter::repeat_n(self.spec.fill, before));
        self.out.push_str(body);
        self.out.extend(std::iter::repeat_n(self.spec.fill, after));
    }

    fn child_spec(&self) -> ResolvedSpec {
        ResolvedSpec {
            alternate: self.spec.alternate,
            kind: FormatKind::Debug,
            ..ResolvedSpec::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DebugStyle {
    Tuple,
    List,
    Map,
    Struct,
    Set,
}

impl DebugStyle {
    /// (首条目前缀, 结束定界符)；struct 的开定界符随首个字段出现。
    fn delimiters(self) -> (&'static str, &'static str) {
        match self {
            Self::Tuple => ("(", ")"),
            Self::List => ("[", "]"),
            Self::Map | Self::Set => ("{", "}"),
            Self::Struct => (" { ", " }"),
        }
    }
}

/// 结构化 debug 写入器；`#` 时按四空格缩进逐行展开。
pub struct DebugBuilder<'a> {
    formatter: &'a mut Formatter,
    style: DebugStyle,
    entries: usize,
    finished: bool,
}

impl<'a> DebugBuilder<'a> {
    fn new(formatter: &'a mut Formatter, name: &str, style: DebugStyle) -> Self {
        formatter.out.push_str(name);
        if style != DebugStyle::Struct {
            formatter.out.push_str(style.delimiters().0);
        }
        Self {
            formatter,
            style,
            entries: 0,
            finished: false,
        }
    }

    /// 追加一个匿名条目（tuple / list）。
    pub fn entry(&mut self, value: impl FnOnce(&mut Formatter)) -> &mut Self {
        self.separator();
        let rendered = self.render(value);
        self.formatter.out.push_str(&rendered);
        self
    }

    /// 追加一个带键条目（struct 字段或 map 键值）。
    pub fn field(
        &mut self,
        key: impl FnOnce(&mut Formatter),
        value: impl FnOnce(&mut Formatter),
    ) -> &mut Self {
        self.separator();
        let key = self.render(key);
        let value = self.render(value);
        self.formatter.out.push_str(&key);
        self.formatter.out.push_str(": ");
        self.formatter.out.push_str(&value);
        self
    }

    /// 写入结束定界符。
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let pretty = self.formatter.spec.alternate;
        let close = self.style.delimiters().1;
        if self.style == DebugStyle::Struct && self.entries == 0 {
            return;
        }
        if pretty && self.entries > 0 {
            self.formatter.out.push_str(",\n");
            self.formatter.out.push_str(close.trim_start());
        } else {
            self.formatter.out.push_str(close);
        }
    }

    fn separator(&mut self) {
        assert!(!self.finished, "debug builder already finished");
        let pretty = self.formatter.spec.alternate;
        let first = self.entries == 0;
        self.entries += 1;
        if self.style == DebugStyle::Struct && first {
            self.formatter
                .out
                .push_str(if pretty { " {\n    " } else { " { " });
            return;
        }
        match (pretty, first) {
            (true, true) => self.formatter.out.push_str("\n    "),
            (true, false) => self.formatter.out.push_str(",\n    "),
            (false, true) => {}
            (false, false) => self.formatter.out.push_str(", "),
        }
    }

    fn render(&mut self, value: impl FnOnce(&mut Formatter)) -> String {
        let mut child = Formatter::new(self.formatter.child_spec());
        value(&mut child);
        let text = child.finish();
        if self.formatter.spec.alternate {
            text.replace('\n', "\n    ")
        } else {
            text
        }
    }
}

/// 整数按格式码写入：十进制走 Print/Debug，进制格式按 64 位补码，`#` 加前缀。
pub fn format_int(out: &mut Formatter, value: i64) {
    let kind = out.spec.kind;
    let bits = value as u64;
    let (prefix, digits) = match kind {
        FormatKind::Binary => ("0b", format!("{bits:b}")),
        FormatKind::Octal => ("0o", format!("{bits:o}")),
        FormatKind::LowerHex => ("0x", format!("{bits:x}")),
        FormatKind::UpperHex => ("0x", format!("{bits:X}")),
        _ => ("", value.unsigned_abs().to_string()),
    };
    let non_negative = value >= 0 || !matches!(kind, FormatKind::Print | FormatKind::Debug);
    out.pad_integral(non_negative, prefix, &digits);
}

macro_rules! float_formatter {
    ($name:ident, $ty:ty) => {
        /// 使用源浮点类型的 Rust 格式化，不经 binary64 扩宽。
        pub fn $name(out: &mut Formatter, value: $ty) {
            let precision = out.spec.precision.map(clamp);
            let digits = match (out.spec.kind, precision) {
                (FormatKind::LowerExponent, Some(p)) => format!("{:.*e}", p, value.abs()),
                (FormatKind::LowerExponent, None) => format!("{:e}", value.abs()),
                (FormatKind::UpperExponent, Some(p)) => format!("{:.*E}", p, value.abs()),
                (FormatKind::UpperExponent, None) => format!("{:E}", value.abs()),
                (FormatKind::Debug, Some(p)) => format!("{:.*?}", p, value.abs()),
                (FormatKind::Debug, None) => format!("{:?}", value.abs()),
                (_, Some(p)) => format!("{:.*}", p, value.abs()),
                (_, None) => format!("{}", value.abs()),
            };
            if value.is_finite() {
                out.pad_integral(!value.is_sign_negative(), "", &digits);
            } else if value.is_nan() {
                out.write_aligned("NaN", Alignment::Right);
            } else {
                let sign = match (value.is_sign_negative(), out.spec.sign) {
                    (true, _) => "-",
                    (_, Some(Sign::Plus)) => "+",
                    (_, Some(Sign::Space)) => " ",
                    _ => "",
                };
                out.write_aligned(&format!("{sign}{digits}"), Alignment::Right);
            }
        }
    };
}

float_formatter!(format_float, f64);
float_formatter!(format_float32, f32);

/// bool 只有 Print/Debug：`true` / `false`。
pub fn format_bool(out: &mut Formatter, value: bool) {
    out.pad(if value { "true" } else { "false" });
}

/// char：Print 原样，Debug 加单引号并转义。
pub fn format_char(out: &mut Formatter, value: char) {
    if out.spec.kind == FormatKind::Debug {
        let escaped: String = value.escape_debug().collect();
        out.pad(&format!("'{escaped}'"));
    } else {
        out.pad(&value.to_string());
    }
}

/// string：Print 按 precision 截断，Debug 加双引号并转义、不截断。
pub fn format_str(out: &mut Formatter, value: &str) {
    if out.spec.kind == FormatKind::Debug {
        let escaped: String = value.escape_debug().collect();
        out.write_aligned(&format!("\"{escaped}\""), Alignment::Left);
    } else {
        out.pad(value);
    }
}

fn clamp(count: u64) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX)
}
