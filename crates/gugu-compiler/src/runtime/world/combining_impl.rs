//! typed combining 与 topology/range 慢路径的世界接线。
//!
//! 把 `CombiningPlane` 的冷操作记录接到真实世界事实上：
//!
//! - direct 模式（默认）在请求者上下文逐条执行同一份 handler，不创建任何记录；
//! - combined 模式把同一条请求记录进非移动池，无争用走单原子 fast path，争用时挂到
//!   owner 的 combiner 链上，由后续 `drain_combining` 按轮次合并执行；
//! - 四条真实动作分别是平台 trim 的页撤销、extent coalescing 的 buddy 合、topology
//!   目录重建与 operation 记录池的 `GlobalRange` refill；
//! - 平台 wait/wake 由 `PlatformRange` 的 wait 字承担：挂链记录在请求者上下文注册等待，
//!   本轮发布 response 的记录数就是需要唤醒的等待者上界。
//!
//! handler 是封闭枚举上的方法，不捕获任何用户环境，也不调用 `wait`/park，因此 combiner
//! 永远不会执行用户 closure、drop glue 或跨 safepoint 持锁。

use super::super::combining::{
    CombiningLimits, CombiningPlane, CombiningStats, MergeGroup, OpTicket,
    Operation as PlaneOperation, OperationOutcome, OperationRequest as PlaneRequest, OperationTag,
    PublishOutcome, RoundReport,
};
use super::super::combining_schema::{CombiningMode, CombiningRuntimeContract};
use super::super::extent::ExtentId;
use super::super::model::RawModelError;
use super::super::owner::RawOwner;
use super::super::provider::{RangeProvider, WaitOutcome, WaitWordId};
use super::super::slab::{Epoch, MemoryDomainId, OwnerDirectory, OwnerToken, RawInvariant};
use super::RawWorld;

/// 一次 `drain_combining` 允许的最大轮次数；正常深度远小于该界。
///
/// 这个上界只是「不变量失败之前先把已经认领的工作做完」的固定迭代数：每轮至少认领
/// 一条记录，因此超过它说明池或预算进入了不可推进的状态，必须按不变量失败。
const COMBINING_ROUND_LIMIT: u32 = 64;

/// 一条冷路径请求的规范载荷；与 `combining_schema` 的 tag 目录一一对应。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationRequest {
    /// 记录池自身补充 chunk；`chunks` 是本次请求补充的 chunk 数。
    GlobalRangeRefill { chunks: u32, bytes: u64 },
    /// extent coalescing：把已撤销物理页的 extent 合回 buddy 阶梯。
    ExtentCoalesce {
        extent: ExtentId,
        class: u32,
        owner_index: u32,
        bytes: u64,
    },
    /// topology 目录重建。
    TopologyRebuild { epoch: Epoch },
    /// 平台页 trim：撤销一个 extent 的物理页。
    PlatformTrim {
        extent: ExtentId,
        class: u32,
        owner_index: u32,
        bytes: u64,
    },
}

impl OperationRequest {
    /// 记录池补充请求；`bytes` 是本次请求占用的轮次预算。
    pub(crate) const fn global_range_refill(chunks: u32, bytes: u64) -> Self {
        Self::GlobalRangeRefill { chunks, bytes }
    }

    /// extent coalescing 请求。
    pub(crate) const fn extent_coalesce(
        extent: ExtentId,
        class: u32,
        owner_index: u32,
        bytes: u64,
    ) -> Self {
        Self::ExtentCoalesce {
            extent,
            class,
            owner_index,
            bytes,
        }
    }

    /// topology 目录重建请求。
    pub(crate) const fn topology_rebuild(epoch: Epoch) -> Self {
        Self::TopologyRebuild { epoch }
    }

    /// 平台页 trim 请求。
    pub(crate) const fn platform_trim(
        extent: ExtentId,
        class: u32,
        owner_index: u32,
        bytes: u64,
    ) -> Self {
        Self::PlatformTrim {
            extent,
            class,
            owner_index,
            bytes,
        }
    }

    /// 返回冷操作 tag。
    pub(crate) const fn tag(&self) -> OperationTag {
        match self {
            Self::GlobalRangeRefill { .. } => OperationTag::GlobalRangeRefill,
            Self::ExtentCoalesce { .. } => OperationTag::ExtentCoalesce,
            Self::TopologyRebuild { .. } => OperationTag::TopologyRebuild,
            Self::PlatformTrim { .. } => OperationTag::PlatformTrim,
        }
    }

    /// 返回同类合并键。
    ///
    /// trim 按 class 合并（同一个 class 的多次页撤销进同一次执行），coalesce 按 owner
    /// arena 合并（buddy 阶梯的插入只在同一 arena 内相邻），refill 与 topology 各自
    /// 全局唯一，因此按自身参数合并。
    pub(crate) const fn merge_key(&self) -> u64 {
        // `From` 不是 const trait，这里的 u32 → u64 是无损加宽，因此直接用 `as`。
        match self {
            Self::GlobalRangeRefill { .. } => 0,
            Self::ExtentCoalesce { owner_index, .. } => *owner_index as u64,
            Self::TopologyRebuild { epoch } => epoch.raw() as u64,
            Self::PlatformTrim { class, .. } => *class as u64,
        }
    }

    /// 返回 stable descriptor id。
    pub(crate) const fn descriptor(&self) -> u64 {
        match self {
            Self::GlobalRangeRefill { chunks, .. } => *chunks as u64,
            Self::ExtentCoalesce { extent, .. } | Self::PlatformTrim { extent, .. } => {
                extent.raw() as u64
            }
            Self::TopologyRebuild { epoch } => epoch.raw() as u64,
        }
    }

    /// 返回标量参数。
    pub(crate) const fn scalar(&self) -> u64 {
        match self {
            Self::GlobalRangeRefill { .. } | Self::TopologyRebuild { .. } => 0,
            Self::ExtentCoalesce { class, .. } | Self::PlatformTrim { class, .. } => *class as u64,
        }
    }

    /// 返回请求字节数。
    pub(crate) const fn bytes(&self) -> u64 {
        match self {
            Self::GlobalRangeRefill { bytes, .. }
            | Self::ExtentCoalesce { bytes, .. }
            | Self::PlatformTrim { bytes, .. } => *bytes,
            Self::TopologyRebuild { .. } => 0,
        }
    }

    /// 返回执行所需的操作视图；direct 模式与 fast path 都用它。
    pub(crate) const fn operation(&self) -> PlaneOperation {
        PlaneOperation {
            tag: self.tag(),
            descriptor: self.descriptor(),
            scalar: self.scalar(),
            bytes: self.bytes(),
            merge_key: self.merge_key(),
        }
    }

    /// 转换为平面请求。
    fn plane_request(&self, slot: u32) -> PlaneRequest {
        PlaneRequest {
            tag: self.tag(),
            owner: slot,
            merge_key: self.merge_key(),
            descriptor: self.descriptor(),
            scalar: self.scalar(),
            bytes: self.bytes(),
        }
    }

    /// 返回本请求所属的 combiner owner token。
    ///
    /// extent 类的两条操作取 extent 所属 arena 的 owner token：arena 在 `open_arena` 时
    /// 就登记了它归属的 owner，managed arena 也由 raw owner 持有，因此 trim 与 buddy 合并
    /// 都落回同一个 owner 的 combiner 队列。refill 与 topology 是 domain owner 的全局操作。
    fn owner_token(&self, world: &RawWorld) -> Result<OwnerToken, RawInvariant> {
        match *self {
            Self::GlobalRangeRefill { .. } | Self::TopologyRebuild { .. } => Ok(world.domain_owner),
            Self::ExtentCoalesce { extent, .. } | Self::PlatformTrim { extent, .. } => world
                .extents
                .descriptor(extent)
                .map(|descriptor| descriptor.owner)
                .ok_or_else(|| RawInvariant::new("冷操作引用未知 extent")),
        }
    }
}

/// 一次 `drain_combining` 的累计结果；只进统计与确定性测试的观察面。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CombiningDrainReport {
    pub(crate) rounds: u32,
    pub(crate) executions: u32,
    pub(crate) items: u32,
    pub(crate) bytes: u64,
    pub(crate) merged: u32,
    pub(crate) cancellations: u32,
    pub(crate) timeouts: u32,
    pub(crate) woken: u32,
}

impl CombiningDrainReport {
    /// 累加一轮的结果。
    fn accumulate(&mut self, report: RoundReport) {
        self.rounds += 1;
        self.executions = self.executions.saturating_add(report.executions);
        self.items = self.items.saturating_add(report.items);
        self.bytes = self.bytes.saturating_add(report.bytes);
        self.merged = self.merged.saturating_add(report.merged);
        self.cancellations = self.cancellations.saturating_add(report.cancellations);
        self.timeouts = self.timeouts.saturating_add(report.timeouts);
        self.woken = self.woken.saturating_add(report.woken);
    }
}

/// topology 目录的稠密索引。
///
/// 旧实现每次查 owner token 都线性扫描 `owners` 与 `resource_owners`；冷路径上的
/// extent 归还、combiner 槽位解析与 trim 门禁都要做这件事，因此这里按 `owner_id`
/// 建两张稠密表：`raw[owner_id]` 是 raw owner 列表下标，`resource[owner_id]` 是
/// resource owner 列表下标，`u32::MAX` 表示该 domain 下没有这个 owner。
///
/// 表只在 `rebuild` 里整表重填，且只在 topology epoch 前进时重建；查询是两次数组
/// 索引，没有任何扫描。
#[derive(Debug, Default)]
pub(crate) struct TopologyIndex {
    epoch: Epoch,
    raw: Vec<u32>,
    resource: Vec<u32>,
}

impl TopologyIndex {
    /// 创建空索引；`RawWorld::new` 在 owner 登记完成后立即重建一次。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 按当前 owner 目录重建索引。
    pub(crate) fn rebuild(
        &mut self,
        directory: &OwnerDirectory,
        owners: &[RawOwner],
        resource_owners: &[RawOwner],
    ) {
        let count = directory.owners().len();
        self.raw = vec![u32::MAX; count];
        self.resource = vec![u32::MAX; count];
        for (index, owner) in owners.iter().enumerate() {
            let slot = u32::try_from(index).expect("owner 下标适配 u32");
            if let Some(entry) = usize::try_from(owner.token().owner_id.raw())
                .ok()
                .and_then(|id| self.raw.get_mut(id))
            {
                *entry = slot;
            }
        }
        for (index, owner) in resource_owners.iter().enumerate() {
            let slot = u32::try_from(index).expect("owner 下标适配 u32");
            if let Some(entry) = usize::try_from(owner.token().owner_id.raw())
                .ok()
                .and_then(|id| self.resource.get_mut(id))
            {
                *entry = slot;
            }
        }
        self.epoch = directory.epoch();
    }

    /// 返回 token 在所属 owner 列表里的下标；未登记或 domain 不符时返回 `None`。
    pub(crate) fn slot(&self, token: &OwnerToken) -> Option<u32> {
        let table = match token.domain {
            MemoryDomainId::RESOURCE => &self.resource,
            _ => &self.raw,
        };
        let index = usize::try_from(token.owner_id.raw()).ok()?;
        let slot = *table.get(index)?;
        (slot != u32::MAX).then_some(slot)
    }

    /// 返回索引建立时的 topology epoch。
    pub(crate) const fn epoch(&self) -> Epoch {
        self.epoch
    }
}

impl RawWorld {
    /// 按契约配置 combining 平面；要求世界没有在飞 operation record。
    ///
    /// 配置后立即重建 topology 索引：combined 模式下这次重建本身就走一条 typed cold
    /// operation，因此「索引可用」与「平面可用」是同一个事实。
    pub(crate) fn configure_combining(
        &mut self,
        contract: &CombiningRuntimeContract,
    ) -> Result<(), RawModelError> {
        if self.combining.pending_records() != 0 {
            return Err(RawModelError::new(
                "combining 平面配置前不得存在在飞 operation record",
            ));
        }
        let queue_count = self.combiner_queue_count();
        self.combining = CombiningPlane::new(
            contract.mode(),
            queue_count,
            CombiningLimits::from_contract(contract),
        );
        // 每个 combiner 槽位一个平台 wait 字：挂链记录在上面等待，轮次结束时按发布
        // response 的记录数唤醒。direct 模式不会用到它们，但契约仍要求槽位齐备。
        self.combining_words = (0..queue_count)
            .map(|_| self.provider.register_wait_word(0))
            .collect();
        self.request_topology_rebuild()?;
        Ok(())
    }

    /// 返回 combining 平面统计快照。
    pub(crate) fn combining_stats(&self) -> CombiningStats {
        self.combining.stats()
    }

    /// 返回某个 tag 的累计冷操作请求数。
    pub(crate) fn combining_tag_requests(&self, tag: OperationTag) -> u64 {
        self.combining.tag_requests(tag)
    }

    /// 返回在飞 operation record 数。
    pub(crate) fn combining_pending_records(&self) -> u32 {
        self.combining.pending_records()
    }

    /// 返回在飞 operation record 携带的请求字节数。
    pub(crate) fn combining_pending_bytes(&self) -> u64 {
        self.combining.pending_bytes()
    }

    /// 返回记录池的 chunk 数。
    pub(crate) fn combining_pool_chunks(&self) -> u32 {
        self.combining.chunk_count()
    }

    /// 返回当前 combining 模式。
    pub(crate) fn combining_mode(&self) -> CombiningMode {
        self.combining.mode()
    }

    /// 返回 topology 索引建立时的 epoch。
    pub(crate) fn combining_topology_epoch(&self) -> Epoch {
        self.topology.epoch()
    }

    /// 判断当前是否有 combiner 正在执行一轮。
    pub(crate) fn combining_round_open(&self) -> bool {
        self.combining.round_open()
    }

    /// 返回 combiner 槽位总数：raw owner、resource owner 各一个，外加 domain owner 的
    /// GlobalRange/topology 队列。
    pub(super) fn combiner_queue_count(&self) -> u32 {
        u32::try_from(self.owners.len() + self.resource_owners.len() + 1)
            .expect("combiner 槽位数适配 u32")
    }

    /// 把一个 owner token 映射到 combiner 槽位。
    ///
    /// 与 inbox 槽位的区别是 resource owner 不与 raw owner 共用编号：combiner 槽位是
    /// 「每个 owner token 一条队列」，domain owner 独占最后一个槽位。登记校验、错误文本
    /// 与 inbox 槽位共用同一份实现，因此伪造 token 不可能命中同 owner 的队列。
    fn combiner_slot(&self, token: &OwnerToken) -> Result<u32, RawInvariant> {
        if *token == self.domain_owner {
            return Ok(self.combiner_queue_count() - 1);
        }
        let index = u32::try_from(self.owner_slot(token)?)
            .map_err(|_| RawInvariant::new("owner 槽位编号越过 u32"))?;
        if token.domain == MemoryDomainId::RESOURCE {
            let raw_count = u32::try_from(self.owners.len()).expect("owner 数量适配 u32");
            return Ok(raw_count + index);
        }
        Ok(index)
    }

    /// 提交一条冷操作；池满时先补充记录池，再由平面按模式分流。
    pub(super) fn request_combining(
        &mut self,
        request: OperationRequest,
    ) -> Result<OperationOutcome, RawInvariant> {
        self.ensure_operation_pool()?;
        self.submit_combining(request)
    }

    /// 批量提交冷操作。
    ///
    /// combined 模式先把全部请求入链（同类请求因此在同一轮里被合并），再按 combiner
    /// 槽位排空，最后按输入顺序读回 response；direct 模式没有记录池，批量语义退化为
    /// 逐条执行同一份 handler。
    pub(super) fn request_combining_batch(
        &mut self,
        requests: Vec<OperationRequest>,
    ) -> Result<Vec<OperationOutcome>, RawInvariant> {
        let mut outcomes = Vec::with_capacity(requests.len());
        if requests.is_empty() {
            return Ok(outcomes);
        }
        if self.combining.mode() == CombiningMode::Direct {
            for request in requests {
                outcomes.push(self.submit_combining(request)?);
            }
            return Ok(outcomes);
        }
        let mut tickets: Vec<(u32, OpTicket)> = Vec::with_capacity(requests.len());
        for request in requests {
            self.ensure_operation_pool()?;
            let slot = self.combiner_slot(&request.owner_token(self)?)?;
            let ticket = self.combining.enqueue(request.plane_request(slot))?;
            tickets.push((slot, ticket));
        }
        let mut drained: Vec<u32> = Vec::new();
        for (slot, _) in &tickets {
            if !drained.contains(slot) {
                drained.push(*slot);
                self.drain_combining(*slot)?;
            }
        }
        for (_, ticket) in tickets {
            let response = self.combining.response(ticket)?;
            outcomes.push(combining_outcome(response)?);
            self.combining.release(ticket)?;
        }
        Ok(outcomes)
    }

    /// 按平面分流执行一条已经通过池检查的请求。
    fn submit_combining(
        &mut self,
        request: OperationRequest,
    ) -> Result<OperationOutcome, RawInvariant> {
        let slot = self.combiner_slot(&request.owner_token(self)?)?;
        match self.combining.publish(request.plane_request(slot))? {
            PublishOutcome::Direct => self.execute_combining_operation(request.operation()),
            PublishOutcome::FastPath(ticket) => {
                let operation = self.combining.operation(ticket)?;
                let outcome = self.execute_combining_operation(operation)?;
                self.combining.complete(ticket, outcome)?;
                // 请求者读完 response 之后回收槽位：fast path 的 ticket 由本次请求独占。
                self.combining.release(ticket)?;
                Ok(outcome)
            }
            PublishOutcome::Parked(ticket) => {
                // 争用路径：先在平台 wait 字上登记等待，再由本上下文推进 combiner 队列。
                self.park_combining_request(slot)?;
                self.drain_combining(slot)?;
                let response = self.combining.response(ticket)?;
                let outcome = combining_outcome(response);
                // 已经完成的记录由本次请求回收槽位（超时同样算完成）；已取消的记录仍挂在
                // 链上，必须留给下一轮从链上摘除，未发布的记录保持原状等待后续 drain。
                if self.combining.is_completed(ticket)? {
                    self.combining.release(ticket)?;
                }
                outcome
            }
        }
    }

    /// 记录池满时补充一个 chunk。
    ///
    /// refill 自身也走 `submit_combining`：它是唯一允许消费 chunk 尾部预留槽位的 tag，
    /// 因此「普通槽位用尽」不会让补充记录池这条唯一的出路也被卡住。这里刻意不经过
    /// `request_combining`，否则池满会递归触发第二次补充检查。
    fn ensure_operation_pool(&mut self) -> Result<(), RawInvariant> {
        if !self.combining.needs_refill() {
            return Ok(());
        }
        let record_bytes = u64::from(self.combining.record_bytes());
        let outcome =
            self.submit_combining(OperationRequest::global_range_refill(1, record_bytes))?;
        if outcome.applied_bytes().is_none() {
            return Err(RawInvariant::new("global-range-refill 未执行"));
        }
        Ok(())
    }

    /// 在平台 wait 字上登记一次等待；期望值被平台改动时重新读取一次。
    fn park_combining_request(&mut self, slot: u32) -> Result<(), RawInvariant> {
        let word = self.combining_wait_word(slot)?;
        let expected = self
            .provider
            .word_value(word)
            .ok_or_else(|| RawInvariant::new("combining 的 wait 字未登记"))?;
        match self.provider.wait(word, expected)? {
            WaitOutcome::Woken => Ok(()),
            WaitOutcome::Mismatch => {
                let expected = self
                    .provider
                    .word_value(word)
                    .ok_or_else(|| RawInvariant::new("combining 的 wait 字未登记"))?;
                match self.provider.wait(word, expected)? {
                    WaitOutcome::Woken => Ok(()),
                    WaitOutcome::Mismatch => {
                        Err(RawInvariant::new("combining park 的 wait 字期望值无法稳定"))
                    }
                }
            }
        }
    }

    /// 按本轮发布 response 的记录数唤醒等待者。
    fn wake_combining_waiters(&mut self, slot: u32, woken: u32) -> Result<(), RawInvariant> {
        if woken == 0 {
            return Ok(());
        }
        let word = self.combining_wait_word(slot)?;
        let released = self.provider.wake(word, woken)?;
        debug_assert!(released <= woken);
        Ok(())
    }

    /// 返回某个 combiner 槽位的平台 wait 字。
    fn combining_wait_word(&self, slot: u32) -> Result<WaitWordId, RawInvariant> {
        let index = usize::try_from(slot).expect("combiner 槽位适配 usize");
        self.combining_words
            .get(index)
            .copied()
            .ok_or_else(|| RawInvariant::new("combining 槽位没有登记的平台 wait 字"))
    }

    /// 反复开轮直到某个 combiner 的等待记录清空；每轮有固定上界。
    pub(super) fn drain_combining(
        &mut self,
        slot: u32,
    ) -> Result<CombiningDrainReport, RawInvariant> {
        let mut total = CombiningDrainReport::default();
        for _ in 0..COMBINING_ROUND_LIMIT {
            // 空队列与已打开的轮次都不开轮：owner service 上的这次检查是 O(1) 字段读取，
            // 因此热路径不会产生空轮次，也不会污染统计。
            if !self.combining.queue_open(slot) {
                return Ok(total);
            }
            let groups = self.combining.begin_round(slot)?;
            let mut executions: Vec<(MergeGroup, Vec<OperationOutcome>)> =
                Vec::with_capacity(groups.len());
            for group in groups {
                let operations = self.combining.group_operations(&group)?;
                let mut outcomes = Vec::with_capacity(operations.len());
                for operation in operations {
                    outcomes.push(self.execute_combining_operation(operation)?);
                }
                executions.push((group, outcomes));
            }
            let report = self.combining.end_round(slot, &executions)?;
            self.wake_combining_waiters(slot, report.woken)?;
            total.accumulate(report);
        }
        Err(RawInvariant::new("combining 排空超过固定轮次上界"))
    }

    /// 推进某个 owner 的两条 combiner 队列：raw 与 resource 各一条。
    pub(super) fn drain_owner_combining(&mut self, owner: u32) -> Result<(), RawInvariant> {
        self.drain_combining(owner)?;
        let resource = owner
            .checked_add(u32::try_from(self.owners.len()).expect("owner 数量适配 u32"))
            .ok_or_else(|| RawInvariant::new("combiner 槽位编号溢出"))?;
        self.drain_combining(resource)?;
        Ok(())
    }

    /// 冷操作的唯一执行入口；handler 是封闭枚举上的方法，不捕获用户环境。
    fn execute_combining_operation(
        &mut self,
        operation: PlaneOperation,
    ) -> Result<OperationOutcome, RawInvariant> {
        match operation.tag {
            OperationTag::GlobalRangeRefill => self.apply_global_range_refill(operation),
            OperationTag::ExtentCoalesce => self.apply_extent_coalesce(operation),
            OperationTag::TopologyRebuild => self.apply_topology_rebuild(operation),
            OperationTag::PlatformTrim => self.apply_platform_trim(operation),
        }
    }

    /// 平台 trim：撤销一个 extent 的物理页。
    fn apply_platform_trim(
        &mut self,
        operation: PlaneOperation,
    ) -> Result<OperationOutcome, RawInvariant> {
        let extent = plane_extent(operation.descriptor, "platform-trim")?;
        let descriptor = *self
            .extents
            .descriptor(extent)
            .ok_or_else(|| RawInvariant::new("platform-trim 引用未知 extent"))?;
        if descriptor.bytes != operation.bytes || u64::from(descriptor.class) != operation.scalar {
            return Err(RawInvariant::new(
                "platform-trim 记录的 extent 参数与描述符不一致",
            ));
        }
        let range = self
            .extents
            .arena_range_of(extent)
            .ok_or_else(|| RawInvariant::new("platform-trim 的 extent 缺少所属 arena"))?;
        let offset = self.extents.provider_offset_of(&descriptor);
        self.provider
            .decommit_pages(range, offset, descriptor.bytes)?;
        Ok(OperationOutcome::Applied {
            bytes: descriptor.bytes,
        })
    }

    /// extent coalescing：把 extent 合回 buddy 阶梯。
    fn apply_extent_coalesce(
        &mut self,
        operation: PlaneOperation,
    ) -> Result<OperationOutcome, RawInvariant> {
        let extent = plane_extent(operation.descriptor, "extent-coalesce")?;
        self.extents.give_back(extent)?;
        Ok(OperationOutcome::Applied {
            bytes: operation.bytes,
        })
    }

    /// 记录池补充：每一个 chunk 都是整块 push 的非移动 metadata。
    fn apply_global_range_refill(
        &mut self,
        operation: PlaneOperation,
    ) -> Result<OperationOutcome, RawInvariant> {
        let requested = u32::try_from(operation.descriptor)
            .map_err(|_| RawInvariant::new("global-range-refill 的 chunk 数越过 u32"))?
            .max(1);
        let mut added = 0_u64;
        for _ in 0..requested {
            let slots = self.combining.grow_operation_pool()?;
            added = added.saturating_add(u64::from(slots));
        }
        Ok(OperationOutcome::Applied {
            bytes: added.saturating_mul(u64::from(self.combining.record_bytes())),
        })
    }

    /// topology 目录重建：把新的 owner 集合与 epoch 固化进稠密索引。
    fn apply_topology_rebuild(
        &mut self,
        operation: PlaneOperation,
    ) -> Result<OperationOutcome, RawInvariant> {
        let epoch = Epoch::from_raw(
            u32::try_from(operation.descriptor)
                .map_err(|_| RawInvariant::new("topology-rebuild 的 epoch 越过 u32"))?,
        );
        self.rebuild_topology(epoch)?;
        Ok(OperationOutcome::Applied { bytes: 0 })
    }

    /// 按目录当前 epoch 重建 topology 索引。
    fn rebuild_topology(&mut self, epoch: Epoch) -> Result<(), RawInvariant> {
        if epoch != self.directory.epoch() {
            return Err(RawInvariant::new(
                "topology-rebuild 记录的 epoch 与目录当前 epoch 不一致",
            ));
        }
        self.topology
            .rebuild(&self.directory, &self.owners, &self.resource_owners);
        Ok(())
    }

    /// 冷路径上的目录重建请求：direct 模式直接重建，combined 模式走 typed cold operation。
    pub(super) fn request_topology_rebuild(&mut self) -> Result<(), RawInvariant> {
        let epoch = self.directory.epoch();
        match self.combining.mode() {
            CombiningMode::Direct => self.rebuild_topology(epoch),
            CombiningMode::Combined => self
                .request_combining(OperationRequest::topology_rebuild(epoch))
                .map(|_| ()),
        }
    }

    /// `RawWorld::new` 使用的无平面版本：契约还不可用时索引也必须可查。
    pub(crate) fn rebuild_topology_inline(&mut self) {
        self.topology
            .rebuild(&self.directory, &self.owners, &self.resource_owners);
    }

    /// 在空队列上真开一轮，后续请求会因 claim 字被占而挂链；只供争用交错测试。
    ///
    /// 该槽位已有的等待记录会先被排空：否则它们既不会被执行，也无法被 `end_round`
    /// 结清，测试会拿到与场景无关的不变量失败。
    #[cfg(test)]
    pub(crate) fn begin_combining_round_for_test(&mut self, slot: u32) -> Result<(), RawInvariant> {
        if self.combining.round_open() {
            return Err(RawInvariant::new("combiner 轮次已经打开"));
        }
        self.drain_combining(slot)?;
        self.combining.begin_round(slot).map(|_| ())
    }

    /// 结束测试用的一轮空轮次。
    #[cfg(test)]
    pub(crate) fn end_combining_round_for_test(
        &mut self,
        slot: u32,
    ) -> Result<RoundReport, RawInvariant> {
        self.combining.end_round(slot, &[])
    }

    /// 排空某个 combiner 队列；只供确定性测试与 bench 驱动。
    #[cfg(test)]
    pub(crate) fn drain_combining_for_test(
        &mut self,
        slot: u32,
    ) -> Result<CombiningDrainReport, RawInvariant> {
        self.drain_combining(slot)
    }

    /// 取消某个 combiner 队列头部的等待记录；只供确定性测试构造取消交错。
    #[cfg(test)]
    pub(crate) fn cancel_parked_combining_for_test(
        &mut self,
        slot: u32,
    ) -> Result<OperationOutcome, RawInvariant> {
        self.combining
            .cancel_parked(slot)
            .map(|(_, outcome)| outcome)
    }

    /// 返回 owner token 的 combiner 槽位；只供确定性测试与 bench 驱动。
    #[cfg(test)]
    pub(crate) fn combining_slot_for_test(&self, token: &OwnerToken) -> Result<u32, RawInvariant> {
        self.combiner_slot(token)
    }

    /// 返回只读的 combining 平面；只供确定性测试观察内部状态。
    #[cfg(test)]
    pub(crate) const fn combining_ref(&self) -> &CombiningPlane {
        &self.combining
    }

    /// 返回可变的 combining 平面；只供确定性测试构造内部状态。
    #[cfg(test)]
    pub(crate) fn combining_mut(&mut self) -> &mut CombiningPlane {
        &mut self.combining
    }
}

/// 把操作记录里的 descriptor 还原成 extent 编号。
fn plane_extent(descriptor: u64, label: &str) -> Result<ExtentId, RawInvariant> {
    u32::try_from(descriptor)
        .map(ExtentId::from_raw)
        .map_err(|_| RawInvariant::new(format!("{label} 记录的 extent 编号越过 u32")))
}

/// 把已经读取的 response 翻译成执行结局。
///
/// 这是 parked 路径唯一的结束判定：没有 response 说明 combiner 没有在固定轮次内结清
/// 记录，`cancelled`/`timed-out` 说明范围操作没有执行——两者都不能被静默跳过。
pub(super) fn combining_outcome(
    response: Option<OperationOutcome>,
) -> Result<OperationOutcome, RawInvariant> {
    match response {
        Some(applied @ OperationOutcome::Applied { .. }) => Ok(applied),
        Some(OperationOutcome::Cancelled | OperationOutcome::TimedOut) => {
            Err(RawInvariant::new("combining 请求未被执行"))
        }
        None => Err(RawInvariant::new("combiner 未在固定轮次内发布 response")),
    }
}

/// `RawWorld::new` 使用的默认平面：direct 模式不创建任何记录。
pub(crate) fn default_plane(queue_count: u32) -> CombiningPlane {
    CombiningPlane::new(
        CombiningMode::Direct,
        queue_count,
        CombiningLimits::from_contract_default(),
    )
}
