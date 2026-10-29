//! runtime raw 契约的 query driver：汇集各层需求、构建契约、注入 backend 片段并做跨层校验。
//!
//! 契约对象与 verifier 属于 runtime 层；本模块位于编排层，因为它同时需要冻结 HIR、具体
//! GIR 的布局校验（`frontend::contract`）与 backend 片段编码器。

use crate::backend::x64::fragments;
use crate::frontend::contract::{
    barrier_layout, channel_layout, coroutine_layout, local_heap_layout, mark_layout,
    shared_heap_layout, sync_layout,
};
use crate::frontend::{gir::concrete::RecordTypes, hir};
use crate::query::{QueryEngine, QueryError, QueryKey, QueryKind, QueryResult};
use crate::runtime::{
    BarrierDemand, CompressionDemand, GcMetadataDemand, GcPacingDemand, LocalHeapDemand,
    MarkDemand, PlatformProfile, RAW_MODEL_SCHEMA, RawModelError, RawPlaneDemand, RawPlanePolicyV1,
    RawResourceDemand, Rt0Demand, RuntimeRawContractV1, RuntimeResources, SchedulerDemand,
    StackMapDemand, SyncDemand, WaitDemand,
};
use crate::{Diagnostic, DiagnosticCode, TargetName};

/// 契约 query 的输入集合。
pub(crate) struct RawModelInputs<'a> {
    pub(crate) target: TargetName,
    pub(crate) policy: RawPlanePolicyV1,
    /// 平台 profile；决定平台常量、失败映射与 extent 阶梯的端点。
    pub(crate) profile: PlatformProfile,
    pub(crate) demand: RawPlaneDemand,
    pub(crate) resource_demand: RawResourceDemand,
    /// rt0 启动需求视图：入口存在性与 main 返回类型。
    pub(crate) rt0_demand: Rt0Demand,
    pub(crate) scheduler_demand: SchedulerDemand,
    /// 等待源需求视图：channel / Join / select 调用计数。
    pub(crate) wait_demand: WaitDemand,
    /// 同步需求视图：atomic / mutex / rwlock / condvar / once / cancel 操作计数。
    pub(crate) sync_demand: SyncDemand,
    /// 栈图需求视图：逻辑函数、安全点、kind 分类与根字数。
    pub(crate) stackmap_demand: StackMapDemand,
    /// GC metadata 需求视图：类型表大小、trace/value program 字节数与 arena 布局。
    pub(crate) gc_metadata_demand: GcMetadataDemand,
    /// barrier 需求视图：permit 额度、reserved/bare 屏障与 edge summary 站点。
    pub(crate) barrier_demand: BarrierDemand,
    /// pacing 需求视图：分配站点、屏障站点、assist slow edge 与受管类型数。
    pub(crate) pacing_demand: GcPacingDemand,
    /// mark 需求视图：根站点、屏障站点、shared 站点与 edge delta 站点。
    pub(crate) mark_demand: MarkDemand,
    /// LocalHeap 需求视图：placement 站点、类型 footprint 与屏障站点。
    pub(crate) local_heap_demand: LocalHeapDemand,
    /// 压缩需求视图：解码点与压缩根槽上界。
    pub(crate) compression_demand: CompressionDemand,
    /// 已由 frontend 编码的真实 type/meta section。
    pub(crate) gc_type_section: &'a [u8],
    pub(crate) gc_metadata_section: &'a [u8],
    /// 生成契约所依据的 LIR 输入指纹。
    pub(crate) lir_fingerprint: [u8; 32],
    /// placement world 指纹。
    pub(crate) placement_fingerprint: [u8; 32],
    pub(crate) hir: &'a hir::Module,
    pub(crate) gir: &'a [RecordTypes],
    pub(crate) resources: &'a RuntimeResources<'a>,
}

/// 通过 query 构建并校验契约对象；失败进入 `E0058`。
pub(crate) fn run(
    inputs: RawModelInputs<'_>,
    queries: &QueryEngine,
) -> Result<RuntimeRawContractV1, Vec<Diagnostic>> {
    let mut key_bytes = crate::canonical::to_vec(&(
        inputs.policy,
        inputs.demand,
        inputs.resource_demand,
        inputs.rt0_demand,
        inputs.scheduler_demand,
        inputs.wait_demand,
        inputs.sync_demand,
        inputs.stackmap_demand,
        inputs.gc_metadata_demand,
        inputs.barrier_demand,
        inputs.pacing_demand,
        inputs.mark_demand,
        inputs.local_heap_demand,
        inputs.compression_demand,
        inputs.gc_type_section,
        inputs.gc_metadata_section,
    ))
    .expect("runtime需求与策略可序列化");
    key_bytes.extend_from_slice(&inputs.lir_fingerprint);
    key_bytes.extend_from_slice(&inputs.placement_fingerprint);
    key_bytes.extend_from_slice(&crate::canonical::to_vec(inputs.gir).expect("契约类型目录编码"));
    key_bytes.extend_from_slice(inputs.target.to_string().as_bytes());
    key_bytes.extend_from_slice(inputs.profile.name().as_bytes());
    let key = QueryKey::new(QueryKind::RuntimeRawModel, RAW_MODEL_SCHEMA, &key_bytes);
    let mut fresh = None;
    let result: QueryResult = queries
        .compute(key, |context| {
            context.record_dependency(
                QueryKey::new(
                    QueryKind::BuildLir,
                    crate::lir::SCHEMA,
                    inputs.lir_fingerprint,
                ),
                inputs.lir_fingerprint,
            );
            context.record_dependency(
                QueryKey::new(
                    QueryKind::EscapeAndPlacement,
                    1,
                    inputs.placement_fingerprint,
                ),
                inputs.placement_fingerprint,
            );
            let contract = RuntimeRawContractV1::build(
                inputs.target,
                inputs.policy,
                inputs.demand,
                inputs.resource_demand,
                inputs.rt0_demand,
                inputs.scheduler_demand,
                inputs.wait_demand,
                inputs.sync_demand,
                inputs.stackmap_demand,
                inputs.gc_metadata_demand,
                inputs.barrier_demand,
                inputs.pacing_demand,
                inputs.mark_demand,
                inputs.local_heap_demand,
                inputs.compression_demand,
                inputs.profile,
            )
            .and_then(|contract| {
                contract.with_gc_sections(
                    inputs.gc_type_section.to_vec(),
                    inputs.gc_metadata_section.to_vec(),
                )
            })
            .map(|contract| {
                contract.with_coroutine_code(
                    fragments::morestack_entry_code(),
                    fragments::main_return_code(),
                )
            })
            .map_err(|error| QueryError::Failed(error.message().to_owned()))?;
            verify_cross_layer(&contract, &inputs)
                .map_err(|error| QueryError::Failed(error.message().to_owned()))?;
            let bytes = crate::canonical::to_vec(&contract).expect("runtime raw 契约编码");
            fresh = Some(contract);
            Ok((bytes, Vec::new()))
        })
        .map_err(|error| {
            vec![Diagnostic::error(
                DiagnosticCode::RuntimeRawInvariant,
                error.to_string(),
                None,
            )]
        })?;
    if let Some(contract) = fresh {
        return Ok(contract);
    }
    let contract: RuntimeRawContractV1 =
        crate::canonical::from_slice(result.payload()).map_err(|_| {
            vec![RawModelError::new("缓存的 runtime raw 契约不是合法 schema").diagnostic()]
        })?;
    if contract.target_semantics() != inputs.target.name() || contract.policy() != &inputs.policy {
        return Err(vec![
            RawModelError::new("缓存 runtime raw 契约与当前目标或策略不一致").diagnostic(),
        ]);
    }
    contract
        .verify()
        .map_err(|error| vec![error.diagnostic()])?;
    verify_cross_layer(&contract, &inputs).map_err(|error| vec![error.diagnostic()])?;
    Ok(contract)
}

/// runtime 层 verifier 之外的跨层校验：backend 片段逐字节比对与源码控制块布局对照。
fn verify_cross_layer(
    contract: &RuntimeRawContractV1,
    inputs: &RawModelInputs<'_>,
) -> Result<(), RawModelError> {
    fragments::verify_coroutine_code(contract.coroutine())?;
    coroutine_layout::verify_source(
        contract.coroutine(),
        inputs.hir,
        inputs.gir,
        inputs.resources,
    )?;
    channel_layout::verify_source(contract.wait(), inputs.hir, inputs.gir)?;
    sync_layout::verify_source(contract.sync(), inputs.hir, inputs.gir)?;
    barrier_layout::verify_source(contract.barrier(), inputs.hir, inputs.gir)?;
    local_heap_layout::verify_source(contract.local_heap(), inputs.hir, inputs.gir)?;
    shared_heap_layout::verify_source(contract.shared_heap(), inputs.hir, inputs.gir)?;
    mark_layout::verify_source(contract.mark(), inputs.hir, inputs.gir)
}
