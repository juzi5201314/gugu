//! 类型化表达式与确定初始化检查共用唯一的局部槽表。
pub(crate) mod assembly;
pub(crate) mod borrow;
mod checker;
pub(crate) mod foreign;
mod initialization;
pub(crate) mod linkage;
pub(crate) mod model;
mod numeric;
mod opaque;
mod output;
mod patterns;
mod query;
mod safety;
mod traits;
pub(crate) use output::{CheckedBody, CheckedSemantics, MemoryOperation};
#[cfg(test)]
mod callable_tests;
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
    queries: &crate::query::QueryEngine,
) -> Result<(CheckedSemantics, Vec<super::types::Layout>), Vec<Diagnostic>> {
    let model = model::Model::new(modules, names)?;
    let checked = query::check(&model, sources, cfg, queries)?;
    let layouts = super::types::form_and_layout(&model, &checked, cfg.target())?;
    Ok((checked, layouts))
}
