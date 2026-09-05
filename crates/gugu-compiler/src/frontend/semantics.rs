//! 类型化表达式与确定初始化检查共用唯一的局部槽表。
mod checker;
mod initialization;
pub(crate) mod model;
mod numeric;
mod output;
mod patterns;
mod query;
pub(crate) use output::CheckedSemantics;
#[cfg(test)]
mod callable_tests;
#[cfg(test)]
mod tests;

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
    let layouts = super::types::form_and_layout(&model, &checked)?;
    Ok((checked, layouts))
}
