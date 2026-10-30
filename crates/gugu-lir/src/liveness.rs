//! 槽活跃性：栈根筛选的唯一来源。
//!
//! 与 `verify/provenance.rs` 的 must 语义分析分开：那里要求「所有路径都活」用于校验读取，
//! 根登记必须用 may 语义——只要**某条**路径上该槽的值还会被用到，它的根就必须登记，否则
//! 移动式收集器会在别的路径上漏更新该槽，留下悬垂指针。
//!
//! 生命周期事件来自 `StorageLive`/`StorageDead` 降级。没有事件的槽（例如 lowering 自建的
//! 临时槽）无法被事件模型证明死亡，恒视为活跃，只能保守登记。
//!
//! 集合按机器字打包：槽规模常见在几十以内，一个 `u64` 就能覆盖，汇合退化成一条按字或。

use super::body::{BlockId, Body, Definition, InstId, UseSite, id, range};
use super::pass::graph::Graph;

/// 槽活跃位图；位下标即槽下标。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveSet {
    words: Vec<u64>,
}

impl LiveSet {
    pub fn empty(slots: usize) -> Self {
        Self {
            words: vec![0; words_for(slots)],
        }
    }

    /// 全部槽活跃的位图；末字只覆盖真实存在的槽。
    pub fn full(slots: usize) -> Self {
        let mut set = Self::empty(slots);
        let last = set.words.len() - 1;
        let tail = slots % 64;
        let tail_bits = if tail == 0 && slots != 0 { 64 } else { tail };
        for (index, word) in set.words.iter_mut().enumerate() {
            // 末字只覆盖真实存在的槽：槽数为 64 的整数倍（含 0）时不能多置位。
            *word = if index == last && tail_bits != 64 {
                (1u64 << tail_bits) - 1
            } else {
                u64::MAX
            };
        }
        set
    }

    pub fn is_live(&self, slot: usize) -> bool {
        self.words
            .get(slot / 64)
            .is_some_and(|word| word >> (slot % 64) & 1 == 1)
    }

    pub fn set(&mut self, slot: usize, live: bool) {
        let Some(word) = self.words.get_mut(slot / 64) else {
            return;
        };
        let bit = 1u64 << (slot % 64);
        if live {
            *word |= bit;
        } else {
            *word &= !bit;
        }
    }

    fn union_with(&mut self, other: &Self) {
        for (word, other) in self.words.iter_mut().zip(&other.words) {
            *word |= *other;
        }
    }

    /// must 语义的汇合：只保留两侧都置位的元素。
    #[allow(
        dead_code,
        reason = "逐根初始化证明（root_init）是当前唯一消费者，随该模块转入生产路径"
    )]
    pub fn intersect_with(&mut self, other: &Self) {
        for (word, other) in self.words.iter_mut().zip(&other.words) {
            *word &= *other;
        }
    }
}

/// 块级活跃不动点 + 逐位置事件重放。
pub struct LiveSlots {
    /// 每块的 live-in 位图。
    input: Vec<LiveSet>,
    /// 每块按位置排序的事件：`(块内位置, 槽下标, 活跃)`。
    events: Vec<Vec<(u32, u32, bool)>>,
}

impl LiveSlots {
    /// 以 may 语义求块级不动点：汇合点取前驱的并集。
    pub fn new(body: &Body, graph: &Graph) -> Self {
        let slots = body.stack_slots.len();
        let mut untracked = LiveSet::full(slots);
        let mut events = vec![Vec::new(); body.blocks.len()];
        for lifetime in &body.lifetimes {
            untracked.set(lifetime.slot.index(), false);
            if let Some(block) = events.get_mut(lifetime.block.index()) {
                block.push((lifetime.position, lifetime.slot.0, lifetime.live));
            }
        }
        for block in &mut events {
            block.sort_by_key(|(position, slot, _)| (*position, *slot));
        }
        let mut input = vec![LiveSet::empty(slots); body.blocks.len()];
        let mut output = input.clone();
        input[body.entry.index()] = untracked.clone();
        loop {
            let mut changed = false;
            for &block in &graph.order {
                let mut live = if block == body.entry {
                    untracked.clone()
                } else {
                    LiveSet::empty(slots)
                };
                for edge in predecessors(body, block) {
                    let from = body.edges[edge.index()].from;
                    live.union_with(&output[from.index()]);
                }
                let entry = live.clone();
                for &(_, slot, is_live) in &events[block.index()] {
                    live.set(slot as usize, is_live);
                }
                input[block.index()] = entry;
                if output[block.index()] != live {
                    output[block.index()] = live;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Self { input, events }
    }

    /// 块内某位置的活跃集合；位置是块内相对指令下标，与事件同一坐标。
    pub fn at(&self, block: BlockId, position: u32) -> LiveSet {
        let mut live = self.input[block.index()].clone();
        for &(at, slot, is_live) in &self.events[block.index()] {
            if at > position {
                break;
            }
            live.set(slot as usize, is_live);
        }
        live
    }
}

/// 块级值活跃不动点（SSA 逆向，may 语义）。
///
/// 与槽活跃性的关键差别是块参数：参数是**块内的定义**，真正被持有的对象在边的实参一侧，
/// 因此向后传播时要把后继块参数替换为对应实参。汇合取并集——只要某条路径上该值还会被
/// 用到，它在该点的物理位置（分配器为跨调用、分配、挂起与 bridge 点位生成的 spill 槽）
/// 就必须登记，否则收集器扫不到仍被持有的对象。
///
/// 位置坐标与 `LiveSlots` 一致：块内相对指令下标，终结符与边的使用记在块尾位置。
pub struct LiveValues {
    /// 每块的出口活跃集合，已消解块参数。
    output: Vec<LiveSet>,
    /// 每块按位置排序的使用事件：`(块内位置, 值下标)`。
    uses: Vec<Vec<(u32, u32)>>,
    /// 每块的定义事件：块参数记在位置 0，其余记在所属指令位置。
    defs: Vec<Vec<(u32, u32)>>,
    /// 每块的块参数集合。参数是**块的输入**，在块入口处已经可用，因此不能当作
    /// 「本点之后才写入的定义」被扣除；它们只应由对应边的实参在向后传播时替换。
    parameters: Vec<LiveSet>,
}

impl LiveValues {
    pub fn new(body: &Body, graph: &Graph) -> Self {
        let values = body.values.len();
        let mut uses = vec![Vec::new(); body.blocks.len()];
        let mut defs = vec![Vec::new(); body.blocks.len()];
        // 本块内由指令或终结符定义的值。块参数不算：参数是被持有的接收入口，值来自边实参。
        let mut local = vec![LiveSet::empty(values); body.blocks.len()];
        // 块参数是被持有的接收入口：它必须留在 live-in 里，随后由对应边的实参替换。
        // 若不单独记下来，下面的「扣除本块定义」会把参数一起扣掉，替换循环永远不触发，
        // 跨块携带的受管值（未取地址的切片就退化成一条参数链）就再也传播不回前驱块。
        let mut parameters = vec![LiveSet::empty(values); body.blocks.len()];
        for (index, value) in body.values.iter().enumerate() {
            let index = id(index);
            match value.definition {
                Definition::Parameter { block, .. } => {
                    if let Some(events) = defs.get_mut(block.index()) {
                        events.push((0, index));
                    }
                    parameters[block.index()].set(index as usize, true);
                }
                Definition::Instruction { instruction, .. } => {
                    let block = graph.instruction_blocks[instruction.index()];
                    defs[block.index()].push((position_in(body, block, instruction), index));
                    local[block.index()].set(index as usize, true);
                }
                Definition::Invoke { block, .. } => {
                    defs[block.index()].push((block_end(body, block), index));
                    local[block.index()].set(index as usize, true);
                }
            }
            for site in &body.uses[range(&value.uses)] {
                let (block, position) = match site.site {
                    UseSite::Instruction(instruction) => {
                        let block = graph.instruction_blocks[instruction.index()];
                        (block, position_in(body, block, instruction))
                    }
                    UseSite::Terminator(block) => (block, block_end(body, block)),
                    UseSite::Edge(edge) => {
                        let edge = &body.edges[edge.index()];
                        (edge.from, block_end(body, edge.from))
                    }
                };
                if let Some(events) = uses.get_mut(block.index()) {
                    events.push((position, index));
                }
            }
        }
        for events in &mut uses {
            events.sort_unstable();
        }
        for events in &mut defs {
            events.sort_unstable();
        }
        let mut output = vec![LiveSet::empty(values); body.blocks.len()];
        let mut incoming = output.clone();
        loop {
            let mut changed = false;
            for &block in &graph.order {
                let mut live = LiveSet::empty(values);
                // SSA 下同块定义必然支配同块使用，所以本块自己定义过的值不构成 live-in：
                // 它的值由本块产生，只有块参数才需要沿边接收（随后由实参替换）。
                for &(_, value) in &uses[block.index()] {
                    if !local[block.index()].is_live(value as usize) {
                        live.set(value as usize, true);
                    }
                }
                let mut carried = output[block.index()].clone();
                for &(_, value) in &defs[block.index()] {
                    // 块参数不是「本块产生」的值：它必须留在 live-in 里，好让下面的替换
                    // 循环把它换成对应边的实参，否则跨块携带的受管值会在前驱块里凭空消失。
                    if parameters[block.index()].is_live(value as usize) {
                        continue;
                    }
                    carried.set(value as usize, false);
                }
                live.union_with(&carried);
                if live != incoming[block.index()] {
                    incoming[block.index()] = live.clone();
                    changed = true;
                }
                for edge in &body.predecessors[range(&body.blocks[block.index()].predecessors)] {
                    let edge = &body.edges[edge.index()];
                    let from = edge.from;
                    let mut flow = live.clone();
                    for (parameter, argument) in
                        body.params(block).iter().zip(body.args(&edge.arguments))
                    {
                        if !flow.is_live(parameter.value.index()) {
                            continue;
                        }
                        flow.set(parameter.value.index(), false);
                        flow.set(argument.index(), true);
                    }
                    let before = output[from.index()].clone();
                    output[from.index()].union_with(&flow);
                    if output[from.index()] != before {
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        Self {
            output,
            uses,
            defs,
            parameters,
        }
    }

    /// 该点位**之后**仍活跃的值。
    ///
    /// 位于该点位的定义一并扣除：安全点在该指令（或调用）执行期间触发，结果尚未写入。
    pub fn after(&self, block: BlockId, position: u32) -> LiveSet {
        let mut live = self.output[block.index()].clone();
        for &(at, value) in &self.uses[block.index()] {
            if at > position {
                live.set(value as usize, true);
            }
        }
        for &(at, value) in &self.defs[block.index()] {
            // 块参数在块入口处就已可用，不是「本点之后才写入」的结果：若在这里扣掉，
            // 位于块首（位置 0）的安全点会把本块参数整条排除，跨块携带的受管值随之丢根。
            if self.parameters[block.index()].is_live(value as usize) {
                continue;
            }
            if at >= position {
                live.set(value as usize, false);
            }
        }
        live
    }
}

/// 指令在所属块内的相对位置。
fn position_in(body: &Body, block: BlockId, instruction: InstId) -> u32 {
    instruction.0 - body.blocks[block.index()].instructions.start
}

/// 块尾位置：终结符、边与 `Invoke` 结果的定义坐标。
fn block_end(body: &Body, block: BlockId) -> u32 {
    let instructions = &body.blocks[block.index()].instructions;
    instructions.end - instructions.start
}

fn words_for(slots: usize) -> usize {
    slots.div_ceil(64).max(1)
}

fn predecessors(body: &Body, block: BlockId) -> &[super::body::EdgeId] {
    let range = &body.blocks[block.index()].predecessors;
    &body.predecessors[range.start as usize..range.end as usize]
}
