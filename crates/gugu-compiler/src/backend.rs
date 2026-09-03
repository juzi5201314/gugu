use crate::{ir::IrModule, target::TargetName};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BackendPlan {
    pub(crate) target: TargetName,
    pub(crate) entry: &'static str,
    pub(crate) function_count: u32,
}

pub(crate) fn plan(target: TargetName, ir: &IrModule) -> Option<BackendPlan> {
    let entry = ir
        .entry
        .map(|function_id| ir.functions[function_id.index()].name)?;
    Some(BackendPlan {
        target,
        entry,
        function_count: ir.functions.len() as u32,
    })
}
