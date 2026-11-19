//! Stable parallel-copy ordering; target policy supplies cycle-save legalization.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Copy<L, T> {
    pub src: L,
    pub dst: L,
    pub ty: T,
}

pub struct CycleBreak<L, T> {
    pub holder: L,
    pub saves: Vec<Copy<L, T>>,
    pub uses_scratch: bool,
}

pub trait CopyPolicy<L, T> {
    fn break_cycle(&self, saved: L, ty: T, pending: &[Copy<L, T>]) -> CycleBreak<L, T>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyPlan<L, T> {
    pub moves: Vec<Copy<L, T>>,
    pub cycles: u32,
    pub uses_scratch: bool,
}

pub fn resolve_copies<L: std::marker::Copy + Eq, T: std::marker::Copy>(
    moves: &[Copy<L, T>],
    policy: &impl CopyPolicy<L, T>,
    scratch_available: bool,
) -> Result<CopyPlan<L, T>, super::Error> {
    let mut emitted = Vec::with_capacity(moves.len());
    let mut pending = moves.to_vec();
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
            emitted.push(pending.remove(index));
            continue;
        }
        let saved = pending[0].dst;
        cycles += 1;
        let cycle = policy.break_cycle(saved, pending[0].ty, &pending);
        uses_scratch |= cycle.uses_scratch;
        if cycle.uses_scratch && !scratch_available {
            break;
        }
        if cycle.holder == saved {
            return Err(super::Error::new("copy scratch aliases cycle destination"));
        }
        emitted.extend(cycle.saves);
        for mov in &mut pending {
            if mov.src == saved {
                mov.src = cycle.holder;
            }
        }
    }
    Ok(CopyPlan {
        moves: emitted,
        cycles,
        uses_scratch,
    })
}
