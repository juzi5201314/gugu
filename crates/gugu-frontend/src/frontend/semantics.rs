//! 类型化表达式与确定初始化检查共用唯一的局部槽表。
pub(crate) mod assembly;
pub(crate) use crate::frontend::analysis;
pub(crate) mod borrow;
mod checker;
pub(crate) mod comptime;
pub(crate) mod foreign;
mod hir;
mod initialization;
pub(crate) mod linkage;
pub(crate) mod model;
mod numeric;
mod opaque;
mod output;
mod patterns;
pub(crate) mod query;
mod safety;
pub(crate) mod traits;
pub(crate) use checker::must_use::lints as must_use_lints;
pub(crate) use hir::identity::Identities;
pub(crate) use model::{CallableId, DefRef, Model, Ty, substitute};
pub(crate) use output::SCHEMA_VERSION as CHECKED_SCHEMA_VERSION;
pub(crate) use output::{CheckedBody, CheckedSemantics, MemoryOperation};
pub(crate) use traits::TraitRef;
#[cfg(all(test, feature = "slow-tests"))]
mod callable_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod comptime_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod opaque_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod runtime_tests;
#[cfg(test)]
mod tests;
#[cfg(all(test, feature = "slow-tests"))]
mod trait_tests;
#[cfg(all(test, feature = "slow-tests"))]
mod unsafe_tests;

use super::{ParsedModule, names::NameResolution};
use crate::Diagnostic;
/// 一次前端派生冻结出的全部 world 产物。
///
/// 字段顺序即管线顺序：早期常量 → 类型检查 → 布局 → HIR → 身份 → 分析 → 单态化 → GIR。
/// 构建期物化从这里取与用户无关的层，用户编译路径消费同一组字段。
pub(crate) struct DerivedWorld {
    pub(crate) checked: CheckedSemantics,
    /// 早期常量表：构建期物化读取，用户编译路径只消费冻结进 HIR 的结果。
    pub(crate) early: comptime::EarlyConstTable,
    pub(crate) layouts: Vec<super::types::Layout>,
    pub(crate) registry_identity: (u32, [u8; 32]),
    pub(crate) hir: super::hir::Validated,
    /// 身份表：构建期物化读取，用户编译路径只经查询键间接消费。
    pub(crate) identities: hir::identity::Identities,
    pub(crate) analysis: analysis::AnalysisWorldV1,
    pub(crate) mono: super::mono::MonoWorldV1,
    pub(crate) gir: super::gir::GirWorldV1,
    pub(crate) gir_stats: super::gir::pass::GirPassStats,
}

pub(crate) fn check(
    modules: &[ParsedModule],
    names: &NameResolution,
    sources: &crate::SourceMap,
    cfg: &super::cfg::CfgContext,
    entry: Option<model::CallableId>,
    queries: &crate::query::QueryEngine,
    runtime_paths: &std::collections::BTreeSet<String>,
    prefix: Option<&super::prefix::BuiltinPrefix>,
) -> Result<DerivedWorld, Vec<Diagnostic>> {
    let model = model::Model::new(modules, names, runtime_paths)?;
    let (mut early, early_dependency) = comptime::evaluate(&model, sources, cfg, queries, prefix)?;
    if let Some(prefix) = prefix {
        // 内建定义的早期常量直取物化表；用户定义现算后按定义序追加。
        let mut constants = prefix.early_constants().constants.clone();
        constants.append(&mut early.constants);
        early.constants = constants;
    }
    let registry_identity = early.registry_identity();
    let (checked, dependency) =
        query::check(&model, sources, cfg, queries, &early, &early_dependency)?;
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
    let gir = super::gir::build_world(&hir, queries, sources, prefix)?;
    let identities =
        hir::identities(&model, names, &checked, sources, prefix).map_err(|error| vec![error])?;
    let context = super::mono::keys::MonoContext::new(
        &model,
        &checked,
        &identities,
        hir.module(),
        sources,
        cfg.target(),
        cfg.harness(),
    );
    let mut mono_world = super::mono::close(&context, queries, &gir)?;
    // 内建 link 未被裁剪时，契约记录类型目录直取物化表：七个 layout verifier 仍随后逐字段
    // 校验它，加载即校验的规矩不变。
    if let Some(prefix) = prefix {
        mono_world.contracts = prefix.record_types().to_vec();
    }
    super::late::run(hir.module(), &mut mono_world, queries, sources)?;
    let gir = super::gir::attach_fragments(gir, &mono_world);
    let (analysis_world, analysis_dependency) = analysis::run_world(
        hir.module(),
        hir.fingerprint(),
        &gir,
        &mono_world,
        cfg,
        &dependency,
        &lower_dependency,
        analysis::AnalysisPolicyV1::default(),
        queries,
        sources,
    )?;
    let gir = super::gir::place_world(&hir, gir, &analysis_world, queries, sources)?;
    let summaries = super::mono::summary::project(
        cfg.target(),
        &mono_world,
        &analysis_world,
        &analysis_dependency,
        queries,
    )?;
    mono_world.public_summaries = summaries;
    let mut gir = gir;
    gir.concrete = super::gir::concrete::build(&context, &gir, &mono_world)?;
    let gir_stats = super::gir::pass::run(&mut gir, hir.module(), &analysis_world)?;
    gir.fingerprint = super::gir::world_fingerprint(
        &gir.bodies,
        &gir.fragments,
        gir.hir_fingerprint,
        &gir.placement,
        &gir.concrete,
    );
    Ok(DerivedWorld {
        checked,
        early,
        layouts,
        registry_identity,
        hir,
        identities,
        analysis: analysis_world,
        mono: mono_world,
        gir,
        gir_stats,
    })
}
