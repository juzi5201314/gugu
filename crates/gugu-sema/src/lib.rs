#![forbid(unsafe_code)]

//! 语义层：词法、语法、cfg、源码宏展开、名称解析、类型检查、早期 comptime 与 HIR 冻结。
//!
//! 本 crate 的产物止于冻结 HIR 与类型检查表（`semantics::SemaWorld`）；generic GIR、单态化、
//! late comptime、全程序分析与后端都在上层。内建块的 sema 层物化表由 [`sema_prefix::SemaPrefix`]
//! 安装，GIR 层物化表不进入本层。只依赖 `gugu-base` 与 `gugu-runtime-abi`。

use gugu_base::{canonical, diagnostics, query, source, target};

use diagnostics::{Diagnostic, DiagnosticCode, Severity};
use source::{ExpansionId, SourceFileId, SourceMap, Span};
use target::TargetName;

pub mod ast;
pub mod attr;
pub mod cfg;
pub mod expand;
pub mod format;
pub mod hir;
pub mod intern;
pub mod lex;
pub mod modules;
pub mod names;
pub mod parse;
pub mod sema_prefix;
pub mod semantics;
pub mod string;
pub mod token;
pub mod types;

pub use lex::lex;
pub use parse::parse;

/// 一个已完成词法、语法与 cfg 配置的源码模块。
#[derive(Clone, Debug)]
pub struct ParsedModule {
    pub path: String,
    pub file: ast::AstFile,
    pub arena: ast::AstArena,
    pub tokens: token::TokenBuffer,
    pub configured: cfg::ConfiguredAst,
    /// 该模块来自 compiler 内建的源树登记，而不是用户源码。
    ///
    /// `std.option`、`std.runtime` 等公开标准库模块是 package API。`std.prelude`、`std.runtime.*` 与
    /// `runtime.*` 仍是私有实现：用户源码不能声明 `std` 根，也不能导入这些私有模块。
    pub builtin: bool,
}

/// 探针入口程序所在的内建源前缀。
const ENTRY_ONLY_BUILTIN_SOURCES: &str = "std/runtime/fixtures/";

/// 探针专用内建源：探针库与探针入口程序。
///
/// 它们随 `runtime-gc-probes` 特性进入 artifact 源表，只供 `CompileInput::RuntimeGcProbe`
/// 的编译安装。普通编译一旦安装：探针库会把自身的大拷贝诊断混进用户诊断，探针入口程序会为
/// 本不属于该程序的 `main` 多派生一份世界，且其中 `#[cfg(runtime_gc_case = ...)]` 的选择
/// 函数在探针编译之外全部落空。
pub fn is_probe_builtin_source(logical_path: &str) -> bool {
    logical_path == "std/runtime/gc_probes.gg"
        || logical_path.starts_with(ENTRY_ONLY_BUILTIN_SOURCES)
}

/// 内建块的包身份：artifact 源表在用户 package 之外，必须与任何用户包身份区分。
pub const BUILTIN_PACKAGE_IDENTITY: &str = "builtin";
