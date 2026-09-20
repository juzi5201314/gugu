//! TLAB / TurnRegion bump、SafepointPoll 与 StackCheck 热路。

use crate::backend::x64::inst::{Operand, RelocKind, RelocTarget};
use crate::backend::x64::mangle;
use crate::backend::x64::reg::{Gpr, Reg};
use crate::backend::x64::table::{Access, OperandKind};
use crate::frontend::gir::body::SourceInfo;
use crate::frontend::gir::passing::PassingClass;
use crate::frontend::gir::placement::PlacementKind;
use crate::lir::body::Op;
use crate::runtime::gc_metadata_contract::GC_BLOCK_BYTES;
use crate::runtime::local_heap::{
    CONTROL_GENERATION_SHIFT, CONTROL_REPRESENTATION_SHIFT, GENERATION_NURSERY,
    OBJECT_HEADER_BYTES, REPRESENTATION_COMPRESSED_REF, REPRESENTATION_LOCAL_DIRECT,
    REPRESENTATION_TURN_REGION,
};
use crate::runtime::local_heap_schema::HEAP_GRANULE_BYTES;
use crate::runtime::processor::{
    poll_flags_offset, tlab_cursor_offset, tlab_limit_offset, turn_region_cursor_offset,
    turn_region_limit_offset,
};

use super::{Builder, LowerCtx, LoweringError, SiteValue, gpr, imm, mem_base, move_kinds, reg};

const REL32: &[OperandKind] = &[OperandKind::Rel32];
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
    // payload 对齐属于语言承诺；header 落在 `payload - HEADER`。granule 下界让 header 仍是
    // granule 的整数倍，object-start 位图与 runtime 的 `resolve` 才能按同一规则反查。
    let align = u64::from(align).max(u64::from(HEAP_GRANULE_BYTES));
    // 非规范对齐或超出 imm32 的算术走 runtime slow path，不另写经验常量。
    let (Ok(adjust), Ok(mask), Ok(payload)) = (
        i32::try_from(OBJECT_HEADER_BYTES + align - 1),
        i32::try_from(-(align as i64)),
        i32::try_from(payload),
    ) else {
        return slow_alloc(dest, compressed, builder, slow);
    };
    let (cursor_off, limit_off) = tlab_offsets(ctx, turn_region);
    let r11 = gpr(Gpr::R11);
    let r15 = gpr(Gpr::R15);
    builder.clobber_gpr(Gpr::R11);
    builder.clobber_gpr(Gpr::Rax);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Read,
        vec![mem_base(r15, cursor_off), reg(r11)],
    );
    // `add` 才会为 `jc` 留下真实的进位：`lea` 不改 flags，无法检测地址环绕。
    builder.emit(
        "add",
        &[OperandKind::Rm64, OperandKind::Imm32],
        Access::ReadWrite,
        vec![reg(r11), imm(u64::from(adjust as u32))],
    );
    let overflow = builder.label();
    let done = builder.label();
    builder.emit("jb", REL32, Access::Read, vec![Operand::Label(overflow)]);
    builder.emit(
        "and",
        &[OperandKind::Rm64, OperandKind::Imm32],
        Access::ReadWrite,
        vec![reg(r11), imm(mask as u64)],
    );
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), reg(r11)],
    );
    builder.emit(
        "add",
        &[OperandKind::Rm64, OperandKind::Imm32],
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rax)), imm(payload as u64)],
    );
    builder.emit("jb", REL32, Access::Read, vec![Operand::Label(overflow)]);
    builder.emit(
        "cmp",
        &[OperandKind::Rm64, OperandKind::R64],
        Access::Read,
        vec![mem_base(r15, limit_off), reg(gpr(Gpr::Rax))],
    );
    builder.emit("ja", REL32, Access::Read, vec![Operand::Label(overflow)]);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![mem_base(r15, cursor_off), reg(gpr(Gpr::Rax))],
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
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![
            mem_base(
                r11,
                -i32::try_from(OBJECT_HEADER_BYTES).expect("header 适配 i32"),
            ),
            reg(gpr(Gpr::Rax)),
        ],
    );
    emit_imm64(builder, Gpr::Rax, payload as u64);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![
            mem_base(
                r11,
                -i32::try_from(OBJECT_HEADER_BYTES / 2).expect("header 半宽适配 i32"),
            ),
            reg(gpr(Gpr::Rax)),
        ],
    );
    if let Some(dest) = dest {
        builder.emit(
            "mov",
            move_kinds(64),
            Access::Write,
            vec![reg(dest), reg(r11)],
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
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(mangle::runtime_symbol(name)),
            RelocKind::PcRel32,
        )],
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

/// TLAB/TurnRegion 的 cursor/limit 偏移：镜像内取契约字段，探针退回同一布局神谕。
fn tlab_offsets(ctx: LowerCtx<'_>, turn_region: bool) -> (i32, i32) {
    let (cursor, limit) = match (ctx.raw, turn_region) {
        (Some(raw), true) => (
            raw.scheduler().turn_region_cursor_offset(),
            raw.scheduler().turn_region_limit_offset(),
        ),
        (Some(raw), false) => (
            raw.scheduler().tlab_cursor_offset(),
            raw.scheduler().tlab_limit_offset(),
        ),
        (None, true) => (turn_region_cursor_offset(), turn_region_limit_offset()),
        (None, false) => (tlab_cursor_offset(), tlab_limit_offset()),
    };
    (
        i32::try_from(cursor).expect("TLAB cursor 偏移适配 i32"),
        i32::try_from(limit).expect("TLAB limit 偏移适配 i32"),
    )
}

fn safepoint_poll(ctx: LowerCtx<'_>, builder: &mut Builder) -> Result<(), LoweringError> {
    let raw = ctx
        .raw
        .map_or_else(poll_flags_offset, |raw| raw.scheduler().poll_flags_offset());
    let offset = i32::try_from(raw).expect("poll_flags 偏移适配 i32");
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
    // 快路是无 pending poll 时的直落路径，慢路只由条件分支进入。
    let cold = builder.label();
    builder.emit("jne", REL32, Access::Read, vec![Operand::Label(cold)]);
    let done = builder.label();
    builder.emit("jmp", REL32, Access::Read, vec![Operand::Label(done)]);
    builder.define(cold);
    runtime_call(builder, "safepoint_slow")?;
    builder.define(done);
    Ok(())
}

fn stack_check(ctx: LowerCtx<'_>, builder: &mut Builder) -> Result<(), LoweringError> {
    let raw = ctx
        .raw
        .map_or_else(crate::runtime::stack_check_offset, |raw| {
            raw.coroutine().stack_check_offset
        });
    let offset = i32::try_from(raw).expect("stack_check 偏移适配 i32");
    builder.clobber_gpr(Gpr::R11);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![reg(gpr(Gpr::R11)), reg(gpr(Gpr::Rsp))],
    );
    builder.emit(
        "cmp",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R14), offset), reg(gpr(Gpr::R11))],
    );
    // candidate < stack_check 走冷路：容量不足与 poison 由同一次比较捕获。
    let cold = builder.label();
    builder.emit("ja", REL32, Access::Read, vec![Operand::Label(cold)]);
    let done = builder.label();
    builder.emit("jmp", REL32, Access::Read, vec![Operand::Label(done)]);
    builder.define(cold);
    runtime_call(builder, "morestack_or_poll")?;
    builder.define(done);
    Ok(())
}

fn runtime_call(builder: &mut Builder, name: &str) -> Result<(), LoweringError> {
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(mangle::runtime_symbol(name)),
            RelocKind::PcRel32,
        )],
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
