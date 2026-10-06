//! 逐根初始化的块级 must 不动点：判断安全点上"该槽的根字是否全部已写成指针"。
//!
//! `slot.roots` 是多变体聚合的并集超集，逐根初始化位图把它收窄成"该字在当前值里已经写成
//! 指针"。这里以 **must** 语义求块级不动点（汇合点取前驱交集），因此
//! [`InitSlots::fully_initialized`] 只在该点沿所有路径都写全根字时为真——它是 `mask = 0`
//! （静态证明完整初始化，无需读帧内位图）的判定依据。与 `liveness` 的 may 语义刻意分开。

use super::body::{BlockId, Body};
use super::liveness::LiveSet;
use super::pass::graph::Graph;

/// 块级逐根初始化不动点 + 逐位置事件重放。
pub(crate) struct InitSlots {
    /// 每块的 in 状态：`input[block][slot]` 是"一定已写成指针"的根位集合。
    input: Vec<Vec<LiveSet>>,
    /// 每块按位置排序的事件：`(块内位置, 槽下标, 根下标, initialized)`。
    events: Vec<Vec<(u32, u32, u32, bool)>>,
}

impl InitSlots {
    pub(crate) fn new(body: &Body, graph: &Graph) -> Self {
        let empty = || {
            body.stack_slots
                .iter()
                .map(|slot| LiveSet::empty(slot.roots.len()))
                .collect::<Vec<_>>()
        };
        let mut events = vec![Vec::new(); body.blocks.len()];
        for root in &body.root_inits {
            if let Some(block) = events.get_mut(root.block.index()) {
                block.push((root.position, root.slot.0, root.bit, root.initialized));
            }
        }
        for block in &mut events {
            block.sort_by_key(|(position, slot, bit, _)| (*position, *slot, *bit));
        }
        let mut input = vec![empty(); body.blocks.len()];
        let mut output = input.clone();
        loop {
            let mut changed = false;
            for &block in &graph.order {
                // must 语义：汇合点取前驱交集；无前驱（入口）视为尚未写入任何根字。
                let incoming = predecessors(body, block);
                let mut state = incoming.first().map_or_else(&empty, |edge| {
                    output[body.edges[edge.index()].from.index()].clone()
                });
                for edge in incoming.iter().skip(1) {
                    let from = body.edges[edge.index()].from;
                    for (slot, other) in state.iter_mut().zip(&output[from.index()]) {
                        slot.intersect_with(other);
                    }
                }
                let entry = state.clone();
                for &(_, slot, bit, initialized) in &events[block.index()] {
                    state[slot as usize].set(bit as usize, initialized);
                }
                input[block.index()] = entry;
                if output[block.index()] != state {
                    output[block.index()] = state;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Self { input, events }
    }

    /// 块内某位置的逐槽初始化位；位置是块内相对指令下标，与事件同一坐标。
    pub(crate) fn at(&self, block: BlockId, position: u32) -> Vec<LiveSet> {
        let mut state = self.input[block.index()].clone();
        for &(at, slot, bit, initialized) in &self.events[block.index()] {
            if at > position {
                break;
            }
            state[slot as usize].set(bit as usize, initialized);
        }
        state
    }

    /// 该点该槽的某个根字是否已经写成指针；越界的槽或根下标视作未初始化。
    pub(crate) fn is_initialized(
        &self,
        block: BlockId,
        position: u32,
        slot: usize,
        bit: usize,
    ) -> bool {
        self.at(block, position)
            .get(slot)
            .is_some_and(|state| state.is_live(bit))
    }

    /// 该点该槽的根字是否**全部**已写成指针；没有根字的槽恒为真。
    pub(crate) fn fully_initialized(
        &self,
        body: &Body,
        block: BlockId,
        position: u32,
        slot: usize,
    ) -> bool {
        let roots = body.stack_slots[slot].roots.len();
        (0..roots).all(|bit| self.at(block, position)[slot].is_live(bit))
    }
}

fn predecessors(body: &Body, block: BlockId) -> &[super::body::EdgeId] {
    let range = &body.blocks[block.index()].predecessors;
    &body.predecessors[range.start as usize..range.end as usize]
}
