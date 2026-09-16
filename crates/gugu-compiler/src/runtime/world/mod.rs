//! raw plane 世界：owner 集合、producer 发布路径、owner drain、grace 与 retire。
//!
//! producer 只接触自己的 staging、node pool 与目标 inbox；owner 本地的 descriptor、
//! free structure 与账本只由 owner 上下文读写。跨 owner 的归还先经过 exactly-once 的
//! `ReturnQueued` 状态迁移，再发布只携带逻辑序号的 return message。

pub(crate) mod barrier_impl;
pub(crate) mod coroutine_impl;
mod extent_impl;
pub(crate) mod heap_impl;
pub(crate) mod mark_impl;
pub(crate) mod pacing_impl;
mod region_impl;
mod resource_impl;
pub(crate) mod sync_impl;
pub(crate) mod termination_impl;
mod wait_impl;

#[cfg(test)]
#[path = "../coroutine_tests.rs"]
mod coroutine_tests;

#[cfg(test)]
#[path = "../wait_tests.rs"]
mod wait_tests;

#[cfg(test)]
#[path = "../sync_tests.rs"]
mod sync_tests;

#[cfg(test)]
#[path = "../barrier_tests.rs"]
mod barrier_tests;

#[cfg(test)]
#[path = "heap_tests.rs"]
mod heap_tests;

#[cfg(test)]
#[path = "../pacing_tests.rs"]
mod pacing_tests;

#[cfg(test)]
#[path = "../mark_tests.rs"]
mod mark_tests;

#[cfg(test)]
#[path = "mark_tests.rs"]
mod mark_world_tests;
#[cfg(test)]
#[path = "region_tests.rs"]
mod region_tests;

#[cfg(test)]
pub(crate) use extent_impl::OWNER_ARENA_BYTES;

use std::collections::VecDeque;
use std::sync::Arc;

use super::barrier::BarrierFlushReason;
use super::barrier_schema::MessageFamilyTag;
use super::extent::{ExtentId, ExtentTable, TrimReport};
use super::inbox::{
    DrainReport, GraceOutcome, OwnerConsumer, OwnerInbox, ServiceBudget, ShardIndex,
};
use super::message::{
    BatchLimits, FlushTrigger, IntegrityTag, LinkCodec, MessageState, ProducerStaging,
    PublishOutcome, ReturnKind, ReturnMessage, ReturnNodeId, ReturnNodePool, ReturnSlabCache,
    RingCloseReason, StagedChain, flush_staging, stage_message,
};
use super::owner::{Allocation, RawOwner};
use super::platform::FakePlatform;
use super::provider::{ProviderStats, RangeDescriptor, RangeProvider};
use super::resource::{self, ReleaseRegistry, ReleaseTicket, ResourceCellTable};
use super::size_class::{RuntimeSizeClassId, RuntimeSizeClassTable};
use super::slab::{
    Epoch, MemoryDomainId, OwnerAccounting, OwnerDirectory, OwnerId, OwnerToken, RawInvariant,
    RawSlot, Resolution, RuntimeSeed, SlabDescriptorId, SlabGeneration, SlabState, SlabTable,
    SlotState,
};

/// owner retire 的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetireReport {
    /// 排空时转发的消息数。
    pub(crate) forwarded: u32,
    /// 排空时本地消费的消息数。
    pub(crate) consumed: u32,
    /// grace 的收敛结果。
    pub(crate) grace: GraceOutcome,
    /// 回收的 span 数量。
    pub(crate) reclaimed_spans: u32,
    /// 因 lease、slot、在途 return 或 grace 门禁未通过而保留 committed 的 span 数量。
    pub(crate) blocked_spans: u32,
}

/// 资源分配的 payload 形状；kind 与对齐只作登记，不进入用户可见类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResourceShape {
    pub(crate) kind_id: u8,
    pub(crate) payload_bytes: u32,
    pub(crate) alignment: u32,
}

/// raw plane 的可运行模型世界。
#[derive(Debug)]
pub(crate) struct RawWorld {
    classes: RuntimeSizeClassTable,
    resource_classes: RuntimeSizeClassTable,
    provider: FakePlatform,
    /// 全部 owner 的 extent 阶梯；平台 range 只经它暴露给 slab 层。
    extents: ExtentTable,
    /// raw 与 resource 共享的 slab 描述符表；descriptor id 全局唯一。
    table: SlabTable,
    directory: OwnerDirectory,
    owners: Vec<RawOwner>,
    resource_owners: Vec<RawOwner>,
    inboxes: Vec<Arc<OwnerInbox>>,
    consumers: Vec<OwnerConsumer>,
    pool: Arc<ReturnNodePool>,
    link_codec: LinkCodec,
    integrity_secret: [u8; 32],
    limits: BatchLimits,
    epoch: Epoch,
    domain_owner: OwnerToken,
    /// 已完成消费但尚未过 queue-page grace 的 node；在 grace 之后才允许复用。
    graced_nodes: Vec<ReturnNodeId>,
    /// 已过归还线性化点、正在等 queue-page grace 的 extent 及其账本 owner。
    ///
    /// grace 按 epoch 累积，跨多次 owner service 推进；记录在这里使等待中的 extent 不需要重新
    /// 发布消息，也不会因为一次未走完就丢失归还。owner 与 extent 一起记录：归还完成后描述符
    /// 槽位会被复用，届时就无法再从 extent 反查账本归属。
    pending_extent_trims: Vec<(ExtentId, OwnerId)>,
    /// 与 slab descriptor 平行的 ResourceCell header。
    cells: ResourceCellTable,
    /// 统一 release 入口的 glue 与描述符目录。
    registry: ReleaseRegistry,
    /// 首个关闭者或最后 lease 入队的 release 请求。
    release_queue: VecDeque<ReleaseTicket>,
    /// rt0 进程模型：生命周期、启动配置、报告与终止计划；`boot` 之前为 `None`。
    rt0: Option<termination_impl::Rt0Process>,
    controls: super::coroutine::CoroutineTable,
    stacks: super::stack_arena::StackAllocator,
    coroutine_storage: Vec<Option<coroutine_impl::CoroutineStorage>>,
    completion_barriers: Vec<(super::coroutine::CoroutineHandle, u64, u64)>,
    scheduler: super::scheduler::SchedulerWorld,
    wait: super::wait::WaitPlane,
    channels: super::channel::ChannelTable,
    sync: super::sync::SyncPlane,
    /// hybrid write barrier 的 processor 账本、arena card table 与 edge summary。
    barrier: super::barrier::BarrierPlane,
    /// 已经由 owner 取走的跨 block edge delta 总数。
    edge_delta_total: u64,
    /// 每个 owner 的 return producer staging；未发布的链必须在 drain/retire 前交出。
    return_stagings: Vec<ProducerStaging>,
    /// 每个 owner 的 source-slab return cache；关闭时把 ring 转成 return batch。
    return_caches: Vec<ReturnSlabCache>,
    /// GC debt、owner credit、pacing 与 pressure episode 的执行平面。
    pacing: super::pacing::PacingPlane,
    /// TurnRegion 私有区与 `RegionTransfer` 投递平面；按已构建的契约配置。
    regions: Option<super::region::RegionPlane>,
    /// LocalHeap 契约快照；`configure_gc` 之后才可用。
    local_heap_contract: Option<super::local_heap_schema::LocalHeapRuntimeContract>,
    /// MarkMailbox、owner credit 与终止检测的执行平面；`configure_gc` 之后才可用。
    mark: Option<super::mark::MarkPlane>,
    /// 每个 owner 的 mark worklist；元素是待标记对象的 payload 地址。
    mark_worklists: Vec<Vec<u64>>,
    /// 已收敛的 mark cycle epoch；未收敛的 cycle 不推进它。
    mark_cycle_epoch: u64,
    /// 每个 owner 的 LocalHeap Immix arena 集合。
    local_heaps: Option<Vec<super::local_heap::LocalHeap>>,
    /// 从镜像 section 解码出的运行时可读 GC 类型表。
    gc_types: Option<super::gc_metadata_section::GcRuntimeMetadata>,
    /// 模型根槽数组；每个槽是 managed pointer 或 0。
    managed_roots: Vec<u64>,
    /// 根槽的 `(root kind, type index)` 登记。
    managed_root_kinds: Vec<(u32, u32)>,
    /// LocalHeap cycle epoch；每次 minor/major 前进一。
    heap_cycle_epoch: u64,
    /// LocalHeap block 对应的 extent class 编号。
    heap_block_class: u32,
}

impl RawWorld {
    /// 创建世界：一个 domain owner 加 `owners` 个 raw owner，node pool 容量固定。
    pub(crate) fn new(
        seed: u64,
        owners: u32,
        node_capacity: u32,
        limits: BatchLimits,
    ) -> Result<Self, RawInvariant> {
        let mut seed = RuntimeSeed::new(seed);
        let mut directory = OwnerDirectory::new(Epoch::from_raw(0));
        let domain_owner =
            directory.register(&mut seed, MemoryDomainId::RUNTIME_RAW, Epoch::from_raw(0));
        let classes = RuntimeSizeClassTable::ladder(MemoryDomainId::RUNTIME_RAW)?;
        let resource_classes = RuntimeSizeClassTable::resource_ladder()?;
        resource::verify_header_layout()?;
        let registry = ReleaseRegistry::builtin()?;
        let integrity_secret = seed.secret();
        let link_codec = LinkCodec::new(seed.secret());
        let mut owner_list = Vec::with_capacity(owners as usize);
        let mut resource_owners = Vec::with_capacity(owners as usize);
        let mut inboxes = Vec::with_capacity(owners as usize);
        let mut consumers = Vec::with_capacity(owners as usize);
        for _ in 0..owners {
            let token =
                directory.register(&mut seed, MemoryDomainId::RUNTIME_RAW, Epoch::from_raw(0));
            owner_list.push(RawOwner::new(token, &classes));
            let resource_token =
                directory.register(&mut seed, MemoryDomainId::RESOURCE, Epoch::from_raw(0));
            resource_owners.push(RawOwner::new(resource_token, &resource_classes));
            inboxes.push(Arc::new(OwnerInbox::new(super::OWNER_INBOX_SHARDS)));
            consumers.push(OwnerConsumer::new(super::OWNER_INBOX_SHARDS));
        }
        let scheduler =
            super::scheduler::SchedulerWorld::new(u64::from(owners.max(1)), seed.next())?;
        let wait = super::wait::WaitPlane::new()?;
        let mut world = Self {
            classes,
            resource_classes,
            provider: FakePlatform::new(super::PlatformProfile::Linux, u64::from(u32::MAX)),
            extents: ExtentTable::new(),
            table: SlabTable::new(),
            directory,
            owners: owner_list,
            resource_owners,
            inboxes,
            consumers,
            pool: Arc::new(ReturnNodePool::new(node_capacity)),
            link_codec,
            integrity_secret,
            limits,
            epoch: Epoch::from_raw(0),
            domain_owner,
            graced_nodes: Vec::new(),
            pending_extent_trims: Vec::new(),
            cells: ResourceCellTable::new(),
            registry,
            release_queue: VecDeque::new(),
            rt0: None,
            controls: super::coroutine::CoroutineTable::default(),
            stacks: super::stack_arena::StackAllocator::default(),
            coroutine_storage: Vec::new(),
            completion_barriers: Vec::new(),
            scheduler,
            wait,
            channels: super::channel::ChannelTable::new(),
            sync: super::sync::SyncPlane::new(),
            barrier: super::barrier::BarrierPlane::new(0),
            edge_delta_total: 0,
            pacing: super::pacing::PacingPlane::default(),
            return_stagings: (0..owners).map(|_| ProducerStaging::new(limits)).collect(),
            return_caches: (0..owners).map(|_| ReturnSlabCache::new()).collect(),
            regions: None,
            local_heap_contract: None,
            mark: None,
            mark_worklists: Vec::new(),
            mark_cycle_epoch: 0,
            local_heaps: None,
            gc_types: None,
            managed_roots: Vec::new(),
            managed_root_kinds: Vec::new(),
            heap_cycle_epoch: 0,
            heap_block_class: 0,
        };
        // 每个 owner 在 raw 与 Resource 两个 domain 上各持有自己的 arena；arena 只预留虚拟
        // 地址，物理页在 extent 被发放时按页提交。
        for owner in 0..owners {
            let token = world.owners[owner as usize].token();
            world.open_arena(owner, token, MemoryDomainId::RUNTIME_RAW)?;
            let resource_token = world.resource_owners[owner as usize].token();
            world.open_arena(owner, resource_token, MemoryDomainId::RESOURCE)?;
        }
        Ok(world)
    }

    /// 返回 owner 数量。
    pub(crate) fn owner_count(&self) -> u32 {
        self.owners.len() as u32
    }

    /// 返回 owner 的 token。
    pub(crate) fn token(&self, index: u32) -> OwnerToken {
        self.owners[index as usize].token()
    }

    /// 返回 Resource domain owner 的 token。
    pub(crate) fn resource_token(&self, index: u32) -> OwnerToken {
        self.resource_owners[index as usize].token()
    }

    /// 返回 owner 的 inbox；producer 通过它发布，不需要接触 owner 本地状态。
    pub(crate) fn inbox(&self, index: u32) -> Arc<OwnerInbox> {
        Arc::clone(&self.inboxes[index as usize])
    }

    /// 返回 domain owner 的 token；owner 不可达或 retire 时作为 injection 终点。
    pub(crate) const fn domain_owner(&self) -> OwnerToken {
        self.domain_owner
    }

    /// 返回 node pool。
    pub(crate) fn pool(&self) -> Arc<ReturnNodePool> {
        Arc::clone(&self.pool)
    }

    /// 返回 class 表。
    pub(crate) fn classes(&self) -> &RuntimeSizeClassTable {
        &self.classes
    }

    /// 返回 batch 上限。
    pub(crate) const fn limits(&self) -> BatchLimits {
        self.limits
    }

    /// 返回 integrity secret 派生的只读副本。
    pub(crate) const fn integrity_secret(&self) -> &[u8; 32] {
        &self.integrity_secret
    }

    /// 返回 owner directory。
    pub(crate) const fn directory(&self) -> &OwnerDirectory {
        &self.directory
    }

    /// 返回 owner 账本。
    pub(crate) fn accounting(&self, index: u32) -> &OwnerAccounting {
        self.directory
            .accounting(self.owners[index as usize].token().owner_id)
            .expect("owner 必须已登记")
    }

    /// 返回 slab 描述符表。
    pub(crate) const fn table(&self) -> &SlabTable {
        &self.table
    }

    /// 返回某个描述符的 class 与 generation，供 consumer 读取 node payload。
    pub(crate) fn descriptor(
        &self,
        id: SlabDescriptorId,
    ) -> Result<&super::slab::SlabDescriptor, RawInvariant> {
        self.table
            .descriptor(id)
            .ok_or_else(|| RawInvariant::new("引用未知 slab 描述符"))
    }

    /// 返回 provider 统计：reserved/committed 字节与拒绝次数。
    pub(crate) fn provider_stats(&self) -> ProviderStats {
        self.provider.stats()
    }

    /// 返回 provider 登记的 range 列表。
    pub(crate) fn provider_ranges(&self) -> &[RangeDescriptor] {
        self.provider.describe_all()
    }

    /// 返回当前 epoch。
    pub(crate) const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// 本地分配：owner 上下文的唯一分配入口。
    pub(crate) fn allocate(
        &mut self,
        owner: u32,
        class: RuntimeSizeClassId,
    ) -> Result<Allocation, RawInvariant> {
        // allocation slow edge：本地 fast bump 不读全局状态，慢路径探测由 `slow_edge_due`
        // 决定；所有全局读取、assist、drain 与 headroom 判定都收敛在 `pacing_slow_edge` 内。
        // 先解析 class 再把本次请求的 slot 字节交给 headroom：软上限必须看到真实请求大小，
        // 否则越界分配会被放行到下一次分配才被发现。
        let class = *self
            .classes
            .get(class)
            .ok_or_else(|| RawInvariant::new("分配引用未知 class"))?;
        self.pacing_slow_edge(owner, u64::from(class.slot_stride))?;
        let token = self.owners[owner as usize].token();
        let secret_index = u32::from(class.id.raw());
        let accounting = self
            .directory
            .accounting_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("分配缺少 owner 账本"))?;
        let codec = self.link_codec.clone();
        let provider = &mut self.provider;
        let table = &mut self.table;
        let extents = &mut self.extents;
        let slab_epoch = self.epoch;
        let extent_class = super::owner::span_extent_class();
        let allocation = self.owners[owner as usize].allocate(
            owner,
            &class,
            extent_class,
            table,
            extents,
            provider,
            &codec,
            accounting,
            secret_index,
            slab_epoch,
        )?;
        // allocation debt 在分配成功之后推进：本地 fast bump 不读全局 debt，这里只记账。
        self.pacing.observe_allocation(u64::from(class.slot_stride));
        Ok(allocation)
    }

    /// 记录完成生命周期并赢得 return 线性化点。
    pub(crate) fn queue_return(
        &mut self,
        owner: u32,
        slot: RawSlot,
        bytes: u64,
    ) -> Result<(), RawInvariant> {
        let token = self.owners[owner as usize].token();
        let accounting = self
            .directory
            .accounting_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("return 缺少 owner 账本"))?;
        self.owners[owner as usize].begin_return(slot, &mut self.table)?;
        self.owners[owner as usize].queue_return(slot, &mut self.table, accounting, bytes)
    }

    /// 本地 return：当前执行者仍是 slab owner，直接把 slot 放回本地 free structure。
    pub(crate) fn local_return(
        &mut self,
        owner: u32,
        slot: RawSlot,
        bytes: u64,
    ) -> Result<(), RawInvariant> {
        let token = self.owners[owner as usize].token();
        let codec = self.link_codec.clone();
        let accounting = self
            .directory
            .accounting_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("本地 return 缺少 owner 账本"))?;
        self.owners[owner as usize].begin_return(slot, &mut self.table)?;
        self.owners[owner as usize].queue_return(slot, &mut self.table, accounting, 0)?;
        self.owners[owner as usize].consume_return(slot, &mut self.table, &codec, accounting, bytes)
    }

    /// 发布转发目标：把旧 owner 置为 Draining/Forwarding，使在飞消息沿转发路径归还。
    pub(crate) fn begin_forwarding(
        &mut self,
        owner: u32,
        target: OwnerToken,
    ) -> Result<Epoch, RawInvariant> {
        let token = self.owners[owner as usize].token();
        let tick = self.epoch.next();
        self.directory.begin_drain(&token, tick)?;
        self.directory.begin_forward(&token, target, tick)?;
        self.epoch = tick;
        Ok(tick)
    }

    /// 构造一个 return message；integrity 由 per-domain secret 绑定全部身份字段。
    pub(crate) fn message(
        &self,
        target: OwnerToken,
        kind: ReturnKind,
        slot: RawSlot,
        bytes: u32,
    ) -> Result<ReturnMessage, RawInvariant> {
        let descriptor = self.descriptor(slot.descriptor)?;
        let mut message = ReturnMessage {
            next: None,
            target,
            kind,
            descriptor: slot.descriptor,
            unit: slot.index,
            bytes,
            source_epoch: self.epoch,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: if kind == ReturnKind::ResourceRelease {
                    slot.generation
                } else {
                    descriptor.generation
                },
                class: descriptor.class,
                owner_id: target.owner_id,
                route_key: target.route_key,
                checksum: 0,
            },
        };
        message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
        Ok(message)
    }

    /// producer 侧：用本 owner 的 world-owned staging 按目标 owner 路由。
    ///
    /// 返回本次调用产生的发布记录：目标改变时先有一条 `TargetChanged`，随后可能有一条由
    /// item/byte 上限或显式触发产生的发布。staging 由 `RawWorld` 持有，因此未发布的字节是
    /// credit 平面可以观测、也必须可以冲刷的真实在飞量。
    pub(crate) fn publish_message(
        &mut self,
        owner: u32,
        message: &ReturnMessage,
        shard: ShardIndex,
        forced: Option<FlushTrigger>,
    ) -> Result<Vec<PublishOutcome>, RawInvariant> {
        let mut outcomes = Vec::new();
        if let Some(previous) = self.return_stagings[owner as usize].target()
            && previous != message.target
        {
            let inbox = self.inbox_for(&previous)?;
            outcomes.push(flush_staging(
                &self.pool,
                &inbox,
                &mut self.return_stagings[owner as usize],
                FlushTrigger::TargetChanged,
            )?);
        }
        let inbox = self.inbox_for(&message.target)?;
        if let Some(outcome) = stage_message(
            &self.pool,
            Some(&inbox),
            &mut self.return_stagings[owner as usize],
            message,
            shard,
            forced,
        )? {
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    /// 把关闭的 source-slab ring 转成一个 return batch 并发布。
    ///
    /// ring 批次直接进入目标 owner 的 inbox，不经过 producer staging：staging 只承载单条
    /// return/card 消息，因此这里由 owner 自己推导目标与 shard。
    pub(crate) fn publish_ring(
        &self,
        owner: u32,
        closed: &super::message::ClosedRing,
    ) -> Result<u32, RawInvariant> {
        let target = self.token(owner);
        let shard = ShardIndex::from_raw(owner % super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("ring 关闭的 shard 编号越界"))?;
        let descriptor = self.descriptor(closed.key.0)?.clone();
        let mut first = None;
        let mut last = None;
        for slot in &closed.slots {
            let message =
                self.ring_message(&target, closed.key.0, &descriptor, *slot, closed.key.1)?;
            let node = self.pool.allocate()?;
            self.pool.store(node, &message, message.integrity.checksum);
            self.pool.link(node, None);
            if let Some(previous) = last {
                self.pool.link(previous, Some(node));
            }
            first = first.or(Some(node));
            last = Some(node);
        }
        let (Some(first), Some(last)) = (first, last) else {
            return Ok(0);
        };
        let chain = StagedChain {
            first,
            last,
            count: u32::try_from(closed.slots.len()).expect("ring 大小适配 u32"),
            bytes: closed.bytes,
            target: Some(target),
            shard: Some(shard),
        };
        let inbox = Arc::clone(&self.inboxes[owner as usize]);
        inbox.publish_batch(&chain, &self.pool)?;
        Ok(chain.count)
    }

    fn ring_message(
        &self,
        target: &OwnerToken,
        descriptor_id: SlabDescriptorId,
        descriptor: &super::slab::SlabDescriptor,
        slot: u32,
        generation: SlabGeneration,
    ) -> Result<ReturnMessage, RawInvariant> {
        let mut message = ReturnMessage {
            next: None,
            target: *target,
            kind: ReturnKind::RawSlot,
            descriptor: descriptor_id,
            unit: slot,
            bytes: descriptor.slot_stride,
            source_epoch: self.epoch,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation,
                class: descriptor.class,
                owner_id: target.owner_id,
                route_key: target.route_key,
                checksum: 0,
            },
        };
        message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
        Ok(message)
    }

    /// consumer 侧 same-slab 聚合：关闭本 owner 的 return cache 并发布。
    ///
    /// cache 与 staging 都由 `RawWorld` 持有，因此这里关闭的是 owner 真实的 cache；
    /// `GcHandoff`/`PressureDrain` 顺带把本 owner 的 barrier 账本带出，避免 card 键跟着
    /// ring 一起消失。
    pub(crate) fn close_cache(
        &mut self,
        owner: u32,
        reason: RingCloseReason,
    ) -> Result<u32, RawInvariant> {
        let closed = self.return_caches[owner as usize].close_all(reason);
        let mut published = 0;
        let barrier_reason = match reason {
            RingCloseReason::GcHandoff => Some(BarrierFlushReason::MinorStop),
            RingCloseReason::PressureDrain => Some(BarrierFlushReason::MemoryPressure),
            _ => None,
        };
        if let Some(barrier_reason) = barrier_reason {
            published += self.flush_all_barriers(owner, barrier_reason)?;
        }
        for ring in &closed {
            published += self.publish_ring(owner, ring)?;
        }
        Ok(published)
    }

    /// 把一个 owner 尚未发布的 return chain 交给目标 owner 的 inbox。
    ///
    /// credit 观测到的 `producer-staging` 字节就是这里的输入；drain/retire 前必须冲刷，
    /// 否则「cycle 边界前 credit 收敛」会看到真实的在飞量而不是观测盲区。
    pub(crate) fn flush_return_staging(
        &mut self,
        owner: u32,
        trigger: FlushTrigger,
    ) -> Result<u32, RawInvariant> {
        let Some(target) = self.return_stagings[owner as usize].target() else {
            return Ok(0);
        };
        let inbox = self.inbox_for(&target)?;
        let outcome = flush_staging(
            &self.pool,
            &inbox,
            &mut self.return_stagings[owner as usize],
            trigger,
        )?;
        Ok(outcome.items)
    }

    /// 把一个返回 slot 追加到 owner 的 source-slab 聚合 cache，并发布被关闭的 ring。
    ///
    /// 这是 consumer 侧 same-slab 聚合的接入点：命中同一 `(descriptor, generation)` 时继续
    /// 填充，victim ring 被驱逐时立即整批发布给目标 owner。返回本次发布的 slot 数。
    pub(crate) fn cache_return_slot(
        &mut self,
        owner: u32,
        key: (SlabDescriptorId, SlabGeneration),
        slot: u32,
        bytes: u64,
    ) -> Result<u32, RawInvariant> {
        let closed = self.return_caches[owner as usize].insert(key, slot, bytes);
        match closed {
            Some(ring) => self.publish_ring(owner, &ring),
            None => Ok(0),
        }
    }

    /// owner 上下文：按固定顺序 service 一个 shard。
    pub(crate) fn service(
        &mut self,
        owner: u32,
        shard: ShardIndex,
        budget: &ServiceBudget,
    ) -> Result<DrainReport, RawInvariant> {
        let inbox = Arc::clone(&self.inboxes[owner as usize]);
        let raw_token = self.owners[owner as usize].token();
        let resource_token = self.resource_owners[owner as usize].token();
        let snapshot = {
            let consumer = &self.consumers[owner as usize];
            inbox.snapshot(shard, consumer, budget, &self.pool)
        };
        let mut forwarded = 0_u32;
        let mut consumed = 0_u32;
        let mut extent_returns = 0_u32;
        // 先推进上一轮留在 grace 等待里的归还；它们已经过了线性化点，只差 epoch 计步。
        extent_returns += self.advance_pending_extent_trims()?;
        for node in snapshot.nodes() {
            let message_id = *node;
            // 消息族决定车道解释：card batch 只带 arena/card 序号，不进入 return 路径。
            if self.pool.family_of(message_id) == MessageFamilyTag::RegionTransfer {
                let batch = self.pool.load_region_transfer(message_id);
                if self.pool.owner_id_of(message_id) != self.owners[owner as usize].token().owner_id
                {
                    return Err(RawInvariant::new(
                        "region transfer 投递到非目标 owner 的 inbox",
                    ));
                }
                self.service_region_transfer(owner, &batch)?;
                self.graced_nodes.push(message_id);
                consumed += 1;
                continue;
            }
            if self.pool.family_of(message_id) == MessageFamilyTag::CardMark {
                let batch = self.pool.load_card_mark(message_id);
                if self.pool.owner_id_of(message_id) != self.owners[owner as usize].token().owner_id
                {
                    return Err(RawInvariant::new("card batch 投递到非目标 owner 的 inbox"));
                }
                self.service_card_mark(owner, &batch)?;
                self.graced_nodes.push(message_id);
                consumed += 1;
                continue;
            }
            if self.pool.family_of(message_id) == MessageFamilyTag::MarkTicket {
                let ticket = self.pool.load_mark_ticket(message_id);
                if self.pool.owner_id_of(message_id) != self.owners[owner as usize].token().owner_id
                {
                    return Err(RawInvariant::new("mark ticket 投递到非目标 owner 的 inbox"));
                }
                self.service_mark_ticket(owner, &ticket)?;
                self.graced_nodes.push(message_id);
                consumed += 1;
                continue;
            }
            let message = self.load_return_message(message_id)?;
            if message.kind == ReturnKind::StackSpan {
                self.service_stack_return(owner, &message)?;
                self.graced_nodes.push(message_id);
                continue;
            }
            let resource = message.kind == ReturnKind::ResourceRelease;
            if message.kind == ReturnKind::Extent {
                // 上面的载入键分支已经解析过 extent；这里只消费已经过校验的消息。
                extent_returns += u32::from(self.service_extent_return(owner, &message)?);
                self.graced_nodes.push(message_id);
                continue;
            }
            let token = if resource { resource_token } else { raw_token };
            if self.pool.owner_id_of(message_id) != token.owner_id {
                return Err(RawInvariant::new("消息投递到非目标 owner 的 inbox"));
            }
            if message.integrity.checksum != IntegrityTag::compute(&self.integrity_secret, &message)
            {
                return Err(RawInvariant::new("return message integrity 校验失败"));
            }
            let descriptor = self.descriptor(message.descriptor)?.clone();
            if message.bytes != descriptor.slot_stride {
                return Err(RawInvariant::new(
                    "return message 的 bytes 与 class stride 不一致",
                ));
            }
            if message.source_epoch > self.epoch {
                return Err(RawInvariant::new(
                    "return message 的 source epoch 越过当前 epoch",
                ));
            }
            match self.directory.resolve(&message.target) {
                Resolution::Match => {
                    if !descriptor.contains_index(message.unit) {
                        return Err(RawInvariant::new("return message 的 unit 越过 span"));
                    }
                    let state = self.table.state(message.descriptor, message.unit)?;
                    if state != SlotState::ReturnQueued {
                        return Err(RawInvariant::new(
                            "消息指向的 slot 不处于唯一的 ReturnQueued 状态",
                        ));
                    }
                    let slot = RawSlot {
                        descriptor: message.descriptor,
                        index: message.unit,
                        generation: if resource {
                            descriptor.generation
                        } else {
                            message.integrity.generation
                        },
                    };
                    let accounting = self
                        .directory
                        .accounting_mut(token.owner_id)
                        .ok_or_else(|| RawInvariant::new("consume 缺少 owner 账本"))?;
                    let codec = self.link_codec.clone();
                    if resource {
                        let (cell_generation, detached) = {
                            let cell = self.cells.get(message.descriptor, message.unit)?;
                            (cell.generation, cell.is_detached())
                        };
                        if cell_generation != message.integrity.generation.raw() {
                            return Err(RawInvariant::new(
                                "release 引用了过期 generation 的资源 cell",
                            ));
                        }
                        self.cells.complete_release(
                            message.descriptor,
                            message.unit,
                            message.integrity.generation,
                            detached,
                        )?;
                        self.resource_owners[owner as usize].consume_return(
                            slot,
                            &mut self.table,
                            &codec,
                            accounting,
                            u64::from(message.bytes),
                        )?;
                        self.cells
                            .finish_reclaim(message.descriptor, message.unit)?;
                    } else {
                        self.owners[owner as usize].consume_return(
                            slot,
                            &mut self.table,
                            &codec,
                            accounting,
                            u64::from(message.bytes),
                        )?;
                    }
                    consumed += 1;
                }
                Resolution::Forward(target) => {
                    self.forward_message(&message, target)?;
                    forwarded += 1;
                }
                Resolution::Retired | Resolution::Unknown => {
                    return Err(RawInvariant::new(
                        "owner 已 retire 的消息必须进入 domain injection 或按 retired 路径处理",
                    ));
                }
            }
            self.graced_nodes.push(message_id);
        }
        let mut consumer = std::mem::take(&mut self.consumers[owner as usize]);
        inbox.advance_front(shard, &mut consumer, &snapshot);
        self.consumers[owner as usize] = consumer;
        if forwarded > 0 {
            inbox.record_forward(shard, u64::from(forwarded));
        }
        let _ = consumed;
        Ok(DrainReport {
            items: snapshot.nodes().len() as u32,
            bytes: snapshot.bytes(),
            forwarded,
            extent_returns,
            stop: snapshot.stop(),
        })
    }

    /// 排空全部 shard，直到队列为空或只剩不可见链。
    pub(crate) fn drain_all(
        &mut self,
        owner: u32,
        budget: &ServiceBudget,
    ) -> Result<(u32, u32), RawInvariant> {
        // drain 之后 owner 不再跑代码，本 owner 的 card 键必须已经离开 processor 账本。
        self.flush_all_barriers(owner, BarrierFlushReason::ProducerStopGate)?;
        self.drain_inboxes(owner, budget, true)
    }

    fn forward_message(
        &self,
        message: &ReturnMessage,
        target: OwnerToken,
    ) -> Result<(), RawInvariant> {
        let inbox = self.inbox_for(&target)?;
        let node = self.pool.allocate()?;
        let mut forwarded = *message;
        forwarded.target = target;
        forwarded.state = MessageState::Forwarded;
        forwarded.integrity.owner_id = target.owner_id;
        forwarded.integrity.route_key = target.route_key;
        forwarded.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &forwarded);
        self.pool
            .store(node, &forwarded, forwarded.integrity.checksum);
        self.pool.link(node, None);
        let chain = StagedChain {
            first: node,
            last: node,
            count: 1,
            bytes: u64::from(forwarded.bytes),
            target: Some(target),
            shard: Some(ShardIndex::from_raw(0).expect("shard 0 合法")),
        };
        inbox.publish_batch(&chain, &self.pool)
    }

    /// 把 owner token 映射到 inbox 槽位；raw owner 与 resource owner 各占一个槽位。
    pub(super) fn owner_slot(&self, token: &OwnerToken) -> Result<usize, RawInvariant> {
        self.owners
            .iter()
            .position(|owner| &owner.token() == token)
            .or_else(|| {
                self.resource_owners
                    .iter()
                    .position(|owner| &owner.token() == token)
            })
            .ok_or_else(|| RawInvariant::new("目标 owner 没有登记的 inbox 槽位"))
    }

    pub(super) fn inbox_for(&self, token: &OwnerToken) -> Result<Arc<OwnerInbox>, RawInvariant> {
        let index = self.owner_slot(token)?;
        self.inboxes
            .get(index)
            .map(Arc::clone)
            .ok_or_else(|| RawInvariant::new("目标 owner 没有对应的 inbox"))
    }

    /// queue-page grace：发布新 epoch 与 reclaim gate。
    pub(crate) fn open_grace(&mut self, inbox: &OwnerInbox) -> Epoch {
        let epoch = inbox.gate().open_grace();
        self.epoch = epoch;
        epoch
    }

    /// 返回 grace 收敛结果。
    pub(crate) fn grace_outcome(inbox: &OwnerInbox) -> GraceOutcome {
        inbox.gate().grace_outcome()
    }

    /// 关闭 grace 并重新开放登记。
    pub(crate) fn close_grace(&mut self, inbox: &OwnerInbox) {
        inbox.gate().close_grace();
        self.epoch = self.epoch.next();
    }

    /// retire 协议：Draining → Forwarding → 独占排空 → grace → 回收 → Retired。
    pub(crate) fn retire(
        &mut self,
        owner: u32,
        target: OwnerToken,
        budget: &ServiceBudget,
    ) -> Result<RetireReport, RawInvariant> {
        let token = self.owners[owner as usize].token();
        let inbox = Arc::clone(&self.inboxes[owner as usize]);
        let tick = self.epoch.next();
        let state = self
            .directory
            .record(token.owner_id)
            .map(|record| record.state);
        match state {
            Some(super::slab::OwnerState::Active) => {
                self.directory.begin_drain(&token, tick)?;
                self.directory.begin_forward(&token, target, tick)?;
            }
            Some(super::slab::OwnerState::Draining) => {
                self.directory.begin_forward(&token, target, tick)?;
            }
            Some(super::slab::OwnerState::Forwarding) => {}
            _ => return Err(RawInvariant::new("owner 已 retire，不能重复 retire")),
        }
        self.stacks
            .trim_cache(owner, 0, &mut self.provider)
            .map_err(|error| self.stack_failure(error))?;
        self.epoch = tick;
        // retire 是 processor 交接：本 owner 的 barrier 账本必须在 inbox 排空与 grace 之前
        // 冲刷完，否则 arena owner 会在 card 键落地前看到“已经排空”的假象。
        self.flush_all_barriers(owner, BarrierFlushReason::ProcessorHandoff)?;
        let (forwarded, consumed) = self.drain_all(owner, budget)?;
        self.open_grace(&inbox);
        let grace = Self::grace_outcome(&inbox);
        if grace == GraceOutcome::Pending {
            return Ok(RetireReport {
                forwarded,
                consumed,
                grace,
                reclaimed_spans: 0,
                blocked_spans: 0,
            });
        }
        let reclaimed = self.reclaim(owner)?;
        self.directory.retire(&token, tick)?;
        self.close_grace(&inbox);
        Ok(RetireReport {
            forwarded,
            consumed,
            grace: GraceOutcome::Converged,
            reclaimed_spans: reclaimed.trimmed,
            blocked_spans: reclaimed.blocked_count(),
        })
    }

    /// 回收一个 owner 的空闲 span：逐 extent 通过 lease 与 grace 门禁后撤销物理页。
    ///
    /// 被门禁拒绝的 extent 保持 committed，本次不回收，并把原因记进 `RetireReport`；下一次
    /// retire 或 pressure trim 会重新尝试。绝不因为「看起来空闲」就撤销仍被 allocator、
    /// scanner 或 forwarder 使用的页。
    fn reclaim(&mut self, owner: u32) -> Result<TrimReport, RawInvariant> {
        let token = self.owners[owner as usize].token();
        // 候选、门禁与描述符释放都走 pressure trim 的同一条路径：两处不会各自维护一套
        // “空闲 extent” 判定，也不会一边撤销物理页一边把字节留在 committed 口径。
        let owned: Vec<_> = self
            .trim_candidates()
            .into_iter()
            .filter(|candidate| {
                candidate.occupancy.live_slots == 0
                    && candidate.occupancy.queued_slots == 0
                    && self.table.descriptors().iter().any(|descriptor| {
                        descriptor.extent == candidate.extent
                            && descriptor.owner == token
                            && descriptor.state != SlabState::Released
                    })
            })
            .collect();
        self.trim_extents(&owned)
    }

    /// 通过 queue-page grace 并复用已消费的 message node。
    ///
    /// 只在队列静止（producer 已停止或已排空）后调用：node 在 consumer 完成并经过 grace
    /// 前不得复用，否则 consumer 的 front/last 记账会指向被重新写入的 node。
    pub(crate) fn release_graced_nodes(&mut self) -> Result<u32, RawInvariant> {
        let nodes = std::mem::take(&mut self.graced_nodes);
        for node in &nodes {
            self.pool.release(*node)?;
        }
        for consumer in &mut self.consumers {
            consumer.reset();
        }
        Ok(u32::try_from(nodes.len()).expect("node 数适配 u32"))
    }

    /// 返回尚未过 grace 的 node 数量。
    pub(crate) fn pending_grace_nodes(&self) -> usize {
        self.graced_nodes.len()
    }

    /// 校验账本：committed 必须等于 pending、reclaimable、cache 与 live record 之和。
    pub(crate) fn ledger_invariant(&self, owner: u32) -> Result<(), RawInvariant> {
        let token = self.owners[owner as usize].token();
        let accounting = self
            .directory
            .accounting(token.owner_id)
            .ok_or_else(|| RawInvariant::new("账本校验引用未知 owner"))?;
        let mut live = 0_u64;
        let mut committed = 0_u64;
        for (index, descriptor) in self.table.descriptors().iter().enumerate() {
            if descriptor.owner != token {
                continue;
            }
            committed += descriptor.committed_bytes;
            let slots = descriptor.slot_count();
            for slot in 0..slots {
                if self.table.state(
                    SlabDescriptorId::from_raw(u32::try_from(index).expect("描述符下标适配 u32")),
                    slot,
                )? == SlotState::Live
                {
                    live += u64::from(descriptor.slot_stride);
                }
            }
        }
        let classified = accounting.pending_return_bytes()
            + accounting.reclaimable_bytes()
            + accounting.owner_cache_bytes()
            + live;
        if committed != classified {
            return Err(RawInvariant::new(format!(
                "账本分类不互斥：committed {committed}，分类合计 {classified}"
            )));
        }
        Ok(())
    }

    /// 校验 Resource domain 的账本：committed 等于 live、pending、reclaimable 与 cache 之和。
    pub(crate) fn resource_ledger_invariant(&self, owner: u32) -> Result<(), RawInvariant> {
        let token = self.resource_owners[owner as usize].token();
        let accounting = self
            .directory
            .accounting(token.owner_id)
            .ok_or_else(|| RawInvariant::new("资源账本校验引用未知 owner"))?;
        let mut live = 0_u64;
        let mut committed = 0_u64;
        for (index, descriptor) in self.table.descriptors().iter().enumerate() {
            if descriptor.owner != token {
                continue;
            }
            committed += descriptor.committed_bytes;
            let id = SlabDescriptorId::from_raw(u32::try_from(index).expect("描述符下标适配 u32"));
            for slot in 0..descriptor.slot_count() {
                if self.table.state(id, slot)? == SlotState::Live {
                    live += u64::from(descriptor.slot_stride);
                }
            }
        }
        let classified = accounting.pending_return_bytes()
            + accounting.reclaimable_bytes()
            + accounting.owner_cache_bytes()
            + live;
        if committed != classified {
            return Err(RawInvariant::new(format!(
                "资源账本分类不互斥：committed {committed}，分类合计 {classified}"
            )));
        }
        Ok(())
    }

    /// 校验 free 链完整性与描述符计数。
    pub(crate) fn verify_links(&self) -> Result<(), RawInvariant> {
        for (index, descriptor) in self.table.descriptors().iter().enumerate() {
            if descriptor.link_usable {
                let id =
                    SlabDescriptorId::from_raw(u32::try_from(index).expect("描述符下标适配 u32"));
                self.table.verify_free_chain(id, &self.link_codec)?;
            }
        }
        self.table.verify()
    }
}
