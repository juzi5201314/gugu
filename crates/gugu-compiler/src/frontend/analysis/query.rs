//! `WholeProgramAnalysis` 与嵌套的 SCC / 函数摘要 query。

use super::callgraph;
use super::policy::AnalysisPolicyV1;
use super::solver;
use super::types::{
    AnalysisOwnerKey, AnalysisWorldV1, FunctionSummary, SccSummaryV1, WORLD_SCHEMA_VERSION,
};
use crate::SourceMap;
use crate::frontend::cfg::CfgContext;
use crate::frontend::hir::Module;
use crate::query::{DependencyFingerprint, QueryEngine, QueryKey, QueryKind};

const SCC_SCHEMA: u32 = 1;
const FUNCTION_SCHEMA: u32 = 1;

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
    let key = QueryKey::new(
        QueryKind::WholeProgramAnalysis,
        WORLD_SCHEMA_VERSION,
        input_fingerprint,
    );
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
        let keys = callgraph::callable_keys(module);
        let (_graph, sccs) = callgraph::call_graph(module, &keys);
        let mut parts = Vec::new();
        for component in &sccs {
            let members: Vec<_> = component.iter().map(|&node| keys[node]).collect();
            let summary = scc_summary(
                module,
                &members,
                &keys,
                &sccs,
                pre_freeze_fingerprint,
                policy,
                queries,
                type_check_dependency,
                lower_hir_dependency,
            )?;
            parts.push(summary);
        }
        for key in &keys {
            let _ = function_summary(
                module,
                *key,
                &keys,
                &sccs,
                pre_freeze_fingerprint,
                policy,
                queries,
                type_check_dependency,
                lower_hir_dependency,
            )?;
        }
        let mut world = solver::world_from_sccs(parts, input_fingerprint);
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

fn scc_summary(
    module: &Module,
    members: &[AnalysisOwnerKey],
    all_keys: &[AnalysisOwnerKey],
    sccs: &[Vec<usize>],
    pre_freeze: [u8; 32],
    policy: AnalysisPolicyV1,
    queries: &QueryEngine,
    type_check_dependency: &DependencyFingerprint,
    lower_hir_dependency: &DependencyFingerprint,
) -> Result<SccSummaryV1, crate::query::QueryError> {
    let key = QueryKey::new(
        QueryKind::AnalysisSccSummary,
        SCC_SCHEMA,
        scc_fingerprint(members, pre_freeze, policy),
    );
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
        let in_scc: Vec<_> = members.iter().map(|item| item.definition).collect();
        let callees = |def: crate::frontend::hir::DefId| {
            if in_scc.contains(&def) {
                return FunctionSummary::default();
            }
            all_keys
                .iter()
                .find(|item| item.definition == def)
                .and_then(|item| {
                    function_summary(
                        module,
                        *item,
                        all_keys,
                        sccs,
                        pre_freeze,
                        policy,
                        queries,
                        type_check_dependency,
                        lower_hir_dependency,
                    )
                    .ok()
                })
                .unwrap_or_else(FunctionSummary::conservative)
        };
        let summary = solver::analyze_scc(module, members, policy, &callees);
        let bytes = serde_json::to_vec(&summary).expect("scc summary serializes");
        fresh = Some(summary);
        Ok((bytes, Vec::new()))
    })?;
    if let Some(summary) = fresh {
        return Ok(summary);
    }
    serde_json::from_slice(result.payload())
        .map_err(|_| crate::query::QueryError::Failed("AnalysisSccSummary schema".into()))
}

fn function_summary(
    module: &Module,
    owner: AnalysisOwnerKey,
    all_keys: &[AnalysisOwnerKey],
    sccs: &[Vec<usize>],
    pre_freeze: [u8; 32],
    policy: AnalysisPolicyV1,
    queries: &QueryEngine,
    type_check_dependency: &DependencyFingerprint,
    lower_hir_dependency: &DependencyFingerprint,
) -> Result<FunctionSummary, crate::query::QueryError> {
    let key = QueryKey::new(
        QueryKind::FunctionAnalysisSummary,
        FUNCTION_SCHEMA,
        function_fingerprint(owner, pre_freeze, policy),
    );
    let mut fresh = None;
    let result = queries.compute(key, |context| {
        context.record_dependency(
            type_check_dependency.key().clone(),
            type_check_dependency.fingerprint(),
        );
        let Some(node) = all_keys.iter().position(|item| *item == owner) else {
            return Err(crate::query::QueryError::Failed("missing owner".into()));
        };
        let Some(component) = sccs.iter().find(|component| component.contains(&node)) else {
            return Err(crate::query::QueryError::Failed("missing scc".into()));
        };
        let members: Vec<_> = component.iter().map(|&index| all_keys[index]).collect();
        let scc_key = QueryKey::new(
            QueryKind::AnalysisSccSummary,
            SCC_SCHEMA,
            scc_fingerprint(&members, pre_freeze, policy),
        );
        let summary = scc_summary(
            module,
            &members,
            all_keys,
            sccs,
            pre_freeze,
            policy,
            queries,
            type_check_dependency,
            lower_hir_dependency,
        )?;
        context.record_dependency(scc_key, [0; 32]);
        let projected = summary
            .owners
            .iter()
            .find(|record| record.key == owner)
            .map(|record| record.summary.clone())
            .unwrap_or_else(FunctionSummary::conservative);
        let bytes = serde_json::to_vec(&projected).expect("function summary serializes");
        fresh = Some(projected);
        Ok((bytes, Vec::new()))
    })?;
    if let Some(summary) = fresh {
        return Ok(summary);
    }
    serde_json::from_slice(result.payload())
        .map_err(|_| crate::query::QueryError::Failed("FunctionAnalysisSummary schema".into()))
}

fn scc_fingerprint(
    members: &[AnalysisOwnerKey],
    pre_freeze: [u8; 32],
    policy: AnalysisPolicyV1,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-analysis-scc-v1");
    hash.update(&pre_freeze);
    hash.update(&policy.canonical_bytes());
    for member in members {
        hash.update(&member.owner_index.to_le_bytes());
        hash.update(&member.definition.0.to_le_bytes());
    }
    *hash.finalize().as_bytes()
}

fn function_fingerprint(
    owner: AnalysisOwnerKey,
    pre_freeze: [u8; 32],
    policy: AnalysisPolicyV1,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-analysis-function-v1");
    hash.update(&pre_freeze);
    hash.update(&policy.canonical_bytes());
    hash.update(&owner.owner_index.to_le_bytes());
    hash.update(&owner.definition.0.to_le_bytes());
    *hash.finalize().as_bytes()
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
