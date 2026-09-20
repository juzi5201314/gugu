//! 调用、runtime glue 与 effect-fence 空序列。

use crate::backend::x64::abi::{self, AbiLayout, AbiSlot};
use crate::backend::x64::inst::{Operand, RelocKind, RelocTarget};
use crate::backend::x64::mangle;
use crate::backend::x64::reg::{Gpr, Reg, Xmm};
use crate::backend::x64::table::{Access, OperandKind};
use crate::frontend::late::universe::TypeUniverse;
use crate::lir::body::{Call, CallTarget, Op, RuntimeCall, Symbol};
use crate::runtime::PlatformOp;

use super::{Builder, LowerCtx, LoweringError, SiteValue, move_kinds, reg};

const REL32: &[OperandKind] = &[OperandKind::Rel32];
const RM64: &[OperandKind] = &[OperandKind::Rm64];
const XMMRM_XMM: &[OperandKind] = &[OperandKind::XmmRm, OperandKind::Xmm];

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
        Op::PlatformCall(op) => runtime_named(builder, &format!("platform_{}", op.name())),
        Op::RegionPublish { .. } => runtime_named(builder, "gc_region_publish"),
        Op::RegionReset { .. } => runtime_named(builder, "gc_region_reset"),
        Op::PromoteManaged { .. } => runtime_named(builder, "gc_region_promote"),
        Op::RegionTransfer { .. } => runtime_named(builder, "gc_region_transfer"),
        Op::MarkTicketBatch => runtime_named(builder, "gc_mark_ticket_batch"),
        Op::EdgeDeltaBatch => runtime_named(builder, "gc_edge_delta_batch"),
        Op::ResolveSharedHandle => runtime_named(builder, "gc_resolve_shared_handle"),
        Op::SharedAccessBegin { .. } => runtime_named(builder, "gc_shared_access_begin"),
        Op::SharedAccessEnd { .. } => runtime_named(builder, "gc_shared_access_end"),
        Op::SharedFieldBarrier { .. } | Op::SharedFieldBarrierReserved { .. } => {
            runtime_named(builder, "gc_shared_field_barrier")
        }
        Op::ForwardSharedHandle => runtime_named(builder, "gc_forward_shared_handle"),
        Op::GcWriteBarrier { .. } | Op::GcWriteBarrierReserved { .. } => {
            runtime_named(builder, "gc_write_barrier")
        }
        Op::Park => runtime_named(builder, "sched_park"),
        Op::Ready => runtime_named(builder, "sched_ready"),
        Op::Call(call) => emit_call(call, operands, results, ctx, builder, false),
        Op::ForeignCall(call) => emit_call(call, operands, results, ctx, builder, true),
        _ => Err(LoweringError::InvalidOperands),
    }
}

fn runtime_named(builder: &mut Builder, name: &str) -> Result<(), LoweringError> {
    let symbol = Symbol::External {
        key: crate::frontend::mono::keys::hash_domain("gugu-runtime-symbol-v1", name.as_bytes()),
        name: mangle::mangle_runtime(name),
    };
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(RelocTarget::Lir(symbol), RelocKind::PcRel32)],
    );
    Ok(())
}

fn emit_call(
    call: &Call,
    operands: &[SiteValue],
    results: &[SiteValue],
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
    foreign: bool,
) -> Result<(), LoweringError> {
    let empty = TypeUniverse::default();
    let universe = ctx.universe.unwrap_or(&empty);
    let layout = abi::classify_call(call, ctx.target, universe)?;
    shuffle_arguments(operands, &layout, builder)?;
    match &call.target {
        CallTarget::Instance(key) => call_symbol(builder, Symbol::Instance(*key), foreign)?,
        CallTarget::External { key, name } => call_symbol(
            builder,
            Symbol::External {
                key: *key,
                name: name.clone(),
            },
            true,
        )?,
        CallTarget::Runtime(runtime) => {
            let name = mangle::mangle_runtime_call(*runtime);
            call_symbol(
                builder,
                Symbol::External {
                    key: crate::frontend::mono::keys::hash_domain(
                        "gugu-runtime-symbol-v1",
                        name.as_bytes(),
                    ),
                    name,
                },
                false,
            )?;
        }
        CallTarget::Indirect => {
            let callee = operands.last().ok_or(LoweringError::InvalidOperands)?;
            builder.emit("call", RM64, Access::Read, vec![reg(callee.reg)]);
        }
        CallTarget::Vtable { slot } => {
            let receiver = operands.first().ok_or(LoweringError::InvalidOperands)?;
            builder.clobber_gpr(Gpr::R11);
            let disp = i32::try_from(*slot)
                .ok()
                .and_then(|slot| slot.checked_mul(8))
                .ok_or(LoweringError::InvalidOperands)?;
            builder.emit(
                "mov",
                move_kinds(64),
                Access::Read,
                vec![super::mem_base(receiver.reg, 0), reg(Reg::Gpr(Gpr::R11))],
            );
            builder.emit(
                "call",
                RM64,
                Access::Read,
                vec![super::mem_base(Reg::Gpr(Gpr::R11), disp)],
            );
        }
    }
    collect_results(results, &layout, builder)?;
    let _ = RuntimeCall::Yield;
    let _ = PlatformOp::Commit;
    Ok(())
}

fn call_symbol(builder: &mut Builder, symbol: Symbol, _foreign: bool) -> Result<(), LoweringError> {
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(RelocTarget::Lir(symbol), RelocKind::PcRel32)],
    );
    Ok(())
}

fn shuffle_arguments(
    operands: &[SiteValue],
    layout: &AbiLayout,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    for value in &layout.arguments {
        if value.index == u32::MAX {
            continue;
        }
        let index = usize::try_from(value.index).expect("参数下标适配 usize");
        let Some(src) = operands.get(index) else {
            continue;
        };
        let Some(slot) = value.slots.first() else {
            continue;
        };
        match slot {
            AbiSlot::Integer(gpr) => emit_gpr_move(builder, src.reg, *gpr)?,
            AbiSlot::Float(xmm) => emit_xmm_move(builder, src.reg, *xmm)?,
            AbiSlot::Stack { .. } => {}
        }
    }
    Ok(())
}

fn collect_results(
    results: &[SiteValue],
    layout: &AbiLayout,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    for (index, value) in layout.results.iter().enumerate() {
        let Some(dest) = results.get(index) else {
            continue;
        };
        let Some(slot) = value.slots.first() else {
            continue;
        };
        match slot {
            AbiSlot::Integer(gpr) => emit_gpr_reg(builder, Reg::Gpr(*gpr), dest.reg)?,
            AbiSlot::Float(xmm) => emit_xmm_reg(builder, Reg::Xmm(*xmm), dest.reg)?,
            AbiSlot::Stack { .. } => {}
        }
    }
    Ok(())
}

fn emit_gpr_move(builder: &mut Builder, src: Reg, dest: Gpr) -> Result<(), LoweringError> {
    emit_gpr_reg(builder, src, Reg::Gpr(dest))
}

fn emit_gpr_reg(builder: &mut Builder, src: Reg, dest: Reg) -> Result<(), LoweringError> {
    if src == dest {
        return Ok(());
    }
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![reg(dest), reg(src)],
    );
    Ok(())
}

fn emit_xmm_move(builder: &mut Builder, src: Reg, dest: Xmm) -> Result<(), LoweringError> {
    emit_xmm_reg(builder, src, Reg::Xmm(dest))
}

fn emit_xmm_reg(builder: &mut Builder, src: Reg, dest: Reg) -> Result<(), LoweringError> {
    if src == dest {
        return Ok(());
    }
    builder.emit(
        "movaps",
        XMMRM_XMM,
        Access::Write,
        vec![reg(dest), reg(src)],
    );
    Ok(())
}
