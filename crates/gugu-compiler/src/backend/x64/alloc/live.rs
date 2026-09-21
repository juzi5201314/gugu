//! 点位编号、CFG 活跃定点、线性区间与 spill 权重。
//!
//! # 点位空间
//!
//! 机器块与 LIR block 一一对应，块按 [`layout::schedule`](super::super::layout::schedule)
//! 的顺序排列。函数内按布局顺序遍历机器指令，第 `p` 个点位（一条机器指令或一个拷贝组）
//! 占用 `use_slot(p) = 2p` 与 `def_slot(p) = 2p + 1`：所有读发生在 `2p`，所有写在 `2p + 1`。
//! 拷贝组内所有 move 共享同一点位，组内顺序由物理并行拷贝解析负责，因此组里出现的
//! 临时虚拟寄存器绝不进入活跃。
//!
//! # 活跃事件
//!
//! 事件全部来自机器序列，不重新解释 LIR 语义：
//!
//! - `Operand::Reg(Reg::Virtual(v))` 按 form 的 `access` 分为使用（`Read`/`Address`）或
//!   定义（`Write`/`ReadWrite`；`ReadWrite` 同时是使用）；
//! - `Mem` 的虚拟 base/index 恒为使用（地址计算读取它）；
//! - 组的 `pairs`：虚拟 `src` 是使用、虚拟 `dest` 是定义，物理一侧不产生事件；
//! - `StackAddr` 的 frame 占位编号（`>= FRAME_SLOT_BASE`）不是值。
//!
//! 除操作数外，点位 mask 还并上指令**读到**的物理寄存器：`call` 的返回值寄存器在收集点上
//! 仍被读，任何插在那里、写同一寄存器的新 move 都会破坏返回值，因此读集合必须与写集合
//! 一样参与候选过滤。
//!
//! # 区间与分配单位
//!
//! 每个值得到一个凸区间：起点取所有触碰点与 `live_in` 入口的最小 slot，终点取所有触碰点与
//! `live_out` 出口的最大 slot。块级活跃把「活跃但无触碰」的区域（穿越的 mid block、循环
//! 回边）折进区间，循环参数因此覆盖整个循环。
//!
//!   分配单位是**值本身**，不按 clobber 点切段：点位边界上的过渡 move 只在「该边界位于
//! def 到 use 的每条路径上」时才正确，而块内冷路径的 `jmp`（`GcAlloc` 的快/慢两条路径、
//! `SafepointPoll` 把 `call` 放在冷路）、多定义（块参数由多条入边定义）与跨越布局顺序的
//! 回边都会破坏这个前提。统一位置换来可证明的正确性：插入的 move 只有「定义处写位置」与
//! 「使用处读位置」两种，都只依赖当前路径。非指针跨 call 仍优先 `rbp/r12/r13`；
//! `CallReturn`、分配、挂起与 bridge 上的 managed/stack 指针整段落槽。

use std::ops::Range;

use super::super::copies::Copy;
use super::super::inst::{Inst, Operand};
use super::super::layout;
use super::super::lower::Lowered;
use super::super::reg::{Clobbers, FRAME_SLOT_BASE, Gpr, Reg};
use super::super::select::{SelectedFunction, SiteKind};
use super::super::table::{self, Access, OperandKind};
use super::{AllocError, Hint, RegClass, RootClass};
use crate::lir::body::{BlockId, Body, Definition, Op, SafepointKind, Terminator};
use crate::target::TargetName;

/// 调用点 outgoing 区里的一个指针字。偏移相对 outgoing 区起点，供栈图扫描栈上副本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OutgoingRoot {
    /// 实参值编号。
    pub(crate) value: u32,
    /// [`RootClass`] 判别值。
    pub(crate) root: u8,
    /// 相对 outgoing 区起点的字节偏移。
    pub(crate) offset: u32,
}

/// `CallReturn`、分配、挂起与 bridge 禁止用户寄存器根；`Poll` 可以留在寄存器里。
pub(crate) fn spills_pointers(kind: Option<SafepointKind>) -> bool {
    matches!(
        kind,
        Some(
            SafepointKind::CallReturn
                | SafepointKind::Allocation
                | SafepointKind::Suspend
                | SafepointKind::ForeignBridge
                | SafepointKind::DirtyCpuBridge
        )
    )
}

/// 点位种类；判别值与 payload 一致（0 Normal / 1 Call / 2 Bridge / 3 Prologue）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum PointKind {
    Normal = 0,
    Call = 1,
    Bridge = 2,
    Prologue = 3,
}

impl PointKind {
    /// payload 判别值。
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }

    /// 是否为 call/bridge 点位：spill 权重与「紧跟 safepoint」判定共用。
    pub(crate) const fn is_call_or_bridge(self) -> bool {
        matches!(self, Self::Call | Self::Bridge)
    }
}

/// 一个点位：一条机器指令或一个拷贝组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Point {
    /// 片段 sites 数组下标（块内站点顺序，终结符紧随块内站点）。
    pub(crate) site: u32,
    pub(crate) kind: PointKind,
    /// 布局序块下标。
    pub(crate) block: u32,
    pub(crate) use_slot: u32,
    pub(crate) def_slot: u32,
    /// 站点序列内的机器指令范围（组的渲染指令整体落在组点位上）。
    pub(crate) instructions: Range<u32>,
    /// 跨该点位存活的值不得使用的物理寄存器：写 ∪ 读 ∪ call/bridge 的 caller-saved 规则。
    pub(crate) mask: Clobbers,
    /// 该点位写掉的物理寄存器子集；call/bridge 点位并上 caller-saved。
    pub(crate) clobber: Clobbers,
    /// managed/stack 指针跨该点位必须落 frame slot。
    pub(crate) pointer_spill: bool,
    /// 点位含固定物理寄存器操作数或破坏物理寄存器；spill 权重取 4。
    pub(crate) fixed_physical: bool,
    /// 该调用点 outgoing 区中的指针字；非调用点为空。
    pub(crate) outgoing_roots: Vec<OutgoingRoot>,
}

/// 一个站点的点位区间。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SitePoints {
    pub(crate) site: u32,
    pub(crate) points: Range<u32>,
}

/// 一个值在分配输入侧的静态属性。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValueLive {
    pub(crate) class: RegClass,
    pub(crate) root: RootClass,
    /// 该值出现在 `Rm8`/`R8` 操作数位置：候选寄存器必须能编码低字节。
    pub(crate) byte_operand: bool,
    /// 定义 op 是常量/symbol/stack 地址：spill 后可重建。
    pub(crate) rematerializable: bool,
    /// 使用 slot，按点位顺序递增。
    pub(crate) uses: Vec<u32>,
    /// 区间起点；`end < start` 表示值已死（没有分配需求）。
    pub(crate) start: u32,
    /// 区间终点。
    pub(crate) end: u32,
    /// `u64` 饱和累加的 spill 权重。
    pub(crate) weight: u64,
    /// 拷贝提示，按 `(组起点, pair 序号)` 排列。
    pub(crate) hints: Vec<Hint>,
}

impl ValueLive {
    /// 是否有分配需求。
    pub(crate) const fn is_live(&self) -> bool {
        self.start <= self.end
    }
}

/// 值区间是否覆盖点位下标 `point`。区间端点是 slot（`use_slot = 2p`），不能拿下标直接比。
pub(crate) fn range_covers_point(range: (u32, u32), point: u32) -> bool {
    debug_assert!(point <= u32::MAX / 2, "点位下标必须能换成 slot");
    let use_slot = point * 2;
    let def_slot = use_slot + 1;
    use_slot <= range.1 && def_slot >= range.0
}

/// 全部点位、站点索引与值属性。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LiveInfo {
    pub(crate) points: Vec<Point>,
    pub(crate) sites: Vec<SitePoints>,
    pub(crate) values: Vec<ValueLive>,
}

/// 站点序列的一个点位划分项。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlanEntry {
    /// 站点序列内的机器指令范围。
    pub(crate) instructions: Range<u32>,
    /// 拷贝组的并行配对；`None` 表示这一项是一条机器指令。
    pub(crate) pairs: Option<Vec<Copy>>,
}

/// 按拷贝组划分站点序列；组的渲染指令整体归组点位。
pub(crate) fn site_plan(lowered: &Lowered) -> Result<Vec<PlanEntry>, AllocError> {
    let mut groups = lowered.copy_groups.iter().peekable();
    let count = u32::try_from(lowered.sequence.instructions.len()).expect("站点指令数适配 u32");
    let mut entries = Vec::with_capacity(lowered.sequence.instructions.len());
    let mut cursor = 0_u32;
    while cursor < count {
        if let Some(group) = groups.peek() {
            if group.instructions.start < cursor {
                return Err(AllocError::new("并行拷贝组的指令范围与前一范围重叠"));
            }
            if group.instructions.start == cursor {
                if group.instructions.end > count {
                    return Err(AllocError::new("并行拷贝组的指令范围越过站点序列"));
                }
                entries.push(PlanEntry {
                    instructions: group.instructions.clone(),
                    pairs: Some(group.pairs.clone()),
                });
                cursor = group.instructions.end;
                groups.next();
                continue;
            }
        }
        entries.push(PlanEntry {
            instructions: cursor..cursor + 1,
            pairs: None,
        });
        cursor += 1;
    }
    if groups.next().is_some() {
        return Err(AllocError::new("并行拷贝组的指令范围越过站点序列"));
    }
    Ok(entries)
}

/// 全部点位、活跃与区间。
pub(crate) fn analyze(body: &Body, selected: &SelectedFunction) -> Result<LiveInfo, AllocError> {
    let value_count = body.values.len();
    if u32::try_from(value_count).expect("值编号适配 u32") >= FRAME_SLOT_BASE {
        return Err(AllocError::new("值编号越过 frame 占位空间"));
    }
    let mut values = collect_values(body);
    let mut points = Vec::new();
    let mut sites: Vec<SitePoints> = Vec::new();
    let words = value_count.div_ceil(64);
    let mut uses = vec![vec![0_u64; words]; body.blocks.len()];
    let mut defs = vec![vec![0_u64; words]; body.blocks.len()];

    for (order, block) in selected.blocks.iter().enumerate() {
        let lir = block.id.index();
        let order = u32::try_from(order).expect("块序适配 u32");
        for site in &block.sites {
            let index = u32::try_from(sites.len()).expect("站点数适配 u32");
            let first = u32::try_from(points.len()).expect("点位数适配 u32");
            let instruction = &body.instructions
                [usize::try_from(site.instruction).expect("站点指令编号适配 usize")];
            visit_site(
                body,
                site.kind,
                spills_pointers(instruction.op.safepoint_kind()),
                &site.lowered,
                index,
                order,
                &mut points,
                &mut values,
                &mut uses[lir],
                &mut defs[lir],
            )?;
            sites.push(SitePoints {
                site: index,
                points: first..u32::try_from(points.len()).expect("点位数适配 u32"),
            });
        }
        let index = u32::try_from(sites.len()).expect("站点数适配 u32");
        let first = u32::try_from(points.len()).expect("点位数适配 u32");
        visit_site(
            body,
            SiteKind::Normal,
            terminator_spills(&body.blocks[lir].terminator),
            &block.terminator,
            index,
            order,
            &mut points,
            &mut values,
            &mut uses[lir],
            &mut defs[lir],
        )?;
        sites.push(SitePoints {
            site: index,
            points: first..u32::try_from(points.len()).expect("点位数适配 u32"),
        });
    }

    let liveness = liveness(body, &uses, &defs);
    let bounds = block_bounds(&points, body.blocks.len());
    intervals(&bounds, &mut values, &liveness);
    let depths = loop_depths(body);
    weights(&points, &depths, &mut values);
    Ok(LiveInfo {
        points,
        sites,
        values,
    })
}

/// 值的静态属性与定义来源。
fn collect_values(body: &Body) -> Vec<ValueLive> {
    body.values
        .iter()
        .map(|value| {
            let rematerializable = match value.definition {
                Definition::Instruction { instruction, .. } => matches!(
                    &body.instructions[instruction.index()].op,
                    Op::IConst(_) | Op::FConst(_) | Op::SymbolAddr(_) | Op::StackAddr(_)
                ),
                Definition::Parameter { .. } | Definition::Invoke { .. } => false,
            };
            ValueLive {
                class: RegClass::of(value.kind.ty),
                root: RootClass::of(value.kind.provenance),
                byte_operand: false,
                rematerializable,
                uses: Vec::new(),
                start: u32::MAX,
                end: 0,
                weight: 0,
                hints: Vec::new(),
            }
        })
        .collect()
}

fn terminator_spills(terminator: &Terminator) -> bool {
    match terminator {
        Terminator::Invoke { call, .. } | Terminator::TailCall { call, .. } => {
            spills_pointers(call.safepoint_kind())
        }
        _ => false,
    }
}

/// 遍历一个站点，产出点位并登记活跃事件。
#[expect(
    clippy::too_many_arguments,
    reason = "点位遍历需要同时推进值属性、块级事件位图与点位表"
)]
fn visit_site(
    body: &Body,
    kind: SiteKind,
    spills_pointers: bool,
    lowered: &Lowered,
    site: u32,
    block: u32,
    points: &mut Vec<Point>,
    values: &mut [ValueLive],
    uses: &mut [u64],
    defs: &mut [u64],
) -> Result<(), AllocError> {
    let entries = site_plan(lowered)?;
    let prologue = kind == SiteKind::Prologue;
    let bridge = kind == SiteKind::Bridge;
    for entry in entries {
        let index = u32::try_from(points.len()).expect("点位数适配 u32");
        let use_slot = index * 2;
        let mut point = Point {
            site,
            kind: if prologue {
                PointKind::Prologue
            } else if bridge {
                PointKind::Bridge
            } else {
                PointKind::Normal
            },
            block,
            use_slot,
            def_slot: use_slot + 1,
            instructions: entry.instructions.clone(),
            mask: Clobbers::NONE,
            clobber: Clobbers::NONE,
            pointer_spill: spills_pointers,
            fixed_physical: false,
            outgoing_roots: Vec::new(),
        };
        match &entry.pairs {
            Some(pairs) => {
                for pair in pairs {
                    event(body, values, uses, defs, pair.src, Event::Use, &point)?;
                    event(body, values, uses, defs, pair.dest, Event::Def, &point)?;
                    point.fixed_physical |= is_physical(pair.src) || is_physical(pair.dest);
                }
                for pair in pairs {
                    record_hint(values, pair, use_slot)?;
                }
            }
            None => {
                let instruction = &lowered.sequence.instructions[entry.instructions.start as usize];
                let form = table::form(instruction.form);
                if form.mnemonic == "call" {
                    point.kind = if bridge {
                        PointKind::Bridge
                    } else {
                        PointKind::Call
                    };
                }
                for (position, operand) in instruction.operands.iter().enumerate() {
                    let Some(access) = form.access.get(position) else {
                        continue;
                    };
                    let operand_kind = form.operands[position];
                    match operand {
                        Operand::Reg(reg) => {
                            if matches!(reg, Reg::Virtual(_)) {
                                byte_marking(values, operand_kind, *reg);
                            } else {
                                point.fixed_physical = true;
                            }
                            match access {
                                Access::Read | Access::Address => {
                                    event(body, values, uses, defs, *reg, Event::Use, &point)?;
                                }
                                Access::Write | Access::ReadWrite => {
                                    event(body, values, uses, defs, *reg, Event::Def, &point)?;
                                    if matches!(access, Access::ReadWrite) {
                                        event(body, values, uses, defs, *reg, Event::Use, &point)?;
                                    }
                                }
                            }
                        }
                        Operand::Mem(mem) => {
                            for reg in [mem.base, mem.index].into_iter().flatten() {
                                if matches!(reg, Reg::Virtual(_)) {
                                    event(body, values, uses, defs, reg, Event::Use, &point)?;
                                } else {
                                    point.fixed_physical = true;
                                }
                            }
                        }
                        Operand::Imm(_)
                        | Operand::Rip(..)
                        | Operand::Reloc(..)
                        | Operand::Label(_) => {}
                    }
                }
                point.clobber = clobber_mask(instruction, form);
                point.mask = point.clobber.union(read_mask(instruction, form));
                if form.mnemonic == "call" {
                    point.clobber = point.clobber.union(caller_saved());
                    point.mask = point.clobber;
                }
            }
        }
        if prologue {
            // 合成 prologue 点位：序列渲染由 frame 阶段产出，这里只保留 r11 占位 mask。
            point.mask = Clobbers::gpr(Gpr::R11);
            point.clobber = Clobbers::NONE;
            point.fixed_physical = false;
            point.kind = PointKind::Prologue;
            point.pointer_spill = false;
        }
        if bridge {
            point.mask = point.mask.union(caller_saved());
            point.clobber = point.clobber.union(caller_saved());
        }
        debug_assert_eq!(point.use_slot, index * 2, "点位编号必须稠密");
        points.push(point);
    }
    Ok(())
}

/// 单个操作数的活跃事件：登记块级位图与值的区间端点。
fn event(
    body: &Body,
    values: &mut [ValueLive],
    uses: &mut [u64],
    defs: &mut [u64],
    reg: Reg,
    slot: Event,
    point: &Point,
) -> Result<(), AllocError> {
    let Reg::Virtual(id) = reg else {
        return Ok(());
    };
    if id >= FRAME_SLOT_BASE {
        return Ok(());
    }
    let index = usize::try_from(id).map_err(|_| AllocError::new("虚拟寄存器编号超出宿主范围"))?;
    let Some(value) = values.get_mut(index) else {
        return Err(AllocError::new(format!(
            "虚拟寄存器 v{id} 越过值编号空间（值数 {}），临时编号泄漏进了站点序列",
            body.values.len()
        )));
    };
    let touch = match slot {
        Event::Use => {
            value.uses.push(point.use_slot);
            point.use_slot
        }
        Event::Def => point.def_slot,
    };
    value.start = value.start.min(touch);
    value.end = value.end.max(touch);
    let bits = match slot {
        Event::Use => &mut *uses,
        Event::Def => &mut *defs,
    };
    bits[index / 64] |= 1_u64 << (index % 64);
    Ok(())
}

/// 值出现在低字节操作数位置：候选寄存器必须能编码 `r8` 形态。
fn byte_marking(values: &mut [ValueLive], kind: OperandKind, reg: Reg) {
    let Reg::Virtual(id) = reg else {
        return;
    };
    if !matches!(kind, OperandKind::Rm8 | OperandKind::R8) {
        return;
    }
    if let Some(value) = usize::try_from(id)
        .ok()
        .and_then(|index| values.get_mut(index))
    {
        value.byte_operand = true;
    }
}

/// 拷贝提示：物理一侧给另一侧记寄存器提示，双虚拟边给 `dest` 记「与 `src` 同寄存器」。
fn record_hint(values: &mut [ValueLive], pair: &Copy, slot: u32) -> Result<(), AllocError> {
    let (value, hint): (u32, Hint) = match (pair.src, pair.dest) {
        (Reg::Virtual(dest), src) if is_physical(src) => (dest, Hint::Register(src, slot)),
        (src, Reg::Virtual(dest)) if is_physical(src) => (dest, Hint::Register(src, slot)),
        (Reg::Virtual(src), Reg::Virtual(dest)) => (dest, Hint::SameAs(src, slot)),
        _ => return Ok(()),
    };
    let index =
        usize::try_from(value).map_err(|_| AllocError::new("虚拟寄存器编号超出宿主范围"))?;
    let Some(entry) = values.get_mut(index) else {
        return Err(AllocError::new(format!(
            "虚拟寄存器 v{value} 越过值编号空间"
        )));
    };
    entry.hints.push(hint);
    Ok(())
}

/// 事件槽：使用或定义。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Event {
    Use,
    Def,
}

/// 物理寄存器判定。
const fn is_physical(reg: Reg) -> bool {
    matches!(reg, Reg::Gpr(_) | Reg::Xmm(_))
}

/// 点位覆盖时必须回避的 caller-saved 集合：九个整数参数寄存器与全部 XMM。
pub(crate) fn caller_saved() -> Clobbers {
    let mut gpr = 0_u32;
    for reg in [
        Gpr::Rax,
        Gpr::Rcx,
        Gpr::Rdx,
        Gpr::Rbx,
        Gpr::Rsi,
        Gpr::Rdi,
        Gpr::R8,
        Gpr::R9,
        Gpr::R10,
    ] {
        gpr |= reg.bit();
    }
    Clobbers { gpr, xmm: u32::MAX }
}

/// 指令写掉的物理寄存器：`Write`/`ReadWrite` 操作数加上 form 隐式破坏。
pub(crate) fn clobber_mask(inst: &Inst, form: &table::Form) -> Clobbers {
    let mut clobbers = form.clobbers;
    for (position, operand) in inst.operands.iter().enumerate() {
        if !matches!(
            form.access.get(position),
            Some(Access::Write | Access::ReadWrite)
        ) {
            continue;
        }
        if let Operand::Reg(reg) = operand {
            clobbers = clobbers.union(register_bit(*reg));
        }
    }
    clobbers
}

/// 指令读到的物理寄存器：`Read`/`Address`/`ReadWrite` 操作数与 `Mem` 的物理基址索引。
///
/// 读集合同样进入点位 mask：`call` 的返回值寄存器在收集点上还被读，任何插在那里、
/// 写该寄存器的新 move 都会破坏返回值。
pub(crate) fn read_mask(inst: &Inst, form: &table::Form) -> Clobbers {
    let mut reads = Clobbers::NONE;
    for (position, operand) in inst.operands.iter().enumerate() {
        if !matches!(
            form.access.get(position),
            Some(Access::Read | Access::Address | Access::ReadWrite)
        ) {
            continue;
        }
        match operand {
            Operand::Reg(reg) => reads = reads.union(register_bit(*reg)),
            Operand::Mem(mem) => {
                for reg in [mem.base, mem.index].into_iter().flatten() {
                    reads = reads.union(register_bit(reg));
                }
            }
            _ => {}
        }
    }
    reads
}

/// 单个物理寄存器的位图；虚拟寄存器不产生位。
pub(crate) const fn register_bit(reg: Reg) -> Clobbers {
    match reg {
        Reg::Gpr(gpr) => Clobbers::gpr(gpr),
        Reg::Xmm(xmm) => Clobbers::xmm(xmm),
        Reg::Virtual(_) => Clobbers::NONE,
    }
}

/// 块级活跃定点：`live_in[b] = uses[b] ∪ (live_out[b] \ defs[b])`，`live_out[b] = ∪ live_in[succ]`。
pub(crate) struct Liveness {
    live_in: Vec<Vec<u64>>,
    live_out: Vec<Vec<u64>>,
}

fn liveness(body: &Body, uses: &[Vec<u64>], defs: &[Vec<u64>]) -> Liveness {
    let words = uses.first().map_or(0, Vec::len);
    let mut live_in = vec![vec![0_u64; words]; body.blocks.len()];
    let mut live_out = vec![vec![0_u64; words]; body.blocks.len()];
    let successors: Vec<Vec<BlockId>> = (0..body.blocks.len())
        .map(|index| {
            layout::successors(body, BlockId(u32::try_from(index).expect("块下标适配 u32")))
        })
        .collect();
    loop {
        let mut changed = false;
        for (index, succs) in successors.iter().enumerate() {
            let mut out = vec![0_u64; words];
            for succ in succs {
                union_into(&mut out, &live_in[succ.index()]);
            }
            let mut entry = out.clone();
            for (word, def) in defs[index].iter().enumerate() {
                entry[word] |= uses[index][word];
                entry[word] &= !def;
            }
            if out != live_out[index] {
                live_out[index] = out;
                changed = true;
            }
            if entry != live_in[index] {
                live_in[index] = entry;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Liveness { live_in, live_out }
}

fn union_into(dst: &mut [u64], src: &[u64]) {
    for (left, right) in dst.iter_mut().zip(src) {
        *left |= *right;
    }
}

/// 区间构造：触碰点加上 `live_in`/`live_out` 的全部块边界。
fn intervals(bounds: &[BlockBounds], values: &mut [ValueLive], liveness: &Liveness) {
    for (index, value) in values.iter_mut().enumerate() {
        if !value.is_live() {
            continue;
        }
        let mut start = value.start;
        let mut end = value.end;
        for (bound, (live_in, live_out)) in bounds
            .iter()
            .zip(liveness.live_in.iter().zip(&liveness.live_out))
        {
            if bit(live_in, index) {
                start = start.min(bound.first_use);
            }
            if bit(live_out, index) {
                end = end.max(bound.last_def);
            }
        }
        value.start = start;
        value.end = end;
    }
}

/// 位图取位。
fn bit(bits: &[u64], index: usize) -> bool {
    bits.get(index / 64)
        .is_some_and(|word| word & (1_u64 << (index % 64)) != 0)
}

/// 布局序块的 slot 边界：`live_in`/`live_out` 与点的 slot 区间的换算。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BlockBounds {
    first_use: u32,
    last_def: u32,
}

/// 布局序块 → slot 边界；空块（无点位）不参与区间扩展。
fn block_bounds(points: &[Point], blocks: usize) -> Vec<BlockBounds> {
    let mut bounds = vec![
        BlockBounds {
            first_use: u32::MAX,
            last_def: 0,
        };
        blocks
    ];
    for point in points {
        let bound = &mut bounds[point.block as usize];
        bound.first_use = bound.first_use.min(point.use_slot);
        bound.last_def = bound.last_def.max(point.def_slot);
    }
    for bound in &mut bounds {
        if bound.first_use == u32::MAX {
            bound.first_use = 0;
        }
    }
    bounds
}

/// spill 权重：`u64` 饱和累加，常量/symbol 地址为 0。
fn weights(points: &[Point], depths: &[u32], values: &mut [ValueLive]) {
    for value in values.iter_mut() {
        if value.rematerializable {
            value.weight = 0;
            continue;
        }
        let mut total = 0_u64;
        for slot in &value.uses {
            let Some(point) = points.get((*slot / 2) as usize) else {
                continue;
            };
            let base = if point.fixed_physical || point.kind.is_call_or_bridge() {
                4_u64
            } else {
                1
            };
            let depth = depths.get(point.block as usize).copied().unwrap_or(0);
            let mut weight = base.saturating_mul(loop_factor(depth));
            if point.use_slot >= 2
                && points
                    .get(((point.use_slot - 2) / 2) as usize)
                    .is_some_and(|previous| {
                        previous.block == point.block && previous.kind.is_call_or_bridge()
                    })
            {
                weight = weight.saturating_mul(2);
            }
            total = total.saturating_add(weight);
        }
        value.weight = total;
    }
}

/// 循环深度因子：`min(10^d, 1_000_000)`。
pub(crate) fn loop_factor(depth: u32) -> u64 {
    10_u64.checked_pow(depth).unwrap_or(u64::MAX).min(1_000_000)
}

/// 自然循环深度：迭代求 dominator，对回边收集循环体；环上未被覆盖的块保守计 1。
fn loop_depths(body: &Body) -> Vec<u32> {
    let count = body.blocks.len();
    let words = count.div_ceil(64);
    let entry = body.entry.index();
    let predecessors: Vec<Vec<usize>> = (0..count)
        .map(|index| {
            body.predecessors[crate::lir::body::range(&body.blocks[index].predecessors)]
                .iter()
                .map(|edge| body.edges[edge.index()].from.index())
                .collect()
        })
        .collect();
    let mut dominators: Vec<Vec<u64>> = (0..count)
        .map(|block| {
            let mut bits = vec![u64::MAX; words];
            if block == entry {
                bits = vec![0; words];
                bits[entry / 64] |= 1_u64 << (entry % 64);
            }
            bits
        })
        .collect();
    loop {
        let mut changed = false;
        for block in 0..count {
            if block == entry {
                continue;
            }
            let mut bits = vec![u64::MAX; words];
            for pred in &predecessors[block] {
                for (word, value) in bits.iter_mut().zip(&dominators[*pred]) {
                    *word &= *value;
                }
            }
            bits[block / 64] |= 1_u64 << (block % 64);
            if bits != dominators[block] {
                dominators[block] = bits;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut depth = vec![0_u32; count];
    for block in 0..count {
        for pred in &predecessors[block] {
            if !bit(&dominators[*pred], block) {
                continue;
            }
            // 回边 pred → block：沿前驱回溯到 block 收集循环体。
            let mut members = vec![*pred];
            let mut seen = vec![false; count];
            seen[*pred] = true;
            let mut cursor = 0;
            while cursor < members.len() {
                let member = members[cursor];
                cursor += 1;
                for next in &predecessors[member] {
                    if *next == block || seen[*next] || !bit(&dominators[*next], block) {
                        continue;
                    }
                    seen[*next] = true;
                    members.push(*next);
                }
            }
            for member in members {
                depth[member] = depth[member].saturating_add(1);
            }
        }
    }
    conservative_cycles(&predecessors, &mut depth);
    depth
}

/// 不可归约环兜底：位于环上、自然环未覆盖的块深度至少为 1。
fn conservative_cycles(predecessors: &[Vec<usize>], depth: &mut [u32]) {
    for block in 0..predecessors.len() {
        if depth[block] > 0 {
            continue;
        }
        let mut seen = vec![false; predecessors.len()];
        let mut stack: Vec<usize> = predecessors[block].clone();
        let mut in_cycle = false;
        while let Some(node) = stack.pop() {
            if node == block {
                in_cycle = true;
                break;
            }
            if std::mem::replace(&mut seen[node], true) {
                continue;
            }
            stack.extend(predecessors[node].iter().copied());
        }
        if in_cycle {
            depth[block] = 1;
        }
    }
}

/// 把调用点 outgoing 区里的指针字记到对应的 `call` 点位上。
pub(crate) fn note_outgoing_roots(
    body: &Body,
    selected: &SelectedFunction,
    target: TargetName,
    live: &mut LiveInfo,
) -> Result<(), AllocError> {
    let mut site_index = 0_usize;
    for block in &selected.blocks {
        let lir = &body.blocks[block.id.index()];
        for site in &block.sites {
            let instruction = &body.instructions
                [usize::try_from(site.instruction).expect("站点指令编号适配 usize")];
            if let Op::Call(call) | Op::ForeignCall(call) = &instruction.op {
                attach_outgoing(
                    body,
                    call,
                    body.args(&instruction.arguments),
                    target,
                    &site.lowered,
                    site_index,
                    live,
                )?;
            }
            site_index += 1;
        }
        match &lir.terminator {
            Terminator::Invoke {
                call, arguments, ..
            }
            | Terminator::TailCall {
                call, arguments, ..
            } => {
                attach_outgoing(
                    body,
                    call,
                    body.args(arguments),
                    target,
                    &block.terminator,
                    site_index,
                    live,
                )?;
            }
            _ => {}
        }
        site_index += 1;
    }
    Ok(())
}

fn attach_outgoing(
    body: &Body,
    call: &crate::lir::body::Call,
    args: &[crate::lir::body::ValueId],
    target: TargetName,
    lowered: &Lowered,
    site_index: usize,
    live: &mut LiveInfo,
) -> Result<(), AllocError> {
    if !spills_pointers(call.safepoint_kind()) {
        return Ok(());
    }
    let layout = super::super::abi::classify_call(call, target)
        .map_err(|_| AllocError::new("调用 ABI 分类失败"))?;
    let mut roots = Vec::new();
    for argument in &layout.arguments {
        let Some(super::super::abi::AbiSlot::Stack { offset }) = argument.slot else {
            continue;
        };
        let Some(value) = args.get(usize::try_from(argument.index).expect("参数下标适配 usize"))
        else {
            continue;
        };
        let root = RootClass::of(body.values[value.index()].kind.provenance);
        if !root.must_spill() {
            continue;
        }
        roots.push(OutgoingRoot {
            value: value.0,
            root: root.code(),
            offset,
        });
    }
    if roots.is_empty() {
        return Ok(());
    }
    let range = live
        .sites
        .get(site_index)
        .ok_or_else(|| AllocError::new("outgoing 根缺少站点"))?
        .points
        .clone();
    let call_at = range.into_iter().find(|&index| {
        let point = &live.points[index as usize];
        let start = usize::try_from(point.instructions.start).expect("指令下标适配 usize");
        point.instructions.end == point.instructions.start.saturating_add(1)
            && lowered
                .sequence
                .instructions
                .get(start)
                .is_some_and(|inst| table::form(inst.form).mnemonic == "call")
    });
    let Some(index) = call_at else {
        return Err(AllocError::new("指针栈参数没有落到 call 点位"));
    };
    live.points[index as usize].outgoing_roots = roots;
    Ok(())
}
