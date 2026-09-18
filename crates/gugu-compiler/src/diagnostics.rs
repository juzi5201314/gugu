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
#[non_exhaustive]
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
    /// 沿一条实例化 ancestry 的单态化无法收敛。
    MonoDivergence,
    /// 单态化实例总数超过实现上界。
    MonoInstanceLimit,
    /// late 值越过类型冻结阶段边界。
    LateComptime,
    /// GIR 结构、清理序列或效果区域不满足内部不变量。
    GirInvariant,
    /// 按值传递超过 64 字节的位结构体。
    LargeCopy,
    /// LIR SSA、内存链、provenance 或 effect region 不满足内部不变量。
    LirInvariant,
    /// runtime raw 平面契约或 publish 序列不满足内部不变量。
    RuntimeRawInvariant,
    /// 资源值越过资源域边界进入 managed region 分配。
    ResourceInvariant,
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
            Self::MonoDivergence => "E0052",
            Self::MonoInstanceLimit => "E0053",
            Self::LateComptime => "E0054",
            Self::GirInvariant => "E0055",
            Self::LargeCopy => "E0056",
            Self::LirInvariant => "E0057",
            Self::RuntimeRawInvariant => "E0058",
            Self::ResourceInvariant => "E0059",
        };
        formatter.write_str(code)
    }
}

impl DiagnosticCode {
    /// 全部诊断代码，顺序与枚举声明一致（当前即 `code()` 的稳定升序）。
    ///
    /// 新增 variant 必须同时补本表与 [`Self::index`]；`index` 是不带通配分支的穷尽匹配，
    /// 漏项会让 compiler crate 的测试构建直接编译失败，这是覆盖闸门的编译期半边。
    #[cfg(test)]
    pub(crate) const ALL: [Self; 59] = [
        Self::SourceRead,
        Self::MissingMain,
        Self::MalformedSource,
        Self::InvalidUtf8,
        Self::SourceBom,
        Self::InvalidSourcePath,
        Self::SpanOutOfBounds,
        Self::SourceTooLarge,
        Self::LexUnterminated,
        Self::LexUnterminatedComment,
        Self::LexInvalidEscape,
        Self::LexInvalidNumeric,
        Self::LexInvalidToken,
        Self::LexUnknownAttribute,
        Self::LexInvalidAttributeArg,
        Self::LexInvalidUnicodeScalar,
        Self::LexCStringNul,
        Self::LexInvalidByteChar,
        Self::LexInvalidFormatSpec,
        Self::ParseUnexpected,
        Self::ParseExpected,
        Self::ParseUnclosed,
        Self::ParseInvalidPrecedence,
        Self::ParseInvalidPlace,
        Self::ParseInvalidSelectArm,
        Self::ParseImplementationLimit,
        Self::CfgInvalidPredicate,
        Self::ModuleInvalidPath,
        Self::ModuleNotFound,
        Self::ModulePathCaseMismatch,
        Self::ReservedName,
        Self::DuplicateDefinition,
        Self::ImportCycle,
        Self::PrivateImport,
        Self::ImportNotFound,
        Self::ImportConflict,
        Self::DefinitionHashCollision,
        Self::InvalidType,
        Self::RecursiveType,
        Self::InvalidDeclaration,
        Self::InvalidExpression,
        Self::InvalidPattern,
        Self::InvalidLetElse,
        Self::InvalidMainSignature,
        Self::ComptimeCapability,
        Self::ComptimeBudget,
        Self::ComptimePanic,
        Self::ExpansionCycle,
        Self::ExpansionLimit,
        Self::ExpansionFragmentMismatch,
        Self::MacroBoundaryError,
        Self::MonoDivergence,
        Self::MonoInstanceLimit,
        Self::LateComptime,
        Self::GirInvariant,
        Self::LargeCopy,
        Self::LirInvariant,
        Self::RuntimeRawInvariant,
        Self::ResourceInvariant,
    ];

    /// 返回该代码在 [`Self::ALL`] 中的稠密下标。
    ///
    /// 本匹配必须保持穷尽：漏掉任一 variant 都是编译错误，诊断契约表据此按下标落位。
    #[cfg(test)]
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::SourceRead => 0,
            Self::MissingMain => 1,
            Self::MalformedSource => 2,
            Self::InvalidUtf8 => 3,
            Self::SourceBom => 4,
            Self::InvalidSourcePath => 5,
            Self::SpanOutOfBounds => 6,
            Self::SourceTooLarge => 7,
            Self::LexUnterminated => 8,
            Self::LexUnterminatedComment => 9,
            Self::LexInvalidEscape => 10,
            Self::LexInvalidNumeric => 11,
            Self::LexInvalidToken => 12,
            Self::LexUnknownAttribute => 13,
            Self::LexInvalidAttributeArg => 14,
            Self::LexInvalidUnicodeScalar => 15,
            Self::LexCStringNul => 16,
            Self::LexInvalidByteChar => 17,
            Self::LexInvalidFormatSpec => 18,
            Self::ParseUnexpected => 19,
            Self::ParseExpected => 20,
            Self::ParseUnclosed => 21,
            Self::ParseInvalidPrecedence => 22,
            Self::ParseInvalidPlace => 23,
            Self::ParseInvalidSelectArm => 24,
            Self::ParseImplementationLimit => 25,
            Self::CfgInvalidPredicate => 26,
            Self::ModuleInvalidPath => 27,
            Self::ModuleNotFound => 28,
            Self::ModulePathCaseMismatch => 29,
            Self::ReservedName => 30,
            Self::DuplicateDefinition => 31,
            Self::ImportCycle => 32,
            Self::PrivateImport => 33,
            Self::ImportNotFound => 34,
            Self::ImportConflict => 35,
            Self::DefinitionHashCollision => 36,
            Self::InvalidType => 37,
            Self::RecursiveType => 38,
            Self::InvalidDeclaration => 39,
            Self::InvalidExpression => 40,
            Self::InvalidPattern => 41,
            Self::InvalidLetElse => 42,
            Self::InvalidMainSignature => 43,
            Self::ComptimeCapability => 44,
            Self::ComptimeBudget => 45,
            Self::ComptimePanic => 46,
            Self::ExpansionCycle => 47,
            Self::ExpansionLimit => 48,
            Self::ExpansionFragmentMismatch => 49,
            Self::MacroBoundaryError => 50,
            Self::MonoDivergence => 51,
            Self::MonoInstanceLimit => 52,
            Self::LateComptime => 53,
            Self::GirInvariant => 54,
            Self::LargeCopy => 55,
            Self::LirInvariant => 56,
            Self::RuntimeRawInvariant => 57,
            Self::ResourceInvariant => 58,
        }
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

/// 返回源码快照校验失败的稳定诊断代码。
///
/// 快照字节校验（超限、非法 UTF-8、BOM）与逻辑路径校验共用本映射：宏生成文本的
/// 快照失败也必须落在快照自身的代码上，不能再一律归为非法逻辑路径。
pub(crate) fn source_error_code(error: &SourceError) -> DiagnosticCode {
    match error {
        SourceError::TooLarge { .. } => DiagnosticCode::SourceTooLarge,
        SourceError::InvalidUtf8 { .. } => DiagnosticCode::InvalidUtf8,
        SourceError::Bom { .. } => DiagnosticCode::SourceBom,
        SourceError::InvalidPath { .. } => DiagnosticCode::InvalidSourcePath,
    }
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
        let (path, offset) = match error {
            SourceError::TooLarge { path } => (path, 0),
            SourceError::InvalidUtf8 { path, offset } => (path, *offset as usize),
            SourceError::Bom { path } => (path, 0),
            SourceError::InvalidPath { path } => (path, 0),
        };
        Self::error(
            source_error_code(error),
            error.to_string(),
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

    /// 将诊断渲染为规范文本格式。
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
