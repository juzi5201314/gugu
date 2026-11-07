//! 普通 ForeignBridge 把 C ABI 车道交给 Gugu admission；native 不持有协程栈参数。
use super::*;
use crate::gir::body::CallKind;
use crate::lir::body::{Call, CallTarget, Provenance, Symbol};
use crate::runtime::foreign_packet as packet;
use crate::x64::abi::{self, AbiLayout, AbiSlot};
use crate::x64::mangle;
use crate::x64::reg::Xmm;

pub(crate) fn dispatcher(kind: CallKind) -> Option<&'static str> {
    match kind {
        CallKind::ForeignBridge => Some("foreign_dispatch"),
        CallKind::ForeignBridgeDirtyCpu => Some("foreign_dirty_dispatch"),
        _ => None,
    }
}

/// 调用者 outgoing 区中的实参位置；bridge 的寄存器车道也已存入参数包。
pub(crate) fn outgoing_offset(
    kind: CallKind,
    slot: AbiSlot,
    target: TargetName,
) -> Result<Option<u32>, LoweringError> {
    if dispatcher(kind).is_some() {
        lane(slot, target).map(Some)
    } else if let AbiSlot::Stack { offset } = slot {
        Ok(Some(offset))
    } else {
        Ok(None)
    }
}

pub(crate) struct PacketLayout {
    pub bytes: u32,
    stack_bytes: u32,
    fixups: u32,
    copies: Vec<(u32, u32, u64, u64)>,
    abi: AbiLayout,
}

fn lane(slot: AbiSlot, target: TargetName) -> Result<u32, LoweringError> {
    Ok(match slot {
        AbiSlot::Integer(register) => {
            let registers: &[Gpr] = if target == TargetName::X86_64Linux {
                &[Gpr::Rdi, Gpr::Rsi, Gpr::Rdx, Gpr::Rcx, Gpr::R8, Gpr::R9]
            } else {
                &[Gpr::Rcx, Gpr::Rdx, Gpr::R8, Gpr::R9]
            };
            packet::FOREIGN_PACKET_INTEGERS as u32
                + registers
                    .iter()
                    .position(|r| *r == register)
                    .ok_or(LoweringError::InvalidOperands)? as u32
                    * 8
        }
        AbiSlot::Float(register) => packet::FOREIGN_PACKET_FLOATS as u32 + register as u32 * 8,
        AbiSlot::Stack { offset } => (packet::FOREIGN_PACKET_BYTES as u32)
            .checked_add(offset)
            .ok_or(LoweringError::InvalidOperands)?,
    })
}

pub(crate) fn layout(call: &Call, target: TargetName) -> Result<PacketLayout, LoweringError> {
    let abi = abi::classify_call(call, target)?;
    let mut stack_bytes = abi
        .stack_slots
        .checked_mul(8)
        .ok_or(LoweringError::InvalidOperands)?;
    if target == TargetName::X86_64Windows {
        stack_bytes = stack_bytes
            .checked_add(32)
            .ok_or(LoweringError::InvalidOperands)?;
    }
    stack_bytes = stack_bytes
        .checked_add(15)
        .ok_or(LoweringError::InvalidOperands)?
        & !15;
    let fixups = (packet::FOREIGN_PACKET_BYTES as u32)
        .checked_add(stack_bytes)
        .ok_or(LoweringError::InvalidOperands)?;
    let mut copies = Vec::new();
    for (index, bytes, output) in call
        .by_value
        .iter()
        .map(|(index, _, bytes)| (*index, *bytes, packet::FIXUP_INPUT))
        .chain(
            call.sret
                .map(|(index, bytes, _)| (index, bytes, packet::FIXUP_OUTPUT)),
        )
    {
        let slot = abi
            .arguments
            .get(index as usize)
            .and_then(|value| value.slot)
            .ok_or(LoweringError::InvalidOperands)?;
        copies.push((index, lane(slot, target)?, bytes, output));
    }
    for value in &abi.arguments {
        if value.ty.provenance == Some(Provenance::Raw)
            && !copies.iter().any(|(index, ..)| *index == value.index)
            && let Some(slot) = value.slot
        {
            copies.push((value.index, lane(slot, target)?, 0, packet::FIXUP_POINTER));
        }
    }
    let bytes = fixups
        .checked_add(
            u32::try_from(copies.len())
                .map_err(|_| LoweringError::InvalidOperands)?
                .checked_mul(32)
                .ok_or(LoweringError::InvalidOperands)?,
        )
        .ok_or(LoweringError::InvalidOperands)?;
    if bytes > i32::MAX as u32 {
        return Err(LoweringError::InvalidOperands);
    }
    Ok(PacketLayout {
        bytes,
        stack_bytes,
        fixups,
        copies,
        abi,
    })
}

fn constant(builder: &mut Builder, offset: u32, value: u64) {
    builder.emit(
        "mov",
        &[OperandKind::R64, OperandKind::Imm64],
        Access::Write,
        vec![reg(gpr(Gpr::R11)), imm(value)],
    );
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![mem_base(gpr(Gpr::Rsp), offset as i32), reg(gpr(Gpr::R11))],
    );
}

pub(super) fn emit(
    call: &Call,
    operands: &[SiteValue],
    results: &[SiteValue],
    target: TargetName,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let dispatcher = dispatcher(call.kind).ok_or(LoweringError::InvalidOperands)?;
    let packet = layout(call, target)?;
    for offset in (0..packet.bytes).step_by(8) {
        builder.emit(
            "mov",
            &[OperandKind::Rm64, OperandKind::Imm32],
            Access::Write,
            vec![mem_base(gpr(Gpr::Rsp), offset as i32), imm(0)],
        );
    }
    // 先写所有用户实参，后占用 runtime 参数寄存器；不存在中途 GC 或切栈。
    for value in &packet.abi.arguments {
        if let Some(slot) = value.slot {
            let source = operands
                .get(value.index as usize)
                .ok_or(LoweringError::InvalidOperands)?;
            store_stack(builder, source.reg, lane(slot, target)?, value.ty.ty)?;
        }
    }
    for (index, &(parameter, slot, bytes, output)) in packet.copies.iter().enumerate() {
        let offset = packet.fixups + index as u32 * 32;
        constant(builder, offset, u64::from(slot));
        store_stack(
            builder,
            operands[parameter as usize].reg,
            offset + 8,
            Type::Ptr,
        )?;
        if output != packet::FIXUP_POINTER {
            builder.emit(
                "mov",
                move_kinds(64),
                Access::Read,
                vec![mem_base(gpr(Gpr::R14), 80), reg(gpr(Gpr::R11))],
            );
            builder.emit(
                "sub",
                move_kinds(64),
                Access::Read,
                vec![
                    mem_base(gpr(Gpr::Rsp), (offset + 8) as i32),
                    reg(gpr(Gpr::R11)),
                ],
            );
            store_stack(builder, gpr(Gpr::R11), offset + 8, Type::I64)?;
        }
        constant(builder, offset + 16, bytes);
        constant(builder, offset + 24, output);
    }
    match &call.target {
        CallTarget::External { key, name } => {
            builder.emit(
                "lea",
                &[OperandKind::Mem, OperandKind::R64],
                Access::Address,
                vec![
                    Operand::Rip(
                        RelocTarget::Lir(Symbol::External {
                            key: *key,
                            name: name.clone(),
                        }),
                        0,
                    ),
                    reg(gpr(Gpr::R11)),
                ],
            );
            store_stack(builder, gpr(Gpr::R11), 0, Type::Ptr)?;
        }
        CallTarget::Instance(instance) => {
            builder.emit(
                "lea",
                &[OperandKind::Mem, OperandKind::R64],
                Access::Address,
                vec![
                    Operand::Rip(RelocTarget::Lir(Symbol::Instance(*instance)), 0),
                    reg(gpr(Gpr::R11)),
                ],
            );
            store_stack(builder, gpr(Gpr::R11), 0, Type::Ptr)?;
        }
        CallTarget::Indirect => {
            let index = packet.abi.callee.ok_or(LoweringError::InvalidOperands)?;
            store_stack(builder, operands[index as usize].reg, 0, Type::Ptr)?;
        }
        _ => return Err(LoweringError::InvalidOperands),
    }
    constant(
        builder,
        packet::FOREIGN_PACKET_STACK_BYTES as u32,
        u64::from(packet.stack_bytes),
    );
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Read,
        vec![mem_base(gpr(Gpr::R14), 80), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "sub",
        move_kinds(64),
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rax)), reg(gpr(Gpr::Rsp))],
    );
    for (register, value) in [
        (Gpr::Rbx, packet.bytes),
        (Gpr::Rcx, packet.fixups),
        (Gpr::Rdx, packet.copies.len() as u32),
    ] {
        builder.emit(
            "mov",
            &[OperandKind::R32, OperandKind::Imm32],
            Access::Write,
            vec![reg(gpr(register)), imm(u64::from(value))],
        );
    }
    ensure_runtime_glue(dispatcher)?;
    builder.emit(
        "call",
        &[OperandKind::Rel32],
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(mangle::runtime_symbol(dispatcher)),
            RelocKind::PcRel32,
        )],
    );
    for result in &packet.abi.results {
        let offset = match result.slot {
            Some(AbiSlot::Integer(Gpr::Rax)) => 128,
            Some(AbiSlot::Integer(Gpr::Rdx)) => 136,
            Some(AbiSlot::Float(Xmm::Xmm0)) => 144,
            Some(AbiSlot::Float(Xmm::Xmm1)) => 152,
            _ => return Err(LoweringError::InvalidOperands),
        };
        load_stack(
            builder,
            offset,
            results[result.index as usize].reg,
            result.ty.ty,
        )?;
    }
    Ok(())
}
