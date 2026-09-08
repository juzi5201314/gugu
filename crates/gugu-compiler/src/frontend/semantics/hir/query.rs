use super::*;
use crate::frontend::analysis::{self, AnalysisPolicyV1};
use crate::frontend::mono;
use crate::query::{DependencyFingerprint, QueryEngine, QueryKey, QueryKind};

pub(super) fn lower(
    model: &Model<'_>,
    names: &NameResolution,
    checked: &CheckedSemantics,
    sources: &SourceMap,
    cfg: &crate::frontend::cfg::CfgContext,
    entry: Option<CallableId>,
    dependency: &DependencyFingerprint,
    queries: &QueryEngine,
) -> Result<(hir::Validated, analysis::AnalysisWorldV1, mono::MonoWorldV1), Vec<Diagnostic>> {
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
    let key = QueryKey::new(QueryKind::LowerHir, 2, lower_input_fingerprint);
    let policy = AnalysisPolicyV1::default();
    let lower_dependency = DependencyFingerprint::new(key.clone(), lower_input_fingerprint);
    // 身份表在 query 外构造：缓存命中路径同样需要它重跑单态化闭合。
    let (definitions, identities) =
        identity::collect(model, names, checked, sources).map_err(|error| vec![error])?;
    let mut fresh = None;
    let result = queries.compute(key.clone(), |context| {
        context.record_dependency(dependency.key().clone(), dependency.fingerprint());
        let formed = match Builder::new(model, &identities, definitions, checked, sources, entry)
            .and_then(Builder::build)
        {
            Ok(mut module) => {
                module.input_fingerprint = lower_input_fingerprint;
                let (world, mono_world) = form(
                    &mut module,
                    model,
                    checked,
                    &identities,
                    cfg,
                    dependency,
                    &lower_dependency,
                    policy,
                    queries,
                    sources,
                )?;
                let (validated, canonical) = hir::Validated::freeze(module)
                    .map_err(|error| super::super::query::store_errors(&[error]))?;
                fresh = Some((validated, world, mono_world));
                Ok((canonical, Vec::new()))
            }
            Err(error) => Err(super::super::query::store_errors(&[error])),
        };
        formed
    });
    let result = result.map_err(|error| super::super::query::restore_errors(error, sources))?;
    if let Some(triple) = fresh {
        return Ok(triple);
    }
    // 缓存命中：载荷是 patch 后的模块，重跑闭世界与分析恢复 world 与摘要，
    // 再校验 canonical 字节不变。
    let mut module: hir::Module = serde_json::from_slice(result.payload())
        .map_err(|_| vec![invalid("HIR query 缓存 schema 不合法")])?;
    if module.input_fingerprint != lower_input_fingerprint {
        return Err(vec![invalid("HIR query 输入身份不匹配")]);
    }
    // strip 掉缓存的 proof 再算输入身份，与分析侧口径一致。
    for owner in &mut module.owners {
        for check in &mut owner.checks {
            check.proof = None;
        }
    }
    let (world, mono_world) = form(
        &mut module,
        model,
        checked,
        &identities,
        cfg,
        dependency,
        &lower_dependency,
        policy,
        queries,
        sources,
    )
    .map_err(|error| super::super::query::restore_errors(error, sources))?;
    let (validated, canonical) = hir::Validated::freeze(module).map_err(|error| vec![error])?;
    if canonical != result.payload() {
        return Err(vec![invalid("HIR query 结果不是规范序列化")]);
    }
    Ok((validated, world, mono_world))
}

/// 冻结前完成单态化闭合、whole-program 分析、proof 写回与公共摘要投影。
#[allow(clippy::too_many_arguments)]
fn form(
    module: &mut hir::Module,
    model: &Model<'_>,
    checked: &CheckedSemantics,
    identities: &Identities,
    cfg: &crate::frontend::cfg::CfgContext,
    dependency: &DependencyFingerprint,
    lower_dependency: &DependencyFingerprint,
    policy: AnalysisPolicyV1,
    queries: &QueryEngine,
    sources: &SourceMap,
) -> Result<(analysis::AnalysisWorldV1, mono::MonoWorldV1), crate::query::QueryError> {
    module
        .verify()
        .map_err(|error| super::super::query::store_errors(&[error]))?;
    // 分析的输入身份取 proof 写回前的模块指纹；proof 字段不参与
    // 字面量/类型/调用图事实，patch 前后分析结果一致。
    let pre_freeze_fingerprint = module_fingerprint(module);
    let context = mono::keys::MonoContext::new(
        model,
        checked,
        identities,
        module,
        sources,
        cfg.target(),
        cfg.harness(),
    );
    let mut mono_world = mono::close(&context, queries)
        .map_err(|errors| super::super::query::store_errors(&errors))?;
    crate::frontend::late::run(module, &mut mono_world, queries, sources)
        .map_err(|errors| super::super::query::store_errors(&errors))?;
    let (world, analysis_dependency) = analysis::run_world(
        module,
        pre_freeze_fingerprint,
        &mono_world,
        cfg,
        dependency,
        lower_dependency,
        policy,
        queries,
        sources,
    )
    .map_err(|errors| super::super::query::store_errors(&errors))?;
    let summaries = mono::summary::project(
        cfg.target(),
        &mono_world,
        &world,
        &analysis_dependency,
        queries,
    )
    .map_err(|errors| super::super::query::store_errors(&errors))?;
    mono_world.public_summaries = summaries;
    analysis::patch_module(module, &world);
    Ok((world, mono_world))
}

/// 与 `Validated::freeze` 一致的模块规范序列化指纹。
fn module_fingerprint(module: &hir::Module) -> [u8; 32] {
    let bytes = serde_json::to_vec(module).expect("HIR schema 序列化");
    *blake3::Hasher::new_derive_key("gugu-validated-hir-v1")
        .update(&bytes)
        .finalize()
        .as_bytes()
}

fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::InvalidType, message, None)
}
