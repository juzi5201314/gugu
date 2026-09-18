//! LIR 是 backend 的唯一低层输入；构造与缓存恢复共享结构 verifier。
pub(crate) mod body;
mod build;
mod dump;
mod effects;
mod pass;
pub(crate) mod stackmap;
mod uses;
mod verify;

use crate::{
    Diagnostic, DiagnosticCode,
    frontend::{gir, hir, mono},
    query::{QueryEngine, QueryKey, QueryKind},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub(crate) const SCHEMA: u32 = 3;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct World {
    schema: u32,
    target: String,
    input_fingerprint: [u8; 32],
    bodies: Vec<body::Body>,
    fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Validated {
    world: Arc<World>,
}

impl Validated {
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.world.fingerprint
    }
    pub(crate) fn bodies(&self) -> usize {
        self.world.bodies.len()
    }
    pub(crate) fn blocks(&self) -> usize {
        self.world.bodies.iter().map(|body| body.blocks.len()).sum()
    }
    pub(crate) fn instructions(&self) -> usize {
        self.world
            .bodies
            .iter()
            .map(|body| body.instructions.len())
            .sum()
    }
    pub(crate) fn memory_operations(&self) -> usize {
        self.world
            .bodies
            .iter()
            .map(|body| {
                body.instructions
                    .iter()
                    .filter(|instruction| instruction.memory.is_some())
                    .count()
                    + body
                        .blocks
                        .iter()
                        .filter(|block| matches!(block.terminator, body::Terminator::Invoke { .. }))
                        .count()
            })
            .sum()
    }
    pub(crate) fn safepoints(&self) -> usize {
        self.world
            .bodies
            .iter()
            .map(|body| body.safepoints.len())
            .sum()
    }
    /// runtime只消费优化后仍存在的创建、入口检查与suspend边界。
    pub(crate) fn coroutine_demand(&self) -> crate::runtime::CoroutineDemand {
        let mut demand = crate::runtime::CoroutineDemand::default();
        for body in &self.world.bodies {
            demand.checked_entries += u32::from(body.poll_summary.entry_stack_check);
            let is_spawn = |call: &body::Call| {
                matches!(
                    call.target,
                    body::CallTarget::Runtime(body::RuntimeCall::Spawn)
                )
            };
            for instruction in &body.instructions {
                if let body::Op::Call(call) = &instruction.op {
                    demand.creation_sites += u32::from(is_spawn(call));
                }
            }
            for block in &body.blocks {
                if let body::Terminator::Invoke { call, .. }
                | body::Terminator::TailCall { call, .. } = &block.terminator
                {
                    demand.creation_sites += u32::from(is_spawn(call));
                }
            }
            demand.suspend_points += u32::try_from(
                body.safepoints
                    .iter()
                    .filter(|point| point.kind == body::SafepointKind::Suspend)
                    .count(),
            )
            .expect("suspend数量适配u32");
        }
        demand
    }
    /// TurnRegion 需求：来自优化后 LIR 的 region 指令，而不是构造期的 placement 计划。
    ///
    /// region 编号只在单个 body 内稠密，因此 region 数量按 `(body, region)` 去重；容量 class
    /// 由 `RegionAlloc` 的字节实参落在哪一档阶梯决定，`max_region_bytes` 是单个 region 的
    /// payload 上界。
    pub(crate) fn turn_region_demand(&self) -> crate::runtime::region_schema::TurnRegionDemand {
        use crate::runtime::region_schema::{REGION_CAPACITY_CLASSES, TurnRegionDemand};
        let mut demand = TurnRegionDemand::default();
        let mut classes = 0_u32;
        for world_body in &self.world.bodies {
            let mut per_region: Vec<u64> = Vec::new();
            for instruction in &world_body.instructions {
                match &instruction.op {
                    body::Op::RegionAlloc { region, .. } => {
                        demand.allocations += 1;
                        let bytes = world_body
                            .args(&instruction.arguments)
                            .first()
                            .map(|value| const_bytes(world_body, *value))
                            .unwrap_or(0);
                        if let Some(index) = REGION_CAPACITY_CLASSES
                            .iter()
                            .position(|class| u64::from(*class) >= bytes)
                        {
                            classes |= 1 << index;
                        }
                        let region = *region as usize;
                        if per_region.len() <= region {
                            per_region.resize(region + 1, 0);
                        }
                        per_region[region] += bytes;
                    }
                    body::Op::RegionPublish { .. } => demand.publish_sites += 1,
                    body::Op::RegionReset { .. } => demand.reset_sites += 1,
                    body::Op::PromoteManaged { .. } => demand.promote_sites += 1,
                    body::Op::RegionTransfer { .. } => demand.transfer_sites += 1,
                    _ => {}
                }
            }
            per_region.retain(|bytes| *bytes != 0);
            demand.regions += u32::try_from(per_region.len()).expect("region数量适配u32");
            for bytes in per_region {
                demand.total_bytes += bytes;
                demand.max_region_bytes = demand.max_region_bytes.max(bytes);
            }
        }
        demand.capacity_classes = classes.count_ones();
        demand
    }

    /// 调度需求：创建点与挂起点复用协程口径，yield 点统计全 body 的 `RuntimeCall::Yield`。
    pub(crate) fn scheduler_demand(&self) -> crate::runtime::SchedulerDemand {
        let coroutine = self.coroutine_demand();
        let mut yield_sites = 0_u32;
        let is_yield = |call: &body::Call| {
            matches!(
                call.target,
                body::CallTarget::Runtime(body::RuntimeCall::Yield)
            )
        };
        for world_body in &self.world.bodies {
            for instruction in &world_body.instructions {
                match &instruction.op {
                    body::Op::Call(call) | body::Op::ForeignCall(call) => {
                        yield_sites += u32::from(is_yield(call));
                    }
                    _ => {}
                }
            }
            for block in &world_body.blocks {
                if let body::Terminator::Invoke { call, .. }
                | body::Terminator::TailCall { call, .. } = &block.terminator
                {
                    yield_sites += u32::from(is_yield(call));
                }
            }
        }
        crate::runtime::SchedulerDemand {
            spawn_sites: coroutine.creation_sites,
            yield_sites,
            suspend_points: coroutine.suspend_points,
        }
    }
    /// 等待源需求：从优化后 LIR 统计 channel / Join / select 调用与 Select safepoint。
    pub(crate) fn wait_demand(&self) -> crate::runtime::WaitDemand {
        let mut demand = crate::runtime::WaitDemand::default();
        let visit = |demand: &mut crate::runtime::WaitDemand, call: &body::Call| {
            let body::CallTarget::Runtime(target) = call.target else {
                return;
            };
            match target {
                body::RuntimeCall::ChannelNew => demand.channel_new += 1,
                body::RuntimeCall::ChannelClose => demand.channel_close += 1,
                body::RuntimeCall::ChannelSend => demand.channel_send += 1,
                body::RuntimeCall::ChannelReceive => demand.channel_receive += 1,
                body::RuntimeCall::ChannelTrySend => demand.channel_try_send += 1,
                body::RuntimeCall::ChannelTryRecv => demand.channel_try_recv += 1,
                body::RuntimeCall::JoinWait => demand.join_wait += 1,
                body::RuntimeCall::SelectCommit { cases, has_default } => {
                    demand.select_commit += 1;
                    if cases == 0 && !has_default {
                        demand.never_select += 1;
                    }
                }
                _ => {}
            }
        };
        for world_body in &self.world.bodies {
            for instruction in &world_body.instructions {
                if let body::Op::Call(call) | body::Op::ForeignCall(call) = &instruction.op {
                    visit(&mut demand, call);
                }
            }
            for block in &world_body.blocks {
                if let body::Terminator::Invoke { call, .. }
                | body::Terminator::TailCall { call, .. } = &block.terminator
                {
                    visit(&mut demand, call);
                }
            }
            demand.select_safepoints += u32::try_from(
                world_body
                    .safepoints
                    .iter()
                    .filter(|point| point.kind == body::SafepointKind::Select)
                    .count(),
            )
            .expect("select safepoint 数量适配 u32");
        }
        demand
    }
    /// barrier 需求：从优化后 LIR 的 permit、region 与独占 store 推导。
    ///
    /// permit 额度已经在 pass 中按 region 静态复算并由 verifier 强制，这里只做汇总；
    /// edge summary 站点按「托管 store」计数，card-mark 站点按 barrier 条数计数。
    pub(crate) fn barrier_demand(&self) -> crate::runtime::BarrierDemand {
        let mut demand = crate::runtime::BarrierDemand::default();
        for world_body in &self.world.bodies {
            demand.regions += world_body.no_safepoint_regions.len() as u32;
            demand.permits += world_body.barrier_permits.len() as u32;
            for permit in &world_body.barrier_permits {
                demand.max_shades_permit = demand.max_shades_permit.max(permit.max_shades);
                demand.max_card_marks_permit =
                    demand.max_card_marks_permit.max(permit.max_card_marks);
                demand.shade_slots = demand.shade_slots.saturating_add(permit.max_shades);
                demand.card_mark_slots =
                    demand.card_mark_slots.saturating_add(permit.max_card_marks);
            }
            for instruction in &world_body.instructions {
                match instruction.op {
                    body::Op::GcWriteBarrier { .. } => demand.bare_barriers += 1,
                    body::Op::GcWriteBarrierReserved { .. } => demand.reserved_barriers += 1,
                    body::Op::SharedFieldBarrier { .. }
                    | body::Op::SharedFieldBarrierReserved { .. } => {
                        demand.shared_field_sites += 1;
                    }
                    _ => {}
                }
            }
        }
        // card 键与 edge delta 都只可能由 managed store 的 barrier 产生，因此两个站点视图
        // 覆盖同一个站点集合：前者统计可能记账的写入，后者统计可能贡献边增删的写入。
        demand.card_mark_sites = demand.barrier_sites();
        demand.edge_summary_sites = demand.barrier_sites();
        demand
    }

    /// pacing 需求：从优化后 LIR 与冻结类型表推导 debt、assist 与 trace 的口径上界。
    ///
    /// 分配站点覆盖 `GcAlloc`、`RegionAlloc` 与 `PromoteManaged`：三者都会推进 cycle 内的
    /// allocation debt。slow edge 是「允许执行一次 assist」的边界，因此等于分配站点加显式
    /// `SafepointPoll`；barrier 站点复用同一站点集合，managed_types 由冻结类型表给出。
    pub(crate) fn pacing_demand(&self, managed_types: u32) -> crate::runtime::GcPacingDemand {
        let mut demand = crate::runtime::GcPacingDemand {
            managed_types,
            ..crate::runtime::GcPacingDemand::default()
        };
        for world_body in &self.world.bodies {
            for instruction in &world_body.instructions {
                match instruction.op {
                    body::Op::GcAlloc { .. }
                    | body::Op::RegionAlloc { .. }
                    | body::Op::PromoteManaged { .. } => demand.alloc_sites += 1,
                    body::Op::GcWriteBarrier { .. } | body::Op::GcWriteBarrierReserved { .. } => {
                        demand.barrier_sites += 1;
                    }
                    body::Op::SafepointPoll { .. } => demand.slow_edges += 1,
                    _ => {}
                }
            }
        }
        demand.slow_edges = demand.slow_edges.saturating_add(demand.alloc_sites);
        demand
    }

    /// LocalHeap 需求：从优化后 LIR 统计 placement、pin 与 managed store 站点。
    ///
    /// 类型侧口径（managed 类型数、超过单 block 的类型数、最大 payload）由冻结类型表给出，
    /// 因此调用方必须传入同一份计数，跨段相等性由 runtime raw 契约复验。
    pub(crate) fn local_heap_demand(
        &self,
        managed_types: u32,
        large_types: u32,
        max_object_bytes: u64,
    ) -> crate::runtime::LocalHeapDemand {
        use crate::frontend::gir::placement::PlacementKind;
        let mut demand = crate::runtime::LocalHeapDemand {
            managed_types,
            large_types,
            max_object_bytes,
            ..crate::runtime::LocalHeapDemand::default()
        };
        let visit = |demand: &mut crate::runtime::LocalHeapDemand, call: &body::Call| {
            let body::CallTarget::Runtime(target) = call.target else {
                return;
            };
            match target {
                body::RuntimeCall::Pin => demand.pin_sites += 1,
                body::RuntimeCall::Unpin => demand.unpin_sites += 1,
                _ => {}
            }
        };
        for world_body in &self.world.bodies {
            for instruction in &world_body.instructions {
                match &instruction.op {
                    body::Op::GcAlloc { placement, .. } => {
                        demand.alloc_sites += 1;
                        match placement {
                            PlacementKind::Pinned => demand.pinned_sites += 1,
                            PlacementKind::Resource => demand.resource_sites += 1,
                            PlacementKind::LocalHeap | PlacementKind::SharedHeap => {}
                            _ => {}
                        }
                    }
                    body::Op::PromoteManaged { .. } => demand.promote_sites += 1,
                    body::Op::GcWriteBarrier { .. } | body::Op::GcWriteBarrierReserved { .. } => {
                        demand.barrier_sites += 1;
                    }
                    body::Op::Call(call) | body::Op::ForeignCall(call) => visit(&mut demand, call),
                    _ => {}
                }
            }
            for block in &world_body.blocks {
                if let body::Terminator::Invoke { call, .. }
                | body::Terminator::TailCall { call, .. } = &block.terminator
                {
                    visit(&mut demand, call);
                }
            }
        }
        demand
    }

    /// SharedHeap 需求：从优化后 LIR 的 handle 指令与 placement 推导。
    ///
    /// 分配站点与解析站点分别计数，再由 `SharedHeapDemand::verify` 强制相等：任何一个
    /// SharedHeap allocation 缺少对应解析都会在契约构建阶段变成 `E0058`。pin 站点只看
    /// `RuntimeCall::Pin`/`Unpin` 的第一参数 provenance 是否为 SharedHandle，因此 LocalHeap
    /// 的 pin 不会进入共享段。payload 复制站点与 forward 站点同源，共享字段屏障站点按
    /// `SharedFieldBarrier`/`SharedFieldBarrierReserved` 计数，mark 站点取同一集合（每次共享
    /// 字段写入都可能产生跨 owner 标记）。最大 payload 字节数由调用方传入的冻结类型表上界
    /// 给出，没有共享分配时保持 0。
    pub(crate) fn shared_heap_demand(
        &self,
        max_payload_bytes: u64,
    ) -> crate::runtime::SharedHeapDemand {
        use crate::frontend::gir::placement::PlacementKind;
        let mut demand = crate::runtime::SharedHeapDemand::default();
        for world_body in &self.world.bodies {
            let shared_pin = |call: &body::Call, arguments: &std::ops::Range<u32>| {
                if !matches!(
                    call.target,
                    body::CallTarget::Runtime(body::RuntimeCall::Pin | body::RuntimeCall::Unpin)
                ) {
                    return false;
                }
                world_body.args(arguments).first().is_some_and(|value| {
                    world_body.values[value.index()].kind.provenance
                        == Some(body::Provenance::SharedHandle)
                })
            };
            for instruction in &world_body.instructions {
                match &instruction.op {
                    body::Op::GcAlloc { placement, .. } => {
                        if *placement == PlacementKind::SharedHeap {
                            demand.alloc_sites += 1;
                        }
                    }
                    body::Op::ResolveSharedHandle => demand.resolve_sites += 1,
                    body::Op::SharedAccessBegin { .. } => demand.access_begin_sites += 1,
                    body::Op::SharedAccessEnd { .. } => demand.access_end_sites += 1,
                    body::Op::ForwardSharedHandle => demand.forward_sites += 1,
                    body::Op::SharedFieldBarrier { .. }
                    | body::Op::SharedFieldBarrierReserved { .. } => demand.barrier_sites += 1,
                    body::Op::Call(call) | body::Op::ForeignCall(call) => {
                        if shared_pin(call, &instruction.arguments) {
                            demand.pin_sites += 1;
                        }
                    }
                    _ => {}
                }
            }
            for block in &world_body.blocks {
                match &block.terminator {
                    body::Terminator::Invoke {
                        call, arguments, ..
                    }
                    | body::Terminator::TailCall {
                        call, arguments, ..
                    } => {
                        if shared_pin(call, arguments) {
                            demand.pin_sites += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
        demand.handle_slots = demand.alloc_sites;
        demand.payload_copy_sites = demand.forward_sites;
        demand.mark_sites = demand.barrier_sites;
        if demand.alloc_sites != 0 {
            demand.max_payload_bytes = max_payload_bytes;
        }
        demand
    }

    /// 同步需求：从优化后 LIR 统计原子操作与同步原语需求。
    pub(crate) fn sync_demand(&self) -> crate::runtime::SyncDemand {
        let mut demand = crate::runtime::SyncDemand::default();
        for world_body in &self.world.bodies {
            for instruction in &world_body.instructions {
                if let body::Op::Atomic { .. } = &instruction.op {
                    demand.atomic_ops += 1;
                }
            }
        }
        demand
    }

    /// 一次推导栈图世界，同时给出栈图需求与压缩引用需求。
    ///
    /// 推导失败返回零栈图需求与只含解码点计数的压缩需求，由 `RuntimeRawContractV1` 的构建
    /// 路径按 `E0058` 拒绝；调用方必须保证传入优化后且已通过 verifier 的 world。
    pub(crate) fn stackmap_demands(
        &self,
        module: &hir::Module,
    ) -> (
        crate::runtime::StackMapDemand,
        crate::runtime::CompressionDemand,
    ) {
        let mut compression = crate::runtime::CompressionDemand {
            decode_sites: 0,
            compressed_root_slots: 0,
        };
        for world_body in &self.world.bodies {
            for instruction in &world_body.instructions {
                if matches!(instruction.op, body::Op::DecodeCompressedRef) {
                    compression.decode_sites += 1;
                }
            }
        }
        let mut demand = crate::runtime::StackMapDemand::default();
        let Ok(world) = stackmap::derive(&self.world.bodies, module) else {
            return (demand, compression);
        };
        demand.functions = world.functions.len() as u32;
        demand.safepoints = world.safepoints.len() as u32;
        demand.maps = world.map_count;
        for safepoint in &world.safepoints {
            match safepoint.kind {
                stackmap::KIND_CALL_RETURN => demand.call_return += 1,
                stackmap::KIND_POLL_RESUME => demand.poll_resume += 1,
                stackmap::KIND_SUSPEND_RESUME => demand.suspend_resume += 1,
                stackmap::KIND_FOREIGN_BRIDGE => demand.foreign_bridge += 1,
                stackmap::KIND_MORESTACK_ENTRY => demand.morestack_entry += 1,
                _ => {}
            }
            demand.root_words += safepoint.roots.words();
        }
        for function in &world.functions {
            demand.alloc_sites += function.alloc_sites;
            demand.barrier_sites += function.barrier_sites;
            demand.functions_with_landing += u32::from(function.has_landing);
        }
        compression.compressed_root_slots = u32::try_from(
            world
                .safepoints
                .iter()
                .map(|point| point.roots.compressed.len())
                .sum::<usize>(),
        )
        .expect("压缩根槽数适配 u32");
        (demand, compression)
    }
    /// 世界内 `SafepointPoll` 数量。
    pub(crate) fn poll_count(&self) -> usize {
        self.world
            .bodies
            .iter()
            .flat_map(|body| &body.instructions)
            .filter(|instruction| matches!(instruction.op, body::Op::SafepointPoll { .. }))
            .count()
    }
    /// 被分类为 poll-free 叶的 managed 直接调用目标数量。
    pub(crate) fn poll_free_leaf_count(&self) -> usize {
        let mut keys = std::collections::BTreeSet::new();
        for body in &self.world.bodies {
            for instruction in &body.instructions {
                if let body::Op::Call(call) | body::Op::ForeignCall(call) = &instruction.op
                    && call.poll_free_leaf
                    && let body::CallTarget::Instance(key) = call.target
                {
                    keys.insert(key);
                }
            }
            for block in &body.blocks {
                if let body::Terminator::Invoke { call, .. }
                | body::Terminator::TailCall { call, .. } = &block.terminator
                    && call.poll_free_leaf
                    && let body::CallTarget::Instance(key) = call.target
                {
                    keys.insert(key);
                }
            }
        }
        keys.len()
    }
    /// poll 摘要按 body 顺序的域哈希。
    pub(crate) fn poll_summary_fingerprint(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new_derive_key("gugu-lir-poll-summary-v1");
        for body in &self.world.bodies {
            hash.update(&serde_json::to_vec(&body.poll_summary).expect("poll 摘要可序列化"));
        }
        *hash.finalize().as_bytes()
    }
    pub(crate) fn dump(&self) -> String {
        dump::world(&self.world)
    }
    /// 固定管线 revision。
    pub(crate) fn optimization_revision(&self) -> u32 {
        pass::policy::PASS_PIPELINE_REVISION
    }
    /// poll 预算。
    pub(crate) fn poll_budget(&self) -> u32 {
        pass::policy::POLL_BUDGET
    }
}

/// 进入 action key 的优化策略规范字节。
pub(crate) fn optimization_policy_bytes() -> Vec<u8> {
    pass::policy::OptimizationPolicyV1::default().canonical_bytes()
}

pub(crate) fn build(
    hir: &hir::Validated,
    gir: &gir::GirWorldV1,
    mono: &mono::MonoWorldV1,
    target: crate::TargetName,
    queries: &QueryEngine,
    sources: &crate::SourceMap,
) -> Result<Validated, Vec<Diagnostic>> {
    let profile = target.descriptor().cost_profile;
    let target = target.to_string();
    let mut hash = blake3::Hasher::new_derive_key("gugu-lir-input-v1");
    hash.update(&hir.fingerprint());
    hash.update(&gir.fingerprint);
    hash.update(&mono.graph_fingerprint);
    hash.update(&mono.universe.fingerprint);
    hash.update(&mono.late.fingerprint);
    hash.update(target.as_bytes());
    let fingerprint = *hash.finalize().as_bytes();
    let mut bodies = Vec::with_capacity(gir.concrete.len());
    for concrete in &gir.concrete {
        let mut bytes = fingerprint.to_vec();
        bytes.extend_from_slice(&concrete.instance);
        bytes.extend_from_slice(&concrete.fingerprint);
        let key = QueryKey::new(QueryKind::BuildLir, SCHEMA, &bytes);
        let computed = queries.compute(key, |context| {
            for source in sources.snapshots() {
                context.record_dependency(
                    QueryKey::new(QueryKind::SourceSnapshot, 1, source.logical_path()),
                    source.content_hash(),
                );
            }
            context.record_dependency(
                QueryKey::new(
                    QueryKind::BuildGenericGir,
                    gir::BUILD_SCHEMA,
                    concrete.body.owner_key,
                ),
                concrete.fingerprint,
            );
            context.record_dependency(
                QueryKey::new(QueryKind::FreezeTypeUniverse, 1, mono.graph_fingerprint),
                mono.universe.fingerprint,
            );
            context.record_dependency(
                QueryKey::new(QueryKind::EvaluateLateComptime, 1, mono.graph_fingerprint),
                mono.late.fingerprint,
            );
            let body = build::lower(concrete, hir.module(), gir, mono, &target, fingerprint)
                .and_then(|body| {
                    verify::verify_structure(&body, hir.module())?;
                    Ok(body)
                })
                .map_err(|error| crate::frontend::semantics::query::store_errors(&[error]))?;
            Ok((
                serde_json::to_vec(&body).expect("LIR body 可序列化"),
                Vec::new(),
            ))
        });
        let computed = computed
            .map_err(|error| crate::frontend::semantics::query::restore_errors(error, sources))?;
        let body: body::Body = serde_json::from_slice(computed.payload())
            .map_err(|_| vec![invalid("缓存 LIR 不是合法 schema")])?;
        if body.input_fingerprint != fingerprint
            || body.instance != concrete.instance
            || body.target != target
        {
            return Err(vec![invalid("缓存 LIR 没有绑定当前输入或目标")]);
        }
        verify::verify_structure(&body, hir.module()).map_err(|error| vec![error])?;
        bodies.push(body);
    }
    pass::optimize_world(&mut bodies, hir.module(), &profile)?;
    for body in &bodies {
        verify::verify(body, hir.module()).map_err(|error| vec![error])?;
    }
    let mut world = World {
        schema: SCHEMA,
        target,
        input_fingerprint: fingerprint,
        bodies,
        fingerprint: [0; 32],
    };
    world.fingerprint = world_fingerprint(&world);
    verify_world(&world, mono).map_err(|error| vec![error])?;
    Ok(Validated {
        world: Arc::new(world),
    })
}

fn world_fingerprint(world: &World) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key("gugu-lir-world-v2");
    hash.update(&world.schema.to_le_bytes());
    hash.update(world.target.as_bytes());
    hash.update(&world.input_fingerprint);
    for body in &world.bodies {
        hash.update(&body.fingerprint());
    }
    *hash.finalize().as_bytes()
}

fn verify_world(world: &World, mono: &mono::MonoWorldV1) -> Result<(), Diagnostic> {
    if world.schema != SCHEMA
        || world.fingerprint != world_fingerprint(world)
        || !world
            .bodies
            .windows(2)
            .all(|pair| pair[0].instance < pair[1].instance)
    {
        return Err(invalid("LIR world 身份、schema 或规范实例顺序不匹配"));
    }
    for body in &world.bodies {
        for instruction in &body.instructions {
            match &instruction.op {
                body::Op::SymbolAddr(body::Symbol::Instance(key)) => instance_exists(mono, key)?,
                body::Op::SymbolAddr(body::Symbol::TypeId(key)) => {
                    mono.universe.record(key)?;
                }
                body::Op::Call(call) | body::Op::ForeignCall(call) => {
                    verify_call(world, mono, call)?
                }
                _ => {}
            }
        }
        for block in &body.blocks {
            if let body::Terminator::Invoke { call, .. } | body::Terminator::TailCall { call, .. } =
                &block.terminator
            {
                verify_call(world, mono, call)?;
            }
        }
    }
    // 资源隔离是跨 body 的世界级不变量：描述符集合来自资源调用参数，闸门在分配点生效。
    verify::resource_isolation::verify(&world.bodies)?;
    Ok(())
}

fn instance_exists(mono: &mono::MonoWorldV1, key: &[u8; 32]) -> Result<(), Diagnostic> {
    if mono
        .instances
        .iter()
        .any(|instance| &mono::digest_of(&instance.mono_key) == key)
    {
        Ok(())
    } else {
        Err(invalid("LIR 引用了实例闭包之外的代码"))
    }
}

fn verify_call(
    world: &World,
    mono: &mono::MonoWorldV1,
    call: &body::Call,
) -> Result<(), Diagnostic> {
    let body::CallTarget::Instance(key) = call.target else {
        return Ok(());
    };
    instance_exists(mono, &key)?;
    let target = world
        .bodies
        .binary_search_by_key(&key, |body| body.instance)
        .ok()
        .map(|index| &world.bodies[index])
        .ok_or_else(|| invalid("静态调用目标没有具体 LIR body"))?;
    if target.signature.parameters.len() != call.parameters.len()
        || target.signature.results.len() != call.results.len()
        || target.signature.by_value != call.by_value
        || target.signature.sret.map(|(bytes, _, key)| (0, bytes, key)) != call.sret
        || target
            .signature
            .parameters
            .iter()
            .zip(&call.parameters)
            .enumerate()
            .any(|(index, (expected, actual))| {
                let address = target.signature.by_value.iter().any(|(parameter, _, _)| {
                    usize::try_from(*parameter).expect("参数编号") == index
                }) || target.signature.sret.is_some() && index == 0;
                if address {
                    actual.ty != body::Type::Ptr
                        || matches!(
                            actual.provenance,
                            Some(body::Provenance::Code | body::Provenance::Metadata)
                        )
                } else {
                    !verify::compatible(*expected, *actual)
                }
            })
        || target
            .signature
            .results
            .iter()
            .zip(&call.results)
            .any(|(expected, actual)| !verify::compatible(*expected, *actual))
    {
        return Err(invalid(&format!(
            "LIR 静态调用与目标机器签名不一致：{}，期望 {:?}，实际 {:?}",
            target.name, target.signature, call
        )));
    }
    Ok(())
}

fn invalid(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::LirInvariant, message, None)
}

/// runtime raw 平面契约失败：publish 区域或消息字段违反登记的不变量。
pub(crate) fn invalid_raw(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::RuntimeRawInvariant, message, None)
}

/// 资源隔离失败：resource 类描述符越过资源域边界进入 managed region 分配。
pub(crate) fn invalid_resource(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::ResourceInvariant, message, None)
}

#[cfg(test)]
mod tests;

/// 返回一个值对应的整数常量；不是常量时返回 0。
fn const_bytes(body: &body::Body, value: body::ValueId) -> u64 {
    let definition = &body.values[value.index()].definition;
    match definition {
        body::Definition::Instruction { instruction, .. } => {
            match &body.instructions[instruction.index()].op {
                body::Op::IConst(bytes) => *bytes,
                _ => 0,
            }
        }
        _ => 0,
    }
}
