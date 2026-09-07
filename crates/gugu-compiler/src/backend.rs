use crate::{frontend::hir::Validated, frontend::mono::MonoWorldV1, target::TargetName};

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
}

pub(crate) fn plan(
    target: TargetName,
    hir: &Validated,
    mono: &MonoWorldV1,
    runtime_checks_elided_count: u32,
) -> Option<BackendPlan> {
    let module = hir.module();
    let entry = module.entry?;
    Some(BackendPlan {
        target,
        entry: module.definitions[entry.index()].name.clone(),
        function_count: mono.instances.len() as u32,
        semantic_fingerprint: hir.fingerprint(),
        runtime_checks_elided_count,
        mono_instance_count: mono.instances.len() as u32,
        mono_root_count: mono.roots.len() as u32,
        mono_graph_fingerprint: mono.graph_fingerprint,
    })
}
