//! GIR 层以下的再导出：sema 模块来自 `gugu-sema`，GIR 层与前端入口来自 `gugu-gir`。
//!
//! 这里只保留需要完整 `Compiler` 的前端回归；只依赖 GIR 层输入的测试留在 `gugu-gir`。

#[cfg(all(test, feature = "slow-tests"))]
pub(crate) use gugu_gir::semantics;
pub(crate) use gugu_gir::{
    FrontendOutput, PrefixMode, SourceInput, analysis, bootstrap_with, cfg, contract, format, gc,
    gir, hir, mono, prebuild,
};
#[cfg(test)]
pub(crate) use gugu_gir::{ast, bootstrap, installs_builtin_source, source_map_span};

#[cfg(test)]
use gugu_sema::{lex, parse};
#[cfg(test)]
#[allow(unused_imports, reason = "parse helper 仅由部分测试目标共享")]
use parse::{dump_ast, has_main_fn, parent_before_child};

#[cfg(test)]
mod docs_code_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod gir_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod lex_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod mono_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod parse_tests;
#[cfg(test)]
mod semantics_tests;
