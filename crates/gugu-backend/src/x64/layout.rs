//! 热/冷块布局与分支收缩：先 rel32，只许长变短，按 code offset 迭代到不动点。

use std::sync::OnceLock;

use crate::lir::body::{Block, BlockId, Body, EdgeId, Op, Terminator};

use super::encode::encoded_len;
use super::inst::{Inst, Operand, Sequence};
use super::lower::LoweringError;
use super::table::{self, FormId, OperandKind};

/// 一个已布局块：LIR 身份、热/冷、布局下标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutBlock {
    pub id: BlockId,
    pub hot: bool,
    pub order: u32,
}

/// 函数级布局：热块在前，冷块按 RPO 接在后面。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockLayout {
    pub blocks: Vec<LayoutBlock>,
    /// `BlockId.0` → 布局下标。
    pub index_of: Vec<u32>,
}

/// 计算热/冷并排出 entry 的 reverse postorder。
pub fn schedule(body: &Body) -> BlockLayout {
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
pub fn branch_order(
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
pub fn jump_falls_through(body: &Body, layout: &BlockLayout, from: BlockId, edge: EdgeId) -> bool {
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
///
/// 位移必须按**缩短后**的指令终点判定：`jmp` 短跳少 3 字节、`jcc` 少 4 字节，前向跳的终点
/// 前移会让 `target - end` 变大，按旧终点收下的边界分支会在下一轮编码时越界。收缩只会让
/// 其它分支的位移向合法区间移动，因此按 code offset 迭代到不动点后必然收敛。
pub fn relax(sequence: &mut Sequence) -> Result<u32, LoweringError> {
    let mut label_at: Vec<Option<usize>> = Vec::new();
    for definition in &sequence.labels {
        let index =
            usize::try_from(definition.label.0).map_err(|_| LoweringError::InvalidOperands)?;
        let at = usize::try_from(definition.at).map_err(|_| LoweringError::InvalidOperands)?;
        if label_at.len() <= index {
            label_at.resize(index + 1, None);
        }
        if label_at[index].replace(at).is_some() {
            return Err(LoweringError::InvalidOperands);
        }
    }
    let mut labels = Vec::with_capacity(label_at.len());
    for at in label_at {
        labels.push(at.ok_or(LoweringError::InvalidOperands)?);
    }

    let inst_count = sequence.instructions.len();
    let mut lengths = Vec::with_capacity(inst_count);
    let mut total = 0_u32;
    let mut candidates = Vec::new();
    for (index, inst) in sequence.instructions.iter().enumerate() {
        let len = encoded_len(inst).map_err(|_| LoweringError::InvalidOperands)?;
        total = total
            .checked_add(len)
            .ok_or(LoweringError::InvalidOperands)?;
        lengths.push(len);
        if let Some(rel32) = rel32_local_label(inst)
            && let Some(rel8) = matching_rel8(inst.form)
        {
            let target_label =
                usize::try_from(rel32.label.0).map_err(|_| LoweringError::InvalidOperands)?;
            if target_label >= labels.len() {
                return Err(LoweringError::InvalidOperands);
            }
            let rel8_len = encoded_len(&Inst {
                form: rel8,
                lock: false,
                operands: vec![Operand::Label(rel32.label)],
            })
            .map_err(|_| LoweringError::InvalidOperands)?;
            candidates.push((index, target_label, rel8, rel8_len, false));
        }
    }
    if candidates.is_empty() {
        return Ok(count_rel8(sequence));
    }

    let mut offsets = vec![0_u32; inst_count + 1];
    loop {
        let mut cursor = 0_u32;
        for (index, &len) in lengths.iter().enumerate() {
            offsets[index] = cursor;
            cursor += len;
        }
        offsets[inst_count] = cursor;

        let mut changed = false;
        for candidate in &mut candidates {
            if candidate.4 {
                continue;
            }
            let (index, target_label, rel8, rel8_len, _) = *candidate;
            let start = offsets[index];
            let shrunk_len = lengths[index]
                .saturating_sub(REL32_FIELD_BYTES)
                .saturating_add(REL8_FIELD_BYTES);
            let target_inst = labels[target_label].min(inst_count);
            let target = offsets[target_inst];
            let dist = i64::from(target) - (i64::from(start) + i64::from(shrunk_len));
            if i8::try_from(dist).is_err() {
                continue;
            }
            sequence.instructions[index].form = rel8;
            lengths[index] = rel8_len;
            candidate.4 = true;
            changed = true;
        }
        if !changed {
            break;
        }
    }
    Ok(count_rel8(sequence))
}

/// 序列里最终的短跳条数。
fn count_rel8(sequence: &Sequence) -> u32 {
    sequence
        .instructions
        .iter()
        .filter(|inst| table::form(inst.form).operands == [OperandKind::Rel8])
        .count()
        .try_into()
        .expect("短跳条数适配 u32")
}

/// rel32 字段宽度。
const REL32_FIELD_BYTES: u32 = 4;
/// rel8 字段宽度。
const REL8_FIELD_BYTES: u32 = 1;

struct LocalRel32 {
    label: crate::x64::inst::LabelId,
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

fn matching_rel8(form_id: FormId) -> Option<FormId> {
    // 稠密 FormId 查表（表单数量编译期固定且 < 512），避免每次分支松弛线性扫描整张 FORMS 表。
    static REL8_BY_FORM: OnceLock<Vec<Option<FormId>>> = OnceLock::new();
    let table = REL8_BY_FORM.get_or_init(|| {
        table::FORMS
            .iter()
            .map(|form| {
                (form.operands == [OperandKind::Rel32]).then(|| {
                    table::FORMS
                        .iter()
                        .enumerate()
                        .find_map(|(index, candidate)| {
                            (candidate.mnemonic == form.mnemonic
                                && candidate.operands == [OperandKind::Rel8])
                            .then(|| FormId(u16::try_from(index).expect("form 数量适配 u16")))
                        })
                })?
            })
            .collect()
    });
    table.get(usize::from(form_id.0)).copied().flatten()
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
    for successor in body.successors(id) {
        visit(body, successor, seen, post);
    }
    post.push(id);
}
