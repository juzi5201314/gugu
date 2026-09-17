//! SharedHeap guard 数据流 verifier：确认 fresh payload 只被解析一次、guard 不跨切断点、
//! 派生地址不逃出 guard，以及共享字段屏障绑定同一个 token。
//!
//! 这些规则不能由 lowering 自己保证：缓存恢复的 LIR 必须重新通过同一份检查，因此这里只看
//! 指令与 value provenance，不依赖构造器的正确性。

use super::invalid;
use crate::Diagnostic;
use crate::frontend::gir::placement::PlacementKind;
use crate::lir::body::{Body, Op, Origin, Provenance, Terminator, ValueId, id, range};
use std::collections::BTreeMap;

/// token 在 body 内的状态；顺序即生命周期顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TokenState {
    Begun,
    Resolved,
    Ended,
}

pub(super) fn verify(body: &Body) -> Result<(), Diagnostic> {
    fresh_payload_resolution(body)?;
    let addresses = shared_addresses(body);
    guard_discipline(body, &addresses)
}

/// 每个 `GcAlloc(SharedHeap)` 的 fresh 值必须只有一个 use，且就是紧随其后的 `ResolveSharedHandle`。
fn fresh_payload_resolution(body: &Body) -> Result<(), Diagnostic> {
    for (index, instruction) in body.instructions.iter().enumerate() {
        let Op::GcAlloc {
            placement: PlacementKind::SharedHeap,
            ..
        } = &instruction.op
        else {
            continue;
        };
        let results = range(&instruction.results);
        if results.len() != 1 {
            return Err(invalid("SharedHeap 分配必须恰好产生一个 fresh payload"));
        }
        let value = ValueId(id(results.start));
        if body.values[value.index()].kind.provenance != Some(Provenance::GcHeap) {
            return Err(invalid("fresh shared payload 必须是 direct managed 值"));
        }
        let uses = &body.uses[range(&body.values[value.index()].uses)];
        if uses.len() != 1 {
            return Err(invalid("fresh shared payload 只能被解析一次，不能另有使用"));
        }
        let next = body
            .instructions
            .get(index + 1)
            .ok_or_else(|| invalid("fresh shared payload 之后必须紧跟解析"))?;
        if !matches!(next.op, Op::ResolveSharedHandle)
            || body.args(&next.arguments) != [value]
            || uses[0].site != crate::lir::body::UseSite::Instruction(id_instruction(index + 1))
        {
            return Err(invalid("fresh shared payload 必须紧跟它自己的解析"));
        }
    }
    Ok(())
}

fn id_instruction(index: usize) -> crate::lir::body::InstId {
    crate::lir::body::InstId(id(index))
}

/// 按 block 顺序检查 guard 生命周期、派生地址作用域与共享字段屏障的 token。
fn guard_discipline(
    body: &Body,
    addresses: &BTreeMap<ValueId, Vec<u32>>,
) -> Result<(), Diagnostic> {
    let mut states: BTreeMap<u32, TokenState> = BTreeMap::new();
    let mut stack: Vec<u32> = Vec::new();
    for block in &body.blocks {
        for instruction in &body.instructions[range(&block.instructions)] {
            let active = stack.last().copied();
            match &instruction.op {
                Op::SharedAccessBegin { token } => {
                    if *token == 0 || states.insert(*token, TokenState::Begun).is_some() {
                        return Err(invalid("shared access token 必须非零且在 body 内唯一"));
                    }
                    stack.push(*token);
                }
                Op::SharedAccessEnd { token } => match stack.pop() {
                    Some(open) if open == *token => {
                        if states.get(token) != Some(&TokenState::Resolved) {
                            return Err(invalid("shared access guard 必须在访问之后才结束"));
                        }
                        states.insert(*token, TokenState::Ended);
                    }
                    _ => return Err(invalid("shared access guard 的 begin/end 不配对")),
                },
                Op::SharedFieldBarrier { store, token }
                | Op::SharedFieldBarrierReserved { store, token, .. } => {
                    let store = &body.instructions[store.index()];
                    let bound = body.args(&store.arguments).first().is_some_and(|address| {
                        addresses
                            .get(address)
                            .is_some_and(|owners| owners.contains(token))
                    });
                    if active != Some(*token) || !bound {
                        return Err(invalid(
                            "共享字段屏障必须绑定当前 guard 与该 guard 内的 Store",
                        ));
                    }
                }
                Op::ForwardSharedHandle if active.is_some() => {
                    return Err(invalid("shared handle 不能在 active guard 内 forward"));
                }
                op => {
                    if active.is_some() && crosses_cut_point(op) {
                        return Err(invalid(
                            "shared access guard 不能跨 safepoint、调用、挂起、NoSafepointRegion 或 region 生命周期操作",
                        ));
                    }
                    for argument in address_positions(body, instruction) {
                        let Some(owners) = addresses.get(&argument) else {
                            continue;
                        };
                        if !owners.iter().any(|owner| stack.contains(owner)) {
                            return Err(invalid(
                                "shared 派生地址只能在派生它的 access guard 内使用",
                            ));
                        }
                    }
                    // 首次真实字段访问把当前 guard 推进到 resolved。
                    if let Some(token) = active
                        && address_positions(body, instruction)
                            .iter()
                            .any(|argument| addresses.contains_key(argument))
                    {
                        states.insert(token, TokenState::Resolved);
                    }
                }
            }
        }
        if stack.last().is_some() {
            return Err(invalid("shared access guard 不能跨 block 终结符或出口"));
        }
        if let Some(value) = terminator_escape(body, addresses, &block.terminator) {
            let _ = value;
            return Err(invalid("shared 派生地址不能进入终结符、边参数或返回值"));
        }
    }
    Ok(())
}

/// 一条指令是否是不能出现在 guard 内部的切断点。
fn crosses_cut_point(op: &Op) -> bool {
    if matches!(
        op,
        Op::SharedFieldBarrier { .. } | Op::SharedFieldBarrierReserved { .. }
    ) {
        // 共享字段屏障正是 guard 内部的记账步骤。
        return false;
    }
    if op.safepoint_kind().is_some() {
        return true;
    }
    matches!(
        op,
        Op::NoSafepointBegin(_)
            | Op::NoSafepointEnd(_)
            | Op::RegionPublish { .. }
            | Op::RegionReset { .. }
            | Op::RegionTransfer { .. }
            | Op::PromoteManaged { .. }
    )
}

/// 返回一条指令里处于「地址位置」的操作数。
///
/// 只有这些位置才要求 shared 身份已经解析；把 handle 当数据搬运（store 的值、调用参数、
/// 返回值）不要求 guard。
fn address_positions(body: &Body, instruction: &crate::lir::body::Instruction) -> Vec<ValueId> {
    let args = body.args(&instruction.arguments);
    match &instruction.op {
        Op::Load(_) | Op::Store(_) | Op::Atomic { .. } | Op::Memset { .. } | Op::PtrOffset => {
            args.first().copied().into_iter().collect()
        }
        Op::Memmove { .. } | Op::Memcpy { .. } => args.iter().take(2).copied().collect(),
        _ => Vec::new(),
    }
}

/// 终结符是否把 shared 派生地址当作数据带走。
fn terminator_escape(
    body: &Body,
    addresses: &BTreeMap<ValueId, Vec<u32>>,
    terminator: &Terminator,
) -> Option<ValueId> {
    fn first(values: &[ValueId], addresses: &BTreeMap<ValueId, Vec<u32>>) -> Option<ValueId> {
        values
            .iter()
            .find(|value| addresses.contains_key(value))
            .copied()
    }
    match terminator {
        Terminator::Branch { condition, .. }
        | Terminator::Switch {
            value: condition, ..
        } => addresses.get(condition).map(|_| *condition),
        Terminator::Invoke { arguments, .. } | Terminator::TailCall { arguments, .. } => {
            first(body.args(arguments), addresses)
        }
        Terminator::Return { values, .. } => first(body.args(values), addresses),
        Terminator::Jump(_)
        | Terminator::ResumePanic { .. }
        | Terminator::Trap { .. }
        | Terminator::Unreachable { .. } => None,
    }
}

/// 收集所有 shared 地址 value 及拥有它的 guard token 集合。
///
/// 地址包括两部分：每个 `SharedAccessBegin` 的 handle root（它在该 token 内被当作 payload
/// 基址），以及 guard 内从 shared 基址 `PtrOffset` 出来的派生值。同一个 handle 可以是多个
/// guard 的根（同一对象可以先后开多个 guard），因此每个地址记录一组 token；使用点只要落在
/// 其中任何一个仍然打开的 guard 内即可。从 payload 读出的嵌套 handle 是数据，可以在 guard
/// 结束后继续使用，因此不在这里登记。
fn shared_addresses(body: &Body) -> BTreeMap<ValueId, Vec<u32>> {
    let mut addresses: BTreeMap<ValueId, Vec<u32>> = BTreeMap::new();
    for block in &body.blocks {
        let mut stack: Vec<u32> = Vec::new();
        for instruction in &body.instructions[range(&block.instructions)] {
            match &instruction.op {
                Op::SharedAccessBegin { token } => {
                    if let Some(root) = body.args(&instruction.arguments).first().copied()
                        && body.values[root.index()].kind.provenance
                            == Some(Provenance::SharedHandle)
                    {
                        let owners = addresses.entry(root).or_default();
                        if !owners.contains(token) {
                            owners.push(*token);
                        }
                    }
                    stack.push(*token);
                }
                Op::SharedAccessEnd { .. } => {
                    stack.pop();
                }
                Op::PtrOffset => {
                    let Some(token) = stack.last().copied() else {
                        continue;
                    };
                    let Some(base) = body.args(&instruction.arguments).first().copied() else {
                        continue;
                    };
                    let shared_base =
                        body.values[base.index()].kind.provenance == Some(Provenance::SharedHandle);
                    if !shared_base {
                        continue;
                    }
                    for result in range(&instruction.results) {
                        let value = ValueId(id(result));
                        if body.values[value.index()].kind.provenance
                            == Some(Provenance::SharedHandle)
                            && matches!(body.values[value.index()].origin, Origin::Derived(_))
                        {
                            let owners = addresses.entry(value).or_default();
                            if !owners.contains(&token) {
                                owners.push(token);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    addresses
}
