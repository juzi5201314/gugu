//! 调用、runtime glue 与 effect-fence 空序列。
//!
//! 调用 lowering 分三步：按分类结果把实参搬进 ABI 槽（寄存器与 caller outgoing 区栈
//! piece 都要落位）、发射目标（直接符号、间接寄存器或 vtable 槽）、最后把返回值搬回虚拟
//! 寄存器。间接调用的目标是分类阶段定位的 `Provenance::Code` 参数，动态派发目标是
//! `Provenance::Metadata` 参数指向的 vtable，都不占用普通参数槽。

use crate::lir::body::{Call, CallTarget, Op, Symbol};
use crate::x64::abi::{self, AbiLayout, AbiSlot, Dispatch};
use crate::x64::inst::{Operand, RelocKind, RelocTarget};
use crate::x64::mangle;
use crate::x64::reg::{Gpr, Reg};
use crate::x64::table::{Access, OperandKind};

use super::{
    Builder, Copy, LowerCtx, LoweringError, SiteValue, ensure_runtime_glue, mem_base, move_kinds,
    reg, value_move,
};

const REL32: &[OperandKind] = &[OperandKind::Rel32];
const RM64: &[OperandKind] = &[OperandKind::Rm64];
/// 一个 vtable 槽的字节宽度。
const VTABLE_SLOT_BYTES: u32 = 8;

pub(super) fn lower(
    op: &Op,
    operands: &[SiteValue],
    results: &[SiteValue],
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match op {
        Op::BarrierReserve(_)
        | Op::ScopedViewBegin { .. }
        | Op::ScopedViewEnd { .. }
        | Op::NoSafepointBegin(_)
        | Op::NoSafepointEnd(_) => Ok(()),
        Op::PlatformCall(op) => runtime_named(
            operands,
            results,
            builder,
            &format!("platform_{}", op.name()),
        ),
        Op::RuntimeMachineCall(op) => runtime_named(operands, results, builder, op.name()),
        // 分配 op 由 `lower/alloc.rs` 走专用四车道 ABI；这里显式拒绝，避免落到下面的目录分支。
        Op::GcAlloc { .. } | Op::RegionAlloc { .. } => Err(LoweringError::InvalidOperands),
        // 其余 runtime 入口全部来自同一份目录，名字不再在 lowering 里重复一遍。
        op if crate::x64::glue::implied_glue(op).is_some() => {
            let glue = crate::x64::glue::implied_glue(op).expect("目录已判定存在");
            runtime_named(operands, results, builder, glue)
        }
        Op::Call(call) => emit_call(call, operands, results, ctx, builder),
        Op::ForeignCall(call) => emit_call(call, operands, results, ctx, builder),
        _ => Err(LoweringError::InvalidOperands),
    }
}

/// op 形式的 runtime/glue 调用：这些 op 不走 `Call` 的 ABI 落位，必须在发出调用前按内部 ABI 的
/// 实参寄存器顺序（`INTERNAL_INTEGER`）把操作数搬进参数寄存器；否则被调片段只拿到碰巧落在首个
/// 参数寄存器里的那个操作数，其余参数保持调用点的残留值。
fn runtime_named(
    operands: &[SiteValue],
    results: &[SiteValue],
    builder: &mut Builder,
    name: &str,
) -> Result<(), LoweringError> {
    ensure_runtime_glue(name)?;
    if operands.len() > abi::INTERNAL_INTEGER.len() {
        return Err(LoweringError::InvalidOperands);
    }
    let pairs: Vec<Copy> = operands
        .iter()
        .enumerate()
        .map(|(index, operand)| Copy {
            src: operand.reg,
            dest: Reg::Gpr(abi::INTERNAL_INTEGER[index]),
            ty: operand.ty.ty,
        })
        .collect();
    builder.record_group(&pairs, |builder| {
        for (index, operand) in operands.iter().enumerate() {
            value_move(
                builder,
                operand.reg,
                Reg::Gpr(abi::INTERNAL_INTEGER[index]),
                operand.ty.ty,
            )?;
        }
        Ok(())
    })?;
    emit_symbol_call(builder, mangle::runtime_symbol(name))?;
    for (result, register) in results.iter().zip([Gpr::Rax, Gpr::Rbx]) {
        value_move(builder, Reg::Gpr(register), result.reg, result.ty.ty)?;
    }
    Ok(())
}

/// 站点调用 lowering：分类 → 实参落位 → 目标 → 返回值回收。
fn emit_call(
    call: &Call,
    operands: &[SiteValue],
    results: &[SiteValue],
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    if super::foreign::dispatcher(call.kind).is_some() {
        return super::foreign::emit(call, operands, results, ctx.target, builder);
    }
    let layout = abi::classify_call(call, ctx.target)?;
    shuffle_arguments(operands, &layout, builder)?;
    emit_target(call, &layout, operands, builder)?;
    collect_results(results, &layout, builder)?;
    Ok(())
}

/// 发射调用目标；`Invoke`/`TailCall` 共用同一目标选择规则。
pub(super) fn emit_target(
    call: &Call,
    layout: &AbiLayout,
    operands: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match &call.target {
        CallTarget::Instance(key) => emit_symbol_call(builder, Symbol::Instance(*key)),
        CallTarget::External { key, name } => emit_symbol_call(
            builder,
            Symbol::External {
                key: *key,
                name: name.clone(),
            },
        ),
        CallTarget::Runtime(runtime) => {
            let name = mangle::runtime_call_name(*runtime);
            ensure_runtime_glue(name)?;
            emit_symbol_call(builder, mangle::runtime_call_symbol(*runtime))
        }
        CallTarget::Indirect => {
            let callee = target_value(operands, layout.callee)?;
            builder.emit("call", RM64, Access::Read, vec![reg(callee)]);
            Ok(())
        }
        CallTarget::Vtable { slot } => emit_dispatch(
            builder,
            "call",
            operands,
            layout.dispatch,
            vtable_disp(*slot)?,
        ),
    }
}

/// 发射 `jmp` 形式的目标；`TailCall` 复用同一目标选择。
pub(super) fn emit_jump_target(
    call: &Call,
    layout: &AbiLayout,
    operands: &[SiteValue],
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match &call.target {
        CallTarget::Instance(key) => emit_symbol_jump(builder, Symbol::Instance(*key)),
        CallTarget::External { key, name } => emit_symbol_jump(
            builder,
            Symbol::External {
                key: *key,
                name: name.clone(),
            },
        ),
        CallTarget::Runtime(runtime) => {
            let name = mangle::runtime_call_name(*runtime);
            ensure_runtime_glue(name)?;
            emit_symbol_jump(builder, mangle::runtime_call_symbol(*runtime))
        }
        CallTarget::Indirect => {
            let callee = target_value(operands, layout.callee)?;
            builder.emit("jmp", RM64, Access::Read, vec![reg(callee)]);
            Ok(())
        }
        CallTarget::Vtable { slot } => emit_dispatch(
            builder,
            "jmp",
            operands,
            layout.dispatch,
            vtable_disp(*slot)?,
        ),
    }
}

/// 按派发形态取 vtable 再进入槽：胖指针 lane 直接用它，胖对指针先取 `+8` 处的 vtable。
fn emit_dispatch(
    builder: &mut Builder,
    mnemonic: &'static str,
    operands: &[SiteValue],
    dispatch: Option<Dispatch>,
    slot: i32,
) -> Result<(), LoweringError> {
    let dispatch = dispatch.ok_or(LoweringError::InvalidOperands)?;
    let index = match dispatch {
        Dispatch::Lane(index) | Dispatch::Pairs(index) => index,
    };
    let receiver = operand_reg(operands, Some(index))?;
    let vtable = match dispatch {
        Dispatch::Lane(_) => receiver,
        Dispatch::Pairs(_) => {
            // 借用后端 scratch r11 承载 vtable，避免占用参数寄存器。
            builder.clobber_gpr(Gpr::R11);
            builder.emit(
                "mov",
                move_kinds(64),
                Access::Read,
                vec![mem_base(receiver, VTABLE_OFFSET), reg(Reg::Gpr(Gpr::R11))],
            );
            Reg::Gpr(Gpr::R11)
        }
    };
    builder.emit(mnemonic, RM64, Access::Read, vec![mem_base(vtable, slot)]);
    Ok(())
}

/// vtable 指针在 `dyn` 胖对里的字节偏移。
const VTABLE_OFFSET: i32 = 8;

/// 目标参数寄存器；缺失或下标越界都是内部不变量失败。
fn target_value(operands: &[SiteValue], index: Option<u32>) -> Result<Reg, LoweringError> {
    operand_reg(operands, index)
}

fn operand_reg(operands: &[SiteValue], index: Option<u32>) -> Result<Reg, LoweringError> {
    let index = usize::try_from(index.ok_or(LoweringError::InvalidOperands)?)
        .map_err(|_| LoweringError::InvalidOperands)?;
    operands
        .get(index)
        .map(|value| value.reg)
        .ok_or(LoweringError::InvalidOperands)
}

/// vtable 槽的字节偏移。
fn vtable_disp(slot: u32) -> Result<i32, LoweringError> {
    slot.checked_mul(VTABLE_SLOT_BYTES)
        .and_then(|bytes| i32::try_from(bytes).ok())
        .ok_or(LoweringError::InvalidOperands)
}

/// 实参落位：寄存器槽直接搬，栈 piece 写进 caller outgoing 区。
///
/// outgoing 栈 piece 先保存，再把全部寄存器实参作为同一并行拷贝组落位；
/// 栈实参的源可能恰好占用某个参数寄存器，不能在该寄存器改写后读取。
pub(super) fn shuffle_arguments(
    operands: &[SiteValue],
    layout: &AbiLayout,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    for value in &layout.arguments {
        if let Some(AbiSlot::Stack { offset }) = value.slot {
            let source = operands
                .get(value.index as usize)
                .ok_or(LoweringError::InvalidOperands)?;
            super::store_stack(builder, source.reg, offset, value.ty.ty)?;
        }
    }
    let mut pending: Vec<Copy> = Vec::new();
    let mut start = builder.instruction_count();
    for value in &layout.arguments {
        let Some(slot) = value.slot else {
            continue;
        };
        let index = usize::try_from(value.index).expect("参数下标适配 usize");
        let Some(source) = operands.get(index) else {
            continue;
        };
        match slot {
            AbiSlot::Integer(gpr) => {
                pending.push(Copy {
                    src: source.reg,
                    dest: Reg::Gpr(gpr),
                    ty: value.ty.ty,
                });
                value_move(builder, source.reg, Reg::Gpr(gpr), value.ty.ty)?;
            }
            AbiSlot::Float(xmm) => {
                pending.push(Copy {
                    src: source.reg,
                    dest: Reg::Xmm(xmm),
                    ty: value.ty.ty,
                });
                value_move(builder, source.reg, Reg::Xmm(xmm), value.ty.ty)?;
            }
            AbiSlot::Stack { .. } => {}
        }
    }
    flush_group(builder, &mut pending, &mut start);
    Ok(())
}

/// 收束当前寄存器渲染段为并行拷贝组。
pub(super) fn flush_group(builder: &mut Builder, pending: &mut Vec<Copy>, start: &mut u32) {
    if pending.is_empty() {
        return;
    }
    let end = builder.instruction_count();
    builder.record_group_span(std::mem::take(pending), *start..end);
    *start = end;
}

/// 返回值回收：寄存器槽搬回虚拟寄存器。
pub(super) fn collect_results(
    results: &[SiteValue],
    layout: &AbiLayout,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let mut pending: Vec<Copy> = Vec::new();
    let mut start = builder.instruction_count();
    for value in &layout.results {
        let Some(slot) = value.slot else {
            continue;
        };
        let index = usize::try_from(value.index).expect("返回下标适配 usize");
        let Some(dest) = results.get(index) else {
            continue;
        };
        match slot {
            AbiSlot::Integer(gpr) => {
                pending.push(Copy {
                    src: Reg::Gpr(gpr),
                    dest: dest.reg,
                    ty: value.ty.ty,
                });
                value_move(builder, Reg::Gpr(gpr), dest.reg, value.ty.ty)?;
            }
            AbiSlot::Float(xmm) => {
                pending.push(Copy {
                    src: Reg::Xmm(xmm),
                    dest: dest.reg,
                    ty: value.ty.ty,
                });
                value_move(builder, Reg::Xmm(xmm), dest.reg, value.ty.ty)?;
            }
            AbiSlot::Stack { offset } => {
                flush_group(builder, &mut pending, &mut start);
                super::load_stack(builder, offset, dest.reg, value.ty.ty)?;
            }
        }
    }
    flush_group(builder, &mut pending, &mut start);
    Ok(())
}

fn emit_symbol_call(builder: &mut Builder, symbol: Symbol) -> Result<(), LoweringError> {
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(RelocTarget::Lir(symbol), RelocKind::PcRel32)],
    );
    Ok(())
}

#[cfg(test)]
mod argument_tests {
    use super::*;
    use crate::gir::body::CallKind;
    use crate::lir::body::{Type, ValueType};

    #[test]
    fn stack_argument_survives_register_argument_overwrite() {
        let ty = ValueType::scalar(Type::I64);
        let call = Call {
            target: CallTarget::Instance([1; 32]),
            kind: CallKind::Managed,
            parameters: vec![ty; 10],
            results: vec![],
            may_unwind: false,
            may_suspend: false,
            may_allocate: false,
            captures_arguments: false,
            by_value: vec![],
            foreign_arguments: vec![],
            sret: None,
            poll_free_leaf: false,
        };
        let layout = abi::classify_call(&call, crate::TargetName::X86_64Linux).unwrap();
        let mut operands: Vec<_> = (0..10)
            .map(|index| SiteValue {
                ty,
                reg: Reg::Virtual(index),
            })
            .collect();
        operands[9].reg = Reg::Gpr(Gpr::Rax);
        let mut builder = Builder::new();
        shuffle_arguments(&operands, &layout, &mut builder).unwrap();
        let lowered = builder.finish();
        let stack_write = lowered.sequence.instructions.iter().position(|inst| {
            matches!(inst.operands.first(), Some(Operand::Mem(mem)) if mem.base == Some(Reg::Gpr(Gpr::Rsp)))
        }).unwrap();
        let overwrite = lowered
            .sequence
            .instructions
            .iter()
            .position(|inst| inst.operands == [reg(Reg::Gpr(Gpr::Rax)), reg(Reg::Virtual(0))])
            .unwrap();
        assert!(
            stack_write < overwrite,
            "第十实参必须在 rax 被第一个实参覆盖前写入 outgoing 区"
        );
    }
}

fn emit_symbol_jump(builder: &mut Builder, symbol: Symbol) -> Result<(), LoweringError> {
    builder.emit(
        "jmp",
        REL32,
        Access::Read,
        vec![Operand::Reloc(RelocTarget::Lir(symbol), RelocKind::PcRel32)],
    );
    Ok(())
}
