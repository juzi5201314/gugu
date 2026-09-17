//! 二次幂 extent 的分裂、合并与 trim 门禁。
//!
//! 每个 owner 持有自己的一段 extent arena，按二次幂 class 阶梯用位图管理空闲块。分裂与相邻
//! 合并都只发生在 owner/domain reducer 上：没有共享 buddy tree，没有原子，没有跨 owner 的
//! 空闲结构写。跨 owner 的归还只发布携带 extent 描述符的 `ReturnKind::Extent` 消息，由目标
//! owner 在自己的上下文里合并。
//!
//! 描述符表按稠密 `ExtentId` 存放，点查是连续内存上的下标访问；空闲块只活在位图里，不占用
//! 描述符槽位。slot 回收结束后，descriptor 槽位经 free list 复用并推进 generation，使过期
//! 引用必然被拒绝。
//!
//! decommit 门禁固定为四条同时成立：allocator、scanner、forwarder 三路 lease 全部归零，
//! 没有 live/queued slot，没有在途 return 消息，且 queue-page grace 已经走完固定步数。

use super::model::GRACE_STEPS;
use super::provider::RangeId;
use super::slab::{Epoch, MemoryDomainId, OwnerToken, RawInvariant};

/// extent class 阶梯：从平台页到 huge page，每级都是二次幂且是上一级的两倍。
pub const EXTENT_CLASS_LADDER: [u64; 10] = [
    4096, 8192, 16384, 32768, 65536, 131_072, 262_144, 524_288, 1_048_576, 2_097_152,
];

/// 返回 class 编号对应的字节数。
pub(crate) fn class_bytes(index: u32) -> Option<u64> {
    EXTENT_CLASS_LADDER.get(index as usize).copied()
}

/// 返回能容纳 `bytes` 的最小 class 编号。
pub(crate) fn class_for_bytes(bytes: u64) -> Option<u32> {
    EXTENT_CLASS_LADDER
        .iter()
        .position(|entry| *entry >= bytes)
        .and_then(|index| u32::try_from(index).ok())
}

/// 一个 extent 的稳定编号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ExtentId(u32);

impl ExtentId {
    /// 由编号原值还原。
    ///
    /// 跨 owner 归还消息只携带编号；consumer 用它定位描述符，编号不在表中时校验失败。
    pub(crate) const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// 返回编号原值。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    /// 返回下标。
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// extent 的生命周期状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentState {
    /// 描述符槽位空闲，可由新 extent 复用。
    Vacant,
    /// 已交给 consumer，仍持有 payload。
    Live,
    /// 已赢得 return 线性化点，归还消息在途。
    ReturnQueued,
}

impl ExtentState {
    /// 返回状态名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Vacant => "vacant",
            Self::Live => "live",
            Self::ReturnQueued => "return-queued",
        }
    }
}

/// extent 的 generation；槽位复用时推进，使过期引用必然被拒绝。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ExtentGeneration(u32);

impl ExtentGeneration {
    /// 返回原值。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }
}

/// 三路 lease 的来源。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ExtentLease {
    /// 分配者正在该 extent 上取用 slot。
    Allocator,
    /// scanner 正在遍历该 extent 的 metadata 或 payload。
    Scanner,
    /// forwarder 正在沿该 extent 转发消息。
    Forwarder,
}

impl ExtentLease {
    /// 全部来源的稠密登记顺序。
    pub(crate) const ALL: [Self; 3] = [Self::Allocator, Self::Scanner, Self::Forwarder];

    /// 返回来源名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Allocator => "allocator",
            Self::Scanner => "scanner",
            Self::Forwarder => "forwarder",
        }
    }
}

/// 一个 extent 上的三路独立 lease 计数。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ExtentLeases {
    counts: [u32; 3],
}

impl ExtentLeases {
    /// 返回某一来源的当前计数。
    pub(crate) const fn count(&self, lease: ExtentLease) -> u32 {
        self.counts[lease as usize]
    }

    /// 判断三路是否全部归零。
    pub(crate) const fn is_idle(&self) -> bool {
        self.counts[0] == 0 && self.counts[1] == 0 && self.counts[2] == 0
    }

    /// 返回第一条未归零的来源；全部归零时为 `None`。
    pub(crate) fn outstanding(&self) -> Option<ExtentLease> {
        ExtentLease::ALL
            .into_iter()
            .find(|lease| self.counts[*lease as usize] != 0)
    }

    /// 取得一条 lease；溢出按不变量失败。
    pub(crate) fn acquire(&mut self, lease: ExtentLease) -> Result<(), RawInvariant> {
        let slot = &mut self.counts[lease as usize];
        *slot = slot
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("extent lease 计数溢出"))?;
        Ok(())
    }

    /// 结束一条 lease；没有对应 lease 时报不变量失败。
    pub(crate) fn release(&mut self, lease: ExtentLease) -> Result<(), RawInvariant> {
        let slot = &mut self.counts[lease as usize];
        if *slot == 0 {
            return Err(RawInvariant::new(format!(
                "extent 的 {} lease 未持有却被结束",
                lease.name()
            )));
        }
        *slot -= 1;
        Ok(())
    }
}

/// 一个 extent 的稳定描述。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtentDescriptor {
    pub(crate) id: ExtentId,
    pub(crate) base: u64,
    pub(crate) bytes: u64,
    pub(crate) alignment: u64,
    pub(crate) domain: MemoryDomainId,
    pub(crate) owner: OwnerToken,
    pub(crate) generation: ExtentGeneration,
    pub(crate) state: ExtentState,
    pub(crate) class: u32,
    /// 该 extent 落在哪个 owner arena；归还时按它定位 buddy 位图。
    pub(crate) owner_index: u32,
    pub(crate) huge_page: bool,
    pub(crate) leases: ExtentLeases,
}

impl ExtentDescriptor {
    /// 返回结束地址。
    pub(crate) const fn end(&self) -> u64 {
        self.base + self.bytes
    }

    /// 判断半开区间是否落在本 extent 内且满足对齐。
    pub(crate) fn contains(&self, address: u64, bytes: u64, alignment: u64) -> bool {
        let Some(end) = address.checked_add(bytes) else {
            return false;
        };
        address >= self.base
            && end <= self.end()
            && alignment.is_power_of_two()
            && address.is_multiple_of(alignment)
    }
}

/// trim 被拒绝的具体原因；诊断必须点名是哪一条门禁。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrimBlocked {
    /// 某一路 lease 尚未归零。
    Lease(ExtentLease),
    /// 仍有 live slot。
    LiveSlots(u32),
    /// 仍有已排队但未被消费的 slot。
    QueuedSlots(u32),
    /// 仍有在途 return 消息。
    PendingReturns(u32),
    /// queue-page grace 尚未走完。
    GracePending {
        /// 已完成步数。
        completed: u32,
        /// 需要的步数。
        required: u32,
    },
}

impl TrimBlocked {
    /// 返回稳定原因名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Lease(_) => "outstanding-lease",
            Self::LiveSlots(_) => "live-slots",
            Self::QueuedSlots(_) => "queued-slots",
            Self::PendingReturns(_) => "pending-returns",
            Self::GracePending { .. } => "grace-pending",
        }
    }

    /// 返回带数量的可读描述；lease 与 grace 原因点名具体来源与步数。
    pub(crate) fn describe(self) -> String {
        match self {
            Self::Lease(lease) => format!("{} lease 尚未归零", lease.name()),
            Self::LiveSlots(count) => format!("仍有 {count} 个 live slot"),
            Self::QueuedSlots(count) => format!("仍有 {count} 个已排队 slot"),
            Self::PendingReturns(count) => format!("仍有 {count} 条在途 return 消息"),
            Self::GracePending {
                completed,
                required,
            } => format!("queue-page grace 仅完成 {completed}/{required} 步"),
        }
    }
}

impl std::fmt::Display for TrimBlocked {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.describe())
    }
}

/// 一次批量 trim 的结果：成功撤销物理页的 extent 数量与被门禁拒绝的原因。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TrimReport {
    /// 已成功 trim 并归还 buddy 阶梯的 extent 数量。
    pub(crate) trimmed: u32,
    /// 被门禁拒绝的 extent 及原因；诊断必须原样呈现。
    pub(crate) blocked: Vec<(ExtentId, TrimBlocked)>,
}

impl TrimReport {
    /// 返回被拒绝的 extent 数量。
    pub(crate) fn blocked_count(&self) -> u32 {
        u32::try_from(self.blocked.len()).expect("被拒绝的 extent 数量适配 u32")
    }

    /// 返回第一条拒绝原因的稳定名；没有拒绝时返回 `None`。
    pub(crate) fn first_reason(&self) -> Option<&'static str> {
        self.blocked.first().map(|(_, reason)| reason.name())
    }

    /// 返回全部拒绝原因的可读描述。
    pub(crate) fn describe(&self) -> Vec<String> {
        self.blocked
            .iter()
            .map(|(extent, reason)| format!("extent {}：{reason}", extent.raw()))
            .collect()
    }
}

/// 一个 extent 上的 slot 占用；由 slab 表统计后传入，保持唯一真相源。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ExtentOccupancy {
    pub(crate) live_slots: u32,
    pub(crate) queued_slots: u32,
    pub(crate) pending_returns: u32,
}

/// 一次 trim 的进行中凭据；grace 步数由显式 epoch 推进。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TrimTicket {
    extent: ExtentId,
    last_epoch: Epoch,
    completed: u32,
}

impl TrimTicket {
    /// 返回目标 extent。
    pub(crate) const fn extent(&self) -> ExtentId {
        self.extent
    }

    /// 返回已完成的 grace 步数。
    pub(crate) const fn completed(&self) -> u32 {
        self.completed
    }

    /// 判断 grace 是否走完。
    pub(crate) const fn is_ready(&self) -> bool {
        self.completed >= GRACE_STEPS
    }
}

/// 一个 owner 的 extent arena 与空闲块位图。
#[derive(Clone, Debug, Eq, PartialEq)]
struct OwnerExtentSpace {
    token: OwnerToken,
    /// arena 所属 memory domain；raw 与 Resource 各自持有独立 arena。
    domain: MemoryDomainId,
    /// arena 对应的平台 range；extent 是它的子区间，commit/decommit 以页为粒度作用于它。
    range: RangeId,
    base: u64,
    bytes: u64,
    /// 每 class 的空闲块位图；位下标是该 class 下的块编号。
    free: Vec<Vec<u64>>,
    /// 每 class 的空闲块数量；与位图 popcount 保持一致。
    free_counts: Vec<u32>,
}

impl OwnerExtentSpace {
    fn new(
        token: OwnerToken,
        domain: MemoryDomainId,
        range: RangeId,
        base: u64,
        bytes: u64,
    ) -> Result<Self, RawInvariant> {
        let largest = *EXTENT_CLASS_LADDER.last().expect("class 阶梯非空");
        if !base.is_multiple_of(largest) {
            return Err(RawInvariant::new("extent arena 基址未按最大 class 对齐"));
        }
        if bytes == 0 || !bytes.is_multiple_of(largest) {
            return Err(RawInvariant::new(
                "extent arena 容量必须是最大 class 的整数倍",
            ));
        }
        let mut free = Vec::with_capacity(EXTENT_CLASS_LADDER.len());
        let mut free_counts = Vec::with_capacity(EXTENT_CLASS_LADDER.len());
        for entry in EXTENT_CLASS_LADDER {
            let blocks = bytes / entry;
            let words = blocks.div_ceil(64);
            free.push(vec![
                0_u64;
                usize::try_from(words).expect("位图字数适配 usize")
            ]);
            free_counts.push(0);
        }
        let mut space = Self {
            token,
            domain,
            range,
            base,
            bytes,
            free,
            free_counts,
        };
        // 初始时整个 arena 是一个最大 class 的空闲块。
        let top = u32::try_from(EXTENT_CLASS_LADDER.len() - 1).expect("顶层 class 编号适配 u32");
        space.set_free(top, 0, true);
        Ok(space)
    }

    fn blocks(&self, class: u32) -> u64 {
        self.bytes / EXTENT_CLASS_LADDER[class as usize]
    }

    fn test_free(&self, class: u32, index: u64) -> bool {
        let word = usize::try_from(index / 64).expect("位图字下标适配 usize");
        let bit = u32::try_from(index % 64).expect("位下标适配 u32");
        self.free[class as usize][word] & (1_u64 << bit) != 0
    }

    fn set_free(&mut self, class: u32, index: u64, value: bool) {
        let word = usize::try_from(index / 64).expect("位图字下标适配 usize");
        let bit = u32::try_from(index % 64).expect("位下标适配 u32");
        let mask = 1_u64 << bit;
        let slot = &mut self.free[class as usize][word];
        let was = *slot & mask != 0;
        if value {
            *slot |= mask;
        } else {
            *slot &= !mask;
        }
        if was != value {
            let count = &mut self.free_counts[class as usize];
            if value {
                *count += 1;
            } else {
                *count -= 1;
            }
        }
    }

    /// 从 `class` 起向上找第一个有空闲块的 class。
    fn find_free(&self, class: u32) -> Option<u32> {
        (class..u32::try_from(EXTENT_CLASS_LADDER.len()).expect("class 数量适配 u32"))
            .find(|candidate| self.free_counts[*candidate as usize] != 0)
    }

    /// 找到 `class` 下第一个空闲块的块编号。
    fn first_free(&self, class: u32) -> Option<u64> {
        let words = &self.free[class as usize];
        for (word_index, word) in words.iter().enumerate() {
            if *word == 0 {
                continue;
            }
            let bit = word.trailing_zeros();
            return Some(u64::try_from(word_index).expect("字下标适配 u64") * 64 + u64::from(bit));
        }
        None
    }

    /// 统计位图 popcount，供 `debug_assert!` 校验不变量。
    fn popcount(&self, class: u32) -> u32 {
        self.free[class as usize]
            .iter()
            .map(|word| word.count_ones())
            .sum()
    }
}

/// 全部 owner 的 extent 描述符表与 buddy 阶梯。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtentTable {
    descriptors: Vec<ExtentDescriptor>,
    spaces: Vec<OwnerExtentSpace>,
    /// owner 编号 → 该 owner 已登记的 arena 下标；一个 owner 可以持有多个 arena。
    owner_spaces: Vec<Vec<u32>>,
    vacant: Vec<u32>,
    /// 每个 extent 的 trim 进行中凭据；按稠密 extent 编号索引，空闲槽为 `None`。
    trims: Vec<Option<TrimTicket>>,
}

impl ExtentTable {
    /// 创建空的 extent 表。
    pub(crate) fn new() -> Self {
        Self {
            descriptors: Vec::new(),
            spaces: Vec::new(),
            owner_spaces: Vec::new(),
            vacant: Vec::new(),
            trims: Vec::new(),
        }
    }

    /// 为一个 owner 登记 extent arena；基址与容量必须按最大 class 对齐。
    pub(crate) fn register_owner(
        &mut self,
        owner: u32,
        token: OwnerToken,
        domain: MemoryDomainId,
        range: RangeId,
        base: u64,
        bytes: u64,
    ) -> Result<u32, RawInvariant> {
        let space = OwnerExtentSpace::new(token, domain, range, base, bytes)?;
        let index = u32::try_from(self.spaces.len()).expect("owner arena 数量适配 u32");
        self.spaces.push(space);
        while self.owner_spaces.len() <= owner as usize {
            self.owner_spaces.push(Vec::new());
        }
        self.owner_spaces[owner as usize].push(index);
        Ok(index)
    }

    /// 返回 owner 已登记的 arena 下标。
    pub(crate) fn spaces_of(&self, owner: u32) -> &[u32] {
        self.owner_spaces
            .get(owner as usize)
            .map_or(&[], |spaces| spaces.as_slice())
    }

    /// 返回 owner arena 对应的平台 range。
    pub(crate) fn arena_range(&self, owner_index: u32) -> Option<RangeId> {
        self.spaces
            .get(owner_index as usize)
            .map(|space| space.range)
    }

    /// 返回一个 arena 的 domain；越界编号返回 `None`。
    pub(crate) fn arena_domain(&self, owner_index: u32) -> Option<MemoryDomainId> {
        self.spaces
            .get(owner_index as usize)
            .map(|space| space.domain)
    }

    /// 返回 owner arena 的基址。
    pub(crate) fn arena_base(&self, owner_index: u32) -> Option<u64> {
        self.spaces
            .get(owner_index as usize)
            .map(|space| space.base)
    }

    /// 返回 extent 在所属 arena 内的字节偏移。
    pub(crate) fn offset_of(&self, descriptor: &ExtentDescriptor) -> u64 {
        descriptor.base - self.spaces[descriptor.owner_index as usize].base
    }

    /// 按编号返回 extent 在所属 arena 内的字节偏移。
    pub(crate) fn offset_of_id(&self, id: ExtentId) -> u64 {
        self.descriptor(id)
            .map_or(0, |descriptor| self.offset_of(descriptor))
    }

    /// 按编号返回 extent 所属 arena 的平台 range。
    pub(crate) fn arena_range_of(&self, id: ExtentId) -> Option<RangeId> {
        let descriptor = self.descriptor(id)?;
        self.spaces
            .get(descriptor.owner_index as usize)
            .map(|space| space.range)
    }

    /// 返回已登记 arena 的数量。
    pub(crate) fn owner_count(&self) -> u32 {
        u32::try_from(self.spaces.len()).expect("owner arena 数量适配 u32")
    }

    /// 返回描述符数量。
    pub(crate) fn len(&self) -> usize {
        self.descriptors.len()
    }

    /// 按稠密编号取描述符。
    pub(crate) fn descriptor(&self, id: ExtentId) -> Option<&ExtentDescriptor> {
        self.descriptors
            .get(id.index())
            .filter(|descriptor| descriptor.state != ExtentState::Vacant)
    }

    /// 按稠密编号取可变描述符；非 live 状态返回 `None`。
    pub(crate) fn descriptor_mut(&mut self, id: ExtentId) -> Option<&mut ExtentDescriptor> {
        self.descriptors
            .get_mut(id.index())
            .filter(|descriptor| descriptor.state != ExtentState::Vacant)
    }

    /// 返回某个 arena 在给定 class 的空闲块数量。
    pub(crate) fn free_blocks(&self, arena: u32, class: u32) -> u32 {
        self.spaces[arena as usize].free_counts[class as usize]
    }

    /// 判断 owner 在给定 domain 上是否有 arena 能满足 `class` 的请求。
    pub(crate) fn has_free(&self, owner: u32, domain: MemoryDomainId, class: u32) -> bool {
        self.spaces_of(owner).iter().any(|arena| {
            let space = &self.spaces[*arena as usize];
            space.domain == domain && space.find_free(class).is_some()
        })
    }

    pub(crate) fn has_free_in_arena(&self, arena: u32, class: u32) -> bool {
        self.spaces
            .get(arena as usize)
            .is_some_and(|space| space.find_free(class).is_some())
    }

    /// 返回 owner 在给定 domain 上已发放的 extent 数量。
    pub(crate) fn live_in(&self, owner: u32, domain: MemoryDomainId) -> usize {
        self.descriptors
            .iter()
            .filter(|descriptor| {
                descriptor.state != ExtentState::Vacant
                    && (descriptor.owner_index as usize) < self.spaces.len()
                    && self.spaces[descriptor.owner_index as usize].domain == domain
                    && self.spaces_of(owner).contains(&descriptor.owner_index)
            })
            .count()
    }

    /// 校验位图 popcount 与计数一致，且没有越界的空闲块。
    pub(crate) fn verify(&self) -> Result<(), RawInvariant> {
        for space in &self.spaces {
            for class in 0..EXTENT_CLASS_LADDER.len() {
                let class = u32::try_from(class).expect("class 编号适配 u32");
                let expected = space.popcount(class);
                if expected != space.free_counts[class as usize] {
                    return Err(RawInvariant::new("extent 空闲位图与块计数不一致"));
                }
                let blocks = space.blocks(class);
                for index in 0..blocks {
                    if space.test_free(class, index) {
                        let base = space.base + index * EXTENT_CLASS_LADDER[class as usize];
                        if !self.is_free_at(base, EXTENT_CLASS_LADDER[class as usize]) {
                            return Err(RawInvariant::new("extent 位图标记了仍被占用的地址区间"));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 判断地址区间是否完全没有被任何 live/queued 描述符覆盖。
    fn is_free_at(&self, base: u64, bytes: u64) -> bool {
        let end = base + bytes;
        !self.descriptors.iter().any(|descriptor| {
            descriptor.state != ExtentState::Vacant
                && descriptor.base < end
                && descriptor.end() > base
        })
    }

    /// 从 owner 的任一 arena 取得一个 `class` 的 extent，必要时向下分裂。
    ///
    /// arena 之间按登记顺序扫描；单 owner reducer 只在 owner 上下文调用，因此这里没有共享
    /// 空闲结构的并发写。
    pub(crate) fn allocate(
        &mut self,
        owner: u32,
        class: u32,
        domain: MemoryDomainId,
    ) -> Result<ExtentId, RawInvariant> {
        if class_bytes(class).is_none() {
            return Err(RawInvariant::new("extent 分配引用未知 class"));
        }
        let space_index = self
            .owner_spaces
            .get(owner as usize)
            .and_then(|spaces| {
                spaces.iter().copied().find(|index| {
                    let space = &self.spaces[*index as usize];
                    space.domain == domain && space.find_free(class).is_some()
                })
            })
            .ok_or_else(|| RawInvariant::new("owner 在目标 domain 没有满足请求的空闲 extent 块"))?;
        self.allocate_in_arena(space_index, class)
    }

    /// 从指定 arena 分裂；managed heap 必须与实际提交的地址范围保持一致。
    pub(crate) fn allocate_in_arena(
        &mut self,
        arena: u32,
        class: u32,
    ) -> Result<ExtentId, RawInvariant> {
        if class_bytes(class).is_none() {
            return Err(RawInvariant::new("extent 分配引用未知 class"));
        }
        let owner_index = arena;
        let space = self
            .spaces
            .get_mut(arena as usize)
            .ok_or_else(|| RawInvariant::new("extent 分配引用未知 arena"))?;
        let domain = space.domain;
        let source = space
            .find_free(class)
            .ok_or_else(|| RawInvariant::new("extent arena 没有满足请求的空闲块"))?;
        let index = space
            .first_free(source)
            .ok_or_else(|| RawInvariant::new("extent 空闲计数与位图不一致"))?;
        space.set_free(source, index, false);
        // 从 source 逐级分裂到 class：每级把另一半标记为空闲。
        let mut current = source;
        let mut block = index;
        while current > class {
            current -= 1;
            block *= 2;
            space.set_free(current, block + 1, true);
        }
        let bytes = EXTENT_CLASS_LADDER[class as usize];
        let base = space.base + block * bytes;
        let token = space.token;
        debug_assert_eq!(base % bytes, 0, "extent 基址必须按自身大小对齐");
        self.trims.push(None);
        let id = self.take_slot(ExtentDescriptor {
            id: ExtentId(0),
            base,
            bytes,
            alignment: bytes,
            domain,
            owner: token,
            generation: ExtentGeneration(0),
            state: ExtentState::Live,
            class,
            owner_index,
            huge_page: bytes >= 2 * 1024 * 1024,
            leases: ExtentLeases::default(),
        });
        Ok(id)
    }

    /// 取得一个描述符槽位；优先复用空闲槽并推进 generation。
    fn take_slot(&mut self, mut descriptor: ExtentDescriptor) -> ExtentId {
        match self.vacant.pop() {
            Some(index) => {
                let slot = &mut self.descriptors[index as usize];
                let generation = ExtentGeneration(slot.generation.0.wrapping_add(1));
                descriptor.generation = generation;
                descriptor.id = ExtentId(index);
                *slot = descriptor;
                ExtentId(index)
            }
            None => {
                let index = u32::try_from(self.descriptors.len()).expect("extent 数量适配 u32");
                descriptor.id = ExtentId(index);
                self.descriptors.push(descriptor);
                ExtentId(index)
            }
        }
    }

    /// 取得一条 lease；新的 lease 使已完成的 grace 失效。
    pub(crate) fn acquire_lease(
        &mut self,
        id: ExtentId,
        lease: ExtentLease,
    ) -> Result<(), RawInvariant> {
        self.live_descriptor_mut(id)?.leases.acquire(lease)?;
        self.cancel_trim(id);
        Ok(())
    }

    /// 结束一条 lease。
    pub(crate) fn release_lease(
        &mut self,
        id: ExtentId,
        lease: ExtentLease,
    ) -> Result<(), RawInvariant> {
        self.live_descriptor_mut(id)?.leases.release(lease)
    }

    fn live_descriptor_mut(&mut self, id: ExtentId) -> Result<&mut ExtentDescriptor, RawInvariant> {
        match self.descriptors.get_mut(id.index()) {
            Some(descriptor) if descriptor.state == ExtentState::Live => Ok(descriptor),
            Some(_) => Err(RawInvariant::new("extent 操作引用了非 live 状态")),
            None => Err(RawInvariant::new("extent 操作引用未知描述符")),
        }
    }

    /// 记录该 extent 上的 return 消息已发布。
    pub(crate) fn mark_return_queued(&mut self, id: ExtentId) -> Result<(), RawInvariant> {
        let descriptor = self.live_descriptor_mut(id)?;
        descriptor.state = ExtentState::ReturnQueued;
        Ok(())
    }

    /// 推进一次 trim：检查 lease、slot、在途消息门禁，再按 epoch 走 grace。
    ///
    /// 返回 `Ok(())` 表示四条门禁全部满足，调用者可以撤销物理页；`Err(TrimBlocked)` 给出被
    /// 拒绝的具体原因与数量，诊断必须原样呈现。进行中的 ticket 按 extent 保存，使 grace 可以
    /// 跨多次调用累积步数。
    pub(crate) fn poll_trim(
        &mut self,
        id: ExtentId,
        epoch: Epoch,
        occupancy: ExtentOccupancy,
    ) -> Result<(), TrimBlocked> {
        let descriptor = self
            .descriptor(id)
            .ok_or(TrimBlocked::LiveSlots(occupancy.live_slots))?;
        if let Some(lease) = descriptor.leases.outstanding() {
            self.cancel_trim(id);
            return Err(TrimBlocked::Lease(lease));
        }
        if occupancy.live_slots != 0 {
            self.cancel_trim(id);
            return Err(TrimBlocked::LiveSlots(occupancy.live_slots));
        }
        if occupancy.queued_slots != 0 {
            self.cancel_trim(id);
            return Err(TrimBlocked::QueuedSlots(occupancy.queued_slots));
        }
        if occupancy.pending_returns != 0 {
            self.cancel_trim(id);
            return Err(TrimBlocked::PendingReturns(occupancy.pending_returns));
        }
        let slot = &mut self.trims[id.index()];
        let ticket = slot.get_or_insert(TrimTicket {
            extent: id,
            last_epoch: epoch,
            completed: 0,
        });
        if epoch.raw() != ticket.last_epoch.raw() {
            ticket.last_epoch = epoch;
            ticket.completed += 1;
        }
        if ticket.completed < GRACE_STEPS {
            return Err(TrimBlocked::GracePending {
                completed: ticket.completed,
                required: GRACE_STEPS,
            });
        }
        Ok(())
    }

    /// 放弃一个 extent 的 trim 进度；新的分配或新的 lease 都会重置 grace。
    pub(crate) fn cancel_trim(&mut self, id: ExtentId) {
        if let Some(slot) = self.trims.get_mut(id.index()) {
            *slot = None;
        }
    }

    /// 返回 extent 已完成的 grace 步数。
    pub(crate) fn trim_progress(&self, id: ExtentId) -> u32 {
        self.trims
            .get(id.index())
            .and_then(|slot| *slot)
            .map_or(0, |ticket| ticket.completed)
    }

    /// 归还一个 extent：合并相邻空闲块，并释放描述符槽位。
    pub(crate) fn give_back(&mut self, id: ExtentId) -> Result<(), RawInvariant> {
        let descriptor = match self.descriptors.get(id.index()) {
            Some(descriptor) if descriptor.state != ExtentState::Vacant => *descriptor,
            Some(_) => return Err(RawInvariant::new("extent 已被归还")),
            None => return Err(RawInvariant::new("归还引用未知 extent")),
        };
        if let Some(lease) = descriptor.leases.outstanding() {
            return Err(RawInvariant::new(format!(
                "extent 归还时仍有 {} lease 未归零",
                lease.name()
            )));
        }
        let space = self
            .spaces
            .get_mut(descriptor.owner_index as usize)
            .ok_or_else(|| RawInvariant::new("归还引用未登记的 owner arena"))?;
        let bytes = descriptor.bytes;
        let mut class = descriptor.class;
        let mut block = (descriptor.base - space.base) / bytes;
        // 相邻合并：buddy 编号是块编号的低位翻转；只有 buddy 空闲才能升到上一级。
        while class + 1 < EXTENT_CLASS_LADDER.len() as u32 {
            let buddy = block ^ 1;
            if !space.test_free(class, buddy) {
                break;
            }
            space.set_free(class, buddy, false);
            class += 1;
            block /= 2;
        }
        space.set_free(class, block, true);
        debug_assert_eq!(space.popcount(class), space.free_counts[class as usize]);
        let slot = &mut self.descriptors[id.index()];
        slot.state = ExtentState::Vacant;
        slot.leases = ExtentLeases::default();
        self.cancel_trim(id);
        self.vacant.push(id.raw());
        Ok(())
    }

    /// 返回全部描述符的规范顺序；用于确定性 dump 与统计。
    pub(crate) fn descriptors(&self) -> &[ExtentDescriptor] {
        &self.descriptors
    }

    /// 返回 live 状态的 extent 数量。
    pub(crate) fn live_count(&self) -> usize {
        self.descriptors
            .iter()
            .filter(|descriptor| descriptor.state == ExtentState::Live)
            .count()
    }

    /// 返回已借出的总字节数。
    pub(crate) fn live_bytes(&self) -> u64 {
        self.descriptors
            .iter()
            .filter(|descriptor| descriptor.state != ExtentState::Vacant)
            .map(|descriptor| descriptor.bytes)
            .sum()
    }
}
