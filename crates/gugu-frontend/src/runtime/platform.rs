//! Linux 与 Windows 共用的确定性平台 range 实现。
//!
//! 两个目标只差一组常量：页大小、huge-page 阈值、guard 字节数与平台容量。实现是同一份，
//! 因此「Linux fake platform 与 Windows fake platform 的错误映射一致」是结构事实而不是
//! 需要靠测试维持的约定；`platform_schema` 的 verifier 再按 `FaultClass` 逐类比对两个
//! profile 的失败映射表，任何一侧漂移都会被拒绝。
//!
//! commit 以页为粒度记账：owner 的 arena range 是平台 range，domain extent 是它的子区间，
//! trim 只在 extent 上撤销物理页。`range_reserved_bytes` 与 `runtime_committed_bytes` 因此
//! 是同一段地址空间的两个互斥分区，而不是两次独立统计。
//!
//! 这里不发起系统调用：基址单调递增、编号永不复用、wait/wake 用字表确定性建模，全部错误
//! 都可重现。

use super::provider::{
    CommitBitmap, DumpPolicy, FAKE_RANGE_BASE, FaultClass, GuardEdges, ProviderError,
    ProviderStats, RangeDescriptor, RangeId, RangeProvider, RangeState, WaitOutcome, WaitWordId,
};
use super::slab::{MemoryDomainId, RuntimeSeed};
use crate::target::{OperatingSystem, TargetName};

/// 首批 adapter 的两个目标 profile。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformProfile {
    /// Linux x86_64：syscall 直接 `mmap`，4 KiB 页。
    Linux,
    /// Windows x86_64：薄 IAT 入口，4 KiB 粒度。
    Windows,
}

impl PlatformProfile {
    /// 全部 profile 的稠密登记顺序。
    pub(crate) const ALL: [Self; 2] = [Self::Linux, Self::Windows];

    /// 返回 profile 名；契约编码与 `ImagePlan` 都使用它。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }

    /// 返回该 profile 的平台常量。
    pub(crate) const fn constants(self) -> PlatformConstants {
        match self {
            // Linux：mmap/mprotect/madvise；entropy 来自 getrandom。
            Self::Linux => PlatformConstants {
                profile: self,
                page_bytes: 4096,
                huge_page_bytes: 2 * 1024 * 1024,
                guard_bytes: 4096,
                mapping_limit: 1 << 20,
                entropy_available: true,
                dump_policy_default: DumpPolicy::Included,
                select_scratch_cache_bytes: 65536,
            },
            // Windows：VirtualAlloc/VirtualProtect；entropy 来自 BCryptGenRandom。
            Self::Windows => PlatformConstants {
                profile: self,
                page_bytes: 4096,
                huge_page_bytes: 2 * 1024 * 1024,
                guard_bytes: 4096,
                mapping_limit: 1 << 20,
                entropy_available: true,
                dump_policy_default: DumpPolicy::Included,
                select_scratch_cache_bytes: 65536,
            },
        }
    }

    /// 返回该 profile 下每个失败类别映射到的统一 runtime 分类。
    ///
    /// 两个 profile 目前完全一致；该函数是契约比对与测试断言的唯一入口，任何 profile 想
    /// 分叉都必须先改这里，从而必然进入 `platform_schema` 的 verifier。
    pub(crate) const fn fault_class(self, error: ProviderError) -> FaultClass {
        let _ = self;
        error.fault_class()
    }
}

impl std::fmt::Display for PlatformProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

impl From<TargetName> for PlatformProfile {
    fn from(value: TargetName) -> Self {
        match value.descriptor().os {
            OperatingSystem::Linux => Self::Linux,
            OperatingSystem::Windows => Self::Windows,
        }
    }
}

/// 一个 profile 的平台常量。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PlatformConstants {
    pub(crate) profile: PlatformProfile,
    pub(crate) page_bytes: u64,
    pub(crate) huge_page_bytes: u64,
    /// 每个受保护 range 尾部保留的 guard 字节数。
    pub(crate) guard_bytes: u64,
    /// 同时活跃的 mapping 上界。
    pub(crate) mapping_limit: u64,
    /// 平台 entropy 源是否可用；不可用时 `entropy` 映射到 `ResourceExhausted`。
    pub(crate) entropy_available: bool,
    pub(crate) dump_policy_default: DumpPolicy,
    /// processor-local select scratch cache 的累计字节上限。
    pub(crate) select_scratch_cache_bytes: u64,
}

impl PlatformConstants {
    /// 校验常量自洽：页与 guard 是二次幂、huge page 是页的整数倍且不小于页。
    pub(crate) const fn is_valid(&self) -> bool {
        self.page_bytes.is_power_of_two()
            && self.guard_bytes.is_power_of_two()
            && self.guard_bytes <= self.page_bytes
            && self.huge_page_bytes.is_power_of_two()
            && self.huge_page_bytes >= self.page_bytes
            && self.mapping_limit > 0
    }
}

/// 平台 wait 字的确定性状态。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct WaitWord {
    /// 字的当前值；每次 wake 推进一次，使睡眠前的期望值必然过期。
    value: u64,
    /// 当前睡眠的等待者数。
    sleepers: u32,
    /// 累计唤醒次数。
    wakes: u64,
}

/// 确定性平台替身：基址单调递增、编号永不复用、所有错误都可重现。
#[derive(Clone, Debug)]
pub(crate) struct FakePlatform {
    constants: PlatformConstants,
    ranges: Vec<RangeDescriptor>,
    commits: Vec<CommitBitmap>,
    next_base: u64,
    limit: u64,
    words: Vec<WaitWord>,
    seed: RuntimeSeed,
    stats: ProviderStats,
}

impl FakePlatform {
    /// 以 profile 默认基址与可预留容量创建替身。
    pub(crate) fn new(profile: PlatformProfile, capacity_bytes: u64) -> Self {
        Self::with_capacity(
            profile,
            FAKE_RANGE_BASE,
            capacity_bytes,
            RuntimeSeed::new(0x9e37_79b9_7f4a_7c15),
        )
    }

    /// 以显式基址、容量与 entropy 种子创建替身。
    pub(crate) fn with_capacity(
        profile: PlatformProfile,
        base: u64,
        capacity_bytes: u64,
        seed: RuntimeSeed,
    ) -> Self {
        Self {
            constants: profile.constants(),
            ranges: Vec::new(),
            commits: Vec::new(),
            next_base: base,
            limit: base.saturating_add(capacity_bytes),
            words: Vec::new(),
            seed,
            stats: ProviderStats::default(),
        }
    }

    /// 返回平台常量。
    pub(crate) const fn constants(&self) -> PlatformConstants {
        self.constants
    }

    /// 返回 profile。
    pub(crate) const fn profile(&self) -> PlatformProfile {
        self.constants.profile
    }

    /// 返回处于给定状态的 range 数量。
    pub(crate) fn count_in(&self, state: RangeState) -> usize {
        self.ranges
            .iter()
            .filter(|range| range.state == state)
            .count()
    }

    /// 返回某个 range 已提交的页数。
    pub(crate) fn committed_pages(&self, range: RangeId) -> u64 {
        self.commits
            .get(range.index())
            .map_or(0, CommitBitmap::committed_pages)
    }

    /// 登记一个新的 wait 字并返回其稳定编号。
    pub(crate) fn register_wait_word(&mut self, value: u64) -> WaitWordId {
        let id = WaitWordId(u32::try_from(self.words.len()).expect("wait 字数量不会超过 u32"));
        self.words.push(WaitWord {
            value,
            sleepers: 0,
            wakes: 0,
        });
        id
    }

    /// 返回 wait 字的当前值。
    pub(crate) fn word_value(&self, word: WaitWordId) -> Option<u64> {
        self.words.get(word.index()).map(|entry| entry.value)
    }

    /// 返回 wait 字上当前睡眠的等待者数。
    pub(crate) fn word_sleepers(&self, word: WaitWordId) -> Option<u32> {
        self.words.get(word.index()).map(|entry| entry.sleepers)
    }

    /// 返回 wait 字累计唤醒次数。
    pub(crate) fn word_wakes(&self, word: WaitWordId) -> Option<u64> {
        self.words.get(word.index()).map(|entry| entry.wakes)
    }

    fn align_up(address: u64, alignment: u64) -> Option<u64> {
        let mask = alignment - 1;
        address.checked_add(mask).map(|value| value & !mask)
    }

    fn reject(&mut self, error: ProviderError) -> ProviderError {
        self.stats.rejected_requests += 1;
        error
    }

    /// 返回 range 的稠密下标；未登记时返回 `None`。
    fn range_index(&self, range: RangeId) -> Result<Option<usize>, ProviderError> {
        Ok((range.index() < self.ranges.len()).then_some(range.index()))
    }

    /// 重新计算两个当前量口径。
    ///
    /// 未提交的虚拟字节计入 `reserved_bytes`，已提交的物理页计入 `committed_bytes`；两者之和
    /// 恒等于活跃 range 的总字节数，因此同一个字节不可能同时出现在两个口径里。
    fn recompute(&mut self) {
        let page = self.constants.page_bytes;
        let mut reserved = 0_u64;
        let mut committed = 0_u64;
        let mut guarded = 0_u64;
        for (index, range) in self.ranges.iter().enumerate() {
            if range.state == RangeState::Released {
                continue;
            }
            let committed_bytes = self.commits[index].committed_pages() * page;
            committed += committed_bytes;
            reserved += range.bytes - committed_bytes;
            for guard_page in 0..range.guard_low_bytes / page {
                guarded += u64::from(self.commits[index].is_committed(guard_page)) * page;
            }
            for guard_page in (range.bytes - range.guard_bytes) / page..range.bytes / page {
                guarded += u64::from(self.commits[index].is_committed(guard_page)) * page;
            }
        }
        self.stats.reserved_bytes = reserved;
        self.stats.committed_bytes = committed;
        self.stats.guarded_bytes = guarded;
    }

    /// 把字节区间换算成页区间；越界、非页对齐或长度为 0 时报错。
    fn page_span(&self, bytes: u64, offset: u64, len: u64) -> Result<(u64, u64), ProviderError> {
        let page = self.constants.page_bytes;
        if len == 0 || !offset.is_multiple_of(page) || !len.is_multiple_of(page) {
            return Err(ProviderError::InvalidSubRange);
        }
        let Some(end) = offset.checked_add(len) else {
            return Err(ProviderError::ArithmeticOverflow);
        };
        if end > bytes {
            return Err(ProviderError::InvalidSubRange);
        }
        Ok((offset / page, len / page))
    }
}

impl RangeProvider for FakePlatform {
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
        if self
            .ranges
            .iter()
            .filter(|range| range.state != RangeState::Released)
            .count()
            >= usize::try_from(self.constants.mapping_limit).expect("mapping上限适配宿主")
        {
            return Err(self.reject(ProviderError::MappingLimit));
        }
        let base = Self::align_up(self.next_base, alignment)
            .ok_or_else(|| self.reject(ProviderError::ArithmeticOverflow))?;
        // range 大小按页取整后再登记，使页位图覆盖全部字节且每个子区间都按页对齐。
        let page = self.constants.page_bytes;
        let rounded = bytes
            .checked_next_multiple_of(page)
            .ok_or_else(|| self.reject(ProviderError::ArithmeticOverflow))?;
        let Some(end) = base.checked_add(rounded) else {
            return Err(self.reject(ProviderError::ArithmeticOverflow));
        };
        if end > self.limit {
            return Err(self.reject(ProviderError::OutOfSpace));
        }
        let Ok(id) = u32::try_from(self.ranges.len()) else {
            return Err(self.reject(ProviderError::OutOfSpace));
        };
        self.ranges.push(RangeDescriptor {
            base,
            bytes: rounded,
            alignment,
            domain,
            state: RangeState::Reserved,
            guard_low_bytes: 0,
            guard_bytes: 0,
            dump_policy: self.constants.dump_policy_default,
            huge_page: false,
        });
        self.commits.push(CommitBitmap::new(rounded / page));
        self.next_base = end;
        self.stats.reserved_total += rounded;
        self.recompute();
        Ok(RangeId(id))
    }

    fn commit(&mut self, range: RangeId) -> Result<(), ProviderError> {
        let Some(index) = self.range_index(range)? else {
            return Err(self.reject(ProviderError::UnknownRange));
        };
        match self.ranges[index].state {
            RangeState::Committed => {
                return Err(self.reject(ProviderError::AlreadyCommitted));
            }
            RangeState::Released => return Err(self.reject(ProviderError::DoubleRelease)),
            RangeState::Reserved | RangeState::Decommitted => {}
        }
        let pages = self.commits[index].pages();
        self.commits[index].fill(0, pages, true);
        self.ranges[index].state = RangeState::Committed;
        self.stats.committed_total += self.ranges[index].bytes;
        self.recompute();
        Ok(())
    }

    fn decommit(&mut self, range: RangeId) -> Result<(), ProviderError> {
        let Some(index) = self.range_index(range)? else {
            return Err(self.reject(ProviderError::UnknownRange));
        };
        match self.ranges[index].state {
            RangeState::Released => return Err(self.reject(ProviderError::DoubleRelease)),
            RangeState::Reserved | RangeState::Decommitted => {
                return Err(self.reject(ProviderError::NotCommitted));
            }
            RangeState::Committed => {}
        }
        let pages = self.commits[index].pages();
        self.commits[index].fill(0, pages, false);
        self.ranges[index].state = RangeState::Decommitted;
        self.stats.decommitted_total += self.ranges[index].bytes;
        self.recompute();
        Ok(())
    }

    fn commit_pages(
        &mut self,
        range: RangeId,
        offset: u64,
        bytes: u64,
    ) -> Result<(), ProviderError> {
        let Some(index) = self.range_index(range)? else {
            return Err(self.reject(ProviderError::UnknownRange));
        };
        if self.ranges[index].state == RangeState::Released {
            return Err(self.reject(ProviderError::DoubleRelease));
        }
        let (start, count) = match self.page_span(self.ranges[index].bytes, offset, bytes) {
            Ok(span) => span,
            Err(error) => return Err(self.reject(error)),
        };
        let descriptor = self.ranges[index];
        if offset < descriptor.guard_low_bytes
            || offset
                .checked_add(bytes)
                .is_none_or(|end| end > descriptor.bytes - descriptor.guard_bytes)
        {
            return Err(self.reject(ProviderError::GuardOverlap));
        }
        let changed = self.commits[index].fill(start, count, true);
        self.ranges[index].state = RangeState::Committed;
        self.stats.committed_total += changed * self.constants.page_bytes;
        self.recompute();
        Ok(())
    }

    fn decommit_pages(
        &mut self,
        range: RangeId,
        offset: u64,
        bytes: u64,
    ) -> Result<(), ProviderError> {
        let Some(index) = self.range_index(range)? else {
            return Err(self.reject(ProviderError::UnknownRange));
        };
        if self.ranges[index].state == RangeState::Released {
            return Err(self.reject(ProviderError::DoubleRelease));
        }
        let (start, count) = match self.page_span(self.ranges[index].bytes, offset, bytes) {
            Ok(span) => span,
            Err(error) => return Err(self.reject(error)),
        };
        let changed = self.commits[index].fill(start, count, false);
        self.stats.decommitted_total += changed * self.constants.page_bytes;
        self.recompute();
        Ok(())
    }

    fn release(&mut self, range: RangeId) -> Result<(), ProviderError> {
        let Some(index) = self.range_index(range)? else {
            return Err(self.reject(ProviderError::UnknownRange));
        };
        if self.ranges[index].state == RangeState::Released {
            return Err(self.reject(ProviderError::DoubleRelease));
        }
        self.ranges[index].state = RangeState::Released;
        self.ranges[index].guard_low_bytes = 0;
        self.ranges[index].guard_bytes = 0;
        self.ranges[index].bytes = 0;
        self.commits[index] = CommitBitmap::new(0);
        self.stats.released_ranges += 1;
        self.recompute();
        Ok(())
    }

    fn protect_guard(&mut self, range: RangeId, edges: GuardEdges) -> Result<(), ProviderError> {
        let guard = self.constants.guard_bytes;
        let Some(index) = self.range_index(range)? else {
            return Err(self.reject(ProviderError::UnknownRange));
        };
        if self.ranges[index].state == RangeState::Released {
            return Err(self.reject(ProviderError::DoubleRelease));
        }
        let leading = if edges == GuardEdges::Both { guard } else { 0 };
        let descriptor = &mut self.ranges[index];
        if descriptor.guard_bytes != 0
            || descriptor.guard_low_bytes != 0
            || descriptor.bytes <= guard + leading
        {
            return Err(self.reject(ProviderError::GuardOverlap));
        }
        descriptor.guard_low_bytes = leading;
        descriptor.guard_bytes = guard;
        self.recompute();
        Ok(())
    }

    fn unprotect(&mut self, range: RangeId) -> Result<(), ProviderError> {
        let Some(index) = self.range_index(range)? else {
            return Err(self.reject(ProviderError::UnknownRange));
        };
        if self.ranges[index].guard_bytes == 0 {
            return Err(self.reject(ProviderError::NotGuarded));
        }
        self.ranges[index].guard_low_bytes = 0;
        self.ranges[index].guard_bytes = 0;
        self.recompute();
        Ok(())
    }

    fn zero(&mut self, range: RangeId) -> Result<(), ProviderError> {
        let Some(index) = self.range_index(range)? else {
            return Err(self.reject(ProviderError::UnknownRange));
        };
        if self.ranges[index].state != RangeState::Committed {
            return Err(self.reject(ProviderError::NotCommitted));
        }
        self.stats.zeroed_bytes += self.ranges[index].payload_bytes();
        Ok(())
    }

    fn set_dump_policy(&mut self, range: RangeId, policy: DumpPolicy) -> Result<(), ProviderError> {
        let Some(index) = self.range_index(range)? else {
            return Err(self.reject(ProviderError::UnknownRange));
        };
        if self.ranges[index].state == RangeState::Released {
            return Err(self.reject(ProviderError::DoubleRelease));
        }
        self.ranges[index].dump_policy = policy;
        Ok(())
    }

    fn huge_page_hint(&mut self, range: RangeId) -> Result<(), ProviderError> {
        let Some(index) = self.range_index(range)? else {
            return Err(self.reject(ProviderError::UnknownRange));
        };
        if self.ranges[index].state == RangeState::Released {
            return Err(self.reject(ProviderError::DoubleRelease));
        }
        self.ranges[index].huge_page = true;
        Ok(())
    }

    fn wait(&mut self, word: WaitWordId, expected: u64) -> Result<WaitOutcome, ProviderError> {
        let Some(index) = (word.index() < self.words.len()).then_some(word.index()) else {
            return Err(self.reject(ProviderError::UnknownWaitWord));
        };
        if self.words[index].value != expected {
            return Ok(WaitOutcome::Mismatch);
        }
        self.words[index].sleepers = self.words[index].sleepers.saturating_add(1);
        Ok(WaitOutcome::Woken)
    }

    fn wake(&mut self, word: WaitWordId, count: u32) -> Result<u32, ProviderError> {
        let Some(index) = (word.index() < self.words.len()).then_some(word.index()) else {
            return Err(self.reject(ProviderError::UnknownWaitWord));
        };
        let released = self.words[index].sleepers.min(count);
        self.words[index].sleepers -= released;
        // 每次 wake 推进字值：睡眠前的期望值必然过期，等待者必须重新读取当前值。
        self.words[index].value = self.words[index].value.wrapping_add(1);
        self.words[index].wakes += u64::from(released);
        Ok(released)
    }

    fn entropy(&mut self, bytes: u64) -> Result<Vec<u8>, ProviderError> {
        if !self.constants.entropy_available {
            return Err(self.reject(ProviderError::EntropyUnavailable));
        }
        let Ok(len) = usize::try_from(bytes) else {
            return Err(self.reject(ProviderError::OutOfSpace));
        };
        let mut output = Vec::with_capacity(len);
        while output.len() < len {
            output.extend_from_slice(&self.seed.next().to_le_bytes());
        }
        output.truncate(len);
        self.stats.entropy_bytes += bytes;
        Ok(output)
    }

    fn describe(&self, range: RangeId) -> Option<RangeDescriptor> {
        self.ranges.get(range.index()).copied()
    }

    fn describe_all(&self) -> &[RangeDescriptor] {
        &self.ranges
    }

    fn stats(&self) -> ProviderStats {
        self.stats
    }
}
