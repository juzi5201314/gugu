use crate::{
    frontend::gir::GirWorldV1, frontend::hir::Validated, frontend::mono::MonoWorldV1,
    target::TargetName,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BackendPlan {
    pub(crate) target: TargetName,
    pub(crate) entry: String,
    pub(crate) function_count: u32,
    pub(crate) semantic_fingerprint: [u8; 32],
    pub(crate) runtime_checks_elided_count: u32,
    pub(crate) mono_instance_count: u32,
    pub(crate) mono_root_count: u32,
    pub(crate) mono_graph_fingerprint: [u8; 32],
    pub(crate) type_id_count: u32,
    pub(crate) type_universe_fingerprint: [u8; 32],
    pub(crate) late_constant_count: u32,
    pub(crate) late_constants_fingerprint: [u8; 32],
    pub(crate) gir_body_count: u32,
    pub(crate) gir_block_count: u32,
    pub(crate) gir_statement_count: u32,
    pub(crate) gir_fingerprint: [u8; 32],
    pub(crate) placement_count: u32,
    pub(crate) turn_region_count: u32,
    pub(crate) local_heap_count: u32,
    pub(crate) shared_heap_count: u32,
    pub(crate) placement_fingerprint: [u8; 32],
}

pub(crate) fn plan(
    target: TargetName,
    hir: &Validated,
    mono: &MonoWorldV1,
    gir: &GirWorldV1,
    runtime_checks_elided_count: u32,
) -> Option<BackendPlan> {
    let module = hir.module();
    let entry = module.entry?;
    let placement = gir.placement.counts();
    Some(BackendPlan {
        target,
        entry: module.definitions[entry.index()].name.clone(),
        function_count: mono.instances.len() as u32,
        semantic_fingerprint: hir.fingerprint(),
        runtime_checks_elided_count,
        mono_instance_count: mono.instances.len() as u32,
        mono_root_count: mono.roots.len() as u32,
        mono_graph_fingerprint: mono.graph_fingerprint,
        type_id_count: mono.universe.records.len() as u32,
        type_universe_fingerprint: mono.universe.fingerprint,
        late_constant_count: mono.late.results.len() as u32,
        late_constants_fingerprint: mono.late.fingerprint,
        gir_body_count: gir.bodies.len() as u32,
        gir_block_count: gir.bodies.iter().map(|body| body.blocks.len() as u32).sum(),
        gir_statement_count: gir
            .bodies
            .iter()
            .map(|body| body.statements.len() as u32)
            .sum(),
        gir_fingerprint: gir.fingerprint,
        placement_count: placement.total,
        turn_region_count: placement.turn_region,
        local_heap_count: placement.local_heap,
        shared_heap_count: placement.shared_heap,
        placement_fingerprint: gir.placement.fingerprint,
    })
}
