//! 闭世界单态化：从冻结 HIR 与已检查语义收集根、闭合实例图并固定分析身份键。
//!
//! 本模块的实例身份按 [单态化与编译缓存](../../../docs/src/internals/monomorphization-cache.md)
//! 的 `gugu-mono-v1` 域与 GBC1 规范编码；`MonoId` 只在收集闭合后按 key 字节序
//! 分配。闭合在 `LowerHir` 冻结后运行，替换与 impl 选择复用语义层的
//! `substitute`/`select_impl`，不建立第二套 trait 选择。

pub mod collect;
pub mod instantiate;
pub mod keys;
pub mod roots;
pub mod storage;
pub mod summary;
#[cfg(all(test, feature = "slow-tests"))]
mod tests;
mod types;
pub mod universe;

pub use collect::{MonoWorldV1, empty_world};

use crate::hash::hash_domain;
use crate::query::QueryEngine;

/// 收集根并闭合可达实例图。
pub fn close(
    context: &keys::MonoContext<'_>,
    queries: &QueryEngine,
    gir: &crate::gir::GirWorldV1,
) -> Result<MonoWorldV1, Vec<crate::Diagnostic>> {
    let mut interner = keys::MonoInterner::default();
    collect::close(context, &mut interner, queries, gir)
}

/// `MonoKey` 规范字节的域摘要。
pub fn digest_of(mono_key: &[u8]) -> [u8; 32] {
    hash_domain("gugu-mono-v1", mono_key)
}
