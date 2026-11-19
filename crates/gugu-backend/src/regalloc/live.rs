//! CFG fixed point, convex intervals and deterministic loop-weighted uses.
use super::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Interval {
    pub start: u32,
    pub end: u32,
    pub uses: Vec<u32>,
    pub rematerializable: bool,
    pub weight: u64,
}
impl Interval {
    pub const fn is_live(&self) -> bool {
        self.start <= self.end
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockPoint {
    pub block: u32,
    pub use_slot: u32,
    pub def_slot: u32,
    pub fixed_physical: bool,
    pub call: bool,
}

pub struct Program {
    pub successors: Vec<Vec<usize>>,
    pub predecessors: Vec<Vec<usize>>,
    pub entry: usize,
    pub uses: Vec<Vec<u64>>,
    pub defs: Vec<Vec<u64>>,
    pub points: Vec<BlockPoint>,
    pub values: Vec<Interval>,
}

pub fn analyze(mut program: Program) -> Result<Vec<Interval>, Error> {
    let count = program.successors.len();
    let words = program.values.len().div_ceil(64);
    if program.predecessors.len() != count
        || program.uses.len() != count
        || program.defs.len() != count
        || (count > 0 && program.entry >= count)
        || program
            .successors
            .iter()
            .chain(&program.predecessors)
            .flatten()
            .any(|id| *id >= count)
        || program
            .uses
            .iter()
            .chain(&program.defs)
            .any(|bits| bits.len() != words)
        || program
            .points
            .iter()
            .any(|point| point.block as usize >= count)
    {
        return Err(Error::new("invalid normalized allocation CFG"));
    }
    let liveness = liveness(&program.successors, &program.uses, &program.defs);
    let bounds = block_bounds(&program.points, count);
    intervals(&bounds, &mut program.values, &liveness);
    let depths = loop_depths(&program.predecessors, program.entry);
    weights(&program.points, &depths, &mut program.values);
    Ok(program.values)
}

pub struct Liveness {
    live_in: Vec<Vec<u64>>,
    live_out: Vec<Vec<u64>>,
}

fn liveness(successors: &[Vec<usize>], uses: &[Vec<u64>], defs: &[Vec<u64>]) -> Liveness {
    let words = uses.first().map_or(0, Vec::len);
    let mut live_in = vec![vec![0_u64; words]; successors.len()];
    let mut live_out = vec![vec![0_u64; words]; successors.len()];
    loop {
        let mut changed = false;
        for (index, succs) in successors.iter().enumerate() {
            let mut out = vec![0_u64; words];
            for succ in succs {
                union_into(&mut out, &live_in[*succ]);
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
fn intervals(bounds: &[BlockBounds], values: &mut [Interval], liveness: &Liveness) {
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

fn block_bounds(points: &[BlockPoint], blocks: usize) -> Vec<BlockBounds> {
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
fn weights(points: &[BlockPoint], depths: &[u32], values: &mut [Interval]) {
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
            let base = if point.fixed_physical || point.call {
                4_u64
            } else {
                1
            };
            let depth = depths.get(point.block as usize).copied().unwrap_or(0);
            let mut weight = base.saturating_mul(loop_factor(depth));
            if point.use_slot >= 2
                && points
                    .get(((point.use_slot - 2) / 2) as usize)
                    .is_some_and(|previous| previous.block == point.block && previous.call)
            {
                weight = weight.saturating_mul(2);
            }
            total = total.saturating_add(weight);
        }
        value.weight = total;
    }
}

/// 循环深度因子：`min(10^d, 1_000_000)`。
pub fn loop_factor(depth: u32) -> u64 {
    10_u64.checked_pow(depth).unwrap_or(u64::MAX).min(1_000_000)
}

fn loop_depths(predecessors: &[Vec<usize>], entry: usize) -> Vec<u32> {
    let count = predecessors.len();
    let words = count.div_ceil(64);
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
    conservative_cycles(predecessors, &mut depth);
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
