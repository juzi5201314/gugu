//! 实例 SCC 按凝聚图顺序完成；函数 query 只投影已完成结果，不递归请求半初始化摘要。

use super::callgraph;
use super::policy::AnalysisPolicyV1;
use super::solver::{self, SccMember};
use super::types::{AnalysisWorldV1, FunctionSummary, SccSummaryV1, WORLD_SCHEMA_VERSION};
use crate::SourceMap;
use crate::frontend::cfg::CfgContext;
use crate::frontend::gir::GirWorldV1;
use crate::frontend::hir::Module;
use crate::frontend::mono::{MonoWorldV1, digest_of};
use crate::query::{DependencyFingerprint, QueryEngine, QueryKey, QueryKind};

const SCC_SCHEMA: u32 = 4;
const FUNCTION_SCHEMA: u32 = 4;

struct InstancePlan {
    members: Vec<SccMember>,
    sccs: Vec<Vec<usize>>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "world 边界固定 HIR、GIR、实例图、策略与 query 来源"
)]
pub(crate) fn run_world(
    module: &Module,
    hir_fingerprint: [u8; 32],
    gir: &GirWorldV1,
    mono: &MonoWorldV1,
    cfg: &CfgContext,
    type_check_dependency: &DependencyFingerprint,
    lower_hir_dependency: &DependencyFingerprint,
    policy: AnalysisPolicyV1,
    queries: &QueryEngine,
    sources: &SourceMap,
) -> Result<(AnalysisWorldV1, DependencyFingerprint), Vec<crate::Diagnostic>> {
    let input_fingerprint = input_fingerprint(
        hir_fingerprint,
        gir.fingerprint,
        crate::frontend::mono::hash_domain(
            "gugu-analysis-late-input-v1",
            &[
                mono.graph_fingerprint.as_slice(),
                mono.universe.fingerprint.as_slice(),
                mono.late.fingerprint.as_slice(),
            ]
            .concat(),
        ),
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
    let result = queries
        .compute(key.clone(), |context| {
            context.record_dependency(
                type_check_dependency.key().clone(),
                type_check_dependency.fingerprint(),
            );
            context.record_dependency(
                lower_hir_dependency.key().clone(),
                lower_hir_dependency.fingerprint(),
            );
            context.record_dependency(
                QueryKey::new(QueryKind::FreezeTypeUniverse, 1, mono.graph_fingerprint),
                mono.universe.fingerprint,
            );
            let plan = plan_instances(module, mono)
                .map_err(|error| crate::frontend::semantics::query::store_errors(&[error]))?;
            let mut completed = vec![None; plan.members.len()];
            let mut parts = Vec::with_capacity(plan.sccs.len());
            for component in &plan.sccs {
                let (part, dependency) = scc_summary(
                    module,
                    gir,
                    &plan.members,
                    component,
                    input_fingerprint,
                    policy,
                    &completed,
                    queries,
                )?;
                context.record_dependency(dependency.key().clone(), dependency.fingerprint());
                for (&index, record) in component.iter().zip(&part.instances) {
                    let (summary, function_dependency) = function_summary(
                        &record.mono_key,
                        &record.summary,
                        &dependency,
                        input_fingerprint,
                        queries,
                    )?;
                    context.record_dependency(
                        function_dependency.key().clone(),
                        function_dependency.fingerprint(),
                    );
                    completed[index] = Some((summary, function_dependency));
                }
                parts.push(part);
            }
            let world = solver::world_from_sccs(parts, input_fingerprint);
            let bytes = serde_json::to_vec(&world).expect("分析 world 可序列化");
            fresh = Some(world);
            Ok((bytes, Vec::new()))
        })
        .map_err(|error| crate::frontend::semantics::query::restore_errors(error, sources))?;
    let dependency = DependencyFingerprint::new(key, result.fingerprint());
    let world = match fresh {
        Some(world) => world,
        None => serde_json::from_slice(result.payload())
            .map_err(|_| vec![invalid("WholeProgramAnalysis 缓存 schema 不合法")])?,
    };
    if world.schema != WORLD_SCHEMA_VERSION || world.input_fingerprint != input_fingerprint {
        return Err(vec![invalid("WholeProgramAnalysis 输入身份不匹配")]);
    }
    Ok((world, dependency))
}

fn plan_instances(module: &Module, mono: &MonoWorldV1) -> Result<InstancePlan, crate::Diagnostic> {
    let mut owner_of = vec![None; module.definitions.len()];
    for (index, owner) in module.owners.iter().enumerate() {
        owner_of[owner.definition.index()] = Some(index);
    }
    // 定义表按稳定 key 排序；实例表按 digest 排序，映射使用稠密实例下标。
    let digests: Vec<_> = mono
        .instances
        .iter()
        .map(|instance| digest_of(&instance.mono_key))
        .collect();
    let mut body_of_instance = vec![None; mono.instances.len()];
    let mut selected = Vec::new();
    for (index, instance) in mono.instances.iter().enumerate() {
        let key = instance
            .mono_key
            .get(..32)
            .ok_or_else(|| invalid("实例键缺少定义身份"))?;
        let definition = module
            .definitions
            .binary_search_by(|definition| definition.key.as_slice().cmp(key))
            .map_err(|_| invalid("闭合实例引用未知定义"))?;
        if let Some(owner) = owner_of[definition] {
            body_of_instance[index] = Some(selected.len());
            selected.push((instance, owner));
        } else if module.definitions[definition].kind
            != crate::frontend::hir::DefinitionKind::Constant
        {
            return Err(invalid("可执行实例缺少 HIR body"));
        }
    }
    let mut members = Vec::with_capacity(selected.len());
    let mut graph = Vec::with_capacity(selected.len());
    let target = |digest: &[u8; 32]| {
        digests
            .binary_search(digest)
            .map_err(|_| invalid("实例图含未闭合的 callee"))
    };
    for (instance, owner_index) in selected {
        let calls = instance
            .call_targets
            .iter()
            .map(|(site, digest)| {
                let index = target(digest)?;
                let body = body_of_instance[index]
                    .ok_or_else(|| invalid("调用位点指向无可执行 body 的常量"))?;
                Ok((*site, body))
            })
            .collect::<Result<_, crate::Diagnostic>>()?;
        let mut successors = Vec::with_capacity(instance.callees.len());
        for digest in &instance.callees {
            if let Some(body) = body_of_instance[target(digest)?] {
                successors.push(body);
            }
        }
        graph.push(successors);
        members.push(SccMember {
            mono_key: instance.mono_key.clone(),
            owner: callgraph::callable_key_at(module, owner_index),
            calls,
        });
    }
    Ok(InstancePlan {
        members,
        sccs: callgraph::strongly_connected_components(&graph),
    })
}

fn scc_summary(
    module: &Module,
    gir: &GirWorldV1,
    members: &[SccMember],
    component: &[usize],
    input: [u8; 32],
    policy: AnalysisPolicyV1,
    completed: &[Option<(FunctionSummary, DependencyFingerprint)>],
    queries: &QueryEngine,
) -> Result<(SccSummaryV1, DependencyFingerprint), crate::query::QueryError> {
    let mut bytes = input.to_vec();
    bytes.extend_from_slice(
        &u64::try_from(component.len())
            .expect("SCC 大小适配 GBC1")
            .to_le_bytes(),
    );
    for &index in component {
        bytes.extend_from_slice(&members[index].mono_key);
    }
    let key = QueryKey::new(QueryKind::AnalysisSccSummary, SCC_SCHEMA, bytes);
    let mut fresh = None;
    let result = queries.compute(key.clone(), |context| {
        for &index in component {
            for &(_, target) in &members[index].calls {
                if component.binary_search(&target).is_err() {
                    let (_, dependency) = completed[target].as_ref().expect("外部 callee 已完成");
                    context.record_dependency(dependency.key().clone(), dependency.fingerprint());
                }
            }
        }
        let summary = solver::analyze_scc(module, gir, members, component, policy, &|index| {
            completed[index]
                .as_ref()
                .expect("凝聚图拓扑保证外部 callee SCC 已完成")
                .0
                .clone()
        });
        let bytes = serde_json::to_vec(&summary).expect("SCC 摘要可序列化");
        fresh = Some(summary);
        Ok((bytes, Vec::new()))
    })?;
    let summary: SccSummaryV1 = match fresh {
        Some(summary) => summary,
        None => serde_json::from_slice(result.payload())
            .map_err(|_| crate::query::QueryError::Failed("SCC 摘要 schema 不合法".into()))?,
    };
    if summary.instances.len() != component.len()
        || summary
            .instances
            .iter()
            .zip(component)
            .any(|(record, &index)| record.mono_key != members[index].mono_key)
    {
        return Err(crate::query::QueryError::Failed(
            "SCC 缓存成员与实例图不一致".into(),
        ));
    }
    Ok((
        summary,
        DependencyFingerprint::new(key, result.fingerprint()),
    ))
}

fn function_summary(
    mono_key: &[u8],
    summary: &FunctionSummary,
    scc: &DependencyFingerprint,
    input: [u8; 32],
    queries: &QueryEngine,
) -> Result<(FunctionSummary, DependencyFingerprint), crate::query::QueryError> {
    let mut bytes = input.to_vec();
    bytes.extend_from_slice(mono_key);
    let key = QueryKey::new(QueryKind::FunctionAnalysisSummary, FUNCTION_SCHEMA, bytes);
    let result = queries.compute(key.clone(), |context| {
        context.record_dependency(scc.key().clone(), scc.fingerprint());
        Ok((
            serde_json::to_vec(summary).expect("函数摘要可序列化"),
            Vec::new(),
        ))
    })?;
    let summary = serde_json::from_slice(result.payload())
        .map_err(|_| crate::query::QueryError::Failed("函数摘要 schema 不合法".into()))?;
    Ok((
        summary,
        DependencyFingerprint::new(key, result.fingerprint()),
    ))
}

/// world 输入指纹：冻结前 HIR、实例图、两级 query 依赖与策略编码。
#[allow(clippy::too_many_arguments)]
pub(crate) fn input_fingerprint(
    hir_fingerprint: [u8; 32],
    gir_fingerprint: [u8; 32],
    graph_fingerprint: [u8; 32],
    cfg: &CfgContext,
    type_check_dependency: &DependencyFingerprint,
    lower_hir_dependency: &DependencyFingerprint,
    policy: AnalysisPolicyV1,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-abstract-analysis-input-v1");
    hash.update(&hir_fingerprint);
    hash.update(&gir_fingerprint);
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
