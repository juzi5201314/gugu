use std::fmt;

/// 一个编译 action 中的源码范围。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Span {
    path: std::path::PathBuf,
    start: u32,
    end: u32,
}

impl Span {
    pub(crate) fn new(path: &std::path::Path, start: usize, end: usize) -> Self {
        debug_assert!(start <= u32::MAX as usize);
        debug_assert!(end <= u32::MAX as usize);
        Self {
            path: path.to_path_buf(),
            start: start as u32,
            end: end as u32,
        }
    }

    /// 返回该范围所属的源码路径。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 返回半开字节范围的起点。
    pub fn start(&self) -> u32 {
        self.start
    }

    /// 返回半开字节范围的终点。
    pub fn end(&self) -> u32 {
        self.end
    }
}

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

/// 阶段 1 使用的稳定诊断代码。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DiagnosticCode {
    /// 源文件无法读取。
    SourceRead,
    /// 源文件没有可执行入口。
    MissingMain,
    /// bootstrap 前端无法识别源文件结构。
    MalformedSource,
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            Self::SourceRead => "E0001",
            Self::MissingMain => "E0002",
            Self::MalformedSource => "E0003",
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
            Some(Span::new(path, 0, 0)),
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

    /// 将诊断渲染为阶段 1 的文本格式。
    pub fn render_text(&self) -> String {
        match &self.span {
            Some(span) => format!(
                "{}[{}] {}:{}:{}: {}",
                self.severity,
                self.code,
                span.path.display(),
                1,
                span.start + 1,
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
            let left_key = left
                .span
                .as_ref()
                .map(|span| (span.path.as_os_str(), span.start, span.end));
            let right_key = right
                .span
                .as_ref()
                .map(|span| (span.path.as_os_str(), span.start, span.end));
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
