//! 物理并行拷贝解析：目标空闲顺序发射 + `r11`/16 字节 scratch 打断环。
//!
//! 语义仍是「所有读先于所有写」。解析分三步：
//!
//! 1. 把组的 `pairs` 映射成物理位置；源是 spilled 且可重建的值先在组前物化进一个
//!    该点位空闲、不在点位 mask 内、且不在本组任何源/目标里的 scratch 寄存器；
//! 2. 丢弃 `src == dst` 的自拷贝；
//! 3. 反复发射「目标不再是任何剩余 move 源」的 move；只剩环时取第一条 move 的**目标**，
//!    保存到 GPR scratch（`r11`）或为该函数预留的 16 字节 frame scratch，再把仍读该目标的
//!    源改读 scratch，环即被打断。内存目标的保存用 `r11` 中转两个机器字。
//!
//! scratch 不进入 stack map，也不参与分配：`r11` 不在池里，frame scratch 只在布局阶段
//! 按「解析是否真的用到了它」预留。

use super::super::lower::Lowered;
use super::super::reg::{Clobbers, Gpr, Reg, Xmm};
use super::super::select::{SelectedFunction, SiteKind};
use super::{AllocError, LiveInfo, Location, Placement, RegClass, ValueAllocation, live};
use crate::lir::body::{Body, Type};

/// 一个物理位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Loc {
    Gpr(Gpr),
    Xmm(Xmm),
    /// spill slot 下标。
    Slot(u32),
    /// 为该函数预留的 16 字节 copy scratch（`[rsp + scratch_offset]`）。
    Scratch,
}

impl Loc {
    /// 位置写到的物理寄存器；内存位置返回 `None`。
    pub(crate) const fn register(self) -> Option<Reg> {
        match self {
            Self::Gpr(gpr) => Some(Reg::Gpr(gpr)),
            Self::Xmm(xmm) => Some(Reg::Xmm(xmm)),
            Self::Slot(_) | Self::Scratch => None,
        }
    }
}

/// 一条待发射的物理 move。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Move {
    pub(crate) src: Loc,
    pub(crate) dst: Loc,
    pub(crate) ty: Type,
}

/// 重发单元：位置搬运或常量重建。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmittedMove {
    Move {
        src: Loc,
        dst: Loc,
        ty: Type,
    },
    /// 重建常量/symbol/stack 地址到寄存器（该值的定义站点已被丢弃）。
    Rematerialize {
        value: u32,
        dest: Loc,
    },
}

/// 一个点位的解析结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedGroup {
    /// 片段 sites 下标；[`PREPEND_SITE`] 表示前插到入口块首个序列之前。
    pub(crate) site: u32,
    /// 点位下标；prologue 参数组用 [`PROLOGUE_POINT`]。
    pub(crate) point: u32,
    pub(crate) moves: Vec<EmittedMove>,
}

/// 全部组的解析结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Resolution {
    pub(crate) groups: Vec<ResolvedGroup>,
    pub(crate) uses_scratch: bool,
    pub(crate) cycles: u32,
    pub(crate) moves: u32,
}

impl Resolution {
    /// 取某个点位的解析结果。
    pub(crate) fn point(&self, point: u32) -> Option<&ResolvedGroup> {
        self.groups.iter().find(|group| group.point == point)
    }

    /// 取 prologue 参数组。
    pub(crate) fn prologue(&self) -> Option<&ResolvedGroup> {
        self.point(PROLOGUE_POINT)
    }
}

/// prologue 参数组的点位标识。
pub(crate) const PROLOGUE_POINT: u32 = u32::MAX;
/// 无 prologue 站点时参数组的落点：前插到入口块首个序列之前。
pub(crate) const PREPEND_SITE: u32 = u32::MAX;

/// 组的一侧：物理位置或需要先重建的可重建值。
enum Side {
    Loc(Loc),
    Rebuild { value: u32, class: RegClass },
}

/// 一个待解析的组。
struct PendingGroup {
    site: u32,
    point: u32,
    moves: Vec<Move>,
    /// `(值, 物化目标)`，按出现顺序在组前发射。
    materializations: Vec<(u32, Loc)>,
}

/// 收集全部待解析的并行拷贝组。
pub(crate) struct Request {
    groups: Vec<PendingGroup>,
    /// 参数组里落在 outgoing 栈上的入口参数（`(outgoing 偏移, 值, 类型)`）。
    pub(crate) stack_arguments: Vec<(u32, u32, Type)>,
}

impl Request {
    /// 收集站点拷贝组与 prologue 参数组。
    pub(crate) fn new(
        body: &Body,
        selected: &SelectedFunction,
        live: &LiveInfo,
        values: &[ValueAllocation],
    ) -> Result<Self, AllocError> {
        let mut groups = Vec::new();
        let mut site_index = 0_u32;
        let mut prologue_site = None;
        let mut entry_free = Clobbers::NONE;
        for block in &selected.blocks {
            let mut visit = |kind: SiteKind, lowered: &Lowered| -> Result<(), AllocError> {
                let entries = live::site_plan(lowered)?;
                let points = live
                    .sites
                    .get(site_index as usize)
                    .ok_or_else(|| AllocError::new("点位表缺少站点"))?;
                if entry_free == Clobbers::NONE && points.points.start < points.points.end {
                    entry_free = point_free(live, values, points.points.start);
                }
                for (offset, entry) in entries.iter().enumerate() {
                    let Some(pairs) = &entry.pairs else {
                        continue;
                    };
                    let point =
                        points.points.start + u32::try_from(offset).expect("点位偏移适配 u32");
                    let free = point_free(live, values, point);
                    let (moves, materializations) = group_moves(pairs, values, free)?;
                    groups.push(PendingGroup {
                        site: site_index,
                        point,
                        moves,
                        materializations,
                    });
                }
                if kind == SiteKind::Prologue {
                    prologue_site = Some(site_index);
                }
                site_index += 1;
                Ok(())
            };
            for site in &block.sites {
                visit(site.kind, &site.lowered)?;
            }
            visit(SiteKind::Normal, &block.terminator)?;
        }
        let stack_arguments = stack_arguments(body, selected);
        let (moves, materializations) = prologue_moves(body, selected, values)?;
        groups.push(PendingGroup {
            site: prologue_site.unwrap_or(PREPEND_SITE),
            point: PROLOGUE_POINT,
            moves,
            materializations,
        });
        Ok(Self {
            groups,
            stack_arguments,
        })
    }

    /// 解析全部组；`scratch` 是 frame 为 16 字节 copy scratch 预留的偏移。
    pub(crate) fn resolve(&self, scratch: Option<u32>) -> Result<Resolution, AllocError> {
        let mut groups = Vec::with_capacity(self.groups.len());
        let mut uses_scratch = false;
        let mut cycles = 0_u32;
        let mut moves = 0_u32;
        for group in &self.groups {
            let (emitted, group_cycles, group_scratch) =
                resolve_group(&group.moves, &group.materializations, scratch)?;
            uses_scratch |= group_scratch;
            cycles = cycles.saturating_add(group_cycles);
            moves = moves.saturating_add(u32::try_from(emitted.len()).expect("拷贝数适配 u32"));
            groups.push(ResolvedGroup {
                site: group.site,
                point: group.point,
                moves: emitted,
            });
        }
        Ok(Resolution {
            groups,
            uses_scratch,
            cycles,
            moves,
        })
    }
}

/// 点位空闲的寄存器：池减去活跃值占用与点位 mask。
///
/// 物化与环打断都在**该点位**上插入指令，所以只看这一个点位的 mask 与占用：按整段站点
/// 合并会误把站点内其它指令破坏的寄存器也算进来，常见形态下会把 scratch 池清空。
fn point_free(live: &LiveInfo, values: &[ValueAllocation], point: u32) -> Clobbers {
    let mask = live
        .points
        .get(point as usize)
        .map_or(Clobbers::NONE, |point| point.mask);
    let mut occupied = Clobbers::NONE;
    for value in values {
        if !value.is_live() || value.range.0 > point || value.range.1 < point {
            continue;
        }
        match value.location() {
            Some(Location::Gpr(gpr)) => occupied = occupied.union(Clobbers::gpr(gpr)),
            Some(Location::Xmm(xmm)) => occupied = occupied.union(Clobbers::xmm(xmm)),
            Some(Location::Slot(_)) | None => {}
        }
    }
    let pool_gpr = super::scan::GPR_POOL
        .iter()
        .fold(0_u32, |bits, gpr| bits | gpr.bit());
    let pool_xmm = super::scan::XMM_POOL
        .iter()
        .fold(0_u32, |bits, xmm| bits | xmm.bit());
    Clobbers {
        // `r11` 留给环打断与指令级 reload scratch，不作物化目标。
        gpr: pool_gpr & !occupied.gpr & !mask.gpr & !Gpr::R11.bit(),
        xmm: pool_xmm & !occupied.xmm & !mask.xmm,
    }
}

/// 一组的映射结果：串行 move 与需要先重建的 `(值, 重建目标)`。
type GroupMoves = (Vec<Move>, Vec<(u32, Loc)>);

/// 组 `pairs` 的物理位置映射与需要重建的源。
fn group_moves(
    pairs: &[super::super::copies::Copy],
    values: &[ValueAllocation],
    free: Clobbers,
) -> Result<GroupMoves, AllocError> {
    let mut moves = Vec::with_capacity(pairs.len());
    let mut materializations: Vec<(u32, Loc)> = Vec::new();
    let mut touched = Clobbers::NONE;
    let mut sides: Vec<(Side, Loc, Type)> = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let (src, dst) = (side(pair.src, values)?, side(pair.dest, values)?);
        if let Side::Loc(location) = src {
            touched = touched.union(bits_of(location));
        }
        if let Side::Loc(location) = dst {
            touched = touched.union(bits_of(location));
        }
        let dst = match dst {
            Side::Loc(location) => location,
            Side::Rebuild { value, .. } => {
                return Err(AllocError::new(format!(
                    "可重建值 v{value} 不能作为并行拷贝的目标"
                )));
            }
        };
        sides.push((src, dst, pair.ty));
    }
    for (src, dst, ty) in sides {
        let src = match src {
            Side::Loc(location) => location,
            Side::Rebuild { value, class } => {
                let dest = pick_rebuild(class, free, touched)?;
                touched = touched.union(bits_of(dest));
                materializations.push((value, dest));
                dest
            }
        };
        moves.push(Move { src, dst, ty });
    }
    Ok((moves, materializations))
}

/// 物化 scratch：按 bank 偏好序取第一个既空闲、又不在本组任何源/目标里的寄存器。
fn pick_rebuild(class: RegClass, free: Clobbers, touched: Clobbers) -> Result<Loc, AllocError> {
    match class {
        RegClass::Gpr => super::scan::GPR_POOL
            .iter()
            .copied()
            .find(|gpr| free.gpr & gpr.bit() != 0 && touched.gpr & gpr.bit() == 0)
            .map(Loc::Gpr)
            .ok_or_else(|| AllocError::new("并行拷贝组里没有可用的常量重建 scratch")),
        RegClass::Xmm => super::scan::XMM_POOL
            .iter()
            .copied()
            .find(|xmm| free.xmm & xmm.bit() != 0 && touched.xmm & xmm.bit() == 0)
            .map(Loc::Xmm)
            .ok_or_else(|| AllocError::new("并行拷贝组里没有可用的常量重建 scratch")),
    }
}

/// 物理寄存器的位图。
const fn bits_of(location: Loc) -> Clobbers {
    match location {
        Loc::Gpr(gpr) => Clobbers::gpr(gpr),
        Loc::Xmm(xmm) => Clobbers::xmm(xmm),
        Loc::Slot(_) | Loc::Scratch => Clobbers::NONE,
    }
}

/// 把组的一侧映射成物理位置或待重建值。
fn side(reg: Reg, values: &[ValueAllocation]) -> Result<Side, AllocError> {
    match reg {
        Reg::Gpr(gpr) => Ok(Side::Loc(Loc::Gpr(gpr))),
        Reg::Xmm(xmm) => Ok(Side::Loc(Loc::Xmm(xmm))),
        Reg::Virtual(id) => {
            let index = usize::try_from(id).map_err(|_| AllocError::new("虚拟编号超出宿主范围"))?;
            let value = values
                .get(index)
                .ok_or_else(|| AllocError::new(format!("拷贝引用未登记的值 v{id}")))?;
            match value.placement {
                Placement::Location(location) => Ok(Side::Loc(loc_of(location))),
                Placement::Rematerialize => Ok(Side::Rebuild {
                    value: id,
                    class: value.class,
                }),
                Placement::Dead => Err(AllocError::new(format!("拷贝引用已死的值 v{id}"))),
            }
        }
    }
}

/// 分配位置 → 物理位置。
pub(crate) const fn loc_of(location: Location) -> Loc {
    match location {
        Location::Gpr(gpr) => Loc::Gpr(gpr),
        Location::Xmm(xmm) => Loc::Xmm(xmm),
        Location::Slot(slot) => Loc::Slot(slot),
    }
}

/// prologue 参数组的 moves：入口参数按 ABI 槽落位。
///
/// 入口 block 的参数 0 是每个 block 都有的 Mem 伪参数（`source` 为 `None`），ABI 参数
/// 从位置 1 起、与 `AbiLayout::arguments` 按序对应。
type PrologueEntry = (Vec<Move>, Vec<(u32, Loc)>);

fn prologue_moves(
    body: &Body,
    selected: &SelectedFunction,
    values: &[ValueAllocation],
) -> Result<PrologueEntry, AllocError> {
    let mut moves = Vec::new();
    for (position, parameter) in body.params(body.entry).iter().enumerate() {
        let Some(index) = position.checked_sub(1) else {
            continue;
        };
        let Some(argument) = selected.abi.arguments.get(index) else {
            return Err(AllocError::new(format!(
                "入口参数 {index} 在 ABI 布局里没有对应槽"
            )));
        };
        if argument.index != u32::try_from(index).expect("参数下标适配 u32") {
            return Err(AllocError::new("入口参数与 ABI 布局顺序不一致"));
        }
        let Some(slot) = argument.slot else {
            continue;
        };
        // 未使用的入口参数没有活跃区间，不产生落位拷贝。
        if values
            .get(parameter.value.index())
            .is_none_or(|value| !value.is_live())
        {
            continue;
        }
        let dst = side(Reg::Virtual(parameter.value.0), values)?;
        let Side::Loc(dst) = dst else {
            return Err(AllocError::new("入口参数值不可能是可重建值"));
        };
        let src = match slot {
            super::super::abi::AbiSlot::Integer(gpr) => Loc::Gpr(gpr),
            super::super::abi::AbiSlot::Float(xmm) => Loc::Xmm(xmm),
            super::super::abi::AbiSlot::Stack { .. } => continue,
        };
        moves.push(Move {
            src,
            dst,
            ty: argument.ty.ty,
        });
    }
    Ok((moves, Vec::new()))
}

/// 落在 outgoing 栈上的入口参数：`(outgoing 偏移, 值, 类型)`。
fn stack_arguments(body: &Body, selected: &SelectedFunction) -> Vec<(u32, u32, Type)> {
    let mut stack = Vec::new();
    for (position, parameter) in body.params(body.entry).iter().enumerate() {
        let Some(index) = position.checked_sub(1) else {
            continue;
        };
        let Some(argument) = selected.abi.arguments.get(index) else {
            continue;
        };
        if let Some(super::super::abi::AbiSlot::Stack { offset }) = argument.slot {
            stack.push((offset, parameter.value.0, argument.ty.ty));
        }
    }
    stack
}

/// 一个组的解析：物化、去自拷贝、目标空闲顺序发射与环打断。
fn resolve_group(
    moves: &[Move],
    materializations: &[(u32, Loc)],
    scratch: Option<u32>,
) -> Result<(Vec<EmittedMove>, u32, bool), AllocError> {
    let mut emitted: Vec<EmittedMove> = materializations
        .iter()
        .map(|(value, dest)| EmittedMove::Rematerialize {
            value: *value,
            dest: *dest,
        })
        .collect();
    let mut pending: Vec<Move> = moves.to_vec();
    let mut cycles = 0_u32;
    let mut uses_scratch = false;
    loop {
        pending
            .retain(|mov| !(mov.src == mov.dst && !matches!(mov.dst, Loc::Slot(_) | Loc::Scratch)));
        if pending.is_empty() {
            break;
        }
        let ready = pending
            .iter()
            .position(|candidate| !pending.iter().any(|other| other.src == candidate.dst));
        if let Some(index) = ready {
            let mov = pending.remove(index);
            emitted.push(EmittedMove::Move {
                src: mov.src,
                dst: mov.dst,
                ty: mov.ty,
            });
            continue;
        }
        // 环：保存第一条 move 的目标，再把仍读该目标的源改读 scratch。
        let saved = pending[0].dst;
        cycles += 1;
        match saved {
            Loc::Gpr(_) => {
                let holder = Loc::Gpr(Gpr::R11);
                emitted.push(EmittedMove::Move {
                    src: saved,
                    dst: holder,
                    ty: pending[0].ty,
                });
                for mov in &mut pending {
                    if mov.src == saved {
                        mov.src = holder;
                    }
                }
            }
            Loc::Xmm(_) | Loc::Slot(_) | Loc::Scratch => {
                uses_scratch = true;
                if scratch.is_none() {
                    // 探测阶段：只需知道需要预留 16 字节 scratch。
                    return Ok((emitted, cycles, true));
                }
                save_to_scratch(&mut emitted, saved, pending[0].ty);
                for mov in &mut pending {
                    if mov.src == saved {
                        mov.src = Loc::Scratch;
                    }
                }
            }
        }
    }
    Ok((emitted, cycles, uses_scratch))
}

/// 把环上的目标保存到 16 字节 scratch：XMM 一次 `movups`，其余经 `r11` 搬两个机器字。
fn save_to_scratch(emitted: &mut Vec<EmittedMove>, saved: Loc, ty: Type) {
    match saved {
        Loc::Xmm(_) => emitted.push(EmittedMove::Move {
            src: saved,
            dst: Loc::Scratch,
            ty,
        }),
        Loc::Gpr(_) | Loc::Slot(_) | Loc::Scratch => {
            emitted.push(EmittedMove::Move {
                src: saved,
                dst: Loc::Gpr(Gpr::R11),
                ty: Type::I64,
            });
            emitted.push(EmittedMove::Move {
                src: Loc::Gpr(Gpr::R11),
                dst: Loc::Scratch,
                ty: Type::I64,
            });
        }
    }
}
