//! 诊断归属契约表与全量覆盖闸门。
//!
//! 每个 `DiagnosticCode` 在 [`CASES`] 中至少有一行决策：要么给出一个让该代码经公开
//! 编译输入失败（或告警）的最小 [`Input`]，并逐项断言 code、级别、1 基位置、逻辑路径
//! 后缀、附注条数与消息子串；要么记录该代码在默认套件内不可触达的机制理由（内部
//! 不变量、巨型输入或稳定摘要碰撞）。
//!
//! 三个闸门测试分别覆盖：编译期穷尽匹配之外的运行期缺口（`every_code_has_a_decision`）、
//! 代码枚举与派生的稠密下标的一致性（`all_is_dense_and_ascending`）、以及每一行活体
//! 输入与实装诊断的逐项一致（`cases_match_the_compiler`）。新增或删除 variant 时前两个
//! 测试立即失败，逼迫本表同步；改错发射点则第三个测试失败。
//!
//! 表按 `code()` 稳定升序排列，同一代码可以有多行（E0038 的不同规则、E0044 的两种
//! 非法签名、E0051 的两种宏失败、E0056 的 warn/allow/deny 三态）。绝大多数规则的输入
//! 只产生一条主诊断；解析恢复（E0022/E0026）与 `?` 的非 `Try` 操作数（E0038）会同时
//! 产生伴随诊断，这些行用 [`Expected::Diagnostic`] 的 `companions` 逐条登记，而不是放宽断言。

use std::collections::BTreeSet;

use crate::{
    CompileRequest, Compiler, Diagnostic, DiagnosticCode, Package, Severity, SourceMap,
    SourceSnapshot, Target, TargetKind, TargetName,
    frontend::{SourceInput, cfg::CfgContext},
};

/// 契约行统一使用的目标：与宿主平台无关，保证 cfg 求值在 CI 与开发机上一致。
const TARGET: TargetName = TargetName::X86_64Linux;
/// `Input::Sources` 行的入口与源码根，与 `frontend/stage10_tests.rs` 的约定一致。
const ENTRY: &str = "src/main.gg";
const SOURCE_ROOT: &str = "src";

/// 一行的输入构造方式。
#[derive(Clone, Copy, Debug)]
enum Input {
    /// 内存单文件请求：逻辑路径 + 源码文本。
    Source {
        path: &'static str,
        text: &'static str,
    },
    /// 多文件源码表注入：cfg、模块与导入层行使用。
    Sources(&'static [(&'static str, &'static str)]),
    /// 真实文件读取：`bytes = None` 表示路径不存在。
    File {
        name: &'static str,
        bytes: Option<&'static [u8]>,
    },
    /// 真实 project 树：`src/main.gg` + 指向 `main.gg` 的符号链接。
    ///
    /// `Project::discover` 在 target 发现阶段就拒绝源码树符号链接；本输入直接构造
    /// `Package`/`Target`，验证 compiler 输入校验（`collect_source_paths`）对同一结构
    /// 违规给出稳定诊断 E0003。
    SymlinkTree { link: &'static str },
    /// 递归嵌套 `&` 类型：超过 `parse/ty.rs` 的 `MAX_TY_DEPTH`。
    DeepRefType { depth: u32 },
    /// 直接在源码表上请求越界 span。
    ///
    /// 公开编译输入无法触达该分支：`source_map_span` 的现有调用点都请求已注册文件的
    /// `(0, 0)`；本行调用生产函数本身验证 E0007 的归属。
    SpanOutOfBounds,
}

/// 一条被断言的诊断。
#[derive(Clone, Copy, Debug)]
struct RowDiagnostic {
    code: DiagnosticCode,
    severity: Severity,
    /// `Some` 断言 1 基行列；`None` 断言诊断不带 span。
    position: Option<(u32, u32)>,
    /// 断言逻辑路径以该子串结尾（临时目录 fixture 只固定相对后缀）。
    path_suffix: Option<&'static str>,
    /// 必须出现的主消息子串：规范术语或关键字，禁止整句文案快照。
    message: &'static str,
}

/// 一行活体输入的期望。
#[derive(Clone, Copy, Debug)]
enum Expected {
    /// 恰好一条主诊断，外加 `companions` 列出的伴随诊断与 `notes` 条附注。
    ///
    /// 伴随诊断用于同一输入必然同时产生的其它诊断（解析恢复的 item 错误、`?` 的双路径
    /// 上报）；它们与主诊断一样逐项定址，只是不充当本行的 code。
    Diagnostic {
        severity: Severity,
        /// `Some` 断言主诊断的 1 基行列；`None` 断言主诊断不带 span。
        position: Option<(u32, u32)>,
        /// 断言主诊断逻辑路径以该子串结尾（临时目录 fixture 只固定相对后缀）。
        path_suffix: Option<&'static str>,
        /// 允许的附注（`Severity::Note`）条数。
        notes: u8,
        /// 必须出现的主消息子串：规范术语或关键字，禁止整句文案快照。
        message: &'static str,
        companions: &'static [RowDiagnostic],
    },
    /// 输入必须完全静默：诊断列表为空。
    Silent,
}

/// 一个诊断代码的契约决策。
#[derive(Clone, Copy, Debug)]
enum Decision {
    /// 活体输入：产生的诊断必须与 `expected` 一致。
    Live { input: Input, expected: Expected },
    /// 默认套件内不可触达：附机制理由与既有断言的归属。
    Unreachable(
        #[expect(
            dead_code,
            reason = "理由文本是给评审者与维护者的契约文档，闸门只断言决策存在"
        )]
        &'static str,
    ),
}

/// 一行诊断归属契约。
#[derive(Clone, Copy, Debug)]
struct Case {
    code: DiagnosticCode,
    decision: Decision,
}

/// 构造恰好一条错误主诊断的契约行。
const fn error_row(
    code: DiagnosticCode,
    input: Input,
    position: (u32, u32),
    path_suffix: &'static str,
    notes: u8,
    message: &'static str,
) -> Case {
    error_row_with_companions(code, input, position, path_suffix, notes, message, &[])
}

/// 构造带伴随诊断的错误契约行。
const fn error_row_with_companions(
    code: DiagnosticCode,
    input: Input,
    position: (u32, u32),
    path_suffix: &'static str,
    notes: u8,
    message: &'static str,
    companions: &'static [RowDiagnostic],
) -> Case {
    Case {
        code,
        decision: Decision::Live {
            input,
            expected: Expected::Diagnostic {
                severity: Severity::Error,
                position: Some(position),
                path_suffix: Some(path_suffix),
                notes,
                message,
                companions,
            },
        },
    }
}

/// 构造不带 span 的错误契约行。
const fn error_row_detached(
    code: DiagnosticCode,
    input: Input,
    notes: u8,
    message: &'static str,
) -> Case {
    Case {
        code,
        decision: Decision::Live {
            input,
            expected: Expected::Diagnostic {
                severity: Severity::Error,
                position: None,
                path_suffix: None,
                notes,
                message,
                companions: &[],
            },
        },
    }
}

/// 构造恰好一条警告主诊断的契约行。
const fn warning_row(
    code: DiagnosticCode,
    input: Input,
    position: (u32, u32),
    path_suffix: &'static str,
    notes: u8,
    message: &'static str,
) -> Case {
    Case {
        code,
        decision: Decision::Live {
            input,
            expected: Expected::Diagnostic {
                severity: Severity::Warning,
                position: Some(position),
                path_suffix: Some(path_suffix),
                notes,
                message,
                companions: &[],
            },
        },
    }
}

/// 构造静默契约行：输入必须不产生任何诊断。
const fn silent_row(code: DiagnosticCode, input: Input) -> Case {
    Case {
        code,
        decision: Decision::Live {
            input,
            expected: Expected::Silent,
        },
    }
}

/// 构造不可触达的决策行，附机制理由与既有断言的归属。
const fn unreachable(code: DiagnosticCode, reason: &'static str) -> Case {
    Case {
        code,
        decision: Decision::Unreachable(reason),
    }
}

/// 位于 `src/main.gg` 的错误伴随诊断。
const fn main_error(
    code: DiagnosticCode,
    position: (u32, u32),
    message: &'static str,
) -> RowDiagnostic {
    RowDiagnostic {
        code,
        severity: Severity::Error,
        position: Some(position),
        path_suffix: Some("src/main.gg"),
        message,
    }
}

/// E0022：未闭合分隔符只在 item 恢复路径上报，同一输入必然先有一条 item 错误。
const PARSE_RECOVERY_COMPANIONS: &[RowDiagnostic] = &[main_error(
    DiagnosticCode::ParseUnexpected,
    (2, 1),
    "此处需要模块项",
)];

/// E0026：深度超限后剩余 `&` 记号必然触发一次「此处需要类型」的后续报告。
const DEEP_TYPE_COMPANIONS: &[RowDiagnostic] = &[main_error(
    DiagnosticCode::ParseUnexpected,
    (1, 532),
    "此处需要类型",
)];

/// E0038：`?` 的非 `Try` 操作数由拒绝选择与方法查找两条路径各报一次。
const TRY_COMPANIONS: &[RowDiagnostic] = &[main_error(
    DiagnosticCode::InvalidType,
    (1, 28),
    "没有适用的 Try 实现",
)];

const CASES: &[Case] = &[
    // ── E0001–E0008：源码快照、bootstrap 结构与 span 校验 ──
    error_row(
        DiagnosticCode::SourceRead,
        Input::File {
            name: "missing.gg",
            bytes: None,
        },
        (1, 1),
        "missing.gg",
        0,
        "无法读取源文件",
    ),
    error_row(
        DiagnosticCode::MissingMain,
        Input::Source {
            path: "src/main.gg",
            text: "fn util() {}\n",
        },
        (1, 1),
        "src/main.gg",
        0,
        "必须包含 `fn main()",
    ),
    error_row(
        DiagnosticCode::MalformedSource,
        Input::SymlinkTree { link: "link.gg" },
        (1, 1),
        "src/link.gg",
        0,
        "符号链接",
    ),
    error_row(
        DiagnosticCode::InvalidUtf8,
        Input::File {
            name: "src/main.gg",
            bytes: Some(b"\xff\xfefn main() {}"),
        },
        (1, 1),
        "src/main.gg",
        0,
        "非法 UTF-8",
    ),
    error_row(
        DiagnosticCode::SourceBom,
        Input::File {
            name: "src/main.gg",
            bytes: Some(b"\xef\xbb\xbffn main() {}"),
        },
        (1, 1),
        "src/main.gg",
        0,
        "BOM",
    ),
    error_row(
        DiagnosticCode::InvalidSourcePath,
        Input::Source {
            path: r"src/a\b.gg",
            text: "fn main() {}",
        },
        (1, 1),
        r"src/a\b.gg",
        0,
        "package-relative",
    ),
    error_row_detached(
        DiagnosticCode::SpanOutOfBounds,
        Input::SpanOutOfBounds,
        0,
        "超出",
    ),
    unreachable(
        DiagnosticCode::SourceTooLarge,
        "源码快照上限是 u32::MAX 字节；构造该输入需要 ≥4 GiB 源文本，默认套件不承担重型分配",
    ),
    // ── E0009–E0019：词法 ──
    error_row(
        DiagnosticCode::LexUnterminated,
        Input::Source {
            path: "src/main.gg",
            text: "raw\"a\nb",
        },
        (1, 1),
        "src/main.gg",
        0,
        "未转义换行",
    ),
    error_row(
        DiagnosticCode::LexUnterminatedComment,
        Input::Source {
            path: "src/main.gg",
            text: "/* 未闭合\nfn main() {}",
        },
        (1, 1),
        "src/main.gg",
        0,
        "块注释未闭合",
    ),
    error_row(
        DiagnosticCode::LexInvalidEscape,
        Input::Source {
            path: "src/main.gg",
            text: r#""\q""#,
        },
        (1, 2),
        "src/main.gg",
        0,
        "未知转义",
    ),
    error_row(
        DiagnosticCode::LexInvalidNumeric,
        Input::Source {
            path: "src/main.gg",
            text: "08",
        },
        (1, 1),
        "src/main.gg",
        0,
        "前导 0",
    ),
    error_row(
        DiagnosticCode::LexInvalidToken,
        Input::Source {
            path: "src/main.gg",
            text: "let α = 1",
        },
        (1, 5),
        "src/main.gg",
        0,
        "无法形成记号",
    ),
    error_row(
        DiagnosticCode::LexUnknownAttribute,
        Input::Source {
            path: "src/main.gg",
            text: "#[unknown] fn f() {}",
        },
        (1, 3),
        "src/main.gg",
        0,
        "未知属性",
    ),
    error_row(
        DiagnosticCode::LexInvalidAttributeArg,
        Input::Source {
            path: "src/main.gg",
            text: "#[repr(foo)]",
        },
        (1, 8),
        "src/main.gg",
        0,
        "未知 repr 参数",
    ),
    error_row(
        DiagnosticCode::LexInvalidUnicodeScalar,
        Input::Source {
            path: "src/main.gg",
            text: r#"'\u{D800}'"#,
        },
        (1, 2),
        "src/main.gg",
        0,
        "非法 Unicode scalar",
    ),
    error_row(
        DiagnosticCode::LexCStringNul,
        Input::Source {
            path: "src/main.gg",
            text: "c\"a\\0b\"",
        },
        (1, 1),
        "src/main.gg",
        0,
        "内嵌 0 字节",
    ),
    error_row(
        DiagnosticCode::LexInvalidByteChar,
        Input::Source {
            path: "src/main.gg",
            text: "b'\\u{1F600}'",
        },
        (1, 1),
        "src/main.gg",
        0,
        "字节字符必须恰好一个字节",
    ),
    error_row(
        DiagnosticCode::LexInvalidFormatSpec,
        Input::Source {
            path: "src/main.gg",
            text: r#"f"{1:q}""#,
        },
        (1, 5),
        "src/main.gg",
        0,
        "未知格式码",
    ),
    // ── E0020–E0026：语法与解析实现限制 ──
    error_row(
        DiagnosticCode::ParseUnexpected,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { = 1 }",
        },
        (1, 13),
        "src/main.gg",
        0,
        "此处需要表达式",
    ),
    error_row(
        DiagnosticCode::ParseExpected,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() {",
        },
        (1, 12),
        "src/main.gg",
        0,
        "块需要 `}`",
    ),
    // 未闭合分隔符只在 item 恢复路径上报告，同一输入必然先有一条 item 错误。
    error_row_with_companions(
        DiagnosticCode::ParseUnclosed,
        Input::Source {
            path: "src/main.gg",
            text: "fn f() {}\n123 ( x",
        },
        (2, 8),
        "src/main.gg",
        0,
        "分隔符未闭合",
        PARSE_RECOVERY_COMPANIONS,
    ),
    error_row(
        DiagnosticCode::ParseInvalidPrecedence,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { a < b < c }",
        },
        (1, 19),
        "src/main.gg",
        1,
        "不结合",
    ),
    error_row(
        DiagnosticCode::ParseInvalidPlace,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { 1 = 2 }",
        },
        (1, 13),
        "src/main.gg",
        0,
        "place",
    ),
    error_row(
        DiagnosticCode::ParseInvalidSelectArm,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { select { value => 1 } }",
        },
        (1, 22),
        "src/main.gg",
        0,
        "发送臂必须是",
    ),
    // 深度超限后剩余 `&` 记号必然触发一次「此处需要类型」的后续报告。
    error_row_with_companions(
        DiagnosticCode::ParseImplementationLimit,
        Input::DeepRefType { depth: 300 },
        (1, 532),
        "src/main.gg",
        0,
        "深度超过上限",
        DEEP_TYPE_COMPANIONS,
    ),
    // ── E0027–E0037：cfg、模块表与导入 ──
    error_row(
        DiagnosticCode::CfgInvalidPredicate,
        Input::Sources(&[(
            "src/main.gg",
            "#[cfg(feature = \"missing\")] fn main() {}\n",
        )]),
        (1, 7),
        "src/main.gg",
        0,
        "未声明",
    ),
    error_row(
        DiagnosticCode::ModuleInvalidPath,
        Input::Sources(&[
            ("src/main.gg", "fn main() {}\n"),
            ("src/net.gg", "pub fn open() {}\n"),
            ("src/net/mod.gg", "pub fn close() {}\n"),
        ]),
        (1, 1),
        "src/net/mod.gg",
        0,
        "同时由",
    ),
    error_row(
        DiagnosticCode::ModuleNotFound,
        Input::Sources(&[("src/main.gg", "use missing\nfn main() {}\n")]),
        (1, 1),
        "src/main.gg",
        0,
        "找不到模块",
    ),
    error_row(
        DiagnosticCode::ModulePathCaseMismatch,
        Input::Sources(&[
            ("src/main.gg", "use Worker\nfn main() {}\n"),
            ("src/worker.gg", "pub fn run() {}\n"),
        ]),
        (1, 1),
        "src/main.gg",
        0,
        "大小写",
    ),
    error_row(
        DiagnosticCode::ReservedName,
        Input::Sources(&[("src/main.gg", "struct Option {}\nfn main() {}\n")]),
        (1, 8),
        "src/main.gg",
        0,
        "保留",
    ),
    error_row(
        DiagnosticCode::DuplicateDefinition,
        Input::Sources(&[(
            "src/main.gg",
            "fn repeated() {}\nfn repeated() {}\nfn main() {}\n",
        )]),
        (2, 1),
        "src/main.gg",
        1,
        "重复声明",
    ),
    error_row(
        DiagnosticCode::ImportCycle,
        Input::Sources(&[
            ("src/main.gg", "use worker\nfn main() {}\n"),
            ("src/worker.gg", "use main\n"),
        ]),
        (1, 1),
        "src/worker.gg",
        0,
        "循环",
    ),
    error_row(
        DiagnosticCode::PrivateImport,
        Input::Sources(&[
            ("src/main.gg", "use worker.{run}\nfn main() {}\n"),
            ("src/worker.gg", "fn run() {}\n"),
        ]),
        (1, 13),
        "src/main.gg",
        0,
        "私有项",
    ),
    error_row(
        DiagnosticCode::ImportNotFound,
        Input::Sources(&[
            ("src/main.gg", "use worker.{missing}\nfn main() {}\n"),
            ("src/worker.gg", "pub fn run() {}\n"),
        ]),
        (1, 13),
        "src/main.gg",
        0,
        "不存在可导入项",
    ),
    error_row(
        DiagnosticCode::ImportConflict,
        Input::Sources(&[
            (
                "src/main.gg",
                "use worker.{run}\nfn run() {}\nfn main() {}\n",
            ),
            ("src/worker.gg", "pub fn run() {}\n"),
        ]),
        (1, 13),
        "src/main.gg",
        0,
        "冲突",
    ),
    unreachable(
        DiagnosticCode::DefinitionHashCollision,
        "需要两个不同 DefPath 的稳定摘要碰撞（BLAKE3-256），公开输入不可构造",
    ),
    // ── E0038–E0044：类型、声明、表达式与模式（含 ADR-0004/0005/0006 规则族）──
    // ADR-0006：`!` 没有 TypeId。
    error_row(
        DiagnosticCode::InvalidType,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { _ = type_id[!]() }",
        },
        (1, 17),
        "src/main.gg",
        0,
        "没有 TypeId",
    ),
    // ADR-0006：`MaybeUninit` 没有 TypeId。
    error_row(
        DiagnosticCode::InvalidType,
        Input::Source {
            path: "src/main.gg",
            text: "use std.mem.{MaybeUninit}\nfn main() { _ = type_id[MaybeUninit[int]]() }",
        },
        (2, 17),
        "src/main.gg",
        0,
        "没有 TypeId",
    ),
    // ADR-0006：窄 downcast 不能猜 trait 对象。
    error_row(
        DiagnosticCode::InvalidType,
        Input::Source {
            path: "src/main.gg",
            text: "trait Value { fn value(self) int }\nfn invalid(value: dyn Value) { _ = value.downcast::[int]() }\nfn main() {}",
        },
        (2, 36),
        "src/main.gg",
        0,
        "没有适用的方法 `downcast`",
    ),
    // ADR-0005：`?` 要求操作数与出口类型都实现 `Try`；同一失败由拒绝选择与
    // 方法查找两条路径分别上报，伴随诊断逐项登记而不是放宽断言。
    error_row_with_companions(
        DiagnosticCode::InvalidType,
        Input::Source {
            path: "src/main.gg",
            text: "fn f(o: Option[int]) int { o? }\nfn main() {}",
        },
        (1, 28),
        "src/main.gg",
        0,
        "不满足 trait Try",
        TRY_COMPANIONS,
    ),
    error_row_detached(
        DiagnosticCode::RecursiveType,
        Input::Source {
            path: "src/main.gg",
            text: "struct A { b: B }\nstruct B { a: A }\nfn main() {}",
        },
        0,
        "无限大小递归",
    ),
    // ADR-0005：APIT 不能强化声明边界。
    error_row(
        DiagnosticCode::InvalidDeclaration,
        Input::Source {
            path: "src/main.gg",
            text: "trait Reader { fn read(self, value: impl Clone) }\nstruct R {}\nimpl Reader for R { fn read(self, value: impl Eq) {} }\nfn main() {}",
        },
        (3, 21),
        "src/main.gg",
        0,
        "与 trait 不一致",
    ),
    // ADR-0005：否定 impl 与肯定 impl 不能重叠。
    error_row(
        DiagnosticCode::InvalidDeclaration,
        Input::Source {
            path: "src/main.gg",
            text: "trait T {}\nstruct S {}\nimpl T for S {}\nimpl !T for S {}\nfn main() {}",
        },
        (4, 1),
        "src/main.gg",
        0,
        "impl 重叠",
    ),
    // ADR-0005：两个本地属性互斥。
    error_row(
        DiagnosticCode::InvalidDeclaration,
        Input::Source {
            path: "src/main.gg",
            text: "#[coroutine_local] #[os_thread_local] static A: int = 1\nfn main() {}",
        },
        (1, 1),
        "src/main.gg",
        0,
        "局部存储属性只能单独用于 static",
    ),
    // ADR-0005：普通 static 不能引用协程本地槽。
    error_row(
        DiagnosticCode::InvalidDeclaration,
        Input::Source {
            path: "src/main.gg",
            text: "#[coroutine_local] static A: int = 1\nstatic B: int = A\nfn main() {}",
        },
        (1, 1),
        "src/main.gg",
        0,
        "不能读取运行时局部 static",
    ),
    // ADR-0006：用户不能手写语言内建 trait 的 impl。
    error_row(
        DiagnosticCode::InvalidDeclaration,
        Input::Source {
            path: "src/main.gg",
            text: "use std.any.{Any}\nstruct S {}\nimpl Any for S {}\nfn main() {}",
        },
        (3, 1),
        "src/main.gg",
        0,
        "不能覆盖编译器提供的语言实现",
    ),
    // ADR-0004：`!` 与 `()` 不同。
    error_row(
        DiagnosticCode::InvalidExpression,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { let x: ! = ()\n _ = x }",
        },
        (1, 24),
        "src/main.gg",
        0,
        "类型不一致：() 与 !",
    ),
    // ADR-0004：union 字段访问只在 unsafe 内允许（构造本身允许单字段初始化）。
    error_row(
        DiagnosticCode::InvalidExpression,
        Input::Source {
            path: "src/main.gg",
            text: "union Word { i: int }\nfn main() { let word = Word { i: 1 }\n _ = word.i }",
        },
        (3, 11),
        "src/main.gg",
        0,
        "unsafe",
    ),
    // ADR-0005：RPIT 不泄露具体类型字段。
    error_row(
        DiagnosticCode::InvalidExpression,
        Input::Source {
            path: "src/main.gg",
            text: "trait Value {}\nstruct Point { x: int }\nimpl Value for Point {}\nfn make() impl Value = Point { x: 1 }\nfn main() { let value = make()\n _ = value.x }",
        },
        (6, 12),
        "src/main.gg",
        0,
        "没有字段 `x`",
    ),
    // ADR-0006：`TypeId` 不是整数。
    error_row(
        DiagnosticCode::InvalidExpression,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { let id: TypeId = 1\n _ = id }",
        },
        (1, 30),
        "src/main.gg",
        0,
        "数值字面量不能隐式转换",
    ),
    // ADR-0004：守卫不贡献覆盖。
    error_row(
        DiagnosticCode::InvalidPattern,
        Input::Source {
            path: "src/main.gg",
            text: "fn f(b: bool) int { match b { true => 1, false if b => 2 } }\nfn main() {}",
        },
        (1, 27),
        "src/main.gg",
        0,
        "match 未穷尽",
    ),
    // ADR-0004：不可驳 `let` 段。
    error_row(
        DiagnosticCode::InvalidPattern,
        Input::Source {
            path: "src/main.gg",
            text: "fn f(x: int) { let 1 = x }\nfn main() {}",
        },
        (1, 20),
        "src/main.gg",
        0,
        "不可驳模式",
    ),
    // ADR-0004：or 模式绑定集合必须一致。
    error_row(
        DiagnosticCode::InvalidPattern,
        Input::Source {
            path: "src/main.gg",
            text: "fn f(t: (int, int)) { match t { (1, y) | (2, 2) => ()\n _ => () } }\nfn main() {}",
        },
        (1, 40),
        "src/main.gg",
        0,
        "or 模式必须绑定相同名字及类型",
    ),
    // ADR-0004：空范围模式。
    error_row(
        DiagnosticCode::InvalidPattern,
        Input::Source {
            path: "src/main.gg",
            text: "fn f(x: int) int { match x { 2..1 => 1, _ => 0 } }\nfn main() {}",
        },
        (1, 30),
        "src/main.gg",
        0,
        "范围端点",
    ),
    // ADR-0004：`let-else` 的 else 必须发散。
    error_row(
        DiagnosticCode::InvalidLetElse,
        Input::Source {
            path: "src/main.gg",
            text: "fn f(x: int) { let 1 = x else { 1 } }\nfn main() {}",
        },
        (1, 16),
        "src/main.gg",
        0,
        "发散",
    ),
    // ADR-0005：`main` 签名。
    error_row(
        DiagnosticCode::InvalidMainSignature,
        Input::Source {
            path: "src/main.gg",
            text: "fn main(_: int) {}",
        },
        (1, 1),
        "src/main.gg",
        0,
        "必须无参数",
    ),
    // ── E0045–E0054：comptime、展开、单态化与 late 值 ──
    error_row(
        DiagnosticCode::ComptimeCapability,
        Input::Source {
            path: "src/main.gg",
            text: "const X: int = std.io.println(\"x\")\nfn main() {}",
        },
        (1, 16),
        "src/main.gg",
        0,
        "未在 comptime capability registry 登记",
    ),
    // 4 MiB heap 上限：`[0; 100_000]` 的计账字节立即超限，不分配宿主数组。
    error_row(
        DiagnosticCode::ComptimeBudget,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { let a = comptime { [0; 100_000] }\n _ = a }",
        },
        (1, 32),
        "src/main.gg",
        0,
        "heap 分配超过字节上限",
    ),
    error_row(
        DiagnosticCode::ComptimePanic,
        Input::Source {
            path: "src/main.gg",
            text: "const X: int = panic(\"boom\")\nfn main() {}",
        },
        (1, 16),
        "src/main.gg",
        0,
        "panic",
    ),
    error_row(
        DiagnosticCode::ExpansionCycle,
        Input::Source {
            path: "src/main.gg",
            text: "const SELF: string = \"comptime source {\\n    std.syntax.parse_items(SELF)\\n}\"\ncomptime source {\n    std.syntax.parse_items(SELF)\n}\nfn main() {}",
        },
        (2, 1),
        "src/main.gg",
        3,
        "展开形成循环",
    ),
    error_row(
        DiagnosticCode::ExpansionLimit,
        Input::Source {
            path: "src/main.gg",
            text: "#![comptime(expansion_limit = 0)]\nfn main() {}",
        },
        (1, 1),
        "src/main.gg",
        0,
        "expansion_limit 必须在 1..=256",
    ),
    error_row(
        DiagnosticCode::ExpansionFragmentMismatch,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { let x = comptime source {\n    std.syntax.parse_items(\"fn f() int { 1 }\")\n}\n _ = x }",
        },
        (1, 21),
        "src/main.gg",
        0,
        "插入位置",
    ),
    error_row(
        DiagnosticCode::MacroBoundaryError,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { let x = comptime source {\n    std.syntax.parse_source(\"1 +\")\n}\n _ = x }",
        },
        (1, 21),
        "src/main.gg",
        0,
        "源码宏脚本返回 Err",
    ),
    // 生成文本含 BOM：解析闸门 `validate_source_fragment` 先以脚本级 Err 拒绝（消息来自
    // 该闸门），未捕获即到宏边界。splice 的快照注册分支因此不可达；该分支已按快照码
    // 修复（`source_error_code`），避免未来把 BOM/非法 UTF-8/超限误报为 E0006。
    error_row(
        DiagnosticCode::MacroBoundaryError,
        Input::Source {
            path: "src/main.gg",
            text: "fn main() { let x = comptime source {\n    std.syntax.parse_source(\"\\u{feff}1\")\n}\n _ = x }",
        },
        (1, 21),
        "src/main.gg",
        0,
        "生成文本不是合法 UTF-8",
    ),
    error_row_detached(
        DiagnosticCode::MonoDivergence,
        Input::Source {
            path: "src/main.gg",
            text: "fn wrap[T](value: T) { wrap([value]) }\nfn main() { wrap(1) }",
        },
        0,
        "严格增长",
    ),
    unreachable(
        DiagnosticCode::MonoInstanceLimit,
        "单态化实例上界是 2^32；构造该规模实例图不是默认套件的事务",
    ),
    // comptime.md：late-only 值不能进数组长度等早期位置。
    error_row(
        DiagnosticCode::LateComptime,
        Input::Source {
            path: "src/main.gg",
            text: "const N: int = type_id_count()\ntype A = [int; N]\nfn main() {}",
        },
        (2, 16),
        "src/main.gg",
        0,
        "late 值不能用于早期",
    ),
    // ── E0055–E0059：内部不变量与 lint ──
    unreachable(
        DiagnosticCode::GirInvariant,
        "GIR verifier 只在编译器自身不一致时失败；直接构造断言保留在 frontend/gir/tests.rs 的 verifier_rejects_* 测试",
    ),
    // E0056 warn：按值传递超过 64 字节的位结构体默认告警。
    warning_row(
        DiagnosticCode::LargeCopy,
        Input::Source {
            path: "src/main.gg",
            text: "fn take(xs: [uint; 9]) { _ = xs }\nfn main() { take([0; 9]) }",
        },
        (2, 18),
        "src/main.gg",
        0,
        "超过 64 字节",
    ),
    silent_row(
        DiagnosticCode::LargeCopy,
        Input::Source {
            path: "src/main.gg",
            text: "fn take(xs: [uint; 9]) { _ = xs }\n#[allow(large_copy)]\nfn main() { take([0; 9]) }",
        },
    ),
    error_row(
        DiagnosticCode::LargeCopy,
        Input::Source {
            path: "src/main.gg",
            text: "#![deny(large_copy)]\nfn take(xs: [uint; 9]) { _ = xs }\nfn main() { take([0; 9]) }",
        },
        (3, 18),
        "src/main.gg",
        0,
        "超过 64 字节",
    ),
    unreachable(
        DiagnosticCode::LirInvariant,
        "LIR verifier 只在编译器自身不一致时失败；直接构造断言保留在 lir/tests.rs",
    ),
    unreachable(
        DiagnosticCode::RuntimeRawInvariant,
        "runtime raw 平面 verifier 只在编译器自身不一致时失败；直接构造断言保留在 runtime/tests.rs",
    ),
    unreachable(
        DiagnosticCode::ResourceInvariant,
        "资源域 verifier 只在编译器自身不一致时失败；直接构造断言保留在 lir/tests.rs",
    ),
];

/// 闸门运行器：整表复用一个 compiler 与 query 引擎。
///
/// 引擎复用与真实编译进程一致：首行是冷启动，其余行在共享 query 缓存上运行，
/// 因此覆盖面同时包含冷热两条诊断恢复路径；查询键按源码内容与 cfg 计算，跨行复用
/// 不会串味（冷热等价由 semantics 测试单独覆盖）。
struct Runner {
    compiler: Compiler,
    queries: crate::QueryEngine,
}

impl Runner {
    fn new() -> Self {
        Self {
            compiler: Compiler::new(),
            queries: crate::QueryEngine::new(),
        }
    }

    /// 运行一行的输入并返回观测到的诊断；`Unreachable` 行不运行。
    fn diagnostics(&self, input: &Input) -> Vec<Diagnostic> {
        match input {
            Input::Source { path, text } => self.compile_memory(path, text),
            Input::Sources(sources) => {
                let snapshots = sources
                    .iter()
                    .map(|(path, text)| {
                        SourceSnapshot::from_str(path, text).expect("fixture 必须是合法 UTF-8 快照")
                    })
                    .collect();
                let mut source_map = SourceMap::new(snapshots).expect("fixture 逻辑路径必须唯一");
                let cfg = CfgContext::target_only(TARGET);
                match crate::frontend::bootstrap(
                    SourceInput::Sources {
                        source_map: &mut source_map,
                        entry: ENTRY,
                        source_root: SOURCE_ROOT,
                        package_identity: "acme/demo@1.0.0",
                        require_main: true,
                        cfg: &cfg,
                        external_packages: &BTreeSet::new(),
                    },
                    &self.queries,
                ) {
                    Ok(output) => output.lints.clone(),
                    Err(errors) => errors,
                }
            }
            Input::File { name, bytes } => {
                let directory = tempfile::tempdir().expect("临时目录");
                let path = directory.path().join(name);
                if let Some(bytes) = bytes {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).expect("创建 fixture 父目录");
                    }
                    std::fs::write(&path, bytes).expect("写入 fixture");
                }
                self.compiler
                    .compile(CompileRequest::single_file_path(&path, TARGET))
                    .diagnostics()
                    .items()
                    .to_vec()
            }
            Input::SymlinkTree { link } => {
                let directory = tempfile::tempdir().expect("临时目录");
                let root = directory.path();
                let source_root = root.join("src");
                std::fs::create_dir(&source_root).expect("创建源码目录");
                std::fs::write(source_root.join("main.gg"), "fn main() {}\n").expect("写入入口");
                create_symlink(&source_root.join(link), &source_root.join("main.gg"))
                    .expect("创建符号链接 fixture");
                let target = Target::new(
                    TargetKind::Bin,
                    "demo".to_owned(),
                    source_root.join("main.gg"),
                    source_root,
                    Vec::new(),
                    false,
                );
                let package = Package::new(
                    root.to_path_buf(),
                    root.join("gugu.toml"),
                    None,
                    "demo".to_owned(),
                    "1.0.0".to_owned(),
                    Vec::new(),
                    vec![target],
                );
                self.compiler
                    .compile(CompileRequest::project_target(
                        &package,
                        package.targets().first().expect("bin target"),
                        "demo@1.0.0 (path+.)",
                        TARGET,
                        Vec::new(),
                        BTreeSet::new(),
                    ))
                    .diagnostics()
                    .items()
                    .to_vec()
            }
            Input::DeepRefType { depth } => {
                // `&&` 是独立记号，嵌套引用必须写成空格分隔的单个 `&`。
                let reference = "& ".repeat(*depth as usize);
                let text = format!("fn main() {{ let x: {reference}int\n _ = x }}");
                self.compile_memory("src/main.gg", &text)
            }
            Input::SpanOutOfBounds => {
                let snapshot =
                    SourceSnapshot::from_str("probe.gg", "fn main() {}").expect("fixture 合法");
                let source_map = SourceMap::new(vec![snapshot]).expect("fixture 逻辑路径唯一");
                let file = source_map.file_id("probe.gg").expect("fixture 已注册");
                let end = source_map
                    .snapshot(file)
                    .expect("fixture 快照存在")
                    .content()
                    .len()
                    + 1;
                crate::frontend::source_map_span(&source_map, file, 0, end)
                    .expect_err("越界 span 必须被拒绝")
            }
        }
    }

    /// 以内存单文件请求编译并取回诊断。
    fn compile_memory(&self, path: &str, text: &str) -> Vec<Diagnostic> {
        self.compiler
            .compile(CompileRequest::single_file(path, text, TARGET))
            .diagnostics()
            .items()
            .to_vec()
    }
}

#[cfg(not(target_os = "windows"))]
fn create_symlink(link: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(target_os = "windows")]
fn create_symlink(link: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

/// 诊断列表的稳定渲染，用于漂移报告。
fn render(diagnostics: &[Diagnostic]) -> String {
    if diagnostics.is_empty() {
        return "<无诊断>".to_owned();
    }
    diagnostics
        .iter()
        .map(Diagnostic::render_text)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// 判断一条诊断是否满足契约条目。
fn entry_matches(entry: &RowDiagnostic, observed: &Diagnostic) -> bool {
    if observed.code() != entry.code || observed.severity() != entry.severity {
        return false;
    }
    match (entry.position, observed.span()) {
        (Some((line, column)), Some(span)) => {
            if (span.line(), span.column()) != (line, column) {
                return false;
            }
            if let Some(suffix) = entry.path_suffix
                && !span.path().to_string_lossy().ends_with(suffix)
            {
                return false;
            }
        }
        (None, None) => {}
        _ => return false,
    }
    observed.message().contains(entry.message)
}

/// 契约条目的人读描述。
fn describe(entry: &RowDiagnostic) -> String {
    match entry.position {
        Some((line, column)) => format!(
            "{}[{}] {line}:{column}：{}",
            entry.severity, entry.code, entry.message
        ),
        None => format!(
            "{}[{}]（无 span）：{}",
            entry.severity, entry.code, entry.message
        ),
    }
}

/// 逐项核对一行活体输入与契约，返回全部漂移说明。
fn drift(runner: &Runner, case: &Case) -> Vec<String> {
    let Decision::Live { input, expected } = case.decision else {
        return Vec::new();
    };
    let diagnostics = runner.diagnostics(&input);
    let problems = match expected {
        Expected::Silent => {
            if diagnostics.is_empty() {
                Vec::new()
            } else {
                vec![format!("期望静默，实际 {} 条诊断", diagnostics.len())]
            }
        }
        Expected::Diagnostic {
            severity,
            position,
            path_suffix,
            notes,
            message,
            companions,
        } => {
            let primary = RowDiagnostic {
                code: case.code,
                severity,
                position,
                path_suffix,
                message,
            };
            let mut problems = Vec::new();
            let mut matched = vec![false; diagnostics.len()];
            for entry in std::iter::once(&primary).chain(companions) {
                let found = diagnostics
                    .iter()
                    .enumerate()
                    .position(|(index, observed)| {
                        !matched[index] && entry_matches(entry, observed)
                    });
                match found {
                    Some(index) => matched[index] = true,
                    None => problems.push(format!("缺少诊断 {}", describe(entry))),
                }
            }
            let mut unmatched_notes = 0_u32;
            for (index, observed) in diagnostics.iter().enumerate() {
                if matched[index] {
                    continue;
                }
                if observed.severity() == Severity::Note {
                    unmatched_notes += 1;
                } else {
                    problems.push(format!("意外诊断 {}", observed.render_text()));
                }
            }
            if unmatched_notes != u32::from(notes) {
                problems.push(format!("附注 {unmatched_notes} 条（期望 {notes}）"));
            }
            problems
        }
    };
    if problems.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "{}: {}；实观测：{}",
            case.code,
            problems.join("；"),
            render(&diagnostics)
        )]
    }
}

/// 每个代码都必须有决策，且表按 `code()` 稳定升序排列。
#[test]
fn every_code_has_a_decision() {
    let mut verdict: [Option<DiagnosticCode>; DiagnosticCode::ALL.len()] =
        [None; DiagnosticCode::ALL.len()];
    let mut previous = None;
    for case in CASES {
        let index = case.code.index();
        if let Some(previous) = previous {
            assert!(
                previous <= index,
                "CASES 必须按 code 升序：{} 之后出现 {}",
                DiagnosticCode::ALL[previous],
                case.code
            );
        }
        previous = Some(index);
        verdict[index] = Some(case.code);
    }
    let missing = verdict
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.is_none())
        .map(|(index, _)| DiagnosticCode::ALL[index].to_string())
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "以下诊断代码没有契约决策：{}",
        missing.join("、")
    );
}

/// `ALL` 与 `index()` 必须完全一致且严格升序。
#[test]
fn all_is_dense_and_ascending() {
    for (index, code) in DiagnosticCode::ALL.iter().enumerate() {
        assert_eq!(code.index(), index, "{code} 的稠密下标与 ALL 槽位不一致");
    }
    for pair in DiagnosticCode::ALL.windows(2) {
        assert!(
            pair[0].to_string() < pair[1].to_string(),
            "ALL 必须按稳定代码严格升序：{}、{}",
            pair[0],
            pair[1]
        );
    }
}

/// 全部活体输入必须与实装诊断逐项一致。
#[test]
fn cases_match_the_compiler() {
    let runner = Runner::new();
    let drift = CASES
        .iter()
        .flat_map(|case| drift(&runner, case))
        .collect::<Vec<_>>();
    assert!(
        drift.is_empty(),
        "诊断契约漂移（实装行为与契约表不一致）：\n{}",
        drift.join("\n")
    );
}
