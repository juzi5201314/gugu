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
pub(crate) use hir::identity::Identities;
pub(crate) use model::{CallableId, DefRef, Model, Ty, substitute};
pub(crate) use output::{CheckedBody, CheckedSemantics, MemoryOperation};
pub(crate) use traits::TraitRef;
#[cfg(test)]
mod callable_tests;
#[cfg(test)]
mod comptime_tests;
#[cfg(test)]
mod opaque_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod trait_tests;
#[cfg(test)]
mod unsafe_tests;

use super::{ParsedModule, names::NameResolution};
use crate::diagnostics::Diagnostic;

pub(crate) fn check(
    modules: &[ParsedModule],
    names: &NameResolution,
    sources: &crate::SourceMap,
    cfg: &super::cfg::CfgContext,
    entry: Option<model::CallableId>,
    queries: &crate::query::QueryEngine,
) -> Result<
    (
        CheckedSemantics,
        Vec<super::types::Layout>,
        (u32, [u8; 32]),
        super::hir::Validated,
        analysis::AnalysisWorldV1,
        super::mono::MonoWorldV1,
        super::gir::GirWorldV1,
    ),
    Vec<Diagnostic>,
> {
    let model = model::Model::new(modules, names)?;
    let (early, early_dependency) = comptime::evaluate(&model, sources, cfg, queries)?;
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
    )?;
    let gir = super::gir::build_world(&hir, queries, sources)?;
    let identities =
        hir::identities(&model, names, &checked, sources).map_err(|error| vec![error])?;
    let context = super::mono::keys::MonoContext::new(
        &model,
        &checked,
        &identities,
        hir.module(),
        sources,
        cfg.target(),
        cfg.harness(),
    );
    let mut mono_world = super::mono::close(&context, queries)?;
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
    Ok((
        checked,
        layouts,
        registry_identity,
        hir,
        analysis_world,
        mono_world,
        gir,
    ))
}
