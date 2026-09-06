use super::*;
use crate::frontend::analysis::{self, AnalysisPolicyV1};
use crate::frontend::semantics::comptime::EarlyConstTable;
use crate::query::{DependencyFingerprint, QueryEngine, QueryKey, QueryKind};

pub(super) fn lower(
    model: &Model<'_>,
    names: &NameResolution,
    checked: &CheckedSemantics,
    early: &EarlyConstTable,
    sources: &SourceMap,
    cfg: &crate::frontend::cfg::CfgContext,
    entry: Option<CallableId>,
    dependency: &DependencyFingerprint,
    queries: &QueryEngine,
) -> Result<(hir::Validated, analysis::AnalysisWorldV1), Vec<Diagnostic>> {
    let mut hash = blake3::Hasher::new_derive_key("gugu-lower-hir-input-v1");
    hash.update(&dependency.fingerprint());
    if let Some(entry) = entry {
        hash.update(model.modules[entry.module].path.as_bytes());
        hash.update(b"main");
    }
    for expansion in sources.expansions() {
        hash.update(&expansion.parent().as_u32().to_le_bytes());
        hash.update(&expansion.source_hash());
        hash.update(expansion.macro_call().path().as_os_str().as_encoded_bytes());
        hash.update(&expansion.macro_call().start().to_le_bytes());
        hash.update(&expansion.macro_call().end().to_le_bytes());
    }
    let lower_input_fingerprint = *hash.finalize().as_bytes();
    let key = QueryKey::new(QueryKind::LowerHir, 1, lower_input_fingerprint);
    let policy = AnalysisPolicyV1::default();
    let mut fresh = None;
    let key_for_dep = key.clone();
    let result = queries.compute(key.clone(), |context| {
        context.record_dependency(dependency.key().clone(), dependency.fingerprint());
        let formed = match Builder::new(model, names, checked, sources, entry)
            .and_then(Builder::build)
        {
            Ok(mut module) => {
                module.input_fingerprint = lower_input_fingerprint;
                let mut world = analysis::solver::analyze(&module, checked, early, model, policy);
                analysis::patch_module(&mut module, &world);
                match hir::Validated::freeze(module) {
                    Ok((validated, bytes)) => {
                        world.input_fingerprint = analysis::query::input_fingerprint(
                            checked,
                            early,
                            &validated,
                            cfg,
                            dependency,
                            &DependencyFingerprint::new(
                                key_for_dep.clone(),
                                lower_input_fingerprint,
                            ),
                            policy,
                        );
                        fresh = Some((validated, world));
                        Ok((bytes, Vec::new()))
                    }
                    Err(error) => Err(super::super::query::store_errors(&[error])),
                }
            }
            Err(error) => Err(super::super::query::store_errors(&[error])),
        };
        formed
    });
    let result = result.map_err(|error| super::super::query::restore_errors(error, sources))?;
    if let Some(pair) = fresh {
        return Ok(pair);
    }
    let mut module: hir::Module = serde_json::from_slice(result.payload())
        .map_err(|_| vec![invalid("HIR query 缓存 schema 不合法")])?;
    if module.input_fingerprint != lower_input_fingerprint {
        return Err(vec![invalid("HIR query 输入身份不匹配")]);
    }
    let mut world = analysis::solver::analyze(&module, checked, early, model, policy);
    analysis::patch_module(&mut module, &world);
    let (validated, canonical) = hir::Validated::freeze(module).map_err(|error| vec![error])?;
    if canonical != result.payload() {
        return Err(vec![invalid("HIR query 结果不是规范序列化")]);
    }
    world.input_fingerprint = analysis::query::input_fingerprint(
        checked,
        early,
        &validated,
        cfg,
        dependency,
        &DependencyFingerprint::new(key.clone(), lower_input_fingerprint),
        policy,
    );
    Ok((validated, world))
}

fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::InvalidType, message, None)
}
