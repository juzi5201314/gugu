//! 共享不可变前缀与当前会话追加区。

use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ops::Index;
use std::slice;
use std::sync::Arc;

/// 共享一个不可变前缀，并把新元素保存在独立的追加区。
///
/// 前缀长度在创建后固定，追加区索引紧随其后。`share` 只在前缀为空时把已有追加区移动为
/// 新前缀；它不会为了合并多个区段而复制或展平整张表。
#[derive(Clone, Debug)]
pub struct SharedTable<T> {
    base: Arc<Vec<T>>,
    suffix: Vec<T>,
}

impl<T> SharedTable<T> {
    /// 从一个已共享前缀创建追加区为空的表。
    pub fn from_shared(base: Arc<Vec<T>>) -> Self {
        Self {
            base,
            suffix: Vec::new(),
        }
    }

    /// 冻结当前追加区作为共享前缀；只有当前前缀为空时才移动元素。
    pub fn share(&mut self) {
        if self.base.is_empty() {
            self.base = Arc::new(std::mem::take(&mut self.suffix));
        }
    }

    /// 表中元素总数。
    #[inline]
    pub fn len(&self) -> usize {
        self.base.len() + self.suffix.len()
    }

    /// 表是否为空。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.base.is_empty() && self.suffix.is_empty()
    }

    /// 按稠密全局下标读取元素。
    #[inline]
    pub fn get(&self, index: usize) -> Option<&T> {
        if index < self.base.len() {
            self.base.get(index)
        } else {
            self.suffix.get(index - self.base.len())
        }
    }

    /// 按稠密全局下标读取或修改追加区元素；共享前缀始终不可变。
    #[inline]
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.suffix.get_mut(index.checked_sub(self.base.len())?)
    }

    /// 只访问追加区，供排序等仅作用于会话数据的操作使用。
    #[inline]
    pub fn suffix_mut(&mut self) -> &mut Vec<T> {
        &mut self.suffix
    }

    /// 追加一个元素并返回其全局下标。
    #[inline]
    pub fn push(&mut self, value: T) -> usize {
        let index = self.len();
        self.suffix.push(value);
        index
    }

    /// 追加一组元素。
    #[inline]
    pub fn extend(&mut self, values: impl IntoIterator<Item = T>) {
        self.suffix.extend(values);
    }

    /// 顺序遍历共享前缀与追加区。
    #[inline]
    pub fn iter(&self) -> Iter<'_, T> {
        self.base.iter().chain(self.suffix.iter())
    }

    /// 在完整有序表中执行二分查找，返回全局下标或插入位置。
    #[inline]
    pub fn binary_search_by<F>(&self, mut compare: F) -> Result<usize, usize>
    where
        F: FnMut(&T) -> std::cmp::Ordering,
    {
        match self.base.binary_search_by(&mut compare) {
            Ok(index) => Ok(index),
            Err(index) if index < self.base.len() => Err(index),
            Err(offset) => self
                .suffix
                .binary_search_by(compare)
                .map(|index| offset + index)
                .map_err(|index| offset + index),
        }
    }

    /// 在完整的有序表中查找键，返回稠密全局下标。
    #[inline]
    pub fn binary_search_by_key<K: Ord, F>(&self, key: &K, mut key_of: F) -> Result<usize, usize>
    where
        F: FnMut(&T) -> K,
    {
        self.binary_search_by(|value| key_of(value).cmp(key))
    }
}

/// `SharedTable::iter` 与 `&SharedTable` 的迭代器类型。
pub type Iter<'a, T> = std::iter::Chain<slice::Iter<'a, T>, slice::Iter<'a, T>>;

impl<T> Default for SharedTable<T> {
    fn default() -> Self {
        Self {
            base: Arc::new(Vec::new()),
            suffix: Vec::new(),
        }
    }
}

impl<T> From<Vec<T>> for SharedTable<T> {
    fn from(suffix: Vec<T>) -> Self {
        Self {
            base: Arc::new(Vec::new()),
            suffix,
        }
    }
}

impl<T> FromIterator<T> for SharedTable<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl<T> Extend<T> for SharedTable<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.suffix.extend(iter);
    }
}

impl<T> Index<usize> for SharedTable<T> {
    type Output = T;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("SharedTable 下标越界")
    }
}

impl<'a, T> IntoIterator for &'a SharedTable<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T: PartialEq> PartialEq for SharedTable<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(left, right)| left == right)
    }
}

impl<T: Eq> Eq for SharedTable<T> {}

impl<T: Serialize> Serialize for SharedTable<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.len()))?;
        for value in self {
            sequence.serialize_element(value)?;
        }
        sequence.end()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for SharedTable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::SharedTable;
    use crate::canonical;

    #[test]
    fn append_shares_frozen_prefix_without_mutating_sibling() {
        let mut base = SharedTable::from(vec![10, 20]);
        base.share();
        let mut session = base.clone();
        assert!(std::ptr::eq(&base[0], &session[0]));
        assert_eq!(session.push(30), 2);
        session.extend([40, 50]);

        assert_eq!(base.iter().copied().collect::<Vec<_>>(), [10, 20]);
        assert_eq!(
            session.iter().copied().collect::<Vec<_>>(),
            [10, 20, 30, 40, 50]
        );
        assert_eq!(session.get(1), Some(&20));
        assert_eq!(session.get(2), Some(&30));
        assert_eq!(session.get(5), None);
        assert_eq!(session.get_mut(1), None, "共享前缀不可变");
        assert!(std::ptr::eq(&base[1], &session[1]), "追加不能复制前缀");
        assert_eq!(session.get_mut(2), Some(&mut 30));
    }

    #[test]
    fn canonical_sequence_matches_vec_across_shared_and_appended_parts() {
        let mut shared = SharedTable::from(vec![1_u32, 2]);
        shared.share();
        shared.extend([3, 4]);
        let flat = vec![1_u32, 2, 3, 4];

        assert_eq!(
            canonical::to_vec(&shared).unwrap(),
            canonical::to_vec(&flat).unwrap()
        );
        assert_eq!(
            canonical::from_slice::<SharedTable<u32>>(&canonical::to_vec(&flat).unwrap()).unwrap(),
            shared
        );
        assert_eq!(SharedTable::from(flat.clone()), shared);
    }

    #[test]
    fn search_spans_both_segments_and_suffix_sorting_is_local() {
        let mut table = SharedTable::from_shared(std::sync::Arc::new(vec![1, 3]));
        table.extend([8, 6, 4]);
        table.suffix_mut().sort_unstable();

        assert_eq!(table.binary_search_by(|value| value.cmp(&6)), Ok(3));
        assert_eq!(table.binary_search_by(|value| value.cmp(&5)), Err(3));
        for key in 0..=10 {
            assert_eq!(
                table.binary_search_by_key(&key, |value| *value),
                [1, 3, 4, 6, 8].binary_search(&key)
            );
        }
        assert_eq!(table.iter().copied().collect::<Vec<_>>(), [1, 3, 4, 6, 8]);
    }
}
