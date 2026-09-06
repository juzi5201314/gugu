use crate::{
    frontend::hir::{DefinitionKind, Validated},
    target::TargetName,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BackendPlan {
    pub(crate) target: TargetName,
    pub(crate) entry: String,
    pub(crate) function_count: u32,
    pub(crate) semantic_fingerprint: [u8; 32],
    pub(crate) runtime_checks_elided_count: u32,
}

pub(crate) fn plan(
    target: TargetName,
    hir: &Validated,
    runtime_checks_elided_count: u32,
) -> Option<BackendPlan> {
    let module = hir.module();
    let entry = module.entry?;
    Some(BackendPlan {
        target,
        entry: module.definitions[entry.index()].name.clone(),
        function_count: module
            .owners
            .iter()
            .filter(|owner| {
                matches!(
                    module.definitions[owner.definition.index()].kind,
                    DefinitionKind::Function | DefinitionKind::Closure | DefinitionKind::Async
                )
            })
            .count() as u32,
        semantic_fingerprint: hir.fingerprint(),
        runtime_checks_elided_count,
    })
}
