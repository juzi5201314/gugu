//! LIR 层：把具体实例的 GIR 降为 LIR，运行固定优化管线、poll placement 与栈图推导，
//! 并以结构 verifier 守住 backend 的唯一低层输入；构造与缓存恢复共享同一 verifier。
//! 目标知识只经 [`target::TargetOracle`] 注入，本 crate 不依赖 backend。
#![forbid(unsafe_code)]
pub mod body;
mod build;
mod callback_adapter;
mod dump;
mod effects;
pub mod exports;
pub mod function_adapter;
pub mod installed;
pub use pass::graph::Graph;
pub mod liveness;
pub mod pass;
#[allow(
    dead_code,
    reason = "逐根初始化证明由确定性回归测试消费；帧内位图与 section 条目接入后转入生产路径"
)]
pub mod root_init;
pub mod stackmap;
pub mod target;
pub mod uses;
pub mod verify;

use gugu_base::{
    canonical,
    diagnostics::{Diagnostic, DiagnosticCode},
    hash, query,
    source::{SourceFileId, SourceMap},
    target::{BackendCostProfile, TargetName},
};
use gugu_gir::{gir, hir, late, mono, semantics};
use gugu_runtime_abi as runtime;
use query::{QueryEngine, QueryKey, QueryKind};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// LIR query 与 world 的 schema。22：结构化 IEEE 比较。
pub const SCHEMA: u32 = 22;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct World {
    schema: u32,
    target: String,
    input_fingerprint: [u8; 32],
    bodies: Vec<body::Body>,
    exports: Vec<exports::Export>,
    /// 与 `bodies` 同序：该 body 是否由内建函数表安装。安装来源不是 body 内容，不进指纹。
    installed: Vec<bool>,
    fingerprint: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Validated {
    world: Arc<World>,
}

impl Validated {
    pub fn fingerprint(&self) -> [u8; 32] {
        self.world.fingerprint
    }
    pub fn exports(&self) -> &[exports::Export] {
        &self.world.exports
    }
    pub fn body_count(&self) -> usize {
        self.world.bodies.len()
    }
    /// 按实例稳定键升序的 body 列表。
    pub fn bodies(&self) -> &[body::Body] {
        &self.world.bodies
    }
    /// 与 [`Self::bodies`] 同序：该 body 是否由内建函数表安装（poll 之后的录制形态）。
    pub fn installed(&self) -> &[bool] {
        &self.world.installed
    }
    /// 测试用：沿用本 world 的 schema、目标与指纹，只替换 body 列表（视为全部现算）；不重算指纹。
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_bodies(&self, bodies: Vec<body::Body>) -> Self {
        let mut world = (*self.world).clone();
        world.installed = vec![false; bodies.len()];
        world.bodies = bodies;
        Self {
            world: Arc::new(world),
        }
    }
    pub fn blocks(&self) -> usize {
        self.world.bodies.iter().map(|body| body.blocks.len()).sum()
    }
    pub fn instructions(&self) -> usize {
        self.world
            .bodies
            .iter()
            .map(|body| body.instructions.len())
            .sum()
    }
    pub fn memory_operations(&self) -> usize {
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
    pub fn safepoints(&self) -> usize {
        self.world
            .bodies
            .iter()
            .map(|body| body.safepoints.len())
            .sum()
    }
    /// runtime只消费优化后仍存在的创建、入口检查与suspend边界。
    pub fn coroutine_demand(&self) -> crate::runtime::CoroutineDemand {
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
    pub fn turn_region_demand(&self) -> crate::runtime::region_schema::TurnRegionDemand {
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
    pub fn scheduler_demand(&self) -> crate::runtime::SchedulerDemand {
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

    /// 外调需求：按调用点的 `CallKind` 计数，不从符号名猜测模式。
    pub fn foreign_demand(&self) -> crate::runtime::ForeignDemand {
        let mut demand = crate::runtime::ForeignDemand::default();
        let bump = |demand: &mut crate::runtime::ForeignDemand, call: &body::Call| {
            use crate::gir::body::CallKind;
            match call.kind {
                CallKind::ForeignBridge => demand.ordinary += 1,
                CallKind::ForeignBridgeDirtyCpu => demand.dirty += 1,
                CallKind::ForeignLeaf { .. } => demand.leaf += 1,
                CallKind::Managed => {}
            }
            if let body::CallTarget::Runtime(body::RuntimeCall::BlockingIo { .. }) = call.target {
                demand.ordinary += 1;
                demand.blocking_io += 1;
            }
        };
        for world_body in &self.world.bodies {
            for instruction in &world_body.instructions {
                if let body::Op::Call(call) | body::Op::ForeignCall(call) = &instruction.op {
                    bump(&mut demand, call);
                }
                if matches!(
                    instruction.op,
                    body::Op::RuntimeMachineCall(crate::runtime::RuntimeMachineOp::FsStart)
                ) {
                    demand.ordinary += 1;
                    demand.blocking_io += 1;
                }
            }
            for block in &world_body.blocks {
                if let body::Terminator::Invoke { call, .. }
                | body::Terminator::TailCall { call, .. } = &block.terminator
                {
                    bump(&mut demand, call);
                }
            }
        }
        demand
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn demand_with_blocking(&self, body: &body::Body) -> crate::runtime::ForeignDemand {
        let mut demand = self.foreign_demand();
        for instruction in &body.instructions {
            if let body::Op::Call(call) | body::Op::ForeignCall(call) = &instruction.op
                && matches!(
                    call.target,
                    body::CallTarget::Runtime(body::RuntimeCall::BlockingIo { .. })
                )
            {
                demand.ordinary += 1;
                demand.blocking_io += 1;
            }
        }
        demand
    }
    /// 等待源需求：从优化后 LIR 统计 channel / Join / select 调用与 Select safepoint。
    pub fn wait_demand(&self) -> crate::runtime::WaitDemand {
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
    pub fn barrier_demand(&self) -> crate::runtime::BarrierDemand {
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
    pub fn pacing_demand(&self, managed_types: u32) -> crate::runtime::GcPacingDemand {
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
    pub fn local_heap_demand(
        &self,
        module: &hir::Module,
        managed_types: u32,
        large_types: u32,
        max_object_bytes: u64,
    ) -> crate::runtime::LocalHeapDemand {
        use crate::gir::placement::PlacementKind;
        let mut demand = crate::runtime::LocalHeapDemand {
            managed_types,
            large_types,
            max_object_bytes,
            ..crate::runtime::LocalHeapDemand::default()
        };
        let pin_targets = ["pin_acquire", "pin_release"].map(|name| {
            module
                .definitions
                .iter()
                .find(|definition| {
                    definition.name == name
                        && definition.kind == hir::DefinitionKind::Function
                        && definition.location.as_ref().is_some_and(|location| {
                            module.sources[location.source as usize].path
                                == "std/runtime/pinning.gg"
                        })
                })
                .and_then(|definition| {
                    self.bodies()
                        .iter()
                        .find(|body| body.owner == definition.key)
                })
                .map(|body| body.instance)
        });
        let visit = |demand: &mut crate::runtime::LocalHeapDemand, call: &body::Call| {
            let body::CallTarget::Instance(target) = call.target else {
                return;
            };
            if pin_targets[0] == Some(target) {
                demand.pin_sites += 1;
            }
            if pin_targets[1] == Some(target) {
                demand.unpin_sites += 1;
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
    /// SharedHeap allocation 缺少对应解析都会在契约构建阶段变成 `E0058`。源码 pin 的
    /// acquire/release 调用统一进入 LocalHeap 需求；未产生 handle pin 指令时共享 pin 计数为零。
    /// payload 复制站点与 forward 站点同源，共享字段屏障站点按
    /// `SharedFieldBarrier`/`SharedFieldBarrierReserved` 计数，mark 站点取同一集合（每次共享
    /// 字段写入都可能产生跨 owner 标记）。最大 payload 字节数由调用方传入的冻结类型表上界
    /// 给出，没有共享分配时保持 0。
    pub fn shared_heap_demand(&self, max_payload_bytes: u64) -> crate::runtime::SharedHeapDemand {
        use crate::gir::placement::PlacementKind;
        let mut demand = crate::runtime::SharedHeapDemand::default();
        for world_body in &self.world.bodies {
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
    pub fn sync_demand(&self) -> crate::runtime::SyncDemand {
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
    pub fn stackmap_demands(
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
    pub fn poll_count(&self) -> usize {
        self.world
            .bodies
            .iter()
            .flat_map(|body| &body.instructions)
            .filter(|instruction| matches!(instruction.op, body::Op::SafepointPoll { .. }))
            .count()
    }
    /// 被分类为 poll-free 叶的 managed 直接调用目标数量。
    pub fn poll_free_leaf_count(&self) -> usize {
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
    pub fn poll_summary_fingerprint(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new_derive_key("gugu-lir-poll-summary-v1");
        for body in &self.world.bodies {
            hash.update(&[u8::from(body.poll_summary.entry_stack_check)]);
            hash.update(&body.poll_summary.poll_free_cost.to_le_bytes());
            hash.update(&[u8::from(body.poll_summary.has_poll_free_cycle)]);
        }
        *hash.finalize().as_bytes()
    }
    /// LIR world 的编译目标。
    pub fn target(&self) -> crate::TargetName {
        crate::TargetName::from_compiler_artifact(&self.world.target).expect("LIR world 目标有效")
    }
    pub fn dump(&self, oracle: &dyn target::TargetOracle) -> String {
        dump::world(&self.world, oracle)
    }
    /// 固定管线 revision。
    pub fn optimization_revision(&self) -> u32 {
        pass::policy::PASS_PIPELINE_REVISION
    }
    /// poll 预算。
    pub fn poll_budget(&self) -> u32 {
        pass::policy::POLL_BUDGET
    }
}

/// 进入 action key 的优化策略规范字节。
pub fn optimization_policy_bytes() -> Vec<u8> {
    pass::policy::OptimizationPolicyV1::default().canonical_bytes()
}

#[expect(
    clippy::too_many_arguments,
    reason = "LIR 构建需要冻结 HIR、GIR/mono world、目标、压缩策略、目标 oracle、query、源码表与函数表"
)]
pub fn build(
    hir: &hir::Validated,
    gir: &gir::GirWorldV1,
    mono: &mono::MonoWorldV1,
    target: crate::TargetName,
    compression: crate::runtime::CompressionPolicyV1,
    oracle: &dyn target::TargetOracle,
    queries: &QueryEngine,
    sources: &crate::SourceMap,
    functions: (
        &gugu_gir::functions::InstalledFunctions,
        Option<&crate::runtime::RuntimePrebuiltV1>,
    ),
) -> Result<Validated, Vec<Diagnostic>> {
    let (installed_functions, prebuilt) = functions;
    let entries = installed::entries(prebuilt, compression).map_err(|error| vec![error])?;
    let descriptor = target.descriptor();
    let profile = descriptor.cost_profile;
    let target = target.to_string();
    let mut hash = blake3::Hasher::new_derive_key("gugu-lir-input-v1");
    hash.update(&hir.fingerprint());
    hash.update(&gir.fingerprint);
    hash.update(&mono.graph_fingerprint);
    hash.update(&mono.universe.fingerprint);
    hash.update(&mono.late.fingerprint);
    hash.update(target.as_bytes());
    // cage profile 参与 LIR 输入身份：开启与关闭产生不同的 lowering 与解码点集合。
    hash.update(&[u8::from(compression.enabled)]);
    hash.update(&compression.cage_bytes.to_le_bytes());
    let fingerprint = *hash.finalize().as_bytes();
    let mut bodies = Vec::with_capacity(gir.concrete.len());
    // 索引按 LIR 输入建一次：逐 body 线性扫描实例表与具体 body 会把 lowering 放大成
    // O(body × 实例) 次 mono 摘要计算与 O(body × 语句) 次闭包创建点扫描。
    let index = build::LirIndex::new(mono, &gir.concrete);
    let mut installed_instances = Vec::new();
    for concrete in &gir.concrete {
        // 函数表安装的 body 已是 poll 之后的形态，不经 BuildLir 与逐 body pass。
        if let Some(body) =
            installed::decode(installed_functions, entries, &concrete.instance, &target)
                .map_err(|error| vec![error])?
        {
            installed_instances.push(body.instance);
            bodies.push(body);
            continue;
        }
        let mut bytes = fingerprint.to_vec();
        bytes.extend_from_slice(&concrete.instance);
        bytes.extend_from_slice(&concrete.fingerprint);
        let key = QueryKey::new(QueryKind::BuildLir, SCHEMA, &bytes);
        let mut fresh = None;
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
            let body = build::lower(
                concrete,
                hir.module(),
                gir,
                mono,
                &index,
                &target,
                compression,
                sources,
            )
            .and_then(|body| {
                verify::verify_structure(&body, hir.module())?;
                Ok(body)
            })
            .map_err(|error| crate::semantics::query::store_errors(&[error]))?;
            let payload = crate::canonical::to_vec(&body).expect("LIR body 编码");
            fresh = Some(body);
            Ok((payload, Vec::new()))
        });
        let computed =
            computed.map_err(|error| crate::semantics::query::restore_errors(error, sources))?;
        let body = if let Some(body) = fresh {
            body
        } else {
            let body: body::Body = crate::canonical::from_slice(computed.payload())
                .map_err(|_| vec![invalid("缓存 LIR 不是合法 schema")])?;
            // 输入身份由 BuildLir query key（world 输入指纹 + 实例 + 具体 GIR 指纹）绑定；
            // body 自身只核对 revision、实例与目标。
            if body.revision != body::REVISION
                || body.instance != concrete.instance
                || body.target != target
            {
                return Err(vec![invalid("缓存 LIR 没有绑定当前实例或目标")]);
            }
            verify::verify_structure(&body, hir.module()).map_err(|error| vec![error])?;
            body
        };
        bodies.push(body);
    }
    let exports = exports::materialize(&mut bodies, hir.module(), &gir.concrete)
        .map_err(|error| vec![error])?;
    function_adapter::materialize(&mut bodies, hir.module()).map_err(|error| vec![error])?;
    installed_instances.sort_unstable();
    let installed: Vec<bool> = bodies
        .iter()
        .map(|body| installed_instances.binary_search(&body.instance).is_ok())
        .collect();
    pass::optimize_world(&mut bodies, &installed, hir.module(), &profile, oracle)?;
    for body in &bodies {
        verify::verify(body, hir.module(), oracle).map_err(|error| vec![error])?;
    }
    let mut world = World {
        schema: SCHEMA,
        target,
        input_fingerprint: fingerprint,
        bodies,
        exports,
        installed,
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
    hash.update(&canonical::to_vec(&world.exports).expect("C 导出清单编码"));
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
    // 实例摘要点查表：逐指令线性扫描实例表会重算 O(指令 × 实例) 次 mono 摘要。
    let instances: Vec<[u8; 32]> = {
        let mut digests: Vec<[u8; 32]> = mono
            .instances
            .iter()
            .map(|instance| mono::digest_of(&instance.mono_key))
            .collect();
        digests.sort_unstable();
        digests
    };
    for body in &world.bodies {
        for instruction in &body.instructions {
            match &instruction.op {
                body::Op::SymbolAddr(body::Symbol::Instance(key)) => {
                    instance_exists(&instances, key)?
                }
                body::Op::SymbolAddr(body::Symbol::FunctionAdapter {
                    target,
                    has_environment,
                }) => {
                    instance_exists(&instances, target)?;
                    let key = function_adapter::key(target, *has_environment);
                    if world
                        .bodies
                        .binary_search_by_key(&key, |body| body.instance)
                        .is_err()
                    {
                        return Err(invalid("函数句柄缺少 checked 适配入口"));
                    }
                }
                body::Op::SymbolAddr(body::Symbol::TypeDescriptor(key)) => {
                    mono.universe.record(key)?;
                }
                body::Op::Call(call) | body::Op::ForeignCall(call) => {
                    verify_call(world, &instances, call)?
                }
                _ => {}
            }
        }
        for block in &body.blocks {
            if let body::Terminator::Invoke { call, .. } | body::Terminator::TailCall { call, .. } =
                &block.terminator
            {
                verify_call(world, &instances, call)?;
            }
        }
    }
    // 资源隔离是跨 body 的世界级不变量：描述符集合来自资源调用参数，闸门在分配点生效。
    verify::resource_isolation::verify(&world.bodies)?;
    Ok(())
}

/// 已排序实例摘要表里的点查；表由 `verify_world` 建一次。
fn instance_exists(instances: &[[u8; 32]], key: &[u8; 32]) -> Result<(), Diagnostic> {
    if instances.binary_search(key).is_ok() {
        Ok(())
    } else {
        Err(invalid("LIR 引用了实例闭包之外的代码"))
    }
}

fn verify_call(world: &World, instances: &[[u8; 32]], call: &body::Call) -> Result<(), Diagnostic> {
    let body::CallTarget::Instance(key) = call.target else {
        return Ok(());
    };
    instance_exists(instances, &key)?;
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
pub fn invalid_raw(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::RuntimeRawInvariant, message, None)
}

/// 资源隔离失败：resource 类描述符越过资源域边界进入 managed region 分配。
pub fn invalid_resource(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::ResourceInvariant, message, None)
}

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
