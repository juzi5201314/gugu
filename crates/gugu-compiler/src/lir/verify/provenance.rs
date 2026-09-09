use super::{Graph, invalid};
use crate::Diagnostic;
use crate::lir::body::{
    AliasClass, Body, Conversion, Definition, Op, Origin, Provenance, SlotId, Terminator, Type,
    ValueId, range,
};

pub(super) fn verify(body: &Body, graph: &Graph) -> Result<(), Diagnostic> {
    for (index, instruction) in body.instructions.iter().enumerate() {
        let args = body.args(&instruction.arguments);
        let results = &body.values[range(&instruction.results)];
        match &instruction.op {
            Op::StackAddr(slot) => {
                if results[0].origin != Origin::Stack(*slot) {
                    return Err(invalid("StackAddr 的来源槽被伪造"));
                }
            }
            Op::PtrOffset => {
                let input = body.values[args[0].index()].kind.provenance;
                let output = results[0].kind.provenance;
                if input != output
                    && !matches!(
                        (input, output),
                        (Some(Provenance::GcHeap), Some(Provenance::GcInterior))
                    )
                {
                    return Err(invalid("PtrOffset 越权改变 pointer provenance"));
                }
                if results[0].origin != Origin::Derived(args[0]) {
                    return Err(invalid("PtrOffset 丢失基址来源"));
                }
            }
            Op::Convert(Conversion::IntToPointer) => {
                if results[0].kind.provenance != Some(Provenance::Raw) {
                    return Err(invalid(
                        "整数不能生成 managed、stack、code 或 metadata pointer",
                    ));
                }
            }
            Op::Convert(Conversion::PointerCast) => {
                let source = body.values[args[0].index()].kind.provenance;
                let target = results[0].kind.provenance;
                if source != target && target != Some(Provenance::Raw) {
                    return Err(invalid("pointer cast 不能提升追踪权限"));
                }
                if results[0].origin != Origin::Derived(args[0]) {
                    return Err(invalid("pointer cast 丢失原始来源"));
                }
            }
            Op::Convert(Conversion::RawToReference) => {
                if body.values[args[0].index()].kind.provenance != Some(Provenance::Raw) {
                    return Err(invalid("只有裸指针可以显式构造引用"));
                }
                if results[0].kind.provenance.is_none() {
                    return Err(invalid("引用构造缺少目标 provenance"));
                }
                if results[0].origin != Origin::Derived(args[0]) {
                    return Err(invalid("引用构造丢失原始来源"));
                }
            }
            Op::Load(access) => {
                if matches!(access.alias, AliasClass::Foreign)
                    && results[0].kind.provenance.is_some_and(Provenance::managed)
                    && body.values[args[0].index()].kind.provenance == Some(Provenance::Raw)
                {
                    return Err(invalid("未经 descriptor 的 raw load 不能生成 GC 根"));
                }
                alias(body, args[0], &access.alias, access.volatile)?;
            }
            Op::Store(access) => {
                alias(body, args[0], &access.alias, access.volatile)?;
                let heap = !matches!(access.alias, AliasClass::Stack(_))
                    && body.values[args[0].index()]
                        .kind
                        .provenance
                        .is_some_and(Provenance::managed);
                if heap && body.values[args[1].index()].kind.provenance == Some(Provenance::Stack) {
                    return Err(invalid("栈引用逃逸到 managed heap，缺少存储提升"));
                }
                if heap
                    && body.values[args[1].index()]
                        .kind
                        .provenance
                        .is_some_and(Provenance::managed)
                {
                    let block = &body.blocks[graph.instruction_blocks[index].index()];
                    let barrier = body.instructions
                        [index + 1..usize::try_from(block.instructions.end).expect("指令范围")]
                        .iter()
                        .find(|instruction| instruction.memory.is_some());
                    if !barrier.is_some_and(|barrier| matches!(&barrier.op, Op::GcWriteBarrier { store } | Op::GcWriteBarrierReserved { store, .. } if store.index() == index)) {
                        return Err(invalid("heap managed 引用 store 缺少紧随的写屏障"));
                    }
                }
            }
            Op::GcWriteBarrier { store } | Op::GcWriteBarrierReserved { store, .. } => {
                if store.index() >= index
                    || graph.instruction_blocks[store.index()] != graph.instruction_blocks[index]
                {
                    return Err(invalid("写屏障必须跟随同 block 的实际 store"));
                }
                let store = &body.instructions[store.index()];
                let stored = body.args(&store.arguments);
                if !matches!(store.op, Op::Store(_)) || stored[0] != args[0] || stored[1] != args[2]
                {
                    return Err(invalid("写屏障记录与实际目标/新值不匹配"));
                }
                let Definition::Instruction {
                    instruction: old, ..
                } = body.values[args[1].index()].definition
                else {
                    return Err(invalid("hybrid barrier 缺少覆盖前旧引用"));
                };
                let old = &body.instructions[old.index()];
                if !matches!(old.op, Op::Load(_))
                    || body.args(&old.arguments) != [args[0]]
                    || old.memory.map(|memory| memory.output)
                        != store.memory.map(|memory| memory.input)
                {
                    return Err(invalid("hybrid barrier 的旧引用没有在 store 前读取"));
                }
            }
            _ => {}
        }
        for result in results {
            if let Origin::Derived(base) = result.origin
                && !args.contains(&base)
            {
                return Err(invalid("pointer 来源不在指令操作数中"));
            }
            if result.kind.ty != Type::Ptr
                && matches!(result.origin, Origin::Stack(_) | Origin::Allocation(_))
            {
                return Err(invalid("非指针值伪造 storage provenance"));
            }
        }
    }
    lifetimes(body, graph)
}

fn alias(
    body: &Body,
    pointer: ValueId,
    alias: &AliasClass,
    volatile: bool,
) -> Result<(), Diagnostic> {
    if volatile != matches!(alias, AliasClass::Volatile) {
        return Err(invalid("volatile effect 与 AliasClass 不一致"));
    }
    if let AliasClass::Stack(slot) = alias {
        let roots = stack_origins(body, pointer);
        if roots != [*slot] {
            return Err(invalid("stack alias class 与真实 pointer 来源不一致"));
        }
    }
    if matches!(alias, AliasClass::FreshHeap(_))
        && !matches!(body.values[pointer.index()].origin, Origin::Allocation(_))
    {
        return Err(invalid(
            "已合流或发布的 pointer 不能伪造 fresh allocation alias",
        ));
    }
    Ok(())
}

pub(super) fn stack_origins(body: &Body, value: ValueId) -> Vec<SlotId> {
    let mut seen = vec![false; body.values.len()];
    let mut work = vec![value];
    let mut slots = Vec::new();
    while let Some(value) = work.pop() {
        if std::mem::replace(&mut seen[value.index()], true) {
            continue;
        }
        match body.values[value.index()].origin {
            Origin::Stack(slot) => slots.push(slot),
            Origin::Derived(base) => work.push(base),
            Origin::Merge => {
                if let Definition::Parameter { block, index } =
                    body.values[value.index()].definition
                {
                    for edge in &body.predecessors[range(&body.blocks[block.index()].predecessors)]
                    {
                        work.push(
                            body.args(&body.edges[edge.index()].arguments)
                                [usize::try_from(index).expect("参数编号")],
                        );
                    }
                }
            }
            _ => {}
        }
    }
    slots.sort_unstable();
    slots.dedup();
    slots
}

fn lifetimes(body: &Body, graph: &Graph) -> Result<(), Diagnostic> {
    let mut input = vec![vec![true; body.stack_slots.len()]; body.blocks.len()];
    let mut output = input.clone();
    input[body.entry.index()].fill(false);
    loop {
        let mut changed = false;
        for &block in &graph.order {
            let mut live = if block == body.entry {
                vec![false; body.stack_slots.len()]
            } else {
                vec![true; body.stack_slots.len()]
            };
            if block != body.entry {
                for edge in &body.predecessors[range(&body.blocks[block.index()].predecessors)] {
                    let from = body.edges[edge.index()].from;
                    for (live, predecessor) in live.iter_mut().zip(&output[from.index()]) {
                        *live &= *predecessor;
                    }
                }
            }
            input[block.index()] = live.clone();
            for lifetime in body
                .lifetimes
                .iter()
                .filter(|lifetime| lifetime.block == block)
            {
                live[lifetime.slot.index()] = lifetime.live;
            }
            if output[block.index()] != live {
                output[block.index()] = live;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for &block_id in &graph.order {
        let block = &body.blocks[block_id.index()];
        let mut live = input[block_id.index()].clone();
        for position in 0..=block.instructions.end - block.instructions.start {
            for lifetime in body
                .lifetimes
                .iter()
                .filter(|lifetime| lifetime.block == block_id && lifetime.position == position)
            {
                live[lifetime.slot.index()] = lifetime.live;
            }
            if position == block.instructions.end - block.instructions.start {
                break;
            }
            let instruction = &body.instructions
                [usize::try_from(block.instructions.start + position).expect("指令位置")];
            let args = body.args(&instruction.arguments);
            let reads_address = matches!(
                instruction.op,
                Op::Load(_)
                    | Op::Store(_)
                    | Op::Memcpy { .. }
                    | Op::Memmove { .. }
                    | Op::Memset { .. }
                    | Op::Atomic { .. }
                    | Op::Call(_)
                    | Op::ForeignCall(_)
                    | Op::ScopedViewBegin { .. }
                    | Op::SharedAccessBegin { .. }
            );
            if reads_address {
                for &argument in args {
                    live_pointer(body, argument, &live)?;
                }
            }
            if let Op::Call(call) | Op::ForeignCall(call) = &instruction.op {
                escaping_call(body, call, args)?;
            }
        }
        match &block.terminator {
            Terminator::Return { values, .. } => {
                for value in body.args(values) {
                    if body.values[value.index()].kind.provenance != Some(Provenance::Raw)
                        && !stack_origins(body, *value).is_empty()
                    {
                        return Err(invalid("Return 暴露当前 frame 的栈引用"));
                    }
                }
            }
            Terminator::Invoke {
                arguments, call, ..
            }
            | Terminator::TailCall {
                arguments, call, ..
            } => {
                for &argument in body.args(arguments) {
                    live_pointer(body, argument, &live)?;
                }
                escaping_call(body, call, body.args(arguments))?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn live_pointer(body: &Body, value: ValueId, live: &[bool]) -> Result<(), Diagnostic> {
    if stack_origins(body, value)
        .iter()
        .any(|slot| !live[slot.index()])
    {
        Err(invalid("内存访问使用已结束寿命的栈槽引用"))
    } else {
        Ok(())
    }
}
fn escaping_call(
    body: &Body,
    call: &crate::lir::body::Call,
    args: &[ValueId],
) -> Result<(), Diagnostic> {
    if !call.captures_arguments {
        return Ok(());
    }
    for (index, value) in args.iter().enumerate() {
        if body.values[value.index()].kind.provenance == Some(Provenance::Stack)
            && !call
                .by_value
                .iter()
                .any(|(parameter, _, _)| usize::try_from(*parameter).expect("参数编号") == index)
            && !matches!(call.target, crate::lir::body::CallTarget::Runtime(_))
        {
            return Err(invalid("逃逸调用不能保存当前 frame 的栈引用"));
        }
    }
    Ok(())
}
