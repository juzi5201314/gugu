//! 自然循环分析与规范化：唯一 preheader 与规范 induction 变量。
use super::graph::dominates;
use super::rewrite::{Editor, Term};
use crate::Diagnostic;
use crate::lir::body::{BlockId, Condition, EdgeId, IntOp, Op, ValueId};
use std::collections::{BTreeMap, BTreeSet};

/// 单个自然循环的规范信息。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoopInfo {
    pub(crate) header: BlockId,
    pub(crate) latches: Vec<BlockId>,
    pub(crate) blocks: BTreeSet<BlockId>,
    pub(crate) preheader: Option<BlockId>,
    pub(crate) exits: Vec<EdgeId>,
    pub(crate) counted: Option<CountedLoop>,
}

/// 规范计数循环。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CountedLoop {
    pub(crate) induction: ValueId,
    pub(crate) step: i64,
    pub(crate) bound: ValueId,
    pub(crate) condition: Condition,
    pub(crate) signed: bool,
    pub(crate) test_block: BlockId,
    pub(crate) latch: BlockId,
}

/// 识别全部 single-header 自然循环；顺序按 header 编号。
pub(crate) fn analyze(editor: &Editor) -> Vec<LoopInfo> {
    let Some(dominators) = editor.dominators() else {
        return Vec::new();
    };
    let mut loops: Vec<LoopInfo> = Vec::new();
    for block in editor.live_blocks() {
        for edge in editor.terminator(block).edges() {
            let Some(data) = editor.edge(edge) else {
                continue;
            };
            if !dominates(&dominators, data.to, block) {
                continue;
            }
            match loops.iter_mut().find(|natural| natural.header == data.to) {
                Some(existing) => {
                    if !existing.latches.contains(&block) {
                        existing.latches.push(block);
                    }
                }
                None => loops.push(LoopInfo {
                    header: data.to,
                    latches: vec![block],
                    blocks: BTreeSet::new(),
                    preheader: None,
                    exits: Vec::new(),
                    counted: None,
                }),
            }
        }
    }
    for natural in &mut loops {
        natural.latches.sort_unstable();
        let mut blocks = BTreeSet::from([natural.header]);
        for latch in &natural.latches {
            let mut stack = vec![*latch];
            while let Some(block) = stack.pop() {
                if blocks.insert(block) && block != natural.header {
                    for edge in editor.predecessors(block) {
                        if let Some(data) = editor.edge(edge) {
                            stack.push(data.from);
                        }
                    }
                }
            }
        }
        natural.blocks = blocks;
        natural.exits = editor
            .edges()
            .filter(|(_, edge)| {
                natural.blocks.contains(&edge.from) && !natural.blocks.contains(&edge.to)
            })
            .map(|(id, _)| id)
            .collect();
        natural.counted = counted_loop(editor, natural);
    }
    loops.sort_unstable_by_key(|natural| natural.header);
    loops
}

/// 规范化：为每个循环插入唯一 preheader。
pub(crate) fn canonicalize(editor: &mut Editor) -> Result<bool, Diagnostic> {
    let mut changed = false;
    for natural in analyze(editor) {
        if natural.preheader.is_some() {
            continue;
        }
        let header = natural.header;
        let outside: Vec<_> = editor
            .predecessors(header)
            .into_iter()
            .filter(|edge| {
                editor
                    .edge(*edge)
                    .is_some_and(|edge| !natural.blocks.contains(&edge.from))
            })
            .collect();
        if outside.is_empty() {
            continue;
        }
        if outside.len() == 1
            && editor.successors(editor.edge(outside[0]).expect("活跃边").from) == [header]
        {
            // 唯一外部前驱且它只跳到这里：直接复用。
            continue;
        }
        let params: Vec<_> = editor
            .block(header)
            .expect("活跃 block")
            .params
            .iter()
            .map(|param| (editor.kind(param.value), param.source))
            .collect();
        let source = editor.block(header).expect("活跃 block").source.clone();
        let cleanup = editor.block(header).expect("活跃 block").cleanup;
        let preheader = editor.add_block(params, source, cleanup);
        let arguments: Vec<_> = editor
            .block(preheader)
            .expect("活跃 block")
            .params
            .iter()
            .map(|param| param.value)
            .collect();
        let forward = push_edge(editor, preheader, header, arguments);
        editor.set_terminator(preheader, Term::Jump(forward));
        for edge in outside {
            let data = editor.edge(edge).expect("活跃边");
            let arguments = data.arguments.clone();
            editor.redirect_edge(edge, preheader, arguments);
        }
        changed = true;
    }
    Ok(changed)
}

fn push_edge(editor: &mut Editor, from: BlockId, to: BlockId, arguments: Vec<ValueId>) -> EdgeId {
    editor.add_edge(from, to, arguments)
}

/// 识别规范计数循环：header 参数 phi、latch 加法更新、header 分支比较不变量。
fn counted_loop(editor: &Editor, natural: &LoopInfo) -> Option<CountedLoop> {
    if natural.latches.len() != 1 {
        return None;
    }
    let latch = natural.latches[0];
    let header = natural.header;
    let condition = match editor.terminator(header).clone() {
        Term::Branch { condition, .. } => condition,
        // `while cond { .. }` 降为对布尔值的单 case `Switch`。
        Term::Switch {
            value,
            cases,
            otherwise,
        } => {
            if cases.len() != 1 || !matches!(cases[0].0, 0 | 1) {
                return None;
            }
            let inside = |edge: EdgeId| {
                editor
                    .edge(edge)
                    .is_some_and(|data| natural.blocks.contains(&data.to))
            };
            if inside(cases[0].1) == inside(otherwise) {
                return None;
            }
            value
        }
        _ => return None,
    };
    let test = editor.defining_instruction(condition)?;
    let test_instruction = editor.instruction(test);
    let Op::Compare { condition, signed } = &test_instruction.op else {
        return None;
    };
    let (condition, signed) = (*condition, *signed);
    let [left, right] = test_instruction.arguments.as_slice() else {
        return None;
    };
    let induction = *left;
    if !editor.kind(induction).ty.integer() {
        return None;
    }
    // induction 必须是 header 参数。
    let header_params = &editor.block(header)?.params;
    if !header_params.iter().any(|param| param.value == induction) {
        return None;
    }
    // latch 必须用 `induction + step` 更新。
    let mut step = None;
    for edge in editor.terminator(latch).edges() {
        let data = editor.edge(edge)?;
        if data.to != header {
            continue;
        }
        let position = header_params
            .iter()
            .position(|param| param.value == induction)?;
        let next = data.arguments[position];
        let at = editor.defining_instruction(next)?;
        let update = editor.instruction(at);
        if let Op::Integer(IntOp::Add) = update.op {
            let [base, increment] = update.arguments.as_slice() else {
                return None;
            };
            if !carries_induction(editor, natural, *base, induction) {
                return None;
            }
            let at = editor.defining_instruction(*increment)?;
            if let Op::IConst(value) = editor.instruction(at).op {
                step = Some(value as i64);
            }
        }
    }
    let step = step?;
    if step == 0 {
        return None;
    }
    // bound 必须在循环外定义，或是循环内定义的常量（常量与循环迭代无关）。
    if let Some(at) = editor.defining_instruction(*right)
        && natural.blocks.contains(&at.0)
        && !matches!(editor.instruction(at).op, Op::IConst(_) | Op::FConst(_))
    {
        return None;
    }
    Some(CountedLoop {
        induction,
        step,
        bound: *right,
        condition,
        signed,
        test_block: header,
        latch,
    })
}

/// `value` 是否在循环入口携带 induction：自身，或循环内由 header 入边传进 induction 的块参数副本。
fn carries_induction(
    editor: &Editor,
    natural: &LoopInfo,
    value: ValueId,
    induction: ValueId,
) -> bool {
    if value == induction {
        return true;
    }
    for block in &natural.blocks {
        let Some(data) = editor.block(*block) else {
            continue;
        };
        let Some(position) = data.params.iter().position(|param| param.value == value) else {
            continue;
        };
        for edge in editor.predecessors(*block) {
            let Some(predecessor) = editor.edge(edge) else {
                continue;
            };
            if predecessor.from == natural.header
                && predecessor.arguments.get(position) == Some(&induction)
            {
                return true;
            }
        }
    }
    false
}

/// 被复制区间不得含 effect fence。
pub(crate) fn effect_free(editor: &Editor, natural: &LoopInfo) -> bool {
    for &block in &natural.blocks {
        for index in 0..editor.instruction_count(block) {
            let op = &editor.instruction((block, index)).op;
            let allowed = matches!(op, Op::Load(_) | Op::Store(_))
                || !op.has_memory() && op.safepoint_kind().is_none();
            if !allowed {
                return false;
            }
        }
        if matches!(
            editor.terminator(block),
            Term::Invoke { .. } | Term::TailCall { .. }
        ) {
            return false;
        }
    }
    true
}

/// 克隆一组 block 及其内部边，返回旧→新 block 映射。
pub(crate) fn clone_subgraph(
    editor: &mut Editor,
    blocks: &[BlockId],
) -> BTreeMap<BlockId, BlockId> {
    let mut block_map = BTreeMap::new();
    for &old in blocks {
        let data = editor.block(old).expect("活跃 block");
        let params: Vec<_> = data
            .params
            .iter()
            .map(|param| (editor.kind(param.value), param.source))
            .collect();
        let source = data.source.clone();
        let cleanup = data.cleanup;
        let new = editor.add_block(params, source, cleanup);
        block_map.insert(old, new);
    }
    let empty = BTreeMap::new();
    for &old in blocks {
        let new = block_map[&old];
        clone_block_into(editor, old, new, &block_map, &empty);
    }
    block_map
}

/// 把一个 block 的指令与终结符克隆到已存在的 `new`。
///
/// `new` 的参数必须与 `old` 逐位对应；`substitutions` 覆盖集合外值的引用，
/// `block_map` 用于把指向已克隆 block 的边改向到副本。
pub(crate) fn clone_block_into(
    editor: &mut Editor,
    old: BlockId,
    new: BlockId,
    block_map: &BTreeMap<BlockId, BlockId>,
    substitutions: &BTreeMap<ValueId, ValueId>,
) {
    use crate::lir::body::{Memory, Origin, Type, ValueType};
    let old_params: Vec<ValueId> = editor
        .block(old)
        .expect("活跃 block")
        .params
        .iter()
        .map(|param| param.value)
        .collect();
    let new_params: Vec<ValueId> = editor
        .block(new)
        .expect("活跃 block")
        .params
        .iter()
        .map(|param| param.value)
        .collect();
    let mut value_map: BTreeMap<ValueId, ValueId> = old_params
        .iter()
        .zip(&new_params)
        .map(|(old, new)| (*old, *new))
        .collect();
    value_map.extend(substitutions.iter().map(|(old, new)| (*old, *new)));
    for index in 0..editor.instruction_count(old) {
        let instruction = editor.instruction((old, index)).clone();
        if instruction.removed {
            continue;
        }
        for result in instruction.results {
            let kind = editor.kind(result);
            let origin = editor.origin(result).clone();
            let fresh = editor.fresh_value(kind, origin);
            value_map.insert(result, fresh);
        }
        if let Some(memory) = instruction.memory {
            let fresh = editor.fresh_value(
                ValueType {
                    ty: Type::Mem,
                    provenance: None,
                },
                Origin::None,
            );
            value_map.insert(memory.output, fresh);
        }
    }
    if let Term::Invoke {
        results, memory, ..
    } = editor.terminator(old).clone()
    {
        for result in results {
            let kind = editor.kind(result);
            let origin = editor.origin(result).clone();
            let fresh = editor.fresh_value(kind, origin);
            value_map.insert(result, fresh);
        }
        let fresh = editor.fresh_value(
            ValueType {
                ty: Type::Mem,
                provenance: None,
            },
            Origin::None,
        );
        value_map.insert(memory.output, fresh);
    }
    let map = |value: ValueId| value_map.get(&value).copied().unwrap_or(value);
    for index in 0..editor.instruction_count(old) {
        let instruction = editor.instruction((old, index)).clone();
        if instruction.removed {
            continue;
        }
        let arguments = instruction
            .arguments
            .iter()
            .map(|value| map(*value))
            .collect();
        let results = instruction
            .results
            .iter()
            .map(|value| map(*value))
            .collect();
        let memory = instruction.memory.map(|memory| Memory {
            input: map(memory.input),
            output: map(memory.output),
        });
        editor.emit_raw(
            new,
            instruction.op.clone(),
            arguments,
            results,
            memory,
            instruction.safepoint,
        );
    }
    let term = editor.terminator(old).clone();
    let remap_edge = |editor: &mut Editor, edge: EdgeId| -> EdgeId {
        let data = editor.edge(edge).expect("活跃边");
        let to = block_map.get(&data.to).copied().unwrap_or(data.to);
        let arguments = data.arguments.iter().map(|value| map(*value)).collect();
        let unwind = data.unwind;
        editor.add_edge_unwind(new, to, arguments, unwind)
    };
    let term = match term {
        Term::Jump(edge) => Term::Jump(remap_edge(editor, edge)),
        Term::Branch { condition, yes, no } => Term::Branch {
            condition: map(condition),
            yes: remap_edge(editor, yes),
            no: remap_edge(editor, no),
        },
        Term::Switch {
            value,
            cases,
            otherwise,
        } => Term::Switch {
            value: map(value),
            cases: cases
                .into_iter()
                .map(|(case, edge)| (case, remap_edge(editor, edge)))
                .collect(),
            otherwise: remap_edge(editor, otherwise),
        },
        Term::Invoke {
            call,
            arguments,
            results,
            memory,
            normal,
            unwind,
            safepoint,
        } => Term::Invoke {
            call,
            arguments: arguments.iter().map(|value| map(*value)).collect(),
            results: results.iter().map(|value| map(*value)).collect(),
            memory: Memory {
                input: map(memory.input),
                output: map(memory.output),
            },
            normal: remap_edge(editor, normal),
            unwind: remap_edge(editor, unwind),
            safepoint,
        },
        Term::Return { values, memory } => Term::Return {
            values: values.iter().map(|value| map(*value)).collect(),
            memory: map(memory),
        },
        Term::ResumePanic { memory } => Term::ResumePanic {
            memory: map(memory),
        },
        Term::TailCall {
            call,
            arguments,
            memory,
        } => Term::TailCall {
            call,
            arguments: arguments.iter().map(|value| map(*value)).collect(),
            memory: map(memory),
        },
        Term::Trap { memory } => Term::Trap {
            memory: map(memory),
        },
        Term::Unreachable { memory } => Term::Unreachable {
            memory: map(memory),
        },
    };
    editor.set_terminator(new, term);
}
