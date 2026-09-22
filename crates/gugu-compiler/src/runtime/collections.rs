//! `std.collections` Map/Set 的确定性参照模型。
//!
//! 全部 Map/Set 都是共享身份句柄：复制只复制句柄，通过任一别名修改时所有别名观察同一
//! 逻辑集合。普通读取返回 value 的语义副本；`with_ref` / `for_each_ref` 在一次共享结构访问
//! 期间建立 `ScopedRead` view，不复制 K/V，期间对同一 map 的任何写入都是 panic。`iter()`
//! O(1) 封存 backing，之后的第一次修改先分离，旧快照不变。HashMap 类的迭代顺序是实现序，
//! 随哈希族与 seed 变化；BTreeMap 类按键的全序；SmallMap 在内联阶段按插入序线性查找，
//! 溢出后与 HashMap 相同。

use std::cell::RefCell;
use std::rc::Rc;

use super::hash::{HashFamily, HashOutput, StableKey, StableOrdKey};

/// 集合操作会在语言里 panic 的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CollectionFault {
    /// scoped view 尚未结束时对同一集合写入。
    WriteDuringView,
}

type KeyHash<K> = fn(&K, HashFamily) -> HashOutput;

/// 决定实现序的表表示。键的 hash / 比较函数在构造时按稳定键约束固定。
enum Layout<K> {
    /// 开放寻址哈希表：按键 hash 排列，模拟 control-byte group 的实现序。
    Hash {
        family: HashFamily,
        hash: KeyHash<K>,
    },
    /// 有序树：按键全序。
    Ordered(fn(&K, &K) -> std::cmp::Ordering),
    /// 内联 N 槽的小容量表；溢出后换成哈希表。
    Small {
        limit: usize,
        family: HashFamily,
        hash: KeyHash<K>,
    },
}

struct State<K, V> {
    layout: Layout<K>,
    backing: Rc<Vec<(K, V)>>,
}

/// Map 的共享身份句柄。
pub(crate) struct Map<K, V> {
    state: Rc<RefCell<State<K, V>>>,
}

impl<K, V> Clone for Map<K, V> {
    fn clone(&self) -> Self {
        Self {
            state: Rc::clone(&self.state),
        }
    }
}

impl<K: StableKey, V: Clone> Map<K, V> {
    /// `HashMap` / `SecureHashMap`：由调用方选择哈希族。
    pub(crate) fn hashed(family: HashFamily) -> Self {
        Self::with_layout(Layout::Hash {
            family,
            hash: K::stable_hash,
        })
    }

    /// `SmallMap[K, V, N]`：`limit` 是 comptime 提供的内联槽数。
    pub(crate) fn small(limit: usize, family: HashFamily) -> Self {
        Self::with_layout(Layout::Small {
            limit,
            family,
            hash: K::stable_hash,
        })
    }
}

impl<K: StableOrdKey, V: Clone> Map<K, V> {
    /// `BTreeMap`：按 `Ord + StableOrd` 排序。
    pub(crate) fn ordered() -> Self {
        Self::with_layout(Layout::Ordered(K::cmp))
    }
}

impl<K: Clone + Eq, V: Clone> Map<K, V> {
    fn with_layout(layout: Layout<K>) -> Self {
        Self {
            state: Rc::new(RefCell::new(State {
                layout,
                backing: Rc::new(Vec::new()),
            })),
        }
    }

    /// 两个句柄是否指向同一逻辑集合。
    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.state, &other.state)
    }

    pub(crate) fn len(&self) -> usize {
        self.state.borrow().backing.len()
    }

    /// 普通读取：返回 value 的语义副本。
    pub(crate) fn get(&self, key: &K) -> Option<V> {
        let state = self.state.borrow();
        state.find(key).map(|index| state.backing[index].1.clone())
    }

    /// key 存在时对 value 建立 `ScopedRead` view 并调用 callback。
    pub(crate) fn with_ref<R>(&self, key: &K, f: impl FnOnce(&V) -> R) -> Option<R> {
        let state = self.state.borrow();
        let index = state.find(key)?;
        Some(f(&state.backing[index].1))
    }

    /// 在一次共享结构访问中按实现当前顺序逐项调用 callback，不创建 snapshot。
    pub(crate) fn for_each_ref(&self, mut f: impl FnMut(&K, &V)) {
        let state = self.state.borrow();
        for index in state.order() {
            let (key, value) = &state.backing[index];
            f(key, value);
        }
    }

    /// 插入或替换；返回旧 value 的语义副本。
    pub(crate) fn insert(&self, key: K, value: V) -> Result<Option<V>, CollectionFault> {
        let mut state = self.writable()?;
        let existing = state.find(&key);
        let backing = Rc::make_mut(&mut state.backing);
        Ok(match existing {
            Some(index) => Some(std::mem::replace(&mut backing[index].1, value)),
            None => {
                backing.push((key, value));
                None
            }
        })
    }

    /// 删除；返回被删 value 的语义副本。
    pub(crate) fn remove(&self, key: &K) -> Result<Option<V>, CollectionFault> {
        let mut state = self.writable()?;
        let Some(index) = state.find(key) else {
            return Ok(None);
        };
        Ok(Some(Rc::make_mut(&mut state.backing).remove(index).1))
    }

    /// 键存在时调用一次 `f`，把当前 value 的语义副本交给它并用返回值替换槽。
    pub(crate) fn update(&self, key: &K, f: impl FnOnce(V) -> V) -> Result<bool, CollectionFault> {
        let mut state = self.writable()?;
        let Some(index) = state.find(key) else {
            return Ok(false);
        };
        let backing = Rc::make_mut(&mut state.backing);
        let current = backing[index].1.clone();
        backing[index].1 = f(current);
        Ok(true)
    }

    /// 按键建立 Entry；只传入和返回语义副本。
    pub(crate) fn entry(&self, key: K) -> Entry<K, V> {
        Entry {
            map: self.clone(),
            key,
        }
    }

    /// 快照迭代：O(1) 封存 backing，之后任一别名的修改先分离。
    pub(crate) fn iter(&self) -> MapIter<K, V> {
        let state = self.state.borrow();
        MapIter {
            snapshot: Rc::clone(&state.backing),
            order: state.order().into_iter(),
        }
    }

    /// backing 当前是否被快照封存。
    pub(crate) fn is_sealed(&self) -> bool {
        Rc::strong_count(&self.state.borrow().backing) > 1
    }

    /// SmallMap 是否仍在内联阶段。
    pub(crate) fn is_inline(&self) -> bool {
        let state = self.state.borrow();
        matches!(state.layout, Layout::Small { limit, .. } if state.backing.len() <= limit)
    }

    fn writable(&self) -> Result<std::cell::RefMut<'_, State<K, V>>, CollectionFault> {
        self.state
            .try_borrow_mut()
            .map_err(|_| CollectionFault::WriteDuringView)
    }
}

impl<K: Clone + Eq, V> State<K, V> {
    fn find(&self, key: &K) -> Option<usize> {
        self.backing
            .iter()
            .position(|(existing, _)| existing == key)
    }

    /// 实现当前顺序。
    fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.backing.len()).collect();
        match &self.layout {
            Layout::Hash { family, hash } => self.sort_by_hash(&mut order, *family, *hash),
            Layout::Ordered(compare) => {
                order.sort_by(|&left, &right| {
                    compare(&self.backing[left].0, &self.backing[right].0)
                });
            }
            Layout::Small {
                limit,
                family,
                hash,
            } => {
                if self.backing.len() > *limit {
                    self.sort_by_hash(&mut order, *family, *hash);
                }
            }
        }
        order
    }

    fn sort_by_hash(&self, order: &mut [usize], family: HashFamily, hash: KeyHash<K>) {
        let hashes: Vec<u128> = self
            .backing
            .iter()
            .map(|(key, _)| hash_bits(hash(key, family)))
            .collect();
        order.sort_by_key(|&index| (hashes[index], index));
    }
}

fn hash_bits(output: HashOutput) -> u128 {
    match output {
        HashOutput::Bits64(bits) => u128::from(bits),
        HashOutput::Bits128(bits) => bits,
    }
}

/// `entry()` 结果：所有操作只传递语义副本。
pub(crate) struct Entry<K, V> {
    map: Map<K, V>,
    key: K,
}

impl<K: Clone + Eq, V: Clone> Entry<K, V> {
    /// 键存在时用副本调用 `f` 并替换槽。
    pub(crate) fn and_modify(self, f: impl FnOnce(V) -> V) -> Result<Self, CollectionFault> {
        self.map.update(&self.key, f)?;
        Ok(self)
    }

    /// 键不存在时插入；返回槽内 value 的语义副本。
    pub(crate) fn or_insert(self, value: V) -> Result<V, CollectionFault> {
        self.or_insert_with(|| value)
    }

    /// 键不存在时才调用 `f` 插入；返回槽内 value 的语义副本。
    pub(crate) fn or_insert_with(self, f: impl FnOnce() -> V) -> Result<V, CollectionFault> {
        if let Some(existing) = self.map.get(&self.key) {
            return Ok(existing);
        }
        let value = f();
        self.map.insert(self.key, value.clone())?;
        Ok(value)
    }
}

/// 创建时的快照迭代器；产生元素的语义副本。
pub(crate) struct MapIter<K, V> {
    snapshot: Rc<Vec<(K, V)>>,
    order: std::vec::IntoIter<usize>,
}

impl<K: Clone, V: Clone> Iterator for MapIter<K, V> {
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> {
        let index = self.order.next()?;
        Some(self.snapshot[index].clone())
    }
}

/// Set 是键到 unit 的 Map；元素约束与对应 Map 的键约束相同。
pub(crate) struct Set<T> {
    map: Map<T, ()>,
}

impl<T> Clone for Set<T> {
    fn clone(&self) -> Self {
        Self {
            map: self.map.clone(),
        }
    }
}

impl<T: StableKey> Set<T> {
    pub(crate) fn hashed(family: HashFamily) -> Self {
        Self {
            map: Map::hashed(family),
        }
    }
}

impl<T: StableOrdKey> Set<T> {
    pub(crate) fn ordered() -> Self {
        Self {
            map: Map::ordered(),
        }
    }
}

impl<T: Clone + Eq> Set<T> {
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn contains(&self, value: &T) -> bool {
        self.map.get(value).is_some()
    }

    /// 新元素返回 true。
    pub(crate) fn insert(&self, value: T) -> Result<bool, CollectionFault> {
        Ok(self.map.insert(value, ())?.is_none())
    }

    pub(crate) fn remove(&self, value: &T) -> Result<bool, CollectionFault> {
        Ok(self.map.remove(value)?.is_some())
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = T> {
        self.map.iter().map(|(value, ())| value)
    }

    /// 借用一次共享结构访问，按实现当前顺序访问元素。
    pub(crate) fn for_each_ref(&self, mut f: impl FnMut(&T)) {
        self.map.for_each_ref(|value, ()| f(value));
    }
}
