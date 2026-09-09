//! poll budget：叶分类、预算数据流与预算化 poll 插入。
//!
//! 插入规则保证：任意 poll-free 路径的饱和成本不超过 [`POLL_BUDGET`]，
//! 且任何无界环都被 poll 或 `NoSafepoint` 边界切断；可证明有界的计数循环
//! 允许保留为 poll-free 环（由 verifier 复核 trip count 与成本）。
use super::loops::{self, CountedLoop, LoopInfo};
use super::policy::{POLL_BUDGET, POLL_FREE_LEAF_MAX_COST};
use super::rewrite::{Editor, InstRef, Term};
use crate::Diagnostic;
use crate::frontend::gir::body::CallKind;
use crate::frontend::hir;
use crate::lir::body::{
    BlockId, Body, CallTarget, Condition, IntOp, Op, Origin, PollSummary, Provenance,
    SafepointKind, Type, ValueId, ValueType, range,
};
use crate::lir::{invalid, verify};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

/// 每个实例的 (摘要, 是否 poll-free 叶)。
type CalleeInfo = BTreeMap<[u8; 32], (PollSummary, bool)>;

pub(crate) fn run_world(
    bodies: &mut Vec<Body>,
    module: &hir::Module,
    _profile: &crate::BackendCostProfile,
) -> Result<(), Diagnostic> {
    let index: BTreeMap<[u8; 32], usize> = bodies
        .iter()
        .enumerate()
        .map(|(index, body)| (body.instance, index))
        .collect();
    let callees: Vec<Vec<usize>> = bodies
        .iter()
        .map(|body| direct_callees(body, &index))
        .collect();
    let components = call_sccs(&callees);
    let mut component_of = vec![0usize; bodies.len()];
    for (component, members) in components.iter().enumerate() {
        for &body in members {
            component_of[body] = component;
        }
    }
    let recursive: Vec<bool> = (0..bodies.len())
        .map(|body| callees[body].contains(&body) || components[component_of[body]].len() > 1)
        .collect();
    let mut info: CalleeInfo = BTreeMap::new();
    for component in &components {
        loop {
            let mut changed = false;
            for &body in component {
                let (summary, leaf) = analyze(&bodies[body], &info, recursive[body]);
                let key = bodies[body].instance;
                if info.get(&key).copied() != Some((summary, leaf)) {
                    info.insert(key, (summary, leaf));
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
    let mut rebuilt = Vec::with_capacity(bodies.len());
    for body in bodies.drain(..) {
        let key = body.instance;
        let leaf = info.get(&key).is_some_and(|(_, leaf)| *leaf);
        let recursive_body = recursive[index[&key]];
        let mut editor = Editor::new(body);
        if !leaf {
            insert_polls(&mut editor, &info)?;
        }
        mark_calls(&mut editor, &info);
        if leaf {
            remove_entry_stack_check(&mut editor)?;
        }
        let mut body = editor.finish()?;
        let (summary, _) = analyze(&body, &info, recursive_body);
        body.poll_summary = summary;
        verify::verify(&body, module)?;
        rebuilt.push(body);
    }
    *bodies = rebuilt;
    Ok(())
}

fn direct_callees(body: &Body, index: &BTreeMap<[u8; 32], usize>) -> Vec<usize> {
    let mut callees = BTreeSet::new();
    let mut visit = |call: &crate::lir::body::Call| {
        if let CallTarget::Instance(key) = call.target
            && let Some(target) = index.get(&key)
        {
            callees.insert(*target);
        }
    };
    for instruction in &body.instructions {
        match &instruction.op {
            Op::Call(call) | Op::ForeignCall(call) => visit(call),
            _ => {}
        }
    }
    for block in &body.blocks {
        match &block.terminator {
            crate::lir::body::Terminator::Invoke { call, .. }
            | crate::lir::body::Terminator::TailCall { call, .. } => visit(call),
            _ => {}
        }
    }
    callees.into_iter().collect()
}

/// 调用图 Tarjan；边指向被调者，分量按被调者优先的顺序返回。
fn call_sccs(callees: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let count = callees.len();
    let mut index = vec![u32::MAX; count];
    let mut low = vec![0u32; count];
    let mut on_stack = vec![false; count];
    let mut stack = Vec::new();
    let mut next = 0u32;
    let mut components = Vec::new();
    struct Frame {
        node: usize,
        next: usize,
    }
    for root in 0..count {
        if index[root] != u32::MAX {
            continue;
        }
        let mut frames = vec![Frame {
            node: root,
            next: 0,
        }];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(frame) = frames.last_mut() {
            if frame.next < callees[frame.node].len() {
                let successor = callees[frame.node][frame.next];
                frame.next += 1;
                if index[successor] == u32::MAX {
                    index[successor] = next;
                    low[successor] = next;
                    next += 1;
                    stack.push(successor);
                    on_stack[successor] = true;
                    frames.push(Frame {
                        node: successor,
                        next: 0,
                    });
                } else if on_stack[successor] {
                    low[frame.node] = low[frame.node].min(index[successor]);
                }
                continue;
            }
            let node = frame.node;
            frames.pop();
            if let Some(parent) = frames.last() {
                low[parent.node] = low[parent.node].min(low[node]);
            }
            if low[node] == index[node] {
                let mut component = Vec::new();
                loop {
                    let member = stack.pop().expect("Tarjan 栈包含当前节点");
                    on_stack[member] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                component.sort_unstable();
                components.push(component);
            }
        }
    }
    components
}

/// 计算摘要并分类叶。
fn analyze(body: &Body, info: &CalleeInfo, recursive: bool) -> (PollSummary, bool) {
    let (_, cost) = body_costs(body, info);
    let cycle = clean_cycle(body, info).is_some();
    let entry_stack_check = entry_has_stack_check(body);
    let leaf = !recursive && classify_leaf(body, cost, cycle);
    (
        PollSummary {
            entry_stack_check,
            poll_free_cost: cost,
            has_poll_free_cycle: cycle,
        },
        leaf,
    )
}

fn entry_has_stack_check(body: &Body) -> bool {
    let block = &body.blocks[body.entry.index()];
    range(&block.instructions).any(|index| matches!(body.instructions[index].op, Op::StackCheck))
}

fn classify_leaf(body: &Body, cost: u32, cycle: bool) -> bool {
    if cycle || cost > POLL_FREE_LEAF_MAX_COST {
        return false;
    }
    if body.stack_slots.iter().any(|slot| slot.bytes != 0)
        || body.signature.sret.is_some()
        || !body.signature.by_value.is_empty()
        || !body.environments.is_empty()
        || body
            .signature
            .parameters
            .iter()
            .any(|parameter| parameter.provenance == Some(Provenance::Stack))
        || body.edges.iter().any(|edge| edge.unwind)
    {
        return false;
    }
    for block in &body.blocks {
        if block.cleanup {
            return false;
        }
        match &block.terminator {
            crate::lir::body::Terminator::Invoke { .. }
            | crate::lir::body::Terminator::TailCall { .. }
            | crate::lir::body::Terminator::ResumePanic { .. }
            | crate::lir::body::Terminator::Trap { .. } => return false,
            _ => {}
        }
    }
    for instruction in &body.instructions {
        if matches!(instruction.op, Op::Call(_) | Op::ForeignCall(_)) {
            return false;
        }
        if let Some(point) = instruction.safepoint
            && body.safepoints[point.index()].kind != SafepointKind::StackCheck
        {
            return false;
        }
    }
    true
}

/// 单条指令的 (是否切断, 成本)。
fn op_flow(op: &Op, assembly: &[hir::Assembly], info: &CalleeInfo) -> (bool, u32) {
    if let Op::Call(call) | Op::ForeignCall(call) = op {
        if call.kind == CallKind::Managed
            && let CallTarget::Instance(key) = call.target
            && let Some((summary, true)) = info.get(&key)
        {
            return (false, 5u32.saturating_add(summary.poll_free_cost));
        }
        return (
            op.poll_cut_point_with(assembly),
            op.poll_cost_with(assembly),
        );
    }
    (
        op.poll_cut_point_with(assembly),
        op.poll_cost_with(assembly),
    )
}

fn block_flow<'a>(
    ops: impl IntoIterator<Item = &'a Op>,
    assembly: &[hir::Assembly],
    info: &CalleeInfo,
    mut cost: u32,
) -> u32 {
    for op in ops {
        let (cut, step) = op_flow(op, assembly, info);
        cost = if cut {
            0
        } else {
            cost.saturating_add(step).min(POLL_BUDGET.saturating_add(1))
        };
    }
    cost
}

/// 通用预算数据流；返回每个 block 的输出成本与全局最大值。
fn propagate(
    count: usize,
    entry: BlockId,
    order: &[BlockId],
    successors: impl Fn(BlockId) -> Vec<BlockId>,
    mut out: impl FnMut(BlockId, u32) -> u32,
) -> (Vec<u32>, u32) {
    let mut in_cost = vec![0u32; count];
    let mut out_cost = vec![0u32; count];
    let mut max_seen = 0;
    in_cost[entry.index()] = 0;
    loop {
        let mut changed = false;
        for &block in order {
            let current = out(block, in_cost[block.index()]);
            if out_cost[block.index()] != current {
                out_cost[block.index()] = current;
                changed = true;
            }
            max_seen = max_seen.max(current).max(in_cost[block.index()]);
            for successor in successors(block) {
                if in_cost[successor.index()] < current {
                    in_cost[successor.index()] = current;
                    changed = true;
                }
            }
        }
        if !changed {
            return (out_cost, max_seen);
        }
    }
}

fn body_successors(body: &Body, block: BlockId) -> Vec<BlockId> {
    let mut successors = Vec::new();
    verify::edges(body, &body.blocks[block.index()].terminator, |edge| {
        successors.push(body.edges[edge.index()].to)
    });
    successors
}

/// 从 body 自身的 `poll_free_leaf` 标记构造叶信息（叶成本未知按 0 计）。
fn leaf_flags(body: &Body) -> CalleeInfo {
    let mut info = CalleeInfo::new();
    let mut record = |call: &crate::lir::body::Call| {
        if call.poll_free_leaf
            && let CallTarget::Instance(key) = call.target
        {
            info.insert(key, (PollSummary::default(), true));
        }
    };
    for instruction in &body.instructions {
        match &instruction.op {
            Op::Call(call) | Op::ForeignCall(call) => record(call),
            _ => {}
        }
    }
    for block in &body.blocks {
        match &block.terminator {
            crate::lir::body::Terminator::Invoke { call, .. }
            | crate::lir::body::Terminator::TailCall { call, .. } => record(call),
            _ => {}
        }
    }
    info
}

/// verifier 视角的 poll-free 路径饱和成本。
pub(crate) fn body_poll_free_cost(body: &Body) -> u32 {
    body_costs(body, &leaf_flags(body)).1
}

/// verifier 视角的 poll-free 环位置。
pub(crate) fn body_clean_cycle(body: &Body) -> Option<BlockId> {
    clean_cycle(body, &leaf_flags(body))
}

/// 该 block 是否落在可证明有界的计数循环内。
pub(crate) fn bounded_counted_cycle(body: &Body, block: BlockId) -> bool {
    let editor = Editor::new(body.clone());
    let info = leaf_flags(body);
    loops::analyze(&editor).iter().any(|natural| {
        natural.blocks.contains(&block)
            && natural.counted.is_some()
            && loop_total_cost(&editor, natural, &info) <= POLL_BUDGET
    })
}

/// 可证明有界的计数循环回边：它们不构成 poll-free 环，也不参与成本饱和。
fn bounded_back_edges(editor: &Editor, info: &CalleeInfo) -> BTreeSet<(BlockId, BlockId)> {
    let mut edges = BTreeSet::new();
    for natural in loops::analyze(editor) {
        if natural.counted.is_none() || loop_total_cost(editor, &natural, info) > POLL_BUDGET {
            continue;
        }
        for (_, edge) in editor.edges() {
            if edge.to == natural.header && natural.blocks.contains(&edge.from) {
                edges.insert((edge.from, edge.to));
            }
        }
    }
    edges
}

fn body_costs(body: &Body, info: &CalleeInfo) -> (Vec<u32>, u32) {
    let bounded = bounded_back_edges(&Editor::new(body.clone()), info);
    let order = super::graph::reverse_postorder(body);
    let entry = body.entry;
    propagate(
        body.blocks.len(),
        entry,
        &order,
        |block| {
            body_successors(body, block)
                .into_iter()
                .filter(|to| !bounded.contains(&(block, *to)))
                .collect()
        },
        |block, cost| {
            block_flow(
                range(&body.blocks[block.index()].instructions)
                    .map(|index| &body.instructions[index].op),
                &body.assembly,
                info,
                cost,
            )
        },
    )
}

fn editor_costs(editor: &Editor, info: &CalleeInfo) -> (Vec<u32>, u32) {
    let order = editor.reverse_postorder();
    propagate(
        editor.blocks.len(),
        editor.entry(),
        &order,
        |block| editor.successors(block),
        |block, cost| {
            block_flow(
                (0..editor.instruction_count(block))
                    .filter(|index| !editor.instruction((block, *index)).removed)
                    .map(|index| &editor.instruction((block, index)).op),
                &editor.assembly,
                info,
                cost,
            )
        },
    )
}

/// 找到第一个处于 poll-free 环内的 block（clean 子图的 SCC 或自环）。
fn clean_cycle(body: &Body, info: &CalleeInfo) -> Option<BlockId> {
    let bounded = bounded_back_edges(&Editor::new(body.clone()), info);
    let clean: Vec<bool> = body
        .blocks
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let block = BlockId(super::super::body::id(index));
            let terminator_cut = matches!(
                body.blocks[index].terminator,
                crate::lir::body::Terminator::Invoke { .. }
                    | crate::lir::body::Terminator::TailCall { .. }
            );
            !terminator_cut
                && range(&block_range(body, block)).all(|at| {
                    let op = &body.instructions[at].op;
                    !op_flow(op, &body.assembly, info).0
                })
        })
        .collect();
    let mut state = vec![0u8; body.blocks.len()];
    for root in 0..body.blocks.len() {
        if !clean[root] || state[root] != 0 {
            continue;
        }
        let mut stack = vec![(BlockId(super::super::body::id(root)), 0usize)];
        state[root] = 1;
        while let Some((block, next)) = stack.last_mut() {
            let successors: Vec<BlockId> = body_successors(body, *block)
                .into_iter()
                .filter(|successor| {
                    clean[successor.index()] && !bounded.contains(&(*block, *successor))
                })
                .collect();
            if *next < successors.len() {
                let successor = successors[*next];
                *next += 1;
                match state[successor.index()] {
                    0 => {
                        state[successor.index()] = 1;
                        stack.push((successor, 0));
                    }
                    1 => return Some(successor),
                    _ => {}
                }
            } else {
                state[block.index()] = 2;
                stack.pop();
            }
        }
    }
    None
}

fn block_range(body: &Body, block: BlockId) -> std::ops::Range<u32> {
    body.blocks[block.index()].instructions.clone()
}

fn editor_clean_cycle(
    editor: &Editor,
    info: &CalleeInfo,
    exempt: &BTreeSet<BlockId>,
) -> Option<BlockId> {
    let clean: Vec<bool> = editor
        .live_blocks()
        .into_iter()
        .map(|block| {
            let terminator_cut = matches!(
                editor.terminator(block),
                Term::Invoke { .. } | Term::TailCall { .. }
            );
            !terminator_cut
                && (0..editor.instruction_count(block)).all(|index| {
                    let instruction = editor.instruction((block, index));
                    instruction.removed || !op_flow(&instruction.op, &editor.assembly, info).0
                })
        })
        .collect::<Vec<_>>();
    // live_blocks 可能不含已删除 block；用 map 保序。
    let live = editor.live_blocks();
    let clean_of = |block: BlockId| -> bool {
        !exempt.contains(&block)
            && live
                .iter()
                .position(|candidate| *candidate == block)
                .is_some_and(|index| clean[index])
    };
    let mut state: BTreeMap<BlockId, u8> = BTreeMap::new();
    for root in &live {
        if !clean_of(*root) || state.get(root).copied().unwrap_or(0) != 0 {
            continue;
        }
        let mut stack = vec![(*root, 0usize)];
        state.insert(*root, 1);
        while let Some((block, next)) = stack.last_mut() {
            let successors: Vec<BlockId> = editor
                .successors(*block)
                .into_iter()
                .filter(|successor| clean_of(*successor))
                .collect();
            if *next < successors.len() {
                let successor = successors[*next];
                *next += 1;
                match state.get(&successor).copied().unwrap_or(0) {
                    0 => {
                        state.insert(successor, 1);
                        stack.push((successor, 0));
                    }
                    1 => return Some(successor),
                    _ => {}
                }
            } else {
                state.insert(*block, 2);
                stack.pop();
            }
        }
    }
    None
}

/// 把 `poll_free_leaf` 写入所有 managed 直接调用。
fn mark_calls(editor: &mut Editor, info: &CalleeInfo) {
    for block in editor.live_blocks() {
        for index in 0..editor.instruction_count(block) {
            let at: InstRef = (block, index);
            let instruction = editor.instruction(at).clone();
            if instruction.removed {
                continue;
            }
            let (Op::Call(call) | Op::ForeignCall(call)) = &instruction.op else {
                continue;
            };
            if call.kind != CallKind::Managed {
                continue;
            }
            let CallTarget::Instance(key) = call.target else {
                continue;
            };
            let leaf = info.get(&key).is_some_and(|(_, leaf)| *leaf);
            if leaf == call.poll_free_leaf {
                continue;
            }
            let mut call = call.clone();
            call.poll_free_leaf = leaf;
            let op = match instruction.op {
                Op::Call(_) => Op::Call(call),
                _ => Op::ForeignCall(call),
            };
            editor.set_op(at, op);
        }
    }
}

fn remove_entry_stack_check(editor: &mut Editor) -> Result<(), Diagnostic> {
    let entry = editor.entry();
    for index in 0..editor.instruction_count(entry) {
        if matches!(editor.instruction((entry, index)).op, Op::StackCheck) {
            editor.remove_instruction((entry, index))?;
        }
    }
    Ok(())
}

/// 在 `block` 内插入一个预算化 poll。
fn insert_poll(editor: &mut Editor, block: BlockId, interval: u32) -> Result<(), Diagnostic> {
    if (0..editor.instruction_count(block)).any(|index| {
        matches!(
            editor.instruction((block, index)).op,
            Op::NoSafepointBegin(_) | Op::NoSafepointEnd(_)
        )
    }) {
        return Ok(());
    }
    let interval = NonZeroU32::new(interval).expect("poll interval 非零");
    let count = editor.instruction_count(block);
    let memory = (0..count).find(|index| editor.instruction((block, *index)).memory.is_some());
    let (position, input, terminator) = match memory {
        Some(position) => (
            position,
            editor
                .instruction((block, position))
                .memory
                .expect("内存指令")
                .input,
            false,
        ),
        None => match editor.terminator_memory(block) {
            Some(input) => (count, input, true),
            None => (
                count,
                editor.block(block).expect("活跃 block").params[0].value,
                false,
            ),
        },
    };
    let (_, output) = editor.insert_memory(
        (block, position),
        Op::SafepointPoll { interval },
        &[],
        &[],
        input,
    );
    if memory.is_some() {
        editor.set_memory_input((block, position + 1), output);
    } else if terminator {
        editor.set_terminator_memory(block, output);
    } else {
        for edge in editor.terminator(block).edges() {
            if let Some(data) = editor.edge(edge) {
                let to = data.to;
                let mut arguments = data.arguments.clone();
                if arguments.is_empty() {
                    continue;
                }
                arguments[0] = output;
                editor.redirect_edge(edge, to, arguments);
            }
        }
    }
    Ok(())
}

fn insert_polls(editor: &mut Editor, info: &CalleeInfo) -> Result<(), Diagnostic> {
    // 已被 strip mining 的内层 header 由外层 poll 覆盖，不再单独插点。
    let mut stripped: BTreeSet<BlockId> = BTreeSet::new();
    for _ in 0..256 {
        let loops = loops::analyze(editor);
        let exempt: BTreeSet<BlockId> = loops
            .iter()
            .filter(|natural| {
                natural
                    .counted
                    .as_ref()
                    .is_some_and(|_| loop_total_cost(editor, natural, info) <= POLL_BUDGET)
            })
            .flat_map(|natural| natural.blocks.iter().copied())
            .collect();
        let mut handled = false;
        for natural in &loops {
            if stripped.contains(&natural.header) || loop_cut(editor, natural, info) {
                continue;
            }
            if natural
                .counted
                .as_ref()
                .is_some_and(|_| loop_total_cost(editor, natural, info) <= POLL_BUDGET)
            {
                continue;
            }
            if let Some(counted) = &natural.counted
                && strip_mine(editor, natural, counted, info)?
            {
                stripped.insert(natural.header);
                handled = true;
                break;
            }
            let cost = loop_cycle_cost(editor, natural, info);
            let interval = (POLL_BUDGET / cost.max(1)).max(1);
            let latch = natural.latches[0];
            insert_poll(editor, latch, interval)?;
            handled = true;
            break;
        }
        if handled {
            continue;
        }
        if let Some(block) = editor_clean_cycle(editor, info, &exempt) {
            insert_poll(editor, block, 1)?;
            continue;
        }
        if let Some(block) = overflow_block(editor, info, &exempt) {
            insert_poll(editor, block, 1)?;
            continue;
        }
        return Ok(());
    }
    Err(invalid("poll 插入未在有限轮内收敛"))
}

/// 循环内是否已有切断点（含显式 poll 与 statepoint）。
fn loop_cut(editor: &Editor, natural: &LoopInfo, info: &CalleeInfo) -> bool {
    natural.blocks.iter().any(|block| {
        matches!(
            editor.terminator(*block),
            Term::Invoke { .. } | Term::TailCall { .. }
        ) || (0..editor.instruction_count(*block)).any(|index| {
            let instruction = editor.instruction((*block, index));
            !instruction.removed && op_flow(&instruction.op, &editor.assembly, info).0
        })
    })
}

/// 循环一次迭代的保守成本上界（各 block 成本之和）。
fn loop_cycle_cost(editor: &Editor, natural: &LoopInfo, info: &CalleeInfo) -> u32 {
    natural
        .blocks
        .iter()
        .map(|block| {
            block_flow(
                (0..editor.instruction_count(*block))
                    .filter(|index| !editor.instruction((*block, *index)).removed)
                    .map(|index| &editor.instruction((*block, index)).op),
                &editor.assembly,
                info,
                0,
            )
        })
        .fold(0u32, |total, cost| {
            total
                .saturating_add(cost)
                .min(POLL_BUDGET.saturating_add(1))
        })
}

/// 进入循环前的成本：preheader 所在 block 的累计成本（不含循环体，避免回边饱和）。
fn entry_cost(editor: &Editor, natural: &LoopInfo, info: &CalleeInfo) -> u32 {
    let Some(preheader) = editor
        .predecessors(natural.header)
        .into_iter()
        .filter_map(|edge| editor.edge(edge))
        .find(|edge| !natural.blocks.contains(&edge.from))
    else {
        return 0;
    };
    editor_costs(editor, info)
        .0
        .get(preheader.from.index())
        .copied()
        .unwrap_or(0)
}

/// 进入循环前的成本 + 全部迭代成本的保守上界。
fn loop_total_cost(editor: &Editor, natural: &LoopInfo, info: &CalleeInfo) -> u32 {
    let Some(counted) = &natural.counted else {
        return POLL_BUDGET.saturating_add(1);
    };
    let Some(trips) = trip_count(editor, natural, counted) else {
        return POLL_BUDGET.saturating_add(1);
    };
    let entry = entry_cost(editor, natural, info);
    let cycle = loop_cycle_cost(editor, natural, info);
    let total = u128::from(entry) + u128::from(trips) * u128::from(cycle);
    u32::try_from(total).unwrap_or(POLL_BUDGET.saturating_add(1))
}

fn trip_count(editor: &Editor, natural: &LoopInfo, counted: &CountedLoop) -> Option<u64> {
    let header = natural.header;
    let params = &editor.block(header)?.params;
    let position = params
        .iter()
        .position(|param| param.value == counted.induction)?;
    let preheader = editor
        .predecessors(header)
        .into_iter()
        .filter_map(|edge| editor.edge(edge))
        .find(|edge| !natural.blocks.contains(&edge.from))?;
    let Some(initial) = super::constants::constant_bits(editor, preheader.arguments[position])
    else {
        // strip mining 后的内层循环由循环外的 `iv + stride` 限定每次迭代次数。
        return strip_chunk_trips(editor, natural, counted);
    };
    let bound = super::constants::constant_bits(editor, counted.bound)?;
    if counted.step <= 0 {
        return None;
    }
    let step = u128::from(counted.step as u64);
    let initial = u128::from(initial);
    let bound = u128::from(bound);
    let trips = match counted.condition {
        Condition::Lt if bound > initial => (bound - initial + step - 1) / step,
        Condition::Lt => 0,
        Condition::Le if bound >= initial => (bound - initial) / step + 1,
        Condition::Le => 0,
        _ => return None,
    };
    u64::try_from(trips).ok()
}

/// strip mining 后的内层循环：latch 以 `iv_next < chunk_limit` 退出，而 `chunk_limit` 在
/// 循环外由 `iv + stride` 计算，因此每次进入内层至多迭代 `stride / step` 次。
fn strip_chunk_trips(editor: &Editor, natural: &LoopInfo, counted: &CountedLoop) -> Option<u64> {
    if counted.step <= 0 {
        return None;
    }
    let Term::Branch { condition, .. } = editor.terminator(counted.latch).clone() else {
        return None;
    };
    let test = editor.defining_instruction(condition)?;
    let Op::Compare {
        condition: Condition::Lt,
        ..
    } = editor.instruction(test).op
    else {
        return None;
    };
    let [_iv_next, limit] = editor.instruction(test).arguments.as_slice() else {
        return None;
    };
    let at = editor.defining_instruction(*limit)?;
    // chunk_limit 必须定义在循环外的外层 header，且由该 block 的参数与常量相加得到。
    let outer = editor
        .predecessors(natural.header)
        .into_iter()
        .filter_map(|edge| editor.edge(edge))
        .find(|edge| !natural.blocks.contains(&edge.from))?
        .from;
    if at.0 != outer {
        return None;
    }
    let Op::Integer(IntOp::Add) = editor.instruction(at).op else {
        return None;
    };
    let [base, stride] = editor.instruction(at).arguments.as_slice() else {
        return None;
    };
    if !editor
        .block(outer)?
        .params
        .iter()
        .any(|param| param.value == *base)
    {
        return None;
    }
    let stride = super::constants::constant_bits(editor, *stride)?;
    u64::try_from(u128::from(stride) / u128::from(counted.step as u64)).ok()
}

fn overflow_block(
    editor: &Editor,
    info: &CalleeInfo,
    exempt: &BTreeSet<BlockId>,
) -> Option<BlockId> {
    let bounded = bounded_back_edges(editor, info);
    let order = editor.reverse_postorder();
    let mut in_cost: BTreeMap<BlockId, u32> = BTreeMap::new();
    in_cost.insert(editor.entry(), 0);
    loop {
        let mut changed = false;
        for block in &order {
            let current = block_flow(
                (0..editor.instruction_count(*block))
                    .filter(|index| !editor.instruction((*block, *index)).removed)
                    .map(|index| &editor.instruction((*block, index)).op),
                &editor.assembly,
                info,
                in_cost.get(block).copied().unwrap_or(0),
            );
            for successor in editor.successors(*block) {
                if bounded.contains(&(*block, successor)) {
                    continue;
                }
                if in_cost.get(&successor).copied().unwrap_or(0) < current {
                    in_cost.insert(successor, current);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    order.into_iter().find(|block| {
        !exempt.contains(block)
            && in_cost.get(block).copied().unwrap_or(0) > POLL_BUDGET
            && !(0..editor.instruction_count(*block)).any(|index| {
                matches!(
                    editor.instruction((*block, index)).op,
                    Op::NoSafepointBegin(_) | Op::NoSafepointEnd(_)
                )
            })
    })
}

/// 把计数循环 strip mining 成「内层 poll-free + 外层每次 poll」。
fn strip_mine(
    editor: &mut Editor,
    natural: &LoopInfo,
    counted: &CountedLoop,
    info: &CalleeInfo,
) -> Result<bool, Diagnostic> {
    if counted.step <= 0 || !matches!(counted.condition, Condition::Lt | Condition::Le) {
        return Ok(false);
    }
    let header = natural.header;
    let latch = counted.latch;
    let cycle = loop_cycle_cost(editor, natural, info);
    if cycle == 0 || cycle > POLL_BUDGET {
        return Ok(false);
    }
    // 内层 strip 后多出 chunk 边界比较与分支，预留该成本与进入循环前的成本。
    let entry = entry_cost(editor, natural, info);
    let available = POLL_BUDGET.saturating_sub(entry);
    let chunk = u64::from(available / cycle.saturating_add(2).max(1));
    if chunk < 2 {
        return Ok(false);
    }
    let Some(trips) = trip_count(editor, natural, counted) else {
        return Ok(false);
    };
    if chunk >= trips {
        return Ok(false);
    }
    let step = counted.step as u64;
    let stride = chunk.checked_mul(step);
    let Some(stride) = stride else {
        return Ok(false);
    };
    let Some(bound) = super::constants::constant_bits(editor, counted.bound) else {
        return Ok(false);
    };
    if bound.checked_add(stride).is_none() {
        return Ok(false);
    }
    let mut exits = editor
        .terminator(header)
        .edges()
        .into_iter()
        .filter(|edge| {
            editor
                .edge(*edge)
                .is_some_and(|data| !natural.blocks.contains(&data.to))
        });
    let Some(exit_edge) = exits.next() else {
        return Ok(false);
    };
    if exits.next().is_some() {
        return Ok(false);
    }
    let (exit_target, exit_values) = {
        let exit = editor.edge(exit_edge).expect("活跃边");
        (exit.to, exit.arguments.clone())
    };
    let header_params: Vec<ValueId> = editor
        .block(header)
        .expect("活跃 block")
        .params
        .iter()
        .map(|param| param.value)
        .collect();
    let Some(position) = header_params
        .iter()
        .position(|value| *value == counted.induction)
    else {
        return Ok(false);
    };
    let iv_kind = editor.kind(counted.induction);
    let param_kinds: Vec<_> = editor
        .block(header)
        .expect("活跃 block")
        .params
        .iter()
        .map(|param| (editor.kind(param.value), param.source))
        .collect();
    let source = editor.block(header).expect("活跃 block").source.clone();
    let cleanup = editor.block(header).expect("活跃 block").cleanup;
    // 外层 header `H2` 携带与内层相同的循环参数；bound 在入口物化，避免依赖循环内定义。
    let outer = editor.add_block(param_kinds.clone(), source.clone(), cleanup);
    let outer_params: Vec<ValueId> = editor
        .block(outer)
        .expect("活跃 block")
        .params
        .iter()
        .map(|param| param.value)
        .collect();
    let outer_iv = outer_params[position];
    let stride_const = editor.constant(stride, iv_kind.ty);
    let bound_const = editor.constant(bound, iv_kind.ty);
    let chunk_limit = editor.emit(
        outer,
        Op::Integer(IntOp::Add),
        &[outer_iv, stride_const],
        &[(iv_kind, Origin::None)],
    )[0];
    let outer_condition = editor.emit(
        outer,
        Op::Compare {
            condition: counted.condition,
            signed: counted.signed,
        },
        &[outer_iv, bound_const],
        &[(ValueType::scalar(Type::I8), Origin::None)],
    )[0];
    let inner = editor.add_edge(outer, header, outer_params.clone());
    let exit_arguments: Vec<ValueId> = exit_values
        .iter()
        .map(
            |value| match header_params.iter().position(|param| param == value) {
                Some(index) => outer_params[index],
                None => *value,
            },
        )
        .collect();
    let leave = editor.add_edge(outer, exit_target, exit_arguments);
    editor.set_terminator(
        outer,
        Term::Branch {
            condition: outer_condition,
            yes: inner,
            no: leave,
        },
    );
    // preheader 改为进入外层。
    let preheader_edge = editor
        .predecessors(header)
        .into_iter()
        .find(|edge| {
            editor
                .edge(*edge)
                .is_some_and(|data| !natural.blocks.contains(&data.from))
        })
        .expect("循环已有 preheader");
    let preheader_arguments = editor
        .edge(preheader_edge)
        .expect("活跃边")
        .arguments
        .clone();
    editor.redirect_edge(preheader_edge, outer, preheader_arguments);
    // 内层 latch：`iv_next < chunk_limit ? header : outer_latch`。
    let Term::Jump(latch_edge) = editor.terminator(latch).clone() else {
        return Ok(false);
    };
    let latch_arguments = editor.edge(latch_edge).expect("活跃边").arguments.clone();
    let iv_next = latch_arguments[position];
    let latch_condition = editor.emit(
        latch,
        Op::Compare {
            condition: Condition::Lt,
            signed: counted.signed,
        },
        &[iv_next, chunk_limit],
        &[(ValueType::scalar(Type::I8), Origin::None)],
    )[0];
    let outer_latch = editor.add_block(param_kinds, source, cleanup);
    let outer_latch_params: Vec<ValueId> = editor
        .block(outer_latch)
        .expect("活跃 block")
        .params
        .iter()
        .map(|param| param.value)
        .collect();
    let (_, polled) = editor.insert_memory(
        (outer_latch, 0),
        Op::SafepointPoll {
            interval: NonZeroU32::new(1).expect("poll interval 非零"),
        },
        &[],
        &[],
        outer_latch_params[0],
    );
    let mut back_arguments = outer_latch_params.clone();
    back_arguments[0] = polled;
    let back = editor.add_edge(outer_latch, outer, back_arguments);
    editor.set_terminator(outer_latch, Term::Jump(back));
    let again = editor.add_edge(latch, outer_latch, latch_arguments);
    editor.set_terminator(
        latch,
        Term::Branch {
            condition: latch_condition,
            yes: latch_edge,
            no: again,
        },
    );
    Ok(true)
}
