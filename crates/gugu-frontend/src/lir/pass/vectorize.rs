//! 循环展开：把计数循环体复制若干次，展开因子由成本基线的向量能力决定。
//!
//! `vector_lowering == false` 时不生成任何 `V128`，只做标量展开；向量 lowering
//! 与校准 profile 由后端阶段提供。
use super::loops::{self, CountedLoop, LoopInfo};
use super::rewrite::{Editor, Term};
use crate::lir::body::{BlockId, Condition, ValueId};
use crate::{BackendCostProfile, Diagnostic};
use std::collections::BTreeMap;

pub(crate) fn run(editor: &mut Editor, profile: &BackendCostProfile) -> Result<bool, Diagnostic> {
    let factor: u64 = if profile.vector_lowering { 4 } else { 2 };
    let mut changed = false;
    for natural in loops::analyze(editor) {
        let Some(counted) = natural.counted.clone() else {
            continue;
        };
        changed |= unroll(editor, &natural, &counted, factor)?;
    }
    Ok(changed)
}

fn unroll(
    editor: &mut Editor,
    natural: &LoopInfo,
    counted: &CountedLoop,
    factor: u64,
) -> Result<bool, Diagnostic> {
    if natural.blocks.len() != 3 {
        return Ok(false);
    }
    let header = natural.header;
    let latch = counted.latch;
    let Some(body) = natural
        .blocks
        .iter()
        .copied()
        .find(|block| *block != header && *block != latch)
    else {
        return Ok(false);
    };
    if !matches!(counted.condition, Condition::Lt | Condition::Le) || counted.step <= 0 {
        return Ok(false);
    }
    let body_params = editor.block(body).expect("活跃 block").params.len();
    let latch_params = editor.block(latch).expect("活跃 block").params.len();
    if body_params != 1 || latch_params != 1 {
        return Ok(false);
    }
    let Term::Branch { condition, yes, no } = editor.terminator(header).clone() else {
        return Ok(false);
    };
    let body_edge = if editor.edge(yes).is_some_and(|edge| edge.to == body) {
        yes
    } else if editor.edge(no).is_some_and(|edge| edge.to == body) {
        no
    } else {
        return Ok(false);
    };
    let _ = condition;
    let Term::Jump(body_to_latch) = editor.terminator(body).clone() else {
        return Ok(false);
    };
    if editor
        .edge(body_to_latch)
        .is_none_or(|edge| edge.to != latch)
    {
        return Ok(false);
    }
    let Term::Jump(latch_to_header) = editor.terminator(latch).clone() else {
        return Ok(false);
    };
    if editor
        .edge(latch_to_header)
        .is_none_or(|edge| edge.to != header)
    {
        return Ok(false);
    }
    let Some(trips) = trip_count(editor, natural, counted) else {
        return Ok(false);
    };
    if trips < factor || trips % factor != 0 {
        return Ok(false);
    }
    if !supported_operands(editor, natural, &[body, latch], counted.induction) {
        return Ok(false);
    }
    if !loops::effect_free(editor, natural) {
        return Ok(false);
    }
    let header_params = editor.block(header).expect("活跃 block").params.clone();
    let Some(position) = header_params
        .iter()
        .position(|param| param.value == counted.induction)
    else {
        return Ok(false);
    };
    let latch_arguments = editor
        .edge(latch_to_header)
        .expect("活跃边")
        .arguments
        .clone();
    let mut mem_out = latch_arguments[0];
    let mut iv_out = latch_arguments[position];
    let mem_kind = editor.kind(editor.block(body).expect("活跃 block").params[0].value);
    let body_source = editor.block(body).expect("活跃 block").source.clone();
    let body_cleanup = editor.block(body).expect("活跃 block").cleanup;
    let latch_source = editor.block(latch).expect("活跃 block").source.clone();
    let latch_cleanup = editor.block(latch).expect("活跃 block").cleanup;
    let mut current_latch = latch;
    let mut current_edge = Some(latch_to_header);
    for _ in 2..=factor {
        let body_copy = editor.add_block(vec![(mem_kind, None)], body_source.clone(), body_cleanup);
        let substitutions = BTreeMap::from([(counted.induction, iv_out)]);
        let empty = BTreeMap::new();
        loops::clone_block_into(editor, body, body_copy, &empty, &substitutions);
        let latch_copy =
            editor.add_block(vec![(mem_kind, None)], latch_source.clone(), latch_cleanup);
        loops::clone_block_into(editor, latch, latch_copy, &empty, &substitutions);
        // 把上一个 latch 接到这一份 body。
        let edge = editor.add_edge(current_latch, body_copy, vec![mem_out]);
        editor.set_terminator(current_latch, Term::Jump(edge));
        if let Some(previous) = current_edge.take() {
            editor.remove_edge(previous);
        }
        let copied = editor.terminator(latch_copy).clone();
        let Term::Jump(copied_edge) = copied else {
            return Err(crate::lir::invalid("展开副本的 latch 不是无条件跳转"));
        };
        let arguments = editor.edge(copied_edge).expect("活跃边").arguments.clone();
        mem_out = arguments[0];
        iv_out = arguments[position];
        current_latch = latch_copy;
        current_edge = Some(copied_edge);
    }
    let _ = body_edge;
    Ok(true)
}

/// 规范计数循环的常量 trip count；无法证明有限或存在负值时返回 `None`。
fn trip_count(editor: &Editor, natural: &LoopInfo, counted: &CountedLoop) -> Option<u64> {
    let header = natural.header;
    let induction = counted.induction;
    let params = &editor.block(header)?.params;
    let position = params.iter().position(|param| param.value == induction)?;
    let preheader = editor
        .predecessors(header)
        .into_iter()
        .filter_map(|edge| editor.edge(edge))
        .find(|edge| !natural.blocks.contains(&edge.from))?;
    let initial = super::constants::constant_bits(editor, preheader.arguments[position])?;
    let bound = super::constants::constant_bits(editor, counted.bound)?;
    let bits = editor.kind(induction).ty.bytes()? * 8;
    let bits = u32::try_from(bits).expect("位宽适配 u32");
    if counted.signed {
        let negative = |value: u64| bits < 64 && (value >> (bits - 1)) & 1 == 1;
        if negative(initial) || negative(bound) {
            return None;
        }
    }
    let step = u128::from(counted.step as u64);
    let initial = u128::from(initial);
    let bound = u128::from(bound);
    let trips = match counted.condition {
        Condition::Lt if bound > initial => (bound - initial).div_ceil(step),
        Condition::Lt => 0,
        Condition::Le if bound >= initial => (bound - initial) / step + 1,
        Condition::Le => 0,
        _ => return None,
    };
    u64::try_from(trips).ok()
}

/// 被复制区间的操作数必须是循环内定义、`induction` 本身或循环外定义。
fn supported_operands(
    editor: &Editor,
    natural: &LoopInfo,
    blocks: &[BlockId],
    induction: ValueId,
) -> bool {
    let mut supported = true;
    let mut check = |value: ValueId| {
        if value == induction {
            return;
        }
        match editor.defining_instruction(value) {
            Some((block, _)) => {
                if natural.blocks.contains(&block) && !blocks.contains(&block) {
                    // 循环内、但未被复制的定义（如 header 指令）会跨迭代失效。
                    supported = false;
                }
            }
            None => {
                let local = blocks.iter().any(|block| {
                    editor
                        .block(*block)
                        .is_some_and(|data| data.params.iter().any(|p| p.value == value))
                });
                if !local {
                    supported = false;
                }
            }
        }
    };
    for &block in blocks {
        for index in 0..editor.instruction_count(block) {
            let instruction = editor.instruction((block, index));
            if instruction.removed {
                continue;
            }
            for argument in &instruction.arguments {
                check(*argument);
            }
            if let Some(memory) = instruction.memory {
                check(memory.input);
            }
        }
        for value in editor.terminator(block).uses() {
            check(value);
        }
    }
    supported
}
