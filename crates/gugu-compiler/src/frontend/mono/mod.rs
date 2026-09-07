//! 闭世界单态化：从冻结前 HIR 与已检查语义收集根、闭合实例图并固定分析身份键。
//!
//! 本模块的实例身份按 [单态化与编译缓存](../../../docs/src/internals/monomorphization-cache.md)
//! 的 `gugu-mono-v1` 域与 GBC1 规范编码；`MonoId` 只在收集闭合后按 key 字节序
//! 分配。闭合嵌套在 `LowerHir` compute 内运行——替换与 impl 选择复用语义层的
//! `substitute`/`select_impl`，不建立第二套 trait 选择。

pub(crate) mod collect;
pub(crate) mod instantiate;
pub(crate) mod keys;
pub(crate) mod roots;
pub(crate) mod summary;
#[cfg(test)]
mod tests;

pub(crate) use collect::{MonoWorldV1, empty_world};
pub(crate) use keys::hash_domain;

use crate::frontend::hir::Module;
use crate::frontend::semantics::{CheckedSemantics, Identities, Model};
use crate::query::QueryEngine;
use crate::target::TargetName;

/// 收集根并闭合可达实例图。
pub(crate) fn close(
    model: &Model<'_>,
    checked: &CheckedSemantics,
    identities: &Identities,
    module: &Module,
    target: TargetName,
    harness: bool,
    queries: &QueryEngine,
) -> Result<MonoWorldV1, Vec<crate::Diagnostic>> {
    let context = keys::MonoContext::new(model, checked, identities, module, target, harness);
    let mut interner = keys::MonoInterner::default();
    collect::close(&context, &mut interner, queries)
}

/// `MonoKey` 规范字节的域摘要。
pub(crate) fn digest_of(mono_key: &[u8]) -> [u8; 32] {
    hash_domain("gugu-mono-v1", mono_key)
}
