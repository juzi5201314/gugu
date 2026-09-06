use std::fmt;

use crate::source::{SourceError, Span};

/// 诊断严重级别。
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub enum Severity {
    /// 阻止生成结果的错误。
    Error,
    /// 不阻止生成结果的警告。
    Warning,
    /// 附着在主诊断上的次级说明，不单独构成失败。
    Note,
}

impl fmt::Display for Severity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => formatter.write_str("error"),
            Self::Warning => formatter.write_str("warning"),
            Self::Note => formatter.write_str("note"),
        }
    }
}

/// 稳定的源码、清单与路径诊断代码。
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
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
    /// 遇到当前产生式不允许的记号。
    ParseUnexpected,
    /// 缺少当前产生式要求的记号。
    ParseExpected,
    /// 分隔符未闭合。
    ParseUnclosed,
    /// 比较或区间运算符违反非结合约束。
    ParseInvalidPrecedence,
    /// 赋值左侧不是 place 形态。
    ParseInvalidPlace,
    /// `select` 分支不是允许的 send/recv/wait/default 形态。
    ParseInvalidSelectArm,
    /// 解析器实现限制（递归深度或 AST 规模上界）。
    ParseImplementationLimit,
    /// cfg 谓词使用未知键、值或非法语义组合。
    CfgInvalidPredicate,
    /// 模块文件的规范路径无效。
    ModuleInvalidPath,
    /// use 指向不存在的模块。
    ModuleNotFound,
    /// 模块路径与源码声明仅大小写不一致。
    ModulePathCaseMismatch,
    /// 用户声明了编译器保留名称。
    ReservedName,
    /// 同一作用域和命名空间存在重复定义。
    DuplicateDefinition,
    /// use 依赖图或再导出图形成循环。
    ImportCycle,
    /// use 跨模块访问私有项。
    PrivateImport,
    /// use 指向模块中不存在的项。
    ImportNotFound,
    /// use 别名与已有定义或导入冲突。
    ImportConflict,
    /// 两个不同定义路径产生相同稳定摘要。
    DefinitionHashCollision,
    /// 类型形成或布局非法。
    InvalidType,
    /// 类型递归导致无限大小。
    RecursiveType,
    /// 声明、绑定或初始化数据流非法。
    InvalidDeclaration,
    /// 表达式或控制流类型非法。
    InvalidExpression,
    /// 模式结构或穷尽性非法。
    InvalidPattern,
    /// `let-else` 约束非法。
    InvalidLetElse,
    /// `main` 签名非法。
    InvalidMainSignature,
    /// comptime 调用未登记能力或执行域未获准。
    ComptimeCapability,
    /// comptime 求值超出 fuel、heap 或深度边界。
    ComptimeBudget,
    /// comptime 求值执行了 panic。
    ComptimePanic,
    /// 完全相同的展开键再次出现在当前展开栈。
    ExpansionCycle,
    /// 源码宏展开预算或 `expansion_limit` 属性非法。
    ExpansionLimit,
    /// 生成片段类别与插入位置不符。
    ExpansionFragmentMismatch,
    /// 源码宏脚本在边界返回 `Err`。
    MacroBoundaryError,
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
            Self::ParseUnexpected => "E0020",
            Self::ParseExpected => "E0021",
            Self::ParseUnclosed => "E0022",
            Self::ParseInvalidPrecedence => "E0023",
            Self::ParseInvalidPlace => "E0024",
            Self::ParseInvalidSelectArm => "E0025",
            Self::ParseImplementationLimit => "E0026",
            Self::CfgInvalidPredicate => "E0027",
            Self::ModuleInvalidPath => "E0028",
            Self::ModuleNotFound => "E0029",
            Self::ModulePathCaseMismatch => "E0030",
            Self::ReservedName => "E0031",
            Self::DuplicateDefinition => "E0032",
            Self::ImportCycle => "E0033",
            Self::PrivateImport => "E0034",
            Self::ImportNotFound => "E0035",
            Self::ImportConflict => "E0036",
            Self::DefinitionHashCollision => "E0037",
            Self::InvalidType => "E0038",
            Self::RecursiveType => "E0039",
            Self::InvalidDeclaration => "E0040",
            Self::InvalidExpression => "E0041",
            Self::InvalidPattern => "E0042",
            Self::InvalidLetElse => "E0043",
            Self::InvalidMainSignature => "E0044",
            Self::ComptimeCapability => "E0045",
            Self::ComptimeBudget => "E0046",
            Self::ComptimePanic => "E0047",
            Self::ExpansionCycle => "E0048",
            Self::ExpansionLimit => "E0049",
            Self::ExpansionFragmentMismatch => "E0050",
            Self::MacroBoundaryError => "E0051",
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
    /// 前端发射顺序；`u32::MAX` 表示未指定，排序时退回到 span。
    seq: u32,
}

impl Diagnostic {
    /// 创建带源码范围的错误诊断。
    pub(crate) fn error(
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Option<Span>,
    ) -> Self {
        Self::new(Severity::Error, code, message, span, u32::MAX)
    }

    pub(crate) fn note(
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Option<Span>,
    ) -> Self {
        Self::new(Severity::Note, code, message, span, u32::MAX)
    }

    pub(crate) fn with_seq(mut self, seq: u32) -> Self {
        self.seq = seq;
        self
    }

    pub(crate) fn sequence(&self) -> u32 {
        self.seq
    }

    pub(crate) fn new(
        severity: Severity,
        code: DiagnosticCode,
        message: impl Into<String>,
        span: Option<Span>,
        seq: u32,
    ) -> Self {
        Self {
            severity,
            code,
            message: message.into(),
            span,
            seq,
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
            left.seq
                .cmp(&right.seq)
                .then_with(|| span_sort_key(left).cmp(&span_sort_key(right)))
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

fn span_sort_key(
    diagnostic: &Diagnostic,
) -> Option<(&std::ffi::OsStr, u32, u32, crate::source::ExpansionId)> {
    diagnostic.span.as_ref().map(|span| {
        (
            span.path().as_os_str(),
            span.start(),
            span.end(),
            span.expansion(),
        )
    })
}
