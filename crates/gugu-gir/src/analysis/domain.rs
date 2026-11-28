//! 过程内抽象域：区间、差约束、别名类、memory version 与效果。
//!
//! 热路径按 LocalId/ExprId 稠密下标用 `Vec` 存区间；关系只覆盖 IV、下标和长度。

use crate::hir::{ExprId, LocalId};

/// 有符号整数区间。`lo > hi` 表示空集；`lo == i128::MIN` / `hi == i128::MAX` 表示 ±∞。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Interval {
    pub lo: i128,
    pub hi: i128,
}

impl Interval {
    pub const EMPTY: Self = Self { lo: 1, hi: 0 };
    pub const UNKNOWN: Self = Self {
        lo: i128::MIN,
        hi: i128::MAX,
    };

    pub fn point(value: i128) -> Self {
        Self {
            lo: value,
            hi: value,
        }
    }

    pub fn is_empty(self) -> bool {
        self.lo > self.hi
    }

    pub fn is_unknown(self) -> bool {
        self.lo == i128::MIN && self.hi == i128::MAX
    }

    pub fn singleton(self) -> Option<i128> {
        (self.lo == self.hi).then_some(self.lo)
    }

    pub fn contains(self, value: i128) -> bool {
        !self.is_empty() && self.lo <= value && value <= self.hi
    }

    pub fn join(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Self {
            lo: self.lo.min(other.lo),
            hi: self.hi.max(other.hi),
        }
    }

    pub fn meet(self, other: Self) -> Self {
        if self.is_empty() || other.is_empty() {
            return Self::EMPTY;
        }
        Self {
            lo: self.lo.max(other.lo),
            hi: self.hi.min(other.hi),
        }
    }

    pub fn widen(self, next: Self) -> Self {
        if self.is_empty() {
            return next;
        }
        if next.is_empty() {
            return self;
        }
        Self {
            lo: if next.lo < self.lo {
                i128::MIN
            } else {
                self.lo
            },
            hi: if next.hi > self.hi {
                i128::MAX
            } else {
                self.hi
            },
        }
    }

    pub fn add(self, other: Self) -> Self {
        binary(self, other, i128::checked_add)
    }

    pub fn sub(self, other: Self) -> Self {
        binary(self, other, i128::checked_sub)
    }

    pub fn neg(self) -> Self {
        if self.is_empty() {
            return Self::EMPTY;
        }
        let hi = match self.lo.checked_neg() {
            Some(value) => value,
            None => return Self::UNKNOWN,
        };
        let lo = match self.hi.checked_neg() {
            Some(value) => value,
            None => return Self::UNKNOWN,
        };
        Self { lo, hi }
    }
}

fn binary(left: Interval, right: Interval, op: fn(i128, i128) -> Option<i128>) -> Interval {
    if left.is_empty() || right.is_empty() {
        return Interval::EMPTY;
    }
    if left.is_unknown() || right.is_unknown() {
        return Interval::UNKNOWN;
    }
    let a = op(left.lo, right.lo);
    let b = op(left.lo, right.hi);
    let c = op(left.hi, right.lo);
    let d = op(left.hi, right.hi);
    match (a, b, c, d) {
        (Some(a), Some(b), Some(c), Some(d)) => Interval {
            lo: a.min(b).min(c).min(d),
            hi: a.max(b).max(c).max(d),
        },
        _ => Interval::UNKNOWN,
    }
}

/// `left <= right + offset`。
///
/// 两个键以 `ValueKey::packed` 的 u64 形式存储：汇合求交与 `relate` 的有序插入是
/// 抽象解释最热路径，派生 `Ord` 由此退化为至多三次基本整数比较，不再经过枚举
/// 判别的嵌套比较调用（O0 测试 profile 下每层调用都直接计入墙钟）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Relation {
    left: u64,
    right: u64,
    offset: i128,
}

impl Relation {
    pub fn new(left: ValueKey, right: ValueKey, offset: i128) -> Self {
        Self {
            left: left.packed(),
            right: right.packed(),
            offset,
        }
    }

    pub fn left(self) -> u64 {
        self.left
    }

    pub fn right(self) -> u64 {
        self.right
    }

    pub fn offset(self) -> i128 {
        self.offset
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ValueKey {
    Local(LocalId),
    Expr(ExprId),
    LocalLen(LocalId),
    ExprLen(ExprId),
}

impl ValueKey {
    /// 打包序键：变体序号占第 32 位、u32 下标占低 32 位，整数比较即全序。
    /// 变体序号与声明序（Local < Expr < LocalLen < ExprLen）一致；`Relation`
    /// 以此存储，打包成本只在构造时付一次。
    pub fn packed(self) -> u64 {
        match self {
            Self::Local(id) => u64::from(id.0),
            Self::Expr(id) => (1 << 32) | u64::from(id.0),
            Self::LocalLen(id) => (2 << 32) | u64::from(id.0),
            Self::ExprLen(id) => (3 << 32) | u64::from(id.0),
        }
    }

    /// `packed` 的逆：高位变体序号译回变体，下标取低 32 位。
    pub fn unpack(packed: u64) -> Self {
        let index = (packed & 0xffff_ffff) as u32;
        match packed >> 32 {
            0 => Self::Local(LocalId(index)),
            1 => Self::Expr(ExprId(index)),
            2 => Self::LocalLen(LocalId(index)),
            _ => Self::ExprLen(ExprId(index)),
        }
    }
}

/// 打包键是否引用 `local`（Local 或 LocalLen 两种变体，见 `ValueKey::packed`）。
fn refers_local(packed: u64, local: LocalId) -> bool {
    let variant = packed >> 32;
    (variant == 0 || variant == 2) && (packed & 0xffff_ffff) as u32 == local.0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AliasClass {
    Unique(LocalId),
    Heap,
    Foreign,
    Static(u32),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EffectFacts {
    pub panic: bool,
    pub allocate: bool,
    pub suspend: bool,
    pub foreign: bool,
    pub cow_seal: bool,
    pub resource_publish: bool,
    pub alias_heap: bool,
    pub call_unknown: bool,
    pub reads_hidden: bool,
    pub writes_hidden: bool,
}

impl EffectFacts {
    pub fn join(&mut self, other: Self) {
        self.panic |= other.panic;
        self.allocate |= other.allocate;
        self.suspend |= other.suspend;
        self.foreign |= other.foreign;
        self.cow_seal |= other.cow_seal;
        self.resource_publish |= other.resource_publish;
        self.alias_heap |= other.alias_heap;
        self.call_unknown |= other.call_unknown;
        self.reads_hidden |= other.reads_hidden;
        self.writes_hidden |= other.writes_hidden;
    }
}

/// 每个程序点的抽象状态；不可达状态不携带事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbstractState {
    pub reachable: bool,
    pub local_range: Vec<Interval>,
    pub expr_range: Vec<Interval>,
    pub local_len: Vec<Interval>,
    pub expr_len: Vec<Interval>,
    pub init: Vec<bool>,
    pub alias: Vec<AliasClass>,
    pub local_version: Vec<u32>,
    pub heap_version: u32,
    pub foreign_version: u32,
    /// 始终按 `Relation` 顺序排列且不重复，供汇合时做单遍归并求交。
    pub relations: Vec<Relation>,
    /// 本状态是否已包含当前 owner body 的固定关系集（`body.relations`）。
    ///
    /// `sync_resolved` 每次调用都把该固定集并入状态，重复归并只是纯扫描；标记为真时
    /// 可直接跳过。只有会收缩关系集的操作（`bump_local`、`bump_heap`，或与未同步状态
    /// 求交）才复位标记，因此 99% 的重复同步都是 O(1)。
    pub relations_synced: bool,
    /// 本状态是否已完成 `sync_resolved` 的全部派生（`resolved` 表的表达式区间与关系集），
    /// 且此后没有任何区间或关系写入。
    ///
    /// 派生是状态上的幂等纯函数：只要状态没变，重算必然得到同一结果。任何区间/关系写入
    /// （`set_range`、`bump_*`、`join`、`widen`）都复位它，因此标记为真时跳过派生严格等价。
    derived_synced: bool,
    pub effects: EffectFacts,
}

impl AbstractState {
    pub fn bottom(locals: usize, exprs: usize) -> Self {
        Self {
            reachable: false,
            local_range: vec![Interval::EMPTY; locals],
            expr_range: vec![Interval::EMPTY; exprs],
            local_len: vec![Interval::EMPTY; locals],
            expr_len: vec![Interval::EMPTY; exprs],
            init: vec![false; locals],
            alias: (0..locals)
                .map(|index| AliasClass::Unique(LocalId(index as u32)))
                .collect(),
            local_version: vec![0; locals],
            heap_version: 0,
            foreign_version: 0,
            relations: Vec::new(),
            relations_synced: false,
            derived_synced: false,
            effects: EffectFacts::default(),
        }
    }

    pub fn entry(locals: usize, exprs: usize, param_count: usize) -> Self {
        let mut state = Self::bottom(locals, exprs);
        state.reachable = true;
        state.local_range.fill(Interval::UNKNOWN);
        state.expr_range.fill(Interval::UNKNOWN);
        state.local_len.fill(Interval::UNKNOWN);
        state.expr_len.fill(Interval::UNKNOWN);
        for initialized in state.init.iter_mut().take(param_count) {
            *initialized = true;
        }
        state
    }

    pub fn range(&self, key: ValueKey) -> Interval {
        if !self.reachable {
            return Interval::EMPTY;
        }
        match key {
            ValueKey::Local(id) => self.local_range[id.index()],
            ValueKey::Expr(id) => self.expr_range[id.index()],
            ValueKey::LocalLen(id) => self.local_len[id.index()],
            ValueKey::ExprLen(id) => self.expr_len[id.index()],
        }
    }

    pub fn set_range(&mut self, key: ValueKey, interval: Interval) {
        if !self.reachable {
            return;
        }
        let slot = match key {
            ValueKey::Local(id) => &mut self.local_range[id.index()],
            ValueKey::Expr(id) => &mut self.expr_range[id.index()],
            ValueKey::LocalLen(id) => &mut self.local_len[id.index()],
            ValueKey::ExprLen(id) => &mut self.expr_len[id.index()],
        };
        if *slot == interval {
            return;
        }
        *slot = interval;
        // 区间实际发生变化时才令已派生的表达式区间失效。
        self.derived_synced = false;
    }

    /// 返回 `sync_resolved` 的派生结果是否仍与当前状态一致。
    pub fn derived_ready(&self) -> bool {
        self.derived_synced
    }

    /// 标记派生结果已按当前状态重算完毕。
    pub fn mark_derived(&mut self) {
        self.derived_synced = true;
    }

    /// 复用已有 `Vec` 缓冲复制状态，避免固定点每块每轮重复分配/释放 8 块堆内存。
    pub fn copy_from(&mut self, other: &Self) {
        self.reachable = other.reachable;
        self.local_range.clone_from(&other.local_range);
        self.expr_range.clone_from(&other.expr_range);
        self.local_len.clone_from(&other.local_len);
        self.expr_len.clone_from(&other.expr_len);
        self.init.clone_from(&other.init);
        self.alias.clone_from(&other.alias);
        self.local_version.clone_from(&other.local_version);
        self.heap_version = other.heap_version;
        self.foreign_version = other.foreign_version;
        self.relations.clone_from(&other.relations);
        self.relations_synced = other.relations_synced;
        self.derived_synced = other.derived_synced;
        self.effects = other.effects;
    }

    /// 复位为不可达态但保留底层 `Vec` 容量供后续复用。
    pub fn reset_unreachable(&mut self) {
        self.reachable = false;
        self.relations.clear();
        self.relations_synced = false;
        self.derived_synced = false;
        self.effects = EffectFacts::default();
    }

    /// 语义相等：两个不可达状态视为相等（忽略保留容量内的旧事实字节）。
    pub fn semantic_eq(&self, other: &Self) -> bool {
        if self.reachable != other.reachable {
            return false;
        }
        if !self.reachable {
            return true;
        }
        self == other
    }

    pub fn join(&mut self, other: &Self) -> bool {
        self.join_common(other, false)
    }

    /// `relations_done` 表示调用方已完成与 `other` 的关系求交（widen 路径），
    /// 跳过第二次求交；此时 `self.relations` 必为 `other.relations` 的子集。
    fn join_common(&mut self, other: &Self, relations_done: bool) -> bool {
        if !other.reachable {
            return false;
        }
        if !self.reachable {
            self.copy_from(other);
            return true;
        }
        let mut changed = false;
        changed |= join_intervals(&mut self.local_range, &other.local_range);
        changed |= join_intervals(&mut self.expr_range, &other.expr_range);
        changed |= join_intervals(&mut self.local_len, &other.local_len);
        changed |= join_intervals(&mut self.expr_len, &other.expr_len);
        for (slot, &ready) in self.init.iter_mut().zip(&other.init) {
            if *slot && !ready {
                *slot = false;
                changed = true;
            }
        }
        for (slot, &alias) in self.alias.iter_mut().zip(&other.alias) {
            if *slot != alias && *slot != AliasClass::Heap {
                *slot = AliasClass::Heap;
                changed = true;
            }
        }
        changed |= join_versions(&mut self.local_version, &other.local_version);
        if other.heap_version > self.heap_version {
            self.heap_version = other.heap_version;
            changed = true;
        }
        if other.foreign_version > self.foreign_version {
            self.foreign_version = other.foreign_version;
            changed = true;
        }
        if !relations_done {
            changed |= intersect_relations(&mut self.relations, &other.relations);
        }
        // 求交只保留双方共有的关系：两边都覆盖 body 关系集时结果仍覆盖，否则必须复位。
        self.relations_synced &= other.relations_synced;
        self.derived_synced = false;
        let before_effects = self.effects;
        self.effects.join(other.effects);
        changed |= self.effects != before_effects;
        changed
    }

    #[cfg(test)]
    pub fn widen(&mut self, other: &Self) -> bool {
        let mut widened_bound = false;
        self.widen_tracked(other, &mut widened_bound)
    }

    /// 与 [`Self::widen`] 相同，并在任意区间端点因外推跳到 `±∞`（严格弱于 `join`）时将
    /// `widened_bound` 置真；未外推时不动点即为精确最小不动点，无需后续 narrowing。
    pub fn widen_tracked(&mut self, other: &Self, widened_bound: &mut bool) -> bool {
        if !other.reachable {
            return false;
        }
        if !self.reachable {
            self.copy_from(other);
            return true;
        }
        let mut changed = false;
        changed |= widen_intervals(&mut self.local_range, &other.local_range, widened_bound);
        changed |= widen_intervals(&mut self.expr_range, &other.expr_range, widened_bound);
        changed |= widen_intervals(&mut self.local_len, &other.local_len, widened_bound);
        changed |= widen_intervals(&mut self.expr_len, &other.expr_len, widened_bound);
        // widen 语义上必然先收缩关系集；求交一次即可，join_common 里跳过重复求交。
        changed |= intersect_relations(&mut self.relations, &other.relations);
        changed |= self.join_common(other, true);
        changed
    }

    pub fn bump_local(&mut self, local: LocalId) {
        let index = local.index();
        if matches!(self.alias[index], AliasClass::Unique(_)) {
            self.alias[index] = AliasClass::Unique(local);
        }
        // 写入后的动态版本不再与入口版本相同；循环内多次写入合并为未知版本。
        self.local_version[index] = u32::MAX;
        self.local_len[index] = Interval::UNKNOWN;
        self.relations.retain(|relation| {
            !refers_local(relation.left(), local) && !refers_local(relation.right(), local)
        });
        // 写入后的本地可能被移出固定关系集，下一处程序点必须重新同步。
        self.relations_synced = false;
        self.derived_synced = false;
    }

    pub fn bump_heap(&mut self) {
        self.heap_version = u32::MAX;
        self.local_range.fill(Interval::UNKNOWN);
        self.local_len.fill(Interval::UNKNOWN);
        self.expr_len.fill(Interval::UNKNOWN);
        self.relations.clear();
        self.relations_synced = false;
        self.derived_synced = false;
    }

    pub fn bump_foreign(&mut self) {
        self.foreign_version = u32::MAX;
        self.effects.foreign = true;
        for alias in &mut self.alias {
            *alias = match *alias {
                AliasClass::Unique(_) => AliasClass::Heap,
                AliasClass::Heap | AliasClass::Foreign => AliasClass::Foreign,
                AliasClass::Static(id) => AliasClass::Static(id),
            };
        }
        self.bump_heap();
    }

    pub fn relate(&mut self, relation: Relation) {
        if !self.reachable {
            return;
        }
        match self.relations.binary_search(&relation) {
            Ok(_) => {}
            Err(index) => self.relations.insert(index, relation),
        }
    }

    /// 输入已排序且不重复；先计缺项，再从尾部就地归并，避免逐项二分与搬移。
    pub fn relate_sorted(&mut self, additions: &[Relation]) {
        if !self.reachable || additions.is_empty() {
            return;
        }
        debug_assert!(additions.windows(2).all(|pair| pair[0] < pair[1]));
        let mut original = 0;
        let mut missing = 0;
        for relation in additions {
            while original < self.relations.len() && self.relations[original] < *relation {
                original += 1;
            }
            missing += usize::from(
                original == self.relations.len() || self.relations[original] != *relation,
            );
        }
        if missing == 0 {
            return;
        }
        original = self.relations.len();
        let mut added = additions.len();
        let mut write = original + missing;
        self.relations.resize(write, additions[0]);
        while added != 0 {
            let relation = additions[added - 1];
            write -= 1;
            if original != 0 && self.relations[original - 1] > relation {
                original -= 1;
                self.relations[write] = self.relations[original];
            } else {
                if original != 0 && self.relations[original - 1] == relation {
                    original -= 1;
                }
                added -= 1;
                self.relations[write] = relation;
            }
        }
        debug_assert_eq!(write, original);
    }
}

fn join_intervals(slots: &mut [Interval], other: &[Interval]) -> bool {
    let mut changed = false;
    for (slot, &next) in slots.iter_mut().zip(other) {
        let joined = slot.join(next);
        if joined != *slot {
            *slot = joined;
            changed = true;
        }
    }
    changed
}

fn widen_intervals(slots: &mut [Interval], other: &[Interval], widened_bound: &mut bool) -> bool {
    let mut changed = false;
    for (slot, &next) in slots.iter_mut().zip(other) {
        let widened = slot.widen(next);
        if widened != *slot {
            if !slot.is_empty() && !next.is_empty() && widened != slot.join(next) {
                *widened_bound = true;
            }
            *slot = widened;
            changed = true;
        }
    }
    changed
}

/// 两边均按 `Relation` 序排列且不重复；就地把 `left` 收缩为与 `right` 的有序交集，
/// 返回是否删除过元素。
///
/// 单遍双指针归并：右指针单调推进，比较次数 O(n+m) 且顺序访存，替代逐元素
/// `binary_search` 的 O(n·log m)；汇合求交是整个抽象解释最热的操作。
fn intersect_relations(left: &mut Vec<Relation>, right: &[Relation]) -> bool {
    debug_assert!(left.is_sorted());
    debug_assert!(right.is_sorted());
    let mut write = 0;
    let mut other = 0;
    for read in 0..left.len() {
        let relation = left[read];
        while other < right.len() && right[other] < relation {
            other += 1;
        }
        if other < right.len() && right[other] == relation {
            left[write] = relation;
            write += 1;
        }
    }
    let removed = left.len() - write;
    left.truncate(write);
    removed != 0
}

fn join_versions(slots: &mut [u32], other: &[u32]) -> bool {
    let mut changed = false;
    for (slot, &next) in slots.iter_mut().zip(other) {
        if next > *slot {
            *slot = next;
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 派生标记只允许以下三种情况保持有效：刚完成派生、克隆、以及不改变区间的读取。
    /// 任何区间或关系写入都必须复位它，否则 `sync_resolved` 会跳过本该重算的派生。
    #[test]
    fn derived_flag_tracks_every_state_write() {
        let mut state = AbstractState::entry(2, 2, 1);
        assert!(!state.derived_ready(), "新建状态尚未派生");
        state.mark_derived();
        assert!(state.derived_ready(), "派生后标记有效");
        assert!(state.clone().derived_ready(), "克隆必须保持标记");
        assert_eq!(
            state.range(ValueKey::Local(LocalId(0))),
            state.clone().range(ValueKey::Local(LocalId(0))),
            "读取不改变状态"
        );
        assert!(state.derived_ready(), "读取不得复位标记");

        state.set_range(ValueKey::Expr(ExprId(0)), Interval::point(3));
        assert!(!state.derived_ready(), "区间写入必须复位标记");

        state.mark_derived();
        state.bump_local(LocalId(0));
        assert!(!state.derived_ready(), "本地写入必须复位标记");

        state.mark_derived();
        state.bump_heap();
        assert!(!state.derived_ready(), "堆写入必须复位标记");

        state.mark_derived();
        let mut other = AbstractState::entry(2, 2, 1);
        other.bump_heap();
        state.join(&other);
        assert!(!state.derived_ready(), "汇合必须复位标记");

        state.mark_derived();
        state.widen(&other);
        assert!(!state.derived_ready(), "widen 必须复位标记");
    }

    #[test]
    fn sorted_union_matches_individual_relations_for_all_small_sets() {
        let relations = [
            Relation::new(ValueKey::Local(LocalId(0)), ValueKey::Expr(ExprId(0)), -1),
            Relation::new(ValueKey::Local(LocalId(0)), ValueKey::Expr(ExprId(0)), 0),
            Relation::new(ValueKey::Expr(ExprId(0)), ValueKey::Local(LocalId(0)), 0),
            Relation::new(
                ValueKey::ExprLen(ExprId(0)),
                ValueKey::LocalLen(LocalId(0)),
                0,
            ),
        ];
        for left in 0_u8..16 {
            for right in 0_u8..16 {
                let mut state = AbstractState::entry(1, 1, 0);
                for (index, &relation) in relations.iter().enumerate() {
                    if left & (1 << index) != 0 {
                        state.relate(relation);
                    }
                }
                let additions: Vec<_> = relations
                    .iter()
                    .enumerate()
                    .filter_map(|(index, &relation)| {
                        (right & (1 << index) != 0).then_some(relation)
                    })
                    .collect();
                let mut expected = state.clone();
                for &relation in &additions {
                    expected.relate(relation);
                }
                state.relate_sorted(&additions);
                assert_eq!(state, expected, "集合 {left:b} 与 {right:b}");
                state.relate_sorted(&additions);
                assert_eq!(state, expected, "重复并集必须幂等");
            }
        }
        let mut unreachable = AbstractState::bottom(1, 1);
        unreachable.relate_sorted(&relations);
        assert!(
            unreachable.relations.is_empty(),
            "不可达状态不得获得关系事实"
        );
    }
}
