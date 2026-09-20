//! TLAB / TurnRegion bump、SafepointPoll 与 StackCheck 热路。

use crate::backend::x64::inst::{Operand, RelocKind, RelocTarget};
use crate::backend::x64::mangle;
use crate::backend::x64::reg::{Gpr, Reg};
use crate::backend::x64::table::{Access, OperandKind};
use crate::frontend::gir::body::SourceInfo;
use crate::frontend::gir::passing::PassingClass;
use crate::frontend::gir::placement::PlacementKind;
use crate::lir::body::{Op, Symbol};
use crate::runtime::gc_metadata_contract::GC_BLOCK_BYTES;
use crate::runtime::local_heap::{
    CONTROL_GENERATION_SHIFT, CONTROL_REPRESENTATION_SHIFT, GENERATION_NURSERY,
    OBJECT_HEADER_BYTES, REPRESENTATION_COMPRESSED_REF, REPRESENTATION_LOCAL_DIRECT,
    REPRESENTATION_TURN_REGION,
};
use crate::runtime::processor::{
    tlab_cursor_offset, tlab_limit_offset, turn_region_cursor_offset, turn_region_limit_offset,
};

use super::{Builder, LowerCtx, LoweringError, SiteValue, gpr, imm, mem_base, move_kinds, reg};

const REL32: &[OperandKind] = &[OperandKind::Rel32];
const MEM_R64: &[OperandKind] = &[OperandKind::Mem, OperandKind::R64];
const RM64_R64: &[OperandKind] = &[OperandKind::Rm64, OperandKind::R64];
const R32_IMM32: &[OperandKind] = &[OperandKind::R32, OperandKind::Imm32];

pub(super) fn lower(
    op: &Op,
    operands: &[SiteValue],
    results: &[SiteValue],
    source: &SourceInfo,
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    match op {
        Op::GcAlloc {
            descriptor,
            align,
            placement,
            compressed,
        } => gc_alloc(
            *descriptor,
            *align,
            *placement,
            *compressed,
            results,
            ctx,
            builder,
        ),
        Op::RegionAlloc {
            descriptor, align, ..
        } => region_alloc(*descriptor, *align, results, ctx, builder),
        Op::SafepointPoll { .. } => safepoint_poll(ctx, builder),
        Op::StackCheck => stack_check(ctx, builder),
        _ => {
            let _ = (operands, source);
            Err(LoweringError::InvalidOperands)
        }
    }
}

fn gc_alloc(
    descriptor: [u8; 32],
    align: u32,
    placement: PlacementKind,
    compressed: bool,
    results: &[SiteValue],
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let dest = results.first().map(|value| value.reg);
    if placement == PlacementKind::TurnRegion {
        return bump(
            dest,
            descriptor,
            align,
            false,
            true,
            ctx,
            builder,
            "gc_region_slow",
        );
    }
    if placement == PlacementKind::LocalHeap && fast_path_eligible(descriptor, align, ctx) {
        return bump(
            dest,
            descriptor,
            align,
            compressed,
            false,
            ctx,
            builder,
            "gc_alloc_slow",
        );
    }
    slow_alloc(dest, compressed, builder, "gc_alloc_slow")
}

fn region_alloc(
    descriptor: [u8; 32],
    align: u32,
    results: &[SiteValue],
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let dest = results.first().map(|value| value.reg);
    bump(
        dest,
        descriptor,
        align,
        false,
        true,
        ctx,
        builder,
        "gc_region_slow",
    )
}

fn fast_path_eligible(descriptor: [u8; 32], align: u32, ctx: LowerCtx<'_>) -> bool {
    let Some(universe) = ctx.universe else {
        return false;
    };
    let Ok(record) = universe.record(&descriptor) else {
        return false;
    };
    if record.passing & PassingClass::RESOURCE.bits() != 0 {
        return false;
    }
    let Some((payload, _)) = record.layout else {
        return false;
    };
    let Some(total) = OBJECT_HEADER_BYTES.checked_add(payload) else {
        return false;
    };
    align.is_power_of_two() && align <= 256 && total <= u64::from(GC_BLOCK_BYTES)
}

fn bump(
    dest: Option<Reg>,
    descriptor: [u8; 32],
    align: u32,
    compressed: bool,
    turn_region: bool,
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
    slow: &str,
) -> Result<(), LoweringError> {
    let Some(universe) = ctx.universe else {
        return slow_alloc(dest, compressed, builder, slow);
    };
    let Ok(record) = universe.record(&descriptor) else {
        return slow_alloc(dest, compressed, builder, slow);
    };
    let Some((payload, _)) = record.layout else {
        return slow_alloc(dest, compressed, builder, slow);
    };
    let Some(type_id) = universe.type_id(&descriptor) else {
        return slow_alloc(dest, compressed, builder, slow);
    };
    let pad = padding(
        OBJECT_HEADER_BYTES
            .checked_add(payload)
            .ok_or(LoweringError::InvalidOperands)?,
        align,
    );
    let step = OBJECT_HEADER_BYTES
        .checked_add(payload)
        .and_then(|bytes| bytes.checked_add(pad))
        .ok_or(LoweringError::InvalidOperands)?;
    let step = i32::try_from(step).map_err(|_| LoweringError::InvalidOperands)?;
    let (cursor_off, limit_off) = if turn_region {
        (
            i32::try_from(turn_region_cursor_offset()).expect("偏移适配 i32"),
            i32::try_from(turn_region_limit_offset()).expect("偏移适配 i32"),
        )
    } else {
        (
            i32::try_from(tlab_cursor_offset()).expect("偏移适配 i32"),
            i32::try_from(tlab_limit_offset()).expect("偏移适配 i32"),
        )
    };
    let r11 = gpr(Gpr::R11);
    let r15 = gpr(Gpr::R15);
    builder.clobber_gpr(Gpr::R11);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Read,
        vec![mem_base(r15, cursor_off), reg(r11)],
    );
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![mem_base(r11, step), reg(r11)],
    );
    let overflow = builder.label();
    let done = builder.label();
    builder.emit("jb", REL32, Access::Read, vec![Operand::Label(overflow)]);
    builder.emit(
        "cmp",
        RM64_R64,
        Access::Read,
        vec![mem_base(r15, limit_off), reg(r11)],
    );
    builder.emit("jb", REL32, Access::Read, vec![Operand::Label(overflow)]);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![mem_base(r15, cursor_off), reg(r11)],
    );
    // cursor 已前进；header 写在旧 cursor = r11 - step。
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![mem_base(r11, -step), reg(r11)],
    );
    let repr = if turn_region {
        REPRESENTATION_TURN_REGION
    } else if compressed {
        REPRESENTATION_COMPRESSED_REF
    } else {
        REPRESENTATION_LOCAL_DIRECT
    };
    let control = u64::from(type_id)
        | (u64::from(GENERATION_NURSERY) << CONTROL_GENERATION_SHIFT)
        | (repr << CONTROL_REPRESENTATION_SHIFT);
    emit_imm64(builder, Gpr::Rax, control);
    builder.clobber_gpr(Gpr::Rax);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![mem_base(r11, 0), reg(gpr(Gpr::Rax))],
    );
    emit_imm64(builder, Gpr::Rax, payload);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![mem_base(r11, 8), reg(gpr(Gpr::Rax))],
    );
    if let Some(dest) = dest {
        builder.emit(
            "lea",
            MEM_R64,
            Access::Address,
            vec![
                mem_base(
                    r11,
                    i32::try_from(OBJECT_HEADER_BYTES).expect("header 适配 i32"),
                ),
                reg(dest),
            ],
        );
    }
    builder.emit("jmp", REL32, Access::Read, vec![Operand::Label(done)]);
    builder.define(overflow);
    slow_alloc(dest, compressed, builder, slow)?;
    builder.define(done);
    Ok(())
}

fn slow_alloc(
    dest: Option<Reg>,
    compressed: bool,
    builder: &mut Builder,
    name: &str,
) -> Result<(), LoweringError> {
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rdx)), imm(u64::from(compressed))],
    );
    builder.clobber_gpr(Gpr::Rax);
    builder.clobber_gpr(Gpr::Rbx);
    builder.clobber_gpr(Gpr::Rcx);
    builder.clobber_gpr(Gpr::Rdx);
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
    if let Some(dest) = dest {
        if dest != gpr(Gpr::Rax) {
            builder.emit(
                "mov",
                move_kinds(64),
                Access::Write,
                vec![reg(dest), reg(gpr(Gpr::Rax))],
            );
        }
    }
    Ok(())
}

fn safepoint_poll(ctx: LowerCtx<'_>, builder: &mut Builder) -> Result<(), LoweringError> {
    let offset = ctx
        .raw
        .map(|raw| raw.scheduler().poll_flags_offset())
        .unwrap_or(0);
    let offset = i32::try_from(offset).expect("poll_flags 偏移适配 i32");
    builder.clobber_gpr(Gpr::Rax);
    builder.emit(
        "mov",
        &[OperandKind::Rm32, OperandKind::R32],
        Access::Read,
        vec![mem_base(gpr(Gpr::R15), offset), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "test",
        &[OperandKind::Rm32, OperandKind::R32],
        Access::Read,
        vec![reg(gpr(Gpr::Rax)), reg(gpr(Gpr::Rax))],
    );
    let cold = builder.label();
    let done = builder.label();
    builder.emit("jne", REL32, Access::Read, vec![Operand::Label(cold)]);
    builder.emit("jmp", REL32, Access::Read, vec![Operand::Label(done)]);
    builder.define(cold);
    runtime_call(builder, "safepoint_slow")?;
    builder.define(done);
    Ok(())
}

fn stack_check(ctx: LowerCtx<'_>, builder: &mut Builder) -> Result<(), LoweringError> {
    let offset = ctx
        .raw
        .map(|raw| raw.coroutine().stack_check_offset)
        .unwrap_or(64);
    let offset = i32::try_from(offset).expect("stack_check 偏移适配 i32");
    builder.clobber_gpr(Gpr::R11);
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![mem_base(gpr(Gpr::Rsp), 0), reg(gpr(Gpr::R11))],
    );
    builder.emit(
        "cmp",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R14), offset), reg(gpr(Gpr::R11))],
    );
    let cold = builder.label();
    let done = builder.label();
    builder.emit("ja", REL32, Access::Read, vec![Operand::Label(cold)]);
    builder.emit("jmp", REL32, Access::Read, vec![Operand::Label(done)]);
    builder.define(cold);
    runtime_call(builder, "morestack_or_poll")?;
    builder.define(done);
    Ok(())
}

fn runtime_call(builder: &mut Builder, name: &str) -> Result<(), LoweringError> {
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

fn emit_imm64(builder: &mut Builder, dest: Gpr, value: u64) {
    if value <= u64::from(u32::MAX) {
        builder.emit(
            "mov",
            R32_IMM32,
            Access::Write,
            vec![reg(gpr(dest)), imm(value)],
        );
    } else {
        builder.emit(
            "mov",
            &[OperandKind::R64, OperandKind::Imm64],
            Access::Write,
            vec![reg(gpr(dest)), imm(value)],
        );
    }
}

fn padding(bytes: u64, align: u32) -> u64 {
    let align = u64::from(align.max(8));
    bytes.wrapping_neg() & (align - 1)
}
