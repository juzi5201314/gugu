//! 全函数 instruction selection：块参数拷贝、站点 lowering、终结符与布局。
//!
//! 标签编号空间是函数级的：块标签占 `0..block_count`，站点局部标签与终结符局部标签随后
//! 按布局顺序连续分配。终结符的 trampoline 因此可以带标签，而 `stitch` 只需要把终结符
//! 序列原样追加，不必重写它的跳转目标（块标签不能被平移）。

use crate::frontend::late::universe::TypeUniverse;
use crate::lir::body::{BlockId, Body, EdgeId, Terminator, ValueId};
use crate::runtime::RuntimeRawContractV1;
use crate::target::TargetName;

use super::abi::{self, AbiLayout};
use super::copies::{self, Temps};
use super::inst::{LabelDefinition, Operand, Sequence};
use super::layout::{self, BlockLayout};
use super::lower::{self, Builder, LowerCtx, Lowered, LoweringError, SiteValue};
use super::mangle;
use super::reg::{Clobbers, Reg};

/// 一个已选择的块。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedBlock {
    pub id: BlockId,
    pub hot: bool,
    pub order: u32,
    pub sites: Vec<SelectedSite>,
    pub terminator: Lowered,
    /// 终结符局部标签的起始编号。
    pub terminator_label_base: u32,
    /// 下一个可用标签编号（终结符已分配的局部标签区间终点）。
    pub terminator_label_end: u32,
}

/// 一个 LIR 指令站点。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedSite {
    pub instruction: u32,
    pub op: &'static str,
    /// 分配阶段的站点分类；决定点位 mask 与 pointer spill 规则。
    pub kind: SiteKind,
    pub lowered: Lowered,
}

/// 站点在分配阶段的分类：由 LIR op 的规范 safepoint 种类推出，不另建第二张分类表。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SiteKind {
    /// 普通站点：mask 只看指令的物理寄存器写。
    Normal,
    /// 含调用或其它 caller-saved 破坏点（call return、分配、poll、屏障、select）。
    Call,
    /// 挂起或 bridge 站点：跨点活跃的 managed/stack 指针必须落 frame slot。
    Bridge,
    /// 入口 `StackCheck` 标记站点：prologue 由 frame 阶段完全合成。
    Prologue,
}

impl SiteKind {
    /// 由 LIR 指令的 safepoint 种类推出站点分类。
    fn of(op: &crate::lir::body::Op) -> Self {
        use crate::lir::body::SafepointKind;
        match op.safepoint_kind() {
            Some(SafepointKind::StackCheck) => Self::Prologue,
            Some(
                SafepointKind::Suspend
                | SafepointKind::ForeignBridge
                | SafepointKind::DirtyCpuBridge,
            ) => Self::Bridge,
            Some(_) => Self::Call,
            None => Self::Normal,
        }
    }
}

/// 已选择的函数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedFunction {
    pub symbol: String,
    pub abi: AbiLayout,
    pub layout: BlockLayout,
    pub blocks: Vec<SelectedBlock>,
    pub clobbers: Clobbers,
    pub sequence: Sequence,
    pub rel8_count: u32,
}

/// 对一个 LIR body 做 instruction selection。
pub(crate) fn select_body(
    body: &Body,
    universe: &TypeUniverse,
    raw: &RuntimeRawContractV1,
    target: TargetName,
) -> Result<SelectedFunction, LoweringError> {
    // 值编号与 lowering 临时编号共享同一上界：`StackAddr` 的 frame 占位基址从
    // `FRAME_SLOT_BASE` 起，两个空间不能重叠。
    debug_assert!(
        u32::try_from(body.values.len()).expect("值编号适配 u32") < super::reg::FRAME_SLOT_BASE
    );
    let abi = abi::classify_signature(&body.signature)?;
    let layout = layout::schedule(body);
    let mut ctx = LowerCtx {
        target,
        body: Some(body),
        universe: Some(universe),
        raw: Some(raw),
        site: 0,
    };
    let mut temps = Temps::new(u32::try_from(body.values.len()).expect("值编号适配 u32"));
    let mut next_label = block_label_count(&layout);
    let mut selected = Vec::with_capacity(layout.blocks.len());
    let mut clobbers = Clobbers::NONE;
    for block in &layout.blocks {
        let data = &body.blocks[block.id.index()];
        let mut sites = Vec::new();
        for index in crate::lir::body::range(&data.instructions) {
            let instruction = &body.instructions[index];
            ctx.site = crate::lir::body::id(index);
            let operands = site_values(body, body.args(&instruction.arguments));
            let results = result_values(body, &instruction.results);
            let lowered = lower::lower_with(
                &instruction.op,
                &operands,
                &results,
                &instruction.source,
                ctx,
            )
            .map_err(|error| LoweringError::Unsupported {
                op: lower::domain(&instruction.op),
                detail: match error {
                    LoweringError::Unsupported { detail, .. } => detail,
                    LoweringError::InvalidOperands => "操作数不符合 op 语义",
                },
            })?;
            clobbers = clobbers.union(lowered.clobbers);
            sites.push(SelectedSite {
                instruction: crate::lir::body::id(index),
                op: lower::domain(&instruction.op),
                kind: SiteKind::of(&instruction.op),
                lowered,
            });
        }
        for site in &sites {
            next_label = next_label.saturating_add(label_usage(&site.lowered.sequence));
        }
        let invert = match &data.terminator {
            Terminator::Branch { yes, no, .. } => layout::branch_order(body, &layout, *yes, *no).2,
            _ => false,
        };
        let emit_jump = match &data.terminator {
            Terminator::Jump(edge) => !layout::jump_falls_through(body, &layout, block.id, *edge),
            Terminator::Branch { yes, no, .. } => {
                let fall = if invert { *yes } else { *no };
                !layout::jump_falls_through(body, &layout, block.id, fall)
            }
            Terminator::Invoke { normal, .. } => {
                !layout::jump_falls_through(body, &layout, block.id, *normal)
            }
            _ => true,
        };
        let terminator_label_base = next_label;
        let mut term_builder = Builder::with_labels(terminator_label_base);
        let mut edge_copies = |edge: EdgeId| copies::edge_copies(body, edge);
        lower::ctrl::terminator(
            body,
            block.id,
            &data.terminator,
            |ids| site_values(body, ids),
            target,
            &mut term_builder,
            emit_jump,
            invert,
            &mut temps,
            |id| super::inst::LabelId(id.0),
            &mut edge_copies,
        )
        .map_err(|error| LoweringError::Unsupported {
            op: "Terminator",
            detail: match error {
                LoweringError::Unsupported { detail, .. } => detail,
                LoweringError::InvalidOperands => "终结符操作数不符合语义",
            },
        })?;
        next_label = term_builder.labels_used();
        let terminator_label_end = next_label;
        let terminator = term_builder.finish();
        clobbers = clobbers.union(terminator.clobbers);
        selected.push(SelectedBlock {
            id: block.id,
            hot: block.hot,
            order: block.order,
            sites,
            terminator,
            terminator_label_base,
            terminator_label_end,
        });
    }
    let mut sequence = Sequence::new();
    stitch(&selected, &mut sequence);
    let rel8_count = layout::relax(&mut sequence)?;
    Ok(SelectedFunction {
        symbol: mangle::mangle_function(body),
        abi,
        layout,
        blocks: selected,
        clobbers,
        sequence,
        rel8_count,
    })
}

/// 块标签占用的编号区间：`0..block_count`。
fn block_label_count(layout: &BlockLayout) -> u32 {
    layout
        .blocks
        .iter()
        .map(|block| block.id.0 + 1)
        .max()
        .unwrap_or(0)
}

/// 一段序列用到的局部标签数；与 [`append_local`] 的编号推进同源。
fn label_usage(sequence: &Sequence) -> u32 {
    let mut used = 0;
    for inst in &sequence.instructions {
        for operand in &inst.operands {
            if let Operand::Label(label) = operand {
                used = used.max(label.0 + 1);
            }
        }
    }
    for definition in &sequence.labels {
        used = used.max(definition.label.0 + 1);
    }
    used
}

/// 重拼函数级序列：块标签原样定义，站点序列按累积位移重写标签，终结符标签已属函数级
/// 编号空间，只做追加。
///
/// 分配阶段改写每个站点的序列后必须用同一函数重拼：追加顺序与指令数不变，块标签与
/// 终结符标签的编号记账因此保持成立。
pub(crate) fn stitch_blocks(blocks: &[SelectedBlock], sequence: &mut Sequence) {
    let block_base = blocks.iter().map(|block| block.id.0 + 1).max().unwrap_or(0);
    let mut local_shift = block_base;
    for block in blocks {
        let at = u32::try_from(sequence.instructions.len()).expect("指令数适配 u32");
        sequence.labels.push(LabelDefinition {
            label: super::inst::LabelId(block.id.0),
            at,
        });
        for site in &block.sites {
            append_local(sequence, &site.lowered.sequence, &mut local_shift);
        }
        // 终结符标签已在选指阶段按同一编号空间分配；块标签不能被平移，因此这里只做追加。
        // 区间终点来自选指阶段：终结符里的块标签引用不能参与局部标签计数。
        debug_assert_eq!(local_shift, block.terminator_label_base);
        let terminator_at = u32::try_from(sequence.instructions.len()).expect("指令数适配 u32");
        for inst in &block.terminator.sequence.instructions {
            sequence.instructions.push(inst.clone());
        }
        // 终结符里的站内标签（跳板标签）定义在终结符序列内部，位置必须重定基到函数序列。
        sequence
            .labels
            .extend(
                block
                    .terminator
                    .sequence
                    .labels
                    .iter()
                    .map(|definition| LabelDefinition {
                        label: definition.label,
                        at: definition.at.saturating_add(terminator_at),
                    }),
            );
        local_shift = block.terminator_label_end;
    }
}

fn stitch(blocks: &[SelectedBlock], sequence: &mut Sequence) {
    stitch_blocks(blocks, sequence);
}

fn append_local(dest: &mut Sequence, src: &Sequence, local_shift: &mut u32) {
    let base = u32::try_from(dest.instructions.len()).expect("指令数适配 u32");
    let shift = *local_shift;
    let mut used = 0_u32;
    for inst in &src.instructions {
        let mut inst = inst.clone();
        for operand in &mut inst.operands {
            if let Operand::Label(label) = operand {
                used = used.max(label.0 + 1);
                *label = super::inst::LabelId(label.0 + shift);
            }
        }
        dest.instructions.push(inst);
    }
    for definition in &src.labels {
        used = used.max(definition.label.0 + 1);
        dest.labels.push(LabelDefinition {
            label: super::inst::LabelId(definition.label.0 + shift),
            at: definition.at + base,
        });
    }
    *local_shift = shift.saturating_add(used);
}

fn site_values(body: &Body, args: &[ValueId]) -> Vec<SiteValue> {
    args.iter()
        .map(|value| SiteValue {
            ty: body.values[value.index()].kind,
            reg: Reg::Virtual(value.0),
        })
        .collect()
}

fn result_values(body: &Body, results: &std::ops::Range<u32>) -> Vec<SiteValue> {
    crate::lir::body::range(results)
        .map(|index| SiteValue {
            ty: body.values[index].kind,
            reg: Reg::Virtual(crate::lir::body::id(index)),
        })
        .collect()
}
