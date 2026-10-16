//! LIR CFG 分析：可达性、逆后序、支配与自然循环。
//!
//! 这些算法由固定管线的 pass 与结构 verifier 共用；`verify.rs` 不再保留私有副本。
use super::super::body::{BlockId, Body, id, range};
use super::super::invalid;
use super::super::verify::edges;
use crate::Diagnostic;

pub(crate) struct Graph {
    /// 逆后序；入口在前。
    pub(crate) order: Vec<BlockId>,
    /// 每条指令所属 block。
    pub(crate) instruction_blocks: Vec<BlockId>,
    dominators: Vec<BlockId>,
}

impl Graph {
    pub(crate) fn new(body: &Body) -> Result<Self, Diagnostic> {
        let mut seen = vec![false; body.blocks.len()];
        let mut postorder = Vec::new();
        let mut stack = vec![(body.entry, false)];
        while let Some((block, exiting)) = stack.pop() {
            if exiting {
                postorder.push(block);
                continue;
            }
            if std::mem::replace(&mut seen[block.index()], true) {
                continue;
            }
            stack.push((block, true));
            edges(body, &body.blocks[block.index()].terminator, |edge| {
                stack.push((body.edges[edge.index()].to, false))
            });
        }
        if seen.iter().any(|seen| !seen) {
            return Err(invalid("LIR 含有未连接入口的 block"));
        }
        postorder.reverse();
        let mut rank = vec![0; body.blocks.len()];
        for (index, block) in postorder.iter().enumerate() {
            rank[block.index()] = index;
        }
        let mut dominators = vec![None; body.blocks.len()];
        dominators[body.entry.index()] = Some(body.entry);
        loop {
            let mut changed = false;
            for &block in postorder.iter().skip(1) {
                let mut next = None;
                for predecessor in
                    &body.predecessors[range(&body.blocks[block.index()].predecessors)]
                {
                    let predecessor = body.edges[predecessor.index()].from;
                    if dominators[predecessor.index()].is_none() {
                        continue;
                    }
                    next = Some(match next {
                        None => predecessor,
                        Some(previous) => intersect(previous, predecessor, &dominators, &rank),
                    });
                }
                if dominators[block.index()] != next {
                    dominators[block.index()] = next;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let dominators = dominators
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| invalid("支配树无法覆盖全部 block"))?;
        let mut instruction_blocks = vec![body.entry; body.instructions.len()];
        for (index, block) in body.blocks.iter().enumerate() {
            for instruction in range(&block.instructions) {
                instruction_blocks[instruction] = BlockId(id(index));
            }
        }
        Ok(Self {
            order: postorder,
            instruction_blocks,
            dominators,
        })
    }

    pub(crate) fn dominates(&self, dominator: BlockId, mut block: BlockId) -> bool {
        loop {
            if block == dominator {
                return true;
            }
            let parent = self.dominators[block.index()];
            if block == parent {
                return false;
            }
            block = parent;
        }
    }
}

fn intersect(
    mut left: BlockId,
    mut right: BlockId,
    dominators: &[Option<BlockId>],
    rank: &[usize],
) -> BlockId {
    while left != right {
        while rank[left.index()] > rank[right.index()] {
            left = dominators[left.index()].expect("已求得支配块");
        }
        while rank[right.index()] > rank[left.index()] {
            right = dominators[right.index()].expect("已求得支配块");
        }
    }
    left
}

/// 逆后序；入口在前，后继顺序与 terminator 边顺序一致。
pub(crate) fn reverse_postorder(body: &Body) -> Vec<BlockId> {
    reverse_postorder_from(body.blocks.len(), body.entry, |block| {
        successors(body, block)
    })
}

/// 基于后继闭包的逆后序。
pub(crate) fn reverse_postorder_from(
    count: usize,
    entry: BlockId,
    successors: impl Fn(BlockId) -> Vec<BlockId>,
) -> Vec<BlockId> {
    let mut seen = vec![false; count];
    let mut postorder = Vec::with_capacity(count);
    let mut stack = vec![(entry, false)];
    while let Some((block, exiting)) = stack.pop() {
        if exiting {
            postorder.push(block);
            continue;
        }
        if std::mem::replace(&mut seen[block.index()], true) {
            continue;
        }
        stack.push((block, true));
        for successor in successors(block) {
            stack.push((successor, false));
        }
    }
    postorder.reverse();
    postorder
}

/// 迭代式支配树；存在不可达 block 时返回 `None`。
pub(crate) fn dominators(
    count: usize,
    entry: BlockId,
    successors: impl Fn(BlockId) -> Vec<BlockId>,
    predecessors: impl Fn(BlockId) -> Vec<BlockId>,
) -> Option<Vec<BlockId>> {
    let postorder = reverse_postorder_from(count, entry, &successors);
    if postorder.len() != count {
        return None;
    }
    let mut rank = vec![0usize; count];
    for (index, block) in postorder.iter().enumerate() {
        rank[block.index()] = index;
    }
    let mut dominators: Vec<Option<BlockId>> = vec![None; count];
    dominators[entry.index()] = Some(entry);
    loop {
        let mut changed = false;
        for &block in postorder.iter().skip(1) {
            let mut next = None;
            for predecessor in predecessors(block) {
                if dominators[predecessor.index()].is_none() {
                    continue;
                }
                next = Some(match next {
                    None => predecessor,
                    Some(previous) => intersect(previous, predecessor, &dominators, &rank),
                });
            }
            if dominators[block.index()] != next {
                dominators[block.index()] = next;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    dominators.into_iter().collect()
}

/// 支配关系查询；`dominators` 来自 [`dominators`]。
pub(crate) fn dominates(dominators: &[BlockId], dominator: BlockId, mut block: BlockId) -> bool {
    loop {
        if block == dominator {
            return true;
        }
        let parent = dominators[block.index()];
        if block == parent {
            return false;
        }
        block = parent;
    }
}

fn successors(body: &Body, block: BlockId) -> Vec<BlockId> {
    let mut successors = Vec::new();
    edges(body, &body.blocks[block.index()].terminator, |edge| {
        successors.push(body.edges[edge.index()].to)
    });
    successors
}
