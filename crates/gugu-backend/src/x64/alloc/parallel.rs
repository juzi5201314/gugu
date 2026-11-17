//! 物理并行拷贝解析：目标空闲顺序发射 + `r11`/16 字节 scratch 打断环。
//!
//! 语义仍是「所有读先于所有写」。解析分三步：
//!
//! 1. 把组的 `pairs` 映射成物理位置；可重建的源另记最终目标，等全部旧值搬运完再物化；
//! 2. 丢弃 `src == dst` 的自拷贝；
//! 3. 反复发射「目标不再是任何剩余 move 源」的 move；只剩环时取第一条 move 的**目标**，
//!    保存到 GPR scratch（`r11`）或为该函数预留的 16 字节 frame scratch，再把仍读该目标的
//!    源改读 scratch，环即被打断。内存目标的保存用 `r11` 中转两个机器字。
//!
//! scratch 不进入 stack map，也不参与分配：`r11` 不在池里，frame scratch 只在布局阶段
//! 按「解析是否真的用到了它」预留。

use super::super::lower::Lowered;
use super::super::reg::{Gpr, Reg, Xmm};
use super::super::select::{SelectedFunction, SiteKind};
use super::{AllocError, LiveInfo, Location, Placement, ValueAllocation, live};
use crate::lir::body::{Body, Type};

#[cfg(test)]
mod fs_copy_tests;

/// 一个物理位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Loc {
    Gpr(Gpr),
    Xmm(Xmm),
    /// spill slot 下标。
    Slot(u32),
    /// 为该函数预留的 16 字节 copy scratch（`[rsp + scratch_offset]`）。
    Scratch,
}

impl Loc {
    /// 位置写到的物理寄存器；内存位置返回 `None`。
    pub const fn register(self) -> Option<Reg> {
        match self {
            Self::Gpr(gpr) => Some(Reg::Gpr(gpr)),
            Self::Xmm(xmm) => Some(Reg::Xmm(xmm)),
            Self::Slot(_) | Self::Scratch => None,
        }
    }
}

/// 一条待发射的物理 move。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Move {
    pub src: Loc,
    pub dst: Loc,
    pub ty: Type,
}

/// 重发单元：位置搬运或常量重建。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmittedMove {
    Move {
        src: Loc,
        dst: Loc,
        ty: Type,
    },
    /// 重建常量/symbol/stack 地址到最终位置（该值的定义站点已被丢弃）。
    Rematerialize {
        value: u32,
        dest: Loc,
    },
}

/// 一个点位的解析结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedGroup {
    /// 片段 sites 下标；[`PREPEND_SITE`] 表示前插到入口块首个序列之前。
    pub site: u32,
    /// 点位下标；prologue 参数组用 [`PROLOGUE_POINT`]。
    pub point: u32,
    pub moves: Vec<EmittedMove>,
}

/// 全部组的解析结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolution {
    pub groups: Vec<ResolvedGroup>,
    pub uses_scratch: bool,
    pub cycles: u32,
    pub moves: u32,
}

impl Resolution {
    /// 取某个点位的解析结果。
    pub fn point(&self, point: u32) -> Option<&ResolvedGroup> {
        self.groups.iter().find(|group| group.point == point)
    }

    /// 取 prologue 参数组。
    pub fn prologue(&self) -> Option<&ResolvedGroup> {
        self.point(PROLOGUE_POINT)
    }
}

/// prologue 参数组的点位标识。
pub const PROLOGUE_POINT: u32 = u32::MAX;
/// 无 prologue 站点时参数组的落点：前插到入口块首个序列之前。
pub const PREPEND_SITE: u32 = u32::MAX;

/// 组的一侧：物理位置或需要先重建的可重建值。
enum Side {
    Loc(Loc),
    Rebuild { value: u32 },
}

/// 一个待解析的组。
struct PendingGroup {
    site: u32,
    point: u32,
    moves: Vec<Move>,
    /// `(值, 最终目标)`，全部旧值搬运结束后按出现顺序物化。
    materializations: Vec<(u32, Loc)>,
}

/// 收集全部待解析的并行拷贝组。
pub struct Request {
    groups: Vec<PendingGroup>,
    /// 参数组里落在 outgoing 栈上的入口参数（`(outgoing 偏移, 值, 类型)`）。
    pub stack_arguments: Vec<(u32, u32, Type)>,
}

impl Request {
    /// 收集站点拷贝组与 prologue 参数组。
    pub fn new(
        body: &Body,
        selected: &SelectedFunction,
        live: &LiveInfo,
        values: &[ValueAllocation],
    ) -> Result<Self, AllocError> {
        let mut groups = Vec::new();
        let mut site_index = 0_u32;
        let mut prologue_site = None;
        for block in &selected.blocks {
            let mut visit = |kind: SiteKind, lowered: &Lowered| -> Result<(), AllocError> {
                let entries = live::site_plan(lowered)?;
                let points = live
                    .sites
                    .get(site_index as usize)
                    .ok_or_else(|| AllocError::new("点位表缺少站点"))?;
                for (offset, entry) in entries.iter().enumerate() {
                    let Some(pairs) = &entry.pairs else {
                        continue;
                    };
                    let point =
                        points.points.start + u32::try_from(offset).expect("点位偏移适配 u32");
                    let (moves, materializations) = group_moves(pairs, values)?;
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
    pub fn resolve(&self, scratch: Option<u32>) -> Result<Resolution, AllocError> {
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

/// 一组的映射结果：串行 move 与需要先重建的 `(值, 重建目标)`。
type GroupMoves = (Vec<Move>, Vec<(u32, Loc)>);

/// 组 `pairs` 的物理位置映射与需要在组尾重建的源。
fn group_moves(
    pairs: &[super::super::copies::Copy],
    values: &[ValueAllocation],
) -> Result<GroupMoves, AllocError> {
    let mut moves = Vec::with_capacity(pairs.len());
    let mut materializations = Vec::new();
    for pair in pairs {
        let (src, dst) = (side(pair.src, values)?, side(pair.dest, values)?);
        let Side::Loc(dst) = dst else {
            return Err(AllocError::new("可重建值不能作为并行拷贝的目标"));
        };
        match src {
            Side::Loc(src) => moves.push(Move {
                src,
                dst,
                ty: pair.ty,
            }),
            Side::Rebuild { value } => materializations.push((value, dst)),
        }
    }
    Ok((moves, materializations))
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
                Placement::Rematerialize => Ok(Side::Rebuild { value: id }),
                Placement::Dead => Err(AllocError::new(format!("拷贝引用已死的值 v{id}"))),
            }
        }
    }
}

/// 分配位置 → 物理位置。
pub const fn loc_of(location: Location) -> Loc {
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

/// 一个组的解析：去自拷贝、目标空闲顺序发射、环打断，最后重建常量。
fn resolve_group(
    moves: &[Move],
    materializations: &[(u32, Loc)],
    scratch: Option<u32>,
) -> Result<(Vec<EmittedMove>, u32, bool), AllocError> {
    let mut emitted = Vec::with_capacity(moves.len() + materializations.len());
    let mut pending: Vec<Move> = moves.to_vec();
    let mut cycles = 0_u32;
    let mut uses_scratch = false;
    loop {
        pending.retain(|mov| mov.src != mov.dst);
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
            Loc::Gpr(_)
                if pending.iter().any(|mov| {
                    mov.src == Loc::Gpr(Gpr::R11)
                        || (matches!(mov.src, Loc::Slot(_) | Loc::Scratch)
                            && matches!(mov.dst, Loc::Slot(_) | Loc::Scratch))
                }) =>
            {
                // 剩余内存搬运会使用 `r11`，环上旧值改存 frame scratch。
                uses_scratch = true;
                if scratch.is_none() {
                    return Ok((emitted, cycles, true));
                }
                save_to_scratch(&mut emitted, saved, pending[0].ty);
                for mov in &mut pending {
                    if mov.src == saved {
                        mov.src = Loc::Scratch;
                    }
                }
            }
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
    emitted.extend(
        materializations
            .iter()
            .map(|&(value, dest)| EmittedMove::Rematerialize { value, dest }),
    );
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
