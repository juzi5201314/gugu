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
    validate(body).map_err(|e| Error(format!("{} entry: {e}", body.name)))?;
    let count = body.blocks.len();
    let mut function = Function {
        name: body.name.clone(),
        entry: body.entry.index(),
        types: body.values.iter().map(|v| v.kind.ty).collect(),
        blocks: vec![
            Block {
                items: vec![],
                successors: vec![]
            };
            count + body.edges.len()
        ],
    };
    for (index, block) in body.blocks.iter().enumerate() {
        let mut b = Builder {
            function: &mut function,
            items: vec![],
            context: format!("{} block {index} entry", body.name),
        };
        if index == body.entry.index() {
            entry(body, &mut b)?;
        }
        for site in block.instructions.clone() {
            let instruction = &body.instructions[site as usize];
            b.context = format!(
                "{} block {index} site {site} {:?}",
                body.name, instruction.op
            );
            lower::lower(body, instruction, &mut b)
                .map_err(|e| Error(format!("{}: {e}", b.context)))?;
        }
        b.context = format!(
            "{} block {index} terminator {:?}",
            body.name, block.terminator
        );
        let successors = terminator(body, &block.terminator, count, &mut b)
            .map_err(|e| Error(format!("{}: {e}", b.context)))?;
        b.function.blocks[index] = Block {
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
            function: &mut function,
            items: vec![],
            context,
        };
        b.push(Item::Copies(pairs));
        jump(&mut b, "b", vec![], edge.to.0)?;
        b.function.blocks[count + index] = Block {
            items: b.items,
            successors: vec![edge.to.index()],
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
    b.push(Item::Copies(
        params
            .iter()
            .take(8)
            .enumerate()
            .map(|(index, p)| (MReg::Physical(index as u8), MReg::Virtual(p.value.0)))
            .collect(),
    ));
    for (index, param) in params.iter().enumerate() {
        if index >= 8 {
            b.push(Item::Incoming {
                dest: param.value.0,
                index: index as u32 - 8,
            });
        }
        let bits = lower::bits(body.values[param.value.index()].kind.ty)?;
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
    term: &Terminator,
    count: usize,
    b: &mut Builder<'_>,
) -> Result<Vec<usize>, Error> {
    let edge = |id: crate::lir::body::EdgeId| count + id.index();
    match term {
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
            b.push(Item::Copies(
                body.args(values)
                    .iter()
                    .enumerate()
                    .map(|(index, value)| (MReg::Virtual(value.0), MReg::Physical(index as u8)))
                    .collect(),
            ));
            b.push(Item::Return);
            Ok(vec![])
        }
        Terminator::Trap { .. } | Terminator::Unreachable { .. } => {
            b.emit("brk", vec![machine::fixed(Operand::Imm(0))])?;
            Ok(vec![])
        }
        _ => Err(super::error(
            "unsupported leaf terminator (call/unwind/runtime)",
        )),
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

fn kind(body: &Body, id: ValueId, ty: Type) -> bool {
    body.values.get(id.index()).is_some_and(|v| v.kind.ty == ty)
}

fn validate(body: &Body) -> Result<(), Error> {
    if !body.poll_interface.leaf
        || body.poll_summary.entry_stack_check
        || body.poll_interface.summary.entry_stack_check
        || body.poll_summary.has_poll_free_cycle
        || !body.safepoints.is_empty()
        || !body.root_inits.is_empty()
    {
        return Err(super::error(
            "requires proven PollFreeLeaf without stack checks, polls, or safepoints",
        ));
    }
    if body.revision != crate::lir::body::REVISION
        || body.entry.index() >= body.blocks.len()
        || body
            .blocks
            .len()
            .checked_add(body.edges.len())
            .is_none_or(|count| count > u32::MAX as usize)
    {
        return Err(super::error("invalid leaf schema/entry/CFG size"));
    }
    if body.signature.sret.is_some()
        || !body.signature.by_value.is_empty()
        || body.signature.results.len() > 2
    {
        return Err(super::error(
            "leaf aggregate/sret or more than two results unsupported",
        ));
    }
    for value in &body.values {
        if !matches!(
            value.kind.ty,
            Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::Ptr | Type::Mem
        ) || value.kind.provenance.is_some_and(|p| p.managed())
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
        lower::bits(ty.ty)?;
        if ty.provenance.is_some_and(|p| p.managed()) {
            return Err(super::error("managed leaf signature unsupported"));
        }
    }
    for slot in &body.stack_slots {
        if !slot.roots.is_empty() || !slot.align.is_power_of_two() || slot.align > 16 {
            return Err(super::error("unsupported rooted/overaligned local slot"));
        }
    }
    for (index, instruction) in body.instructions.iter().enumerate() {
        if !values(body, &instruction.arguments)
            || !values(body, &instruction.results)
            || instruction.memory.is_some_and(|mem| {
                !kind(body, mem.input, Type::Mem) || !kind(body, mem.output, Type::Mem)
            })
            || instruction.safepoint.is_some()
        {
            return Err(Error(format!(
                "site {index} {:?}: invalid operands/memory/safepoint",
                instruction.op
            )));
        }
        if matches!(instruction.op, Op::Load(_) | Op::Store(_)) && instruction.memory.is_none() {
            return Err(Error(format!(
                "site {index}: memory access lacks sequencing token"
            )));
        }
    }
    for (index, block) in body.blocks.iter().enumerate() {
        if block.cleanup
            || !valid_range(&block.instructions, body.instructions.len())
            || !valid_range(&block.parameters, body.parameters.len())
            || !valid_range(&block.predecessors, body.predecessors.len())
            || body
                .params(crate::lir::body::BlockId(index as u32))
                .iter()
                .any(|p| p.value.index() >= body.values.len())
        {
            return Err(Error(format!("block {index}: invalid arena range")));
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
                    && body
                        .args(returned)
                        .iter()
                        .map(|id| body.values[id.index()].kind)
                        .collect::<Vec<_>>()
                        == body.signature.results
            }
            Terminator::Trap { memory } | Terminator::Unreachable { memory } => {
                kind(body, *memory, Type::Mem)
            }
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
            || edge.unwind
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
