use super::{Graph, invalid};
use crate::Diagnostic;
use crate::body::{
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
                // 压缩引用不参与偏移推导：它必须先经 `DecodeCompressedRef` 解码。
                if matches!(input, Some(Provenance::CompressedRef))
                    || matches!(output, Some(Provenance::CompressedRef))
                {
                    return Err(invalid("压缩引用不能直接做指针偏移"));
                }
                if matches!(input, Some(Provenance::SharedHandle))
                    || matches!(output, Some(Provenance::SharedHandle))
                {
                    return Err(invalid(
                        "handle 不是地址，必须先取得 access guard 的 payload",
                    ));
                }
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
                // handle 与压缩引用不参与 cast 相容：降级为直接指针会绕过解析，
                // 升级为 handle 会伪造受管身份。
                if matches!(
                    source,
                    Some(Provenance::SharedHandle | Provenance::CompressedRef)
                ) || matches!(
                    target,
                    Some(Provenance::SharedHandle | Provenance::CompressedRef)
                ) {
                    return Err(invalid("handle 或压缩引用不能参与指针转换"));
                }
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
                if results[0].kind.provenance.is_none()
                    || matches!(
                        results[0].kind.provenance,
                        Some(Provenance::SharedHandle | Provenance::CompressedRef)
                    )
                {
                    return Err(invalid("引用构造缺少合法目标 provenance"));
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
                    // 本地 heap 字段写入由 hybrid barrier 记账；共享 payload 的字段写入由
                    // guard 绑定的 `SharedFieldBarrier` 记账（它同时满足 `shared_access` 的
                    // guard/token 规则，因此这里只要求「紧随的实际屏障覆盖同一条 store」）。
                    let paired = barrier.is_some_and(|barrier| match &barrier.op {
                        Op::GcWriteBarrier { store } | Op::GcWriteBarrierReserved { store, .. } => {
                            store.index() == index
                        }
                        Op::SharedFieldBarrier { store, .. }
                        | Op::SharedFieldBarrierReserved { store, .. } => store.index() == index,
                        _ => false,
                    });
                    if !paired {
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
        let matches_slot = match direct_stack_origin(body, pointer) {
            DirectStackOrigin::Single(found) => found == *slot,
            DirectStackOrigin::None => false,
            DirectStackOrigin::Complex => stack_origins(body, pointer) == [*slot],
        };
        if !matches_slot {
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

enum DirectStackOrigin {
    None,
    Single(SlotId),
    Complex,
}

/// 零分配快路径：99% 的栈指针是直接 `Origin::Stack` 或短 `Origin::Derived` 链，
/// 避免在每条 Load/Store/Call 实参上分配 `vec![false; body.values.len()]`。
fn direct_stack_origin(body: &Body, mut value: ValueId) -> DirectStackOrigin {
    for _ in 0..16 {
        match body.values[value.index()].origin {
            Origin::Stack(slot) => return DirectStackOrigin::Single(slot),
            Origin::Derived(base) => value = base,
            Origin::Merge => return DirectStackOrigin::Complex,
            _ => return DirectStackOrigin::None,
        }
    }
    DirectStackOrigin::Complex
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

fn has_stack_origin(body: &Body, value: ValueId) -> bool {
    match direct_stack_origin(body, value) {
        DirectStackOrigin::None => false,
        DirectStackOrigin::Single(_) => true,
        DirectStackOrigin::Complex => !stack_origins(body, value).is_empty(),
    }
}

fn lifetimes(body: &Body, graph: &Graph) -> Result<(), Diagnostic> {
    let slot_count = body.stack_slots.len();
    if slot_count == 0 {
        // 无栈槽函数不存在栈槽寿命检查，只需校验 escaping_call 与 Return 栈引用规则。
        for &block_id in &graph.order {
            let block = &body.blocks[block_id.index()];
            for instruction in &body.instructions[range(&block.instructions)] {
                if let Op::Call(call) | Op::ForeignCall(call) = &instruction.op {
                    escaping_call(body, call, body.args(&instruction.arguments))?;
                }
            }
            match &block.terminator {
                Terminator::Return { values, .. } => {
                    for &value in body.args(values) {
                        if body.values[value.index()].kind.provenance != Some(Provenance::Raw)
                            && has_stack_origin(body, value)
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
                    escaping_call(body, call, body.args(arguments))?;
                }
                _ => {}
            }
        }
        return Ok(());
    }

    // 按块分桶并在块内按 position 稳定排序（保留同 position 原顺序），消除 O(instructions × lifetimes) 扫描。
    let mut lifetimes_by_block: Vec<Vec<(u32, SlotId, bool)>> = vec![Vec::new(); body.blocks.len()];
    for lifetime in &body.lifetimes {
        lifetimes_by_block[lifetime.block.index()].push((
            lifetime.position,
            lifetime.slot,
            lifetime.live,
        ));
    }
    for events in &mut lifetimes_by_block {
        events.sort_by_key(|(position, _, _)| *position);
    }

    let mut input = vec![vec![true; slot_count]; body.blocks.len()];
    let mut output = input.clone();
    input[body.entry.index()].fill(false);
    let mut live = vec![false; slot_count];
    loop {
        let mut changed = false;
        for &block in &graph.order {
            if block == body.entry {
                live.fill(false);
            } else {
                live.fill(true);
                for edge in &body.predecessors[range(&body.blocks[block.index()].predecessors)] {
                    let from = body.edges[edge.index()].from;
                    for (slot_live, &predecessor) in live.iter_mut().zip(&output[from.index()]) {
                        *slot_live &= predecessor;
                    }
                }
            }
            input[block.index()].copy_from_slice(&live);
            for &(_, slot, is_live) in &lifetimes_by_block[block.index()] {
                live[slot.index()] = is_live;
            }
            if output[block.index()] != live {
                output[block.index()].copy_from_slice(&live);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for &block_id in &graph.order {
        let block = &body.blocks[block_id.index()];
        live.copy_from_slice(&input[block_id.index()]);
        let events = &lifetimes_by_block[block_id.index()];
        let mut event_cursor = 0;
        let len = block.instructions.end - block.instructions.start;
        for position in 0..=len {
            while event_cursor < events.len() && events[event_cursor].0 == position {
                let (_, slot, is_live) = events[event_cursor];
                live[slot.index()] = is_live;
                event_cursor += 1;
            }
            if position == len {
                break;
            }
            let instruction_index =
                usize::try_from(block.instructions.start + position).expect("指令下标");
            let instruction = &body.instructions[instruction_index];
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
                    live_pointer(body, instruction_index, argument, &live)?;
                }
            }
            if let Op::Call(call) | Op::ForeignCall(call) = &instruction.op {
                escaping_call(body, call, args)?;
            }
        }
        match &block.terminator {
            Terminator::Return { values, .. } => {
                for &value in body.args(values) {
                    if body.values[value.index()].kind.provenance != Some(Provenance::Raw)
                        && has_stack_origin(body, value)
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
                let args = body.args(arguments);
                for &argument in args {
                    live_pointer(body, usize::MAX, argument, &live)?;
                }
                escaping_call(body, call, args)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn live_pointer(
    body: &Body,
    index: usize,
    value: ValueId,
    live: &[bool],
) -> Result<(), Diagnostic> {
    let dead: Vec<u32> = match direct_stack_origin(body, value) {
        DirectStackOrigin::None => return Ok(()),
        DirectStackOrigin::Single(slot) => {
            if live[slot.index()] {
                return Ok(());
            }
            vec![u32::try_from(slot.index()).expect("槽下标适配 u32")]
        }
        DirectStackOrigin::Complex => {
            let dead: Vec<u32> = stack_origins(body, value)
                .iter()
                .filter(|slot| !live[slot.index()])
                .map(|slot| u32::try_from(slot.index()).expect("槽下标适配 u32"))
                .collect();
            if dead.is_empty() {
                return Ok(());
            }
            dead
        }
    };
    let at = if index == usize::MAX {
        "terminator".to_owned()
    } else {
        format!("{:?}", body.instructions[index].op)
    };
    Err(invalid(&format!(
        "内存访问使用已结束寿命的栈槽引用（{}；值 #{}；位置 {}；死亡槽 {:?}）",
        body.name,
        value.index(),
        at,
        dead
    )))
}
fn escaping_call(
    body: &Body,
    call: &crate::body::Call,
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
            && !matches!(call.target, crate::body::CallTarget::Runtime(_))
        {
            return Err(invalid("逃逸调用不能保存当前 frame 的栈引用"));
        }
    }
    Ok(())
}
