//! runtime raw 契约的 query driver：汇集各层需求、构建契约、注入 backend 片段并做跨层校验。
//!
//! 契约对象与 verifier 属于 runtime 层；本模块位于编排层，因为它同时需要冻结 HIR、具体
//! GIR 的布局校验（`frontend::contract`）与 backend 片段编码器。

use crate::backend::x64::fragments;
use crate::frontend::contract::{
    barrier_layout, channel_layout, coroutine_layout, local_heap_layout, mark_layout,
    shared_heap_layout, sync_layout,
};
use crate::frontend::{self, gc::GcMetadataBundle, gir::concrete::RecordTypes, hir};
use crate::lir;
use crate::query::{QueryEngine, QueryError, QueryKey, QueryKind, QueryResult};
use crate::runtime::{
    self, BarrierDemand, CompressionDemand, GcMetadataDemand, GcPacingDemand, LocalHeapDemand,
    MarkDemand, PlatformProfile, RAW_MODEL_SCHEMA, RawModelError, RawPlaneDemand, RawPlanePolicyV1,
    RawResourceDemand, Rt0Demand, RuntimeRawContractV1, RuntimeResources, SchedulerDemand,
    StackMapDemand, SyncDemand, WaitDemand,
};
use crate::{Diagnostic, DiagnosticCode, TargetName};

/// 从冻结前端、优化后 LIR 与 GC metadata 汇集各层需求，构建并校验 raw 契约。
///
/// 编译管线与内建函数表录制共用这一份需求口径：录制端的机器片段与用户编译现算的片段读同一份
/// 契约推导规则。
pub(crate) fn derive(
    target: TargetName,
    policy: RawPlanePolicyV1,
    frontend: &frontend::FrontendOutput,
    lir: &lir::Validated,
    gc_metadata: &GcMetadataBundle,
    resources: &RuntimeResources<'_>,
    queries: &QueryEngine,
) -> Result<RuntimeRawContractV1, Vec<Diagnostic>> {
    // LocalHeap 类型侧口径与 GC metadata 共用同一份冻结类型表：managed 类型数、超过单个
    // Immix block 的类型数与最大 payload 都由这些记录推导，避免两处口径漂移。
    let managed_type_count = frontend.mono.universe.records.len() as u32;
    let mut large_type_count = 0_u32;
    let mut max_object_bytes = 0_u64;
    for record in &frontend.mono.universe.records {
        let (size, _) = record.layout.unwrap_or((0, 1));
        max_object_bytes = max_object_bytes.max(size);
        if size + u64::from(runtime::local_heap_schema::HEAP_OBJECT_HEADER_BYTES)
            > u64::from(runtime::gc_metadata_contract::GC_BLOCK_BYTES)
        {
            large_type_count += 1;
        }
    }
    let coroutine_demand = lir.coroutine_demand();
    let shared_heap_demand = lir.shared_heap_demand(max_object_bytes);
    let demand = RawPlaneDemand {
        coroutine_sites: coroutine_demand.creation_sites,
        checked_entries: coroutine_demand.checked_entries,
        suspend_points: coroutine_demand.suspend_points,
        resource_sites: frontend.gir.placement.counts().resource,
        runtime_raw_sites: frontend.gir.placement.counts().runtime_raw,
        owners: 0,
        message_nodes: 0,
        turn_region: lir.turn_region_demand(),
        shared_heap: shared_heap_demand,
        foreign: foreign_demand(lir, &frontend.gir),
    };
    let module = frontend.hir.module();
    let main_returns_result = module
        .entry
        .and_then(|entry| {
            frontend
                .gir
                .bodies
                .iter()
                .find(|body| body.owner == entry)
                .map(|body| body.signature.result)
        })
        .and_then(|result| module.types.get(result.index()))
        .is_some_and(|result| matches!(result, hir::Type::Result(..)));
    let rt0_demand = Rt0Demand {
        entry_present: module.entry.is_some(),
        main_returns_result,
    };
    let action_counts = frontend.gir.resource_action_counts();
    let resource_demand = RawResourceDemand {
        resource_sites: frontend.gir.placement.counts().resource,
        acquire_sites: action_counts.0,
        release_sites: action_counts.1,
        transfer_sites: action_counts.2,
        finalize_sites: action_counts.3,
        owners: 0,
        kinds: 0,
    };
    let barrier_demand = lir.barrier_demand();
    let gc_demand = gc_metadata_demand(gc_metadata);
    let local_heap_demand =
        lir.local_heap_demand(managed_type_count, large_type_count, max_object_bytes);
    // mark 需求完全由已有三份 demand 推导，不新增 LIR 遍历，避免出现第二份站点口径；
    // 跨 owner ticket 的来源就是 SharedHeap 的共享字段屏障站点。
    let mark_demand = MarkDemand {
        root_sites: gc_demand.root_range_count,
        barrier_sites: barrier_demand.card_mark_sites,
        ticket_sites: shared_heap_demand.mark_sites,
        edge_delta_sites: barrier_demand.edge_summary_sites,
    };
    // 栈图与压缩引用来自同一次栈图世界推导：压缩根槽与解码点必须与同一份 LIR 对齐。
    let (stackmap_demand, compression_demand) = lir.stackmap_demands(module);
    run(
        RawModelInputs {
            target,
            policy,
            demand,
            resource_demand,
            rt0_demand,
            scheduler_demand: lir.scheduler_demand(),
            wait_demand: lir.wait_demand(),
            sync_demand: lir.sync_demand(),
            stackmap_demand,
            compression_demand,
            gc_metadata_demand: gc_demand,
            barrier_demand,
            pacing_demand: lir.pacing_demand(managed_type_count),
            mark_demand,
            local_heap_demand,
            gc_type_section: &gc_metadata.type_section,
            gc_metadata_section: &gc_metadata.metadata_section,
            profile: PlatformProfile::from(target),
            lir_fingerprint: lir.fingerprint(),
            placement_fingerprint: frontend.gir.placement.fingerprint,
            hir: module,
            gir: &frontend.mono.contracts,
            resources,
        },
        queries,
    )
}

/// 外调需求：LIR 的调用模式加上 GIR 里的 asm / global_asm 站点。
fn foreign_demand(lir: &lir::Validated, gir: &frontend::gir::GirWorldV1) -> runtime::ForeignDemand {
    let mut demand = lir.foreign_demand();
    for body in &gir.bodies {
        if body.kind == frontend::gir::body::BodyKind::GlobalAsm {
            demand.global_asm += 1;
        }
        for statement in &body.statements {
            if let frontend::gir::body::StatementKind::Assign(
                _,
                frontend::gir::body::Rvalue::Intrinsic {
                    op: frontend::gir::body::IntrinsicOp::Asm(_),
                    ..
                },
            ) = statement.kind
            {
                demand.asm += 1;
            }
        }
    }
    demand
}

/// 将真实 GC metadata bundle 的 section 大小写入 runtime demand。
fn gc_metadata_demand(bundle: &GcMetadataBundle) -> GcMetadataDemand {
    let mut demand = bundle.world.demand();
    demand.type_section_bytes = bundle.type_section.len() as u32;
    demand.metadata_section_bytes = bundle.metadata_section.len() as u32;
    demand
}

/// 契约 query 的输入集合。
struct RawModelInputs<'a> {
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
fn run(
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
