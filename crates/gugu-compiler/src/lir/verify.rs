//! release 与缓存恢复共用的结构闸门；不能依赖构造器的正确性。
mod operations;
mod poll;
mod provenance;
mod regions;

use super::body::{
    BlockId, Body, Definition, EdgeId, InstId, Origin, SafepointId, Terminator, Type, UseSite,
    ValueId, id, range,
};
use super::invalid;
use super::pass::graph::Graph;
use crate::{Diagnostic, frontend::hir};
use std::ops::Range;

pub(crate) fn verify(body: &Body, module: &hir::Module) -> Result<(), Diagnostic> {
    verify_structure(body, module)?;
    let graph = Graph::new(body)?;
    poll::verify(body, &graph)?;
    Ok(())
}

/// 结构不变量；poll 预算由 [`verify`] 在固定管线结束后追加检查。
pub(crate) fn verify_structure(body: &Body, module: &hir::Module) -> Result<(), Diagnostic> {
    structure(body, module)?;
    let graph = Graph::new(body)?;
    definitions(body, &graph)?;
    memory(body)?;
    operations::verify(body)?;
    provenance::verify(body, &graph)?;
    regions::verify(body, &graph)?;
    let (ranges, uses) = super::uses::calculate(body);
    if uses != body.uses
        || body
            .values
            .iter()
            .zip(ranges)
            .any(|(value, range)| value.uses != range)
    {
        return Err(invalid("LIR 紧凑 use 链与实际操作数不一致"));
    }
    Ok(())
}

fn valid_range(value: &Range<u32>, len: usize) -> bool {
    value.start <= value.end && usize::try_from(value.end).is_ok_and(|end| end <= len)
}

fn structure(body: &Body, module: &hir::Module) -> Result<(), Diagnostic> {
    if body.revision != super::body::REVISION
        || crate::TargetName::parse(&body.target).is_err()
        || body.entry.index() >= body.blocks.len()
        || body.source_scopes.is_empty()
    {
        return Err(invalid("LIR schema、目标或入口不合法"));
    }
    for (index, scope) in body.source_scopes.iter().enumerate() {
        if scope.parent.is_some_and(|parent| parent.index() >= index)
            || !location(&scope.location, module)
        {
            return Err(invalid("LIR source scope 不是有根有序作用域树"));
        }
    }
    for value in &body.values {
        if value.kind.ty == Type::Void
            || (value.kind.ty == Type::Ptr) != value.kind.provenance.is_some()
            || !valid_range(&value.uses, body.uses.len())
        {
            return Err(invalid("LIR value 类型、provenance 或 use 范围不合法"));
        }
        if let Origin::Derived(base) = value.origin
            && base.index() >= body.values.len()
        {
            return Err(invalid("pointer origin 引用越界"));
        }
    }
    if body
        .operands
        .iter()
        .any(|value| value.index() >= body.values.len())
    {
        return Err(invalid("LIR 操作数引用越界"));
    }
    let (mut instructions, mut parameters, mut predecessors) = (0, 0, 0);
    let mut edge_uses = vec![0u32; body.edges.len()];
    for (index, block) in body.blocks.iter().enumerate() {
        if block.instructions.start != instructions
            || block.parameters.start != parameters
            || block.predecessors.start != predecessors
            || !valid_range(&block.instructions, body.instructions.len())
            || !valid_range(&block.parameters, body.parameters.len())
            || !valid_range(&block.predecessors, body.predecessors.len())
            || !source(&block.source, body, module)
        {
            return Err(invalid("LIR block arena 范围或来源不合法"));
        }
        instructions = block.instructions.end;
        parameters = block.parameters.end;
        predecessors = block.predecessors.end;
        let params = body.params(BlockId(id(index)));
        if params.is_empty()
            || params[0].source.is_some()
            || params
                .iter()
                .any(|parameter| parameter.value.index() >= body.values.len())
            || body.values[params[0].value.index()].kind.ty != Type::Mem
        {
            return Err(invalid("每个 block 的参数 0 必须是 Mem"));
        }
        if BlockId(id(index)) != body.entry
            && !params[1..]
                .windows(2)
                .all(|pair| pair[0].source < pair[1].source)
        {
            return Err(invalid("普通 block 参数没有按 local/偏移规范排序"));
        }
        if params[1..].iter().any(|parameter| {
            body.values[parameter.value.index()].kind.ty == Type::Mem || parameter.source.is_none()
        }) {
            return Err(invalid("Mem 不能出现在普通 block 参数中"));
        }
        terminator_ranges(body, &block.terminator)?;
        let mut valid = true;
        edges(body, &block.terminator, |edge| {
            if let Some(count) = edge_uses.get_mut(edge.index()) {
                *count += 1;
                valid &= body.edges[edge.index()].from.index() == index;
            } else {
                valid = false;
            }
        });
        if !valid {
            return Err(invalid("terminator 的边引用或来源 block 不合法"));
        }
    }
    if usize::try_from(instructions).expect("范围适配宿主") != body.instructions.len()
        || usize::try_from(parameters).expect("范围适配宿主") != body.parameters.len()
        || usize::try_from(predecessors).expect("范围适配宿主") != body.predecessors.len()
        || edge_uses.iter().any(|count| *count != 1)
    {
        return Err(invalid("LIR arena 存在孤立或重叠指令、参数或边"));
    }
    for (index, edge) in body.edges.iter().enumerate() {
        if edge.from.index() >= body.blocks.len()
            || edge.to.index() >= body.blocks.len()
            || !valid_range(&edge.arguments, body.operands.len())
        {
            return Err(invalid("LIR edge 不合法"));
        }
        let parameters = body.params(edge.to);
        let arguments = body.args(&edge.arguments);
        if parameters.len() != arguments.len()
            || parameters
                .iter()
                .zip(arguments)
                .any(|(parameter, argument)| {
                    !compatible(
                        body.values[parameter.value.index()].kind,
                        body.values[argument.index()].kind,
                    )
                })
        {
            return Err(invalid("block 实参数量、类型或 provenance 不匹配"));
        }
        let predecessors = &body.predecessors[range(&body.blocks[edge.to.index()].predecessors)];
        if predecessors
            .iter()
            .filter(|candidate| candidate.index() == index)
            .count()
            != 1
        {
            return Err(invalid("前驱表不包含恰好一次实际边"));
        }
        if edge.unwind && !body.blocks[edge.to.index()].cleanup {
            return Err(invalid("unwind 边没有进入 cleanup block"));
        }
    }
    for (block, data) in body.blocks.iter().enumerate() {
        for predecessor in &body.predecessors[range(&data.predecessors)] {
            if predecessor.index() >= body.edges.len()
                || body.edges[predecessor.index()].to.index() != block
            {
                return Err(invalid("前驱表含有错误边"));
            }
        }
    }
    for instruction in &body.instructions {
        if !valid_range(&instruction.arguments, body.operands.len())
            || !valid_range(&instruction.results, body.values.len())
            || !source(&instruction.source, body, module)
            || instruction.memory.is_some_and(|memory| {
                memory.input.index() >= body.values.len()
                    || memory.output.index() >= body.values.len()
            })
        {
            return Err(invalid("LIR instruction 范围、Mem 或来源不合法"));
        }
    }
    for lifetime in &body.lifetimes {
        if lifetime.slot.index() >= body.stack_slots.len()
            || lifetime.block.index() >= body.blocks.len()
            || lifetime.position
                > body.blocks[lifetime.block.index()].instructions.end
                    - body.blocks[lifetime.block.index()].instructions.start
        {
            return Err(invalid("栈槽寿命记录越界"));
        }
    }
    for slot in &body.stack_slots {
        if slot.align == 0
            || !slot.align.is_power_of_two()
            || slot.bytes % u64::from(slot.align) != 0
            || slot
                .roots
                .iter()
                .any(|(offset, _)| offset.checked_add(8).is_none_or(|end| end > slot.bytes))
        {
            return Err(invalid("栈槽布局或 root 字段越界"));
        }
    }
    if body.data.iter().any(|data| !data.align.is_power_of_two()) {
        return Err(invalid("常量数据对齐非法"));
    }
    Ok(())
}

fn location(location: &hir::Location, module: &hir::Module) -> bool {
    module
        .sources
        .get(usize::try_from(location.source).expect("源码编号"))
        .is_some_and(|source| {
            location.start <= location.end
                && location.end <= source.length
                && usize::try_from(location.expansion).expect("展开编号") <= module.expansions.len()
        })
}
fn source(
    source: &crate::frontend::gir::body::SourceInfo,
    body: &Body,
    module: &hir::Module,
) -> bool {
    source.scope.index() < body.source_scopes.len() && location(&source.location, module)
}

fn terminator_ranges(body: &Body, terminator: &Terminator) -> Result<(), Diagnostic> {
    let valid_value = |value: ValueId| value.index() < body.values.len();
    let valid = match terminator {
        Terminator::Jump(_) => true,
        Terminator::Branch { condition, .. } => valid_value(*condition),
        Terminator::Switch { value, cases, .. } => {
            valid_value(*value) && valid_range(cases, body.switch_cases.len())
        }
        Terminator::Invoke {
            arguments,
            results,
            memory,
            ..
        } => {
            valid_range(arguments, body.operands.len())
                && valid_range(results, body.values.len())
                && valid_value(memory.input)
                && valid_value(memory.output)
        }
        Terminator::Return { values, memory } => {
            valid_range(values, body.operands.len()) && valid_value(*memory)
        }
        Terminator::TailCall {
            arguments, memory, ..
        } => valid_range(arguments, body.operands.len()) && valid_value(*memory),
        Terminator::ResumePanic { memory }
        | Terminator::Trap { memory }
        | Terminator::Unreachable { memory } => valid_value(*memory),
    };
    if valid {
        Ok(())
    } else {
        Err(invalid("终结符操作数或范围越界"))
    }
}

pub(super) fn edges(body: &Body, terminator: &Terminator, mut visit: impl FnMut(EdgeId)) {
    match terminator {
        Terminator::Jump(edge) => visit(*edge),
        Terminator::Branch { yes, no, .. } => {
            visit(*yes);
            visit(*no);
        }
        Terminator::Switch {
            cases, otherwise, ..
        } => {
            for (_, edge) in &body.switch_cases[range(cases)] {
                visit(*edge);
            }
            visit(*otherwise);
        }
        Terminator::Invoke { normal, unwind, .. } => {
            visit(*normal);
            visit(*unwind);
        }
        _ => {}
    }
}

pub(super) fn compatible(expected: super::body::ValueType, actual: super::body::ValueType) -> bool {
    use super::body::Provenance;
    expected.ty == actual.ty
        && (expected.provenance == actual.provenance
            || matches!(
                (expected.provenance, actual.provenance),
                (
                    Some(Provenance::GcInterior),
                    Some(Provenance::GcHeap | Provenance::Stack)
                ) | (Some(Provenance::Foreign), Some(Provenance::Raw))
            ))
}

fn definitions(body: &Body, graph: &Graph) -> Result<(), Diagnostic> {
    let mut defined = vec![false; body.values.len()];
    let mut define = |value: ValueId, definition| -> Result<(), Diagnostic> {
        if std::mem::replace(&mut defined[value.index()], true)
            || body.values[value.index()].definition != definition
        {
            return Err(invalid("SSA value 被重复定义或定义位置不匹配"));
        }
        Ok(())
    };
    for (block, _) in body.blocks.iter().enumerate() {
        let block = BlockId(id(block));
        for (index, parameter) in body.params(block).iter().enumerate() {
            define(
                parameter.value,
                Definition::Parameter {
                    block,
                    index: id(index),
                },
            )?;
        }
    }
    for (index, instruction) in body.instructions.iter().enumerate() {
        let id = InstId(id(index));
        for (index, value) in instruction.results.clone().map(ValueId).enumerate() {
            define(
                value,
                Definition::Instruction {
                    instruction: id,
                    result: super::body::id(index),
                },
            )?;
        }
        if let Some(memory) = instruction.memory {
            define(
                memory.output,
                Definition::Instruction {
                    instruction: id,
                    result: instruction.results.end - instruction.results.start,
                },
            )?;
        }
    }
    for (index, block) in body.blocks.iter().enumerate() {
        if let Terminator::Invoke {
            results, memory, ..
        } = &block.terminator
        {
            let block = BlockId(id(index));
            for (index, value) in results.clone().map(ValueId).enumerate() {
                define(
                    value,
                    Definition::Invoke {
                        block,
                        result: id(index),
                    },
                )?;
            }
            define(
                memory.output,
                Definition::Invoke {
                    block,
                    result: results.end - results.start,
                },
            )?;
        }
    }
    if defined.iter().any(|defined| !defined) {
        return Err(invalid("LIR value 没有定义"));
    }
    let mut valid = true;
    super::uses::visit(body, |value, use_site| {
        valid &= dominates_use(body, graph, value, use_site.site)
    });
    if !valid {
        return Err(invalid(
            "SSA 定义没有支配使用，或 Invoke 结果逃到 unwind 边",
        ));
    }
    Ok(())
}

fn dominates_use(body: &Body, graph: &Graph, value: ValueId, site: UseSite) -> bool {
    let use_block = match site {
        UseSite::Instruction(instruction) => graph.instruction_blocks[instruction.index()],
        UseSite::Terminator(block) => block,
        UseSite::Edge(edge) => body.edges[edge.index()].from,
    };
    match body.values[value.index()].definition {
        Definition::Parameter { block, .. } => graph.dominates(block, use_block),
        Definition::Instruction { instruction, .. } => {
            let block = graph.instruction_blocks[instruction.index()];
            graph.dominates(block, use_block)
                && match site {
                    UseSite::Instruction(use_instruction) if block == use_block => {
                        instruction.0 < use_instruction.0 || block_independent(body, instruction)
                    }
                    _ => true,
                }
        }
        Definition::Invoke { block, result } => {
            let Terminator::Invoke {
                ref results,
                normal,
                unwind,
                ..
            } = body.blocks[block.index()].terminator
            else {
                return false;
            };
            matches!(site, UseSite::Edge(edge) if edge == normal || result == results.end - results.start && edge == unwind)
        }
    }
}

fn block_independent(body: &Body, instruction: InstId) -> bool {
    let instruction = &body.instructions[instruction.index()];
    matches!(
        instruction.op,
        super::body::Op::IConst(_) | super::body::Op::FConst(_)
    ) && instruction.arguments.is_empty()
        && instruction.memory.is_none()
        && instruction.safepoint.is_none()
}

fn memory(body: &Body) -> Result<(), Diagnostic> {
    let mut safepoints = vec![false; body.safepoints.len()];
    for (block_index, block) in body.blocks.iter().enumerate() {
        let block_id = BlockId(id(block_index));
        let mut current = body.params(block_id)[0].value;
        for index in range(&block.instructions) {
            let instruction = &body.instructions[index];
            if instruction.op.has_memory() != instruction.memory.is_some() {
                return Err(invalid("内存操作缺少 Mem 或纯操作伪造 Mem"));
            }
            if let Some(memory) = instruction.memory {
                if memory.input != current
                    || memory.input == memory.output
                    || body.values[memory.output.index()].kind.ty != Type::Mem
                {
                    return Err(invalid("Mem 链不连续或分叉"));
                }
                current = memory.output;
            }
            safepoint(
                body,
                instruction.safepoint,
                instruction.op.safepoint_kind(),
                block_id,
                Some(InstId(id(index))),
                &mut safepoints,
            )?;
        }
        let terminal = match &block.terminator {
            Terminator::Invoke {
                call,
                memory,
                safepoint: point,
                ..
            } => {
                if memory.input != current
                    || body.values[memory.output.index()].kind.ty != Type::Mem
                {
                    return Err(invalid("Invoke Mem 链不连续"));
                }
                current = memory.output;
                safepoint(
                    body,
                    *point,
                    call.safepoint_kind(),
                    block_id,
                    None,
                    &mut safepoints,
                )?;
                None
            }
            Terminator::Return { memory, .. }
            | Terminator::ResumePanic { memory }
            | Terminator::TailCall { memory, .. }
            | Terminator::Trap { memory }
            | Terminator::Unreachable { memory } => Some(*memory),
            _ => None,
        };
        if terminal.is_some_and(|memory| memory != current) {
            return Err(invalid("终结符没有消费当前 Mem"));
        }
        let mut valid = true;
        edges(body, &block.terminator, |edge| {
            valid &= body.args(&body.edges[edge.index()].arguments).first() == Some(&current)
        });
        if !valid {
            return Err(invalid("后继边没有传递当前 Mem"));
        }
    }
    if safepoints.iter().any(|used| !used) {
        return Err(invalid("存在孤立 safepoint 记录"));
    }
    Ok(())
}

fn safepoint(
    body: &Body,
    point: Option<SafepointId>,
    expected: Option<super::body::SafepointKind>,
    block: BlockId,
    instruction: Option<InstId>,
    used: &mut [bool],
) -> Result<(), Diagnostic> {
    match (point, expected) {
        (None, None) => Ok(()),
        (Some(point), Some(kind)) => {
            let data = body
                .safepoints
                .get(point.index())
                .ok_or_else(|| invalid("safepoint 编号越界"))?;
            if std::mem::replace(&mut used[point.index()], true)
                || data.kind != kind
                || data.block != block
                || data.instruction != instruction
            {
                Err(invalid("safepoint 的种类、位置或唯一性不匹配"))
            } else {
                Ok(())
            }
        }
        _ => Err(invalid("调用、分配、屏障或挂起缺少正确 safepoint")),
    }
}
