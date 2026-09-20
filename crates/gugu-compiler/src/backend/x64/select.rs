//! 全函数 instruction selection：块参数拷贝、站点 lowering、终结符与布局。

use crate::frontend::late::universe::TypeUniverse;
use crate::lir::body::{BlockId, Body, Op, Terminator, ValueId};
use crate::runtime::RuntimeRawContractV1;
use crate::target::TargetName;

use super::abi::{self, AbiLayout};
use super::inst::{Operand, Sequence};
use super::layout::{self, BlockLayout};
use super::lower::{self, Builder, LowerCtx, Lowered, LoweringError, SiteValue};
use super::mangle;
use super::reg::{Clobbers, Gpr, Reg, Xmm};
use super::table::{Access, OperandKind};

/// 一个已选择的块。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedBlock {
    pub id: BlockId,
    pub hot: bool,
    pub order: u32,
    pub sites: Vec<SelectedSite>,
    pub terminator: Lowered,
}

/// 一个 LIR 指令站点。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedSite {
    pub instruction: u32,
    pub op: &'static str,
    pub lowered: Lowered,
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
    let abi = abi::classify_signature(&body.signature, universe)?;
    let layout = layout::schedule(body);
    let mut ctx = LowerCtx {
        target,
        body: Some(body),
        universe: Some(universe),
        raw: Some(raw),
        site: 0,
    };
    let mut next_temp = u32::try_from(body.values.len()).expect("值编号适配 u32");
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
            let inst_index = body.instructions[index].source.location.start;
            let _ = inst_index;
            sites.push(SelectedSite {
                instruction: crate::lir::body::id(index),
                op: lower::domain(&instruction.op),
                lowered,
            });
        }
        let mut term_builder = Builder::new();
        let copies = copy_block_params(body, &data.terminator, &mut next_temp, &mut term_builder)?;
        clobbers = clobbers.union(copies);
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
            _ => true,
        };
        lower::ctrl::terminator(
            body,
            &data.terminator,
            |ids| site_values(body, ids),
            target,
            universe,
            &mut term_builder,
            emit_jump,
            invert,
            |id| super::inst::LabelId(id.0),
            block.id,
        )
        .map_err(|_| LoweringError::Unsupported {
            op: "Terminator",
            detail: "终结符 lowering 失败",
        })?;
        let terminator = term_builder.finish();
        clobbers = clobbers.union(terminator.clobbers);
        selected.push(SelectedBlock {
            id: block.id,
            hot: block.hot,
            order: block.order,
            sites,
            terminator,
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

fn stitch(blocks: &[SelectedBlock], sequence: &mut Sequence) {
    let block_base = blocks.iter().map(|block| block.id.0 + 1).max().unwrap_or(0);
    let mut local_shift = block_base;
    for block in blocks {
        let at = u32::try_from(sequence.instructions.len()).expect("指令数适配 u32");
        sequence.labels.push(super::inst::LabelDefinition {
            label: super::inst::LabelId(block.id.0),
            at,
        });
        for site in &block.sites {
            append_local(sequence, &site.lowered.sequence, &mut local_shift);
        }
        for inst in &block.terminator.sequence.instructions {
            sequence.instructions.push(inst.clone());
        }
    }
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
        dest.labels.push(super::inst::LabelDefinition {
            label: super::inst::LabelId(definition.label.0 + shift),
            at: definition.at + base,
        });
    }
    *local_shift = shift + used;
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

fn copy_block_params(
    body: &Body,
    terminator: &Terminator,
    next_temp: &mut u32,
    builder: &mut Builder,
) -> Result<Clobbers, LoweringError> {
    let edges: Vec<_> = match terminator {
        Terminator::Jump(edge) => vec![*edge],
        Terminator::Branch { yes, no, .. } => vec![*yes, *no],
        Terminator::Switch {
            cases, otherwise, ..
        } => {
            let mut edges: Vec<_> = body.switch_cases[crate::lir::body::range(cases)]
                .iter()
                .map(|(_, edge)| *edge)
                .collect();
            edges.push(*otherwise);
            edges
        }
        Terminator::Invoke { normal, .. } => vec![*normal],
        _ => Vec::new(),
    };
    for edge_id in edges {
        let edge = &body.edges[edge_id.index()];
        let args = body.args(&edge.arguments);
        let params = body.params(edge.to);
        emit_copies(body, args, params, next_temp, builder)?;
    }
    Ok(Clobbers::NONE)
}

fn emit_copies(
    body: &Body,
    args: &[ValueId],
    params: &[crate::lir::body::Parameter],
    next_temp: &mut u32,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    if args.len() != params.len() {
        return Ok(());
    }
    let pairs: Vec<(Reg, Reg, crate::lir::body::Type)> = args
        .iter()
        .zip(params)
        .map(|(src, param)| {
            (
                Reg::Virtual(src.0),
                Reg::Virtual(param.value.0),
                body.values[src.index()].kind.ty,
            )
        })
        .filter(|(src, dest, _)| src != dest)
        .collect();
    let dests: Vec<Reg> = pairs.iter().map(|(_, dest, _)| *dest).collect();
    for (src, dest, ty) in &pairs {
        if dests.contains(src) {
            let temp = Reg::Virtual(*next_temp);
            *next_temp = next_temp.saturating_add(1);
            emit_move(builder, *src, temp, *ty)?;
            emit_move(builder, temp, *dest, *ty)?;
        } else {
            emit_move(builder, *src, *dest, *ty)?;
        }
    }
    let _ = (
        Gpr::Rax,
        Xmm::Xmm0,
        OperandKind::Rm64,
        Access::Write,
        Op::Select,
    );
    Ok(())
}

fn emit_move(
    builder: &mut Builder,
    src: Reg,
    dest: Reg,
    ty: crate::lir::body::Type,
) -> Result<(), LoweringError> {
    if src == dest {
        return Ok(());
    }
    match ty {
        crate::lir::body::Type::F32
        | crate::lir::body::Type::F64
        | crate::lir::body::Type::V128(_) => {
            builder.emit(
                "movaps",
                &[OperandKind::XmmRm, OperandKind::Xmm],
                Access::Write,
                vec![super::lower::reg(dest), super::lower::reg(src)],
            );
        }
        _ => {
            builder.emit(
                "mov",
                super::lower::move_kinds(64),
                Access::Write,
                vec![super::lower::reg(dest), super::lower::reg(src)],
            );
        }
    }
    Ok(())
}
