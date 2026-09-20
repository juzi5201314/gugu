//! 换栈片段与终结符 lowering。
//!
//! 块参数拷贝属于**边**而不是前驱：`Branch` 的两条边各走各的路径，拷贝必须分别落在被选中
//! 的那条路径上，否则两个后继共享同一次拷贝会互相覆盖。形状是：
//!
//! ```text
//! <test>
//! jcc L_taken            ; taken 边需要拷贝时才进 trampoline
//! <fall 边拷贝>
//! jmp fall               ; 有 trampoline 时必须显式跳过它
//! L_taken:
//! <taken 边拷贝>
//! jmp taken
//! ```
//!
//! `Invoke` 只为 normal 边发射拷贝：unwind 边的参数由展开器和 stack map 恢复，不在调用点
//! 内联。`Switch` 的每个 case 各自独立，需要拷贝的 case 走自己的 trampoline。

use crate::backend::x64::abi;
use crate::backend::x64::inst::{LabelId, Operand, RelocKind, RelocTarget};
use crate::backend::x64::mangle;
use crate::backend::x64::reg::Reg;
use crate::backend::x64::table::{Access, OperandKind};
use crate::lir::body::{BlockId, Body, EdgeId, Terminator, ValueId};
use crate::target::TargetName;

use super::call;
use super::{Builder, LoweringError, SiteValue, imm, reg, value_move};

const REL32: &[OperandKind] = &[OperandKind::Rel32];

/// 复用与 `ContextSwitchCode::fixed()` 同源的 62-byte 片段。
pub(super) fn coroutine_switch(builder: &mut Builder) -> Result<(), LoweringError> {
    // rdi=保存位置，rsi=恢复位置，rdx=CoroutineHot*，rcx=LogicalProcessor*。
    // 片段本身写 r14/r15，属于 runtime ABI，不走普通序列的保留寄存器检查；
    // 选指站点用 call 进入同一符号，避免在普通函数体里直接写 r14/r15。
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(mangle::runtime_symbol("coroutine_switch")),
            RelocKind::PcRel32,
        )],
    );
    Ok(())
}

/// 一条边的块参数拷贝。
pub(crate) type Copies = Vec<crate::backend::x64::copies::Copy>;

/// 终结符序列。`copies(edge)` 只为该边生成拷贝，放置位置由这里按路径决定。
#[allow(clippy::too_many_arguments)]
pub(crate) fn terminator(
    body: &Body,
    block: BlockId,
    terminator: &Terminator,
    values: impl Fn(&[ValueId]) -> Vec<SiteValue>,
    target: TargetName,
    builder: &mut Builder,
    emit_jump: bool,
    invert_branch: bool,
    block_label: impl Fn(BlockId) -> LabelId,
    copies: &mut dyn FnMut(EdgeId) -> Result<Copies, LoweringError>,
) -> Result<(), LoweringError> {
    match terminator {
        Terminator::Jump(edge) => {
            place(builder, copies(*edge)?)?;
            if emit_jump {
                emit_to(builder, "jmp", block_label(body.edges[edge.index()].to))?;
            }
            Ok(())
        }
        Terminator::Branch {
            condition, yes, no, ..
        } => {
            let cond = values(&[*condition]);
            let [value] = cond.as_slice() else {
                return Err(LoweringError::InvalidOperands);
            };
            let (taken, fall) = if invert_branch {
                (*no, *yes)
            } else {
                (*yes, *no)
            };
            let taken_copies = copies(taken)?;
            let fall_copies = copies(fall)?;
            builder.emit(
                "test",
                super::wide_kinds(8),
                Access::Read,
                vec![reg(value.reg), reg(value.reg)],
            );
            let mnemonic = if invert_branch { "je" } else { "jne" };
            if taken_copies.is_empty() {
                // 快路：taken 边不需要拷贝，条件分支直接指向目标块。
                emit_to(builder, mnemonic, block_label(body.edges[taken.index()].to))?;
                place(builder, fall_copies)?;
                if emit_jump {
                    emit_to(builder, "jmp", block_label(body.edges[fall.index()].to))?;
                }
                return Ok(());
            }
            let trampoline = builder.label();
            emit_to(builder, mnemonic, trampoline)?;
            place(builder, fall_copies)?;
            // trampoline 夹在中间，fall 路径必须显式跳开。
            emit_to(builder, "jmp", block_label(body.edges[fall.index()].to))?;
            builder.define(trampoline);
            place(builder, taken_copies)?;
            emit_to(builder, "jmp", block_label(body.edges[taken.index()].to))
        }
        Terminator::Switch {
            value,
            cases,
            otherwise,
        } => {
            let scrutinee = values(&[*value]);
            let [scrutinee] = scrutinee.as_slice() else {
                return Err(LoweringError::InvalidOperands);
            };
            let mut trampolines = Vec::new();
            for (case, edge) in &body.switch_cases[crate::lir::body::range(cases)] {
                builder.emit(
                    "cmp",
                    &[OperandKind::Rm64, OperandKind::Imm32],
                    Access::Read,
                    vec![reg(scrutinee.reg), imm(*case)],
                );
                let edge_copies = copies(*edge)?;
                if edge_copies.is_empty() {
                    emit_to(builder, "je", block_label(body.edges[edge.index()].to))?;
                } else {
                    let trampoline = builder.label();
                    emit_to(builder, "je", trampoline)?;
                    trampolines.push((trampoline, *edge, edge_copies));
                }
            }
            // trampoline 跟在默认路径之后，默认跳转必须总是显式发射。
            emit_to(
                builder,
                "jmp",
                block_label(body.edges[otherwise.index()].to),
            )?;
            for (trampoline, edge, edge_copies) in trampolines {
                builder.define(trampoline);
                place(builder, edge_copies)?;
                emit_to(builder, "jmp", block_label(body.edges[edge.index()].to))?;
            }
            Ok(())
        }
        Terminator::Invoke {
            call,
            arguments,
            results,
            normal,
            ..
        } => {
            let args = values(body.args(arguments));
            let dests = result_values(body, results);
            emit_managed_or_foreign(call, &args, &dests, target, builder)?;
            place(builder, copies(*normal)?)?;
            if emit_jump {
                emit_to(builder, "jmp", block_label(body.edges[normal.index()].to))?;
            }
            Ok(())
        }
        Terminator::Return { values: range, .. } => {
            let returned = values(body.args(range));
            let layout = abi::classify_signature(&body.signature)?;
            move_returns(&returned, &layout, builder)?;
            builder.emit("ret", &[], Access::Read, Vec::new());
            Ok(())
        }
        Terminator::TailCall {
            call, arguments, ..
        } => {
            // TailCall eligibility 保证没有 stack argument/sret；这里再核一次，避免把语义上
            // 需要 caller frame 的调用编译成跳转。
            let layout = abi::classify_call(call, target)?;
            if layout.sret || layout.stack_slots != 0 {
                return Err(LoweringError::InvalidOperands);
            }
            let args = values(body.args(arguments));
            call::shuffle_arguments(&args, &layout, builder)?;
            call::emit_jump_target(call, &layout, &args, builder)?;
            Ok(())
        }
        Terminator::ResumePanic { .. } | Terminator::Trap { .. } => {
            builder.emit(
                "jmp",
                REL32,
                Access::Read,
                vec![super::cold_branch(
                    crate::backend::x64::inst::ColdEdgeKind::Trap,
                    &body.blocks[body.entry.index()].source,
                    1 << 31 | block.0,
                )],
            );
            Ok(())
        }
        Terminator::Unreachable { .. } => {
            builder.emit("ud2", &[], Access::Read, Vec::new());
            Ok(())
        }
    }
}

/// 把一条边的块参数拷贝追加进当前路径。
fn place(builder: &mut Builder, copies: Copies) -> Result<(), LoweringError> {
    for copy in copies {
        value_move(builder, copy.src, copy.dest, copy.ty)?;
    }
    Ok(())
}

fn result_values(body: &Body, results: &std::ops::Range<u32>) -> Vec<SiteValue> {
    crate::lir::body::range(results)
        .map(|index| SiteValue {
            ty: body.values[index].kind,
            reg: Reg::Virtual(crate::lir::body::id(index)),
        })
        .collect()
}

fn emit_to(
    builder: &mut Builder,
    mnemonic: &'static str,
    label: LabelId,
) -> Result<(), LoweringError> {
    builder.emit(mnemonic, REL32, Access::Read, vec![Operand::Label(label)]);
    Ok(())
}

fn emit_managed_or_foreign(
    call: &crate::lir::body::Call,
    args: &[SiteValue],
    results: &[SiteValue],
    target: TargetName,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let layout = abi::classify_call(call, target)?;
    call::shuffle_arguments(args, &layout, builder)?;
    call::emit_target(call, &layout, args, builder)?;
    call::collect_results(results, &layout, builder)
}

fn move_returns(
    returned: &[SiteValue],
    layout: &abi::AbiLayout,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    for value in &layout.results {
        let Some(slot) = value.slot else {
            continue;
        };
        let index = usize::try_from(value.index).expect("返回下标适配 usize");
        let Some(source) = returned.get(index) else {
            continue;
        };
        call::move_to_slot(builder, source.reg, slot, value.ty)?;
    }
    Ok(())
}
