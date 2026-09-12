//! 平台 range 的固定操作接口、失败分类与统计口径。
//!
//! 调用者不需要知道系统调用或 CRT 细节：reserve/commit/decommit/release、guard、
//! wait/wake、entropy、zero 与 dump policy 全部经这里的接口进入，失败统一映射到
//! `FaultClass` 的三类 runtime error。实现见 `platform` 模块的确定性替身。
//!
//! 统计分成两组口径：`reserved_bytes`/`committed_bytes`/`guarded_bytes` 是**当前量**，
//! 对应规范里的 `range_reserved_bytes` 与 `runtime_committed_bytes`；其余字段是**累计量**，
//! 只用于诊断，不参与 limit 判断，也不能与当前量相加。

use super::slab::MemoryDomainId;

/// 确定性替身的虚拟基址；真实平台的地址由系统调用决定，这里只表达范围关系。
pub(crate) const FAKE_RANGE_BASE: u64 = 0x1000_0000_0000;

/// 一个已预留 range 的稳定编号，永不复用。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RangeId(pub(crate) u32);

impl RangeId {
    /// 返回编号原值。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    /// 返回下标。
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
    /// 曾 commit 并已 decommit；虚拟地址仍然持有。
    Decommitted,
    /// 已 release，不能再 commit 或 decommit。
    Released,
}

impl RangeState {
    /// 返回状态名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Committed => "committed",
            Self::Decommitted => "decommitted",
            Self::Released => "released",
        }
    }

    /// 判断该状态是否仍持有虚拟地址。
    pub(crate) const fn holds_address(self) -> bool {
        matches!(self, Self::Reserved | Self::Committed | Self::Decommitted)
    }

    /// 返回该状态在字节口径上的归属规则。
    ///
    /// range 可以部分 commit，因此口径由规则决定而不是由状态名直接决定：未提交的页永远计入
    /// `range_reserved_bytes`，已提交的页永远计入 `runtime_committed_bytes`，两者的和恒等于
    /// range 的 `bytes`，同一个字节不可能同时计入两边。
    pub(crate) const fn cost(self) -> RangeCost {
        match self {
            Self::Reserved | Self::Decommitted => RangeCost::Uncommitted,
            Self::Committed => RangeCost::Committed,
            Self::Released => RangeCost::None,
        }
    }
}

/// 一个 range 状态对内存口径的贡献规则。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RangeCost {
    /// 未提交的虚拟字节计入 `range_reserved_bytes`。
    Uncommitted,
    /// 已提交的物理页计入 `runtime_committed_bytes`；其中尚未提交的部分仍计入预留。
    Committed,
    /// 已 release，两个口径都不计入。
    None,
}

impl RangeCost {
    /// 返回规则名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Uncommitted => "uncommitted-bytes",
            Self::Committed => "committed-bytes",
            Self::None => "none",
        }
    }
}

/// range 的 dump policy；只影响 core dump 包含关系，不改变生命周期。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DumpPolicy {
    /// 不进入 core dump；metadata、owner secret 与 queue link 使用该策略。
    Excluded,
    /// 进入 core dump。
    Included,
}

impl DumpPolicy {
    /// 返回策略名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Excluded => "excluded",
            Self::Included => "included",
        }
    }
}

/// 平台 wait/wake 的稳定字编号；由 provider 登记后发放，稠密且不复用。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct WaitWordId(pub(crate) u32);

impl WaitWordId {
    /// 返回编号原值。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    /// 返回下标。
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// 一次 wait 的结果；`Mismatch` 表示字在睡眠前已被唤醒方修改。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitOutcome {
    /// 观察到的字等于期望值，调用者进入睡眠并被后续 wake 释放。
    Woken,
    /// 观察到的字不等于期望值；调用者必须重新读取当前值，不能沿用旧期望。
    Mismatch,
}

/// arena只在首尾设置保护页；内部slot不改变页权限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardEdges {
    Trailing,
    Both,
}

/// 一个 range 的稳定描述；raw provenance 校验以它为唯一依据。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RangeDescriptor {
    pub(crate) base: u64,
    pub(crate) bytes: u64,
    pub(crate) alignment: u64,
    pub(crate) domain: MemoryDomainId,
    pub(crate) state: RangeState,
    /// 首部guard字节数；未提交guard只占虚拟容量。
    pub(crate) guard_low_bytes: u64,
    /// 尾部 guard 字节数；永不作为普通 payload 返回。
    pub(crate) guard_bytes: u64,
    /// dump policy 当前值。
    pub(crate) dump_policy: DumpPolicy,
    /// 是否已请求 huge-page hint；hint 不是正确性保证。
    pub(crate) huge_page: bool,
}

impl RangeDescriptor {
    /// 返回 range 的结束地址（含 guard）。
    pub(crate) const fn end(&self) -> u64 {
        self.base + self.bytes
    }

    /// 返回可承载 payload 的字节数；guard 部分不计入。
    pub(crate) const fn payload_bytes(&self) -> u64 {
        self.bytes - self.guard_low_bytes - self.guard_bytes
    }

    /// 返回 payload 区的结束地址。
    pub(crate) const fn payload_end(&self) -> u64 {
        self.end() - self.guard_bytes
    }

    /// 判断半开区间是否落在本 range 的 payload 区内且满足 base 对齐。
    pub(crate) fn contains(&self, address: u64, bytes: u64, alignment: u64) -> bool {
        let Some(end) = address.checked_add(bytes) else {
            return false;
        };
        address >= self.base + self.guard_low_bytes
            && end <= self.payload_end()
            && alignment.is_power_of_two()
            && address % alignment == 0
    }
}

/// 一个 range 内已提交页的位图。
///
/// 位下标是 range 内的页序号；commit/decommit 以页为粒度，使 owner 的 extent 可以在自己
/// 的 arena range 内独立撤销物理页。位图按机器字批量操作，页数上界由 range 大小决定。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommitBitmap {
    words: Vec<u64>,
    pages: u64,
    committed: u64,
}

impl CommitBitmap {
    /// 为 `pages` 个页创建全未提交的位图。
    pub(crate) fn new(pages: u64) -> Self {
        let words = usize::try_from(pages.div_ceil(64)).expect("页位图字数适配 usize");
        Self {
            words: vec![0; words],
            pages,
            committed: 0,
        }
    }

    /// 返回页数。
    pub(crate) const fn pages(&self) -> u64 {
        self.pages
    }

    /// 返回已提交页数。
    pub(crate) const fn committed_pages(&self) -> u64 {
        self.committed
    }

    /// 判断第 `page` 页是否已提交。
    pub(crate) fn is_committed(&self, page: u64) -> bool {
        let word = usize::try_from(page / 64).expect("页位图字下标适配 usize");
        self.words[word] & (1_u64 << (page % 64)) != 0
    }

    /// 设置第 `page` 页的提交状态，返回是否发生变化。
    fn set(&mut self, page: u64, value: bool) -> bool {
        let word = usize::try_from(page / 64).expect("页位图字下标适配 usize");
        let mask = 1_u64 << (page % 64);
        let slot = &mut self.words[word];
        let was = *slot & mask != 0;
        if was == value {
            return false;
        }
        if value {
            *slot |= mask;
            self.committed += 1;
        } else {
            *slot &= !mask;
            self.committed -= 1;
        }
        true
    }

    /// 把 `[start, start + count)` 区间的页设为给定状态，返回发生变化的页数。
    pub(crate) fn fill(&mut self, start: u64, count: u64, value: bool) -> u64 {
        let mut changed = 0;
        for page in start..start + count {
            if self.set(page, value) {
                changed += 1;
            }
        }
        changed
    }
}

/// 平台失败的统一分类；Linux 与 Windows 的 fake adapter 使用同一张映射表。
///
/// 映射是契约的一部分：`platform_schema` 的 verifier 逐类比对两个 profile，任何一侧
/// 漂移都会被拒绝。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum FaultClass {
    /// runtime 管理内存无法满足分配。
    OutOfMemory,
    /// 平台资源（映射数、entropy 源等）耗尽。
    ResourceExhausted,
    /// 调用者违反平台协议；这是实现故障，程序不能恢复。
    RuntimeInvariant,
}

impl FaultClass {
    /// 全部类别的稠密登记顺序。
    pub(crate) const ALL: [Self; 3] = [
        Self::OutOfMemory,
        Self::ResourceExhausted,
        Self::RuntimeInvariant,
    ];

    /// 返回类别名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::OutOfMemory => "OutOfMemory",
            Self::ResourceExhausted => "ResourceExhausted",
            Self::RuntimeInvariant => "RuntimeInvariant",
        }
    }
}

/// provider 失败分类。
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
    /// 同时活跃的 mapping 数超过平台上限。
    MappingLimit,
    /// 引用了未登记的 range 编号。
    UnknownRange,
    /// range 已处于目标状态。
    AlreadyCommitted,
    /// 尚未 commit 就 decommit。
    NotCommitted,
    /// 已 release 的 range 再次 release。
    DoubleRelease,
    /// guard 区间与已有 guard 或 payload 重叠。
    GuardOverlap,
    /// 对没有 guard 的 range 调用 unprotect。
    NotGuarded,
    /// dump policy 参数与 range 的登记策略不符。
    DumpPolicyRejected,
    /// 平台 entropy 源不可用。
    EntropyUnavailable,
    /// wait 引用了未登记的字。
    UnknownWaitWord,
    /// 子区间越界、未按页对齐或长度为 0。
    InvalidSubRange,
}

impl ProviderError {
    /// 全部失败的稠密登记顺序；契约 verifier 按它逐项比对两个 profile。
    pub(crate) const ALL: [Self; 15] = [
        Self::ZeroBytes,
        Self::NonPowerOfTwoAlignment,
        Self::ArithmeticOverflow,
        Self::OutOfSpace,
        Self::MappingLimit,
        Self::UnknownRange,
        Self::AlreadyCommitted,
        Self::NotCommitted,
        Self::DoubleRelease,
        Self::GuardOverlap,
        Self::NotGuarded,
        Self::DumpPolicyRejected,
        Self::EntropyUnavailable,
        Self::UnknownWaitWord,
        Self::InvalidSubRange,
    ];

    /// 返回稳定的失败名；契约编码与诊断都使用它。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ZeroBytes => "zero-bytes",
            Self::NonPowerOfTwoAlignment => "non-power-of-two-alignment",
            Self::ArithmeticOverflow => "arithmetic-overflow",
            Self::OutOfSpace => "out-of-space",
            Self::MappingLimit => "mapping-limit",
            Self::UnknownRange => "unknown-range",
            Self::AlreadyCommitted => "already-committed",
            Self::NotCommitted => "not-committed",
            Self::DoubleRelease => "double-release",
            Self::GuardOverlap => "guard-overlap",
            Self::NotGuarded => "not-guarded",
            Self::DumpPolicyRejected => "dump-policy-rejected",
            Self::EntropyUnavailable => "entropy-unavailable",
            Self::UnknownWaitWord => "unknown-wait-word",
            Self::InvalidSubRange => "invalid-sub-range",
        }
    }

    /// 返回失败映射到的统一 runtime 分类。
    ///
    /// 只有地址空间与平台资源耗尽进入可恢复的 OOM/ResourceExhausted；其余都是调用者
    /// 违反协议，属于 `RuntimeInvariant`，不能被当成“空闲 range”。
    pub(crate) const fn fault_class(self) -> FaultClass {
        match self {
            Self::OutOfSpace => FaultClass::OutOfMemory,
            Self::MappingLimit | Self::EntropyUnavailable => FaultClass::ResourceExhausted,
            Self::ZeroBytes
            | Self::NonPowerOfTwoAlignment
            | Self::ArithmeticOverflow
            | Self::UnknownRange
            | Self::AlreadyCommitted
            | Self::NotCommitted
            | Self::DoubleRelease
            | Self::GuardOverlap
            | Self::NotGuarded
            | Self::DumpPolicyRejected
            | Self::UnknownWaitWord
            | Self::InvalidSubRange => FaultClass::RuntimeInvariant,
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ZeroBytes => "请求 0 字节",
            Self::NonPowerOfTwoAlignment => "对齐要求不是二次幂",
            Self::ArithmeticOverflow => "对齐或长度计算溢出",
            Self::OutOfSpace => "预留超出虚拟地址上界",
            Self::MappingLimit => "同时活跃的 mapping 数超过平台上限",
            Self::UnknownRange => "引用未登记的 range 编号",
            Self::AlreadyCommitted => "range 已处于目标状态",
            Self::NotCommitted => "range 尚未 commit",
            Self::DoubleRelease => "已 release 的 range 被再次 release",
            Self::GuardOverlap => "guard 区间与已有 guard 或 payload 重叠",
            Self::NotGuarded => "range 没有 guard 页",
            Self::DumpPolicyRejected => "dump policy 与 range 的登记策略不符",
            Self::EntropyUnavailable => "平台 entropy 源不可用",
            Self::UnknownWaitWord => "wait 引用未登记的字",
            Self::InvalidSubRange => "子区间越界、未按页对齐或长度为 0",
        })
    }
}

impl std::error::Error for ProviderError {}

/// provider 的统计口径。
///
/// 前三个字段是当前量：`reserved_bytes` 是尚未 commit 的虚拟字节（含已 decommit 的
/// range），`committed_bytes` 是已提交的字节，`guarded_bytes` 是 committed 中受 guard
/// 保护、不能承载 payload 的子集。其余字段是累计量，只用于诊断。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProviderStats {
    pub(crate) reserved_bytes: u64,
    pub(crate) committed_bytes: u64,
    pub(crate) guarded_bytes: u64,
    pub(crate) reserved_total: u64,
    pub(crate) committed_total: u64,
    pub(crate) decommitted_total: u64,
    pub(crate) released_ranges: u64,
    pub(crate) zeroed_bytes: u64,
    pub(crate) entropy_bytes: u64,
    pub(crate) rejected_requests: u64,
}

impl ProviderStats {
    /// 返回当前物理占用：已 commit 的字节。
    pub(crate) const fn live_bytes(&self) -> u64 {
        self.committed_bytes
    }

    /// 返回当前虚拟占用：尚未 commit 但已预留的字节。
    pub(crate) const fn pending_commit_bytes(&self) -> u64 {
        self.reserved_bytes
    }

    /// 返回可承载 payload 的当前已提交字节；guard 是 committed 的子集，不重复相加。
    pub(crate) const fn payload_bytes(&self) -> u64 {
        self.committed_bytes - self.guarded_bytes
    }

    /// 校验当前量不互相矛盾：guard 只能是 committed 的子集。
    pub(crate) const fn is_consistent(&self) -> bool {
        self.guarded_bytes <= self.committed_bytes
    }
}

/// 平台 range 的固定操作接口。
///
/// 全部操作都在 `PLATFORM_RANGE` domain 的 range 上工作；`reserve_aligned` 允许调用者
/// 指定 payload domain，使 stack span、raw slab 与 large mapping 共享同一套 range 管理。
pub(crate) trait RangeProvider {
    /// 预留至少 `bytes` 个字节、按 `alignment` 对齐的新 range。
    fn reserve_aligned(
        &mut self,
        bytes: u64,
        alignment: u64,
        domain: MemoryDomainId,
    ) -> Result<RangeId, ProviderError>;

    /// 提交整个 range。
    fn commit(&mut self, range: RangeId) -> Result<(), ProviderError>;

    /// 取消提交整个 range；调用者必须已经满足 lease 与 grace 门禁。
    fn decommit(&mut self, range: RangeId) -> Result<(), ProviderError>;

    /// 提交 range 内 `[offset, offset + bytes)` 的页。
    ///
    /// domain 的 extent 是 arena range 的子区间，trim 以 extent 为粒度撤销物理页。
    fn commit_pages(
        &mut self,
        range: RangeId,
        offset: u64,
        bytes: u64,
    ) -> Result<(), ProviderError>;

    /// 取消提交 range 内 `[offset, offset + bytes)` 的页。
    fn decommit_pages(
        &mut self,
        range: RangeId,
        offset: u64,
        bytes: u64,
    ) -> Result<(), ProviderError>;

    /// 释放 range 编号对应的虚拟地址；编号本身不复用。
    fn release(&mut self, range: RangeId) -> Result<(), ProviderError>;

    /// 把range端点保护为guard；允许在尚未提交的reservation上固定保护边界。
    fn protect_guard(&mut self, range: RangeId, edges: GuardEdges) -> Result<(), ProviderError>;

    /// 取消两端guard保护。
    fn unprotect(&mut self, range: RangeId) -> Result<(), ProviderError>;

    /// 显式清零一个已 commit 的 range。
    fn zero(&mut self, range: RangeId) -> Result<(), ProviderError>;

    /// 设置 range 的 dump policy；不改变 range 生命周期，也不能替代 guard。
    fn set_dump_policy(&mut self, range: RangeId, policy: DumpPolicy) -> Result<(), ProviderError>;

    /// 请求 huge-page hint；hint 不是正确性保证。
    fn huge_page_hint(&mut self, range: RangeId) -> Result<(), ProviderError>;

    /// 在字上睡眠；期望值已被修改时返回 `Mismatch`，调用者必须重新读取。
    fn wait(&mut self, word: WaitWordId, expected: u64) -> Result<WaitOutcome, ProviderError>;

    /// 唤醒字上至多 `count` 个等待者，返回实际释放的等待者数。
    fn wake(&mut self, word: WaitWordId, count: u32) -> Result<u32, ProviderError>;

    /// 取得 `bytes` 字节平台 entropy；失败映射到 `ResourceExhausted`。
    fn entropy(&mut self, bytes: u64) -> Result<Vec<u8>, ProviderError>;

    /// 返回 range 的稳定描述。
    fn describe(&self, range: RangeId) -> Option<RangeDescriptor>;

    /// 返回全部 range 的稳定顺序快照。
    fn describe_all(&self) -> &[RangeDescriptor];

    /// 返回累计统计。
    fn stats(&self) -> ProviderStats;
}
