//! 热/冷块布局与分支收缩：先 rel32，只许长变短，按 code offset 迭代到不动点。

use crate::lir::body::{Block, BlockId, Body, EdgeId, Op, Terminator};

use super::encode::assemble;
use super::inst::{Inst, Operand, Sequence};
use super::lower::LoweringError;
use super::table::{self, FormId, OperandKind};

/// 一个已布局块：LIR 身份、热/冷、布局下标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LayoutBlock {
    pub id: BlockId,
    pub hot: bool,
    pub order: u32,
}

/// 函数级布局：热块在前，冷块按 RPO 接在后面。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BlockLayout {
    pub blocks: Vec<LayoutBlock>,
    /// `BlockId.0` → 布局下标。
    pub index_of: Vec<u32>,
}

/// 计算热/冷并排出 entry 的 reverse postorder。
pub(crate) fn schedule(body: &Body) -> BlockLayout {
    let cold = classify_cold(body);
    let rpo = reverse_postorder(body);
    let mut hot = Vec::new();
    let mut cold_blocks = Vec::new();
    for id in rpo {
        if cold[id.index()] {
            cold_blocks.push(id);
        } else {
            hot.push(id);
        }
    }
    let mut blocks = Vec::with_capacity(body.blocks.len());
    for id in hot.into_iter().chain(cold_blocks) {
        blocks.push(LayoutBlock {
            id,
            hot: !cold[id.index()],
            order: 0,
        });
    }
    for (order, block) in blocks.iter_mut().enumerate() {
        block.order = u32::try_from(order).expect("块数适配 u32");
    }
    let mut index_of = vec![0; body.blocks.len()];
    for block in &blocks {
        index_of[block.id.index()] = block.order;
    }
    BlockLayout { blocks, index_of }
}

/// 条件分支的热边优先 fallthrough。返回 `(taken, fallthrough)` 的 LIR 边。
pub(crate) fn branch_order(
    body: &Body,
    layout: &BlockLayout,
    yes: EdgeId,
    no: EdgeId,
) -> (EdgeId, EdgeId, bool) {
    let yes_hot = edge_is_hot(body, layout, yes);
    let no_hot = edge_is_hot(body, layout, no);
    if yes_hot == no_hot {
        // 未知保持 GIR successor 顺序：yes 为 taken、no 为 fallthrough。
        return (yes, no, false);
    }
    if no_hot {
        (yes, no, false)
    } else {
        (no, yes, true)
    }
}

/// 若 Jump 目标是布局中的下一块，省略 jmp。
pub(crate) fn jump_falls_through(
    body: &Body,
    layout: &BlockLayout,
    from: BlockId,
    edge: EdgeId,
) -> bool {
    let target = body.edges[edge.index()].to;
    let from_order = layout.index_of[from.index()];
    let Some(next) = from_order.checked_add(1) else {
        return false;
    };
    layout
        .blocks
        .get(usize::try_from(next).expect("布局下标适配 usize"))
        .is_some_and(|block| block.id == target)
}

/// 先 rel32，只许长变短；外部/冷 stub/`call` 保持 rel32 reloc。
pub(crate) fn relax(sequence: &mut Sequence) -> Result<u32, LoweringError> {
    let mut rel8_count = 0_u32;
    loop {
        let assembled = assemble(sequence).map_err(|_| LoweringError::InvalidOperands)?;
        let mut changed = false;
        for (inst_index, start) in &assembled.instruction_offsets {
            let index = usize::try_from(*inst_index).expect("指令下标适配 usize");
            let inst = &mut sequence.instructions[index];
            let Some(rel32) = rel32_local_label(inst) else {
                continue;
            };
            let form = table::form(inst.form);
            let inst_end = start
                .checked_add(assembled_len(&assembled, index))
                .ok_or(LoweringError::InvalidOperands)?;
            let target =
                assembled.labels[usize::try_from(rel32.label.0).expect("标签编号适配 usize")];
            let dist = i64::from(target) - i64::from(inst_end);
            if i8::try_from(dist).is_err() {
                continue;
            }
            let Some(rel8) = matching_rel8(form) else {
                continue;
            };
            inst.form = rel8;
            changed = true;
            rel8_count = rel8_count.saturating_add(1);
        }
        if !changed {
            break;
        }
    }
    Ok(rel8_count)
}

struct LocalRel32 {
    label: crate::backend::x64::inst::LabelId,
}

fn rel32_local_label(inst: &Inst) -> Option<LocalRel32> {
    let form = table::try_form(inst.form)?;
    if form.operands != [OperandKind::Rel32] {
        return None;
    }
    match inst.operands.first() {
        Some(Operand::Label(label)) => Some(LocalRel32 { label: *label }),
        _ => None,
    }
}

fn matching_rel8(form: &table::Form) -> Option<FormId> {
    table::FORMS
        .iter()
        .enumerate()
        .find_map(|(index, candidate)| {
            (candidate.mnemonic == form.mnemonic && candidate.operands == [OperandKind::Rel8])
                .then(|| FormId(u16::try_from(index).expect("form 数量适配 u16")))
        })
}

fn assembled_len(assembled: &super::inst::Assembled, index: usize) -> u32 {
    let start = assembled.instruction_offsets[index].1;
    let end = assembled
        .instruction_offsets
        .get(index + 1)
        .map(|(_, offset)| *offset)
        .unwrap_or_else(|| u32::try_from(assembled.bytes.len()).expect("片段字节适配 u32"));
    end.saturating_sub(start)
}

fn classify_cold(body: &Body) -> Vec<bool> {
    let mut cold = vec![false; body.blocks.len()];
    let mut hot_pred = vec![false; body.blocks.len()];
    for (index, block) in body.blocks.iter().enumerate() {
        if block.cleanup || terminator_is_cold(&block.terminator) {
            cold[index] = true;
        }
        let preds = &body.predecessors[crate::lir::body::range(&block.predecessors)];
        if preds.len() == 1 && body.edges[preds[0].index()].unwind {
            cold[index] = true;
        }
        for pred in preds {
            let edge = &body.edges[pred.index()];
            if !edge.unwind && !terminator_is_cold(&body.blocks[edge.from.index()].terminator) {
                hot_pred[index] = true;
            }
        }
    }
    for (index, block) in body.blocks.iter().enumerate() {
        if cold[index] {
            continue;
        }
        if only_slow_predecessors(body, block) && !hot_pred[index] {
            cold[index] = true;
        }
    }
    cold[body.entry.index()] = false;
    cold
}

fn terminator_is_cold(terminator: &Terminator) -> bool {
    matches!(
        terminator,
        Terminator::Trap { .. } | Terminator::ResumePanic { .. } | Terminator::Unreachable { .. }
    )
}

fn only_slow_predecessors(body: &Body, block: &Block) -> bool {
    let preds = &body.predecessors[crate::lir::body::range(&block.predecessors)];
    if preds.is_empty() {
        return false;
    }
    preds.iter().all(|pred| {
        let edge = &body.edges[pred.index()];
        edge.unwind || predecessor_is_slow(body, edge.from, edge.to)
    })
}

fn predecessor_is_slow(body: &Body, from: BlockId, to: BlockId) -> bool {
    let block = &body.blocks[from.index()];
    if terminator_is_cold(&block.terminator) {
        return true;
    }
    body.instructions[crate::lir::body::range(&block.instructions)]
        .iter()
        .any(|instruction| match &instruction.op {
            Op::TrapIf | Op::SafepointPoll { .. } | Op::GcAlloc { .. } | Op::RegionAlloc { .. } => {
                matches!(
                    block.terminator,
                    Terminator::Branch { yes, no, .. }
                        if body.edges[yes.index()].to == to || body.edges[no.index()].to == to
                )
            }
            _ => false,
        })
}

fn edge_is_hot(body: &Body, layout: &BlockLayout, edge: EdgeId) -> bool {
    let edge = &body.edges[edge.index()];
    if edge.unwind {
        return false;
    }
    let order = usize::try_from(layout.index_of[edge.to.index()]).expect("布局下标适配 usize");
    let target = layout.blocks[order];
    if !target.hot {
        return false;
    }
    // 循环 backedge：目标布局下标不大于源。
    layout.index_of[edge.to.index()] <= layout.index_of[edge.from.index()]
}

fn reverse_postorder(body: &Body) -> Vec<BlockId> {
    let mut seen = vec![false; body.blocks.len()];
    let mut post = Vec::with_capacity(body.blocks.len());
    visit(body, body.entry, &mut seen, &mut post);
    for index in 0..body.blocks.len() {
        if !seen[index] {
            visit(
                body,
                BlockId(crate::lir::body::id(index)),
                &mut seen,
                &mut post,
            );
        }
    }
    post.reverse();
    post
}

fn visit(body: &Body, id: BlockId, seen: &mut [bool], post: &mut Vec<BlockId>) {
    if seen[id.index()] {
        return;
    }
    seen[id.index()] = true;
    for successor in successors(body, id) {
        visit(body, successor, seen, post);
    }
    post.push(id);
}

fn successors(body: &Body, id: BlockId) -> Vec<BlockId> {
    let terminator = &body.blocks[id.index()].terminator;
    match terminator {
        Terminator::Jump(edge) => vec![body.edges[edge.index()].to],
        Terminator::Branch { yes, no, .. } => {
            vec![body.edges[yes.index()].to, body.edges[no.index()].to]
        }
        Terminator::Switch {
            cases, otherwise, ..
        } => {
            let mut targets = Vec::new();
            for (_, edge) in &body.switch_cases[crate::lir::body::range(cases)] {
                targets.push(body.edges[edge.index()].to);
            }
            targets.push(body.edges[otherwise.index()].to);
            targets
        }
        Terminator::Invoke { normal, unwind, .. } => {
            vec![body.edges[normal.index()].to, body.edges[unwind.index()].to]
        }
        Terminator::Return { .. }
        | Terminator::ResumePanic { .. }
        | Terminator::TailCall { .. }
        | Terminator::Trap { .. }
        | Terminator::Unreachable { .. } => Vec::new(),
    }
}
