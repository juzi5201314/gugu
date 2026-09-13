//! raw owner 的本地分配与回收路径。
//!
//! 本地分配顺序固定为：class 的 owner-local free list → 当前 span 的 bump cursor →
//! owner domain 的 local range cache → typed range request 冷路径。前两步不执行原子 RMW、
//! 不进入全局锁、不调用平台；第 3 步只访问 owner-local metadata；第 4 步才允许进入按
//! safepoint 分类的慢路径。记录完成生命周期后，只有当前执行者仍持有 slab 时才直接进入
//! 本地 free list，否则先完成 exactly-once 的 `ReturnQueued` 状态迁移再发布 return message。

use super::extent::{ExtentId, ExtentTable, class_for_bytes};
use super::message::LinkCodec;
use super::provider::RangeProvider;
use super::size_class::{RuntimeSizeClass, RuntimeSizeClassId, RuntimeSizeClassTable};
use super::slab::{
    Epoch, OwnerAccounting, OwnerToken, RawInvariant, RawSlot, SlabDescriptorId, SlabTable,
    SlotState,
};

/// 本地分配命中的层级。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AllocationLevel {
    /// 从 class 的 owner-local free list 取得。
    FreeList,
    /// 从当前 span 的 bump cursor 取得。
    SpanBump,
    /// 从 owner domain 的 local range cache 取得新 span。
    DomainCache,
    /// 发布 typed range request，由 platform range 冷路径补充。
    RangeRequest,
}

impl AllocationLevel {
    /// 返回层级名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::FreeList => "free-list",
            Self::SpanBump => "span-bump",
            Self::DomainCache => "domain-cache",
            Self::RangeRequest => "range-request",
        }
    }
}

/// 一次本地分配的 slot 与命中层级。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Allocation {
    pub(crate) slot: RawSlot,
    pub(crate) level: AllocationLevel,
}

/// 一个已 decommit、可按 class 复用的 extent。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CachedExtent {
    pub(crate) class: u32,
    pub(crate) extent: ExtentId,
}

/// owner domain 的 local extent cache：保存已 decommit、可直接重新提交的 extent。
///
/// cache 里的 extent 仍占用虚拟地址（计入 `range_reserved_bytes`），但没有物理页；重新取用
/// 时只提交页，不重新走 arena 的 buddy 阶梯。小对象每次释放都不触发 decommit，只有 span 长期
/// 空闲、memory pressure 或 owner/domain trim 才把 extent 送进这里。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DomainRangeCache {
    entries: Vec<CachedExtent>,
    committed_bytes: u64,
    refills: u32,
}

impl DomainRangeCache {
    /// 返回缓存的 extent 数量。
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// 返回累计从平台补充的字节数。
    pub(crate) const fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    /// 返回冷路径 refill 次数。
    pub(crate) const fn refills(&self) -> u32 {
        self.refills
    }

    /// 返回缓存的 extent 快照；用于统计与校验。
    pub(crate) fn entries(&self) -> &[CachedExtent] {
        &self.entries
    }

    /// 取一个匹配 class 的已 decommit extent。
    pub(crate) fn take(&mut self, class: u32) -> Option<ExtentId> {
        let index = self.entries.iter().position(|entry| entry.class == class)?;
        Some(self.entries.swap_remove(index).extent)
    }

    /// 把一个已 decommit 的 extent 放进 cache。
    pub(crate) fn park(&mut self, class: u32, extent: ExtentId) {
        self.entries.push(CachedExtent { class, extent });
    }

    /// 从 owner arena 取得一个 extent 并提交它的页；`reserve_aligned` 只在建 arena 时发生。
    pub(crate) fn refill(
        &mut self,
        extents: &mut ExtentTable,
        provider: &mut dyn RangeProvider,
        owner: u32,
        extent_class: u32,
        domain: super::slab::MemoryDomainId,
    ) -> Result<ExtentId, RawInvariant> {
        let extent = extents.allocate(owner, extent_class, domain)?;
        let range = extents
            .arena_range_of(extent)
            .ok_or_else(|| RawInvariant::new("extent 缺少所属 arena"))?;
        let offset = extents.offset_of_id(extent);
        let bytes = extents
            .descriptor(extent)
            .ok_or_else(|| RawInvariant::new("extent 描述缺失"))?
            .bytes;
        provider.commit_pages(range, offset, bytes)?;
        self.committed_bytes += bytes;
        self.refills += 1;
        Ok(extent)
    }

    /// 重新提交一个已缓存的 extent。
    pub(crate) fn recommit(
        &mut self,
        extents: &ExtentTable,
        provider: &mut dyn RangeProvider,
        extent: ExtentId,
    ) -> Result<u64, RawInvariant> {
        let range = extents
            .arena_range_of(extent)
            .ok_or_else(|| RawInvariant::new("缓存 extent 缺少所属 arena"))?;
        let offset = extents.offset_of_id(extent);
        let bytes = extents
            .descriptor(extent)
            .ok_or_else(|| RawInvariant::new("缓存 extent 描述缺失"))?
            .bytes;
        provider.commit_pages(range, offset, bytes)?;
        self.committed_bytes += bytes;
        Ok(bytes)
    }
}

/// 返回承载一个 raw span 的 extent class 编号。
///
/// raw slab page 是 64 KiB，落在二次幂 extent 阶梯的中间；span 一定取得整个 extent，使同一
/// extent 内的 slot 地址连续且对齐由 extent 自身保证。
pub(crate) fn span_extent_class() -> u32 {
    class_for_bytes(super::RAW_SLAB_PAGE_BYTES).expect("raw slab page 落在 extent 阶梯内")
}

/// 当前正在 bump 的 span 游标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SpanCursor {
    pub(crate) descriptor: SlabDescriptorId,
    pub(crate) next: u32,
}

/// 一个 owner 在一个 class 上的本地 cache 计数与 span 游标。
///
/// free structure 本身由 `SlabTable` 承载：`link_usable` 为真时 encoded link 写在 slot
/// 头部字里，否则退回 descriptor 的显式 free 下标列表。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OwnerClassCache {
    span: Option<SpanCursor>,
    reused: u32,
    bumped: u32,
}

impl OwnerClassCache {
    const fn new() -> Self {
        Self {
            span: None,
            reused: 0,
            bumped: 0,
        }
    }

    /// 返回从 free list 复用的 slot 累计数。
    pub(crate) const fn reused(&self) -> u32 {
        self.reused
    }

    /// 返回从 bump cursor 取得的 slot 累计数。
    pub(crate) const fn bumped(&self) -> u32 {
        self.bumped
    }

    /// 返回当前 span 游标。
    pub(crate) const fn span(&self) -> Option<SpanCursor> {
        self.span
    }
}

/// 一个 raw owner 的本地 cache 与 range cache。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RawOwner {
    token: OwnerToken,
    classes: Vec<OwnerClassCache>,
    range_cache: DomainRangeCache,
}

impl RawOwner {
    /// 为一个 class 表创建 owner 本地 cache。
    pub(crate) fn new(token: OwnerToken, classes: &RuntimeSizeClassTable) -> Self {
        Self {
            token,
            classes: vec![OwnerClassCache::new(); classes.classes().len()],
            range_cache: DomainRangeCache::default(),
        }
    }

    /// 返回 owner token。
    pub(crate) const fn token(&self) -> OwnerToken {
        self.token
    }

    /// 返回 domain range cache。
    pub(crate) const fn range_cache(&self) -> &DomainRangeCache {
        &self.range_cache
    }

    /// 返回某个 class 的本地 cache。
    pub(crate) fn class_cache(&self, id: RuntimeSizeClassId) -> &OwnerClassCache {
        &self.classes[id.index()]
    }

    /// 判断本 owner 是否是给定描述符的回收 owner。
    pub(crate) fn owns(&self, descriptor: &super::slab::SlabDescriptor) -> bool {
        self.token == descriptor.owner
    }

    /// 本地分配：free list → span bump → domain cache → typed range request。
    #[expect(
        clippy::too_many_arguments,
        reason = "本地分配需要同时推进 free list、span、domain cache 与账本"
    )]
    pub(crate) fn allocate(
        &mut self,
        owner: u32,
        class: &RuntimeSizeClass,
        extent_class: u32,
        table: &mut SlabTable,
        extents: &mut ExtentTable,
        provider: &mut dyn RangeProvider,
        codec: &LinkCodec,
        accounting: &mut OwnerAccounting,
        integrity_secret: u32,
        slab_epoch: Epoch,
    ) -> Result<Allocation, RawInvariant> {
        let stride = u64::from(class.slot_stride);
        if let Some(cursor) = self.classes[class.id.index()].span {
            if let Some(index) = table.pop_free(cursor.descriptor, codec)? {
                table.transition(
                    cursor.descriptor,
                    index,
                    SlotState::Returned,
                    SlotState::Live,
                )?;
                let record = table
                    .descriptor_mut(cursor.descriptor)
                    .ok_or_else(|| RawInvariant::new("free list 引用未知 slab"))?;
                record.live += 1;
                let generation = record.generation;
                let cache = &mut self.classes[class.id.index()];
                cache.reused += 1;
                accounting.take_from_cache(stride);
                return Ok(Allocation {
                    slot: RawSlot {
                        descriptor: cursor.descriptor,
                        index,
                        generation,
                    },
                    level: AllocationLevel::FreeList,
                });
            }
            let slots = table
                .descriptor(cursor.descriptor)
                .ok_or_else(|| RawInvariant::new("span 游标引用未知 slab"))?
                .slot_count();
            if cursor.next < slots {
                let index = cursor.next;
                table.transition(
                    cursor.descriptor,
                    index,
                    SlotState::Returned,
                    SlotState::Live,
                )?;
                let record = table
                    .descriptor_mut(cursor.descriptor)
                    .ok_or_else(|| RawInvariant::new("span 游标引用未知 slab"))?;
                record.live += 1;
                record.free -= 1;
                record.bump_cursor = index + 1;
                let generation = record.generation;
                let cache = &mut self.classes[class.id.index()];
                cache.span = Some(SpanCursor {
                    descriptor: cursor.descriptor,
                    next: index + 1,
                });
                cache.bumped += 1;
                accounting.take_from_cache(stride);
                return Ok(Allocation {
                    slot: RawSlot {
                        descriptor: cursor.descriptor,
                        index,
                        generation,
                    },
                    level: AllocationLevel::SpanBump,
                });
            }
        }
        let (extent, level) = match self.range_cache.take(extent_class) {
            Some(extent) => {
                let bytes = self.range_cache.recommit(extents, provider, extent)?;
                let _ = bytes;
                (extent, AllocationLevel::DomainCache)
            }
            None => {
                let extent = self.range_cache.refill(
                    extents,
                    provider,
                    owner,
                    extent_class,
                    class.domain,
                )?;
                (extent, AllocationLevel::RangeRequest)
            }
        };
        let extent_bytes = extents
            .descriptor(extent)
            .ok_or_else(|| RawInvariant::new("extent 描述缺失"))?
            .bytes;
        let descriptor = table.create(
            class,
            self.token,
            extent,
            extent_bytes,
            integrity_secret,
            slab_epoch,
        )?;
        accounting.commit(extent_bytes);
        table.transition(descriptor, 0, SlotState::Returned, SlotState::Live)?;
        let record = table
            .descriptor_mut(descriptor)
            .ok_or_else(|| RawInvariant::new("新建 slab 立即缺失"))?;
        record.live += 1;
        record.free -= 1;
        record.bump_cursor = 1;
        let generation = record.generation;
        self.classes[class.id.index()].span = Some(SpanCursor {
            descriptor,
            next: 1,
        });
        accounting.take_from_cache(stride);
        Ok(Allocation {
            slot: RawSlot {
                descriptor,
                index: 0,
                generation,
            },
            level,
        })
    }

    /// 记录完成生命周期：`Live` → `Dead`。
    pub(crate) fn begin_return(
        &mut self,
        slot: RawSlot,
        table: &mut SlabTable,
    ) -> Result<(), RawInvariant> {
        table.transition(
            slot.descriptor,
            slot.index,
            SlotState::Live,
            SlotState::Dead,
        )
    }

    /// 赢得 return 线性化点：`Dead` → `ReturnQueued`；重复调用报不变量失败。
    pub(crate) fn queue_return(
        &mut self,
        slot: RawSlot,
        table: &mut SlabTable,
        accounting: &mut OwnerAccounting,
        bytes: u64,
    ) -> Result<(), RawInvariant> {
        table.transition(
            slot.descriptor,
            slot.index,
            SlotState::Dead,
            SlotState::ReturnQueued,
        )?;
        let record = table
            .descriptor_mut(slot.descriptor)
            .ok_or_else(|| RawInvariant::new("return 引用未知 slab"))?;
        if slot.generation != record.generation {
            return Err(RawInvariant::new("return 引用了过期 generation 的 slot"));
        }
        record.live -= 1;
        record.queued += 1;
        record.pending_returns += 1;
        accounting.stage_pending(bytes);
        Ok(())
    }

    /// 回滚尚未发布的 return：`ReturnQueued` → `Live`。
    pub(crate) fn cancel_return(
        &mut self,
        slot: RawSlot,
        table: &mut SlabTable,
        accounting: &mut OwnerAccounting,
        bytes: u64,
    ) -> Result<(), RawInvariant> {
        table.transition(
            slot.descriptor,
            slot.index,
            SlotState::ReturnQueued,
            SlotState::Live,
        )?;
        let record = table
            .descriptor_mut(slot.descriptor)
            .ok_or_else(|| RawInvariant::new("cancel return 引用未知 slab"))?;
        record.live += 1;
        record.queued -= 1;
        record.pending_returns = record.pending_returns.saturating_sub(1);
        accounting.cancel_pending(bytes);
        Ok(())
    }

    /// owner 消费消息后把 slot 放回 free structure，并推进账本分类。
    pub(crate) fn consume_return(
        &mut self,
        slot: RawSlot,
        table: &mut SlabTable,
        codec: &LinkCodec,
        accounting: &mut OwnerAccounting,
        bytes: u64,
    ) -> Result<(), RawInvariant> {
        table.transition(
            slot.descriptor,
            slot.index,
            SlotState::ReturnQueued,
            SlotState::Returned,
        )?;
        table.push_free(slot.descriptor, slot.index, codec)?;
        let record = table
            .descriptor_mut(slot.descriptor)
            .ok_or_else(|| RawInvariant::new("consume 引用未知 slab"))?;
        record.queued -= 1;
        record.pending_returns = record.pending_returns.saturating_sub(1);
        accounting.consume_pending(bytes);
        accounting.park_reclaimable(bytes);
        Ok(())
    }
}
