//! 平台 range 的四操作接口与确定性替身。
//!
//! 平台 adapter 的完整接口包含 guard、wait/wake、entropy 与 dump policy；这些操作分别
//! 属于其消费者阶段，本模块只固定 raw slab span 与 owner local range cache 依赖的
//! `reserve_aligned`/`commit`/`decommit`/`release` 四操作、range 状态机与账本统计。

use super::slab::MemoryDomainId;

/// 确定性替身的虚拟基址；真实平台的地址由系统调用决定，这里只表达范围关系。
pub(crate) const FAKE_RANGE_BASE: u64 = 0x1000_0000_0000;

/// 一个已预留 range 的稳定编号，永不复用。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RangeId(u32);

impl RangeId {
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// range 的提交状态；decommit 后可以再次 commit，release 是终态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RangeState {
    /// 已预留但尚未 commit。
    Reserved,
    /// 已 commit，可承载 payload。
    Committed,
    /// 曾 commit 并已 decommit。
    Decommitted,
    /// 已 release，不能再 commit 或 decommit。
    Released,
}

/// 一个 range 的稳定描述；raw provenance 校验以它为唯一依据。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RangeDescriptor {
    pub(crate) base: u64,
    pub(crate) bytes: u64,
    pub(crate) alignment: u64,
    pub(crate) domain: MemoryDomainId,
    pub(crate) state: RangeState,
}

impl RangeDescriptor {
    pub(crate) const fn end(&self) -> u64 {
        self.base + self.bytes
    }

    /// 判断半开区间是否落在本 range 内且满足 base 对齐。
    pub(crate) fn contains(&self, address: u64, bytes: u64, alignment: u64) -> bool {
        let Some(end) = address.checked_add(bytes) else {
            return false;
        };
        address >= self.base
            && end <= self.end()
            && alignment.is_power_of_two()
            && address % alignment == 0
    }
}

/// provider 的四操作统计。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProviderStats {
    pub(crate) reserved_bytes: u64,
    pub(crate) committed_bytes: u64,
    pub(crate) decommitted_bytes: u64,
    pub(crate) released_ranges: u64,
    pub(crate) rejected_requests: u64,
}

/// provider 失败分类；Linux 与 Windows 的 fake adapter 使用同一映射表。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderError {
    /// 请求 0 字节。
    ZeroBytes,
    /// 对齐要求不是二次幂。
    NonPowerOfTwoAlignment,
    /// 对齐或长度计算溢出。
    ArithmeticOverflow,
    /// 预留超出虚拟地址上界。
    OutOfSpace,
    /// 引用了未登记的 range 编号。
    UnknownRange,
    /// range 已处于目标状态。
    AlreadyCommitted,
    /// 尚未 commit 就 decommit。
    NotCommitted,
    /// 已 release 的 range 再次 release。
    DoubleRelease,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ZeroBytes => "请求 0 字节",
            Self::NonPowerOfTwoAlignment => "对齐要求不是二次幂",
            Self::ArithmeticOverflow => "对齐或长度计算溢出",
            Self::OutOfSpace => "预留超出虚拟地址上界",
            Self::UnknownRange => "引用未登记的 range 编号",
            Self::AlreadyCommitted => "range 已处于目标状态",
            Self::NotCommitted => "range 尚未 commit",
            Self::DoubleRelease => "已 release 的 range 被再次 release",
        })
    }
}

impl std::error::Error for ProviderError {}

/// 平台 range 的四操作接口。
pub(crate) trait RangeProvider {
    /// 预留至少 `bytes` 个字节、按 `alignment` 对齐的新 range。
    fn reserve_aligned(
        &mut self,
        bytes: u64,
        alignment: u64,
        domain: MemoryDomainId,
    ) -> Result<RangeId, ProviderError>;

    /// 提交一个已预留或已 decommit 的 range。
    fn commit(&mut self, range: RangeId) -> Result<(), ProviderError>;

    /// 取消提交；仅在整批 lease 与 grace 结束后调用。
    fn decommit(&mut self, range: RangeId) -> Result<(), ProviderError>;

    /// 释放 range 编号对应的虚拟地址；编号本身不复用。
    fn release(&mut self, range: RangeId) -> Result<(), ProviderError>;

    /// 返回 range 的稳定描述。
    fn describe(&self, range: RangeId) -> Option<RangeDescriptor>;

    /// 返回累计统计。
    fn stats(&self) -> ProviderStats;
}

/// 确定性平台替身：基址单调递增、编号永不复用、所有错误都可重现。
#[derive(Clone, Debug)]
pub(crate) struct FakeRangeProvider {
    ranges: Vec<RangeDescriptor>,
    next_base: u64,
    limit: u64,
    stats: ProviderStats,
}

impl FakeRangeProvider {
    /// 以确定性的起始基址和可预留容量创建替身。
    pub(crate) fn new(capacity_bytes: u64) -> Self {
        Self::with_capacity(FAKE_RANGE_BASE, capacity_bytes)
    }

    /// 以显式基址和可预留容量创建替身；上界由基址与容量相加得到。
    pub(crate) fn with_capacity(base: u64, capacity_bytes: u64) -> Self {
        Self {
            ranges: Vec::new(),
            next_base: base,
            limit: base.saturating_add(capacity_bytes),
            stats: ProviderStats::default(),
        }
    }

    /// 返回全部已登记 range 的稳定顺序快照。
    pub(crate) fn describe_all(&self) -> &[RangeDescriptor] {
        &self.ranges
    }

    /// 返回处于给定状态的 range 数量。
    pub(crate) fn count_in(&self, state: RangeState) -> usize {
        self.ranges
            .iter()
            .filter(|range| range.state == state)
            .count()
    }

    fn align_up(&mut self, address: u64, alignment: u64) -> Result<u64, ProviderError> {
        let mask = alignment - 1;
        address
            .checked_add(mask)
            .map(|value| value & !mask)
            .ok_or_else(|| self.reject(ProviderError::ArithmeticOverflow))
    }

    fn reject(&mut self, error: ProviderError) -> ProviderError {
        self.stats.rejected_requests += 1;
        error
    }

    fn descriptor_mut(&mut self, range: RangeId) -> Option<&mut RangeDescriptor> {
        self.ranges.get_mut(range.index())
    }
}

impl RangeProvider for FakeRangeProvider {
    fn reserve_aligned(
        &mut self,
        bytes: u64,
        alignment: u64,
        domain: MemoryDomainId,
    ) -> Result<RangeId, ProviderError> {
        if bytes == 0 {
            return Err(self.reject(ProviderError::ZeroBytes));
        }
        if !alignment.is_power_of_two() {
            return Err(self.reject(ProviderError::NonPowerOfTwoAlignment));
        }
        let base = self.align_up(self.next_base, alignment)?;
        let Some(end) = base.checked_add(bytes) else {
            return Err(self.reject(ProviderError::ArithmeticOverflow));
        };
        if end > self.limit {
            return Err(self.reject(ProviderError::OutOfSpace));
        }
        let id = RangeId(
            u32::try_from(self.ranges.len()).map_err(|_| self.reject(ProviderError::OutOfSpace))?,
        );
        self.ranges.push(RangeDescriptor {
            base,
            bytes,
            alignment,
            domain,
            state: RangeState::Reserved,
        });
        self.next_base = end;
        self.stats.reserved_bytes += bytes;
        Ok(id)
    }

    fn commit(&mut self, range: RangeId) -> Result<(), ProviderError> {
        let bytes = match self.descriptor_mut(range) {
            None => return Err(self.reject(ProviderError::UnknownRange)),
            Some(descriptor) => match descriptor.state {
                RangeState::Committed => {
                    return Err(self.reject(ProviderError::AlreadyCommitted));
                }
                RangeState::Released => return Err(self.reject(ProviderError::DoubleRelease)),
                RangeState::Reserved | RangeState::Decommitted => {
                    descriptor.state = RangeState::Committed;
                    descriptor.bytes
                }
            },
        };
        self.stats.committed_bytes += bytes;
        Ok(())
    }

    fn decommit(&mut self, range: RangeId) -> Result<(), ProviderError> {
        let bytes = match self.descriptor_mut(range) {
            None => return Err(self.reject(ProviderError::UnknownRange)),
            Some(descriptor) => match descriptor.state {
                RangeState::Committed => {
                    descriptor.state = RangeState::Decommitted;
                    descriptor.bytes
                }
                RangeState::Released => return Err(self.reject(ProviderError::DoubleRelease)),
                RangeState::Reserved | RangeState::Decommitted => {
                    return Err(self.reject(ProviderError::NotCommitted));
                }
            },
        };
        self.stats.decommitted_bytes += bytes;
        self.stats.committed_bytes = self.stats.committed_bytes.saturating_sub(bytes);
        Ok(())
    }

    fn release(&mut self, range: RangeId) -> Result<(), ProviderError> {
        let committed = match self.descriptor_mut(range) {
            None => return Err(self.reject(ProviderError::UnknownRange)),
            Some(descriptor) if descriptor.state == RangeState::Released => {
                return Err(self.reject(ProviderError::DoubleRelease));
            }
            Some(descriptor) => {
                let committed = descriptor.state == RangeState::Committed;
                let bytes = descriptor.bytes;
                descriptor.state = RangeState::Released;
                descriptor.bytes = 0;
                (committed, bytes)
            }
        };
        if committed.0 {
            self.stats.committed_bytes = self.stats.committed_bytes.saturating_sub(committed.1);
        }
        self.stats.released_ranges += 1;
        Ok(())
    }

    fn describe(&self, range: RangeId) -> Option<RangeDescriptor> {
        self.ranges.get(range.index()).copied()
    }

    fn stats(&self) -> ProviderStats {
        self.stats
    }
}
