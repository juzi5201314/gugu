//! 逃逸分析与 TurnRegion / LocalHeap / SharedHeap placement。
//!
//! 分析未知时只选择能保留值、引用、身份和 resource 语义的路径，禁止 TurnRegion。
use super::body::{
    CallKind, Callee, IntrinsicOp, LocalId, LocalKind, Operand, Place, Rvalue, StatementKind,
    SuspendReason, Terminator,
};
use super::passing::{PassingClass, PassingTable};
use super::{GirBody, GirWorldV1, WORLD_SCHEMA, world_fingerprint};
use crate::frontend::analysis::{AnalysisWorldV1, ProofStatus};
use crate::frontend::hir::{self, TypeId};
use crate::frontend::semantics::query::restore_errors;
use crate::query::{QueryEngine, QueryKey, QueryKind};
use crate::{Diagnostic, SourceMap};
use serde::{Deserialize, Serialize};

pub(crate) const PLACEMENT_SCHEMA: u32 = 1;

/// export summary 位：逃逸、别名、资源、FFI、发布、未知。上界 6 位。
pub(crate) struct ExportFlags;
impl ExportFlags {
    pub(crate) const ESCAPE: u8 = 1;
    pub(crate) const ALIAS: u8 = 2;
    pub(crate) const RESOURCE: u8 = 4;
    pub(crate) const FOREIGN: u8 = 8;
    pub(crate) const PUBLISH: u8 = 16;
    pub(crate) const UNKNOWN: u8 = 32;
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AllocPlacement {
    pub(crate) body: u32,
    pub(crate) statement: u32,
    pub(crate) kind: PlacementKind,
    pub(crate) proof: ProofStatus,
    pub(crate) export: u8,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlacementWorldV1 {
    pub(crate) schema: u32,
    pub(crate) records: Vec<PlacementRecord>,
    pub(crate) allocs: Vec<AllocPlacement>,
    pub(crate) fingerprint: [u8; 32],
}

impl PlacementWorldV1 {
    pub(crate) fn empty() -> Self {
        Self {
            schema: PLACEMENT_SCHEMA,
            records: Vec::new(),
            allocs: Vec::new(),
            fingerprint: fingerprint(&[], &[]),
        }
    }

    pub(crate) fn counts(&self) -> PlacementCounts {
        let mut counts = PlacementCounts::default();
        for record in &self.records {
            tally(&mut counts, record.kind);
        }
        for alloc in &self.allocs {
            tally(&mut counts, alloc.kind);
        }
        counts
    }
}

fn tally(counts: &mut PlacementCounts, kind: PlacementKind) {
    counts.total += 1;
    match kind {
        PlacementKind::TurnRegion => counts.turn_region += 1,
        PlacementKind::LocalHeap => counts.local_heap += 1,
        PlacementKind::SharedHeap => counts.shared_heap += 1,
        _ => {}
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PlacementCounts {
    pub(crate) total: u32,
    pub(crate) turn_region: u32,
    pub(crate) local_heap: u32,
    pub(crate) shared_heap: u32,
}

fn fingerprint(records: &[PlacementRecord], allocs: &[AllocPlacement]) -> [u8; 32] {
    let bytes = serde_json::to_vec(&(records, allocs)).expect("placement 可序列化");
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
    for (body_index, body) in world.bodies.iter().enumerate() {
        let flags = escape(module, body, &table);
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
        collect_allocs(
            body_index as u32,
            body,
            &table,
            &flags,
            body_unknown,
            &mut allocs,
        );
    }
    records.sort_by_key(|record| (record.body, record.local));
    allocs.sort_by_key(|alloc| (alloc.body, alloc.statement));
    let digest = fingerprint(&records, &allocs);
    PlacementWorldV1 {
        schema: PLACEMENT_SCHEMA,
        records,
        allocs,
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

fn collect_allocs(
    body: u32,
    gir: &GirBody,
    table: &PassingTable,
    flags: &[u8],
    body_unknown: bool,
    allocs: &mut Vec<AllocPlacement>,
) {
    for (index, statement) in gir.statements.iter().enumerate() {
        let StatementKind::Assign(place, rvalue) = &statement.kind else {
            continue;
        };
        if !is_alloc(rvalue) {
            continue;
        }
        let class = table.class(place_ty(gir, *place));
        let export = flags.get(place.local.index()).copied().unwrap_or(0) | class_export(class);
        let (kind, proof) = place_alloc(class, export, body_unknown);
        allocs.push(AllocPlacement {
            body,
            statement: index as u32,
            kind,
            proof,
            export,
        });
    }
}

fn is_alloc(rvalue: &Rvalue) -> bool {
    matches!(
        rvalue,
        Rvalue::AllocObject { .. }
            | Rvalue::AllocArray { .. }
            | Rvalue::Intrinsic {
                op: IntrinsicOp::ChanNew | IntrinsicOp::Spawn(_),
                ..
            }
    )
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
fn escape(module: &hir::Module, body: &GirBody, table: &PassingTable) -> Vec<u8> {
    let mut flags = vec![0u8; body.locals.len()];
    let mut edges = Vec::new();
    seed(body, table, &mut flags, &mut edges);
    propagate(&mut flags, &edges);
    for (index, local) in body.locals.iter().enumerate() {
        if table.class(local.ty).has_resource() {
            flags[index] |= ExportFlags::RESOURCE;
        }
        if local.kind == LocalKind::Argument && table.class(local.ty).has_identity() {
            flags[index] |= ExportFlags::ALIAS;
        }
    }
    let _ = module;
    flags
}

fn seed(body: &GirBody, table: &PassingTable, flags: &mut [u8], edges: &mut Vec<(u32, u32, bool)>) {
    mark(flags, 0, ExportFlags::ESCAPE);
    for statement in &body.statements {
        match &statement.kind {
            StatementKind::Assign(dest, rvalue) => {
                seed_assign(body, table, dest, rvalue, flags, edges);
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
        Rvalue::Intrinsic {
            op: IntrinsicOp::ChanNew | IntrinsicOp::Spawn(_),
            ..
        } => mark_place(flags, *dest, 0),
        _ => {}
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
            mark_operand(flags, value, ExportFlags::PUBLISH | ExportFlags::ESCAPE);
        }
        SuspendReason::ChanRecv { .. } | SuspendReason::JoinWait { .. } | SuspendReason::Yield => {}
    }
}

fn seed_select(operation: &super::body::SelectOperation, flags: &mut [u8]) {
    match operation {
        super::body::SelectOperation::Send { value, .. } => {
            mark_operand(flags, value, ExportFlags::PUBLISH | ExportFlags::ESCAPE);
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
pub(crate) fn record_of<'a>(
    world: &'a PlacementWorldV1,
    body: u32,
    local: LocalId,
) -> Option<&'a PlacementRecord> {
    world
        .records
        .iter()
        .find(|record| record.body == body && record.local == local.0)
}
