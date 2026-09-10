//! raw plane 世界：owner 集合、producer 发布路径、owner drain、grace 与 retire。
//!
//! producer 只接触自己的 staging、node pool 与目标 inbox；owner 本地的 descriptor、
//! free structure 与账本只由 owner 上下文读写。跨 owner 的归还先经过 exactly-once 的
//! `ReturnQueued` 状态迁移，再发布只携带逻辑序号的 return message。

use std::sync::Arc;

use super::inbox::{
    DrainReport, DrainStop, GraceOutcome, OwnerConsumer, OwnerInbox, ServiceBudget, ShardIndex,
};
use super::message::{
    BatchLimits, FlushTrigger, IntegrityTag, LinkCodec, MessageState, ProducerStaging,
    PublishOutcome, ReturnKind, ReturnMessage, ReturnNodeId, ReturnNodePool, ReturnSlabCache,
    RingCloseReason, StagedChain, flush_staging, stage_message,
};
use super::owner::{Allocation, RawOwner};
use super::provider::{FakeRangeProvider, ProviderStats, RangeDescriptor, RangeProvider};
use super::size_class::{RuntimeSizeClassId, RuntimeSizeClassTable};
use super::slab::{
    Epoch, MemoryDomainId, OwnerAccounting, OwnerDirectory, OwnerToken, RawInvariant, RawSlot,
    Resolution, RuntimeSeed, SlabDescriptorId, SlabGeneration, SlabTable, SlotState,
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
}

/// raw plane 的可运行模型世界。
#[derive(Debug)]
pub(crate) struct RawWorld {
    classes: RuntimeSizeClassTable,
    provider: FakeRangeProvider,
    table: SlabTable,
    directory: OwnerDirectory,
    owners: Vec<RawOwner>,
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
        let integrity_secret = seed.secret();
        let link_codec = LinkCodec::new(seed.secret());
        let mut owner_list = Vec::with_capacity(owners as usize);
        let mut inboxes = Vec::with_capacity(owners as usize);
        let mut consumers = Vec::with_capacity(owners as usize);
        for _ in 0..owners {
            let token =
                directory.register(&mut seed, MemoryDomainId::RUNTIME_RAW, Epoch::from_raw(0));
            owner_list.push(RawOwner::new(token, &classes));
            inboxes.push(Arc::new(OwnerInbox::new(super::OWNER_INBOX_SHARDS)));
            consumers.push(OwnerConsumer::new(super::OWNER_INBOX_SHARDS));
        }
        Ok(Self {
            classes,
            provider: FakeRangeProvider::new(u64::from(u32::MAX)),
            table: SlabTable::new(),
            directory,
            owners: owner_list,
            inboxes,
            consumers,
            pool: Arc::new(ReturnNodePool::new(node_capacity)),
            link_codec,
            integrity_secret,
            limits,
            epoch: Epoch::from_raw(0),
            domain_owner,
            graced_nodes: Vec::new(),
        })
    }

    /// 返回 owner 数量。
    pub(crate) fn owner_count(&self) -> u32 {
        self.owners.len() as u32
    }

    /// 返回 owner 的 token。
    pub(crate) fn token(&self, index: u32) -> OwnerToken {
        self.owners[index as usize].token()
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

    /// 返回 provider 统计。
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
        let class = *self
            .classes
            .get(class)
            .ok_or_else(|| RawInvariant::new("分配引用未知 class"))?;
        let token = self.owners[owner as usize].token();
        let secret_index = u32::from(class.id.raw());
        let accounting = self
            .directory
            .accounting_mut(token.owner_id)
            .ok_or_else(|| RawInvariant::new("分配缺少 owner 账本"))?;
        let codec = self.link_codec.clone();
        let provider = &mut self.provider;
        let table = &mut self.table;
        let slab_epoch = self.epoch;
        self.owners[owner as usize].allocate(
            &class,
            table,
            provider,
            &codec,
            accounting,
            secret_index,
            slab_epoch,
        )
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
                generation: descriptor.generation,
                class: descriptor.class,
                owner_id: target.owner_id,
                route_key: target.route_key,
                checksum: 0,
            },
        };
        message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
        Ok(message)
    }

    /// producer 侧：按目标 owner 路由，必要时先刷新旧 chain 再暂存。
    ///
    /// 返回本次调用产生的发布记录：目标改变时先有一条 `TargetChanged`，随后可能有一条由
    /// item/byte 上限或显式触发产生的发布。
    pub(crate) fn publish_message(
        &self,
        staging: &mut ProducerStaging,
        message: &ReturnMessage,
        shard: ShardIndex,
        forced: Option<FlushTrigger>,
    ) -> Result<Vec<PublishOutcome>, RawInvariant> {
        let mut outcomes = Vec::new();
        if let Some(previous) = staging.target()
            && previous != message.target
        {
            let inbox = self.inbox_for(&previous)?;
            outcomes.push(flush_staging(
                &self.pool,
                &inbox,
                staging,
                FlushTrigger::TargetChanged,
            )?);
        }
        let inbox = self.inbox_for(&message.target)?;
        if let Some(outcome) =
            stage_message(&self.pool, Some(&inbox), staging, message, shard, forced)?
        {
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    /// 把关闭的 source-slab ring 转成一个 return batch 并发布。
    pub(crate) fn publish_ring(
        &self,
        owner: u32,
        staging: &mut ProducerStaging,
        closed: &super::message::ClosedRing,
    ) -> Result<u32, RawInvariant> {
        let target = staging
            .target()
            .ok_or_else(|| RawInvariant::new("ring 关闭缺少目标 owner"))?;
        let shard = staging
            .shard()
            .ok_or_else(|| RawInvariant::new("ring 关闭缺少目标 shard"))?;
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

    /// consumer 侧 same-slab 聚合：关闭全部 open ring 并发布。
    pub(crate) fn close_cache(
        &self,
        owner: u32,
        staging: &mut ProducerStaging,
        cache: &mut ReturnSlabCache,
        reason: RingCloseReason,
    ) -> Result<u32, RawInvariant> {
        let closed = cache.close_all(reason);
        let mut published = 0;
        for ring in &closed {
            published += self.publish_ring(owner, staging, ring)?;
        }
        Ok(published)
    }

    /// owner 上下文：按固定顺序 service 一个 shard。
    pub(crate) fn service(
        &mut self,
        owner: u32,
        shard: ShardIndex,
        budget: &ServiceBudget,
    ) -> Result<DrainReport, RawInvariant> {
        let inbox = Arc::clone(&self.inboxes[owner as usize]);
        let token = self.owners[owner as usize].token();
        let snapshot = {
            let consumer = &self.consumers[owner as usize];
            inbox.snapshot(shard, consumer, budget, &self.pool)
        };
        let mut forwarded = 0_u32;
        let mut consumed = 0_u32;
        for node in snapshot.nodes() {
            let message_id = *node;
            let descriptor_id = self.pool.descriptor_of(message_id);
            let descriptor = self.descriptor(descriptor_id)?.clone();
            let message = self
                .pool
                .load(message_id, descriptor.class, descriptor.generation);
            if self.pool.owner_id_of(message_id) != token.owner_id {
                return Err(RawInvariant::new("消息投递到非目标 owner 的 inbox"));
            }
            if message.integrity.checksum != IntegrityTag::compute(&self.integrity_secret, &message)
            {
                return Err(RawInvariant::new("return message integrity 校验失败"));
            }
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
                        generation: message.integrity.generation,
                    };
                    let accounting = self
                        .directory
                        .accounting_mut(token.owner_id)
                        .ok_or_else(|| RawInvariant::new("consume 缺少 owner 账本"))?;
                    let codec = self.link_codec.clone();
                    self.owners[owner as usize].consume_return(
                        slot,
                        &mut self.table,
                        &codec,
                        accounting,
                        u64::from(message.bytes),
                    )?;
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
            stop: snapshot.stop(),
        })
    }

    /// 排空全部 shard，直到队列为空或只剩不可见链。
    pub(crate) fn drain_all(
        &mut self,
        owner: u32,
        budget: &ServiceBudget,
    ) -> Result<(u32, u32), RawInvariant> {
        let mut forwarded = 0_u32;
        let mut consumed = 0_u32;
        for index in 0..super::OWNER_INBOX_SHARDS {
            let shard = ShardIndex::from_raw(index).expect("shard 编号合法");
            loop {
                let report = self.service(owner, shard, budget)?;
                forwarded += report.forwarded;
                consumed += report.items - report.forwarded;
                if report.items == 0 || report.stop != DrainStop::Budget {
                    break;
                }
            }
        }
        Ok((forwarded, consumed))
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

    fn inbox_for(&self, token: &OwnerToken) -> Result<Arc<OwnerInbox>, RawInvariant> {
        let raw = token
            .owner_id
            .raw()
            .checked_sub(1)
            .ok_or_else(|| RawInvariant::new("domain owner 不是 raw slab 的回收目标"))?;
        let index = usize::try_from(raw).map_err(|_| RawInvariant::new("owner 编号越界"))?;
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
        self.epoch = tick;
        let (forwarded, consumed) = self.drain_all(owner, budget)?;
        self.open_grace(&inbox);
        let grace = Self::grace_outcome(&inbox);
        if grace == GraceOutcome::Pending {
            return Ok(RetireReport {
                forwarded,
                consumed,
                grace,
                reclaimed_spans: 0,
            });
        }
        let reclaimed = self.reclaim(owner)?;
        self.directory.retire(&token, tick)?;
        self.close_grace(&inbox);
        Ok(RetireReport {
            forwarded,
            consumed,
            grace: GraceOutcome::Converged,
            reclaimed_spans: reclaimed,
        })
    }

    fn reclaim(&mut self, owner: u32) -> Result<u32, RawInvariant> {
        let token = self.owners[owner as usize].token();
        let mut reclaimed = 0_u32;
        let mut ranges = Vec::new();
        for descriptor in self.table.descriptors() {
            if descriptor.owner == token && descriptor.live == 0 && descriptor.queued == 0 {
                ranges.push(descriptor.range);
            }
        }
        let bytes: u64 = ranges
            .iter()
            .filter_map(|range| self.provider.describe(*range))
            .map(|range| range.bytes)
            .sum();
        for range in ranges {
            self.provider.decommit(range)?;
            self.provider.release(range)?;
            reclaimed += 1;
        }
        if let Some(accounting) = self.directory.accounting_mut(token.owner_id) {
            accounting.release(bytes);
        }
        Ok(reclaimed)
    }

    /// 通过 queue-page grace 并复用已消费的 message node。
    ///
    /// 只在队列静止（producer 已停止或已排空）后调用：node 在 consumer 完成并经过 grace
    /// 前不得复用，否则 consumer 的 front/last 记账会指向被重新写入的 node。
    pub(crate) fn release_graced_nodes(&mut self) -> Result<u32, RawInvariant> {
        let nodes = std::mem::take(&mut self.graced_nodes);
        let mut seen = std::collections::BTreeSet::new();
        for node in &nodes {
            if !seen.insert(node.raw()) {
                eprintln!("DEBUG grace: node {} 重复出现", node.raw());
            }
        }
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
