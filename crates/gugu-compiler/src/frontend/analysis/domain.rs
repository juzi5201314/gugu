//! 过程内抽象域：区间、差约束、别名类、memory version 与效果。
//!
//! 热路径按 LocalId/ExprId 稠密下标用 `Vec` 存区间；关系只覆盖 IV、下标和长度。

use crate::frontend::hir::{ExprId, LocalId};

/// 有符号整数区间。`lo > hi` 表示空集；`lo == i128::MIN` / `hi == i128::MAX` 表示 ±∞。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Interval {
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct Relation {
    pub left: ValueKey,
    pub right: ValueKey,
    pub offset: i128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum ValueKey {
    Local(LocalId),
    Expr(ExprId),
    LocalLen(LocalId),
    ExprLen(ExprId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AliasClass {
    Unique(LocalId),
    Heap,
    Foreign,
    Static(u32),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EffectFacts {
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
pub(crate) struct AbstractState {
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
    pub relations: Vec<Relation>,
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
            effects: EffectFacts::default(),
        }
    }

    pub fn entry(locals: usize, exprs: usize, param_count: usize) -> Self {
        let mut state = Self::bottom(locals, exprs);
        state.reachable = true;
        for range in &mut state.local_range {
            *range = Interval::UNKNOWN;
        }
        for range in &mut state.expr_range {
            *range = Interval::UNKNOWN;
        }
        for range in &mut state.local_len {
            *range = Interval::UNKNOWN;
        }
        for range in &mut state.expr_len {
            *range = Interval::UNKNOWN;
        }
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
        match key {
            ValueKey::Local(id) => self.local_range[id.index()] = interval,
            ValueKey::Expr(id) => self.expr_range[id.index()] = interval,
            ValueKey::LocalLen(id) => self.local_len[id.index()] = interval,
            ValueKey::ExprLen(id) => self.expr_len[id.index()] = interval,
        }
    }

    pub fn join(&mut self, other: &Self) -> bool {
        if !other.reachable {
            return false;
        }
        if !self.reachable {
            *self = other.clone();
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
            if *slot != alias {
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
        let before = self.relations.len();
        self.relations
            .retain(|relation| other.relations.contains(relation));
        changed |= self.relations.len() != before;
        let before_effects = self.effects;
        self.effects.join(other.effects);
        changed |= self.effects != before_effects;
        changed
    }

    pub fn widen(&mut self, other: &Self) -> bool {
        if !other.reachable {
            return false;
        }
        if !self.reachable {
            *self = other.clone();
            return true;
        }
        let mut changed = false;
        changed |= widen_intervals(&mut self.local_range, &other.local_range);
        changed |= widen_intervals(&mut self.expr_range, &other.expr_range);
        changed |= widen_intervals(&mut self.local_len, &other.local_len);
        changed |= widen_intervals(&mut self.expr_len, &other.expr_len);
        let before = self.relations.len();
        self.relations
            .retain(|relation| other.relations.contains(relation));
        changed |= self.relations.len() != before;
        changed |= self.join(other);
        changed
    }

    pub fn bump_local(&mut self, local: LocalId) {
        let index = local.index();
        if matches!(self.alias[index], AliasClass::Unique(_)) {
            self.alias[index] = AliasClass::Unique(local);
        }
        self.local_version[index] = self.local_version[index].saturating_add(1);
        self.local_len[index] = Interval::UNKNOWN;
        self.relations.retain(|relation| {
            relation.left.local() != Some(local) && relation.right.local() != Some(local)
        });
    }

    pub fn bump_heap(&mut self) {
        self.heap_version = self.heap_version.saturating_add(1);
        for len in &mut self.local_len {
            *len = Interval::UNKNOWN;
        }
        for len in &mut self.expr_len {
            *len = Interval::UNKNOWN;
        }
        self.relations.clear();
    }

    pub fn bump_foreign(&mut self) {
        self.foreign_version = self.foreign_version.saturating_add(1);
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
        if !self.reachable || self.relations.contains(&relation) {
            return;
        }
        self.relations.push(relation);
        self.relations.sort_unstable();
    }
}

impl ValueKey {
    fn local(self) -> Option<LocalId> {
        match self {
            Self::Local(id) | Self::LocalLen(id) => Some(id),
            Self::Expr(_) | Self::ExprLen(_) => None,
        }
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

fn widen_intervals(slots: &mut [Interval], other: &[Interval]) -> bool {
    let mut changed = false;
    for (slot, &next) in slots.iter_mut().zip(other) {
        let widened = slot.widen(next);
        if widened != *slot {
            *slot = widened;
            changed = true;
        }
    }
    changed
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
