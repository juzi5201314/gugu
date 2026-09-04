use std::fmt;

use crate::source::{SourceError, Span};

/// 诊断严重级别。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Severity {
    /// 阻止生成结果的错误。
    Error,
    /// 不阻止生成结果的警告。
    Warning,
}

impl fmt::Display for Severity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => formatter.write_str("error"),
            Self::Warning => formatter.write_str("warning"),
        }
    }
}

/// 稳定的源码、清单与路径诊断代码。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DiagnosticCode {
    /// 源文件无法读取。
    SourceRead,
    /// 源文件没有可执行入口。
    MissingMain,
    /// bootstrap 前端无法识别源文件结构。
    MalformedSource,
    /// 源文件不是合法 UTF-8。
    InvalidUtf8,
    /// 源文件含有 BOM。
    SourceBom,
    /// 源码逻辑路径无效。
    InvalidSourcePath,
    /// span 越出源码范围。
    SpanOutOfBounds,
    /// 源文件超过 `u32` 字节范围。
    SourceTooLarge,
    /// 字面量或属性括号未闭合。
    LexUnterminated,
    /// 块注释未闭合。
    LexUnterminatedComment,
    /// 未知或非法转义。
    LexInvalidEscape,
    /// 数字记号非法。
    LexInvalidNumeric,
    /// 无法形成合法记号。
    LexInvalidToken,
    /// 未知属性名。
    LexUnknownAttribute,
    /// 属性参数形状非法。
    LexInvalidAttributeArg,
    /// 非法 Unicode scalar。
    LexInvalidUnicodeScalar,
    /// C 字符串含内嵌 0 字节。
    LexCStringNul,
    /// 字节字符不是恰好一个字节。
    LexInvalidByteChar,
    /// f-string 格式说明非法。
    LexInvalidFormatSpec,
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            Self::SourceRead => "E0001",
            Self::MissingMain => "E0002",
            Self::MalformedSource => "E0003",
            Self::InvalidUtf8 => "E0004",
            Self::SourceBom => "E0005",
            Self::InvalidSourcePath => "E0006",
            Self::SpanOutOfBounds => "E0007",
            Self::SourceTooLarge => "E0008",
            Self::LexUnterminated => "E0009",
            Self::LexUnterminatedComment => "E0010",
            Self::LexInvalidEscape => "E0011",
            Self::LexInvalidNumeric => "E0012",
            Self::LexInvalidToken => "E0013",
            Self::LexUnknownAttribute => "E0014",
            Self::LexInvalidAttributeArg => "E0015",
            Self::LexInvalidUnicodeScalar => "E0016",
            Self::LexCStringNul => "E0017",
            Self::LexInvalidByteChar => "E0018",
            Self::LexInvalidFormatSpec => "E0019",
        };
        formatter.write_str(code)
    }
}

/// 一条可排序的编译诊断。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    severity: Severity,
    code: DiagnosticCode,
    message: String,
    span: Option<Span>,
}

impl Diagnostic {
    /// 创建带源码范围的错误诊断。
    pub(crate) fn error(
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Option<Span>,
    ) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            span,
        }
    }

    /// 创建文件读取错误诊断。
    pub(crate) fn source_read(path: &std::path::Path, error: &std::io::Error) -> Self {
        Self::error(
            DiagnosticCode::SourceRead,
            format!("无法读取源文件 `{}`：{error}", path.display()),
            Some(Span::detached(path, 0, 0)),
        )
    }

    /// 创建源码快照校验错误诊断。
    pub(crate) fn source_error(error: &SourceError) -> Self {
        let (code, path, message, offset) = match error {
            SourceError::TooLarge { path } => {
                (DiagnosticCode::SourceTooLarge, path, error.to_string(), 0)
            }
            SourceError::InvalidUtf8 { path, offset } => (
                DiagnosticCode::InvalidUtf8,
                path,
                error.to_string(),
                *offset as usize,
            ),
            SourceError::Bom { path } => (DiagnosticCode::SourceBom, path, error.to_string(), 0),
            SourceError::InvalidPath { path } => (
                DiagnosticCode::InvalidSourcePath,
                path,
                error.to_string(),
                0,
            ),
        };
        Self::error(
            code,
            message,
            Some(Span::detached(std::path::Path::new(path), offset, offset)),
        )
    }

    /// 返回诊断级别。
    pub fn severity(&self) -> Severity {
        self.severity
    }

    /// 返回稳定诊断代码。
    pub fn code(&self) -> DiagnosticCode {
        self.code
    }

    /// 返回诊断消息。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 返回主源码范围。
    pub fn span(&self) -> Option<&Span> {
        self.span.as_ref()
    }

    /// 将诊断渲染为阶段 3 的文本格式。
    pub fn render_text(&self) -> String {
        match &self.span {
            Some(span) => format!(
                "{}[{}] {}:{}:{}: {}",
                self.severity,
                self.code,
                span.path().display(),
                span.line(),
                span.column(),
                self.message
            ),
            None => format!("{}[{}]: {}", self.severity, self.code, self.message),
        }
    }
}

/// 按规范顺序保存的一组编译诊断。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}
impl Diagnostics {
    pub(crate) fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    pub(crate) fn sort(&mut self) {
        self.items.sort_by(|left, right| {
            let left_key = left.span.as_ref().map(|span| {
                (
                    span.path().as_os_str(),
                    span.start(),
                    span.end(),
                    span.expansion(),
                )
            });
            let right_key = right.span.as_ref().map(|span| {
                (
                    span.path().as_os_str(),
                    span.start(),
                    span.end(),
                    span.expansion(),
                )
            });
            left_key
                .cmp(&right_key)
                .then(left.severity.cmp(&right.severity))
                .then(left.code.cmp(&right.code))
        });
    }

    /// 返回诊断切片。
    pub fn items(&self) -> &[Diagnostic] {
        &self.items
    }

    /// 判断是否存在错误。
    pub fn has_errors(&self) -> bool {
        self.items
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error)
    }
}
