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
pub struct InitSlots {
    dynamic: Vec<bool>,
    /// 每块一个连续位图；槽根按 slot ID 顺序拼接，避免每块每槽独立分配。
    input: Vec<LiveSet>,
    offsets: Vec<usize>,
    /// 每块按位置排序的事件：`(块内位置, 槽下标, 根下标, initialized)`。
    events: Vec<Vec<(u32, u32, u32, bool)>>,
}

impl InitSlots {
    pub fn new(body: &Body, graph: &Graph) -> Self {
        let mut offsets = Vec::with_capacity(body.stack_slots.len() + 1);
        offsets.push(0usize);
        for slot in &body.stack_slots {
            offsets.push(offsets.last().expect("已有起点") + slot.roots.len());
        }
        let empty = || LiveSet::empty(*offsets.last().expect("已有终点"));
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
                let mut state = incoming.first().map_or_else(empty, |edge| {
                    output[body.edges[edge.index()].from.index()].clone()
                });
                for edge in incoming.iter().skip(1) {
                    let from = body.edges[edge.index()].from;
                    state.intersect_with(&output[from.index()]);
                }
                let entry = state.clone();
                for &(_, slot, bit, initialized) in &events[block.index()] {
                    let slot = usize::try_from(slot).expect("槽编号适配宿主");
                    let bit = usize::try_from(bit).expect("根编号适配宿主");
                    debug_assert!(bit < offsets[slot + 1] - offsets[slot]);
                    state.set(offsets[slot] + bit, initialized);
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
        let mut dynamic = vec![false; body.stack_slots.len()];
        for instruction in &body.instructions {
            if let super::body::Op::RootInitAddr(slot) = instruction.op {
                dynamic[slot.index()] = true;
            }
        }
        Self {
            dynamic,
            input,
            events,
            offsets,
        }
    }

    /// 该点该槽的某个根字是否已经写成指针；越界的槽或根下标视作未初始化。
    pub fn is_initialized(&self, block: BlockId, position: u32, slot: usize, bit: usize) -> bool {
        if self.dynamic.get(slot) == Some(&true) {
            return false;
        }
        let Some((&start, &end)) = self.offsets.get(slot).zip(self.offsets.get(slot + 1)) else {
            return false;
        };
        if bit >= end - start {
            return false;
        }
        for &(at, event_slot, event_bit, initialized) in self.events[block.index()].iter().rev() {
            if at <= position
                && usize::try_from(event_slot).expect("槽编号适配宿主") == slot
                && usize::try_from(event_bit).expect("根编号适配宿主") == bit
            {
                return initialized;
            }
        }
        self.input[block.index()].is_live(start + bit)
    }

    /// 该点该槽的根字是否**全部**已写成指针；没有根字的槽恒为真。
    pub fn fully_initialized(
        &self,
        body: &Body,
        block: BlockId,
        position: u32,
        slot: usize,
    ) -> bool {
        let roots = body.stack_slots[slot].roots.len();
        (0..roots).all(|bit| self.is_initialized(block, position, slot, bit))
    }
}

fn predecessors(body: &Body, block: BlockId) -> &[super::body::EdgeId] {
    let range = &body.blocks[block.index()].predecessors;
    &body.predecessors[range.start as usize..range.end as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_queries_keep_entry_facts_until_the_last_local_event() {
        let mut entry = LiveSet::empty(2);
        entry.set(1, true);
        let slots = InitSlots {
            dynamic: vec![false],
            input: vec![entry],
            offsets: vec![0, 2],
            events: vec![vec![
                (2, 0, 0, true),
                (4, 0, 0, false),
                (4, 0, 0, true),
                (5, 0, 1, false),
            ]],
        };
        assert!(!slots.is_initialized(BlockId(0), 1, 0, 0));
        assert!(slots.is_initialized(BlockId(0), 2, 0, 0));
        assert!(slots.is_initialized(BlockId(0), 4, 0, 0));
        assert!(slots.is_initialized(BlockId(0), 4, 0, 1));
        assert!(!slots.is_initialized(BlockId(0), 5, 0, 1));
        assert!(!slots.is_initialized(BlockId(0), 5, 1, 0));
    }

    #[test]
    fn dynamic_root_bytes_never_lose_the_runtime_gate() {
        let slots = InitSlots {
            dynamic: vec![true],
            input: vec![LiveSet::empty(1)],
            offsets: vec![0, 1],
            events: vec![vec![(0, 0, 0, true)]],
        };
        assert!(!slots.is_initialized(BlockId(0), 1, 0, 0));
    }
}
