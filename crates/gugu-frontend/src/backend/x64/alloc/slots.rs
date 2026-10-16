//! spill slot 的分组复用。
//!
//! 槽按 `(size, align, root_class)` 分组：GPR（含全部 I8/I16/I32/I64/Ptr）是 8 字节 8 对齐，
//! XMM（F32/F64/V128）是 16 字节 16 对齐；heap pointer、stack pointer 与 non-pointer 分属
//! 不同的分组，绝不共用槽。复用按 `(区间起点, 值编号)` 升序处理：每个请求在所属分组内按
//! offset 升序找第一个占用区间与它不相交的槽，找不到才追加新槽。槽内偏移由 frame layout
//! 按分组顺序累加，这里只产出组内下标。

use super::{AllocationStats, Location, Placement, RegClass, RootClass, ValueAllocation};

/// 一个 spill slot：组内下标由列表位置给出，`offset` 在 frame layout 阶段填充。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SpillSlot {
    /// 相对 frame base 的字节偏移（frame layout 填充）。
    pub(crate) offset: u32,
    pub(crate) size: u32,
    pub(crate) align: u32,
    pub(crate) root: RootClass,
    /// 该槽承载的区间，按起点升序（供 frame layout 与校验复用）。
    pub(crate) ranges: Vec<(u32, u32)>,
}

/// 全部 spill slot 与分组占用。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Slots {
    pub(crate) slots: Vec<SpillSlot>,
    pub(crate) spill_bytes: u32,
}

/// 分组键：`(size, align, root_class)`。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Group {
    size: u32,
    root: u8,
}

impl Group {
    fn of(class: RegClass, root: RootClass) -> Self {
        Self {
            size: match class {
                RegClass::Gpr => 8,
                RegClass::Xmm => 16,
            },
            root: root.code(),
        }
    }
}

/// 为所有落 slot 的值分配槽位，并把槽下标写回分配结果。
pub(crate) fn assign(values: &mut [ValueAllocation]) -> Slots {
    let mut groups: Vec<(Group, Vec<usize>)> = Vec::new();
    let mut requests: Vec<usize> = values
        .iter()
        .enumerate()
        .filter(|(_, value)| matches!(value.placement, Placement::Location(Location::Slot(_))))
        .map(|(index, _)| index)
        .collect();
    requests.sort_by_key(|index| (values[*index].range.0, values[*index].value));
    let mut slots: Vec<SpillSlot> = Vec::new();
    for index in requests {
        let value = &values[index];
        let group = Group::of(value.class, value.root);
        let entry = match groups.iter_mut().find(|(key, _)| *key == group) {
            Some((_, list)) => list,
            None => {
                groups.push((group, Vec::new()));
                &mut groups.last_mut().expect("刚压入分组").1
            }
        };
        let range = value.range;
        let picked = entry.iter().copied().find(|slot| {
            slots[*slot]
                .ranges
                .iter()
                .all(|(start, end)| *end < range.0 || *start > range.1)
        });
        let slot = match picked {
            Some(slot) => slot,
            None => {
                slots.push(SpillSlot {
                    offset: 0,
                    size: group.size,
                    align: group.size,
                    root: value.root,
                    ranges: Vec::new(),
                });
                let slot = slots.len() - 1;
                entry.push(slot);
                slot
            }
        };
        slots[slot].ranges.push(range);
        values[index].placement = Placement::Location(Location::Slot(
            u32::try_from(slot).expect("spill slot 数适配 u32"),
        ));
    }
    let mut total = 0_u32;
    for slot in &slots {
        total = total.saturating_add(slot.size);
    }
    Slots {
        slots,
        spill_bytes: total,
    }
}

/// 统计里与槽相关的部分。
pub(crate) fn statistics(slots: &Slots, stats: &mut AllocationStats) {
    stats.spill_slot_count = u32::try_from(slots.slots.len()).expect("spill slot 数适配 u32");
    stats.spill_bytes = slots.spill_bytes;
}
