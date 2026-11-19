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
//! scratch 取自该点位「未被活跃值占用、不在点位 mask 内、不是本组源/目标、也不是本指令
//! 已经挑走」的偏好序首个寄存器（GPR 先试 `r11`）；空闲寄存器不足按内部不变量失败，
//! 绝不产出错误机器码。同一条指令里的多个 scratch 互斥，指令结束即释放。

use std::cell::Cell;
use std::ops::Range;

use super::super::inst::{Inst, LabelDefinition, LabelId, Mem, Operand, Scale, Sequence};
use super::super::layout;
use super::super::lower::{self, Builder, LowerCtx, LoweringError, SiteValue};
use super::super::reg::{Clobbers, FRAME_SLOT_BASE, Gpr, Reg, Xmm};
use super::super::select::{self, SelectedBlock, SelectedFunction, SiteKind};
use super::super::table::{self, Access, OperandKind};
use super::super::verify;
use super::frame::{self, FrameLayout};
use super::live::{self, LiveInfo};
use super::parallel::{EmittedMove, Loc, PREPEND_SITE, Resolution};
use super::{AllocError, Location, Placement, RegClass, ValueAllocation};
use crate::lir::body::{Body, Definition, Op, Type};
use crate::target::TargetName;

mod emit;
use emit::emit_moves;

/// 改写阶段累计的溢出流量统计。
///
/// `Emit` 全程按共享引用传递，因此用 `Cell` 就地累加：计数量只用于 payload 统计，
/// 不影响发射出的机器码。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpillTraffic {
    /// 从 spill slot 读回寄存器的指令数。
    pub reloads: u32,
    /// 写回 spill slot 的指令数。
    pub stores: u32,
    /// 重建常量/symbol/栈地址的次数。
    pub rematerializations: u32,
}

/// 改写产物。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rewritten {
    pub blocks: Vec<SelectedBlock>,
    pub sequence: Sequence,
    pub rel8_count: u32,
    /// prologue 与各 epilogue 的函数序列区间；只有这里允许写 `rsp`。
    pub abi_regions: Vec<Range<u32>>,
    /// 溢出流量统计。
    pub traffic: SpillTraffic,
}

/// 改写上下文。
pub struct Emit<'a> {
    pub body: &'a Body,
    pub frame: &'a FrameLayout,
    pub values: &'a [ValueAllocation],
    pub points: &'a [live::Point],
    pub target: TargetName,
    /// 可作 scratch 的 GPR：`r11` 与通用池减去未保存的 callee-saved。
    pub scratch_gpr: Clobbers,
    /// 溢出流量累加器。
    traffic: Cell<SpillTraffic>,
}

/// 改写一个函数的全部站点并重拼序列。
#[expect(
    clippy::too_many_arguments,
    reason = "改写需要分配结果、frame、解析结果与入口契约四类输入"
)]
pub fn rewrite(
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
    let entry_order = blocks
        .iter()
        .position(|block| block.id == body.entry)
        .ok_or_else(|| AllocError::new("入口块不在布局里"))?;
    let synthetic_check = frame.checked
        && !blocks.iter().any(|block| {
            block
                .sites
                .iter()
                .any(|site| site.kind == SiteKind::Prologue)
        });
    // 没有 `StackCheck` 站点时，先在标签空间里让出两个号给合成 prologue。
    let prologue_at = if synthetic_check {
        Some(reserve_prologue_labels(&mut blocks, entry_order)?)
    } else {
        None
    };
    let entry_has_sites = !blocks[entry_order].sites.is_empty();
    let mut regions: Vec<(u32, Range<u32>)> = Vec::new();
    let mut init_events: Vec<Vec<(u32, u32, u32, bool)>> = vec![Vec::new(); body.blocks.len()];
    for root in &body.root_inits {
        if let Some(block) = init_events.get_mut(root.block.index()) {
            block.push((root.position, root.slot.0, root.bit, root.initialized));
        }
    }
    for block in &mut init_events {
        block.sort_by_key(|(position, slot, bit, _)| (*position, *slot, *bit));
    }
    let mut site_index = 0_u32;
    for (order, block) in blocks.iter_mut().enumerate() {
        let site_count = block.sites.len();
        let terminator = terminator_kind(body, block);
        let mut init_cursor = 0_usize;
        for index in 0..site_count {
            let points = live
                .sites
                .get(site_index as usize)
                .ok_or_else(|| AllocError::new("点位表缺少站点"))?;
            let kind = block.sites[index].kind;
            let prepend = resolution
                .prologue()
                .is_some_and(|group| group.site == PREPEND_SITE)
                && order == entry_order
                && index == 0;
            let position = block.sites[index].instruction;
            let sequence = &mut block.sites[index].lowered;
            let stores = drain_init_stores(
                &init_events[block.id.index()],
                &frame.init_bases,
                &mut init_cursor,
                position,
            );
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
                0,
                &stores,
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
            && order == entry_order
            && site_count == 0;
        let label_base = if prepend && !entry_has_sites {
            prologue_at.unwrap_or(0)
        } else {
            0
        };
        let sequence = &mut block.terminator;
        let stores = drain_init_stores(
            &init_events[block.id.index()],
            &frame.init_bases,
            &mut init_cursor,
            u32::MAX,
        );
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
            label_base,
            &stores,
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

/// 为没有 `StackCheck` 站点的有帧函数让出两个 prologue 标签。
///
/// 编码器要求标签从 0 起连续。站点局部标签会在 `stitch` 时平移，所以有站点时把入口块
/// 首个站点的局部标签让出 0/1，并把其后的绝对标签整体加 2。入口块没有站点时，prologue
/// 直接占用绝对编号 `at`/`at+1`，终结符原有标签后移，`terminator_label_base` 保持不动
/// （`stitch` 不把终结符标签算进站点用量）。
fn reserve_prologue_labels(blocks: &mut [SelectedBlock], entry: usize) -> Result<u32, AllocError> {
    const EXTRA: u32 = 2;
    let block_base = blocks.iter().map(|block| block.id.0 + 1).max().unwrap_or(0);
    let at = if entry == 0 {
        block_base
    } else {
        blocks[entry - 1].terminator_label_end
    };
    let entry_has_sites = !blocks[entry].sites.is_empty();
    if entry_has_sites {
        shift_local_labels(&mut blocks[entry].sites[0].lowered.sequence, EXTRA)?;
    }
    for (index, block) in blocks.iter_mut().enumerate() {
        if index < entry {
            continue;
        }
        bump_absolute_labels(&mut block.terminator.sequence, at, EXTRA)?;
        if entry_has_sites || index != entry {
            block.terminator_label_base = block
                .terminator_label_base
                .checked_add(EXTRA)
                .ok_or_else(|| AllocError::new("标签编号溢出"))?;
        }
        block.terminator_label_end = block
            .terminator_label_end
            .checked_add(EXTRA)
            .ok_or_else(|| AllocError::new("标签编号溢出"))?;
    }
    Ok(at)
}

/// 站点局部标签整体后移，给合成 prologue 留出 0/1。
fn shift_local_labels(sequence: &mut Sequence, extra: u32) -> Result<(), AllocError> {
    for instruction in &mut sequence.instructions {
        for operand in &mut instruction.operands {
            if let Operand::Label(label) = operand {
                label.0 = label
                    .0
                    .checked_add(extra)
                    .ok_or_else(|| AllocError::new("标签编号溢出"))?;
            }
        }
    }
    for definition in &mut sequence.labels {
        definition.label.0 = definition
            .label
            .0
            .checked_add(extra)
            .ok_or_else(|| AllocError::new("标签编号溢出"))?;
    }
    Ok(())
}

/// 绝对标签编号 `>= at` 的定义和引用后移。块标签编号更小，保持不动。
fn bump_absolute_labels(sequence: &mut Sequence, at: u32, extra: u32) -> Result<(), AllocError> {
    for instruction in &mut sequence.instructions {
        for operand in &mut instruction.operands {
            if let Operand::Label(label) = operand
                && label.0 >= at
            {
                label.0 = label
                    .0
                    .checked_add(extra)
                    .ok_or_else(|| AllocError::new("标签编号溢出"))?;
            }
        }
    }
    for definition in &mut sequence.labels {
        if definition.label.0 >= at {
            definition.label.0 = definition
                .label
                .0
                .checked_add(extra)
                .ok_or_else(|| AllocError::new("标签编号溢出"))?;
        }
    }
    Ok(())
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

/// 取出截至 `upto` 位置的全部逐根初始化事件，翻译成帧内字节写入。
///
/// `events` 按块内位置排序，`cursor` 在块内单调前进；返回 `(帧内偏移, 置位)`。没有根字的槽
/// 被跳过（verifier 已保证事件只指向带根字的槽，这里跳过只是防御性省略）。
fn drain_init_stores(
    events: &[(u32, u32, u32, bool)],
    bases: &[Option<u32>],
    cursor: &mut usize,
    upto: u32,
) -> Vec<(u32, bool)> {
    let mut stores = Vec::new();
    for &(_, slot, bit, initialized) in crate::metadata::init_events_until(events, cursor, upto) {
        if let Some(base) = bases.get(slot as usize).copied().flatten() {
            stores.push((base + bit, initialized));
        }
    }
    stores
}

/// 发射逐根初始化字节的写入：`mov byte [rsp + offset], 1/0`。
///
/// 每个根字独占一个字节，所以置位与复位都不需要读改写，也不占用 scratch 寄存器；调用点保证
/// 这些写入落在下一次读取该位图的安全点之前。
fn emit_init_stores(builder: &mut Builder, stores: &[(u32, bool)]) {
    for (offset, initialized) in stores {
        let disp = i32::try_from(*offset).expect("帧内偏移适配 i32");
        builder.emit(
            "mov",
            &[OperandKind::Rm8, OperandKind::Imm8],
            Access::Write,
            vec![
                Operand::Mem(Mem {
                    base: Some(Reg::Gpr(Gpr::Rsp)),
                    index: None,
                    scale: Scale::One,
                    disp,
                }),
                Operand::Imm(u64::from(*initialized)),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::drain_init_stores;

    /// 逐根初始化写入按位置单调取出：`upto` 之后的事件留到下一次，无根字的槽不产生写入。
    #[test]
    fn drain_stops_at_position_and_skips_root_free_slots() {
        let events = [(3, 0, 0, false), (5, 1, 1, true), (7, 2, 0, true)];
        let bases = [Some(16), Some(20), None];
        let mut cursor = 0;
        assert_eq!(
            drain_init_stores(&events, &bases, &mut cursor, 4),
            [(16, false)],
            "只取出位置不超过 upto 的事件"
        );
        assert_eq!(
            drain_init_stores(&events, &bases, &mut cursor, u32::MAX),
            [(21, true)],
            "无根字的槽不产生写入"
        );
        assert_eq!(cursor, events.len(), "游标走到末尾");
    }
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
    label_base: u32,
    init_stores: &[(u32, bool)],
) -> Result<Option<Range<u32>>, AllocError> {
    let entries = live::site_plan(lowered)?;
    let original_labels = lowered.sequence.labels.clone();
    let mut builder = Builder::with_labels(label_base);
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
        // 逐根初始化写入必须落在 prologue 之后：`[rsp + offset]` 这时才指向本帧。
        emit_init_stores(&mut builder, init_stores);
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
    // 站点代码之前刷新逐根初始化写入：该站点若是安全点，位图必须已经反映此前的事件。
    emit_init_stores(&mut builder, init_stores);
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
                rewrite_instruction(emit, &mut builder, &inst, point, &mut pending).map_err(
                    |error| AllocError::new(format!("点位 {point} 的 {inst:?}：{error}")),
                )?;
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

/// 站点内寄存器写入后仍需补整宽存储的登记项。
struct PendingStore {
    value: u32,
    /// 最后一次写入的点位。
    last_write: u32,
    /// 值在该点位之后仍活跃。
    live_after: bool,
}

/// 扫描站点，登记不能直接写入 spill slot 的值与最后一次写入点位。
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
            let point_index = points.start + u32::try_from(offset).expect("点位偏移适配 u32");
            let def_slot = point_index * 2 + 1;
            let live_after = value.range.1 > def_slot;
            match pending.iter_mut().find(|store| store.value == *id) {
                Some(store) => {
                    store.last_write = point_index;
                    store.live_after = live_after;
                }
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
    let mut reserved = Clobbers::NONE;
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
                            // 全宽纯写在地址/源操作数读取之后发生，可复用其临时寄存器；
                            // 已选的其它输出仍须独占。窄写与 ReadWrite 不能这样复用。
                            let full_write = access == Access::Write && kind == OperandKind::R64;
                            let mut forbidden = if full_write { Clobbers::NONE } else { reserved };
                            if full_write {
                                for (_, output) in &written {
                                    reserve_scratch(&mut forbidden, *output);
                                }
                            }
                            let scratch =
                                emit.choose_scratch(point, value.class, kind, &forbidden)?;
                            reserve_scratch(&mut reserved, scratch);
                            if !full_write {
                                emit.emit_load_place(builder, offset, scratch, value.class);
                                emit.count_reloads(1);
                            }
                            operands.push(Operand::Reg(reg_of(scratch)?));
                            if matches!(access, Access::Write | Access::ReadWrite) {
                                written.push((*id, scratch));
                            }
                        }
                    }
                    Placement::Rematerialize => {
                        let scratch = emit.choose_scratch(point, value.class, kind, &reserved)?;
                        reserve_scratch(&mut reserved, scratch);
                        emit.rematerialize(builder, *id, scratch)?;
                        operands.push(Operand::Reg(reg_of(scratch)?));
                    }
                    Placement::Dead => {
                        return Err(AllocError::new(format!("指令引用已死的值 v{id}")));
                    }
                }
            }
            Operand::Mem(mem) => {
                operands.push(rewrite_memory(emit, builder, mem, point, &mut reserved)?);
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
/// 内存操作数：可重物化的栈地址与索引常量直接折叠，避免在高寄存器压力点额外抢 scratch。
fn rewrite_memory(
    emit: &Emit<'_>,
    builder: &mut Builder,
    mem: &Mem,
    point: u32,
    reserved: &mut Clobbers,
) -> Result<Operand, AllocError> {
    let mut disp = i64::from(mem.disp);
    let base = match mem.base {
        Some(Reg::Virtual(id)) if id >= super::super::reg::FRAME_INIT_BASE => {
            let slot = (id - super::super::reg::FRAME_INIT_BASE) as usize;
            let offset = emit
                .frame
                .init_bases
                .get(slot)
                .copied()
                .flatten()
                .ok_or_else(|| AllocError::new("根初始化地址缺少帧内字节"))?;
            disp = disp
                .checked_add(i64::from(offset))
                .ok_or_else(|| AllocError::new("根位移溢出"))?;
            Some(Reg::Gpr(Gpr::Rsp))
        }
        Some(Reg::Virtual(id)) if id >= FRAME_SLOT_BASE => {
            let slot = usize::try_from(id - FRAME_SLOT_BASE).expect("frame 槽下标适配 usize");
            let local = emit
                .frame
                .local(slot)
                .ok_or_else(|| AllocError::new(format!("frame 局部槽 {slot} 不存在")))?;
            disp = disp
                .checked_add(i64::from(local.offset))
                .ok_or_else(|| AllocError::new("局部槽位移溢出"))?;
            Some(Reg::Gpr(Gpr::Rsp))
        }
        Some(Reg::Virtual(id)) => match rematerialized_op(emit, id) {
            Some(Op::StackAddr(slot)) => {
                let local = emit
                    .frame
                    .local(slot.index())
                    .ok_or_else(|| AllocError::new("重建栈地址引用未登记的局部槽"))?;
                disp = disp
                    .checked_add(i64::from(local.offset))
                    .ok_or_else(|| AllocError::new("栈地址位移溢出"))?;
                Some(Reg::Gpr(Gpr::Rsp))
            }
            _ => Some(value_register(emit, builder, id, point, reserved)?),
        },
        Some(other) => Some(other),
        None => None,
    };
    let index = match mem.index {
        Some(Reg::Virtual(id)) if id >= FRAME_SLOT_BASE => {
            return Err(AllocError::new("frame 占位基址不能作为索引"));
        }
        Some(Reg::Virtual(id)) => match rematerialized_op(emit, id) {
            Some(Op::IConst(value)) => {
                let scaled = i128::from(*value) << mem.scale.shift();
                let displacement = i128::from(disp) + scaled;
                disp = i64::try_from(displacement)
                    .map_err(|_| AllocError::new("索引常量折叠后位移溢出"))?;
                None
            }
            _ => Some(value_register(emit, builder, id, point, reserved)?),
        },
        Some(other) => Some(other),
        None => None,
    };
    let disp = i32::try_from(disp).map_err(|_| AllocError::new("内存操作数位移超出 i32"))?;
    Ok(Operand::Mem(Mem {
        base,
        index,
        scale: mem.scale,
        disp,
    }))
}

/// 返回可由使用点重物化的无副作用定义。
fn rematerialized_op<'a>(emit: &'a Emit<'_>, value: u32) -> Option<&'a Op> {
    if emit.values.get(value as usize)?.placement != Placement::Rematerialize {
        return None;
    }
    let Definition::Instruction { instruction, .. } =
        emit.body.values.get(value as usize)?.definition
    else {
        return None;
    };
    Some(&emit.body.instructions[instruction.index()].op)
}

/// 虚拟寄存器 → 物理寄存器；spilled 值先载入 scratch。
fn value_register(
    emit: &Emit<'_>,
    builder: &mut Builder,
    id: u32,
    point: u32,
    reserved: &mut Clobbers,
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
            let scratch = emit.choose_scratch(point, RegClass::Gpr, OperandKind::Mem, reserved)?;
            reserve_scratch(reserved, scratch);
            emit.emit_load_place(builder, offset, scratch, RegClass::Gpr);
            emit.count_reloads(1);
            reg_of(scratch)
        }
        Placement::Rematerialize => {
            let scratch = emit.choose_scratch(point, value.class, OperandKind::Mem, reserved)?;
            reserve_scratch(reserved, scratch);
            emit.rematerialize(builder, id, scratch)?;
            reg_of(scratch)
        }
        Placement::Dead => Err(AllocError::new(format!("指令引用已死的值 v{id}"))),
    }
}

/// 本指令已经挑走的 scratch，后续操作数不能再拿。
fn reserve_scratch(reserved: &mut Clobbers, scratch: Loc) {
    *reserved = reserved.union(match scratch {
        Loc::Gpr(gpr) => Clobbers::gpr(gpr),
        Loc::Xmm(xmm) => Clobbers::xmm(xmm),
        Loc::Slot(_) | Loc::Scratch => Clobbers::NONE,
    });
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

#[cfg(test)]
mod spill_tests;
