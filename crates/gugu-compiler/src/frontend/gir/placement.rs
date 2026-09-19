//! 逃逸分析与 TurnRegion / LocalHeap / SharedHeap placement。
//!
//! 分析未知时只选择能保留值、引用、身份和 resource 语义的路径，禁止 TurnRegion。
use super::body::{
    AggregateKind, BlockId, CallKind, Callee, IntrinsicOp, LocalId, LocalKind, Operand, Place,
    Rvalue, SelectOperation, StatementKind, SuspendReason, Terminator,
};
use super::liveness::{Liveness, TurnBoundaryKind, turn_boundaries};
use super::passing::{PassingClass, PassingTable};
use super::{GirBody, GirWorldV1, WORLD_SCHEMA, world_fingerprint};
use crate::frontend::analysis::{AnalysisWorldV1, ProofStatus};
use crate::frontend::hir::{self, TypeId};
use crate::frontend::semantics::query::restore_errors;
use crate::query::{QueryEngine, QueryKey, QueryKind};
use crate::{Diagnostic, SourceMap};
use serde::{Deserialize, Serialize};

pub(crate) const PLACEMENT_SCHEMA: u32 = 2;

/// export summary 位：逃逸、别名、资源、FFI、发布、未知、可转移与被 send 读取。上界 8 位。
pub(crate) struct ExportFlags;
impl ExportFlags {
    pub(crate) const ESCAPE: u8 = 1;
    pub(crate) const ALIAS: u8 = 2;
    pub(crate) const RESOURCE: u8 = 4;
    pub(crate) const FOREIGN: u8 = 8;
    pub(crate) const PUBLISH: u8 = 16;
    pub(crate) const UNKNOWN: u8 = 32;
    /// 私有 region 只在一个 channel send 处按「sender 之后不再使用」整体移交所有权。
    pub(crate) const TRANSFER: u8 = 64;
    /// 该值被 channel send 读取。
    ///
    /// 这是使用点事实，不是所有权事实：它单独出现时仍允许选 TurnRegion（由 region 判定进一步
    /// 决定「移交」还是退回 stable storage），因此不能像 `PUBLISH` 那样直接封死 region。
    pub(crate) const CHANNEL_SEND: u8 = 128;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum PlacementKind {
    Stack,
    TurnRegion,
    LocalHeap,
    SharedHeap,
    Pinned,
    Resource,
    RuntimeRaw,
    Foreign,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlacementRecord {
    pub(crate) body: u32,
    pub(crate) local: u32,
    pub(crate) kind: PlacementKind,
    pub(crate) proof: ProofStatus,
    pub(crate) export: u8,
}

/// 一个分配点的 placement 决策；`region` 指向该站点所属的 TurnRegion 计划。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AllocPlacement {
    pub(crate) body: u32,
    pub(crate) statement: u32,
    pub(crate) kind: PlacementKind,
    pub(crate) proof: ProofStatus,
    pub(crate) export: u8,
    /// 仅 `TurnRegion` 站点拥有；其余为 `None`。
    pub(crate) region: Option<u32>,
}

/// 一个 region 的出口边界；`transfer` 为真表示在该边界上整体移交所有权。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RegionExit {
    pub(crate) block: u32,
    pub(crate) transfer: bool,
}

/// 一个 turn segment 的 region 计划。
///
/// `exits` 是该 segment 的出口边界块；每条到达出口的路径上恰好经过一个出口，因此
/// `RegionPublish` 与 `RegionReset`/`RegionTransfer` 在每条路径上各出现一次。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RegionPlan {
    pub(crate) body: u32,
    pub(crate) region: u32,
    pub(crate) allocations: u32,
    /// 该 region 在 publish 时登记的 export summary 位。
    pub(crate) export: u8,
    pub(crate) exits: Vec<RegionExit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlacementWorldV1 {
    pub(crate) schema: u32,
    pub(crate) records: Vec<PlacementRecord>,
    pub(crate) allocs: Vec<AllocPlacement>,
    /// 按 `(body, region)` 规范排序的 region 计划。
    pub(crate) regions: Vec<RegionPlan>,
    pub(crate) fingerprint: [u8; 32],
}

impl PlacementWorldV1 {
    pub(crate) fn empty() -> Self {
        Self {
            schema: PLACEMENT_SCHEMA,
            records: Vec::new(),
            allocs: Vec::new(),
            regions: Vec::new(),
            fingerprint: fingerprint(&[], &[], &[]),
        }
    }

    /// 按 `(body, region)` 点查 region 计划。
    pub(crate) fn region(&self, body: u32, region: u32) -> Option<&RegionPlan> {
        self.regions
            .iter()
            .find(|plan| plan.body == body && plan.region == region)
    }

    pub(crate) fn counts(&self) -> PlacementCounts {
        let mut counts = PlacementCounts::default();
        for record in &self.records {
            tally(&mut counts, record.kind);
        }
        for alloc in &self.allocs {
            tally(&mut counts, alloc.kind);
        }
        counts.turn_regions = self.regions.len() as u32;
        counts
    }
}

fn tally(counts: &mut PlacementCounts, kind: PlacementKind) {
    counts.total += 1;
    match kind {
        PlacementKind::TurnRegion => counts.turn_region += 1,
        PlacementKind::LocalHeap => counts.local_heap += 1,
        PlacementKind::SharedHeap => counts.shared_heap += 1,
        PlacementKind::Resource => counts.resource += 1,
        PlacementKind::RuntimeRaw => counts.runtime_raw += 1,
        PlacementKind::Stack | PlacementKind::Pinned | PlacementKind::Foreign => {}
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PlacementCounts {
    pub(crate) total: u32,
    pub(crate) turn_region: u32,
    /// 已建立的 region 计划数量。
    pub(crate) turn_regions: u32,
    pub(crate) local_heap: u32,
    pub(crate) shared_heap: u32,
    /// placement 判定为 `Resource` 的记录数量。
    pub(crate) resource: u32,
    /// placement 判定为 `RuntimeRaw` 的记录数量。
    pub(crate) runtime_raw: u32,
}

fn fingerprint(
    records: &[PlacementRecord],
    allocs: &[AllocPlacement],
    regions: &[RegionPlan],
) -> [u8; 32] {
    let bytes = serde_json::to_vec(&(records, allocs, regions)).expect("placement 可序列化");
    *blake3::Hasher::new_derive_key("gugu-placement-v1")
        .update(&bytes)
        .finalize()
        .as_bytes()
}

pub(crate) fn attach(mut world: GirWorldV1, placement: PlacementWorldV1) -> GirWorldV1 {
    world.placement = placement;
    world.schema = WORLD_SCHEMA;
    world.fingerprint = world_fingerprint(
        &world.bodies,
        &world.fragments,
        world.hir_fingerprint,
        &world.placement,
        &world.concrete,
    );
    world
}

pub(crate) fn run(
    hir: &hir::Validated,
    world: GirWorldV1,
    analysis: &AnalysisWorldV1,
    queries: &QueryEngine,
    sources: &SourceMap,
) -> Result<GirWorldV1, Vec<Diagnostic>> {
    let mut hash = blake3::Hasher::new_derive_key("gugu-escape-placement-v1");
    hash.update(&world.fingerprint);
    hash.update(&analysis.input_fingerprint);
    hash.update(&hir.fingerprint());
    let key = QueryKey::new(
        QueryKind::EscapeAndPlacement,
        PLACEMENT_SCHEMA,
        *hash.finalize().as_bytes(),
    );
    let mut fresh = None;
    let result = queries
        .compute(key, |context| {
            context.record_dependency(
                QueryKey::new(
                    QueryKind::BuildGenericGir,
                    super::BUILD_SCHEMA,
                    world.hir_fingerprint,
                ),
                world.fingerprint,
            );
            context.record_dependency(
                QueryKey::new(
                    QueryKind::WholeProgramAnalysis,
                    analysis.schema,
                    analysis.input_fingerprint,
                ),
                analysis.input_fingerprint,
            );
            let placement = compute(hir.module(), &world, analysis);
            let payload = serde_json::to_vec(&placement).expect("placement schema 序列化");
            fresh = Some(placement);
            Ok((payload, Vec::new()))
        })
        .map_err(|error| restore_errors(error, sources))?;
    let placement = match fresh {
        Some(placement) => placement,
        None => serde_json::from_slice(result.payload())
            .map_err(|_| vec![super::gir_error("placement query 缓存 schema 不合法", None)])?,
    };
    if placement.schema != PLACEMENT_SCHEMA {
        return Err(vec![super::gir_error("placement schema 不匹配", None)]);
    }
    verify(&placement).map_err(|error| vec![error])?;
    Ok(attach(world, placement))
}

fn compute(
    module: &hir::Module,
    world: &GirWorldV1,
    analysis: &AnalysisWorldV1,
) -> PlacementWorldV1 {
    let unknown_program = analysis.budget_exhausted;
    let table = PassingTable::new(module);
    let mut records = Vec::new();
    let mut allocs = Vec::new();
    let mut regions = Vec::new();
    for (body_index, body) in world.bodies.iter().enumerate() {
        let mut edges = Vec::new();
        let flags = escape(module, body, &table, &mut edges);
        let body_unknown =
            unknown_program || flags.iter().any(|flag| flag & ExportFlags::UNKNOWN != 0);
        for (local_index, local) in body.locals.iter().enumerate() {
            let class = table.class(local.ty);
            let export = flags.get(local_index).copied().unwrap_or(0) | class_export(class);
            let (kind, proof) = place_local(local.address_taken, class, export, body_unknown);
            records.push(PlacementRecord {
                body: body_index as u32,
                local: local_index as u32,
                kind,
                proof,
                export,
            });
        }
        let decisions = region_decisions(
            module,
            body_index as u32,
            body,
            &table,
            &flags,
            &edges,
            body_unknown,
            &mut allocs,
        );
        regions.extend(decisions.plans);
    }
    records.sort_by_key(|record| (record.body, record.local));
    allocs.sort_by_key(|alloc| (alloc.body, alloc.statement));
    regions.sort_by_key(|plan| (plan.body, plan.region));
    let digest = fingerprint(&records, &allocs, &regions);
    PlacementWorldV1 {
        schema: PLACEMENT_SCHEMA,
        records,
        allocs,
        regions,
        fingerprint: digest,
    }
}

fn class_export(class: PassingClass) -> u8 {
    let mut export = 0;
    if class.has_resource() {
        export |= ExportFlags::RESOURCE;
    }
    if class.is_unknown() {
        export |= ExportFlags::UNKNOWN;
    }
    export
}

fn place_local(
    address_taken: bool,
    class: PassingClass,
    export: u8,
    body_unknown: bool,
) -> (PlacementKind, ProofStatus) {
    if class.has_resource() && !class.has_identity() && !class.has_cow() {
        return (PlacementKind::Resource, ProofStatus::Proved);
    }
    let unknown = body_unknown || export & ExportFlags::UNKNOWN != 0;
    let published = export & ExportFlags::PUBLISH != 0;
    let foreign = export & ExportFlags::FOREIGN != 0;
    let ref_out = export & ExportFlags::ESCAPE != 0;
    if address_taken && ref_out {
        return escaping_slot(published, foreign, unknown);
    }
    if class.is_unknown() {
        return (PlacementKind::LocalHeap, ProofStatus::Unknown);
    }
    (PlacementKind::Stack, ProofStatus::Proved)
}

fn escaping_slot(published: bool, foreign: bool, unknown: bool) -> (PlacementKind, ProofStatus) {
    let proof = if unknown {
        ProofStatus::Unknown
    } else {
        ProofStatus::Proved
    };
    if published {
        (PlacementKind::SharedHeap, proof)
    } else if foreign {
        (PlacementKind::Pinned, proof)
    } else {
        (PlacementKind::LocalHeap, proof)
    }
}

/// 分配点表只收录 managed 分配：对象/数组装箱与真正带环境的闭包。
///
/// 通道与协程控制的存储由 runtime raw 平面持有（LIR 走 `RuntimeCall::ChannelNew`/`Spawn`），
/// 不经过 `RegionAlloc`/`GcAlloc`，因此它们既不是 region 候选，也不产生分配点记录；普通
/// channel 因而永远不会获得 region 的 transfer 语义。
///
/// 无捕获槽的函数项引用同样不产生环境分配——LIR 的 `capture_environment` 直接返回空指针，
/// 因此这里不能为它建立分配点，否则会出现没有 `RegionAlloc` 的 publish。
fn is_alloc(module: &hir::Module, rvalue: &Rvalue) -> bool {
    match rvalue {
        Rvalue::AllocObject { .. } | Rvalue::AllocArray { .. } => true,
        Rvalue::FunctionValue(candidate) => has_environment(module, candidate.definition),
        Rvalue::Aggregate {
            kind: AggregateKind::Closure(definition) | AggregateKind::Coroutine(definition),
            ..
        } => has_environment(module, *definition),
        _ => false,
    }
}

/// 该定义是否真的持有环境存储。
fn has_environment(module: &hir::Module, definition: hir::DefId) -> bool {
    module
        .owners
        .iter()
        .find(|owner| owner.definition == definition)
        .is_some_and(|owner| !owner.captures.is_empty())
}

fn place_alloc(
    class: PassingClass,
    export: u8,
    body_unknown: bool,
) -> (PlacementKind, ProofStatus) {
    if class.has_resource() {
        return (PlacementKind::Resource, ProofStatus::Proved);
    }
    let unknown = body_unknown || export & ExportFlags::UNKNOWN != 0 || class.is_unknown();
    let published = export & ExportFlags::PUBLISH != 0;
    let foreign = export & ExportFlags::FOREIGN != 0;
    let aliased = export & ExportFlags::ALIAS != 0;
    let resource = export & ExportFlags::RESOURCE != 0;
    if published {
        return (
            PlacementKind::SharedHeap,
            if unknown {
                ProofStatus::Unknown
            } else {
                ProofStatus::Proved
            },
        );
    }
    let escaped = export & ExportFlags::ESCAPE != 0;
    if unknown || aliased || resource || foreign || escaped {
        return (
            PlacementKind::LocalHeap,
            if unknown {
                ProofStatus::Unknown
            } else {
                ProofStatus::Proved
            },
        );
    }
    (PlacementKind::TurnRegion, ProofStatus::Proved)
}

fn place_ty(body: &GirBody, place: Place) -> TypeId {
    let mut ty = body.locals[place.local.index()].ty;
    for projection in body.projections_of(place) {
        ty = match projection {
            super::body::Projection::Field { field_ty, .. }
            | super::body::Projection::TupleField { field_ty, .. }
            | super::body::Projection::OpaqueCast(field_ty) => *field_ty,
            _ => ty,
        };
    }
    ty
}

/// 每 local 一个 `u8` 导出位；local 数量等于 body.locals，稠密下标访问。
///
/// `edges` 回传同一份传播图，供 region 判定的指针可达性复用：by-ref 边与 by-value 边都会
/// 被 push，region 对象指针在 local 之间的任何移动都必须经过它。
fn escape(
    module: &hir::Module,
    body: &GirBody,
    table: &PassingTable,
    edges: &mut Vec<(u32, u32, bool)>,
) -> Vec<u8> {
    let mut flags = vec![0u8; body.locals.len()];
    seed(module, body, table, &mut flags, edges);
    propagate(&mut flags, edges);
    for (index, local) in body.locals.iter().enumerate() {
        if table.class(local.ty).has_resource() {
            flags[index] |= ExportFlags::RESOURCE;
        }
        if local.kind == LocalKind::Argument && table.class(local.ty).has_identity() {
            flags[index] |= ExportFlags::ALIAS;
        }
    }
    flags
}

fn seed(
    module: &hir::Module,
    body: &GirBody,
    table: &PassingTable,
    flags: &mut [u8],
    edges: &mut Vec<(u32, u32, bool)>,
) {
    mark(flags, 0, ExportFlags::ESCAPE);
    for statement in &body.statements {
        match &statement.kind {
            StatementKind::Assign(dest, rvalue) => {
                seed_assign(module, body, table, dest, rvalue, flags, edges);
            }
            StatementKind::GcWrite { value, .. } => mark_operand(flags, value, ExportFlags::ALIAS),
            StatementKind::ResourceAction { place, .. } => {
                mark_place(flags, *place, ExportFlags::RESOURCE);
            }
            _ => {}
        }
    }
    seed_terminators(body, flags);
}

fn seed_assign(
    module: &hir::Module,
    body: &GirBody,
    table: &PassingTable,
    dest: &Place,
    rvalue: &Rvalue,
    flags: &mut [u8],
    edges: &mut Vec<(u32, u32, bool)>,
) {
    match rvalue {
        Rvalue::Use(operand) | Rvalue::Cast { operand, .. } | Rvalue::DynErase { operand, .. } => {
            connect(body, table, dest, operand, false, edges);
        }
        Rvalue::ValueCopy(src) | Rvalue::CowSnapshot(src) => {
            connect_places(body, table, dest, src, false, edges);
        }
        Rvalue::Ref(src) => {
            connect_places(body, table, dest, src, true, edges);
            if body.locals[src.local.index()].address_taken {
                mark_place(flags, *src, 0);
            }
        }
        Rvalue::AllocObject { .. } | Rvalue::AllocArray { .. } => {
            mark_place(flags, *dest, 0);
        }
        // 通道与协程创建是跨 owner 共享对象：本身永不进入 TurnRegion，也使宿主值成为发布值。
        Rvalue::Intrinsic {
            op: IntrinsicOp::ChanNew | IntrinsicOp::Spawn(_),
            ..
        } => mark_place(flags, *dest, ExportFlags::PUBLISH | ExportFlags::ESCAPE),
        // 闭包值的环境持有被捕获槽的地址：捕获源槽因此按逃逸处理，不能选 TurnRegion。
        Rvalue::FunctionValue(candidate) => {
            mark_captures(module, body, candidate.definition, flags);
        }
        Rvalue::Aggregate {
            kind: AggregateKind::Closure(definition) | AggregateKind::Coroutine(definition),
            ..
        } => mark_captures(module, body, *definition, flags),
        _ => {}
    }
}

/// 标记 `definition` 的 owner 在本 body 中捕获的源槽。
///
/// 闭包环境持有被捕获槽的地址，而捕获关系不在本 body 的语句中，因此只能由 HIR owner 的捕获
/// 表回查；这些局部必须按逃逸处理。
fn mark_captures(module: &hir::Module, body: &GirBody, definition: hir::DefId, flags: &mut [u8]) {
    let Some(owner) = module
        .owners
        .iter()
        .find(|owner| owner.definition == definition)
    else {
        return;
    };
    for capture in &owner.captures {
        if capture.owner != body.owner {
            continue;
        }
        if let Some(local) = body
            .locals
            .iter()
            .position(|local| local.hir_local == Some(capture.source))
        {
            mark(
                flags,
                local as u32,
                ExportFlags::ESCAPE | ExportFlags::ALIAS,
            );
        }
    }
}

fn seed_terminators(body: &GirBody, flags: &mut [u8]) {
    for block in &body.blocks {
        match &block.terminator {
            Terminator::Return => mark(flags, 0, ExportFlags::ESCAPE),
            Terminator::Call {
                callee,
                args,
                call_kind,
                ..
            } => seed_call(callee, args, *call_kind, flags),
            Terminator::Suspend { reason, .. } => seed_suspend(reason, flags),
            Terminator::SelectCommit { cases, .. } => {
                for case in &body.select_cases[cases.start as usize..cases.end as usize] {
                    seed_select(&case.operation, flags);
                }
            }
            _ => {}
        }
    }
}

fn seed_call(callee: &Callee, args: &[Operand], kind: CallKind, flags: &mut [u8]) {
    let flag = match kind {
        CallKind::ForeignBridge
        | CallKind::ForeignBridgeDirtyCpu
        | CallKind::ForeignLeaf { .. } => ExportFlags::FOREIGN,
        CallKind::Managed => match callee {
            Callee::Value(_) | Callee::Dynamic(_) => ExportFlags::UNKNOWN,
            Callee::Builtin(_) => 0,
            Callee::Dispatch(_) => 0,
        },
    };
    for arg in args {
        mark_operand(flags, arg, flag | ExportFlags::ESCAPE);
    }
}

fn seed_suspend(reason: &SuspendReason, flags: &mut [u8]) {
    match reason {
        SuspendReason::ChanSend { value, .. } => {
            mark_operand(flags, value, ExportFlags::CHANNEL_SEND);
        }
        SuspendReason::ChanRecv { .. } | SuspendReason::JoinWait { .. } | SuspendReason::Yield => {}
    }
}

fn seed_select(operation: &super::body::SelectOperation, flags: &mut [u8]) {
    match operation {
        super::body::SelectOperation::Send { value, .. } => {
            mark_operand(flags, value, ExportFlags::CHANNEL_SEND);
        }
        super::body::SelectOperation::Recv { .. } | super::body::SelectOperation::Wait { .. } => {}
    }
}

fn connect(
    body: &GirBody,
    table: &PassingTable,
    dest: &Place,
    operand: &Operand,
    by_ref: bool,
    edges: &mut Vec<(u32, u32, bool)>,
) {
    let (Operand::Copy(src) | Operand::MoveInternal(src)) = operand else {
        return;
    };
    connect_places(body, table, dest, src, by_ref, edges);
}

fn connect_places(
    body: &GirBody,
    table: &PassingTable,
    dest: &Place,
    src: &Place,
    by_ref: bool,
    edges: &mut Vec<(u32, u32, bool)>,
) {
    let class = table.class(body.locals[src.local.index()].ty);
    if by_ref || class.heap_effect() || class.has_identity() {
        edges.push((src.local.0, dest.local.0, by_ref));
    }
}

fn propagate(flags: &mut [u8], edges: &[(u32, u32, bool)]) {
    let mut changed = true;
    while changed {
        changed = false;
        for &(src, dest, by_ref) in edges {
            let dest_flags = flags[dest as usize];
            let add = if by_ref {
                ref_export(dest_flags)
            } else {
                dest_flags
            };
            let old = flags[src as usize];
            let next = old | add;
            if next != old {
                flags[src as usize] = next;
                changed = true;
            }
            if !by_ref {
                let src_flags = flags[src as usize];
                let old = flags[dest as usize];
                let next = old | src_flags;
                if next != old {
                    flags[dest as usize] = next;
                    changed = true;
                }
            }
        }
    }
}

fn ref_export(dest: u8) -> u8 {
    let mut out = 0;
    if dest
        & (ExportFlags::ESCAPE | ExportFlags::PUBLISH | ExportFlags::FOREIGN | ExportFlags::UNKNOWN)
        != 0
    {
        out |= ExportFlags::ESCAPE;
    }
    if dest & ExportFlags::PUBLISH != 0 {
        out |= ExportFlags::PUBLISH;
    }
    if dest & ExportFlags::FOREIGN != 0 {
        out |= ExportFlags::FOREIGN;
    }
    if dest & ExportFlags::UNKNOWN != 0 {
        out |= ExportFlags::UNKNOWN;
    }
    out
}

fn mark(flags: &mut [u8], local: u32, bit: u8) {
    if let Some(slot) = flags.get_mut(local as usize) {
        *slot |= bit;
    }
}

fn mark_place(flags: &mut [u8], place: Place, bit: u8) {
    mark(flags, place.local.0, bit);
}

fn mark_operand(flags: &mut [u8], operand: &Operand, bit: u8) {
    match operand {
        Operand::Copy(place) | Operand::MoveInternal(place) => mark_place(flags, *place, bit),
        _ => {}
    }
}

pub(crate) fn verify(world: &PlacementWorldV1) -> Result<(), Diagnostic> {
    for record in &world.records {
        check_turn(record.kind, record.proof, record.export)?;
    }
    for alloc in &world.allocs {
        check_turn(alloc.kind, alloc.proof, alloc.export)?;
    }
    verify_regions(world)?;
    Ok(())
}

/// 校验 region 计划的内部一致性：编号稠密、站点归属一致、出口排序去重。
fn verify_regions(world: &PlacementWorldV1) -> Result<(), Diagnostic> {
    let mut bodies: Vec<u32> = world.regions.iter().map(|plan| plan.body).collect();
    bodies.dedup();
    for body in bodies {
        let plans: Vec<&RegionPlan> = world
            .regions
            .iter()
            .filter(|plan| plan.body == body)
            .collect();
        for (expected, plan) in plans.iter().enumerate() {
            if plan.region as usize != expected {
                return Err(super::gir_error("region 编号必须按 body 稠密", None));
            }
            if plan.allocations == 0 {
                return Err(super::gir_error("region 必须至少有一个分配点", None));
            }
            if plan.exits.is_empty() {
                return Err(super::gir_error(
                    "region 必须有出口边界，否则无法结束 turn",
                    None,
                ));
            }
            if plan
                .exits
                .windows(2)
                .any(|pair| pair[0].block >= pair[1].block)
            {
                return Err(super::gir_error("region 出口边界必须严格升序", None));
            }
        }
        let sites = world
            .allocs
            .iter()
            .filter(|alloc| alloc.body == body && alloc.kind == PlacementKind::TurnRegion)
            .count() as u32;
        let declared: u32 = plans.iter().map(|plan| plan.allocations).sum();
        if sites != declared {
            return Err(super::gir_error(
                "TurnRegion 分配点数量与 region 计划不一致",
                None,
            ));
        }
    }
    for alloc in &world.allocs {
        if alloc.kind != PlacementKind::TurnRegion {
            if alloc.region.is_some() {
                return Err(super::gir_error("非 TurnRegion 站点不得携带 region", None));
            }
            continue;
        }
        let Some(region) = alloc.region else {
            return Err(super::gir_error("TurnRegion 站点必须归属 region", None));
        };
        if world.region(alloc.body, region).is_none() {
            return Err(super::gir_error(
                "TurnRegion 站点引用了不存在的 region",
                None,
            ));
        }
    }
    Ok(())
}

fn check_turn(kind: PlacementKind, proof: ProofStatus, export: u8) -> Result<(), Diagnostic> {
    if kind != PlacementKind::TurnRegion {
        return Ok(());
    }
    let forbidden = ExportFlags::UNKNOWN
        | ExportFlags::PUBLISH
        | ExportFlags::FOREIGN
        | ExportFlags::RESOURCE
        | ExportFlags::ALIAS;
    if proof != ProofStatus::Proved || export & forbidden != 0 {
        return Err(super::gir_error(
            "TurnRegion 只能在已证明私有、无 alias/resource/FFI 时选择",
            None,
        ));
    }
    Ok(())
}

#[allow(dead_code, reason = "测试读取 local 下标")]
pub(crate) fn record_of(
    world: &PlacementWorldV1,
    body: u32,
    local: LocalId,
) -> Option<&PlacementRecord> {
    world
        .records
        .iter()
        .find(|record| record.body == body && record.local == local.0)
}

/// 一个 body 的 region 决策结果。
struct RegionDecisions {
    plans: Vec<RegionPlan>,
}

/// region 判定所需的 CFG 事实。
///
/// `segment` 是「去掉 turn 边界块之后的弱连通分量」：同一 segment 内的 region 值不会跨过任何
/// 边界，因此该 segment 的出口边界就是 region 必须结束的位置，每条路径恰好经过其中一个。
struct RegionFacts {
    /// block → segment 编号；边界 block 为 `None`。
    segment: Vec<Option<u32>>,
    /// segment → 出口边界 block（严格升序）。
    exits: Vec<Vec<u32>>,
    /// 每个 block 的支配者位集，按 `words` 分列。
    dominators: Vec<u64>,
    words: usize,
    /// 每个 block 是否从入口可达。
    reachable: Vec<bool>,
    /// 语句下标 → 所属 block。
    statement_block: Vec<u32>,
}

impl RegionFacts {
    fn analyze(body: &GirBody) -> Self {
        let blocks = body.blocks.len();
        let boundaries = turn_boundaries(body);
        let mut boundary: Vec<Option<TurnBoundaryKind>> = vec![None; blocks];
        for entry in &boundaries {
            boundary[entry.block.index()] = Some(entry.kind);
        }
        let mut parent: Vec<u32> = (0..blocks as u32).collect();
        for (index, block) in body.blocks.iter().enumerate() {
            if boundary[index].is_some() {
                continue;
            }
            for successor in block.terminator.successors() {
                if boundary[successor.index()].is_some() {
                    continue;
                }
                union(&mut parent, index as u32, successor.index() as u32);
            }
        }
        let mut id_of_root: Vec<Option<u32>> = vec![None; blocks];
        let mut segment = vec![None; blocks];
        let mut segment_count = 0_u32;
        for index in 0..blocks {
            if boundary[index].is_some() {
                // 边界 block 自成一段：入口处的分配只在本 block 内使用，出口就是它自己。
                segment[index] = Some(segment_count);
                segment_count += 1;
                continue;
            }
            let root = find(&mut parent, index as u32);
            let id = match id_of_root[root as usize] {
                Some(id) => id,
                None => {
                    let id = segment_count;
                    segment_count += 1;
                    id_of_root[root as usize] = Some(id);
                    id
                }
            };
            segment[index] = Some(id);
        }
        let count = segment_count as usize;
        let mut exits = vec![Vec::new(); count];
        for (index, block) in body.blocks.iter().enumerate() {
            if boundary[index].is_none() {
                continue;
            }
            // 边界 block 既是自身段的出口，也是每个以非边界前驱进入它的段的出口。
            exits[segment[index].expect("边界 block 自成一段") as usize].push(index as u32);
            let range = block.predecessors.start as usize..block.predecessors.end as usize;
            for predecessor in &body.predecessors[range] {
                let source = predecessor.index();
                if boundary[source].is_some() {
                    continue;
                }
                if let Some(id) = segment.get(source).copied().flatten() {
                    exits[id as usize].push(index as u32);
                }
            }
        }
        for entry in exits.iter_mut() {
            entry.sort_unstable();
            entry.dedup();
        }
        let words = blocks.div_ceil(64).max(1);
        let reachable = reachable_blocks(body);
        let mut dominators = vec![u64::MAX; blocks * words];
        for index in 0..blocks {
            if !reachable[index] {
                dominators[index * words + index / 64] = 1 << (index % 64);
            }
        }
        let entry = body.entry.index();
        if reachable[entry] {
            for word in 0..words {
                dominators[entry * words + word] = 0;
            }
            dominators[entry * words + entry / 64] = 1 << (entry % 64);
            let mut changed = true;
            while changed {
                changed = false;
                for index in 0..blocks {
                    if index == entry || !reachable[index] {
                        continue;
                    }
                    let block = &body.blocks[index];
                    let range = block.predecessors.start as usize..block.predecessors.end as usize;
                    if range.is_empty() {
                        continue;
                    }
                    let mut next = vec![u64::MAX; words];
                    for predecessor in &body.predecessors[range] {
                        for word in 0..words {
                            next[word] &= dominators[predecessor.index() * words + word];
                        }
                    }
                    next[index / 64] |= 1 << (index % 64);
                    for word in 0..words {
                        if dominators[index * words + word] != next[word] {
                            dominators[index * words + word] = next[word];
                            changed = true;
                        }
                    }
                }
            }
        }
        let mut statement_block = vec![0_u32; body.statements.len()];
        for (index, block) in body.blocks.iter().enumerate() {
            for offset in block.statements.start..block.statements.end {
                statement_block[offset as usize] = index as u32;
            }
        }
        Self {
            segment,
            exits,
            dominators,
            words,
            reachable,
            statement_block,
        }
    }

    fn dominates(&self, dominator: u32, block: u32) -> bool {
        let dominator = dominator as usize;
        let block = block as usize;
        if !self.reachable[block] || !self.reachable[dominator] {
            return false;
        }
        self.dominators[block * self.words + dominator / 64] >> (dominator % 64) & 1 == 1
    }
}

fn find(parent: &mut [u32], mut node: u32) -> u32 {
    while parent[node as usize] != node {
        parent[node as usize] = parent[parent[node as usize] as usize];
        node = parent[node as usize];
    }
    node
}

fn union(parent: &mut [u32], left: u32, right: u32) {
    let left = find(parent, left);
    let right = find(parent, right);
    if left != right {
        parent[right as usize] = left;
    }
}

/// 从入口块出发的可达块。
fn reachable_blocks(body: &GirBody) -> Vec<bool> {
    let mut reachable = vec![false; body.blocks.len()];
    let mut stack = vec![body.entry];
    while let Some(block) = stack.pop() {
        if std::mem::replace(&mut reachable[block.index()], true) {
            continue;
        }
        for successor in body.blocks[block.index()].terminator.successors() {
            if !reachable[successor.index()] {
                stack.push(successor);
            }
        }
    }
    reachable
}

/// 计算一个 body 的 region 计划与站点归属。
///
#[expect(
    clippy::too_many_arguments,
    reason = "region 判定需要 body、分类表、导出位与传播图共同参与"
)]
fn region_decisions(
    module: &hir::Module,
    body_index: u32,
    body: &GirBody,
    table: &PassingTable,
    flags: &[u8],
    edges: &[(u32, u32, bool)],
    body_unknown: bool,
    allocs: &mut Vec<AllocPlacement>,
) -> RegionDecisions {
    let mut site_region = vec![None; body.statements.len()];
    let mut plans = Vec::new();
    let facts = RegionFacts::analyze(body);
    if facts.exits.is_empty() {
        // 无 region 出口：仍为分配点建立记录，但全部回退到 LocalHeap/SharedHeap。
        for (index, statement) in body.statements.iter().enumerate() {
            let StatementKind::Assign(place, rvalue) = &statement.kind else {
                continue;
            };
            if !is_alloc(module, rvalue) {
                continue;
            }
            let class = table.class(place_ty(body, *place));
            let export = flags.get(place.local.index()).copied().unwrap_or(0) | class_export(class);
            let (kind, proof) = place_alloc(class, export, body_unknown);
            let (kind, proof) = if kind == PlacementKind::TurnRegion {
                let kind = if export & ExportFlags::CHANNEL_SEND != 0 {
                    PlacementKind::SharedHeap
                } else {
                    PlacementKind::LocalHeap
                };
                (kind, ProofStatus::Proved)
            } else {
                (kind, proof)
            };
            allocs.push(AllocPlacement {
                body: body_index,
                statement: index as u32,
                kind,
                proof,
                export,
                region: None,
            });
        }
        return RegionDecisions { plans };
    }
    let liveness = Liveness::analyze(body);
    let adjacency = adjacency(body.locals.len(), edges);
    let words = body.locals.len().div_ceil(64).max(1);
    let mut per_segment: Vec<Vec<(u32, u32, u32)>> = vec![Vec::new(); facts.exits.len()];
    for (index, statement) in body.statements.iter().enumerate() {
        let StatementKind::Assign(place, rvalue) = &statement.kind else {
            continue;
        };
        if !is_alloc(module, rvalue) {
            continue;
        }
        let block = facts.statement_block[index];
        let Some(segment) = facts.segment[block as usize] else {
            continue;
        };
        let class = table.class(place_ty(body, *place));
        let export = flags.get(place.local.index()).copied().unwrap_or(0) | class_export(class);
        let (kind, _) = place_alloc(class, export, body_unknown);
        if kind != PlacementKind::TurnRegion {
            continue;
        }
        per_segment[segment as usize].push((index as u32, place.local.0, block));
    }
    for (segment, sites) in per_segment.iter().enumerate() {
        if sites.is_empty() {
            continue;
        }
        let exits = &facts.exits[segment];
        if exits.is_empty() {
            continue;
        }
        // 每个站点必须支配全部出口，否则某些到达出口的路径上没有 RegionAlloc 却要 publish。
        if sites
            .iter()
            .any(|(_, _, block)| exits.iter().any(|exit| !facts.dominates(*block, *exit)))
        {
            continue;
        }
        let mut reachable = vec![0_u64; words];
        for (_, local, _) in sites {
            reachable_from(&adjacency, *local, &mut reachable);
        }
        let mut plan_exits = Vec::with_capacity(exits.len());
        let mut conflict = false;
        for exit in exits {
            let terminator = &body.blocks[*exit as usize].terminator;
            let mut reads = vec![0_u64; words];
            terminator_reads_into(body, terminator, &mut reads);
            and_with(&mut reads, &reachable);
            let mut sends = vec![0_u64; words];
            send_values_into(body, terminator, &mut sends);
            and_with(&mut sends, &reachable);
            let live_after = (0..body.locals.len()).any(|local| {
                live_at(&reachable, local)
                    && liveness.live_out(BlockId(*exit), LocalId(local as u32))
            });
            let nonempty = reads.iter().any(|word| *word != 0);
            if nonempty && is_subset(&reads, &sends) && !live_after {
                plan_exits.push(RegionExit {
                    block: *exit,
                    transfer: true,
                });
            } else if nonempty || live_after {
                conflict = true;
                break;
            } else {
                plan_exits.push(RegionExit {
                    block: *exit,
                    transfer: false,
                });
            }
        }
        if conflict {
            continue;
        }
        let gate = ExportFlags::UNKNOWN
            | ExportFlags::PUBLISH
            | ExportFlags::FOREIGN
            | ExportFlags::RESOURCE
            | ExportFlags::ALIAS;
        let mut export = 0;
        for (_, local, _) in sites {
            export |= flags.get(*local as usize).copied().unwrap_or(0) & gate;
        }
        let region = plans.len() as u32;
        for (statement, _, _) in sites {
            site_region[*statement as usize] = Some(region);
        }
        plans.push(RegionPlan {
            body: body_index,
            region,
            allocations: sites.len() as u32,
            export,
            exits: plan_exits,
        });
    }
    // 收集所有分配点记录（TurnRegion 以外的分配在此处写入，TurnRegion 站点已写入 site_region）。
    for (index, statement) in body.statements.iter().enumerate() {
        let StatementKind::Assign(place, rvalue) = &statement.kind else {
            continue;
        };
        if !is_alloc(module, rvalue) {
            continue;
        }
        let class = table.class(place_ty(body, *place));
        let export = flags.get(place.local.index()).copied().unwrap_or(0) | class_export(class);
        let (kind, proof) = place_alloc(class, export, body_unknown);
        let region = site_region[index];
        // 出口整体移交所有权时，分配点带上 TRANSFER：dump 与契约需求据此区分「turn 结束时
        // 整区 reset」与「移交下一个 owner」两种生命周期。
        let export = match region {
            Some(region) => {
                let transfer = plans
                    .iter()
                    .find(|plan| plan.body == body_index && plan.region == region)
                    .is_some_and(|plan| plan.exits.iter().any(|exit| exit.transfer));
                if transfer {
                    export | ExportFlags::TRANSFER
                } else {
                    export
                }
            }
            None => export,
        };
        // 没有 region 计划时不得留下 TurnRegion：私有性成立但无法证明 turn 边界安全。
        // channel send 读取过的值跨 owner 且保留身份，退回 SharedHeap；其余退回 LocalHeap。
        let (kind, proof) = if kind == PlacementKind::TurnRegion && region.is_none() {
            let kind = if export & ExportFlags::CHANNEL_SEND != 0 {
                PlacementKind::SharedHeap
            } else {
                PlacementKind::LocalHeap
            };
            (kind, ProofStatus::Proved)
        } else {
            (kind, proof)
        };
        allocs.push(AllocPlacement {
            body: body_index,
            statement: index as u32,
            kind,
            proof,
            export,
            region: region.filter(|_| kind == PlacementKind::TurnRegion),
        });
    }
    RegionDecisions { plans }
}

/// region 派生指针在 local 之间的正向传播图；by-ref 与 by-value 边都跟随。
struct Adjacency {
    offsets: Vec<u32>,
    targets: Vec<u32>,
}

fn adjacency(locals: usize, edges: &[(u32, u32, bool)]) -> Adjacency {
    let mut offsets = vec![0_u32; locals + 1];
    for &(source, _, _) in edges {
        if (source as usize) < locals {
            offsets[source as usize + 1] += 1;
        }
    }
    for index in 0..locals {
        offsets[index + 1] += offsets[index];
    }
    let mut cursor = offsets.clone();
    let mut targets = vec![0_u32; edges.len()];
    for &(source, destination, _) in edges {
        if (source as usize) < locals {
            let slot = &mut cursor[source as usize];
            targets[*slot as usize] = destination;
            *slot += 1;
        }
    }
    Adjacency { offsets, targets }
}

/// 从 `root` 出发标记全部可达 local。
fn reachable_from(adjacency: &Adjacency, root: u32, out: &mut [u64]) {
    let mut stack = vec![root];
    set_bit(out, root as usize);
    while let Some(node) = stack.pop() {
        let start = adjacency.offsets[node as usize] as usize;
        let end = adjacency.offsets[node as usize + 1] as usize;
        for &target in &adjacency.targets[start..end] {
            if !get_bit(out, target as usize) {
                set_bit(out, target as usize);
                stack.push(target);
            }
        }
    }
}

fn get_bit(table: &[u64], index: usize) -> bool {
    table
        .get(index / 64)
        .is_some_and(|word| word >> (index % 64) & 1 == 1)
}

fn set_bit(table: &mut [u64], index: usize) {
    if let Some(word) = table.get_mut(index / 64) {
        *word |= 1 << (index % 64);
    }
}

fn live_at(table: &[u64], index: usize) -> bool {
    get_bit(table, index)
}

fn and_with(table: &mut [u64], mask: &[u64]) {
    for (word, mask) in table.iter_mut().zip(mask) {
        *word &= *mask;
    }
}

fn is_subset(left: &[u64], right: &[u64]) -> bool {
    left.iter()
        .zip(right)
        .all(|(left, right)| left & !right == 0)
}

/// terminator 读取的全部 local。
fn terminator_reads_into(body: &GirBody, terminator: &Terminator, out: &mut [u64]) {
    let operand = |operand: &Operand, out: &mut [u64]| {
        if let Operand::Copy(place) | Operand::MoveInternal(place) = operand {
            set_bit(out, place.local.index());
        }
    };
    match terminator {
        Terminator::SwitchInt { value, .. } => operand(value, out),
        Terminator::Call { callee, args, .. } => {
            if let Callee::Value(value) = callee {
                operand(value, out);
            }
            for argument in args {
                operand(argument, out);
            }
        }
        Terminator::Panic { payload, .. } => operand(payload, out),
        Terminator::Suspend { reason, .. } => match reason {
            SuspendReason::ChanSend { channel, value } => {
                operand(channel, out);
                operand(value, out);
            }
            SuspendReason::ChanRecv { channel } => operand(channel, out),
            SuspendReason::JoinWait { join } => operand(join, out),
            SuspendReason::Yield => {}
        },
        Terminator::SelectCommit { cases, index, .. } => {
            set_bit(out, index.index());
            for case in &body.select_cases[cases.start as usize..cases.end as usize] {
                match &case.operation {
                    SelectOperation::Send { channel, value } => {
                        operand(channel, out);
                        operand(value, out);
                    }
                    SelectOperation::Recv { channel } => operand(channel, out),
                    SelectOperation::Wait { join } => operand(join, out),
                }
            }
        }
        Terminator::Goto { .. }
        | Terminator::Return
        | Terminator::ResumePanic
        | Terminator::Abort
        | Terminator::Unreachable => {}
    }
}

/// terminator 在 channel send 中传递的值 local。
fn send_values_into(body: &GirBody, terminator: &Terminator, out: &mut [u64]) {
    let operand = |operand: &Operand, out: &mut [u64]| {
        if let Operand::Copy(place) | Operand::MoveInternal(place) = operand {
            set_bit(out, place.local.index());
        }
    };
    match terminator {
        Terminator::Suspend {
            reason: SuspendReason::ChanSend { value, .. },
            ..
        } => operand(value, out),
        Terminator::SelectCommit { cases, .. } => {
            for case in &body.select_cases[cases.start as usize..cases.end as usize] {
                if let SelectOperation::Send { value, .. } = &case.operation {
                    operand(value, out);
                }
            }
        }
        _ => {}
    }
}
