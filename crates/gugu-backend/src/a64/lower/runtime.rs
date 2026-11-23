//! Implicit runtime operations use source-checked integer lanes, never C imports.
use super::{Builder, signature};
use crate::{
    a64::{Error, abi, machine::Item, runtime},
    gir::placement::PlacementKind,
    lir::body::{Body, Instruction, Op, Type, ValueId, ValueType},
    x64::mangle,
};

pub(crate) fn handles(op: &Op) -> bool {
    runtime::proof_marker(op)
        || runtime::operation_name(op).is_some()
        || matches!(
            op,
            Op::PlatformCall(_) | Op::GcAlloc { .. } | Op::RegionAlloc { .. }
        )
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

/// 隐含调用的点位与操作数上下文。
struct CallSite<'a> {
    body: &'a Body,
    instruction: &'a Instruction,
    block: u32,
    site: u32,
    args: &'a [ValueId],
    results: &'a [ValueId],
}

/// 与 x64 `slow_alloc` 相同的四通道慢路 ABI：type_id、payload 字节数、对齐、flags。
fn alloc(
    call: &CallSite<'_>,
    b: &mut Builder<'_>,
    descriptor: [u8; 32],
    align: u32,
    placement: PlacementKind,
    compressed: bool,
) -> Result<(), Error> {
    let body = call.body;
    let instruction = call.instruction;
    let block = call.block;
    let site = call.site;
    let args = call.args;
    let results = call.results;
    let code = match placement {
        PlacementKind::TurnRegion => 0u64,
        PlacementKind::LocalHeap => 1,
        PlacementKind::SharedHeap => 2,
        PlacementKind::Resource => 3,
        _ => {
            return Err(crate::a64::error(
                "A64 allocation placement is outside the slow-call ABI",
            ));
        }
    };
    if args.len() != 1 || results.len() != 1 {
        return Err(crate::a64::error("invalid A64 allocation signature"));
    }
    let bytes = args[0];
    let dest = results[0];
    for value in [bytes, dest] {
        if abi::class(body.values[value.index()].kind.ty) != abi::GPR {
            return Err(crate::a64::error(
                "A64 allocation lanes must be integer registers",
            ));
        }
    }
    let universe = b
        .universe
        .ok_or_else(|| crate::a64::error("A64 allocation requires a frozen type universe"))?;
    let type_id = universe
        .type_id(&descriptor)
        .ok_or_else(|| crate::a64::error("A64 allocation type is missing from the universe"))?;
    let glue = match placement {
        PlacementKind::TurnRegion => "gc_region_slow",
        PlacementKind::SharedHeap => "gc_shared_alloc",
        _ => "gc_alloc_slow",
    };
    let flags = u64::from(compressed) | (code << 1);
    let arguments = vec![
        b.constant(u64::from(type_id))?,
        bytes.0,
        b.constant(u64::from(align))?,
        b.constant(flags)?,
    ];
    b.push(Item::Call {
        target: mangle::mangle_runtime(glue),
        arguments,
        results: vec![dest.0],
        suspend: false,
        callee: None,
        safepoint: instruction.safepoint.map(|_| (block, site)),
        landing: None,
    });
    let ty = body.values[dest.index()].kind.ty;
    b.normalize(dest.0, dest.0, super::bits(ty)?, false)?;
    Ok(())
}

/// 机器片段/平台原语调用：操作数按序落在 x0-x7，不经 catalog 查 body。
fn machine_call(call: &CallSite<'_>, b: &mut Builder<'_>, name: &str) -> Result<(), Error> {
    let body = call.body;
    let instruction = call.instruction;
    let block = call.block;
    let site = call.site;
    let args = call.args;
    let results = call.results;
    for id in args.iter().chain(results.iter()) {
        if abi::class(body.values[id.index()].kind.ty) != abi::GPR {
            return Err(crate::a64::error(
                "A64 machine binding requires integer lanes",
            ));
        }
    }
    if args.len() > 8 {
        return Err(crate::a64::error(
            "A64 machine binding stack arguments require an outgoing-area contract",
        ));
    }
    abi::results(results.iter().map(|id| body.values[id.index()].kind.ty))?;
    b.push(Item::Call {
        target: mangle::mangle_runtime(name),
        arguments: args.iter().map(|value| value.0).collect(),
        results: results.iter().map(|value| value.0).collect(),
        suspend: instruction.op.safepoint_kind() == Some(crate::lir::body::SafepointKind::Suspend),
        callee: None,
        safepoint: instruction.safepoint.map(|_| (block, site)),
        landing: None,
    });
    for ValueId(value) in results {
        let ty = body.values[*value as usize].kind.ty;
        b.normalize(*value, *value, super::bits(ty)?, false)?;
    }
    Ok(())
}

pub(crate) fn lower(
    body: &Body,
    instruction: &Instruction,
    block: u32,
    site: u32,
    b: &mut Builder<'_>,
) -> Result<(), Error> {
    let args = body.args(&instruction.arguments);
    let results: Vec<crate::lir::body::ValueId> = body.results(instruction).collect();
    let results = results.as_slice();
    if instruction.memory.is_none() {
        return Err(crate::a64::error(
            "A64 runtime operation requires memory sequencing",
        ));
    }
    if runtime::proof_marker(&instruction.op) {
        return Ok(());
    }
    let call = CallSite {
        body,
        instruction,
        block,
        site,
        args,
        results,
    };
    if let Some(name) = runtime::machine_binding(&instruction.op) {
        return machine_call(&call, b, &name);
    }
    match &instruction.op {
        Op::GcAlloc {
            descriptor,
            align,
            placement,
            compressed,
        } => {
            return alloc(&call, b, *descriptor, *align, *placement, *compressed);
        }
        Op::RegionAlloc {
            descriptor, align, ..
        } => {
            return alloc(
                &call,
                b,
                *descriptor,
                *align,
                PlacementKind::TurnRegion,
                false,
            );
        }
        _ => {}
    }
    let catalog = b
        .runtime_catalog
        .ok_or_else(|| crate::a64::error("A64 operation requires a runtime source catalog"))?;
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
        callee: None,
        safepoint: instruction.safepoint.map(|_| (block, site)),
        landing: None,
    });
    for ValueId(value) in results {
        let ty = body.values[*value as usize].kind.ty;
        b.normalize(*value, *value, super::bits(ty)?, false)?;
    }
    Ok(())
}
