//! 分配与编码之后的栈图、展开记录与源码记录。
//!
//! 本模块只消费 [`super::alloc::Allocated`] 的 frame、点位和值位置，以及编码后的
//! 指令边界。它不重新选择寄存器，也不另建一套根种类。逻辑代码节的 `code_rva` 由
//! 世界装配按符号序分配；这里的偏移都相对函数字节起点。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::frontend::gir::body::CallKind;
use crate::frontend::hir;
use crate::lir::body::{
    Body, Call, CallTarget, Op, Provenance, RuntimeCall, SafepointKind, Terminator,
};
use crate::source::SourceMap;
use crate::target::TargetName;

use super::abi::{AbiSlot, AbiValue};
use super::alloc::{Allocated, Location, Point, PointKind};
use super::inst::{Assembled, Inst, Operand, RelocTarget, Sequence};
use super::mangle;
use super::reg::Gpr;
use super::table;

/// 栈图、展开表与源码记录共用的 schema。
pub(crate) const METADATA_SCHEMA: u32 = 1;
/// `CallReturn` 允许的寄存器根：`rbp`、`r12`、`r13`。
pub(super) const CALLEE_SAVED_ROOTS: u16 = (1 << 6) | (1 << 11) | (1 << 12);

/// 统一展开表魔数。
pub(crate) const UNWIND_MAGIC: &[u8; 8] = b"GUGUUN01";
/// 源码记录表魔数。
pub(crate) const SOURCE_MAGIC: &[u8; 8] = b"GUGUSR01";
/// 统一展开表版本。
pub(crate) const UNWIND_VERSION: u16 = 1;
/// 源码记录表版本。
pub(crate) const SOURCE_VERSION: u16 = 1;
/// Linux 栈图节名。
pub(crate) const STACKMAP_LINUX: &str = ".gugu.stackmap";
/// Windows 栈图节名。
pub(crate) const STACKMAP_WINDOWS: &str = ".gugustk";
/// 统一展开表的逻辑节名；镜像写出再投影到平台表。
pub(crate) const UNWIND_SECTION: &str = ".gugu.unwind";
/// 源码记录所属的元数据节；strip 必须保留。
pub(crate) const SOURCE_SECTION: &str = ".gugu.meta";

/// 只恢复传播、没有 cleanup 动作的链标记。
pub(super) const PROPAGATE_ONLY: u32 = u32::MAX;

/// 元数据生成失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MetadataError {
    message: String,
}

impl MetadataError {
    /// 用固定文本创建失败。
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// 一个安全点的物理根图。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct SafepointPayload {
    /// 相对函数起点的字节偏移。
    pub(crate) pc_offset: u32,
    /// 规范 kind：0..4。
    pub(crate) kind: u8,
    /// 允许栈复制。
    pub(crate) copy_allowed: bool,
    /// 允许 GC 扫描。
    pub(crate) scan_allowed: bool,
    /// dirty bridge。
    pub(crate) dirty: bool,
    /// 帧内 8 字节槽数；`MorestackEntry` 为 0。
    pub(crate) slot_count: u32,
    /// 五类寄存器掩码。
    pub(crate) registers: [u16; 5],
    /// 五类槽位图，每类若干 64 位 lane。
    pub(crate) slots: Vec<Vec<u64>>,
}

/// 一条 panic landing。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct LandingPayload {
    /// 调用指令起点。
    pub(crate) pc_start: u32,
    /// 调用指令终点，半开。
    pub(crate) pc_end: u32,
    /// cleanup 块的第一条指令。
    pub(crate) landing_pc: u32,
    /// cleanup 块编号；`u32::MAX` 表示只恢复传播。
    pub(crate) cleanup_chain: u32,
}

/// 一条函数内源码记录。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct SourcePayload {
    /// 半开字节范围起点。
    pub(crate) pc_start: u32,
    /// 半开字节范围终点。
    pub(crate) pc_end: u32,
    /// package 相对逻辑路径。
    pub(crate) path: String,
    /// 从 1 起的行号。
    pub(crate) line: u32,
    /// 从 1 起的列号。
    pub(crate) column: u32,
    /// bit0 panic 现场，bit1 合成位置。
    pub(crate) flags: u32,
}

/// 一个函数在编码完成后的元数据。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct FunctionMetadata {
    /// schema 版本。
    pub(crate) schema: u32,
    /// 是否进入栈图表。零帧且无安全点、无 landing 的叶函数为假。
    pub(crate) included: bool,
    /// 帧字节数。
    pub(crate) frame_size: u32,
    /// 函数字节数。
    pub(crate) code_size: u32,
    /// 已保存 callee-saved GPR 的栈图位掩码。
    pub(crate) saved_gpr_mask: u16,
    /// 函数本身是 runtime bridge。
    pub(crate) runtime_bridge: bool,
    /// 存在 panic landing。
    pub(crate) panic_landing: bool,
    /// 任一安全点含栈内指针。
    pub(crate) has_stack_interior: bool,
    /// 指令起点，供恢复后核对安全点落在边界上。
    pub(crate) boundaries: Vec<u32>,
    /// 按 `pc_offset` 严格递增。
    pub(crate) safepoints: Vec<SafepointPayload>,
    /// 按 `pc_start` 严格递增且不重叠。
    pub(crate) landings: Vec<LandingPayload>,
    /// 站点源码范围。
    pub(crate) sources: Vec<SourcePayload>,
}

/// 生成一个函数的元数据。
pub(crate) fn build(
    body: &Body,
    allocated: &Allocated,
    assembled: &Assembled,
    target: TargetName,
    sources: &SourceMap,
    hir_sources: &[hir::Source],
) -> Result<FunctionMetadata, MetadataError> {
    let spans = site_spans(allocated, assembled)?;
    let block_starts = block_starts(body, &spans);
    let regions = region_membership(body);
    let mut safepoints = Vec::new();
    let mut landings = Vec::new();
    let mut records = Vec::new();
    for span in &spans {
        records.push(records::source_of(body, span, sources, hir_sources));
        if let Some(landing) = records::landing_of(body, allocated, assembled, span, &block_starts)?
        {
            landings.push(landing);
        }
        if let Some(point) = safepoint_of(body, allocated, assembled, span, target, &regions)? {
            safepoints.push(point);
        }
    }
    finish(allocated, assembled, safepoints, landings, records)
}

/// 目标对应的栈图节名。
pub(crate) fn stackmap_section(target: TargetName) -> &'static str {
    match target {
        TargetName::X86_64Linux => STACKMAP_LINUX,
        TargetName::X86_64Windows => STACKMAP_WINDOWS,
    }
}

/// strip 必须保留的运行时元数据节。
pub(crate) fn retained_sections(target: TargetName) -> [&'static str; 3] {
    [stackmap_section(target), UNWIND_SECTION, SOURCE_SECTION]
}

/// 删除列表没有碰到运行时必需节时返回真。
pub(crate) fn strip_preserves(target: TargetName, removed: &[&str]) -> bool {
    retained_sections(target)
        .into_iter()
        .all(|name| !removed.contains(&name))
}

/// 核对函数内安全点、landing 与源码范围。
pub(crate) fn verify(metadata: &FunctionMetadata) -> Result<(), MetadataError> {
    if metadata.schema != METADATA_SCHEMA {
        return Err(MetadataError::new("元数据 schema 不一致"));
    }
    if metadata.code_size > 0 && metadata.boundaries.first() != Some(&0) {
        return Err(MetadataError::new("指令边界没有从 0 开始"));
    }
    records::check_safepoints(metadata)?;
    records::check_landings(metadata)?;
    records::check_sources(metadata)?;
    let interior = metadata.safepoints.iter().any(has_stack_interior);
    if metadata.has_stack_interior != interior {
        return Err(MetadataError::new("栈内指针标志与根图不一致"));
    }
    if metadata.panic_landing != !metadata.landings.is_empty() {
        return Err(MetadataError::new("landing 标志与记录数不一致"));
    }
    let leaf =
        metadata.safepoints.is_empty() && metadata.landings.is_empty() && metadata.frame_size == 0;
    if metadata.included == leaf {
        return Err(MetadataError::new("函数是否进入栈图表与叶规则不一致"));
    }
    Ok(())
}

pub(super) struct SiteSpan {
    pub(super) block: u32,
    pub(super) instruction: u32,
    pub(super) terminator: bool,
    pub(super) bytes: (u32, u32),
    pub(super) insts: (u32, u32),
    pub(super) ordinal: u32,
}

fn site_spans(
    allocated: &Allocated,
    assembled: &Assembled,
) -> Result<Vec<SiteSpan>, MetadataError> {
    let mut spans = Vec::new();
    let mut cursor = 0_u32;
    let mut ordinal = 0_u32;
    for block in &allocated.blocks {
        for site in &block.sites {
            spans.push(span_of(
                assembled,
                block.id.0,
                site.instruction,
                false,
                &site.lowered.sequence,
                &mut cursor,
                ordinal,
            )?);
            ordinal += 1;
        }
        spans.push(span_of(
            assembled,
            block.id.0,
            0,
            true,
            &block.terminator.sequence,
            &mut cursor,
            ordinal,
        )?);
        ordinal += 1;
    }
    if cursor as usize != assembled.instruction_offsets.len() {
        return Err(MetadataError::new("站点指令数与编码序列不一致"));
    }
    Ok(spans)
}

fn span_of(
    assembled: &Assembled,
    block: u32,
    instruction: u32,
    terminator: bool,
    sequence: &Sequence,
    cursor: &mut u32,
    ordinal: u32,
) -> Result<SiteSpan, MetadataError> {
    let count = u32::try_from(sequence.instructions.len()).expect("站点指令数适配 u32");
    let start = *cursor;
    let end = start
        .checked_add(count)
        .ok_or_else(|| MetadataError::new("站点指令范围溢出"))?;
    *cursor = end;
    let bytes_start = offset_at(assembled, start)?;
    let bytes_end = offset_at(assembled, end)?;
    Ok(SiteSpan {
        block,
        instruction,
        terminator,
        bytes: (bytes_start, bytes_end),
        insts: (start, end),
        ordinal,
    })
}

pub(super) fn offset_at(assembled: &Assembled, index: u32) -> Result<u32, MetadataError> {
    if index as usize == assembled.instruction_offsets.len() {
        return Ok(u32::try_from(assembled.bytes.len()).expect("代码长度适配 u32"));
    }
    assembled
        .instruction_offsets
        .get(index as usize)
        .map(|(_, offset)| *offset)
        .ok_or_else(|| MetadataError::new("指令下标越过编码序列"))
}

fn block_starts(body: &Body, spans: &[SiteSpan]) -> Vec<Option<u32>> {
    let mut starts = vec![None; body.blocks.len()];
    for span in spans {
        let index = span.block as usize;
        if starts.get(index).is_some_and(Option::is_none) && span.bytes.0 < span.bytes.1 {
            starts[index] = Some(span.bytes.0);
        }
    }
    starts
}

fn region_membership(body: &Body) -> Vec<bool> {
    let mut members = vec![false; body.instructions.len()];
    let mut depth = 0_u32;
    for (index, instruction) in body.instructions.iter().enumerate() {
        members[index] = depth > 0;
        match &instruction.op {
            Op::NoSafepointBegin(_) => depth = depth.saturating_add(1),
            Op::NoSafepointEnd(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    members
}

fn finish(
    allocated: &Allocated,
    assembled: &Assembled,
    mut safepoints: Vec<SafepointPayload>,
    mut landings: Vec<LandingPayload>,
    mut records: Vec<SourcePayload>,
) -> Result<FunctionMetadata, MetadataError> {
    safepoints.sort_by_key(|point| point.pc_offset);
    landings.sort_by_key(|landing| (landing.pc_start, landing.pc_end));
    records.retain(|record| record.pc_start < record.pc_end);
    records.sort_by(|left, right| {
        (left.pc_start, left.pc_end, &left.path).cmp(&(right.pc_start, right.pc_end, &right.path))
    });
    let code_size = u32::try_from(assembled.bytes.len()).expect("代码长度适配 u32");
    let included = !safepoints.is_empty() || !landings.is_empty() || allocated.frame.frame_size > 0;
    if !included {
        records.clear();
    }
    let metadata = FunctionMetadata {
        schema: METADATA_SCHEMA,
        included,
        frame_size: allocated.frame.frame_size,
        code_size,
        saved_gpr_mask: saved_mask(allocated),
        runtime_bridge: false,
        panic_landing: !landings.is_empty(),
        has_stack_interior: safepoints.iter().any(has_stack_interior),
        boundaries: assembled
            .instruction_offsets
            .iter()
            .map(|(_, offset)| *offset)
            .collect(),
        safepoints,
        landings,
        sources: records,
    };
    verify(&metadata)?;
    Ok(metadata)
}

fn saved_mask(allocated: &Allocated) -> u16 {
    let mut mask = 0_u16;
    for gpr in &allocated.frame.saved {
        if let Some(bit) = stackmap_bit(*gpr) {
            mask |= 1 << bit;
        }
    }
    mask
}

fn has_stack_interior(point: &SafepointPayload) -> bool {
    point.registers[4] != 0
        || point
            .slots
            .get(4)
            .is_some_and(|lane| lane.iter().any(|word| *word != 0))
}

fn safepoint_of(
    body: &Body,
    allocated: &Allocated,
    assembled: &Assembled,
    span: &SiteSpan,
    target: TargetName,
    regions: &[bool],
) -> Result<Option<SafepointPayload>, MetadataError> {
    if span.terminator {
        return terminator_safepoint(body, allocated, assembled, span, target);
    }
    let index = span.instruction as usize;
    let instruction = body
        .instructions
        .get(index)
        .ok_or_else(|| MetadataError::new("站点指令越界"))?;
    if matches!(instruction.op, Op::StackCheck) {
        return morestack(body, allocated, assembled, span);
    }
    if regions.get(index).copied().unwrap_or(false) {
        if instruction.safepoint.is_some() {
            return Err(MetadataError::new("无安全点区域内存在安全点"));
        }
        return Ok(None);
    }
    let Some(point) = instruction.safepoint else {
        return Ok(None);
    };
    let kind = body.safepoints[point.index()].kind;
    instruction_safepoint(
        body,
        allocated,
        assembled,
        span,
        target,
        &instruction.op,
        kind,
    )
}

fn instruction_safepoint(
    body: &Body,
    allocated: &Allocated,
    assembled: &Assembled,
    span: &SiteSpan,
    target: TargetName,
    op: &Op,
    kind: SafepointKind,
) -> Result<Option<SafepointPayload>, MetadataError> {
    let call = match op {
        Op::Call(call) | Op::ForeignCall(call) => Some(call),
        _ => None,
    };
    dispatch_kind(body, allocated, assembled, span, target, call, kind)
}

fn dispatch_kind(
    body: &Body,
    allocated: &Allocated,
    assembled: &Assembled,
    span: &SiteSpan,
    target: TargetName,
    call: Option<&Call>,
    kind: SafepointKind,
) -> Result<Option<SafepointPayload>, MetadataError> {
    if call.is_some_and(is_leaf) {
        return Ok(None);
    }
    match kind {
        SafepointKind::Allocation | SafepointKind::Barrier | SafepointKind::StackCheck => Ok(None),
        SafepointKind::Poll => poll_point(body, allocated, assembled, span, target),
        SafepointKind::Suspend => {
            resume_point(body, allocated, assembled, span, target, 2, false, call)
        }
        SafepointKind::ForeignBridge => {
            resume_point(body, allocated, assembled, span, target, 3, false, call)
        }
        SafepointKind::DirtyCpuBridge => {
            resume_point(body, allocated, assembled, span, target, 3, true, call)
        }
        SafepointKind::Select => select_point(body, allocated, assembled, span, target, call),
        SafepointKind::CallReturn => {
            resume_point(body, allocated, assembled, span, target, 0, false, call)
        }
    }
}

fn terminator_safepoint(
    body: &Body,
    allocated: &Allocated,
    assembled: &Assembled,
    span: &SiteSpan,
    target: TargetName,
) -> Result<Option<SafepointPayload>, MetadataError> {
    let Terminator::Invoke {
        call, safepoint, ..
    } = &body.blocks[span.block as usize].terminator
    else {
        return Ok(None);
    };
    if is_leaf(call) {
        return Ok(None);
    }
    let Some(point) = safepoint else {
        return Err(MetadataError::new("可能展开的调用缺少安全点"));
    };
    let kind = body.safepoints[point.index()].kind;
    dispatch_kind(body, allocated, assembled, span, target, Some(call), kind)
}

fn select_point(
    body: &Body,
    allocated: &Allocated,
    assembled: &Assembled,
    span: &SiteSpan,
    target: TargetName,
    call: Option<&Call>,
) -> Result<Option<SafepointPayload>, MetadataError> {
    let suspending = call.is_some_and(|call| {
        matches!(
            call.target,
            CallTarget::Runtime(RuntimeCall::SelectCommit {
                has_default: false,
                ..
            })
        )
    });
    let kind = if suspending { 2 } else { 0 };
    resume_point(body, allocated, assembled, span, target, kind, false, call)
}

fn is_leaf(call: &Call) -> bool {
    call.poll_free_leaf || matches!(call.kind, CallKind::ForeignLeaf { .. })
}

fn morestack(
    body: &Body,
    allocated: &Allocated,
    assembled: &Assembled,
    span: &SiteSpan,
) -> Result<Option<SafepointPayload>, MetadataError> {
    if !body.poll_summary.entry_stack_check {
        return Err(MetadataError::new("叶函数不得保留入口栈检查"));
    }
    let call = named_call(
        &allocated.sequence,
        span.insts.0,
        span.insts.1,
        "morestack_or_poll",
    )
    .ok_or_else(|| MetadataError::new("入口栈检查没有 morestack 调用"))?;
    let pc = offset_at(assembled, call)?;
    if pc >= span.bytes.1 && span.bytes.1 != assembled.bytes.len() as u32 {
        return Err(MetadataError::new("morestack 调用不在入口站点内"));
    }
    let mut registers = [0_u16; 5];
    for argument in &allocated.abi.arguments {
        record_abi_register(&mut registers, argument)?;
    }
    Ok(Some(point_payload(pc, 4, false, true, false, 0, registers)))
}

fn poll_point(
    body: &Body,
    allocated: &Allocated,
    assembled: &Assembled,
    span: &SiteSpan,
    target: TargetName,
) -> Result<Option<SafepointPayload>, MetadataError> {
    let call = named_call(
        &allocated.sequence,
        span.insts.0,
        span.insts.1,
        "safepoint_slow",
    )
    .ok_or_else(|| MetadataError::new("poll 站点没有慢路径调用"))?;
    let pc = resume_after(allocated, assembled, call)?;
    rooted(
        body,
        allocated,
        span,
        target,
        None,
        pc,
        1,
        true,
        true,
        false,
        RegisterRoots::Any,
    )
}

fn resume_point(
    body: &Body,
    allocated: &Allocated,
    assembled: &Assembled,
    span: &SiteSpan,
    target: TargetName,
    kind: u8,
    dirty: bool,
    call: Option<&Call>,
) -> Result<Option<SafepointPayload>, MetadataError> {
    let index = last_call(&allocated.sequence, span.insts.0, span.insts.1)
        .ok_or_else(|| MetadataError::new("安全点站点没有 call 指令"))?;
    let pc = offset_at(assembled, index + 1)?;
    if pc >= code_len(assembled) {
        return Err(MetadataError::new("调用返回点落在函数末端之外"));
    }
    let outgoing = if kind == 0 { call } else { None };
    let registers = if kind == 0 {
        RegisterRoots::CalleeSaved
    } else {
        RegisterRoots::Spill
    };
    rooted(
        body, allocated, span, target, outgoing, pc, kind, true, true, dirty, registers,
    )
}

fn rooted(
    body: &Body,
    allocated: &Allocated,
    span: &SiteSpan,
    target: TargetName,
    call: Option<&Call>,
    pc: u32,
    kind: u8,
    copy: bool,
    scan: bool,
    dirty: bool,
    registers_mode: RegisterRoots,
) -> Result<Option<SafepointPayload>, MetadataError> {
    let slot_count = allocated.frame.frame_size / 8;
    let mut registers = [0_u16; 5];
    let mut slots = empty_slots(slot_count);
    let point = site_point(allocated, span.ordinal)?;
    record_live(
        body,
        allocated,
        point,
        registers_mode,
        &mut registers,
        &mut slots,
    )?;
    if matches!(kind, 1 | 2 | 3) {
        record_addressed(body, allocated, &mut slots)?;
    }
    if let Some(call) = call {
        record_outgoing(call, target, &mut slots)?;
    }
    if matches!(registers_mode, RegisterRoots::Spill) && registers.iter().any(|mask| *mask != 0) {
        return Err(MetadataError::new("挂起或 bridge 点保留了用户寄存器根"));
    }
    Ok(Some(
        point_payload(pc, kind, copy, scan, dirty, slot_count, registers).with_slots(slots),
    ))
}

fn point_payload(
    pc: u32,
    kind: u8,
    copy: bool,
    scan: bool,
    dirty: bool,
    slot_count: u32,
    registers: [u16; 5],
) -> SafepointPayload {
    SafepointPayload {
        pc_offset: pc,
        kind,
        copy_allowed: copy,
        scan_allowed: scan,
        dirty,
        slot_count,
        registers,
        slots: empty_slots(slot_count).into_iter().collect(),
    }
}

impl SafepointPayload {
    fn with_slots(mut self, slots: [Vec<u64>; 5]) -> Self {
        self.slots = slots.into_iter().collect();
        self
    }
}

fn empty_slots(slot_count: u32) -> [Vec<u64>; 5] {
    let words = slot_count.div_ceil(8) as usize;
    let lanes = words.div_ceil(8).max(1);
    std::array::from_fn(|_| vec![0; lanes])
}

fn site_point(allocated: &Allocated, ordinal: u32) -> Result<&Point, MetadataError> {
    let site = allocated
        .sites
        .get(ordinal as usize)
        .ok_or_else(|| MetadataError::new("点位表缺少站点"))?;
    let points = allocated
        .points
        .get(site.points.start as usize..site.points.end as usize)
        .ok_or_else(|| MetadataError::new("点位范围越界"))?;
    points
        .iter()
        .rev()
        .find(|point| {
            matches!(
                point.kind,
                PointKind::Call | PointKind::Bridge | PointKind::Prologue
            )
        })
        .or_else(|| points.last())
        .ok_or_else(|| MetadataError::new("安全点没有点位"))
}

/// 安全点上的寄存器根策略。
#[derive(Clone, Copy)]
enum RegisterRoots {
    /// 挂起与 bridge：跨点仍活跃的寄存器根是错误。
    Spill,
    /// 普通调用返回：只登记跨调用仍活跃的 `rbp`/`r12`/`r13`。
    CalleeSaved,
    /// poll 恢复点：登记该点仍覆盖到的寄存器根。
    Any,
}

fn record_live(
    body: &Body,
    allocated: &Allocated,
    point: &Point,
    mode: RegisterRoots,
    registers: &mut [u16; 5],
    slots: &mut [Vec<u64>; 5],
) -> Result<(), MetadataError> {
    for value in &allocated.values {
        if !covers(value, point.use_slot) {
            continue;
        }
        let Some(kind) = root_kind(body, value.value) else {
            continue;
        };
        match value.location() {
            Some(Location::Gpr(gpr)) => {
                let survives = value.range.1 > point.use_slot;
                record_register(mode, survives, gpr, kind, registers)?;
            }
            Some(Location::Slot(slot)) => {
                let offset = allocated
                    .frame
                    .spill_offset(slot)
                    .map_err(|_| MetadataError::new("溢出槽不在帧内"))?;
                set_slot(slots, kind, offset / 8)?;
            }
            Some(Location::Xmm(_)) => {
                return Err(MetadataError::new("受管指针不能进入 XMM"));
            }
            None => {}
        }
    }
    Ok(())
}

fn record_register(
    mode: RegisterRoots,
    survives: bool,
    gpr: Gpr,
    kind: usize,
    registers: &mut [u16; 5],
) -> Result<(), MetadataError> {
    match mode {
        RegisterRoots::Spill if survives => {
            Err(MetadataError::new("跨调用的受管指针仍在通用寄存器"))
        }
        RegisterRoots::Spill => Ok(()),
        RegisterRoots::CalleeSaved if !survives => Ok(()),
        RegisterRoots::CalleeSaved if !matches!(gpr, Gpr::Rbp | Gpr::R12 | Gpr::R13) => {
            Err(MetadataError::new("跨调用的受管指针不在被调用者保存寄存器"))
        }
        RegisterRoots::CalleeSaved | RegisterRoots::Any => set_register(registers, kind, gpr),
    }
}

fn covers(value: &super::alloc::ValueAllocation, slot: u32) -> bool {
    value.is_live() && value.range.0 <= slot && slot <= value.range.1
}

fn root_kind(body: &Body, value: u32) -> Option<usize> {
    let value = body.values.get(value as usize)?;
    if value.kind.ty != crate::lir::body::Type::Ptr {
        return None;
    }
    value
        .kind
        .provenance
        .and_then(Provenance::root_kind)
        .map(|kind| kind as usize)
}

fn record_addressed(
    body: &Body,
    allocated: &Allocated,
    slots: &mut [Vec<u64>; 5],
) -> Result<(), MetadataError> {
    for (index, slot) in body.stack_slots.iter().enumerate() {
        let Some(local) = allocated.frame.local(index) else {
            return Err(MetadataError::new("局部槽缺少帧偏移"));
        };
        for (offset, provenance) in &slot.roots {
            let Some(kind) = provenance.root_kind() else {
                continue;
            };
            let byte = u64::from(local.offset)
                .checked_add(*offset)
                .ok_or_else(|| MetadataError::new("局部根偏移溢出"))?;
            if !byte.is_multiple_of(8) {
                return Err(MetadataError::new("局部根偏移未按机器字对齐"));
            }
            set_slot(
                slots,
                kind as usize,
                u32::try_from(byte / 8).unwrap_or(u32::MAX),
            )?;
        }
    }
    Ok(())
}

fn record_outgoing(
    call: &Call,
    target: TargetName,
    slots: &mut [Vec<u64>; 5],
) -> Result<(), MetadataError> {
    let layout = super::abi::classify_call(call, target)
        .map_err(|_| MetadataError::new("调用 ABI 分类失败"))?;
    for argument in &layout.arguments {
        let Some(AbiSlot::Stack { offset }) = argument.slot else {
            continue;
        };
        record_abi_slot(slots, argument, offset)?;
    }
    Ok(())
}

fn record_abi_register(registers: &mut [u16; 5], argument: &AbiValue) -> Result<(), MetadataError> {
    let Some(kind) = argument.ty.provenance.and_then(Provenance::root_kind) else {
        return Ok(());
    };
    if let Some(AbiSlot::Integer(gpr)) = argument.slot {
        set_register(registers, kind as usize, gpr)?;
    }
    Ok(())
}

fn record_abi_slot(
    slots: &mut [Vec<u64>; 5],
    argument: &AbiValue,
    offset: u32,
) -> Result<(), MetadataError> {
    let Some(kind) = argument.ty.provenance.and_then(Provenance::root_kind) else {
        return Ok(());
    };
    if !offset.is_multiple_of(8) {
        return Err(MetadataError::new("栈参数偏移未按机器字对齐"));
    }
    set_slot(slots, kind as usize, offset / 8)
}

fn set_register(registers: &mut [u16; 5], kind: usize, gpr: Gpr) -> Result<(), MetadataError> {
    let Some(bit) = stackmap_bit(gpr) else {
        return Err(MetadataError::new("rsp 不能作为根寄存器"));
    };
    if bit >= 13 {
        return Err(MetadataError::new("普通函数占用 runtime 保留寄存器"));
    }
    let mask = 1_u16 << bit;
    if registers
        .iter()
        .enumerate()
        .any(|(other, existing)| other != kind && existing & mask != 0)
    {
        return Err(MetadataError::new("寄存器根在多个种类中重复"));
    }
    let lane = registers
        .get_mut(kind)
        .ok_or_else(|| MetadataError::new("根种类越界"))?;
    *lane |= mask;
    Ok(())
}

fn set_slot(slots: &mut [Vec<u64>; 5], kind: usize, index: u32) -> Result<(), MetadataError> {
    let lane = (index / 64) as usize;
    let bit = index % 64;
    let mask = 1_u64 << bit;
    if slots
        .iter()
        .enumerate()
        .any(|(other, bitmap)| other != kind && bitmap.get(lane).copied().unwrap_or(0) & mask != 0)
    {
        return Err(MetadataError::new("槽根在多个种类中重复"));
    }
    let bitmap = slots
        .get_mut(kind)
        .ok_or_else(|| MetadataError::new("根种类越界"))?;
    let word = bitmap
        .get_mut(lane)
        .ok_or_else(|| MetadataError::new("槽下标越过帧"))?;
    *word |= mask;
    Ok(())
}

fn stackmap_bit(gpr: Gpr) -> Option<u8> {
    Some(match gpr {
        Gpr::Rax => 0,
        Gpr::Rbx => 1,
        Gpr::Rcx => 2,
        Gpr::Rdx => 3,
        Gpr::Rsi => 4,
        Gpr::Rdi => 5,
        Gpr::Rbp => 6,
        Gpr::R8 => 7,
        Gpr::R9 => 8,
        Gpr::R10 => 9,
        Gpr::R11 => 10,
        Gpr::R12 => 11,
        Gpr::R13 => 12,
        Gpr::R14 => 13,
        Gpr::R15 => 14,
        Gpr::Rsp => return None,
    })
}

mod records;

fn named_call(sequence: &Sequence, start: u32, end: u32, name: &str) -> Option<u32> {
    let expect = mangle::runtime_symbol(name);
    (start..end).find(|&index| {
        let instruction = &sequence.instructions[index as usize];
        is_call(instruction)
            && instruction.operands.iter().any(|operand| match operand {
                Operand::Reloc(RelocTarget::Lir(symbol), _) => symbol == &expect,
                _ => false,
            })
    })
}

pub(super) fn last_call(sequence: &Sequence, start: u32, end: u32) -> Option<u32> {
    (start..end)
        .rev()
        .find(|&index| is_call(&sequence.instructions[index as usize]))
}

fn is_call(instruction: &Inst) -> bool {
    table::form(instruction.form).mnemonic == "call"
}

fn resume_after(
    allocated: &Allocated,
    assembled: &Assembled,
    call: u32,
) -> Result<u32, MetadataError> {
    let at = call + 1;
    let labeled = allocated.sequence.labels.iter().any(|label| label.at == at);
    let offset = if labeled {
        offset_at(assembled, at)?
    } else {
        offset_at(assembled, at)?
    };
    if offset >= code_len(assembled) {
        return Err(MetadataError::new("poll 恢复点落在函数末端之外"));
    }
    Ok(offset)
}

pub(super) fn code_len(assembled: &Assembled) -> u32 {
    u32::try_from(assembled.bytes.len()).expect("代码长度适配 u32")
}
