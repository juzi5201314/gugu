//! TLAB / TurnRegion bump、SafepointPoll 与 StackCheck 热路。

use crate::gir::body::SourceInfo;
use crate::gir::passing::PassingClass;
use crate::gir::placement::PlacementKind;
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
use crate::x64::inst::{Operand, RelocKind, RelocTarget};
use crate::x64::mangle;
use crate::x64::reg::{Gpr, Reg};
use crate::x64::table::{Access, OperandKind};

use super::{
    Builder, LowerCtx, LoweringError, SiteValue, ensure_runtime_glue, gpr, imm, mem_base,
    move_kinds, reg,
};

const REL32: &[OperandKind] = &[OperandKind::Rel32];
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
            operands,
            results,
            ctx,
            builder,
        ),
        Op::RegionAlloc {
            descriptor, align, ..
        } => region_alloc(*descriptor, *align, operands, results, ctx, builder),
        Op::SafepointPoll { .. } => safepoint_poll(ctx, builder),
        // `StackCheck` 是标记：prologue 完全由 frame 阶段按 `FrameLayout` 合成，选指阶段
        // 不产出任何指令。这里只登记站点原有的两个标签 id（`check` 与 `cold`）：`select`
        // 阶段按 `label_usage` 记账终结符的标签基址，分配阶段复用同一对 id 重写 prologue
        // 时编号必须与记账一致（标签定义本身编码为零字节）。
        Op::StackCheck => {
            let check = builder.label();
            let cold = builder.label();
            builder.define(check);
            builder.define(cold);
            Ok(())
        }
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
    operands: &[SiteValue],
    results: &[SiteValue],
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let dest = results.first().map(|value| value.reg);
    let bytes = bytes_operand(operands)?;
    if placement == PlacementKind::TurnRegion {
        return bump(
            dest,
            descriptor,
            align,
            placement,
            compressed,
            bytes,
            ctx,
            builder,
            "gc_region_slow",
        );
    }
    if placement == PlacementKind::SharedHeap {
        return slow_alloc(
            dest,
            descriptor,
            align,
            placement,
            compressed,
            bytes,
            ctx,
            builder,
            "gc_shared_alloc",
        );
    }
    if placement == PlacementKind::LocalHeap && fast_path_eligible(descriptor, align, ctx) {
        return bump(
            dest,
            descriptor,
            align,
            placement,
            compressed,
            bytes,
            ctx,
            builder,
            "gc_alloc_slow",
        );
    }
    slow_alloc(
        dest,
        descriptor,
        align,
        placement,
        compressed,
        bytes,
        ctx,
        builder,
        "gc_alloc_slow",
    )
}

fn region_alloc(
    descriptor: [u8; 32],
    align: u32,
    operands: &[SiteValue],
    results: &[SiteValue],
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
    let dest = results.first().map(|value| value.reg);
    let bytes = bytes_operand(operands)?;
    bump(
        dest,
        descriptor,
        align,
        PlacementKind::TurnRegion,
        false,
        bytes,
        ctx,
        builder,
        "gc_region_slow",
    )
}

/// 取 LIR 的 bytes 操作数：它是**唯一**合法的 payload 尺寸来源；运行期算出的尺寸（变长数组）
/// 与常量尺寸走同一寄存器通道，不拿 descriptor 的静态布局覆盖运行期请求。
fn bytes_operand(operands: &[SiteValue]) -> Result<SiteValue, LoweringError> {
    operands
        .first()
        .copied()
        .ok_or(LoweringError::InvalidOperands)
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
    placement: PlacementKind,
    compressed: bool,
    bytes: SiteValue,
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
    slow: &str,
) -> Result<(), LoweringError> {
    let turn_region = placement == PlacementKind::TurnRegion;
    let Some(universe) = ctx.universe else {
        return slow_alloc(
            dest, descriptor, align, placement, compressed, bytes, ctx, builder, slow,
        );
    };
    let Some(type_id) = universe.type_id(&descriptor) else {
        return slow_alloc(
            dest, descriptor, align, placement, compressed, bytes, ctx, builder, slow,
        );
    };
    // payload 对齐属于语言承诺；header 落在 `payload - HEADER`。granule 下界让 header 仍是
    // granule 的整数倍，object-start 位图与 runtime 的 `resolve` 才能按同一规则反查。
    let granular = u64::from(align).max(u64::from(HEAP_GRANULE_BYTES));
    // 非规范对齐或超出 imm32 的算术走 runtime slow path，不另写经验常量。
    let (Ok(adjust), Ok(mask), Ok(block_limit)) = (
        i32::try_from(OBJECT_HEADER_BYTES + granular - 1),
        i32::try_from(-(granular as i64)),
        i32::try_from(u64::from(GC_BLOCK_BYTES) + 1 - u64::from(OBJECT_HEADER_BYTES) - granular),
    ) else {
        return slow_alloc(
            dest, descriptor, align, placement, compressed, bytes, ctx, builder, slow,
        );
    };
    let (cursor_off, limit_off) = tlab_offsets(ctx, turn_region);
    let r11 = gpr(Gpr::R11);
    let r15 = gpr(Gpr::R15);
    let size = gpr(Gpr::R10);
    builder.clobber_gpr(Gpr::R10);
    builder.clobber_gpr(Gpr::R11);
    builder.clobber_gpr(Gpr::Rax);
    // payload 尺寸来自 LIR 的 bytes 操作数：先搬到专用 scratch，后面的算术与对象头写入都
    // 以它为准，运行期算出的尺寸不会退化成 descriptor 的静态布局大小。
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Read,
        vec![reg(bytes.reg), reg(size)],
    );
    // 普通对象不得跨块：`header + align-1 + bytes` 必须仍在同一 block 内，越界走 slow path。
    builder.emit(
        "cmp",
        &[OperandKind::Rm64, OperandKind::Imm32],
        Access::Read,
        vec![reg(size), imm(u64::from(block_limit as u32))],
    );
    let overflow = builder.label();
    let done = builder.label();
    builder.emit("ja", REL32, Access::Read, vec![Operand::Label(overflow)]);
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
        &[OperandKind::Rm64, OperandKind::R64],
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rax)), reg(size)],
    );
    builder.emit("jb", REL32, Access::Read, vec![Operand::Label(overflow)]);
    builder.emit(
        "cmp",
        &[OperandKind::Rm64, OperandKind::R64],
        Access::Read,
        vec![mem_base(r15, limit_off), reg(gpr(Gpr::Rax))],
    );
    // `cmp limit, new_cursor; jb`：new_cursor 超过 span 才进 slow path。
    // `ja` 会在仍有空间时离开快路径，并在 cursor 为 0 时把对象头写到空地址。
    builder.emit("jb", REL32, Access::Read, vec![Operand::Label(overflow)]);
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
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![
            mem_base(
                r11,
                -i32::try_from(OBJECT_HEADER_BYTES / 2).expect("header 半宽适配 i32"),
            ),
            reg(size),
        ],
    );
    if turn_region {
        record_region_payload(builder);
    } else {
        mark_object_start(builder);
    }
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
    slow_alloc(
        dest, descriptor, align, placement, compressed, bytes, ctx, builder, slow,
    )?;
    builder.define(done);
    Ok(())
}

/// r11 为区域 payload，r10 为动态载荷大小；不引入 safepoint。
pub(crate) fn record_region_payload(builder: &mut Builder) {
    use crate::runtime::processor::PROCESSOR_REGION_PAYLOAD_BYTES_OFFSET;
    use crate::runtime::region_schema::{REGION_OBJECTS_OFFSET, REGION_PAYLOAD_BYTES_OFFSET};
    let rax = gpr(Gpr::Rax);
    let size = gpr(Gpr::R10);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![reg(rax), reg(gpr(Gpr::R11))],
    );
    builder.emit(
        "sub",
        &[OperandKind::Rm64, OperandKind::Imm32],
        Access::ReadWrite,
        vec![reg(rax), imm(OBJECT_HEADER_BYTES)],
    );
    builder.emit(
        "and",
        &[OperandKind::Rm64, OperandKind::Imm32],
        Access::ReadWrite,
        vec![reg(rax), imm((-(i64::from(GC_BLOCK_BYTES))) as u64)],
    );
    builder.emit(
        "add",
        &[OperandKind::Rm64, OperandKind::Imm8],
        Access::ReadWrite,
        vec![
            mem_base(rax, (GC_BLOCK_BYTES + REGION_OBJECTS_OFFSET) as i32),
            imm(1),
        ],
    );
    builder.emit(
        "add",
        move_kinds(64),
        Access::ReadWrite,
        vec![
            mem_base(rax, (GC_BLOCK_BYTES + REGION_PAYLOAD_BYTES_OFFSET) as i32),
            reg(size),
        ],
    );
    builder.emit(
        "add",
        move_kinds(64),
        Access::ReadWrite,
        vec![
            mem_base(gpr(Gpr::R15), PROCESSOR_REGION_PAYLOAD_BYTES_OFFSET as i32),
            reg(size),
        ],
    );
    mark_start(
        builder,
        u64::from(GC_BLOCK_BYTES),
        u64::from(crate::runtime::region_schema::REGION_OBJECT_START_OFFSET),
        Gpr::Rdx,
    );
}

/// r11 为 payload；对象头已经写入。无 safepoint 的同一分配序列发布 object-start。
pub(crate) fn mark_object_start(builder: &mut Builder) {
    mark_start(
        builder,
        crate::runtime::gc_metadata_contract::GC_ARENA_BYTES,
        crate::runtime::local_heap_schema::HEAP_OBJECT_START_OFFSET,
        Gpr::R10,
    );
}

fn mark_start(builder: &mut Builder, range_bytes: u64, bitmap_offset: u64, scratch: Gpr) {
    let rm_imm = &[OperandKind::Rm64, OperandKind::Imm32];
    let shift = &[OperandKind::Rm64, OperandKind::Imm8];
    let rax = gpr(Gpr::Rax);
    let r10 = gpr(scratch);
    let rcx = gpr(Gpr::Rcx);
    builder.clobber_gpr(scratch);
    builder.clobber_gpr(Gpr::Rcx);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![reg(r10), reg(gpr(Gpr::R11))],
    );
    builder.emit(
        "sub",
        rm_imm,
        Access::ReadWrite,
        vec![reg(r10), imm(OBJECT_HEADER_BYTES)],
    );
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![reg(rax), reg(r10)],
    );
    builder.emit(
        "and",
        rm_imm,
        Access::ReadWrite,
        vec![reg(rax), imm((-(range_bytes as i64)) as u64)],
    );
    builder.emit(
        "and",
        rm_imm,
        Access::ReadWrite,
        vec![reg(r10), imm(range_bytes - 1)],
    );
    builder.emit(
        "shr",
        shift,
        Access::ReadWrite,
        vec![
            reg(r10),
            imm(u64::from(HEAP_GRANULE_BYTES.trailing_zeros())),
        ],
    );
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![reg(rcx), reg(r10)],
    );
    builder.emit("and", rm_imm, Access::ReadWrite, vec![reg(rcx), imm(63)]);
    builder.emit("shr", shift, Access::ReadWrite, vec![reg(r10), imm(6)]);
    builder.emit("shl", shift, Access::ReadWrite, vec![reg(r10), imm(3)]);
    builder.emit(
        "add",
        move_kinds(64),
        Access::ReadWrite,
        vec![reg(rax), reg(r10)],
    );
    builder.emit(
        "lea",
        &[OperandKind::Mem, OperandKind::R64],
        Access::Address,
        vec![
            mem_base(rax, (range_bytes + bitmap_offset) as i32),
            reg(rax),
        ],
    );
    emit_imm64(builder, scratch, 1);
    builder.emit(
        "shl",
        &[OperandKind::Rm64, OperandKind::Cl],
        Access::ReadWrite,
        vec![reg(r10), reg(rcx)],
    );
    builder.emit(
        "or",
        move_kinds(64),
        Access::ReadWrite,
        vec![mem_base(rax, 0), reg(r10)],
    );
}

/// 分配 slow path：按固定 ABI 传参并调用 runtime 入口。
///
/// `rax` = type_id、`rbx` = payload_bytes、`rcx` = align、`rdx` = flags（bit0 压缩、bits1..2
/// placement 编码），`rax` 返回 payload。bytes 先搬进 `rbx`，避免随后写 `rax`/`rcx`/`rdx` 的
/// 立即数覆盖仍要读的操作数寄存器。descriptor 未登记或没有 type_id 是编译期诊断，不放行。
fn slow_alloc(
    dest: Option<Reg>,
    descriptor: [u8; 32],
    align: u32,
    placement: PlacementKind,
    compressed: bool,
    bytes: SiteValue,
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
    name: &str,
) -> Result<(), LoweringError> {
    ensure_runtime_glue(name)?;
    let type_id = match ctx.universe {
        // 探针/legalize 没有冻结宇宙，序列只供结构检查、不参与镜像：用 0 占位。
        None => 0,
        // 真实编译里 descriptor 未登记是内部错误，不放行到镜像。
        Some(universe) => universe
            .type_id(&descriptor)
            .ok_or(LoweringError::Unsupported {
                op: "GcAlloc",
                detail: "分配 descriptor 未在冻结宇宙中登记",
            })?,
    };
    let Some(code) = placement_code(placement) else {
        return Err(LoweringError::Unsupported {
            op: "GcAlloc",
            detail: "placement 不适用分配 ABI",
        });
    };
    let flags = u64::from(compressed) | (code << 1);
    builder.emit(
        "mov",
        move_kinds(64),
        Access::Write,
        vec![reg(gpr(Gpr::Rbx)), reg(bytes.reg)],
    );
    emit_imm64(builder, Gpr::Rax, u64::from(type_id));
    emit_imm64(builder, Gpr::Rcx, u64::from(align));
    emit_imm64(builder, Gpr::Rdx, flags);
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

/// region 生命周期入口的 lowering：`region` 编号是计划的区域身份，`export` 是 `publish`
/// 声明的导出位集合（`region_schema` 的 export summary）。
///
/// 与分配族同源：实参是**lowering 合成的立即数**而非操作数（四个 op 的 LIR 形状是空参数空
/// 结果），先落 `rax`（`publish` 再把 export 落 `rbx`），再按 `INTERNAL_INTEGER` 顺序发调用。
/// 入口结果为空，调用后不做结果搬运。区域身份必须在代码里存活，否则 `publish` 的 declared
/// 恒为 0、`reset` 的 summary 判据永远为真——那是明令禁止的假释放。
pub(super) fn region_end(op: &Op, builder: &mut Builder) -> Result<(), LoweringError> {
    let (name, region, export) = match op {
        Op::RegionPublish { region, export } => ("gc_region_publish", *region, Some(*export)),
        Op::RegionReset { region } => ("gc_region_reset", *region, None),
        Op::PromoteManaged { region } => ("gc_region_promote", *region, None),
        Op::RegionTransfer { region } => ("gc_region_transfer", *region, None),
        _ => {
            return Err(LoweringError::Unsupported {
                op: "RegionEnd",
                detail: "非 region 生命周期 op 不得走 region 入口 ABI",
            });
        }
    };
    ensure_runtime_glue(name)?;
    emit_imm64(builder, Gpr::Rax, u64::from(region));
    builder.clobber_gpr(Gpr::Rax);
    if let Some(export) = export {
        emit_imm64(builder, Gpr::Rbx, u64::from(export));
        builder.clobber_gpr(Gpr::Rbx);
    }
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

/// `flags` bits1..2 的 placement 编码；与对象头位布局分开。非分配 placement 返回 `None`，
/// 它们不得经分配 ABI 进入运行期。
fn placement_code(placement: PlacementKind) -> Option<u64> {
    match placement {
        PlacementKind::TurnRegion => Some(0),
        PlacementKind::LocalHeap => Some(1),
        PlacementKind::SharedHeap => Some(2),
        PlacementKind::Resource => Some(3),
        _ => None,
    }
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

pub(crate) fn safepoint_poll(
    ctx: LowerCtx<'_>,
    builder: &mut Builder,
) -> Result<(), LoweringError> {
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

fn runtime_call(builder: &mut Builder, name: &str) -> Result<(), LoweringError> {
    ensure_runtime_glue(name)?;
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
