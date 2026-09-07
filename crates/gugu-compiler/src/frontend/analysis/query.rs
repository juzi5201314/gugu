//! `WholeProgramAnalysis` 与嵌套的 SCC / 函数摘要 query。
//!
//! 阶段 24 起：SCC（27）与函数摘要（23）以 `MonoKey` 为身份键，成员来自闭世界
//! 实例图；world（24）的输入指纹包含实例图指纹。求解仍在定义级 HIR owner 上
//! 进行，每个实例投影其定义的摘要。

use super::callgraph;
use super::policy::AnalysisPolicyV1;
use super::solver::{self, SccMember};
use super::types::{AnalysisWorldV1, FunctionSummary, SccSummaryV1, WORLD_SCHEMA_VERSION};
use crate::SourceMap;
use crate::frontend::cfg::CfgContext;
use crate::frontend::hir::{self, Module};
use crate::frontend::mono::MonoWorldV1;
use crate::query::{DependencyFingerprint, QueryEngine, QueryKey, QueryKind};
use std::collections::BTreeMap;

const SCC_SCHEMA: u32 = 2;
const FUNCTION_SCHEMA: u32 = 2;

/// 实例到定义级 owner 的求解计划。
struct InstancePlan {
    members: Vec<SccMember>,
    sccs: Vec<Vec<usize>>,
    /// 定义 -> 首个实例下标（跨 SCC callee 投影用）。
    first_instance: BTreeMap<hir::DefId, usize>,
}

/// 运行 whole-program 分析并按输入指纹缓存；`pre_freeze_fingerprint` 是
/// proof 写回前的 HIR 模块指纹，实例图指纹作为闭世界输入身份的一部分。
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_world(
    module: &Module,
    pre_freeze_fingerprint: [u8; 32],
    mono: &MonoWorldV1,
    cfg: &CfgContext,
    type_check_dependency: &DependencyFingerprint,
    lower_hir_dependency: &DependencyFingerprint,
    policy: AnalysisPolicyV1,
    queries: &QueryEngine,
    sources: &SourceMap,
) -> Result<AnalysisWorldV1, Vec<crate::Diagnostic>> {
    let input_fingerprint = input_fingerprint(
        pre_freeze_fingerprint,
        mono.graph_fingerprint,
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
        let plan = plan_instances(module, mono)
            .map_err(|error| crate::frontend::semantics::query::store_errors(&[error]))?;
        let mut parts = Vec::new();
        for component in &plan.sccs {
            let members: Vec<SccMember> = component
                .iter()
                .map(|&index| plan.members[index].clone())
                .collect();
            parts.push(scc_summary(
                module,
                mono,
                &plan,
                &members,
                pre_freeze_fingerprint,
                policy,
                queries,
                type_check_dependency,
            )?);
        }
        for member in &plan.members {
            let _ = function_summary(
                module,
                mono,
                &plan,
                member,
                pre_freeze_fingerprint,
                policy,
                queries,
                type_check_dependency,
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

/// 从实例图构造求解计划：成员映射、实例 SCC 与定义首个实例。
fn plan_instances(module: &Module, mono: &MonoWorldV1) -> Result<InstancePlan, crate::Diagnostic> {
    let mut members = Vec::with_capacity(mono.instances.len());
    let mut first_instance: BTreeMap<hir::DefId, usize> = BTreeMap::new();
    for instance in &mono.instances {
        let definition_key: [u8; 32] = instance.mono_key[..32]
            .try_into()
            .map_err(|_| invalid("实例键缺少定义稳定键"))?;
        let Some(definition) = definition_by_key(module, &definition_key) else {
            return Err(invalid("实例定义不在定义表内"));
        };
        // 无 owner 的实例（静态初始化器、global asm、无体定义）没有分析成员。
        let Some(owner_index) = module
            .owners
            .iter()
            .position(|owner| owner.definition == definition)
        else {
            continue;
        };
        let owner = callgraph::callable_key_at(module, owner_index);
        first_instance.entry(definition).or_insert(members.len());
        members.push(SccMember {
            mono_key: instance.mono_key.clone(),
            owner,
        });
    }
    let mut digests: Vec<[u8; 32]> = members
        .iter()
        .map(|member| super::super::mono::digest_of(&member.mono_key))
        .collect();
    digests.sort();
    let mut graph = vec![Vec::new(); members.len()];
    for (index, member) in members.iter().enumerate() {
        let Some(instance) = mono
            .instances
            .iter()
            .find(|instance| instance.mono_key == member.mono_key)
        else {
            continue;
        };
        for callee in &instance.callees {
            if let Ok(target) = digests.binary_search(callee) {
                graph[index].push(target);
            }
        }
    }
    let sccs = callgraph::strongly_connected_components(&graph);
    Ok(InstancePlan {
        members,
        sccs,
        first_instance,
    })
}

fn definition_by_key(module: &Module, key: &[u8; 32]) -> Option<hir::DefId> {
    module
        .definitions
        .binary_search_by(|definition| definition.key.cmp(key))
        .ok()
        .map(|index| hir::DefId(index as u32))
}

#[allow(clippy::too_many_arguments)]
fn scc_summary(
    module: &Module,
    mono: &MonoWorldV1,
    plan: &InstancePlan,
    members: &[SccMember],
    pre_freeze: [u8; 32],
    policy: AnalysisPolicyV1,
    queries: &QueryEngine,
    type_check_dependency: &DependencyFingerprint,
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
        let summary = solver::analyze_scc(module, members, policy, &|def| {
            resolve_callee_summary(
                module,
                mono,
                plan,
                def,
                pre_freeze,
                policy,
                queries,
                type_check_dependency,
            )
            .unwrap_or_else(FunctionSummary::conservative)
        });
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

#[allow(clippy::too_many_arguments)]
fn function_summary(
    module: &Module,
    mono: &MonoWorldV1,
    plan: &InstancePlan,
    member: &SccMember,
    pre_freeze: [u8; 32],
    policy: AnalysisPolicyV1,
    queries: &QueryEngine,
    type_check_dependency: &DependencyFingerprint,
) -> Result<FunctionSummary, crate::query::QueryError> {
    let key = QueryKey::new(
        QueryKind::FunctionAnalysisSummary,
        FUNCTION_SCHEMA,
        function_fingerprint(member, pre_freeze, policy),
    );
    let mut fresh = None;
    let result = queries.compute(key, |context| {
        context.record_dependency(
            type_check_dependency.key().clone(),
            type_check_dependency.fingerprint(),
        );
        let Some(component) = plan.sccs.iter().find(|component| {
            component
                .iter()
                .any(|&index| plan.members[index].mono_key == member.mono_key)
        }) else {
            return Err(crate::query::QueryError::Failed("missing scc".into()));
        };
        let members: Vec<SccMember> = component
            .iter()
            .map(|&index| plan.members[index].clone())
            .collect();
        let summary = scc_summary(
            module,
            mono,
            plan,
            &members,
            pre_freeze,
            policy,
            queries,
            type_check_dependency,
        )?;
        let projected = summary
            .instances
            .iter()
            .find(|record| record.mono_key == member.mono_key)
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

/// 跨 SCC callee：按定义找到首个实例并投影其摘要。
#[allow(clippy::too_many_arguments)]
fn resolve_callee_summary(
    module: &Module,
    mono: &MonoWorldV1,
    plan: &InstancePlan,
    def: hir::DefId,
    pre_freeze: [u8; 32],
    policy: AnalysisPolicyV1,
    queries: &QueryEngine,
    type_check_dependency: &DependencyFingerprint,
) -> Option<FunctionSummary> {
    let index = *plan.first_instance.get(&def)?;
    let member = &plan.members[index];
    function_summary(
        module,
        mono,
        plan,
        member,
        pre_freeze,
        policy,
        queries,
        type_check_dependency,
    )
    .ok()
}

fn scc_fingerprint(
    members: &[SccMember],
    pre_freeze: [u8; 32],
    policy: AnalysisPolicyV1,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-analysis-scc-v2");
    hash.update(&pre_freeze);
    hash.update(&policy.canonical_bytes());
    for member in members {
        hash.update(&member.mono_key);
    }
    *hash.finalize().as_bytes()
}

fn function_fingerprint(
    member: &SccMember,
    pre_freeze: [u8; 32],
    policy: AnalysisPolicyV1,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-analysis-function-v2");
    hash.update(&pre_freeze);
    hash.update(&policy.canonical_bytes());
    hash.update(&member.mono_key);
    *hash.finalize().as_bytes()
}

/// world 输入指纹：冻结前 HIR、实例图、两级 query 依赖与策略编码。
#[allow(clippy::too_many_arguments)]
pub(crate) fn input_fingerprint(
    pre_freeze_fingerprint: [u8; 32],
    graph_fingerprint: [u8; 32],
    cfg: &CfgContext,
    type_check_dependency: &DependencyFingerprint,
    lower_hir_dependency: &DependencyFingerprint,
    policy: AnalysisPolicyV1,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-abstract-analysis-input-v1");
    hash.update(&pre_freeze_fingerprint);
    hash.update(&graph_fingerprint);
    hash.update(&type_check_dependency.fingerprint());
    hash.update(&lower_hir_dependency.fingerprint());
    hash.update(cfg.target().to_string().as_bytes());
    hash.update(&policy.canonical_bytes());
    *hash.finalize().as_bytes()
}

fn invalid(message: &str) -> crate::Diagnostic {
    crate::Diagnostic::error(crate::DiagnosticCode::InvalidType, message, None)
}
