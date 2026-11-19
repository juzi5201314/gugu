//! Implicit runtime operations use source-checked integer lanes, never C imports.
use super::{Builder, signature};
use crate::{
    a64::{Error, machine::Item, runtime},
    lir::body::{Body, Instruction, Op, Type, ValueId, ValueType},
};

pub(crate) fn handles(op: &Op) -> bool {
    runtime::proof_marker(op) || runtime::operation_name(op).is_some()
}

fn region(op: &Op) -> Option<Vec<u64>> {
    Some(match op {
        Op::RegionPublish { region, export } => vec![u64::from(*region), u64::from(*export)],
        Op::RegionReset { region }
        | Op::PromoteManaged { region }
        | Op::RegionTransfer { region } => vec![u64::from(*region)],
        _ => return None,
    })
}

pub(crate) fn lower(
    body: &Body,
    instruction: &Instruction,
    block: u32,
    site: u32,
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    let catalog = b
        .runtime_catalog
        .ok_or_else(|| crate::a64::error("A64 operation requires a runtime source catalog"))?;
    let args = body.args(&instruction.arguments);
    let results = body.args(&instruction.results);
    if instruction.memory.is_none() {
        return Err(crate::a64::error(
            "A64 runtime operation requires memory sequencing",
        ));
    }
    if runtime::proof_marker(&instruction.op) {
        return Ok(());
    }
    let name = runtime::operation_name(&instruction.op)
        .ok_or_else(|| crate::a64::error("unbound A64 runtime operation"))?;
    let parameters = if let Some(values) = region(&instruction.op) {
        signature(body, args, results, &[], &[])?;
        vec![ValueType::scalar(Type::I64); values.len()]
    } else {
        args.iter().map(|id| body.values[id.index()].kind).collect()
    };
    let outputs = results
        .iter()
        .map(|id| body.values[id.index()].kind)
        .collect::<Vec<_>>();
    let (callee, target) = catalog.operation_target(name, &parameters, &outputs)?;
    if parameters.len() > 8 {
        return Err(crate::a64::error(
            "A64 implicit runtime stack arguments require an outgoing-area contract",
        ));
    }
    if instruction.safepoint.is_none() && !callee.poll_interface.leaf {
        return Err(crate::a64::error(
            "A64 runtime operation without a map requires a proven leaf definition",
        ));
    }
    let arguments = if let Some(values) = region(&instruction.op) {
        values
            .into_iter()
            .map(|value| b.constant(value))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        args.iter().map(|value| value.0).collect()
    };
    b.push(Item::Call {
        target,
        arguments,
        results: results.iter().map(|value| value.0).collect(),
        suspend: instruction.op.safepoint_kind() == Some(crate::lir::body::SafepointKind::Suspend),
        safepoint: instruction.safepoint.map(|_| (block, site)),
    });
    for ValueId(value) in results {
        let ty = body.values[*value as usize].kind.ty;
        b.normalize(*value, *value, super::bits(ty)?, false)?;
    }
    Ok(())
}
