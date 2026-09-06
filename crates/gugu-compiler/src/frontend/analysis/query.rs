//! `WholeProgramAnalysis` query 与输入 fingerprint。

use super::policy::AnalysisPolicyV1;
use super::solver;
use super::types::AnalysisWorldV1;
use crate::SourceMap;
use crate::frontend::cfg::CfgContext;
use crate::frontend::hir::Module;
use crate::query::{DependencyFingerprint, QueryEngine, QueryKey, QueryKind};

/// 运行 whole-program 分析并按输入指纹缓存；`pre_freeze_fingerprint` 是
/// proof 写回前的 HIR 模块指纹，作为 world 输入身份（而非混合了证明输出）。
pub(crate) fn run_world(
    module: &Module,
    pre_freeze_fingerprint: [u8; 32],
    cfg: &CfgContext,
    type_check_dependency: &DependencyFingerprint,
    lower_hir_dependency: &DependencyFingerprint,
    policy: AnalysisPolicyV1,
    queries: &QueryEngine,
    sources: &SourceMap,
) -> Result<AnalysisWorldV1, Vec<crate::Diagnostic>> {
    let input_fingerprint = input_fingerprint(
        pre_freeze_fingerprint,
        cfg,
        type_check_dependency,
        lower_hir_dependency,
        policy,
    );
    let key = QueryKey::new(QueryKind::WholeProgramAnalysis, 1, input_fingerprint);
    let mut fresh = None;
    let result = queries.compute(key, |context| {
        context.record_dependency(
            type_check_dependency.key().clone(),
            type_check_dependency.fingerprint(),
        );
        context.record_dependency(
            lower_hir_dependency.key().clone(),
            lower_hir_dependency.fingerprint(),
        );
        let mut world = solver::analyze(module, policy);
        world.input_fingerprint = input_fingerprint;
        let bytes = serde_json::to_vec(&world).expect("analysis world serializes");
        fresh = Some(world);
        Ok((bytes, Vec::new()))
    });
    let result = result
        .map_err(|error| crate::frontend::semantics::query::restore_errors(error, sources))?;
    if let Some(world) = fresh {
        return Ok(world);
    }
    let world: AnalysisWorldV1 = serde_json::from_slice(result.payload())
        .map_err(|_| vec![invalid("WholeProgramAnalysis 缓存 schema 不合法")])?;
    if world.input_fingerprint != input_fingerprint {
        return Err(vec![invalid("WholeProgramAnalysis 输入身份不匹配")]);
    }
    Ok(world)
}

/// world 输入指纹：冻结前 HIR、两级 query 依赖与策略编码。
pub(crate) fn input_fingerprint(
    pre_freeze_fingerprint: [u8; 32],
    cfg: &CfgContext,
    type_check_dependency: &DependencyFingerprint,
    lower_hir_dependency: &DependencyFingerprint,
    policy: AnalysisPolicyV1,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-abstract-analysis-input-v1");
    hash.update(&pre_freeze_fingerprint);
    hash.update(&type_check_dependency.fingerprint());
    hash.update(&lower_hir_dependency.fingerprint());
    hash.update(cfg.target().to_string().as_bytes());
    hash.update(&policy.canonical_bytes());
    *hash.finalize().as_bytes()
}

fn invalid(message: &str) -> crate::Diagnostic {
    crate::Diagnostic::error(crate::DiagnosticCode::InvalidType, message, None)
}
