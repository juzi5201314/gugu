//! `EdgeDelta` 的消费平面：按序号顺序应用、保持乱序记录、维护候选 dirty 集合。
//!
//! 归属边界：
//!
//! 1. **source 侧聚合由 barrier 平面持有**：`EdgeSummary` 把同一 block 对的多次字段写聚合
//!    成 signed 差量，`RawWorld::publish_edge_deltas` 取出后交给这里发布。
//! 2. **target 侧只认序号**：每个 block 对的序号从 1 开始，target 收到 `next_sequence` 才应用；
//!    未来序号连同它的 node 与 credit 一起保留，旧或重复序号是真正的不变量失败。
//! 3. **dirty 集合只去重**：mutator 通过它通知候选平面哪些 block 变了；重复修改只置位，
//!    不累积事件日志，跨批的候选 job 因此可以只失效受影响的子图。
//!
//! `staged` 保存的是**解码后的记录加上它的 node 身份**：缺口补齐时必须能直接应用，并且把
//! 之前保留的 node 交给 grace，因此不能只保存 node 编号。

use std::collections::{BTreeMap, BTreeSet};

use super::local_heap::{BlockRef, ManagedBlockId};
use super::mark_schema::GcCreditId;
use super::message::{EdgeDelta, ReturnNodeId};
use super::slab::RawInvariant;

/// 一个 block 对；`Ord` 保证发布与遍历顺序稳定。
pub(crate) type EdgePair = (BlockRef, BlockRef);

/// 一条被保留的乱序记录：解码后的内容加上它的 node 身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HeldRecord {
    pub(crate) sequence: u64,
    pub(crate) delta: i64,
    pub(crate) credit: GcCreditId,
    pub(crate) node: ReturnNodeId,
}

/// 一个 block 对在 target 侧的状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AppliedPair {
    /// 已按顺序应用的计数；它可能为 0，表示内部边互相抵消。
    applied: i64,
    /// 期待的下一个序号；发布序号从 1 开始，因此初始期待值也是 1。
    next_sequence: u64,
    /// 该对当前保留的 credit 数（乱序记录尚未结算）。
    held: u64,
}

impl Default for AppliedPair {
    fn default() -> Self {
        Self {
            applied: 0,
            next_sequence: 1,
            held: 0,
        }
    }
}

/// 一次应用的结果：立即应用，或因为序号缺口而保留。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EdgeApply {
    /// 记录已按顺序应用；调用方应结算它的 credit。
    Applied,
    /// 序号是未来值；记录连同 credit 被保留，等缺口补齐。
    Held,
}

/// 边差量平面的统计；进入诊断与 dump。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EdgeStats {
    /// 已发布的记录数。
    pub(crate) published: u64,
    /// 已应用的记录数。
    pub(crate) applied: u64,
    /// 因乱序而保留的记录数。
    pub(crate) held: u64,
    /// 缺口补齐时被连带应用的记录数。
    pub(crate) released: u64,
    /// 已清退的零计数 block 对数量。
    pub(crate) retired_pairs: u64,
}

/// 一次 `EdgeDelta` 消费的结果。
///
/// `Held` 是唯一不允许释放 node 的情形：记录与 credit 都被保留，等缺口补齐后再应用。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EdgeOutcome {
    /// 记录已按顺序应用，调用方可以释放 node。
    Applied,
    /// 序号是未来值，记录连同 node 与 credit 一起保留。
    Held,
    /// destination 已经换 manager：记录与 credit 已转投新 manager，旧 node 可以释放。
    Forwarded,
}

/// 消费平面：target 侧已应用计数、保留的乱序记录与候选 dirty 集合。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct EdgePlane {
    /// 按 block 对索引的已应用状态；稀疏有序键，不按稠密 block id 建 HashMap。
    applied: BTreeMap<EdgePair, AppliedPair>,
    /// 每个 block 对尚未补齐的序号；键是序号，值是保留的记录。
    staged: BTreeMap<EdgePair, BTreeMap<u64, HeldRecord>>,
    /// 缺口补齐后已可释放的 node 身份；由世界级 grace 收口。
    released: Vec<HeldRecord>,
    /// 候选 dirty block；重复修改只置位。
    dirty: BTreeSet<ManagedBlockId>,
    stats: EdgeStats,
}

impl EdgePlane {
    /// 创建一个空平面。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 返回统计快照。
    pub(crate) const fn stats(&self) -> EdgeStats {
        self.stats
    }

    /// 返回 dirty block 数量。
    pub(crate) fn dirty_count(&self) -> u64 {
        self.dirty.len() as u64
    }

    /// 按有序顺序枚举 dirty block；候选平面按它选择要推进的 job。
    pub(crate) fn dirty_blocks(&self) -> impl Iterator<Item = ManagedBlockId> + '_ {
        self.dirty.iter().copied()
    }

    /// 标记一个 block 已变化；重复标记只置位。
    pub(crate) fn note_dirty(&mut self, block: ManagedBlockId) {
        self.dirty.insert(block);
    }

    /// 清除一个 block 的 dirty 位；候选 job 重新快照后调用。
    pub(crate) fn clear_dirty(&mut self, block: ManagedBlockId) -> bool {
        self.dirty.remove(&block)
    }

    /// 返回仍待发布的记录数（保留的乱序记录）。
    pub(crate) fn held_records(&self) -> u64 {
        self.staged
            .values()
            .map(|records| records.len() as u64)
            .sum()
    }

    /// 记录一次发布。
    pub(crate) fn note_publish(&mut self, records: u64) {
        self.stats.published = self.stats.published.saturating_add(records);
    }

    /// 返回一个 block 的 incoming 已应用计数之和。
    ///
    /// 这是候选判定的输入：它是 target 侧的真实入边计数，不是对象级引用计数，也不使用
    /// source 侧尚未应用的聚合值。
    pub(crate) fn incoming_applied(&self, target: BlockRef) -> i64 {
        self.applied
            .iter()
            .filter(|((_, destination), _)| *destination == target)
            .map(|(_, pair)| pair.applied)
            .fold(0_i64, i64::saturating_add)
    }

    /// 把一个目标 block 的全部已应用计数迁移到新的目标 block。
    ///
    /// 对象被 evacuate 后，源 block 的引用仍然存在，但目标 block 变了：`(S, 旧)` 的计数必须整体
    /// 变成 `(S, 新)`，否则旧 block 会永远背着已经搬走的入边计数、新 block 则像是没有入边。
    /// 键按 block **身份**匹配（旧目标在搬迁后可能已经复位，保留在键里的世代不再等于当前值），
    /// 新键使用调用方给出的当前身份；`target` 为 `None` 表示目标 block 已无存活对象，整条键清退。
    /// 返回被迁移的计数总和。
    pub(crate) fn relocate_destination(
        &mut self,
        old: ManagedBlockId,
        target: Option<BlockRef>,
    ) -> Result<u64, RawInvariant> {
        let pairs: Vec<RelocatedPair> = self
            .applied
            .iter()
            .filter(|((_, destination), _)| destination.id == old)
            .map(|(key, pair)| {
                let staged = self.staged.get(key).cloned();
                (*key, *pair, staged)
            })
            .collect();
        let mut moved = 0_u64;
        for ((source, old_ref), pair, staged) in pairs {
            self.applied.remove(&(source, old_ref));
            self.staged.remove(&(source, old_ref));
            let Some(target) = target else {
                // 目标 block 已经没有任何存活对象：这条引用不再存在。
                self.stats.retired_pairs = self.stats.retired_pairs.saturating_add(1);
                continue;
            };
            let entry = self.applied.entry((source, target)).or_default();
            entry.applied = entry.applied.saturating_add(pair.applied);
            // 序号血统随计数一起迁移：新键的下一个期待序号不能从头开始，否则会接受已经应用过的
            // 旧序号；保留记录也一起搬家，缺口补齐仍然成立。
            entry.next_sequence = entry.next_sequence.max(pair.next_sequence);
            entry.held = entry.held.saturating_add(pair.held);
            if let Some(staged) = staged {
                let slot = self.staged.entry((source, target)).or_default();
                for (sequence, record) in staged {
                    slot.entry(sequence).or_insert(record);
                }
            }
            moved = moved.saturating_add(u64::try_from(pair.applied.max(0)).unwrap_or(0));
        }
        Ok(moved)
    }

    /// 返回一个 block 对当前的已应用计数。
    pub(crate) fn applied_delta(&self, source: BlockRef, destination: BlockRef) -> i64 {
        self.applied
            .get(&(source, destination))
            .map_or(0, |pair| pair.applied)
    }

    /// 按稳定顺序枚举所有正计数的 block 对；候选平面按它做试验删除。
    pub(crate) fn applied_pairs(&self) -> Vec<(BlockRef, BlockRef, i64)> {
        self.applied
            .iter()
            .filter(|(_, pair)| pair.applied > 0)
            .map(|((source, destination), pair)| (*source, *destination, pair.applied))
            .collect()
    }

    /// 减去一个死亡组留下的出边计数。
    ///
    /// 这是候选平面 `DropOutgoing` 的唯一入口：计数减到 0 就把该 block 对整体清退，避免保留
    /// 一堆零计数的键；扣减超过当前计数是真正的不变量失败，而不是静默截断。
    pub(crate) fn drop_applied(
        &mut self,
        source: BlockRef,
        destination: BlockRef,
        count: u32,
    ) -> Result<i64, RawInvariant> {
        let pair = (source, destination);
        let state = self
            .applied
            .get_mut(&pair)
            .ok_or_else(|| RawInvariant::new("出边减量的 block 对没有已应用计数"))?;
        if i64::from(count) > state.applied {
            return Err(RawInvariant::new("出边减量超过已应用计数"));
        }
        state.applied -= i64::from(count);
        let remaining = state.applied;
        if remaining == 0 && state.held == 0 {
            // 零计数且没有保留记录时清退该键：候选阶段按「计数为零」判断，保留空键会让判定
            // 每次都多走一遍已经不存在的边。
            self.applied.remove(&pair);
            self.stats.retired_pairs = self.stats.retired_pairs.saturating_add(1);
        }
        Ok(remaining)
    }

    /// 返回仍在飞的 credit 数：保留记录各自占用一个。
    pub(crate) fn pending_credits(&self) -> u64 {
        self.applied.values().map(|pair| pair.held).sum()
    }

    /// 应用一条已发布记录。
    ///
    /// 序号必须等于该 block 对期待的下一个值才会应用；更大的序号被保留并返回 `Held`，
    /// 更小或重复的序号是真正的不变量失败。缺口补齐时被连带应用的记录连同它们的 node
    /// 身份一起交给调用方，因此保留的 node 一定会在应用后进入 grace。
    pub(crate) fn apply(
        &mut self,
        source: BlockRef,
        destination: BlockRef,
        record: HeldRecord,
    ) -> Result<EdgeApply, RawInvariant> {
        let pair = (source, destination);
        let state = self.applied.entry(pair).or_default();
        if record.sequence < state.next_sequence {
            return Err(RawInvariant::new(
                "edge delta 的序号已经应用过（旧或重复记录）",
            ));
        }
        if record.sequence > state.next_sequence {
            // 未来序号：记录与 credit 都保留，等缺口补齐；重复序号也不能覆盖既有记录。
            let staged = self.staged.entry(pair).or_default();
            if let std::collections::btree_map::Entry::Occupied(entry) =
                staged.entry(record.sequence)
            {
                if entry.get() != &record {
                    return Err(RawInvariant::new("同一序号出现两条不同的 edge delta"));
                }
                return Ok(EdgeApply::Held);
            }
            staged.insert(record.sequence, record);
            let state = self.applied.get_mut(&pair).expect("刚登记过该对");
            state.held += 1;
            self.stats.held += 1;
            return Ok(EdgeApply::Held);
        }
        self.apply_in_order(pair, record)?;
        Ok(EdgeApply::Applied)
    }

    /// 取走因缺口补齐而已可释放的 node 身份。
    pub(crate) fn take_released(&mut self) -> Vec<HeldRecord> {
        std::mem::take(&mut self.released)
    }

    /// 清退已应用计数为 0、没有保留记录的 block 对。
    ///
    /// 重建同一个 block 对时序号从 1 重新开始，旧记录由发布端的 generation 与这里的序号
    /// 校验拒绝，因此不需要无限积累零计数键。保留记录（乱序缺口）会让所属 block 对留下：清掉它
    /// 就等于把等待补齐的序列丢掉。
    pub(crate) fn retire_zero_pairs(&mut self) -> u64 {
        let mut retired = 0_u64;
        self.applied.retain(|pair, state| {
            let keep = state.applied != 0
                || state.held != 0
                || self
                    .staged
                    .get(pair)
                    .is_some_and(|records| !records.is_empty());
            if !keep {
                retired += 1;
            }
            keep
        });
        self.stats.retired_pairs = self.stats.retired_pairs.saturating_add(retired);
        retired
    }
}

/// 一个目标 block 的已应用计数与它的保留记录：搬迁时整体移动，避免复制或丢弃序列血统。
type RelocatedPair = (
    (BlockRef, BlockRef),
    AppliedPair,
    Option<BTreeMap<u64, HeldRecord>>,
);

/// 应用一条序号正确的记录，并连带应用已经可以补齐的后继序号。
impl EdgePlane {
    fn apply_in_order(&mut self, pair: EdgePair, record: HeldRecord) -> Result<(), RawInvariant> {
        let state = self.applied.get_mut(&pair).expect("pair 已登记");
        state.applied = state
            .applied
            .checked_add(record.delta)
            .ok_or_else(|| RawInvariant::new("edge 已应用计数溢出"))?;
        state.next_sequence = record
            .sequence
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("edge sequence 溢出"))?;
        self.stats.applied += 1;
        while let Some(records) = self.staged.get_mut(&pair) {
            let next = self.applied[&pair].next_sequence;
            let Some(held) = records.remove(&next) else {
                break;
            };
            if records.is_empty() {
                self.staged.remove(&pair);
            }
            let state = self.applied.get_mut(&pair).expect("pair 已登记");
            state.applied = state
                .applied
                .checked_add(held.delta)
                .ok_or_else(|| RawInvariant::new("edge 已应用计数溢出"))?;
            state.next_sequence = next
                .checked_add(1)
                .ok_or_else(|| RawInvariant::new("edge sequence 溢出"))?;
            state.held = state.held.saturating_sub(1);
            self.stats.applied += 1;
            self.stats.released += 1;
            self.released.push(held);
        }
        Ok(())
    }
}

/// 把一条 `EdgeDelta` 消息转换成平面记录。
pub(crate) fn held_record(delta: &EdgeDelta, node: ReturnNodeId) -> HeldRecord {
    HeldRecord {
        sequence: delta.sequence,
        delta: delta.delta,
        credit: delta.credit,
        node,
    }
}
