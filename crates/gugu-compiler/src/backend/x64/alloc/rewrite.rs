//! 序列改写：操作数落地、常量重建、prologue/epilogue 插入与重拼。
//!
//! 每个站点的序列按点位重建：拷贝组丢弃渲染结果、按 [`Resolution`] 重发；普通指令逐操作数
//! 换成物理位置。替换规则与 form 的操作数种类严格对应：
//!
//! - `Reg(v)` → 物理寄存器：直接换；
//! - `Reg(v)` → spill slot 且种类接受内存（`Rm8/16/32/64`、`XmmRm`）：换成 `[rsp + offset]`
//!   （`ReadWrite` 也安全：槽内是零扩展规范形）；
//! - `Reg(v)` → spill slot 且种类只接受寄存器，或出现在 `Mem` 的 base/index：在指令前插入
//!   `mov scratch, [rsp + offset]`（`mov`/`movsd`/`movups`）并使用 scratch；
//! - `Write` 且种类是 `Rm64`/`XmmRm`：直接写内存；`Write` 且种类是窄宽度或仅寄存器：写
//!   scratch，并在该值在本站点最后一次写入之后补 `mov [rsp + offset], scratch`（值在此之后
//!   仍活跃才发射）；
//! - 可重建的值（常量、symbol 地址、stack 地址）：定义站点整段序列被丢弃，每个使用处按原
//!   `IConst`/`FConst`/`SymbolAddr`/`StackAddr` 的 lowering 重新物化进 scratch；
//! - `StackAddr` 的 `Mem { base: Virtual(FRAME_SLOT_BASE + slot) }` → `[rsp + local_offset]`。
//!
//! scratch 取自该点位「未被活跃值占用、不在点位 mask 内、不是本组源/目标」的偏好序首个
//! 寄存器（GPR 先试 `r11`）；空闲寄存器不足按内部不变量失败，绝不产出错误机器码。

use std::cell::Cell;
use std::ops::Range;

use super::super::inst::{Inst, LabelDefinition, LabelId, Mem, Operand, Sequence};
use super::super::layout;
use super::super::lower::{self, Builder, LowerCtx, LoweringError, SiteValue};
use super::super::reg::{Clobbers, FRAME_SLOT_BASE, Gpr, Reg};
use super::super::select::{self, SelectedBlock, SelectedFunction, SiteKind};
use super::super::table::{self, Access, OperandKind};
use super::super::verify;
use super::frame::{self, FrameLayout};
use super::live::{self, LiveInfo};
use super::parallel::{EmittedMove, Loc, PREPEND_SITE, Resolution};
use super::{AllocError, Location, Placement, RegClass, ValueAllocation};
use crate::lir::body::{Body, Definition, Op, Type};
use crate::target::TargetName;

/// 改写阶段累计的溢出流量统计。
///
/// `Emit` 全程按共享引用传递，因此用 `Cell` 就地累加：计数量只用于 payload 统计，
/// 不影响发射出的机器码。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SpillTraffic {
    /// 从 spill slot 读回寄存器的指令数。
    pub(crate) reloads: u32,
    /// 写回 spill slot 的指令数。
    pub(crate) stores: u32,
    /// 重建常量/symbol/栈地址的次数。
    pub(crate) rematerializations: u32,
}

/// 改写产物。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Rewritten {
    pub(crate) blocks: Vec<SelectedBlock>,
    pub(crate) sequence: Sequence,
    pub(crate) rel8_count: u32,
    /// prologue 与各 epilogue 的函数序列区间；只有这里允许写 `rsp`。
    pub(crate) abi_regions: Vec<Range<u32>>,
    /// 溢出流量统计。
    pub(crate) traffic: SpillTraffic,
}

/// 改写上下文。
pub(crate) struct Emit<'a> {
    pub(crate) body: &'a Body,
    pub(crate) frame: &'a FrameLayout,
    pub(crate) values: &'a [ValueAllocation],
    pub(crate) points: &'a [live::Point],
    pub(crate) target: TargetName,
    /// 可作 scratch 的 GPR：`r11` 与通用池减去未保存的 callee-saved。
    pub(crate) scratch_gpr: Clobbers,
    /// 溢出流量累加器。
    traffic: Cell<SpillTraffic>,
}

/// 改写一个函数的全部站点并重拼序列。
#[expect(
    clippy::too_many_arguments,
    reason = "改写需要分配结果、frame、解析结果与入口契约四类输入"
)]
pub(crate) fn rewrite(
    body: &Body,
    selected: &SelectedFunction,
    live: &LiveInfo,
    values: &[ValueAllocation],
    frame: &FrameLayout,
    resolution: &Resolution,
    stack_arguments: &[(u32, u32, Type)],
    target: TargetName,
    stack_check_offset: u32,
) -> Result<Rewritten, AllocError> {
    let emit = Emit {
        body,
        frame,
        values,
        points: &live.points,
        target,
        scratch_gpr: scratch_pool(frame),
        traffic: Cell::new(SpillTraffic::default()),
    };
    let mut blocks = selected.blocks.clone();
    let mut regions: Vec<(u32, Range<u32>)> = Vec::new();
    let mut site_index = 0_u32;
    for (order, block) in blocks.iter_mut().enumerate() {
        let site_count = block.sites.len();
        let terminator = terminator_kind(body, block);
        for index in 0..site_count {
            let points = live
                .sites
                .get(site_index as usize)
                .ok_or_else(|| AllocError::new("点位表缺少站点"))?;
            let kind = block.sites[index].kind;
            let prepend = resolution
                .prologue()
                .is_some_and(|group| group.site == PREPEND_SITE)
                && order == 0
                && index == 0;
            let sequence = &mut block.sites[index].lowered;
            let region = rewrite_sequence(
                &emit,
                sequence,
                kind,
                points.points.clone(),
                resolution,
                prepend,
                false,
                stack_arguments,
                stack_check_offset,
            )?;
            if let Some(range) = region {
                regions.push((site_index, range));
            }
            site_index += 1;
        }
        let points = live
            .sites
            .get(site_index as usize)
            .ok_or_else(|| AllocError::new("点位表缺少终结符站点"))?;
        let prepend = resolution
            .prologue()
            .is_some_and(|group| group.site == PREPEND_SITE)
            && order == 0
            && site_count == 0;
        let sequence = &mut block.terminator;
        let region = rewrite_sequence(
            &emit,
            sequence,
            SiteKind::Normal,
            points.points.clone(),
            resolution,
            prepend,
            terminator,
            stack_arguments,
            stack_check_offset,
        )?;
        if let Some(range) = region {
            regions.push((site_index, range));
        }
        site_index += 1;
    }
    let mut sequence = Sequence::new();
    select::stitch_blocks(&blocks, &mut sequence);
    let rel8_count = layout::relax(&mut sequence)
        .map_err(|error| AllocError::new(format!("分支松弛失败：{error}")))?;
    let abi_regions = absolute_regions(&blocks, &regions);
    verify::verify_allocated_sequence(&sequence, target.descriptor().cpu_baseline, &abi_regions)
        .map_err(|error| AllocError::new(format!("分配后序列校验失败：{error}")))?;
    Ok(Rewritten {
        blocks,
        sequence,
        rel8_count,
        abi_regions,
        traffic: emit.traffic.get(),
    })
}

/// 可作 scratch 的 GPR：`r11` 加通用池减去未保存的 callee-saved。
fn scratch_pool(frame: &FrameLayout) -> Clobbers {
    let mut gpr = Gpr::R11.bit();
    for candidate in super::scan::GPR_POOL {
        if matches!(candidate, Gpr::Rbp | Gpr::R12 | Gpr::R13)
            && frame.save_offset(candidate).is_none()
        {
            // 未保存的 callee-saved 寄存器不能被 scratch 破坏。
            continue;
        }
        gpr |= candidate.bit();
    }
    Clobbers { gpr, xmm: u32::MAX }
}

/// 终结符是否需要 epilogue：`Return` 与 `TailCall`。
fn terminator_kind(body: &Body, block: &SelectedBlock) -> bool {
    matches!(
        body.blocks[block.id.index()].terminator,
        crate::lir::body::Terminator::Return { .. } | crate::lir::body::Terminator::TailCall { .. }
    )
}

/// 改写一个序列；返回 prologue/epilogue 在该序列内的指令区间。
#[expect(
    clippy::too_many_arguments,
    reason = "序列改写需要站点分类、点位与解析结果三类输入"
)]
fn rewrite_sequence(
    emit: &Emit<'_>,
    lowered: &mut lower::Lowered,
    kind: SiteKind,
    points: Range<u32>,
    resolution: &Resolution,
    prepend: bool,
    epilogue: bool,
    stack_arguments: &[(u32, u32, Type)],
    stack_check_offset: u32,
) -> Result<Option<Range<u32>>, AllocError> {
    let entries = live::site_plan(lowered)?;
    let original_labels = lowered.sequence.labels.clone();
    let mut builder = Builder::new();
    let mut region = None;
    if prepend || kind == SiteKind::Prologue {
        let start = builder.instruction_count();
        frame::emit_prologue(
            emit,
            &mut builder,
            resolution.prologue(),
            stack_arguments,
            stack_check_offset,
        )?;
        region = Some(start..builder.instruction_count());
        if kind == SiteKind::Prologue {
            // 复用站点原有的两个站内标签 id（`check` 与 `cold`）：标记 lowering 只登记
            // 这两个编号，`select` 的标签记账与这里重写后的编号必须一致。
            for label in &original_labels {
                if label.label.0 > 1 {
                    return Err(AllocError::new(format!(
                        "入口 StackCheck 站点标签 L{} 超出标记编号",
                        label.label.0
                    )));
                }
            }
            lowered.sequence = builder.finish().sequence;
            return Ok(region);
        }
    }
    let mut labels = original_labels;
    let mut pending = pending_stores(emit, lowered, &entries, &points)?;
    for (offset, entry) in entries.iter().enumerate() {
        define_labels(&mut builder, &mut labels, entry.instructions.start)?;
        let point = points.start + u32::try_from(offset).expect("点位偏移适配 u32");
        match &entry.pairs {
            Some(_) => {
                if let Some(group) = resolution.point(point) {
                    emit_moves(emit, &mut builder, &group.moves)?;
                }
            }
            None => {
                let inst = lowered.sequence.instructions[entry.instructions.start as usize].clone();
                let last = entry.instructions.end as usize == lowered.sequence.instructions.len();
                if epilogue && last {
                    let start = builder.instruction_count();
                    frame::emit_epilogue(&mut builder, emit.frame)?;
                    region = Some(start..builder.instruction_count());
                }
                rewrite_instruction(emit, &mut builder, &inst, point, &mut pending)?;
            }
        }
    }
    define_labels(
        &mut builder,
        &mut labels,
        u32::try_from(lowered.sequence.instructions.len()).expect("站点指令数适配 u32"),
    )?;
    if let Some(label) = labels.first() {
        return Err(AllocError::new(format!(
            "站点序列在改写后仍残留未发射的标签 L{}",
            label.label.0
        )));
    }
    lowered.sequence = builder.finish().sequence;
    Ok(region)
}

/// 在某指令位置定义标签。
fn define_labels(
    builder: &mut Builder,
    labels: &mut Vec<LabelDefinition>,
    position: u32,
) -> Result<(), AllocError> {
    let mut index = 0;
    while index < labels.len() {
        if labels[index].at != position {
            index += 1;
            continue;
        }
        builder.define(LabelId(labels[index].label.0));
        labels.remove(index);
    }
    Ok(())
}

/// 站点内窄宽度写入后仍需补 64 位存储的登记项。
struct PendingStore {
    value: u32,
    /// 最后一次写入的点位。
    last_write: u32,
    /// 值在该点位之后仍活跃。
    live_after: bool,
}

/// 扫描站点，登记窄宽度写入的值与它们的最后一次写入点位。
fn pending_stores(
    emit: &Emit<'_>,
    lowered: &lower::Lowered,
    entries: &[live::PlanEntry],
    points: &Range<u32>,
) -> Result<Vec<PendingStore>, AllocError> {
    let mut pending: Vec<PendingStore> = Vec::new();
    for (offset, entry) in entries.iter().enumerate() {
        if entry.pairs.is_some() {
            continue;
        }
        let inst = &lowered.sequence.instructions[entry.instructions.start as usize];
        let form = table::form(inst.form);
        for (position, operand) in inst.operands.iter().enumerate() {
            let write = matches!(
                form.access.get(position),
                Some(Access::Write | Access::ReadWrite)
            );
            if !write {
                continue;
            }
            let Operand::Reg(Reg::Virtual(id)) = operand else {
                continue;
            };
            let Some(value) = emit.values.get(*id as usize) else {
                continue;
            };
            let Some(Location::Slot(_)) = value.location() else {
                continue;
            };
            let kind = form.operands[position];
            if matches!(kind, OperandKind::Rm64 | OperandKind::XmmRm) {
                continue;
            }
            if matches!(form.access.get(position), Some(Access::ReadWrite)) {
                continue;
            }
            let point_index = points.start + u32::try_from(offset).expect("点位偏移适配 u32");
            let def_slot = point_index * 2 + 1;
            let live_after = value.range.1 > def_slot;
            match pending.iter_mut().find(|store| store.value == *id) {
                Some(store) => store.last_write = point_index,
                None => pending.push(PendingStore {
                    value: *id,
                    last_write: point_index,
                    live_after,
                }),
            }
        }
    }
    Ok(pending)
}

/// 改写一条指令。
fn rewrite_instruction(
    emit: &Emit<'_>,
    builder: &mut Builder,
    inst: &Inst,
    point: u32,
    pending: &mut Vec<PendingStore>,
) -> Result<(), AllocError> {
    let form = table::form(inst.form);
    let mut operands = Vec::with_capacity(inst.operands.len());
    let mut written: Vec<(u32, Loc)> = Vec::new();
    // 可重建值的定义站点整段丢弃：值没有机器位置，每个使用处会重新物化。
    if inst.operands.iter().enumerate().any(|(position, operand)| {
        !matches!(form.access[position], Access::Read)
            && matches!(operand, Operand::Reg(Reg::Virtual(id))
            if emit.values.get(*id as usize).is_some_and(|value| {
                value.placement == Placement::Rematerialize
            }))
    }) {
        return Ok(());
    }
    for (position, operand) in inst.operands.iter().enumerate() {
        let kind = form.operands[position];
        let access = form.access[position];
        match operand {
            Operand::Reg(Reg::Virtual(id)) => {
                let value = emit
                    .values
                    .get(*id as usize)
                    .ok_or_else(|| AllocError::new(format!("指令引用未登记的值 v{id}")))?;
                match value.placement {
                    Placement::Location(Location::Gpr(gpr)) => {
                        operands.push(Operand::Reg(Reg::Gpr(gpr)));
                    }
                    Placement::Location(Location::Xmm(xmm)) => {
                        operands.push(Operand::Reg(Reg::Xmm(xmm)));
                    }
                    Placement::Location(Location::Slot(slot)) => {
                        let offset = emit.frame.spill_offset(slot)?;
                        let writes_memory = matches!(access, Access::Write | Access::ReadWrite)
                            && matches!(kind, OperandKind::Rm64 | OperandKind::XmmRm);
                        let reads_memory = !matches!(access, Access::Write) && accepts_memory(kind);
                        if reads_memory || writes_memory {
                            operands.push(lower::mem_base(Reg::Gpr(Gpr::Rsp), disp(offset)?));
                        } else {
                            let scratch =
                                emit.choose_scratch(point, value.class, kind, &Clobbers::NONE)?;
                            emit.emit_load_place(builder, offset, scratch, value.class);
                            emit.count_reloads(1);
                            operands.push(Operand::Reg(reg_of(scratch)?));
                            if matches!(access, Access::Write | Access::ReadWrite) {
                                written.push((*id, scratch));
                            }
                        }
                    }
                    Placement::Rematerialize => {
                        let scratch =
                            emit.choose_scratch(point, value.class, kind, &Clobbers::NONE)?;
                        emit.rematerialize(builder, *id, scratch)?;
                        operands.push(Operand::Reg(reg_of(scratch)?));
                    }
                    Placement::Dead => {
                        return Err(AllocError::new(format!("指令引用已死的值 v{id}")));
                    }
                }
            }
            Operand::Mem(mem) => {
                operands.push(rewrite_memory(emit, builder, mem, point)?);
            }
            other => operands.push(other.clone()),
        }
    }
    // 同宽度的 `mov x, x`（含 XMM 自拷贝）没有效果：分配把源与目标落到同一位置时省掉它。
    if !is_self_move(form, &operands) {
        builder.push_inst(Inst {
            form: inst.form,
            operands,
            lock: inst.lock,
        });
    }
    for (value, scratch) in written {
        let Some(store) = pending.iter().find(|store| store.value == value) else {
            continue;
        };
        if store.last_write != point || !store.live_after {
            continue;
        }
        let Some(Location::Slot(slot)) = emit.values[value as usize].location() else {
            continue;
        };
        let offset = emit.frame.spill_offset(slot)?;
        emit.emit_store_place(builder, scratch, offset, emit.values[value as usize].class);
        emit.count_stores(1);
        pending.retain(|store| store.value != value);
    }
    Ok(())
}

/// 同宽度、纯写读的 `mov x, x`：两侧操作数形状相同且解析到同一寄存器。
fn is_self_move(form: &table::Form, operands: &[Operand]) -> bool {
    if operands.len() != 2 || form.operands.len() != 2 {
        return false;
    }
    if form.operands[0] != form.operands[1] || form.access != [Access::Write, Access::Read] {
        return false;
    }
    matches!((&operands[0], &operands[1]), (Operand::Reg(left), Operand::Reg(right)) if left == right)
}

/// 内存操作数：`StackAddr` 占位基址换成 frame 偏移，虚拟 base/index 换寄存器（必要时载入）。
fn rewrite_memory(
    emit: &Emit<'_>,
    builder: &mut Builder,
    mem: &Mem,
    point: u32,
) -> Result<Operand, AllocError> {
    let mut disp = mem.disp;
    let base = match mem.base {
        Some(Reg::Virtual(id)) if id >= FRAME_SLOT_BASE => {
            let slot = usize::try_from(id - FRAME_SLOT_BASE).expect("frame 槽下标适配 usize");
            let local = emit
                .frame
                .local(slot)
                .ok_or_else(|| AllocError::new(format!("frame 局部槽 {slot} 不存在")))?;
            disp = i32::try_from(
                i64::from(local.offset)
                    .checked_add(i64::from(mem.disp))
                    .ok_or_else(|| AllocError::new("局部槽位移溢出"))?,
            )
            .map_err(|_| AllocError::new("局部槽位移超出 i32"))?;
            Some(Reg::Gpr(Gpr::Rsp))
        }
        Some(Reg::Virtual(id)) => Some(value_register(emit, builder, id, point)?),
        Some(other) => Some(other),
        None => None,
    };
    let index = match mem.index {
        Some(Reg::Virtual(id)) if id >= FRAME_SLOT_BASE => {
            return Err(AllocError::new("frame 占位基址不能作为索引"));
        }
        Some(Reg::Virtual(id)) => Some(value_register(emit, builder, id, point)?),
        Some(other) => Some(other),
        None => None,
    };
    Ok(Operand::Mem(Mem {
        base,
        index,
        scale: mem.scale,
        disp,
    }))
}

/// 虚拟寄存器 → 物理寄存器；spilled 值先载入 scratch。
fn value_register(
    emit: &Emit<'_>,
    builder: &mut Builder,
    id: u32,
    point: u32,
) -> Result<Reg, AllocError> {
    let value = emit
        .values
        .get(id as usize)
        .ok_or_else(|| AllocError::new(format!("指令引用未登记的值 v{id}")))?;
    match value.placement {
        Placement::Location(Location::Gpr(gpr)) => Ok(Reg::Gpr(gpr)),
        Placement::Location(Location::Xmm(_)) => Err(AllocError::new("地址计算不能用 XMM 值")),
        Placement::Location(Location::Slot(slot)) => {
            let offset = emit.frame.spill_offset(slot)?;
            let scratch =
                emit.choose_scratch(point, RegClass::Gpr, OperandKind::Mem, &Clobbers::NONE)?;
            emit.emit_load_place(builder, offset, scratch, RegClass::Gpr);
            emit.count_reloads(1);
            reg_of(scratch)
        }
        Placement::Rematerialize => {
            let scratch =
                emit.choose_scratch(point, value.class, OperandKind::Mem, &Clobbers::NONE)?;
            emit.rematerialize(builder, id, scratch)?;
            reg_of(scratch)
        }
        Placement::Dead => Err(AllocError::new(format!("指令引用已死的值 v{id}"))),
    }
}

/// 发射重发单元。
fn emit_moves(
    emit: &Emit<'_>,
    builder: &mut Builder,
    moves: &[EmittedMove],
) -> Result<(), AllocError> {
    for mov in moves {
        match *mov {
            EmittedMove::Move { src, dst, ty } => {
                emit_loc_move(emit, builder, src, dst, ty)?;
            }
            EmittedMove::Rematerialize { value, dest } => {
                emit.rematerialize(builder, value, dest)?;
            }
        }
    }
    Ok(())
}

/// 一条 `Loc → Loc` 搬运。
fn emit_loc_move(
    emit: &Emit<'_>,
    builder: &mut Builder,
    src: Loc,
    dst: Loc,
    ty: Type,
) -> Result<(), AllocError> {
    if src == dst {
        return Ok(());
    }
    let float = matches!(ty, Type::F32 | Type::F64 | Type::V128(_));
    match (src, dst) {
        (Loc::Gpr(left), Loc::Gpr(right)) => {
            if float {
                return Err(AllocError::new(format!(
                    "整数寄存器搬运使用了浮点类型 {ty:?}：{left:?} → {right:?}"
                )));
            }
            lower::value_move(builder, Reg::Gpr(left), Reg::Gpr(right), ty).map_err(lowering_error)
        }
        (Loc::Xmm(left), Loc::Xmm(right)) => {
            if !float {
                return Err(AllocError::new(format!(
                    "XMM 寄存器搬运使用了整数类型 {ty:?}：{left:?} → {right:?}"
                )));
            }
            lower::value_move(builder, Reg::Xmm(left), Reg::Xmm(right), ty).map_err(lowering_error)
        }
        (Loc::Gpr(gpr), Loc::Slot(slot)) => {
            let offset = emit.frame.spill_offset(slot)?;
            emit.count_stores(1);
            lower::store_stack(builder, Reg::Gpr(gpr), offset, ty).map_err(lowering_error)
        }
        (Loc::Xmm(xmm), Loc::Slot(slot)) => {
            let offset = emit.frame.spill_offset(slot)?;
            emit.count_stores(1);
            lower::store_stack(builder, Reg::Xmm(xmm), offset, ty).map_err(lowering_error)
        }
        (Loc::Slot(slot), Loc::Gpr(gpr)) => {
            let offset = emit.frame.spill_offset(slot)?;
            emit.count_reloads(1);
            lower::load_stack(builder, offset, Reg::Gpr(gpr), ty).map_err(lowering_error)
        }
        (Loc::Slot(slot), Loc::Xmm(xmm)) => {
            let offset = emit.frame.spill_offset(slot)?;
            emit.count_reloads(1);
            lower::load_stack(builder, offset, Reg::Xmm(xmm), ty).map_err(lowering_error)
        }
        (Loc::Gpr(gpr), Loc::Scratch) => {
            let offset = emit.scratch_offset()?;
            lower::store_stack(builder, Reg::Gpr(gpr), offset, ty).map_err(lowering_error)
        }
        (Loc::Xmm(xmm), Loc::Scratch) => {
            let offset = emit.scratch_offset()?;
            lower::store_stack(builder, Reg::Xmm(xmm), offset, ty).map_err(lowering_error)
        }
        (Loc::Scratch, Loc::Gpr(gpr)) => {
            let offset = emit.scratch_offset()?;
            lower::load_stack(builder, offset, Reg::Gpr(gpr), ty).map_err(lowering_error)
        }
        (Loc::Scratch, Loc::Xmm(xmm)) => {
            let offset = emit.scratch_offset()?;
            lower::load_stack(builder, offset, Reg::Xmm(xmm), ty).map_err(lowering_error)
        }
        (Loc::Slot(_) | Loc::Scratch, Loc::Slot(_) | Loc::Scratch) => {
            // 内存到内存：经 `r11` 搬机器字（16 字节值搬两个字）。
            let bytes = if matches!(ty, Type::V128(_)) { 16 } else { 8 };
            let from = emit.loc_offset(src)?;
            let to = emit.loc_offset(dst)?;
            let words = bytes / 8;
            emit.count_reloads(words);
            emit.count_stores(words);
            let mut at = 0_u32;
            while at < bytes {
                lower::load_stack(builder, from + at, Reg::Gpr(Gpr::R11), Type::I64)
                    .map_err(lowering_error)?;
                lower::store_stack(builder, Reg::Gpr(Gpr::R11), to + at, Type::I64)
                    .map_err(lowering_error)?;
                at += 8;
            }
            Ok(())
        }
        (Loc::Gpr(_) | Loc::Xmm(_), Loc::Gpr(_) | Loc::Xmm(_)) => {
            Err(AllocError::new("寄存器搬运两侧 bank 不一致"))
        }
    }
}

impl Emit<'_> {
    /// 值的位置。
    pub(crate) fn value_location(&self, index: usize) -> Result<Loc, AllocError> {
        let value = self
            .values
            .get(index)
            .ok_or_else(|| AllocError::new("值编号越过分配结果"))?;
        value
            .location()
            .map(frame::loc_of)
            .ok_or_else(|| AllocError::new(format!("值 v{index} 没有机器位置")))
    }

    /// 发射重发单元（frame 阶段合成 prologue 时复用）。
    pub(crate) fn emit_moves(
        &self,
        builder: &mut Builder,
        moves: &[EmittedMove],
    ) -> Result<(), AllocError> {
        emit_moves(self, builder, moves)
    }

    /// 内存位置偏移。
    fn loc_offset(&self, loc: Loc) -> Result<u32, AllocError> {
        match loc {
            Loc::Slot(slot) => self.frame.spill_offset(slot),
            Loc::Scratch => self.scratch_offset(),
            Loc::Gpr(_) | Loc::Xmm(_) => Err(AllocError::new("寄存器位置没有内存偏移")),
        }
    }

    /// 记 `count` 条 spill 重载。
    fn count_reloads(&self, count: u32) {
        let traffic = self.traffic.get();
        self.traffic.set(SpillTraffic {
            reloads: traffic.reloads.saturating_add(count),
            ..traffic
        });
    }

    /// 记 `count` 条 spill 写回。
    fn count_stores(&self, count: u32) {
        let traffic = self.traffic.get();
        self.traffic.set(SpillTraffic {
            stores: traffic.stores.saturating_add(count),
            ..traffic
        });
    }

    /// 记一次常量重建。
    fn count_rematerialization(&self) {
        let traffic = self.traffic.get();
        self.traffic.set(SpillTraffic {
            rematerializations: traffic.rematerializations.saturating_add(1),
            ..traffic
        });
    }

    /// copy scratch 偏移。
    fn scratch_offset(&self) -> Result<u32, AllocError> {
        self.frame
            .scratch_offset()
            .ok_or_else(|| AllocError::new("并行拷贝环需要 copy scratch，但 frame 未预留"))
    }

    /// 按值装载：源是 spill slot 或 scratch。
    fn emit_load_place(&self, builder: &mut Builder, offset: u32, dest: Loc, class: RegClass) {
        match (class, dest) {
            (_, Loc::Gpr(gpr)) => {
                lower::load_stack(builder, offset, Reg::Gpr(gpr), Type::I64).expect("载入可编码");
            }
            (_, Loc::Xmm(xmm)) => {
                lower::load_stack(builder, offset, Reg::Xmm(xmm), Type::F64).expect("载入可编码");
            }
            (_, Loc::Slot(_) | Loc::Scratch) => {}
        }
    }

    /// 按值存储：目标是 spill slot 或 scratch。
    fn emit_store_place(&self, builder: &mut Builder, src: Loc, offset: u32, class: RegClass) {
        match (class, src) {
            (_, Loc::Gpr(gpr)) => {
                lower::store_stack(builder, Reg::Gpr(gpr), offset, Type::I64).expect("可编码");
            }
            (_, Loc::Xmm(xmm)) => {
                lower::store_stack(builder, Reg::Xmm(xmm), offset, Type::F64).expect("可编码");
            }
            (_, Loc::Slot(_) | Loc::Scratch) => {}
        }
    }

    /// 通用载入：把 `[rsp + offset]` 读进位置（目标不是寄存器时经 `r11`）。
    pub(crate) fn emit_load(
        &self,
        builder: &mut Builder,
        offset: u32,
        dest: Loc,
        ty: Type,
    ) -> Result<(), AllocError> {
        match dest {
            Loc::Gpr(gpr) => {
                lower::load_stack(builder, offset, Reg::Gpr(gpr), ty).map_err(lowering_error)
            }
            Loc::Xmm(xmm) => {
                lower::load_stack(builder, offset, Reg::Xmm(xmm), ty).map_err(lowering_error)
            }
            Loc::Slot(_) | Loc::Scratch => {
                let from = offset;
                let to = self.loc_offset(dest)?;
                let bytes = if matches!(ty, Type::V128(_)) { 16 } else { 8 };
                let mut at = 0_u32;
                while at < bytes {
                    lower::load_stack(builder, from + at, Reg::Gpr(Gpr::R11), Type::I64)
                        .map_err(lowering_error)?;
                    lower::store_stack(builder, Reg::Gpr(Gpr::R11), to + at, Type::I64)
                        .map_err(lowering_error)?;
                    at += 8;
                }
                Ok(())
            }
        }
    }

    /// 重建可重建的值。
    fn rematerialize(
        &self,
        builder: &mut Builder,
        value: u32,
        dest: Loc,
    ) -> Result<(), AllocError> {
        let definition = self
            .body
            .values
            .get(value as usize)
            .ok_or_else(|| AllocError::new("重建引用未登记的值"))?;
        let Definition::Instruction { instruction, .. } = definition.definition else {
            return Err(AllocError::new("只有指令定义的值可以重建"));
        };
        let site = u32::try_from(instruction.index()).unwrap_or(u32::MAX);
        let instruction = &self.body.instructions[instruction.index()];
        let op = instruction.op.clone();
        if !matches!(
            op,
            Op::IConst(_) | Op::FConst(_) | Op::SymbolAddr(_) | Op::StackAddr(_)
        ) {
            return Err(AllocError::new("只有常量/symbol/stack 地址可以重建"));
        }
        let operands: Vec<SiteValue> = self
            .body
            .args(&instruction.arguments)
            .iter()
            .map(|value| SiteValue {
                ty: self.body.values[value.index()].kind,
                reg: Reg::Virtual(value.0),
            })
            .collect();
        let results = vec![SiteValue {
            ty: definition.kind,
            reg: reg_of(dest)?,
        }];
        let ctx = LowerCtx {
            target: self.target,
            body: Some(self.body),
            universe: None,
            raw: None,
            site,
        };
        let lowered = lower::lower_with(&op, &operands, &results, &instruction.source, ctx)
            .map_err(|error| AllocError::new(format!("重建常量失败：{error}")))?;
        if !lowered.copy_groups.is_empty() {
            return Err(AllocError::new("重建序列不应包含并行拷贝组"));
        }
        // `StackAddr` 的重建序列同样带 frame 占位基址，必须当场换算成 `[rsp + offset]`。
        let mut sequence = lowered.sequence;
        for inst in &mut sequence.instructions {
            for operand in &mut inst.operands {
                if let Operand::Mem(mem) = operand
                    && let Some(Reg::Virtual(id)) = mem.base
                    && id >= FRAME_SLOT_BASE
                {
                    let slot =
                        usize::try_from(id - FRAME_SLOT_BASE).expect("frame 槽下标适配 usize");
                    let local = self
                        .frame
                        .local(slot)
                        .ok_or_else(|| AllocError::new(format!("frame 局部槽 {slot} 不存在")))?;
                    mem.base = Some(Reg::Gpr(Gpr::Rsp));
                    mem.disp = i32::try_from(
                        i64::from(local.offset)
                            .checked_add(i64::from(mem.disp))
                            .ok_or_else(|| AllocError::new("局部槽位移溢出"))?,
                    )
                    .map_err(|_| AllocError::new("局部槽位移超出 i32"))?;
                }
            }
        }
        // 重建只允许写 `r11`（lowering 的固定 scratch）与目标位置本身。
        let allowed = loc_register(dest);
        for inst in &sequence.instructions {
            let form = table::form(inst.form);
            for (position, operand) in inst.operands.iter().enumerate() {
                if !matches!(
                    form.access.get(position),
                    Some(Access::Write | Access::ReadWrite)
                ) {
                    continue;
                }
                if let Operand::Reg(register) = operand
                    && *register != Reg::Gpr(Gpr::R11)
                    && Some(*register) != allowed
                {
                    return Err(AllocError::new(format!(
                        "重建序列写入了非 scratch 寄存器 {register:?}"
                    )));
                }
            }
        }
        builder.extend(sequence);
        self.count_rematerialization();
        Ok(())
    }

    /// 该点位空闲且可用的 scratch。
    fn choose_scratch(
        &self,
        point: u32,
        class: RegClass,
        kind: OperandKind,
        forbidden: &Clobbers,
    ) -> Result<Loc, AllocError> {
        let byte_register = matches!(kind, OperandKind::Rm8 | OperandKind::R8);
        let occupied = self.occupied(point);
        let mask = self
            .points
            .get(point as usize)
            .map_or(Clobbers::NONE, |point| point.mask);
        match class {
            RegClass::Gpr => {
                let candidates = std::iter::once(Gpr::R11).chain(super::scan::GPR_POOL);
                candidates
                    .into_iter()
                    .find(|gpr| {
                        self.scratch_gpr.gpr & gpr.bit() != 0
                            && mask.gpr & gpr.bit() == 0
                            && occupied.gpr & gpr.bit() == 0
                            && forbidden.gpr & gpr.bit() == 0
                            && (!byte_register || gpr.is_byte_encodable())
                    })
                    .map(Loc::Gpr)
                    .ok_or_else(|| {
                        AllocError::new("指令需要 scratch 寄存器，但该点位没有空闲寄存器")
                    })
            }
            RegClass::Xmm => super::scan::XMM_POOL
                .into_iter()
                .find(|xmm| {
                    mask.xmm & xmm.bit() == 0
                        && occupied.xmm & xmm.bit() == 0
                        && forbidden.xmm & xmm.bit() == 0
                })
                .map(Loc::Xmm)
                .ok_or_else(|| AllocError::new("指令需要 XMM scratch，但该点位没有空闲寄存器")),
        }
    }

    /// 该点位被活跃值占用的寄存器。
    fn occupied(&self, point: u32) -> Clobbers {
        let mut occupied = Clobbers::NONE;
        for value in self.values {
            if !value.is_live() || !super::live::range_covers_point(value.range, point) {
                continue;
            }
            match value.location() {
                Some(Location::Gpr(gpr)) => occupied = occupied.union(Clobbers::gpr(gpr)),
                Some(Location::Xmm(xmm)) => occupied = occupied.union(Clobbers::xmm(xmm)),
                Some(Location::Slot(_)) | None => {}
            }
        }
        occupied
    }
}

/// 接受内存操作数的种类。
const fn accepts_memory(kind: OperandKind) -> bool {
    matches!(
        kind,
        OperandKind::Rm8
            | OperandKind::Rm16
            | OperandKind::Rm32
            | OperandKind::Rm64
            | OperandKind::XmmRm
    )
}

/// 位置 → 寄存器。
fn reg_of(loc: Loc) -> Result<Reg, AllocError> {
    loc.register()
        .ok_or_else(|| AllocError::new("该位置不是寄存器"))
}

/// 位置写到的寄存器；内存位置返回 `None`。
fn loc_register(loc: Loc) -> Option<Reg> {
    loc.register()
}

/// frame 偏移的 i32 位移。
fn disp(offset: u32) -> Result<i32, AllocError> {
    i32::try_from(offset).map_err(|_| AllocError::new("frame 偏移超出 i32"))
}

/// lowering 失败 → 分配失败。
fn lowering_error(error: LoweringError) -> AllocError {
    AllocError::new(format!("序列改写失败：{error}"))
}

/// 站点内区间 → 函数序列内的绝对区间。
///
/// 块标签不占指令槽，指令游标从 0 起；顺序与 [`select::stitch_blocks`] 完全一致。
fn absolute_regions(blocks: &[SelectedBlock], regions: &[(u32, Range<u32>)]) -> Vec<Range<u32>> {
    let mut absolute = Vec::new();
    let mut site_index = 0_u32;
    let mut cursor = 0_u32;
    for block in blocks {
        for site in &block.sites {
            let count = u32::try_from(site.lowered.sequence.instructions.len())
                .expect("站点指令数适配 u32");
            for (index, range) in regions {
                if *index == site_index {
                    absolute.push(cursor + range.start..cursor + range.end);
                }
            }
            cursor += count;
            site_index += 1;
        }
        let count = u32::try_from(block.terminator.sequence.instructions.len())
            .expect("终结符指令数适配 u32");
        for (index, range) in regions {
            if *index == site_index {
                absolute.push(cursor + range.start..cursor + range.end);
            }
        }
        cursor += count;
        site_index += 1;
    }
    absolute
}
