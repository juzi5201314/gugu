//! x64 线性扫描寄存器分配、spill slot、固定 frame 与序列改写。
//!
//! # 管线
//!
//! ```text
//! live::analyze      点位编号、CFG 活跃定点、区间与权重
//! scan::scan         候选过滤、线性扫描、spill 决策、拷贝提示
//! slots::allocate    spill slot 分组复用
//! frame::layout      outgoing、locals、spill、scratch、save slot 与 frame_size
//! parallel::resolve  物理并行拷贝解析（寄存器 / frame slot / 16 字节 scratch）
//! rewrite::rewrite   操作数落地、prologue/epilogue 合成、重拼函数序列
//! verify             残留虚拟寄存器、rsp/r14/r15 纪律与 form 形状
//! ```
//!
//! 分配单位是值本身：每个值一个凸区间、一个位置（见 [`live`] 的模块注释）。
//! `Location::Slot` 按 `(size, align, root_class)` 分组复用，不同 root class 绝不共用槽。
//! frame 布局完成后，`StackAddr` 的占位基址与 spill 位置一起换算成 `[rsp + offset]`。

mod frame;
mod live;
mod parallel;
mod rewrite;
mod scan;
mod slots;

pub(crate) use frame::FrameLayout;
pub(crate) use live::{LiveInfo, Point, PointKind, SitePoints, ValueLive};

use std::fmt;
use std::ops::Range;

use crate::Diagnostic;
use crate::diagnostics::DiagnosticCode;
use crate::lir::body::{Body, Provenance, Type};
use crate::runtime::RuntimeRawContractV1;
use crate::target::TargetName;

use super::inst::Sequence;
use super::reg::{Gpr, Reg, Xmm};
use super::select::{SelectedBlock, SelectedFunction};

/// 分配失败：输入结构或内部不变量被破坏，编译到不了机器码阶段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AllocError {
    message: String,
}

impl AllocError {
    /// 用固定文本创建失败。
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AllocError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AllocError {}

/// 寄存器 bank。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum RegClass {
    Gpr = 0,
    Xmm = 1,
}

impl RegClass {
    /// 机器类型所属 bank：`F32`/`F64`/`V128` 走 XMM，其余走 GPR。
    pub(crate) const fn of(ty: Type) -> Self {
        match ty {
            Type::F32 | Type::F64 | Type::V128(_) => Self::Xmm,
            Type::I8
            | Type::I16
            | Type::I32
            | Type::I64
            | Type::Ptr
            | Type::Flags
            | Type::Mem
            | Type::Void => Self::Gpr,
        }
    }

    /// payload 判别值。
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }
}

/// spill slot 的根类别：不同 root class 绝不共用槽，栈复制与 stack map 因此不依赖
/// 某时刻残留的位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
#[expect(
    clippy::enum_variant_names,
    reason = "三个变体的共同后缀正是它们在 stack map 里表达的含义"
)]
pub(crate) enum RootClass {
    NonPointer = 0,
    HeapPointer = 1,
    StackPointer = 2,
}

impl RootClass {
    /// 由 provenance 推出根类别。
    pub(crate) const fn of(provenance: Option<Provenance>) -> Self {
        match provenance {
            Some(Provenance::Stack) => Self::StackPointer,
            Some(
                Provenance::GcHeap
                | Provenance::GcInterior
                | Provenance::SharedHandle
                | Provenance::CompressedRef,
            ) => Self::HeapPointer,
            Some(
                Provenance::Raw | Provenance::Code | Provenance::Metadata | Provenance::Foreign,
            )
            | None => Self::NonPointer,
        }
    }

    /// payload 判别值。
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }

    /// managed/stack 指针必须落 frame slot（挂起与 bridge 点）。
    pub(crate) const fn must_spill(self) -> bool {
        !matches!(self, Self::NonPointer)
    }
}

/// 一个值的落地点。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Location {
    Gpr(Gpr),
    Xmm(Xmm),
    /// spill slot 下标（slots 表内）。
    Slot(u32),
}

/// 拷贝提示：影响位置选择质量，不影响正确性。
///
/// `slot` 是提示所属拷贝组的起点 slot，多个提示按 `(slot, 出现顺序)` 排列。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Hint {
    /// 建议落到该物理寄存器（组里某一侧的固定寄存器）。
    Register(Reg, u32),
    /// 建议与 `src` 值同寄存器（双虚拟边的拷贝）。
    SameAs(u32, u32),
}

/// 值的分配结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Placement {
    /// 整段区间占用一个物理寄存器或 spill slot。
    Location(Location),
    /// 常量/symbol/stack 地址常量：定义站点被丢弃，每个使用处重建。
    Rematerialize,
    /// 值没有活跃区间（站点序列里没有事件）：不出现任何位置。
    Dead,
}

/// 一个值的分配结果与区间。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValueAllocation {
    pub(crate) value: u32,
    pub(crate) class: RegClass,
    pub(crate) root: RootClass,
    /// 区间，`end < start` 表示值已死。
    pub(crate) range: (u32, u32),
    pub(crate) placement: Placement,
}

impl ValueAllocation {
    /// 是否有分配需求。
    pub(crate) const fn is_live(&self) -> bool {
        self.range.0 <= self.range.1
    }

    /// 位置查询；未落到具体位置（spill 后的重建、已死）返回 `None`。
    pub(crate) const fn location(&self) -> Option<Location> {
        match self.placement {
            Placement::Location(location) => Some(location),
            Placement::Rematerialize | Placement::Dead => None,
        }
    }
}

/// 一次分配的确定性统计。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AllocationStats {
    pub(crate) peak_live_gpr: u32,
    pub(crate) peak_live_xmm: u32,
    pub(crate) spill_slot_count: u32,
    pub(crate) spill_bytes: u32,
    pub(crate) spill_stores: u32,
    pub(crate) reloads: u32,
    pub(crate) rematerializations: u32,
    pub(crate) copy_moves: u32,
    pub(crate) copy_cycles: u32,
    pub(crate) call_sites: u32,
    pub(crate) safepoint_spills: u32,
    pub(crate) allocated_values: u32,
    pub(crate) frame_size_max: u32,
}

/// 分配与 frame 的结果：改写后的块、最终序列与 stack map 需要的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Allocated {
    /// mangled 符号。
    pub(crate) symbol: String,
    /// 块布局。
    pub(crate) layout: super::layout::BlockLayout,
    /// 签名 ABI 布局。
    pub(crate) abi: super::abi::AbiLayout,
    /// 改写后的块（站点与终结符序列已落地）。
    pub(crate) blocks: Vec<SelectedBlock>,
    /// 重拼、松弛后的函数级序列。
    pub(crate) sequence: Sequence,
    pub(crate) rel8_count: u32,
    pub(crate) frame: FrameLayout,
    /// 点位表：stack map 的 PC 区间与寄存器/spill 归属从这里查。
    pub(crate) points: Vec<Point>,
    pub(crate) sites: Vec<SitePoints>,
    /// 逐值分配结果。
    pub(crate) values: Vec<ValueAllocation>,
    pub(crate) stats: AllocationStats,
    /// prologue 与各 epilogue 在函数序列里的指令区间；只有这里允许写 `rsp`。
    pub(crate) abi_regions: Vec<Range<u32>>,
}

/// 对已选指的函数做分配、frame 合成与序列改写。
pub(crate) fn allocate(
    body: &Body,
    selected: SelectedFunction,
    target: TargetName,
    raw: &RuntimeRawContractV1,
) -> Result<Allocated, Vec<Diagnostic>> {
    let symbol = selected.symbol.clone();
    run(body, selected, target, raw).map_err(|error| {
        vec![Diagnostic::error(
            DiagnosticCode::BackendInvariant,
            format!("{symbol} 的寄存器分配失败：{error}"),
            None,
        )]
    })
}

fn run(
    body: &Body,
    selected: SelectedFunction,
    target: TargetName,
    raw: &RuntimeRawContractV1,
) -> Result<Allocated, AllocError> {
    let symbol = selected.symbol.clone();
    let live = live::analyze(body, &selected)?;
    let scanned = scan::scan(&live)?;
    let mut values = scanned.values;
    let mut slots = slots::assign(&mut values);
    let request = parallel::Request::new(body, &selected, &live, &values)?;
    // 第一遍解析只为判定是否需要 16 字节 copy scratch；布局完成后按真实偏移再解一次。
    let probe = request.resolve(None)?;
    let frame = frame::layout(
        body,
        &selected,
        target,
        &values,
        &mut slots,
        probe.uses_scratch,
    )?;
    let groups = request.resolve(frame.scratch_offset())?;
    let rewritten = rewrite::rewrite(
        body,
        &selected,
        &live,
        &values,
        &frame,
        &groups,
        &request.stack_arguments,
        target,
        raw.coroutine().stack_check_offset,
    )?;
    let mut stats = scanned.stats;
    slots::statistics(&slots, &mut stats);
    stats.frame_size_max = frame.frame_size;
    stats.copy_moves = groups.moves;
    stats.copy_cycles = groups.cycles;
    stats.reloads = rewritten.traffic.reloads;
    stats.spill_stores = rewritten.traffic.stores;
    stats.rematerializations = rewritten.traffic.rematerializations;
    Ok(Allocated {
        symbol,
        layout: selected.layout,
        abi: selected.abi,
        blocks: rewritten.blocks,
        sequence: rewritten.sequence,
        rel8_count: rewritten.rel8_count,
        frame,
        points: live.points,
        sites: live.sites,
        values,
        stats,
        abi_regions: rewritten.abi_regions,
    })
}
