//! owner arena：平台 range 与 extent 阶梯之间的唯一接缝。
//!
//! 每个 owner 在每个 domain 上持有固定容量的 arena range。arena 只做一次 `reserve_aligned`，
//! 之后全部分配都从 arena 的二次幂 buddy 阶梯里切出 extent；`commit`/`decommit` 以页为粒度
//! 作用在 arena range 的对应子区间上。slab 层因此只看到 extent 编号，永远看不到平台 range。
//!
//! arena 容量取二次幂阶梯的顶层，保证任何 class 的块都能整块落在 arena 内且按自身大小对齐。

use super::super::extent::{
    ExtentDescriptor, ExtentId, ExtentOccupancy, ExtentState, ExtentTable, TrimBlocked, TrimReport,
};
use super::super::inbox::ShardIndex;
#[cfg(test)]
use super::super::inbox::{DrainStop, ServiceBudget};
use super::super::message::{
    FlushTrigger, IntegrityTag, MessageState, ProducerStaging, ReturnKind, ReturnMessage,
    ReturnNodeId, stage_message,
};
use super::super::provider::{DumpPolicy, ProviderError, RangeProvider};
use super::super::size_class::RuntimeSizeClassId;
use super::super::slab::{
    MemoryDomainId, OwnerId, OwnerToken, RawInvariant, SlabDescriptorId, SlabGeneration, SlabState,
};
use super::RawWorld;

/// 一个 trim 候选：门禁所需的 occupancy 加 pause 预算所需的真实度量。
///
/// `revoked_bytes` 与 `descriptors` 与 `occupancy` 在同一次描述符表扫描里聚合：pause 判定
/// 因此用的是物理事实而不是估算，release 时落账的也是同一批字节与描述符。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TrimCandidate {
    /// 候选 extent。
    pub(super) extent: ExtentId,
    /// allocator/scanner/forwarder 门禁读取的占用。
    pub(super) occupancy: ExtentOccupancy,
    /// 本 extent 名下尚未 `Released` 的 descriptor 的已提交字节。
    pub(super) revoked_bytes: u64,
    /// 本 extent 名下的 descriptor 数。
    pub(super) descriptors: u32,
}

impl Default for TrimCandidate {
    /// 空候选：`ExtentId` 没有语义上的零值，默认候选只用于「未发过的 extent」占位，
    /// 调用方必须在插入时写入真实编号。
    fn default() -> Self {
        Self {
            extent: ExtentId::from_raw(0),
            occupancy: ExtentOccupancy::default(),
            revoked_bytes: 0,
            descriptors: 0,
        }
    }
}

/// 一个 owner arena 的容量；等于二次幂 extent 阶梯的顶层。
pub(crate) const OWNER_ARENA_BYTES: u64 = 2 * 1024 * 1024;

impl RawWorld {
    /// 为一个 owner 的某个 domain 建立 arena，并登记 extent 阶梯。
    ///
    /// 只预留虚拟地址：物理页在 extent 被发放时按页提交，因此 arena 本身全部计入
    /// `range_reserved_bytes`，与 `runtime_committed_bytes` 严格互斥。
    pub(super) fn open_arena(
        &mut self,
        owner: u32,
        token: OwnerToken,
        domain: MemoryDomainId,
    ) -> Result<u32, RawInvariant> {
        let range = self
            .provider
            .reserve_aligned(OWNER_ARENA_BYTES, OWNER_ARENA_BYTES, domain)?;
        let base = self
            .provider
            .describe(range)
            .ok_or_else(|| RawInvariant::new("arena 预留后描述缺失"))?
            .base;
        let index =
            self.extents
                .register_owner(owner, token, domain, range, base, OWNER_ARENA_BYTES)?;
        Ok(index)
    }

    /// 从 owner 的 arena 取得一个 extent 并提交它的页。
    ///
    /// 返回 extent 编号与它在 arena 内的字节偏移；调用者把 extent 登记进 slab 描述符。
    pub(super) fn take_extent(
        &mut self,
        owner: u32,
        class: u32,
        domain: MemoryDomainId,
    ) -> Result<ExtentId, RawInvariant> {
        let extent = self.extents.allocate(owner, class, domain)?;
        let range = self
            .extents
            .arena_range_of(extent)
            .ok_or_else(|| RawInvariant::new("extent 缺少所属 arena"))?;
        let offset = self.extents.offset_of_id(extent);
        let bytes = self
            .extents
            .descriptor(extent)
            .ok_or_else(|| RawInvariant::new("extent 描述缺失"))?
            .bytes;
        self.provider.commit_pages(range, offset, bytes)?;
        Ok(extent)
    }

    /// 归还一个 extent：先撤销它的物理页，再合并回 buddy 阶梯。
    ///
    /// 调用者必须先通过 `poll_trim` 的四条门禁；这里只执行平台侧动作。
    pub(super) fn trim_extent(&mut self, extent: ExtentId) -> Result<u64, RawInvariant> {
        let descriptor = *self
            .extents
            .descriptor(extent)
            .ok_or_else(|| RawInvariant::new("trim 引用未知 extent"))?;
        let range = self
            .extents
            .arena_range_of(extent)
            .ok_or_else(|| RawInvariant::new("trim 的 extent 缺少所属 arena"))?;
        let offset = self.extents.offset_of(&descriptor);
        self.provider
            .decommit_pages(range, offset, descriptor.bytes)?;
        self.extents.give_back(extent)?;
        Ok(descriptor.bytes)
    }

    /// 尝试对一个 extent 执行 trim：四条门禁全部满足才撤销物理页。
    ///
    /// 返回 `Ok(Err(reason))` 表示门禁未通过（正常流程，调用者记录原因）；返回 `Err` 表示
    /// 平台调用本身失败，这是实现故障而不是门禁。
    pub(super) fn poll_trim_extent(
        &mut self,
        extent: ExtentId,
        occupancy: ExtentOccupancy,
    ) -> Result<Result<u64, TrimBlocked>, RawInvariant> {
        let epoch = self.epoch;
        if let Err(blocked) = self.extents.poll_trim(extent, epoch, occupancy) {
            return Ok(Err(blocked));
        }
        self.trim_extent(extent).map(Ok)
    }

    /// pressure trim：把全部空闲且无 pending return 的 Live extent 交回 buddy 阶梯。
    ///
    /// occupancy 与 pause 度量都从 slab 描述符表按 extent 聚合而来，不含任何猜测：一个
    /// extent 只有在 allocator、scanner、forwarder 三路 lease 均归零、没有 live 或 queued
    /// slot、也没有在途 return 时才允许 decommit。
    ///
    /// 返回按 extent 编号排序的候选集（`BTreeMap` 自身有序），供调用方先做 pause 预算判定
    /// 再执行 trim。
    pub(super) fn trim_candidates(&self) -> Vec<TrimCandidate> {
        let mut candidates: std::collections::BTreeMap<ExtentId, TrimCandidate> =
            std::collections::BTreeMap::new();
        for descriptor in self.table.descriptors() {
            if descriptor.state == SlabState::Released {
                continue;
            }
            let entry = candidates.entry(descriptor.extent).or_default();
            entry.extent = descriptor.extent;
            entry.occupancy.live_slots += descriptor.live;
            entry.occupancy.queued_slots += descriptor.queued;
            entry.occupancy.pending_returns += descriptor.pending_returns;
            entry.revoked_bytes = entry
                .revoked_bytes
                .saturating_add(descriptor.committed_bytes);
            entry.descriptors = entry.descriptors.saturating_add(1);
        }
        // 从未发过的 extent 没有被任何描述符引用，天然满足空载门禁。
        for descriptor in self.extents.descriptors() {
            if descriptor.state == ExtentState::Live && !candidates.contains_key(&descriptor.id) {
                candidates.insert(
                    descriptor.id,
                    TrimCandidate {
                        extent: descriptor.id,
                        ..TrimCandidate::default()
                    },
                );
            }
        }
        candidates.into_values().collect()
    }

    /// 对候选 extent 逐个执行四重门禁；未过门禁的保持 committed 并被计入 `blocked`，
    /// 由后续 drain 继续推进，而不是“看起来空闲”就撤销物理页。
    ///
    /// 通过门禁的 extent 同时让名下全部空载 descriptor 离开 committed 口径：物理页与账本
    /// 必须同一步回落，否则分类之和会持续虚报已撤销的 extent。
    pub(super) fn trim_extents(
        &mut self,
        candidates: &[TrimCandidate],
    ) -> Result<TrimReport, RawInvariant> {
        let mut report = TrimReport::default();
        for candidate in candidates {
            match self.poll_trim_extent(candidate.extent, candidate.occupancy)? {
                Ok(_) => {
                    self.release_extent_descriptors(candidate.extent)?;
                    report.trimmed += 1;
                }
                Err(blocked) => report.blocked.push((candidate.extent, blocked)),
            }
        }
        Ok(report)
    }

    /// 让一个已撤销物理页的 extent 名下全部 descriptor 离开 committed 口径。
    ///
    /// 每个 descriptor 的字节先从对应 owner 账本释放，再标记为 `Released` 并把
    /// `committed_bytes` 清零；编号保留但永不复用，二次 trim 不会再把它当候选。
    fn release_extent_descriptors(&mut self, extent: ExtentId) -> Result<(), RawInvariant> {
        let owned: Vec<(SlabDescriptorId, OwnerId, u64)> = self
            .table
            .descriptors()
            .iter()
            .enumerate()
            .filter(|(_, descriptor)| {
                descriptor.extent == extent && descriptor.state != SlabState::Released
            })
            .filter_map(|(index, descriptor)| {
                Some((
                    SlabDescriptorId::from_raw(u32::try_from(index).ok()?),
                    descriptor.owner.owner_id,
                    descriptor.committed_bytes,
                ))
            })
            .collect();
        for (descriptor, owner_id, bytes) in owned {
            if let Some(accounting) = self.directory.accounting_mut(owner_id) {
                accounting.release(bytes);
            }
            let record = self
                .table
                .descriptor_mut(descriptor)
                .ok_or_else(|| RawInvariant::new("撤销 extent 缺少 descriptor"))?;
            record.state = SlabState::Released;
            record.committed_bytes = 0;
        }
        Ok(())
    }

    /// 把一个 extent 归入跨 owner 归还：进入 `ReturnQueued` 并构造 `ReturnKind::Extent` 消息。
    ///
    /// 归还的线性化点在这里：三路 lease 必须先全部归零，状态才迁移到 `ReturnQueued`，之后只有
    /// 一条消息存在。lease 未归零时这里直接拒绝且不改变状态，因此调用方仍能正常结束 lease 并
    /// 重试；decommit 因此永远不可能发生在 allocator/scanner/forwarder 使用期间。
    ///
    /// queue-page grace 是线性化点之后的等待：它按 epoch 累积，未走完时 extent 保持
    /// `ReturnQueued`，由后续 owner service 或 pressure trim 继续推进，不需要重新发布消息。
    pub(crate) fn release_extent(
        &mut self,
        extent: ExtentId,
    ) -> Result<ReturnMessage, RawInvariant> {
        let descriptor = *self
            .extents
            .descriptor(extent)
            .ok_or_else(|| RawInvariant::new("extent 归还引用未知 extent"))?;
        if descriptor.state != ExtentState::Live {
            return Err(RawInvariant::new("只有 Live extent 才能进入唯一的归还点"));
        }
        if let Some(lease) = descriptor.leases.outstanding() {
            return Err(RawInvariant::new(format!(
                "extent 归还前 {} lease 必须归零",
                lease.name()
            )));
        }
        let target = self.extent_return_target(&descriptor)?;
        self.extents.mark_return_queued(extent)?;
        self.extent_message(target, extent, &descriptor)
    }

    /// 把 extent 归还消息发布到目标 owner 的 inbox。
    pub(crate) fn publish_extent_return(
        &self,
        message: &ReturnMessage,
        shard: ShardIndex,
    ) -> Result<(), RawInvariant> {
        let inbox = self.inbox_for(&message.target)?;
        let mut staging = ProducerStaging::new(self.limits);
        stage_message(
            &self.pool,
            Some(&inbox),
            &mut staging,
            message,
            shard,
            Some(FlushTrigger::OwnerPressure),
        )?;
        Ok(())
    }

    /// 返回 extent 所属 arena 的 owner 在 owner 表中的稠密下标。
    ///
    /// arena 按 domain 归属：`RUNTIME_RAW` arena 属于 raw owner，`RESOURCE` arena 属于 resource
    /// owner。descriptor 保存的是 owner token 而不是下标，因为 token 的 `owner_id` 是全局目录
    /// 编号，两个 domain 的 owner 共享同一编号空间；按下标解读会把归还投给错误的 owner。
    pub(super) fn extent_owner_index(
        &self,
        descriptor: &ExtentDescriptor,
    ) -> Result<usize, RawInvariant> {
        let owners = match descriptor.domain {
            MemoryDomainId::RUNTIME_RAW => &self.owners,
            MemoryDomainId::RESOURCE => &self.resource_owners,
            other => {
                return Err(RawInvariant::new(format!(
                    "{} domain 没有 owner arena，不能发布 extent 归还",
                    other.name()
                )));
            }
        };
        owners
            .iter()
            .position(|owner| owner.token() == descriptor.owner)
            .ok_or_else(|| RawInvariant::new("extent 归还引用未登记的 owner arena"))
    }

    /// 返回 extent 所属 arena 的 owner token。
    pub(super) fn extent_return_target(
        &self,
        descriptor: &ExtentDescriptor,
    ) -> Result<OwnerToken, RawInvariant> {
        let index = self.extent_owner_index(descriptor)?;
        let owners = if descriptor.domain == MemoryDomainId::RESOURCE {
            &self.resource_owners
        } else {
            &self.owners
        };
        owners
            .get(index)
            .map(|owner| owner.token())
            .ok_or_else(|| RawInvariant::new("extent 归还引用未登记的 owner arena"))
    }

    /// 构造 extent 归还消息。
    ///
    /// integrity 的载入键取自 extent 阶梯自身：`class` 车道保存 extent class 编号，
    /// `generation` 车道保存 extent generation。引用过期 extent 的投递因此必然校验失败。
    fn extent_message(
        &self,
        target: OwnerToken,
        extent: ExtentId,
        descriptor: &ExtentDescriptor,
    ) -> Result<ReturnMessage, RawInvariant> {
        let Ok(bytes) = u32::try_from(descriptor.bytes) else {
            return Err(RawInvariant::new("extent 字节数越过 return 消息上限"));
        };
        let Ok(class) = u16::try_from(descriptor.class) else {
            return Err(RawInvariant::new("extent class 编号越过 integrity 车道"));
        };
        let mut message = ReturnMessage {
            next: None,
            target,
            kind: ReturnKind::Extent,
            descriptor: SlabDescriptorId::from_raw(extent.raw()),
            unit: extent.raw(),
            bytes,
            source_epoch: self.epoch,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: SlabGeneration::from_raw(u64::from(descriptor.generation.raw())),
                class: RuntimeSizeClassId::from_raw(class),
                owner_id: target.owner_id,
                route_key: target.route_key,
                checksum: 0,
            },
        };
        message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
        Ok(message)
    }

    /// 设置一个 range 的 dump policy；metadata 与 owner secret 使用 `Excluded`。
    pub(crate) fn set_dump_policy(
        &mut self,
        range: super::super::provider::RangeId,
        policy: DumpPolicy,
    ) -> Result<(), ProviderError> {
        self.provider.set_dump_policy(range, policy)
    }

    /// 返回 extent 表的只读视图。
    pub(crate) const fn extents(&self) -> &ExtentTable {
        &self.extents
    }

    /// 对一个 owner 执行一次 reclaim，返回被拒绝的全部门禁原因。
    ///
    /// 供确定性测试直接观察门禁结果；生产路径经 `retire` 与 pressure trim 调用同一个
    /// `reclaim`。
    #[cfg(test)]
    pub(crate) fn reclaim_extents_for_test(
        &mut self,
        owner: u32,
    ) -> Result<Vec<TrimBlocked>, RawInvariant> {
        let report = self.reclaim(owner)?;
        Ok(report
            .blocked
            .into_iter()
            .map(|(_, blocked)| blocked)
            .collect())
    }

    /// 在一个 owner 的 extent 上取得一条 lease；供门禁测试构造交错。
    #[cfg(test)]
    pub(crate) fn acquire_extent_lease_for_test(
        &mut self,
        owner: u32,
        extent: ExtentId,
        lease: super::super::extent::ExtentLease,
    ) -> Result<(), RawInvariant> {
        let _ = owner;
        self.extents.acquire_lease(extent, lease)
    }

    /// 结束一个 owner 的 extent lease。
    #[cfg(test)]
    pub(crate) fn release_extent_lease_for_test(
        &mut self,
        extent: ExtentId,
        lease: super::super::extent::ExtentLease,
    ) -> Result<(), RawInvariant> {
        self.extents.release_lease(extent, lease)
    }

    /// 推进一个 epoch，使 grace 计步前进。
    #[cfg(test)]
    pub(crate) fn advance_epoch_for_test(&mut self) {
        self.epoch = self.epoch.next();
    }

    /// 发布一个 extent 归还（若它还没过线性化点），再在目标 owner 上下文 service 它。
    ///
    /// 供确定性测试驱动完整的跨 owner 归还路径；生产路径由 pressure trim 与 owner service 触发
    /// 同一组函数。extent 已经在等 grace 时不再重新发布消息，只 service 一次推进 ticket。
    #[cfg(test)]
    pub(crate) fn return_extent_for_test(
        &mut self,
        owner: u32,
        extent: ExtentId,
    ) -> Result<u32, RawInvariant> {
        let live = self
            .extents
            .descriptor(extent)
            .is_some_and(|descriptor| descriptor.state == ExtentState::Live);
        if live {
            let message = self.release_extent(extent)?;
            self.publish_extent_return(&message, ShardIndex::from_raw(0).expect("shard 0 合法"))?;
        }
        let budget = ServiceBudget::new(8, 1 << 16);
        let mut completed = 0;
        for index in 0..super::super::OWNER_INBOX_SHARDS {
            let shard = ShardIndex::from_raw(index).expect("shard 编号合法");
            loop {
                let report = self.service(owner, shard, &budget)?;
                completed += report.extent_returns;
                if report.items == 0 || report.stop != DrainStop::Budget {
                    break;
                }
            }
        }
        Ok(completed)
    }

    /// 只发布 extent 归还消息，不消费它；供测试构造在途 return。
    #[cfg(test)]
    pub(crate) fn stage_extent_return_for_test(
        &mut self,
        extent: ExtentId,
    ) -> Result<super::super::message::ReturnMessage, RawInvariant> {
        let message = self.release_extent(extent)?;
        self.publish_extent_return(&message, ShardIndex::from_raw(0).expect("shard 0 合法"))?;
        Ok(message)
    }
}

impl RawWorld {
    /// 从 return node 重建完整消息。
    ///
    /// 载入键来自消息引用的对象：slab 消息用 slab 描述符的 class 与 generation，extent 消息用
    /// extent 阶梯的 class 与 generation。integrity checksum 覆盖这对载入键，因此引用已过期
    /// 对象的投递必然校验失败，而不是被当成另一条合法消息。
    pub(super) fn load_return_message(
        &mut self,
        message_id: ReturnNodeId,
    ) -> Result<ReturnMessage, RawInvariant> {
        if self.pool.kind_of(message_id) == ReturnKind::StackSpan {
            return self.load_stack_return(message_id);
        }
        if self.pool.kind_of(message_id) == ReturnKind::Extent {
            let extent = ExtentId::from_raw(self.pool.descriptor_of(message_id).raw());
            let descriptor = *self
                .extents
                .descriptor(extent)
                .ok_or_else(|| RawInvariant::new("extent 归还引用未知 extent"))?;
            let Ok(class) = u16::try_from(descriptor.class) else {
                return Err(RawInvariant::new("extent class 编号越过 integrity 车道"));
            };
            return Ok(self.pool.load(
                message_id,
                RuntimeSizeClassId::from_raw(class),
                SlabGeneration::from_raw(u64::from(descriptor.generation.raw())),
            ));
        }
        let descriptor = self
            .descriptor(self.pool.descriptor_of(message_id))?
            .clone();
        Ok(self
            .pool
            .load(message_id, descriptor.class, descriptor.generation))
    }

    /// owner 上下文：消费一条 extent 归还消息。
    ///
    /// exactly-once 由 extent 状态机保证：只有 `ReturnQueued` 的 extent 才能被归还，归还后描述符
    /// 直接回到 `Vacant`，因此同一 extent 的第二次投递必然被拒绝。门禁（三路 lease、live/queued
    /// slot、在途 return、grace）与 pressure trim 走同一个 `poll_trim`，区别只是调用点。
    ///
    /// 返回 `true` 表示这次 service 真正完成了归还（物理页已撤销、账本已扣减）；返回 `false`
    /// 表示 grace 尚未走完，消息已被消费但 extent 仍留在 `ReturnQueued` 等待后续推进。
    pub(super) fn service_extent_return(
        &mut self,
        owner: u32,
        message: &ReturnMessage,
    ) -> Result<bool, RawInvariant> {
        let extent = ExtentId::from_raw(message.descriptor.raw());
        let descriptor = *self
            .extents
            .descriptor(extent)
            .ok_or_else(|| RawInvariant::new("extent 归还引用未知 extent"))?;
        let target = self.extent_return_target(&descriptor)?;
        if target != message.target {
            return Err(RawInvariant::new(
                "extent 归还消息的目标 owner 与 arena 归属不一致",
            ));
        }
        if self.extent_owner_index(&descriptor)? != owner as usize {
            return Err(RawInvariant::new(
                "extent 归还消息投递到非 arena owner 的上下文",
            ));
        }
        if descriptor.state != ExtentState::ReturnQueued {
            return Err(RawInvariant::new(
                "extent 不处于唯一的 ReturnQueued 状态，归还只能发生一次",
            ));
        }
        // 归还已在线性化点之前结束：这个 extent 上没有 live/queued slot，也没有在途消息，
        // 只剩三路 lease 与 grace 两道路径门禁，与 pressure trim 走同一个 `poll_trim`。
        let occupancy = ExtentOccupancy {
            live_slots: 0,
            queued_slots: 0,
            pending_returns: 0,
        };
        match self.poll_trim_extent(extent, occupancy)? {
            Ok(bytes) => {
                self.release_extent_bytes(target.owner_id, bytes)?;
                Ok(true)
            }
            // grace 未走完是等待而不是失败：ticket 已经按 epoch 记进 extent 表，extent 保持
            // `ReturnQueued`，登记进待决队列由后续 owner service 继续推进。
            Err(TrimBlocked::GracePending { .. }) => {
                self.pending_extent_trims.push((extent, target.owner_id));
                Ok(false)
            }
            Err(blocked) => Err(RawInvariant::new(format!(
                "extent 归还被门禁拒绝：{}",
                blocked.describe()
            ))),
        }
    }

    /// 推进所有正在等 grace 的 extent 归还。
    ///
    /// 每条记录在 grace 走完后完成归还并从待决队列移除；仍未走完的留在队列里等下一次 service。
    /// 返回本轮真正完成的归还数量。
    pub(super) fn advance_pending_extent_trims(&mut self) -> Result<u32, RawInvariant> {
        if self.pending_extent_trims.is_empty() {
            return Ok(0);
        }
        let pending = std::mem::take(&mut self.pending_extent_trims);
        let mut completed = 0;
        for (extent, owner_id) in pending {
            if self.extents.descriptor(extent).is_none() {
                // 已经被其它路径归还；归还点唯一，这里不重复处理。
                continue;
            }
            let occupancy = ExtentOccupancy {
                live_slots: 0,
                queued_slots: 0,
                pending_returns: 0,
            };
            match self.poll_trim_extent(extent, occupancy)? {
                Ok(bytes) => {
                    self.release_extent_bytes(owner_id, bytes)?;
                    completed += 1;
                }
                Err(TrimBlocked::GracePending { .. }) => {
                    self.pending_extent_trims.push((extent, owner_id));
                }
                Err(blocked) => {
                    return Err(RawInvariant::new(format!(
                        "待决 extent 归还被门禁拒绝：{}",
                        blocked.describe()
                    )));
                }
            }
        }
        Ok(completed)
    }

    /// 把一个已撤销物理页的 extent 从 owner 账本里扣减。
    fn release_extent_bytes(&mut self, owner_id: OwnerId, bytes: u64) -> Result<(), RawInvariant> {
        let accounting = self
            .directory
            .accounting_mut(owner_id)
            .ok_or_else(|| RawInvariant::new("extent 归还缺少 owner 账本"))?;
        accounting.release(bytes);
        Ok(())
    }
}
