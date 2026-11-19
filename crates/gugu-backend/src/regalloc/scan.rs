//! Deterministic weighted linear scan; one convex interval per value.

use super::{
    Class, Error, Hint, LiveInfo, Location, PhysReg, Placement, RegSet, RegisterFile, Value,
    ValueAllocation,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Statistics {
    pub peaks: Vec<(Class, u32)>,
    pub call_sites: u32,
    pub safepoint_spills: u32,
    pub allocated_values: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scanned {
    pub values: Vec<ValueAllocation>,
    pub stats: Statistics,
}

pub fn scan(live: &LiveInfo, regs: &impl RegisterFile) -> Result<Scanned, Error> {
    if live
        .points
        .windows(2)
        .any(|p| p[0].use_slot > p[1].use_slot || p[0].def_slot > p[1].def_slot)
    {
        return Err(Error::new("allocation points are not ordered"));
    }
    let mut values = live
        .values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            Ok(ValueAllocation {
                value: u32::try_from(index)
                    .map_err(|_| Error::new("too many allocation values"))?,
                class: value.class,
                root: value.root,
                range: value.range,
                placement: if value.range.0 <= value.range.1 {
                    Placement::Location(Location::Slot(u32::MAX))
                } else {
                    Placement::Dead
                },
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    for value in &live.values {
        for crosses in [false, true] {
            for reg in regs.pool(value.class, crosses) {
                RegSet::single(*reg)?;
                if regs.class(*reg) != value.class {
                    return Err(Error::new("register pool has the wrong class"));
                }
            }
        }
    }
    let mut order = (0..values.len())
        .filter(|i| values[*i].range.0 <= values[*i].range.1)
        .collect::<Vec<_>>();
    order.sort_by_key(|i| (values[*i].range.0, values[*i].value));
    let blocked = blocked_masks(live);
    let mut owners: [Option<usize>; 64] = [None; 64];
    let mut stats = Statistics::default();
    for index in order {
        let value = &live.values[index];
        let (start, end) = value.range;
        for owner in &mut owners {
            if owner.is_some_and(|other| values[other].range.1 < start) {
                *owner = None;
            }
        }
        let first = live.points.partition_point(|p| p.def_slot < start);
        let cross_call = live.points[first..]
            .iter()
            .take_while(|p| p.use_slot <= end)
            .any(|p| p.call);
        let (mask, forced) = blocked[index];
        let mut assigned = None;
        if !forced {
            assigned = pick(value, mask, cross_call, &values, &owners, regs);
            while assigned.is_none() {
                let victim = victim_of(index, live, &values, &owners);
                if victim == index {
                    break;
                }
                if let Some(Location::Reg(reg)) = values[victim].location() {
                    owners[usize::from(reg.0)] = None;
                }
                values[victim].placement = if live.values[victim].rematerializable {
                    Placement::Rematerialize
                } else {
                    Placement::Location(Location::Slot(u32::MAX))
                };
                assigned = pick(value, mask, cross_call, &values, &owners, regs);
            }
        }
        values[index].placement = match assigned {
            Some(reg) => {
                owners[usize::from(reg.0)] = Some(index);
                Placement::Location(Location::Reg(reg))
            }
            None if value.rematerializable => Placement::Rematerialize,
            None => {
                if forced {
                    stats.safepoint_spills = stats.safepoint_spills.saturating_add(1);
                }
                Placement::Location(Location::Slot(u32::MAX))
            }
        };
        stats.allocated_values = stats.allocated_values.saturating_add(1);
        let occupied = owners
            .iter()
            .enumerate()
            .filter(|(id, owner)| owner.is_some() && regs.class(PhysReg(*id as u8)) == value.class)
            .count() as u32;
        match stats
            .peaks
            .iter_mut()
            .find(|(class, _)| *class == value.class)
        {
            Some((_, peak)) => *peak = (*peak).max(occupied),
            None => stats.peaks.push((value.class, occupied)),
        }
    }
    stats.call_sites = u32::try_from(live.points.iter().filter(|p| p.call).count())
        .map_err(|_| Error::new("too many allocation points"))?;
    Ok(Scanned { values, stats })
}

fn blocked_masks(live: &LiveInfo) -> Vec<(RegSet, bool)> {
    live.values
        .iter()
        .map(|value| {
            let mut mask = RegSet::default();
            let mut forced = value.forced;
            if value.range.0 <= value.range.1 {
                let first = live.points.partition_point(|p| p.def_slot < value.range.0);
                for point in live.points[first..]
                    .iter()
                    .take_while(|p| p.use_slot <= value.range.1)
                {
                    mask = mask.union(point.mask);
                    forced |= point.pointer_spill && value.root.must_spill();
                }
            }
            (mask, forced)
        })
        .collect()
}

fn pick(
    value: &Value,
    mask: RegSet,
    cross_call: bool,
    values: &[ValueAllocation],
    owners: &[Option<usize>; 64],
    regs: &impl RegisterFile,
) -> Option<PhysReg> {
    let free = |location: Location| match location {
        Location::Reg(reg) => {
            reg.0 < 64 && owners[usize::from(reg.0)].is_none() && !mask.contains(reg)
        }
        Location::Slot(_) => false,
    };
    let hint = value.hints.iter().find_map(|hint| {
        let location = match hint {
            Hint::Register(reg) => Some(Location::Reg(*reg)),
            Hint::SameAs(source) => values
                .get(*source as usize)
                .and_then(ValueAllocation::location),
        }?;
        free(location).then_some(location)
    });
    let available = |reg: PhysReg| value.allowed.contains(reg) && free(Location::Reg(reg));
    if let Some(Location::Reg(reg)) = hint
        && regs.class(reg) == value.class
        && available(reg)
    {
        return Some(reg);
    }
    regs.pool(value.class, cross_call)
        .iter()
        .copied()
        .find(|reg| available(*reg))
}

fn victim_of(
    index: usize,
    live: &LiveInfo,
    values: &[ValueAllocation],
    owners: &[Option<usize>; 64],
) -> usize {
    let key = |index: usize| {
        let value = &live.values[index];
        (
            value.weight,
            u128::from(value.range.1.saturating_sub(value.range.0)) + 1,
        )
    };
    let mut victim = index;
    let mut best = key(index);
    let mut candidates = owners.iter().flatten().copied().collect::<Vec<_>>();
    candidates.sort_unstable();
    candidates.dedup();
    for candidate in candidates {
        if values[candidate].location().is_none() {
            continue;
        }
        let next = key(candidate);
        if u128::from(next.0) * best.1 < u128::from(best.0) * next.1
            || (next == best && candidate > victim)
        {
            best = next;
            victim = candidate;
        }
    }
    victim
}
