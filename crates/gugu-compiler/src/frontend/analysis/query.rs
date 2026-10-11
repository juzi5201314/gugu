//! 实例 SCC 按凝聚图顺序完成；函数 query 只投影已完成结果，不递归请求半初始化摘要。
//!
//! 三层 payload 共用 `encode.rs` 的二进制规范编码：SCC 层编码一次实例记录，
//! 函数层 payload 直接取记录内的摘要字节区间（零拷贝切片），world 层复制记录
//! 字节并只重编 proof 表。session 内新鲜路径持有类型化对象，不做解码往返。

use super::callgraph;
use super::encode;
use super::policy::AnalysisPolicyV1;
use super::solver::{self, SccMember};
use super::types::{AnalysisWorldV1, FunctionSummary, SccSummaryV1, WORLD_SCHEMA_VERSION};
use crate::SourceMap;
use crate::frontend::cfg::CfgContext;
use crate::frontend::gir::GirWorldV1;
use crate::frontend::hir::Module;
use crate::frontend::mono::{MonoWorldV1, digest_of};
use crate::query::{DependencyFingerprint, QueryEngine, QueryKey, QueryKind};

const SCC_SCHEMA: u32 = 5;
const FUNCTION_SCHEMA: u32 = 5;

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
            let mut summaries = Vec::with_capacity(plan.sccs.len());
            let mut parts = Vec::with_capacity(plan.sccs.len());
            for component in &plan.sccs {
                let (summary, encoded, dependency) = scc_summary(
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
                for (local, (&index, record)) in
                    component.iter().zip(&summary.instances).enumerate()
                {
                    let summary_range = encoded.records[local].1.clone();
                    let (summary, function_dependency) = function_summary(
                        &record.mono_key,
                        &record.summary,
                        summary_range,
                        &encoded.payload,
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
                summaries.push(summary);
                parts.push(encoded);
            }
            let world = solver::world_from_sccs(summaries, input_fingerprint);
            let bytes = encode::encode_world(&world, &parts);
            fresh = Some(world);
            Ok((bytes, Vec::new()))
        })
        .map_err(|error| crate::frontend::semantics::query::restore_errors(error, sources))?;
    let dependency = DependencyFingerprint::new(key, result.fingerprint());
    let world = match fresh {
        Some(world) => world,
        None => encode::decode_world(&result.payload_shared())
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
    let mut definition_of = vec![None; module.definitions.len()];
    for (index, definition) in module.definitions.iter().enumerate() {
        definition_of[index] = Some(definition.key);
    }
    debug_assert!(module.owners.iter().all(|owner| {
        owner_of[owner.definition.index()]
            .is_some_and(|index| module.owners[index].definition == owner.definition)
    }));
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
        debug_assert!(definition_of[definition].is_some());
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

#[expect(
    clippy::too_many_arguments,
    reason = "SCC 摘要需要 world、成员、依赖与策略一同参与指纹"
)]
fn scc_summary(
    module: &Module,
    gir: &GirWorldV1,
    members: &[SccMember],
    component: &[usize],
    input: [u8; 32],
    policy: AnalysisPolicyV1,
    completed: &[Option<(FunctionSummary, DependencyFingerprint)>],
    queries: &QueryEngine,
) -> Result<(SccSummaryV1, encode::EncodedScc, DependencyFingerprint), crate::query::QueryError> {
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
        let encoded = encode::encode_scc(&summary);
        let payload = encoded.payload.clone();
        fresh = Some((summary, encoded));
        Ok((payload, Vec::new()))
    })?;
    let (summary, encoded) = match fresh {
        Some(pair) => pair,
        None => encode::decode_scc(&result.payload_shared())
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
        encoded,
        DependencyFingerprint::new(key, result.fingerprint()),
    ))
}

fn function_summary(
    mono_key: &[u8],
    summary: &FunctionSummary,
    summary_range: std::ops::Range<u32>,
    scc_payload: &bytes::Bytes,
    scc: &DependencyFingerprint,
    input: [u8; 32],
    queries: &QueryEngine,
) -> Result<(FunctionSummary, DependencyFingerprint), crate::query::QueryError> {
    let mut bytes = input.to_vec();
    bytes.extend_from_slice(mono_key);
    let key = QueryKey::new(QueryKind::FunctionAnalysisSummary, FUNCTION_SCHEMA, bytes);
    let mut fresh = None;
    let result = queries.compute(key.clone(), |context| {
        context.record_dependency(scc.key().clone(), scc.fingerprint());
        fresh = Some(summary.clone());
        // 函数 payload 直接共享 SCC payload 的摘要区间，不再重编码。
        Ok((
            scc_payload.slice(
                usize::try_from(summary_range.start).expect("区间起点适配宿主")
                    ..usize::try_from(summary_range.end).expect("区间终点适配宿主"),
            ),
            Vec::new(),
        ))
    })?;
    let summary = match fresh {
        Some(summary) => summary,
        None => encode::decode_function_summary(&mut encode::Decoder::new(result.payload()))
            .map_err(|_| crate::query::QueryError::Failed("函数摘要 schema 不合法".into()))?,
    };
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
