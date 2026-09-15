//! TurnRegion 私有区的确定性参照实现：owner-local bump、export summary 门禁与状态机。
//!
//! 这一层是实现模型而不是硬件模型：它只维护 descriptor、bump 偏移、export summary 与状态，
//! 不分配真实地址。这样门禁的每一条判据都可以被单元测试穷举，同时 `world` 层把同一套判据
//! 与真实 owner 事实（resource lease、FFI 地址、pending transfer、live root）连起来。
//!
//! 状态机（`region_schema` 的 `REGION_STATE_NAMES` 顺序即强度）：
//!
//! ```text
//! private --bump--> private --publish--> publishing --+--> reset-pending --> reset
//!                                                    +--> local-promote
//!                                                    +--> region-transfer --> received
//! ```
//!
//! `received` 是接收者的起点：接收者像 `private` 一样发布并结束它。

use std::collections::VecDeque;

use super::message::{IntegrityTag, RegionTransferBatch};
use super::region_schema::{REGION_EXPORT_ALL, REGION_STATE_NAMES, TurnRegionRuntimeContract};
use super::slab::{OwnerToken, RawInvariant};

/// region 的稳定编号；只在所属 owner 的 registry 内稠密。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RegionId(pub(crate) u32);

impl RegionId {
    /// 返回编号原值。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    /// 返回作为下标的编号。
    const fn index(self) -> usize {
        self.0 as usize
    }
}

/// region 生命周期状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegionState {
    /// 刚建立，只允许 bump。
    Private,
    /// 已登记 export summary，等待结束动作。
    Publishing,
    /// 门禁通过，正在结算 bump 空间。
    ResetPending,
    /// 已回收，descriptor 仍保留 generation。
    Reset,
    /// summary 未闭合，整区保留为 stable storage。
    LocalPromote,
    /// 所有权已交给目标 owner，等待接收方采纳。
    RegionTransfer,
    /// 接收方已采纳；接收方像 `private` 一样使用。
    Received,
}

impl RegionState {
    /// 返回登记名。
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Private => REGION_STATE_NAMES[0],
            Self::Publishing => REGION_STATE_NAMES[1],
            Self::ResetPending => REGION_STATE_NAMES[2],
            Self::Reset => REGION_STATE_NAMES[3],
            Self::LocalPromote => REGION_STATE_NAMES[4],
            Self::RegionTransfer => REGION_STATE_NAMES[5],
            Self::Received => REGION_STATE_NAMES[6],
        }
    }

    /// 该状态是否允许继续 bump。
    const fn bumpable(self) -> bool {
        matches!(self, Self::Private | Self::Received)
    }

    /// 该状态是否允许登记 export summary。
    const fn publishable(self) -> bool {
        matches!(self, Self::Private | Self::Received)
    }
}

/// 一个 region 的 descriptor。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegionDescriptor {
    /// 所属 owner。
    pub(crate) owner: OwnerToken,
    /// 单调 generation；回收后旧引用必须被拒绝。
    pub(crate) generation: u32,
    /// 容量 class 在阶梯中的下标。
    pub(crate) capacity_class: u32,
    /// 容量 self 字节数。
    pub(crate) capacity_bytes: u32,
    /// 已经 bump 掉的字节数。
    pub(crate) used_bytes: u32,
    /// 已经 bump 的对象数。
    pub(crate) objects: u32,
    /// 编译器声明的 export summary 位。
    pub(crate) declared: u8,
    /// runtime 观察到的 export summary 位。
    pub(crate) observed: u8,
    /// 在途 transfer lease 数；非零时禁止 reset。
    pub(crate) transfer_lease: u32,
    /// 生命周期状态。
    pub(crate) state: RegionState,
}

impl RegionDescriptor {
    /// 返回 export summary 的并集（声明 ∪ 观察）。
    pub(crate) const fn export(&self) -> u8 {
        self.declared | self.observed
    }

    /// summary 是否闭合；闭合是 reset 的必要条件。
    pub(crate) const fn summary_closed(&self) -> bool {
        self.export() & REGION_EXPORT_ALL == 0
    }
}

/// reset 被拒绝的具体原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResetRefusal {
    /// 状态不是 `publishing`。
    State(RegionState),
    /// 仍有在途 transfer lease。
    TransferLease(u32),
    /// export summary 未闭合；携带并集。
    Summary(u8),
}

impl ResetRefusal {
    /// 返回拒绝原因名，用于计数与报告。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::State(_) => "state",
            Self::TransferLease(_) => "transfer-lease",
            Self::Summary(_) => "summary",
        }
    }
}

/// reset 的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResetOutcome {
    /// 整区回收；携带回收的字节与对象数。
    Reset { bytes: u32, objects: u32 },
    /// 门禁拒绝；调用方应当转而 promote。
    Refused(ResetRefusal),
}

/// promote 的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromoteReason {
    /// summary 未闭合。
    Summary,
    /// reset 被拒绝。
    Refused(ResetRefusal),
}

/// 一个 owner 的 region 计数器；全部单调递增。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RegionCounters {
    /// 建立过的 region 总数。
    pub(crate) opened: u64,
    /// 成功 reset 的 region 数。
    pub(crate) resets: u64,
    /// promote 的 region 数。
    pub(crate) promotions: u64,
    /// 发出的 transfer 数。
    pub(crate) transfers: u64,
    /// 采纳的 transfer 数。
    pub(crate) received: u64,
    /// 按原因拒绝的 reset 数。
    pub(crate) refusals: [u64; 3],
    /// reset 回收的字节数。
    pub(crate) reset_bytes: u64,
    /// promote 保留的字节数。
    pub(crate) promoted_bytes: u64,
    /// 在途 transfer 字节数。
    pub(crate) transfer_bytes: u64,
}

impl RegionCounters {
    /// 记录一次拒绝。
    fn record_refusal(&mut self, refusal: ResetRefusal) {
        let index = match refusal {
            ResetRefusal::State(_) => 0,
            ResetRefusal::TransferLease(_) => 1,
            ResetRefusal::Summary(_) => 2,
        };
        self.refusals[index] += 1;
    }
}

/// 一个 owner 的 region registry。
///
/// descriptor 存在稠密 `Vec` 里（编号是 owner 内自增的 `u32`，点查就是下标），空闲编号走
/// free list 复用；`generation` 每次都前进，因此旧编号不会被误认为新 region。容量阶梯只在
/// registry 建立时拷贝一次，热路径上只做位运算与整数比较。
#[derive(Debug)]
pub(crate) struct RegionRegistry {
    owner: OwnerToken,
    capacity_class_bytes: Vec<u32>,
    object_limit: u32,
    max_active: u32,
    descriptors: Vec<Option<RegionDescriptor>>,
    free: Vec<u32>,
    active: u32,
    generation: u32,
    counters: RegionCounters,
}

impl RegionRegistry {
    /// 按契约建立 owner 的 registry。
    pub(crate) fn new(owner: OwnerToken, contract: &TurnRegionRuntimeContract) -> Self {
        Self {
            owner,
            capacity_class_bytes: contract.capacity_class_bytes().to_vec(),
            object_limit: contract.object_limit(),
            max_active: contract.max_active_regions(),
            descriptors: Vec::new(),
            free: Vec::new(),
            active: 0,
            generation: 0,
            counters: RegionCounters::default(),
        }
    }

    /// 返回 owner 身份。
    pub(crate) const fn owner(&self) -> OwnerToken {
        self.owner
    }

    /// 返回当前活跃（未结束）的 region 数。
    pub(crate) const fn active(&self) -> u32 {
        self.active
    }

    /// 返回计数器。
    pub(crate) const fn counters(&self) -> &RegionCounters {
        &self.counters
    }

    /// 返回 descriptor 槽位数（含已回收槽位）。
    pub(crate) const fn slots(&self) -> u32 {
        self.descriptors.len() as u32
    }

    /// 返回 descriptor。
    pub(crate) fn descriptor(&self, region: RegionId) -> Result<RegionDescriptor, RawInvariant> {
        self.descriptors
            .get(region.index())
            .copied()
            .flatten()
            .ok_or_else(|| RawInvariant::new("region 编号未登记"))
    }

    /// 返回在途 transfer 的字节数。
    pub(crate) const fn transfer_bytes(&self) -> u64 {
        self.counters.transfer_bytes
    }

    /// 建立一个新的私有 region，容量取能覆盖 `bytes` 的最小 class。
    pub(crate) fn open(&mut self, bytes: u64) -> Result<RegionId, RawInvariant> {
        let capacity_class = self
            .capacity_class_bytes
            .iter()
            .position(|class| u64::from(*class) >= bytes)
            .ok_or_else(|| RawInvariant::new("region 容量超过登记阶梯上界"))?;
        if bytes == 0 {
            return Err(RawInvariant::new("region 不能为空容量"));
        }
        if self.active >= self.max_active {
            return Err(RawInvariant::new("owner 活跃 region 数超过上界"));
        }
        let capacity_bytes = self.capacity_class_bytes[capacity_class];
        let generation = self.next_generation();
        let descriptor = RegionDescriptor {
            owner: self.owner,
            generation,
            capacity_class: u32::try_from(capacity_class).expect("class 下标适配 u32"),
            capacity_bytes,
            used_bytes: 0,
            objects: 0,
            declared: 0,
            observed: 0,
            transfer_lease: 0,
            state: RegionState::Private,
        };
        let id = match self.free.pop() {
            Some(index) => {
                self.descriptors[index as usize] = Some(descriptor);
                RegionId(index)
            }
            None => {
                let index = u32::try_from(self.descriptors.len()).expect("region 数量适配 u32");
                self.descriptors.push(Some(descriptor));
                RegionId(index)
            }
        };
        self.active += 1;
        self.counters.opened += 1;
        Ok(id)
    }

    /// 在一个 region 上 bump 出一个对象，返回对象偏移。
    ///
    /// 对象数上界与容量上界都要检查：容量上界保证一个 region 的 payload 不超过单个 managed
    /// block，对象数上界保证 descriptor 层面的计数不会退化成无界扫描。
    pub(crate) fn bump(
        &mut self,
        region: RegionId,
        bytes: u32,
        objects: u32,
    ) -> Result<u32, RawInvariant> {
        let object_limit = self.object_limit;
        let descriptor = self.descriptor_mut(region)?;
        if !descriptor.state.bumpable() {
            return Err(RawInvariant::new("region 状态不允许继续分配"));
        }
        if objects == 0 || bytes == 0 {
            return Err(RawInvariant::new("region 分配必须同时给出字节与对象"));
        }
        let used = descriptor
            .used_bytes
            .checked_add(bytes)
            .ok_or_else(|| RawInvariant::new("region bump 字节溢出"))?;
        let count = descriptor
            .objects
            .checked_add(objects)
            .ok_or_else(|| RawInvariant::new("region 对象计数溢出"))?;
        if used > descriptor.capacity_bytes {
            return Err(RawInvariant::new("region bump 超过容量 class"));
        }
        if count > object_limit {
            return Err(RawInvariant::new("region 对象数超过上界"));
        }
        let offset = descriptor.used_bytes;
        descriptor.used_bytes = used;
        descriptor.objects = count;
        Ok(offset)
    }

    /// 登记 export summary 并把 region 推进到 `publishing`。
    pub(crate) fn publish(&mut self, region: RegionId, export: u8) -> Result<(), RawInvariant> {
        if export & !REGION_EXPORT_ALL != 0 {
            return Err(RawInvariant::new("region export summary 含未登记位"));
        }
        let descriptor = self.descriptor_mut(region)?;
        if !descriptor.state.publishable() {
            return Err(RawInvariant::new("region 状态不允许重复发布"));
        }
        descriptor.declared = export;
        descriptor.observed = 0;
        descriptor.state = RegionState::Publishing;
        Ok(())
    }

    /// 记录一个 runtime 观察到的 export summary 位。
    pub(crate) fn observe(&mut self, region: RegionId, bit: u8) -> Result<(), RawInvariant> {
        if bit & !REGION_EXPORT_ALL != 0 || bit == 0 || bit.count_ones() != 1 {
            return Err(RawInvariant::new("region 观察位必须是单个已登记位"));
        }
        let descriptor = self.descriptor_mut(region)?;
        if descriptor.state != RegionState::Publishing {
            return Err(RawInvariant::new("只有已发布的 region 才能记录观察位"));
        }
        descriptor.observed |= bit;
        Ok(())
    }

    /// 清除一个 runtime 观察位；事实消失后 region 可以重新回到可重置状态。
    pub(crate) fn clear_observation(
        &mut self,
        region: RegionId,
        bit: u8,
    ) -> Result<(), RawInvariant> {
        let descriptor = self.descriptor_mut(region)?;
        if descriptor.state != RegionState::Publishing {
            return Err(RawInvariant::new("只有已发布的 region 才能清除观察位"));
        }
        descriptor.observed &= !bit;
        Ok(())
    }

    /// 尝试按五条判据重置 region；任一条不满足都返回拒绝原因。
    ///
    /// 判据：状态是 `publishing`、没有在途 transfer lease、summary 闭合（无外部 alias、
    /// 无 resource lease、无 FFI 地址、无 pending transfer、无 live root）。
    pub(crate) fn reset(&mut self, region: RegionId) -> Result<ResetOutcome, RawInvariant> {
        let descriptor = self.descriptor_mut(region)?;
        if descriptor.state != RegionState::Publishing {
            let refusal = ResetRefusal::State(descriptor.state);
            self.counters.record_refusal(refusal);
            return Ok(ResetOutcome::Refused(refusal));
        }
        if descriptor.transfer_lease != 0 {
            let refusal = ResetRefusal::TransferLease(descriptor.transfer_lease);
            self.counters.record_refusal(refusal);
            return Ok(ResetOutcome::Refused(refusal));
        }
        if !descriptor.summary_closed() {
            let refusal = ResetRefusal::Summary(descriptor.export());
            self.counters.record_refusal(refusal);
            return Ok(ResetOutcome::Refused(refusal));
        }
        descriptor.state = RegionState::ResetPending;
        let bytes = descriptor.used_bytes;
        let objects = descriptor.objects;
        descriptor.used_bytes = 0;
        descriptor.objects = 0;
        descriptor.state = RegionState::Reset;
        self.active -= 1;
        self.counters.resets += 1;
        self.counters.reset_bytes += u64::from(bytes);
        Ok(ResetOutcome::Reset { bytes, objects })
    }

    /// 整区保留为 stable storage；地址稳定，字节继续计入 owner。
    pub(crate) fn promote(
        &mut self,
        region: RegionId,
        reason: PromoteReason,
    ) -> Result<u32, RawInvariant> {
        let descriptor = self.descriptor_mut(region)?;
        if descriptor.state != RegionState::Publishing {
            return Err(RawInvariant::new("只有已发布的 region 才能保留"));
        }
        descriptor.state = RegionState::LocalPromote;
        let bytes = descriptor.used_bytes;
        self.active -= 1;
        self.counters.promotions += 1;
        self.counters.promoted_bytes += u64::from(bytes);
        let _ = reason;
        Ok(bytes)
    }

    /// 生成一条 `RegionTransfer` 消息；region 进入 `region-transfer` 并占用 transfer lease。
    pub(crate) fn transfer(
        &mut self,
        region: RegionId,
        target: OwnerToken,
        type_summary: u32,
        cycle_epoch: u64,
        secret: &[u8; 32],
    ) -> Result<RegionTransferBatch, RawInvariant> {
        if target == self.owner {
            return Err(RawInvariant::new("region 不能移交给同一个 owner"));
        }
        let descriptor = self.descriptor_mut(region)?;
        if descriptor.state != RegionState::Publishing {
            return Err(RawInvariant::new("只有已发布的 region 才能移交"));
        }
        descriptor.transfer_lease += 1;
        descriptor.state = RegionState::RegionTransfer;
        let bytes = descriptor.used_bytes;
        let generation = descriptor.generation;
        let export_state = descriptor.declared;
        let observed = descriptor.observed;
        let capacity_class = descriptor.capacity_class;
        self.counters.transfers += 1;
        self.counters.transfer_bytes += u64::from(bytes);
        self.active -= 1;
        let mut batch = RegionTransferBatch {
            next: None,
            target,
            source: self.owner.owner_id,
            region: region.raw(),
            region_generation: generation,
            type_summary: super::slab::SlabDescriptorId::from_raw(type_summary),
            bytes,
            capacity_class,
            export_state,
            observed,
            cycle_epoch,
            state: super::message::MessageState::Staged,
            integrity: IntegrityTag {
                generation: super::slab::SlabGeneration::from_raw(u64::from(generation)),
                class: super::size_class::RuntimeSizeClassId::from_raw(0),
                owner_id: target.owner_id,
                route_key: target.route_key,
                checksum: 0,
            },
        };
        batch.integrity.checksum = IntegrityTag::compute_region_transfer(secret, &batch);
        Ok(batch)
    }

    /// 采纳一条 `RegionTransfer` 消息。
    ///
    /// 目标 owner、integrity、generation 与状态都要匹配；采纳后编号在接收者 registry 内重新
    /// 稠密分配，因此发送者的编号不会泄漏到接收者的地址空间。
    pub(crate) fn receive(
        &mut self,
        batch: &RegionTransferBatch,
        secret: &[u8; 32],
    ) -> Result<RegionId, RawInvariant> {
        if batch.target != self.owner {
            return Err(RawInvariant::new("region transfer 投递到错误 owner"));
        }
        if batch.integrity.checksum != IntegrityTag::compute_region_transfer(secret, batch) {
            return Err(RawInvariant::new("region transfer integrity 校验失败"));
        }
        if self.active >= self.max_active {
            return Err(RawInvariant::new("接收方活跃 region 数超过上界"));
        }
        let capacity_class = batch.capacity_class as usize;
        let capacity_bytes = *self
            .capacity_class_bytes
            .get(capacity_class)
            .ok_or_else(|| RawInvariant::new("region transfer 容量 class 越界"))?;
        if capacity_bytes < batch.bytes {
            return Err(RawInvariant::new("region transfer 容量 class 小于 payload"));
        }
        let generation = self.next_generation();
        let descriptor = RegionDescriptor {
            owner: self.owner,
            generation,
            capacity_class: batch.capacity_class,
            capacity_bytes,
            used_bytes: batch.bytes,
            objects: 0,
            declared: batch.export_state,
            observed: batch.observed,
            transfer_lease: 0,
            state: RegionState::Received,
        };
        let id = match self.free.pop() {
            Some(index) => {
                self.descriptors[index as usize] = Some(descriptor);
                RegionId(index)
            }
            None => {
                let index = u32::try_from(self.descriptors.len()).expect("region 数量适配 u32");
                self.descriptors.push(Some(descriptor));
                RegionId(index)
            }
        };
        self.active += 1;
        self.counters.received += 1;
        Ok(id)
    }

    /// 发送方确认接收方已采纳；释放 lease 与在途字节。
    pub(crate) fn confirm(&mut self, region: RegionId) -> Result<(), RawInvariant> {
        let descriptor = self.descriptor_mut(region)?;
        if descriptor.state != RegionState::RegionTransfer || descriptor.transfer_lease == 0 {
            return Err(RawInvariant::new("region 没有在途 transfer"));
        }
        descriptor.transfer_lease -= 1;
        let bytes = descriptor.used_bytes;
        self.counters.transfer_bytes -= u64::from(bytes);
        Ok(())
    }

    /// 把一个 `received` region 直接结束为 `reset`：接收方不需要再次 publish。
    pub(crate) fn receive_reset(&mut self, region: RegionId) -> Result<ResetOutcome, RawInvariant> {
        let descriptor = self.descriptor_mut(region)?;
        if descriptor.state != RegionState::Received {
            return Err(RawInvariant::new("只有已采纳的 region 才能直接回收"));
        }
        let bytes = descriptor.used_bytes;
        let objects = descriptor.objects;
        descriptor.state = RegionState::Reset;
        descriptor.used_bytes = 0;
        descriptor.objects = 0;
        self.active -= 1;
        self.counters.resets += 1;
        self.counters.reset_bytes += u64::from(bytes);
        Ok(ResetOutcome::Reset { bytes, objects })
    }

    /// 发放下一个 generation。
    ///
    /// generation 与编号解耦：编号会被 free list 复用，而 generation 每次发放都前进，因此
    /// 旧编号 + 旧 generation 的引用一定会被拒绝。
    fn next_generation(&mut self) -> u32 {
        let generation = self.generation;
        self.generation = self.generation.wrapping_add(1);
        generation
    }

    fn descriptor_mut(&mut self, region: RegionId) -> Result<&mut RegionDescriptor, RawInvariant> {
        self.descriptors
            .get_mut(region.index())
            .and_then(Option::as_mut)
            .ok_or_else(|| RawInvariant::new("region 编号未登记"))
    }
}

/// 全部 owner 的 region registry 与在途 transfer 队列。
#[derive(Debug)]
pub(crate) struct RegionPlane {
    registries: Vec<RegionRegistry>,
    transfers: VecDeque<RegionTransferBatch>,
}

impl RegionPlane {
    /// 按契约与 owner 表建立 plane。
    pub(crate) fn new(owners: &[OwnerToken], contract: &TurnRegionRuntimeContract) -> Self {
        Self {
            registries: owners
                .iter()
                .map(|owner| RegionRegistry::new(*owner, contract))
                .collect(),
            transfers: VecDeque::new(),
        }
    }

    /// 返回 owner 的 registry。
    pub(crate) fn registry(&self, owner: u32) -> &RegionRegistry {
        &self.registries[owner as usize]
    }

    /// 返回 owner 的 registry（可变）。
    pub(crate) fn registry_mut(&mut self, owner: u32) -> &mut RegionRegistry {
        &mut self.registries[owner as usize]
    }

    /// 返回 owner 的活跃 region 数。
    pub(crate) fn active(&self, owner: u32) -> u32 {
        self.registries[owner as usize].active()
    }

    /// 返回在途 transfer 消息数。
    pub(crate) fn pending(&self) -> usize {
        self.transfers.len()
    }

    /// 返回最早排队的在途消息。
    pub(crate) fn front(&self) -> Option<RegionTransferBatch> {
        self.transfers.front().copied()
    }

    /// 返回全部在途 transfer 的字节数；credit 观测把它算进 pending return。
    pub(crate) fn pending_bytes(&self) -> u64 {
        self.registries
            .iter()
            .map(RegionRegistry::transfer_bytes)
            .fold(0_u64, u64::saturating_add)
    }

    /// 把一条 transfer 消息排入投递队列。
    pub(crate) fn enqueue(&mut self, batch: RegionTransferBatch) -> Result<(), RawInvariant> {
        if self.transfers.iter().any(|pending| {
            pending.target == batch.target
                && pending.region == batch.region
                && pending.region_generation == batch.region_generation
        }) {
            return Err(RawInvariant::new("同一个 region 不能有两条在途 transfer"));
        }
        self.transfers.push_back(batch);
        Ok(())
    }

    /// 取出下一条发给指定 owner 的 transfer 消息。
    pub(crate) fn take(&mut self, owner: OwnerToken) -> Option<RegionTransferBatch> {
        let index = self
            .transfers
            .iter()
            .position(|batch| batch.target == owner)?;
        self.transfers.remove(index)
    }
}
