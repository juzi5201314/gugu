//! 线性扫描：候选过滤、位置选择与 spill 决策。
//!
//! # 候选集合
//!
//! - 通用 GPR 池（12 个）：`rax rcx rdx rbx rsi rdi r8 r9 r10 rbp r12 r13`；永不分配
//!   `rsp`、`r14`、`r15`、`r11`。XMM 池是 `xmm0..xmm15`。
//! - 值的区间只要覆盖任一点位，该点位的 mask（写 ∪ 读 ∪ call/bridge 的 caller-saved
//!   规则）就把对应寄存器从候选里去掉：跨 call 的 GPR 因此只剩 `rbp/r12/r13` 与 spill
//!   slot，跨 call 的 XMM 只剩 spill slot。**覆盖**的判定是 `use_slot <= end && def_slot >= start`：
//!   在 `2p` 被读走、此后死掉的值（例如调用实参的最后一次使用）不受该点位的写集影响。
//! - 跨 call（或挂起、bridge）活跃的 GPR 偏好序是 `rbp,r12,r13` 再接通用序。
//! - `Rm8`/`R8` 操作数位置的值只能落在 `is_byte_encodable()` 的寄存器；spill 到内存
//!   不受此限。
//! - 区间覆盖挂起点或 bridge 点时，managed/stack 指针**必须**落 spill slot：这些点位上
//!   runtime 要按 stack map 枚举根，指针不能只活在寄存器里。
//!
//! # 扫描
//!
//! 区间按 `(起点, 值编号)` 升序处理，维护按终点过期的 active 集合。候选为空时在
//! `{当前值} ∪ active` 中选 `weight / remaining_length` 最低者 spill（`u128` 交叉乘比较，
//! 相等时取 `(权重, 值编号)` 较小者），被淘汰的值让出寄存器，当前值重新计算候选。

use super::super::reg::{Clobbers, Gpr, Reg, Xmm};
use super::{
    AllocError, AllocationStats, Hint, LiveInfo, Location, Placement, RegClass, ValueAllocation,
    ValueLive,
};

/// 通用 GPR 池，顺序即不跨 call 时的偏好序。
pub(crate) const GPR_POOL: [Gpr; 12] = [
    Gpr::Rax,
    Gpr::Rcx,
    Gpr::Rdx,
    Gpr::Rbx,
    Gpr::Rsi,
    Gpr::Rdi,
    Gpr::R8,
    Gpr::R9,
    Gpr::R10,
    Gpr::Rbp,
    Gpr::R12,
    Gpr::R13,
];

/// 跨 call/bridge 活跃的 GPR 偏好序前缀：内部 ABI 的 callee-saved 集合。
pub(crate) const CROSS_CALL_GPR: [Gpr; 3] = [Gpr::Rbp, Gpr::R12, Gpr::R13];

/// XMM 池，顺序即偏好序。
pub(crate) const XMM_POOL: [Xmm; 16] = [
    Xmm::Xmm0,
    Xmm::Xmm1,
    Xmm::Xmm2,
    Xmm::Xmm3,
    Xmm::Xmm4,
    Xmm::Xmm5,
    Xmm::Xmm6,
    Xmm::Xmm7,
    Xmm::Xmm8,
    Xmm::Xmm9,
    Xmm::Xmm10,
    Xmm::Xmm11,
    Xmm::Xmm12,
    Xmm::Xmm13,
    Xmm::Xmm14,
    Xmm::Xmm15,
];

/// 扫描结果：逐值分配与统计。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Scanned {
    pub(crate) values: Vec<ValueAllocation>,
    pub(crate) stats: AllocationStats,
}

/// 扫描一个函数的全部值。
pub(crate) fn scan(live: &LiveInfo) -> Result<Scanned, AllocError> {
    let mut values: Vec<ValueAllocation> = live
        .values
        .iter()
        .enumerate()
        .map(|(index, value)| ValueAllocation {
            value: u32::try_from(index).expect("值编号适配 u32"),
            class: value.class,
            root: value.root,
            range: (value.start, value.end),
            placement: if value.is_live() {
                Placement::Location(Location::Slot(u32::MAX))
            } else {
                Placement::Dead
            },
        })
        .collect();
    let mut order: Vec<usize> = (0..values.len())
        .filter(|index| values[*index].is_live())
        .collect();
    order.sort_by_key(|index| (values[*index].range.0, values[*index].value));
    let blocked = blocked_masks(live);
    let mut gpr_owner: [Option<usize>; 16] = [None; 16];
    let mut xmm_owner: [Option<usize>; 16] = [None; 16];
    let mut stats = AllocationStats::default();
    for index in order {
        let value = &live.values[index];
        let (start, end) = (value.start, value.end);
        expire(values.as_slice(), start, &mut gpr_owner);
        expire(values.as_slice(), start, &mut xmm_owner);
        let cross_call = cross_call_range(live, start, end);
        let mut assigned = None;
        if !blocked[index].forced_slot {
            assigned = pick(
                Candidate {
                    class: value.class,
                    mask: &blocked[index].mask,
                    byte_operand: value.byte_operand,
                    cross_call,
                },
                &gpr_owner,
                &xmm_owner,
                hint_target(
                    value,
                    values.as_slice(),
                    &gpr_owner,
                    &xmm_owner,
                    &blocked[index].mask,
                ),
            );
            while assigned.is_none() {
                // 寄存器不足：在 `{当前值} ∪ active` 中淘汰权重密度最低者。
                let victim = victim_of(index, live, values.as_slice(), &gpr_owner, &xmm_owner);
                if victim == index {
                    break;
                }
                spill(&mut values, victim, &mut gpr_owner, &mut xmm_owner);
                assigned = pick(
                    Candidate {
                        class: value.class,
                        mask: &blocked[index].mask,
                        byte_operand: value.byte_operand,
                        cross_call,
                    },
                    &gpr_owner,
                    &xmm_owner,
                    hint_target(
                        value,
                        values.as_slice(),
                        &gpr_owner,
                        &xmm_owner,
                        &blocked[index].mask,
                    ),
                );
            }
        }
        match assigned {
            Some(Location::Gpr(gpr)) => {
                values[index].placement = Placement::Location(Location::Gpr(gpr));
                gpr_owner[gpr.code() as usize] = Some(index);
            }
            Some(Location::Xmm(xmm)) => {
                values[index].placement = Placement::Location(Location::Xmm(xmm));
                xmm_owner[xmm.code() as usize] = Some(index);
            }
            Some(Location::Slot(_)) => return Err(AllocError::new("候选集合只能给出寄存器位置")),
            None if value.rematerializable => {
                values[index].placement = Placement::Rematerialize;
            }
            None => {
                values[index].placement = Placement::Location(Location::Slot(u32::MAX));
                if blocked[index].forced_slot {
                    stats.safepoint_spills = stats.safepoint_spills.saturating_add(1);
                }
            }
        }
        stats.allocated_values = stats.allocated_values.saturating_add(1);
        let occupied = |owners: &[Option<usize>; 16]| {
            u32::try_from(owners.iter().filter(|slot| slot.is_some()).count())
                .expect("占用数适配 u32")
        };
        if value.class == RegClass::Gpr {
            stats.peak_live_gpr = stats.peak_live_gpr.max(occupied(&gpr_owner));
        } else {
            stats.peak_live_xmm = stats.peak_live_xmm.max(occupied(&xmm_owner));
        }
    }
    stats.call_sites = u32::try_from(
        live.points
            .iter()
            .filter(|point| point.kind.is_call_or_bridge())
            .count(),
    )
    .expect("点位数适配 u32");
    Ok(Scanned { values, stats })
}

/// 让区间已结束的 active 值释放寄存器。
fn expire(values: &[ValueAllocation], start: u32, owners: &mut [Option<usize>; 16]) {
    for owner in owners.iter_mut() {
        if let Some(other) = *owner
            && values[other].range.1 < start
        {
            *owner = None;
        }
    }
}

/// 点位的 mask 与强制 spill 标记。
struct Blocked {
    mask: Clobbers,
    forced_slot: bool,
}

/// 把区间覆盖到的全部点位折进一个 mask；指针覆盖挂起/bridge 点时强制落槽。
fn blocked_masks(live: &LiveInfo) -> Vec<Blocked> {
    live.values
        .iter()
        .map(|value| {
            let mut mask = Clobbers::NONE;
            let mut forced_slot = false;
            if !value.is_live() {
                return Blocked { mask, forced_slot };
            }
            for point in live
                .points
                .iter()
                .take_while(|point| point.use_slot <= value.end + 1)
            {
                if point.use_slot > value.end || point.def_slot < value.start {
                    continue;
                }
                mask = mask.union(point.mask);
                if point.pointer_spill && value.root.must_spill() {
                    forced_slot = true;
                }
            }
            Blocked { mask, forced_slot }
        })
        .collect()
}

/// 区间是否覆盖 call/bridge 点位：决定 GPR 偏好序前缀。
fn cross_call_range(live: &LiveInfo, start: u32, end: u32) -> bool {
    live.points
        .iter()
        .take_while(|point| point.use_slot <= end + 1)
        .any(|point| {
            point.kind.is_call_or_bridge() && point.use_slot <= end && point.def_slot >= start
        })
}

/// 候选过滤的固定输入。
struct Candidate<'a> {
    class: RegClass,
    mask: &'a Clobbers,
    byte_operand: bool,
    cross_call: bool,
}

/// 候选选择：提示优先，否则取偏好序第一个可用寄存器。
fn pick(
    candidate: Candidate<'_>,
    gpr_owner: &[Option<usize>; 16],
    xmm_owner: &[Option<usize>; 16],
    hint: Option<Location>,
) -> Option<Location> {
    let available = |location: Location| -> bool {
        match location {
            Location::Gpr(gpr) => {
                candidate.mask.gpr & gpr.bit() == 0
                    && gpr_owner[gpr.code() as usize].is_none()
                    && (!candidate.byte_operand || gpr.is_byte_encodable())
            }
            Location::Xmm(xmm) => {
                candidate.mask.xmm & xmm.bit() == 0 && xmm_owner[xmm.code() as usize].is_none()
            }
            Location::Slot(_) => false,
        }
    };
    if let Some(hint) = hint
        && class_of(hint) == candidate.class
        && available(hint)
    {
        return Some(hint);
    }
    match candidate.class {
        RegClass::Gpr => {
            let preferred = || CROSS_CALL_GPR.iter().copied();
            let generic = || GPR_POOL.iter().copied();
            let order: Box<dyn Iterator<Item = Gpr>> = if candidate.cross_call {
                Box::new(preferred().chain(generic()))
            } else {
                Box::new(generic())
            };
            order
                .map(Location::Gpr)
                .find(|location| available(*location))
        }
        RegClass::Xmm => XMM_POOL
            .iter()
            .copied()
            .map(Location::Xmm)
            .find(|location| available(*location)),
    }
}

/// 位置的 bank。
const fn class_of(location: Location) -> RegClass {
    match location {
        Location::Gpr(_) => RegClass::Gpr,
        Location::Xmm(_) | Location::Slot(_) => RegClass::Xmm,
    }
}

/// 第一个可用的拷贝提示。
fn hint_target(
    value: &ValueLive,
    values: &[ValueAllocation],
    gpr_owner: &[Option<usize>; 16],
    xmm_owner: &[Option<usize>; 16],
    mask: &Clobbers,
) -> Option<Location> {
    let free = |location: Location| -> bool {
        let occupied = match location {
            Location::Gpr(gpr) => gpr_owner[gpr.code() as usize].is_some(),
            Location::Xmm(xmm) => xmm_owner[xmm.code() as usize].is_some(),
            Location::Slot(_) => true,
        };
        !occupied && location_clear(location, mask)
    };
    for hint in &value.hints {
        match *hint {
            Hint::Register(reg, _) => {
                let location = match reg {
                    Reg::Gpr(gpr) => Location::Gpr(gpr),
                    Reg::Xmm(xmm) => Location::Xmm(xmm),
                    Reg::Virtual(_) => continue,
                };
                if free(location) {
                    return Some(location);
                }
            }
            Hint::SameAs(source, _) => {
                if let Some(location) = values
                    .get(source as usize)
                    .and_then(ValueAllocation::location)
                    && free(location)
                {
                    return Some(location);
                }
            }
        }
    }
    None
}

/// 位置是否落在 mask 之外。
const fn location_clear(location: Location, mask: &Clobbers) -> bool {
    match location {
        Location::Gpr(gpr) => mask.gpr & gpr.bit() == 0,
        Location::Xmm(xmm) => mask.xmm & xmm.bit() == 0,
        Location::Slot(_) => true,
    }
}

/// 淘汰决策：`{当前值} ∪ active` 中 `weight / remaining_length` 最低者。
fn victim_of(
    index: usize,
    live: &LiveInfo,
    values: &[ValueAllocation],
    gpr_owner: &[Option<usize>; 16],
    xmm_owner: &[Option<usize>; 16],
) -> usize {
    let mut victim = index;
    let mut best = (
        live.values[index].weight,
        remaining(live.values[index].start, live.values[index].end),
    );
    let mut candidates: Vec<usize> = gpr_owner
        .iter()
        .chain(xmm_owner.iter())
        .flatten()
        .copied()
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    for candidate in candidates {
        if values[candidate].location().is_none() {
            continue;
        }
        let value = &live.values[candidate];
        let key = (value.weight, remaining(value.start, value.end));
        if density_less(key, best) || (key == best && candidate > victim) {
            best = key;
            victim = candidate;
        }
    }
    victim
}

/// 区间长度（slot 数）。
fn remaining(start: u32, end: u32) -> u128 {
    u128::from(end.saturating_sub(start)) + 1
}

/// `weight / remaining` 的交叉乘比较：返回 `left < right`。
fn density_less(left: (u64, u128), right: (u64, u128)) -> bool {
    u128::from(left.0) * right.1 < u128::from(right.0) * left.1
}

/// 把一个值改成 spill。
fn spill(
    values: &mut [ValueAllocation],
    index: usize,
    gpr_owner: &mut [Option<usize>; 16],
    xmm_owner: &mut [Option<usize>; 16],
) {
    match values[index].location() {
        Some(Location::Gpr(gpr)) => gpr_owner[gpr.code() as usize] = None,
        Some(Location::Xmm(xmm)) => xmm_owner[xmm.code() as usize] = None,
        Some(Location::Slot(_)) | None => {}
    }
    values[index].placement = Placement::Location(Location::Slot(u32::MAX));
}
