//! Verified LIR to the internal integer leaf machine representation.
use super::{
    Error,
    inst::Operand,
    lower::{self, Builder},
    machine::{self, Block, Function, Item, MReg},
};
use crate::lir::body::{Body, Op, Terminator, Type, ValueId};
use std::ops::Range;

pub fn select(body: &Body) -> Result<Function, Error> {
    select_mode(body, true, false, None, None, None)
}

pub fn select_function(body: &Body) -> Result<Function, Error> {
    select_mode(body, false, false, None, None, None)
}

pub fn select_runtime(body: &Body) -> Result<Function, Error> {
    select_mode(body, false, true, None, None, None)
}

pub(crate) fn select_image(
    body: &Body,
    symbols: &[(crate::lir::body::Symbol, String)],
    runtime_catalog: Option<&super::runtime::Catalog<'_>>,
    universe: Option<&crate::late::universe::TypeUniverse>,
) -> Result<Function, Error> {
    select_mode(body, false, true, Some(symbols), runtime_catalog, universe)
}

fn select_mode(
    body: &Body,
    leaf: bool,
    runtime: bool,
    symbols: Option<&[(crate::lir::body::Symbol, String)]>,
    runtime_catalog: Option<&super::runtime::Catalog<'_>>,
    universe: Option<&crate::late::universe::TypeUniverse>,
) -> Result<Function, Error> {
    validate(body, leaf, runtime).map_err(|e| Error(format!("{} entry: {e}", body.name)))?;
    let count = body.blocks.len();
    let mut function = Function {
        name: body.name.clone(),
        entry: 0,
        checked_entry: runtime
            && body
                .instructions
                .iter()
                .any(|inst| matches!(inst.op, Op::StackCheck)),
        types: body.values.iter().map(|v| v.kind.ty).collect(),
        blocks: vec![
            Block {
                items: vec![],
                successors: vec![]
            };
            count + body.edges.len() + 1
        ],
    };
    let mut b = Builder {
        runtime_catalog,
        universe,
        symbols,
        function: &mut function,
        items: vec![],
        context: format!("{} incoming arguments", body.name),
    };
    entry(body, &mut b)?;
    if body.entry.index() != 0 {
        jump(&mut b, "b", vec![], body.entry.0 + 1)?;
    }
    b.function.blocks[0] = Block {
        items: b.items,
        successors: vec![body.entry.index() + 1],
    };
    for (index, block) in body.blocks.iter().enumerate() {
        let mut events = if runtime {
            body.root_inits
                .iter()
                .filter(|event| event.block.index() == index)
                .map(|e| (e.position, e.slot.0, e.bit, e.initialized))
                .collect::<Vec<_>>()
        } else {
            vec![]
        };
        events.sort_by_key(|event| (event.0, event.1, event.2));
        let mut cursor = 0;
        let mut b = Builder {
            runtime_catalog,
            universe,
            symbols,
            function: &mut function,
            items: vec![],
            context: format!("{} block {index} entry", body.name),
        };
        for site in block.instructions.clone() {
            for &(_, slot, bit, initialized) in crate::metadata::init_events_until(
                &events,
                &mut cursor,
                site - block.instructions.start,
            ) {
                b.push(Item::RootInit {
                    slot,
                    bit,
                    initialized,
                });
            }
            let instruction = &body.instructions[site as usize];
            b.context = format!(
                "{} block {index} site {site} {:?}",
                body.name, instruction.op
            );
            if runtime {
                match instruction.op {
                    Op::StackCheck => continue,
                    Op::SafepointPoll { .. } => {
                        b.push(Item::Poll {
                            block: index as u32,
                            instruction: site,
                        });
                        continue;
                    }
                    Op::Call(ref call) | Op::ForeignCall(ref call) => {
                        lower::call::lower_mode(
                            body,
                            call,
                            body.args(&instruction.arguments),
                            &body.results(&instruction).collect::<Vec<_>>(),
                            false,
                            lower::call::Mode::Runtime(
                                instruction.safepoint.map(|_| (index as u32, site)),
                            ),
                            None,
                            &mut b,
                        )
                        .map_err(|e| Error(format!("{}: {e}", b.context)))?;
                        continue;
                    }
                    _ => {}
                }
                if b.runtime_catalog.is_some() && lower::runtime::handles(&instruction.op) {
                    lower::runtime::lower(body, instruction, index as u32, site, &mut b)
                        .map_err(|e| Error(format!("{}: {e}", b.context)))?;
                    continue;
                }
            }
            lower::lower(body, instruction, &mut b)
                .map_err(|e| Error(format!("{}: {e}", b.context)))?;
        }
        for &(_, slot, bit, initialized) in crate::metadata::init_events_until(
            &events,
            &mut cursor,
            block.instructions.end - block.instructions.start,
        ) {
            b.push(Item::RootInit {
                slot,
                bit,
                initialized,
            });
        }
        b.context = format!(
            "{} block {index} terminator {:?}",
            body.name, block.terminator
        );
        let successors = terminator(body, block, index, count, &mut b)
            .map_err(|e| Error(format!("{}: {e}", b.context)))?;
        b.function.blocks[index + 1] = Block {
            items: b.items,
            successors,
        };
    }
    for (index, edge) in body.edges.iter().enumerate() {
        let context = format!("{} edge {index}", body.name);
        let pairs = body
            .args(&edge.arguments)
            .iter()
            .zip(body.params(edge.to))
            .filter(|(source, _)| body.values[source.index()].kind.ty != Type::Mem)
            .map(|(source, dest)| (MReg::Virtual(source.0), MReg::Virtual(dest.value.0)))
            .collect();
        let mut b = Builder {
            runtime_catalog,
            universe,
            symbols,
            function: &mut function,
            items: vec![],
            context,
        };
        b.push(Item::Copies(pairs));
        jump(&mut b, "b", vec![], edge.to.0 + 1)?;
        b.function.blocks[count + index + 1] = Block {
            items: b.items,
            successors: vec![edge.to.index() + 1],
        };
    }
    Ok(function)
}

fn entry(body: &Body, b: &mut Builder<'_>) -> Result<(), Error> {
    let params = body
        .params(body.entry)
        .iter()
        .filter(|p| body.values[p.value.index()].kind.ty != Type::Mem)
        .collect::<Vec<_>>();
    let (locations, _) =
        super::abi::arguments(params.iter().map(|p| body.values[p.value.index()].kind.ty))?;
    b.push(Item::Copies(
        params
            .iter()
            .zip(&locations)
            .filter_map(|(p, location)| match location {
                super::abi::Argument::Register(id) => {
                    Some((MReg::Physical(*id), MReg::Virtual(p.value.0)))
                }
                _ => None,
            })
            .collect(),
    ));
    for (param, location) in params.iter().zip(locations) {
        if let super::abi::Argument::Stack(offset) = location {
            b.push(Item::Incoming {
                dest: param.value.0,
                index: offset,
            });
        }
        let ty = body.values[param.value.index()].kind.ty;
        if super::abi::class(ty) == super::abi::FPR {
            continue;
        }
        let bits = lower::bits(ty)?;
        if bits < 64 {
            b.normalize(param.value.0, param.value.0, bits, false)?;
        }
    }
    Ok(())
}

fn jump(
    b: &mut Builder<'_>,
    form: &str,
    operands: Vec<machine::MOperand>,
    target: u32,
) -> Result<(), Error> {
    b.push(Item::Branch {
        form: machine::form(form)?,
        operands,
        target,
    });
    Ok(())
}

fn terminator(
    body: &Body,
    block: &crate::lir::body::Block,
    index: usize,
    count: usize,
    b: &mut Builder<'_>,
) -> Result<Vec<usize>, Error> {
    let edge = |id: crate::lir::body::EdgeId| count + id.index() + 1;
    match &block.terminator {
        Terminator::TailCall {
            call,
            arguments,
            memory,
        } => {
            if !values(body, arguments) || !kind(body, *memory, Type::Mem) {
                return Err(super::error("invalid tail call operands/memory"));
            }
            lower::call::lower(body, call, body.args(arguments), &[], true, b)?;
            Ok(vec![])
        }
        Terminator::Jump(to) => {
            jump(b, "b", vec![], edge(*to) as u32)?;
            Ok(vec![edge(*to)])
        }
        Terminator::Branch { condition, yes, no } => {
            jump(
                b,
                "cbnz.X",
                vec![machine::read(condition.0)],
                edge(*yes) as u32,
            )?;
            jump(b, "b", vec![], edge(*no) as u32)?;
            Ok(vec![edge(*yes), edge(*no)])
        }
        Terminator::Switch {
            value,
            cases,
            otherwise,
        } => {
            let mut successors = Vec::new();
            for (case, target) in &body.switch_cases[crate::lir::body::range(cases)] {
                let constant = b.constant(*case)?;
                b.emit(
                    "subs.X.shift",
                    vec![
                        machine::zero(),
                        machine::read(value.0),
                        machine::read(constant),
                        machine::fixed(Operand::Shift(super::inst::Shift::Lsl, 0)),
                    ],
                )?;
                jump(
                    b,
                    "b.cond",
                    vec![machine::fixed(Operand::Condition(0))],
                    edge(*target) as u32,
                )?;
                successors.push(edge(*target));
            }
            jump(b, "b", vec![], edge(*otherwise) as u32)?;
            successors.push(edge(*otherwise));
            Ok(successors)
        }
        Terminator::Return { values, .. } => {
            let locations = super::abi::results(
                body.args(values)
                    .iter()
                    .map(|value| body.values[value.index()].kind.ty),
            )?;
            b.push(Item::Copies(
                body.args(values)
                    .iter()
                    .zip(locations)
                    .map(|(value, id)| (MReg::Virtual(value.0), MReg::Physical(id)))
                    .collect(),
            ));
            b.push(Item::Return);
            Ok(vec![])
        }
        Terminator::Invoke {
            call,
            arguments,
            results,
            memory,
            normal,
            unwind,
            safepoint,
        } => {
            if !values(body, arguments)
                || !result_values(body, results)
                || !kind(body, memory.input, Type::Mem)
                || !kind(body, memory.output, Type::Mem)
            {
                return Err(super::error("invalid invoke operands/memory/results"));
            }
            let landing_block = body.edges[unwind.index()].to;
            let landing = &body.blocks[landing_block.index()];
            let cleanup = if landing.instructions.is_empty()
                && matches!(landing.terminator, Terminator::ResumePanic { .. })
            {
                u32::MAX
            } else {
                landing_block.0
            };
            let result_ids: Vec<ValueId> = (results.start..results.end).map(ValueId).collect();
            lower::call::lower_mode(
                body,
                call,
                body.args(arguments),
                &result_ids,
                false,
                lower::call::Mode::Runtime(
                    safepoint.map(|_| (index as u32, block.instructions.end)),
                ),
                Some((landing_block.0 + 1, cleanup)),
                b,
            )?;
            jump(b, "b", vec![], edge(*normal) as u32)?;
            Ok(vec![edge(*normal), edge(*unwind)])
        }
        Terminator::ResumePanic { memory } => {
            if !kind(body, *memory, Type::Mem) {
                return Err(super::error("invalid resume-panic memory token"));
            }
            b.push(Item::TailCall {
                target: crate::x64::mangle::mangle_runtime("panic_resume"),
                arguments: vec![],
            });
            Ok(vec![])
        }
        Terminator::Trap { .. } | Terminator::Unreachable { .. } => {
            b.emit("brk", vec![machine::fixed(Operand::Imm(0))])?;
            Ok(vec![])
        }
    }
}

fn valid_range(range: &Range<u32>, len: usize) -> bool {
    range.start <= range.end && range.end as usize <= len
}

fn values(body: &Body, range: &Range<u32>) -> bool {
    valid_range(range, body.operands.len())
        && body
            .args(range)
            .iter()
            .all(|id| id.index() < body.values.len())
}

fn result_values(body: &Body, range: &Range<u32>) -> bool {
    valid_range(range, body.values.len())
}

fn kind(body: &Body, id: ValueId, ty: Type) -> bool {
    body.values.get(id.index()).is_some_and(|v| v.kind.ty == ty)
}

fn validate(body: &Body, leaf: bool, runtime: bool) -> Result<(), Error> {
    if runtime {
        super::sites::validate(body)?;
    }
    if leaf && !body.poll_interface.leaf {
        return Err(super::error("requires proven PollFreeLeaf"));
    }
    if !runtime
        && (body.poll_summary.entry_stack_check
            || body.poll_interface.summary.entry_stack_check
            || body.poll_summary.has_poll_free_cycle
            || !body.safepoints.is_empty()
            || !body.root_inits.is_empty())
    {
        return Err(super::error("requires checked entry / stack maps (4c)"));
    }
    for (index, instruction) in body.instructions.iter().enumerate() {
        let needs_maps = match &instruction.op {
            Op::Call(call) | Op::ForeignCall(call) => lower::call::requires_maps(call),
            _ => false,
        };
        if !runtime
            && (needs_maps
                || matches!(instruction.op, Op::StackCheck | Op::SafepointPoll { .. })
                || instruction.safepoint.is_some())
        {
            return Err(Error(format!(
                "{} {:?}: requires checked entry / stack maps (4c)",
                site_context(body, index),
                instruction.op
            )));
        }
    }
    for (index, block) in body.blocks.iter().enumerate() {
        if matches!(&block.terminator, Terminator::TailCall { call, .. } if lower::call::requires_maps(call))
        {
            return Err(Error(format!(
                "block {index} terminator {:?}: requires checked entry / stack maps (4c)",
                block.terminator
            )));
        }
    }
    if body.revision != crate::lir::body::REVISION
        || body.entry.index() >= body.blocks.len()
        || body
            .blocks
            .len()
            .checked_add(body.edges.len())
            .and_then(|count| count.checked_add(1))
            .is_none_or(|count| count > u32::MAX as usize)
    {
        return Err(super::error("invalid leaf schema/entry/CFG size"));
    }
    super::abi::results(body.signature.results.iter().map(|p| p.ty))?;
    if let Some((bytes, alignment, _)) = body.signature.sret
        && (bytes == 0
            || !alignment.is_power_of_two()
            || alignment > 16
            || !body.signature.results.is_empty()
            || body
                .signature
                .parameters
                .first()
                .is_none_or(|p| p.ty != Type::Ptr))
    {
        return Err(super::error("invalid hidden sret signature"));
    }
    for value in &body.values {
        if super::abi::class(value.kind.ty) == super::abi::FPR && value.kind.provenance.is_some() {
            return Err(super::error(
                "FP/vector values cannot carry pointer provenance",
            ));
        }
        if !matches!(
            value.kind.ty,
            Type::I8
                | Type::I16
                | Type::I32
                | Type::I64
                | Type::Ptr
                | Type::Mem
                | Type::F32
                | Type::F64
                | Type::V128(_)
        ) || (!runtime && value.kind.provenance.is_some_and(|p| p.managed()))
        {
            return Err(Error(format!(
                "unsupported leaf value type {:?}",
                value.kind
            )));
        }
    }
    for ty in body
        .signature
        .parameters
        .iter()
        .chain(&body.signature.results)
    {
        if super::abi::class(ty.ty) == super::abi::GPR {
            lower::bits(ty.ty)?;
        }
        if !runtime && ty.provenance.is_some_and(|p| p.managed()) {
            return Err(super::error("managed leaf signature unsupported"));
        }
    }
    for slot in &body.stack_slots {
        if (!runtime && !slot.roots.is_empty()) || !slot.align.is_power_of_two() || slot.align > 16
        {
            return Err(super::error("unsupported rooted/overaligned local slot"));
        }
    }
    for (index, instruction) in body.instructions.iter().enumerate() {
        if leaf && matches!(instruction.op, Op::Call(_) | Op::ForeignCall(_)) {
            return Err(Error(format!("site {index}: compile_leaf rejects calls")));
        }
        if !values(body, &instruction.arguments)
            || !result_values(body, &instruction.results)
            || instruction.memory.is_some_and(|mem| {
                !kind(body, mem.input, Type::Mem) || !kind(body, mem.output, Type::Mem)
            })
            || (!runtime && instruction.safepoint.is_some())
        {
            return Err(Error(format!(
                "site {index} {:?}: invalid operands/memory/safepoint",
                instruction.op
            )));
        }
        if matches!(
            instruction.op,
            Op::Load(_) | Op::Store(_) | Op::Memcpy { .. } | Op::Memmove { .. } | Op::Memset { .. }
        ) && instruction.memory.is_none()
        {
            return Err(Error(format!(
                "site {index}: memory access lacks sequencing token"
            )));
        }
        if matches!(instruction.op, Op::Call(_) | Op::ForeignCall(_))
            && instruction.memory.is_none()
        {
            return Err(Error(format!(
                "{}: call lacks sequencing token",
                site_context(body, index)
            )));
        }
    }
    for (index, block) in body.blocks.iter().enumerate() {
        if leaf
            && matches!(
                block.terminator,
                Terminator::TailCall { .. } | Terminator::Invoke { .. }
            )
        {
            return Err(Error(format!(
                "block {index}: compile_leaf rejects call terminators"
            )));
        }
        if !valid_range(&block.instructions, body.instructions.len())
            || !valid_range(&block.parameters, body.parameters.len())
            || !valid_range(&block.predecessors, body.predecessors.len())
        {
            return Err(Error(format!("block {index}: invalid arena range")));
        }
        if body
            .params(crate::lir::body::BlockId(index as u32))
            .iter()
            .any(|p| p.value.index() >= body.values.len())
        {
            return Err(Error(format!(
                "block {index}: parameter value out of range"
            )));
        }
        let edge_valid = |id: crate::lir::body::EdgeId| {
            body.edges
                .get(id.index())
                .is_some_and(|e| e.from.index() == index)
        };
        let valid = match &block.terminator {
            Terminator::Jump(e) => edge_valid(*e),
            Terminator::Branch { condition, yes, no } => {
                kind(body, *condition, Type::I8) && edge_valid(*yes) && edge_valid(*no)
            }
            Terminator::Switch {
                value,
                cases,
                otherwise,
            } => {
                body.values
                    .get(value.index())
                    .is_some_and(|v| v.kind.ty.integer())
                    && valid_range(cases, body.switch_cases.len())
                    && body.switch_cases[crate::lir::body::range(cases)]
                        .iter()
                        .all(|(_, e)| edge_valid(*e))
                    && edge_valid(*otherwise)
            }
            Terminator::Return {
                values: returned,
                memory,
            } => {
                values(body, returned)
                    && kind(body, *memory, Type::Mem)
                    // ABI 只要求返回车道类型与签名一致；返回值的 provenance
                    // 是 GC 元数据，可带根标记而不改变调用契约。
                    && body
                        .args(returned)
                        .iter()
                        .map(|id| body.values[id.index()].kind.ty)
                        .eq(body.signature.results.iter().map(|r| r.ty))
            }
            Terminator::Trap { memory } | Terminator::Unreachable { memory } => {
                kind(body, *memory, Type::Mem)
            }
            Terminator::Invoke {
                arguments,
                results,
                memory,
                normal,
                unwind,
                ..
            } => {
                values(body, arguments)
                    && result_values(body, results)
                    && kind(body, memory.input, Type::Mem)
                    && kind(body, memory.output, Type::Mem)
                    && edge_valid(*normal)
                    && edge_valid(*unwind)
                    && body.edges.get(unwind.index()).is_some_and(|e| e.unwind)
                    && body.edges.get(normal.index()).is_some_and(|e| !e.unwind)
            }
            Terminator::ResumePanic { memory } => kind(body, *memory, Type::Mem),
            _ => true,
        };
        if !valid {
            return Err(Error(format!(
                "block {index} terminator {:?}: invalid operands/edges",
                block.terminator
            )));
        }
    }
    for (index, edge) in body.edges.iter().enumerate() {
        if edge.from.index() >= body.blocks.len()
            || edge.to.index() >= body.blocks.len()
            || (edge.unwind
                && !body.blocks.get(edge.from.index()).is_some_and(|b| {
                    matches!(&b.terminator, Terminator::Invoke { unwind, .. } if unwind.index() == index)
                }))
            || !values(body, &edge.arguments)
        {
            return Err(Error(format!(
                "edge {index}: invalid endpoints/arguments/unwind"
            )));
        }
        let params = body.params(edge.to);
        if params.len() != body.args(&edge.arguments).len()
            || params
                .iter()
                .zip(body.args(&edge.arguments))
                .any(|(p, a)| body.values[p.value.index()].kind != body.values[a.index()].kind)
        {
            return Err(Error(format!("edge {index}: parameter mismatch")));
        }
    }
    let params = body
        .params(body.entry)
        .iter()
        .filter(|p| body.values[p.value.index()].kind.ty != Type::Mem)
        .map(|p| body.values[p.value.index()].kind)
        .collect::<Vec<_>>();
    if params != body.signature.parameters {
        return Err(super::error(
            "entry parameters differ from leaf ABI signature",
        ));
    }
    Ok(())
}

fn site_context(body: &Body, site: usize) -> String {
    match body
        .blocks
        .iter()
        .position(|block| block.instructions.contains(&(site as u32)))
    {
        Some(block) => format!("{} block {block} site {site}", body.name),
        None => format!("{} site {site}", body.name),
    }
}
