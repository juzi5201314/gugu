//! 类型化表达式与确定初始化检查共用唯一的局部槽表。
pub mod assembly;
pub mod borrow;
mod checker;
pub mod comptime;
pub mod foreign;
mod hir;
pub use hir::{LOWER_SCHEMA, owner_dependency_fingerprint};
pub mod initialization;
pub mod linkage;
pub mod model;
mod numeric;
mod opaque;
pub mod output;
mod patterns;
pub mod query;
mod safety;
pub mod traits;
pub use checker::must_use::lints as must_use_lints;
pub use hir::identity::Identities;
pub use model::{CallableId, DefRef, Model, Ty, substitute};
pub use output::SCHEMA_VERSION as CHECKED_SCHEMA_VERSION;
pub use output::{CheckedBody, CheckedSemantics, MemoryOperation};
pub use traits::TraitRef;

use super::{ParsedModule, names::NameResolution};
use crate::Diagnostic;
/// 语义层派生冻结出的产物：GIR 层以它为输入继续派生。
///
/// 字段顺序即管线顺序：早期常量 → 类型检查 → 布局 → HIR → 身份。`model` 借用模块表与名称
/// 解析，单态化上下文需要它；`dependency`/`lower_dependency` 是下游分析的 query 依赖。
pub struct SemaWorld<'a> {
    pub model: model::Model<'a>,
    pub checked: CheckedSemantics,
    /// 早期常量表：构建期物化读取，用户编译路径只消费冻结进 HIR 的结果。
    pub early: comptime::EarlyConstTable,
    pub layouts: Vec<super::types::Layout>,
    pub registry_identity: (u32, [u8; 32]),
    pub hir: super::hir::Validated,
    /// 身份表：构建期物化读取，用户编译路径只经查询键间接消费。
    pub identities: hir::identity::Identities,
    pub dependency: crate::query::DependencyFingerprint,
    pub lower_dependency: crate::query::DependencyFingerprint,
}

#[expect(
    clippy::too_many_arguments,
    reason = "语义层入口需要模块表、名称解析、源码映射、cfg、入口、query 引擎与内建前缀"
)]
pub fn check<'a>(
    modules: &'a [ParsedModule],
    names: &'a NameResolution,
    sources: &crate::SourceMap,
    cfg: &super::cfg::CfgContext,
    entry: Option<model::CallableId>,
    queries: &crate::query::QueryEngine,
    runtime_paths: &std::collections::BTreeSet<String>,
    prefix: Option<&super::sema_prefix::SemaPrefix>,
    embed: Option<comptime::embed::EmbedHost>,
) -> Result<SemaWorld<'a>, Vec<Diagnostic>> {
    let mut model = model::Model::new(modules, names, runtime_paths)?;
    model.embed = embed;
    let (mut early, early_dependency) = comptime::evaluate(&model, sources, cfg, queries, prefix)?;
    if let Some(prefix) = prefix {
        // 内建定义的早期常量直取物化表；用户定义现算后按定义序追加。
        let mut constants = prefix.early_constants().constants.clone();
        constants.extend(early.constants.suffix_mut().drain(..));
        early.constants = constants;
    }
    let registry_identity = early.registry_identity();
    let (checked, dependency) = query::check(
        &model,
        sources,
        cfg,
        queries,
        &early,
        &early_dependency,
        prefix,
    )?;
    let layouts = super::types::form_and_layout(&model, &checked, cfg.target())?;
    let (hir, lower_dependency) = hir::lower(
        &model,
        names,
        &checked,
        sources,
        entry,
        &dependency,
        queries,
        prefix,
    )?;
    let identities =
        hir::identities(&model, names, &checked, sources, prefix).map_err(|error| vec![error])?;
    Ok(SemaWorld {
        model,
        checked,
        early,
        layouts,
        registry_identity,
        hir,
        identities,
        dependency,
        lower_dependency,
    })
}
