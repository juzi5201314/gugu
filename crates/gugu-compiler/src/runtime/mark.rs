//! MarkMailbox、owner credit 与 cycle 终止检测的确定性参照实现。
//!
//! 本模块是 `mark_schema` 契约的运行时对偶：契约固定目录、绑定与不变量，这里执行固定算法
//! ——每 owner 单 consumer 的 `MarkMailbox`、跨 owner 的 credit acquire/consume/return 状态机、
//! root snapshot gate 的六类参与者确认、以及「七个收敛条件全为 0 才允许 remark」的终止判定。
//! 所有计数都是整数，不读宿主时钟、不创建线程。
//!
//! 三个归属边界：
//!
//! 1. credit 只统计当前 cycle 尚未归还的在飞占用；「mailbox 为空」不是完成条件，只有七个条件
//!    同时为 0 才能宣布 cycle 终止。
//! 2. ticket 的 cycle/topology 身份在发布时固定，消费时逐项校验；过期 ticket 与自投递都必须
//!    进入不变量失败，而不是被静默丢弃。
//! 3. root snapshot gate 未收齐全部参与者确认前不得进入 mark 阶段；gate 只能开一次、关一次。

use super::barrier_schema::MessageFamilyTag;
use super::mark_schema::{
    GcCreditId, MARK_CONVERGENCE_CONDITIONS, MARK_CREDIT_INITIAL_GENERATION,
    MARK_MAILBOX_CONSUMERS, MARK_SNAPSHOT_PARTICIPANTS, MarkRuntimeContract,
};

/// 一个 credit slot 的生命周期状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreditState {
    /// 已 acquire，尚未被目标 owner consume。
    InFlight,
    /// 已被目标 owner consume，等待归还。
    Done,
    /// 已归还；slot 回到 free list，下一次 acquire 推进 generation。
    Returned,
}

/// 一个 credit slot 的占用者身份与状态。
///
/// 身份字段在 acquire 时固定、consume 时逐项校验：family、来源/目标 owner、cycle 与 topology
/// 都参与校验，因此一个 slot 不可能被另一族或另一 cycle 的消息误用。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CreditSlot {
    generation: u32,
    source: u32,
    target: u32,
    family: MessageFamilyTag,
    cycle: u64,
    topology: u32,
    state: CreditState,
}

/// 一个 owner 的分类计数。
///
/// slot 的占用者身份、generation 与 cycle/topology 都由池本身持有，账本只保留在飞、Done 与
/// returned 三个计数，避免同一份状态在两处各写一遍。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct OwnerCredit {
    in_flight: u64,
    done: u64,
    returned: u64,
    issued: u64,
}

impl OwnerCredit {
    /// 返回已 acquire 且尚未 consume 的 credit 数。
    pub(crate) const fn pending(&self) -> u64 {
        self.in_flight
    }

    /// 返回已 consume 但尚未归还的 credit 数。
    pub(crate) const fn done(&self) -> u64 {
        self.done
    }

    /// 返回已归还的 credit 累计数。
    pub(crate) const fn returned(&self) -> u64 {
        self.returned
    }

    /// 返回已 acquire 的 credit 累计数。
    pub(crate) const fn issued(&self) -> u64 {
        self.issued
    }
}

/// 共享、可复用的 credit slot pool。
///
/// `capacity` 是**同时在飞**的上界：任何在飞的 mark ticket 或 edge delta 都占一个 non-moving
/// node，因此「node 容量加根槽数」是可证明的授权额度。归还的 slot 进 free list 并推进
/// generation，因此一次 cycle 内的累计 issue 次数不受该上界限制，而重放旧 id 必然失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CreditPool {
    capacity: u64,
    slots: Vec<CreditSlot>,
    free: Vec<u32>,
    in_flight: u64,
    done: u64,
    returned: u64,
    owners: Vec<OwnerCredit>,
}

impl CreditPool {
    /// 按授权上界与 owner 数创建空池。
    pub(crate) fn new(capacity: u64, owners: u32) -> Self {
        Self {
            capacity,
            slots: Vec::new(),
            free: Vec::new(),
            in_flight: 0,
            done: 0,
            returned: 0,
            owners: vec![OwnerCredit::default(); owners as usize],
        }
    }

    /// 返回同时在飞的授权上界。
    pub(crate) const fn capacity(&self) -> u64 {
        self.capacity
    }

    /// 返回尚未 acquire 的在飞额度。
    pub(crate) fn available(&self) -> u64 {
        self.capacity.saturating_sub(self.in_flight + self.done)
    }

    /// 返回一个 owner 的分类计数。
    pub(crate) fn owner(&self, owner: u32) -> Result<OwnerCredit, MarkError> {
        self.owners
            .get(owner as usize)
            .copied()
            .ok_or(MarkError::UnknownOwner { owner })
    }

    /// 返回全部 owner 尚未归还的 credit 数。
    pub(crate) const fn pending(&self) -> u64 {
        self.in_flight + self.done
    }

    /// 返回已 consume 但尚未归还的 credit 数。
    pub(crate) const fn done(&self) -> u64 {
        self.done
    }

    /// 返回已归还的 credit 累计数。
    pub(crate) const fn returned(&self) -> u64 {
        self.returned
    }

    /// 返回已创建的 slot 数；它只随真实并发需求增长。
    pub(crate) fn slot_count(&self) -> u64 {
        self.slots.len() as u64
    }

    /// 为一个跨 owner 工作记录 acquire 一个 credit。
    ///
    /// 身份字段在成功返回前就已固定，且 acquire 之前完成全部可失败判定：额度耗尽不会留下
    /// 半初始化的 slot。
    pub(crate) fn acquire_work(
        &mut self,
        source: u32,
        target: u32,
        family: MessageFamilyTag,
        cycle: u64,
        topology: u32,
    ) -> Result<GcCreditId, MarkError> {
        if self.owners.get(source as usize).is_none() {
            return Err(MarkError::UnknownOwner { owner: source });
        }
        if self.owners.get(target as usize).is_none() {
            return Err(MarkError::UnknownOwner { owner: target });
        }
        if self.pending() >= self.capacity {
            return Err(MarkError::PoolExhausted {
                owner: source,
                granted: self.capacity,
            });
        }
        let index = match self.free.pop() {
            Some(index) => {
                let slot = &mut self.slots[index as usize];
                // 复用推进 generation：旧 id 即使再次命中同一 slot 也必然被拒绝。
                slot.generation =
                    slot.generation
                        .checked_add(1)
                        .ok_or(MarkError::PoolExhausted {
                            owner: source,
                            granted: self.capacity,
                        })?;
                index
            }
            None => {
                let index =
                    u32::try_from(self.slots.len()).map_err(|_| MarkError::PoolExhausted {
                        owner: source,
                        granted: self.capacity,
                    })?;
                self.slots.push(CreditSlot {
                    generation: MARK_CREDIT_INITIAL_GENERATION,
                    source,
                    target,
                    family,
                    cycle,
                    topology,
                    state: CreditState::InFlight,
                });
                index
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.source = source;
        slot.target = target;
        slot.family = family;
        slot.cycle = cycle;
        slot.topology = topology;
        slot.state = CreditState::InFlight;
        let generation = slot.generation;
        self.owners[source as usize].in_flight += 1;
        self.owners[source as usize].issued += 1;
        self.in_flight += 1;
        Ok(GcCreditId::new(index, generation))
    }

    /// 校验一个 credit 可以被目标 owner 消费，不改变状态。
    ///
    /// 乱序记录在应用之前必须完成同样的身份校验，但它的 credit 不能被 consume：consume 是
    /// 「已应用」的线性化点。校验失败不改变任何状态，因此错误族或过期记录不会污染新 slot。
    pub(crate) fn validate_consume(
        &self,
        id: GcCreditId,
        family: MessageFamilyTag,
        owner: u32,
        cycle: u64,
        topology: u32,
    ) -> Result<(), MarkError> {
        if self.owners.get(owner as usize).is_none() {
            return Err(MarkError::UnknownOwner { owner });
        }
        let slot = self.slot(id)?;
        if slot.state != CreditState::InFlight {
            return Err(MarkError::CreditNotInFlight {
                owner: slot.source,
                credit: id,
            });
        }
        if slot.family != family {
            return Err(MarkError::CreditFamilyMismatch {
                credit: id,
                expected: slot.family.name(),
            });
        }
        if slot.target != owner {
            return Err(MarkError::CreditTargetMismatch {
                credit: id,
                target: slot.target,
            });
        }
        if slot.cycle != cycle {
            return Err(MarkError::StaleCycle {
                ticket: cycle,
                current: slot.cycle,
            });
        }
        if slot.topology != topology {
            return Err(MarkError::StaleTopology {
                ticket: topology,
                current: slot.topology,
            });
        }
        Ok(())
    }

    /// 校验一个 credit 仍在本 cycle 的在飞集合里，且属于给定族；转发路径使用。
    ///
    /// 转发是源 owner 侧的操作，因此不检查目标 owner，只检查 generation、状态、族与 epoch。
    pub(crate) fn validate_in_flight(
        &self,
        id: GcCreditId,
        family: MessageFamilyTag,
        cycle: u64,
        topology: u32,
    ) -> Result<(), MarkError> {
        let slot = self.slot(id)?;
        if slot.state != CreditState::InFlight {
            return Err(MarkError::CreditNotInFlight {
                owner: slot.source,
                credit: id,
            });
        }
        if slot.family != family {
            return Err(MarkError::CreditFamilyMismatch {
                credit: id,
                expected: slot.family.name(),
            });
        }
        if slot.cycle != cycle {
            return Err(MarkError::StaleCycle {
                ticket: cycle,
                current: slot.cycle,
            });
        }
        if slot.topology != topology {
            return Err(MarkError::StaleTopology {
                ticket: topology,
                current: slot.topology,
            });
        }
        Ok(())
    }

    /// 把一条在飞 credit 的目标 owner 改成新的目标；转发路径使用。
    ///
    /// 源 owner 与 generation 不变：只有最终目标能 consume，而归还仍然记在源 owner 账本上。
    pub(crate) fn retarget(&mut self, id: GcCreditId, target: u32) -> Result<(), MarkError> {
        if self.owners.get(target as usize).is_none() {
            return Err(MarkError::UnknownOwner { owner: target });
        }
        let slot = self.slot(id)?;
        if slot.state != CreditState::InFlight {
            return Err(MarkError::CreditNotInFlight {
                owner: slot.source,
                credit: id,
            });
        }
        self.slots[id.slot() as usize].target = target;
        Ok(())
    }

    /// 目标 owner 消费一个 credit：校验全部身份字段后才改变状态。
    pub(crate) fn consume_work(
        &mut self,
        owner: u32,
        id: GcCreditId,
        family: MessageFamilyTag,
        cycle: u64,
        topology: u32,
    ) -> Result<(), MarkError> {
        if self.owners.get(owner as usize).is_none() {
            return Err(MarkError::UnknownOwner { owner });
        }
        let slot = self.slot(id)?;
        if slot.state != CreditState::InFlight {
            return Err(MarkError::CreditNotInFlight {
                owner: slot.source,
                credit: id,
            });
        }
        if slot.family != family {
            return Err(MarkError::CreditFamilyMismatch {
                credit: id,
                expected: slot.family.name(),
            });
        }
        if slot.target != owner {
            return Err(MarkError::CreditTargetMismatch {
                credit: id,
                target: slot.target,
            });
        }
        if slot.cycle != cycle {
            return Err(MarkError::StaleCycle {
                ticket: cycle,
                current: slot.cycle,
            });
        }
        if slot.topology != topology {
            return Err(MarkError::StaleTopology {
                ticket: topology,
                current: slot.topology,
            });
        }
        let source = slot.source;
        let slot = &mut self.slots[id.slot() as usize];
        slot.state = CreditState::Done;
        self.owners[source as usize].in_flight -= 1;
        self.owners[source as usize].done += 1;
        self.in_flight -= 1;
        self.done += 1;
        Ok(())
    }

    /// 归还一个已 consume 的 credit：Done → Returned 并回到 free list。
    pub(crate) fn return_work(&mut self, id: GcCreditId) -> Result<(), MarkError> {
        let slot = self.slot(id)?;
        if slot.state != CreditState::Done {
            return Err(MarkError::CreditNotDone {
                owner: slot.source,
                credit: id,
            });
        }
        let source = slot.source;
        let slot = &mut self.slots[id.slot() as usize];
        slot.state = CreditState::Returned;
        self.owners[source as usize].done -= 1;
        self.owners[source as usize].returned += 1;
        self.done -= 1;
        self.returned += 1;
        self.free.push(id.slot());
        Ok(())
    }

    /// 归还一个 owner 全部已 consume 的 credit；返回归还数量。
    pub(crate) fn settle_owner(&mut self, owner: u32) -> Result<u64, MarkError> {
        if self.owners.get(owner as usize).is_none() {
            return Err(MarkError::UnknownOwner { owner });
        }
        let mut pending = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.state == CreditState::Done && slot.source == owner {
                pending.push(GcCreditId::new(
                    u32::try_from(index).expect("credit slot 下标适配 u32"),
                    slot.generation,
                ));
            }
        }
        let mut returned = 0_u64;
        for id in pending {
            self.return_work(id)?;
            returned += 1;
        }
        Ok(returned)
    }

    /// 取一个 slot 并校验 generation；generation 不匹配即旧 id 重放。
    fn slot(&self, id: GcCreditId) -> Result<CreditSlot, MarkError> {
        let slot = self
            .slots
            .get(id.slot() as usize)
            .ok_or(MarkError::CreditNotIssued {
                owner: 0,
                credit: id,
            })?;
        if !id.is_valid() || slot.generation != id.generation() {
            return Err(MarkError::CreditNotIssued {
                owner: slot.source,
                credit: id,
            });
        }
        Ok(*slot)
    }
}

/// 一个 owner 的 mark mailbox；每 owner 单 consumer。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MarkMailbox {
    owner: u32,
    cycle: u64,
    topology: u32,
    pending: u64,
    consumed: u64,
    forwarded: u64,
    last_credit: Option<GcCreditId>,
    consumer_slots: u32,
}

impl MarkMailbox {
    /// 创建一个 owner 的 mailbox。
    pub(crate) const fn new(owner: u32) -> Self {
        Self {
            owner,
            cycle: 0,
            topology: 0,
            pending: 0,
            consumed: 0,
            forwarded: 0,
            last_credit: None,
            consumer_slots: MARK_MAILBOX_CONSUMERS,
        }
    }

    /// 以新的 cycle/topology 开始一个 cycle；计数清零，consumer slot 保持不变。
    pub(crate) fn reset(&mut self, cycle: u64, topology: u32) {
        self.cycle = cycle;
        self.topology = topology;
        self.pending = 0;
        self.consumed = 0;
        self.forwarded = 0;
        self.last_credit = None;
    }

    /// 返回 mailbox 所属的 cycle epoch。
    pub(crate) const fn cycle(&self) -> u64 {
        self.cycle
    }

    /// 返回 mailbox 发布时的 topology epoch。
    pub(crate) const fn topology(&self) -> u32 {
        self.topology
    }

    /// 返回 consumer slot 数量。
    pub(crate) const fn consumer_slots(&self) -> u32 {
        self.consumer_slots
    }

    /// 发布一条 ticket。
    pub(crate) fn publish(&mut self, credit: GcCreditId) {
        self.pending += 1;
        self.last_credit = Some(credit);
    }

    /// 单 consumer 消费一条 ticket。
    pub(crate) fn consume(&mut self, credit: GcCreditId) -> Result<(), MarkError> {
        if self.pending == 0 {
            return Err(MarkError::MailboxEmpty { owner: self.owner });
        }
        self.pending -= 1;
        self.consumed += 1;
        self.last_credit = Some(credit);
        Ok(())
    }

    /// 转发一条 ticket：从本 owner 的 mailbox 移除。
    pub(crate) fn forward(&mut self, _credit: GcCreditId) -> Result<(), MarkError> {
        if self.pending == 0 {
            return Err(MarkError::MailboxEmpty { owner: self.owner });
        }
        self.pending -= 1;
        self.forwarded += 1;
        Ok(())
    }

    /// 返回尚未消费的 ticket 数。
    pub(crate) const fn pending(&self) -> u64 {
        self.pending
    }

    /// 返回已消费的 ticket 数。
    pub(crate) const fn consumed(&self) -> u64 {
        self.consumed
    }

    /// 返回已转发的 ticket 数。
    pub(crate) const fn forwarded(&self) -> u64 {
        self.forwarded
    }

    /// 返回最近一次消费或发布的 credit 身份。
    pub(crate) const fn last_credit(&self) -> Option<GcCreditId> {
        self.last_credit
    }
}

/// root snapshot gate 的参与者；顺序即契约 `snapshot_participants`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MarkParticipant {
    /// producer stop epoch 已发布。
    ProducerStopEpoch = 0,
    /// 远端 consumer 已在 snapshot 边界排空。
    RemoteConsumer = 1,
    /// 根槽按 owner 分片登记完成。
    RootSlice = 2,
    /// region registry 的 pending 已检查。
    RegionRegistry = 3,
    /// 全部 access guard 为 0 的登记完成。
    HandleAccessGuard = 4,
    /// 本地 worklist 已清空并登记边界。
    LocalWorklist = 5,
}

impl MarkParticipant {
    /// 全部参与者；顺序即确认顺序。
    pub(crate) const ALL: [Self; 6] = [
        Self::ProducerStopEpoch,
        Self::RemoteConsumer,
        Self::RootSlice,
        Self::RegionRegistry,
        Self::HandleAccessGuard,
        Self::LocalWorklist,
    ];

    /// 返回稠密编号。
    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    /// 返回登记名。
    pub(crate) const fn name(self) -> &'static str {
        MARK_SNAPSHOT_PARTICIPANTS[self.index()]
    }
}

/// root snapshot gate：收齐全部 owner 的六项确认后才允许进入 mark。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RootSnapshotGate {
    cycle: u64,
    topology: u32,
    owners: u32,
    /// 每个 owner 一个位掩码；位序即 `MarkParticipant::index`。
    confirmed: Vec<u64>,
    open: bool,
}

impl RootSnapshotGate {
    /// 打开一个 cycle 的 gate。
    pub(crate) fn open(cycle: u64, topology: u32, owners: u32) -> Self {
        Self {
            cycle,
            topology,
            owners,
            confirmed: vec![0; owners as usize],
            open: true,
        }
    }

    /// 返回 gate 所属的 cycle。
    pub(crate) const fn cycle(&self) -> u64 {
        self.cycle
    }

    /// 返回 gate 的 topology epoch。
    pub(crate) const fn topology(&self) -> u32 {
        self.topology
    }

    /// 返回 gate 是否仍然打开。
    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    /// 返回某个 owner 的某项确认是否已经登记。
    pub(crate) fn confirmed(&self, owner: u32, kind: MarkParticipant) -> bool {
        self.confirmed
            .get(owner as usize)
            .is_some_and(|mask| mask & (1_u64 << kind.index()) != 0)
    }

    /// 登记一个 owner 的一项确认；重复确认必须失败。
    pub(crate) fn confirm(&mut self, owner: u32, kind: MarkParticipant) -> Result<(), MarkError> {
        if !self.open {
            return Err(MarkError::SnapshotNotOpen);
        }
        let slot = self
            .confirmed
            .get_mut(owner as usize)
            .ok_or(MarkError::UnknownOwner { owner })?;
        let bit = 1_u64 << kind.index();
        if *slot & bit != 0 {
            return Err(MarkError::DuplicateConfirm {
                owner,
                kind: kind.name(),
            });
        }
        *slot |= bit;
        Ok(())
    }

    /// 全部 owner 的六项确认是否都已到达。
    pub(crate) fn ready(&self) -> bool {
        self.confirmed.iter().all(|mask| *mask == self.full_mask())
    }

    /// 返回尚未确认的 `(owner, participant)` 列表。
    pub(crate) fn missing(&self) -> Vec<(u32, &'static str)> {
        let mut missing = Vec::new();
        for (owner, mask) in self.confirmed.iter().enumerate() {
            for kind in MarkParticipant::ALL {
                if mask & (1_u64 << kind.index()) == 0 {
                    missing.push((
                        u32::try_from(owner).expect("owner 下标适配 u32"),
                        kind.name(),
                    ));
                }
            }
        }
        missing
    }

    /// 关闭 gate；此后的确认一律失败。
    pub(crate) fn close(&mut self) {
        self.open = false;
    }

    const fn full_mask(&self) -> u64 {
        (1_u64 << MarkParticipant::ALL.len()) - 1
    }
}

/// 终止判定所需的真实工作量观测；都由 world 从真实结构读取。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MarkObservations {
    /// 各 owner 本地 worklist 的深度之和。
    pub(crate) worklist_items: u64,
    /// 尚未落地的已发布 batch 数。
    pub(crate) published_batches: u64,
    /// barrier buffer 中尚未 flush 的 card 键数。
    pub(crate) barrier_buffer_keys: u64,
    /// 在途转发的 GC 工作消息数。
    pub(crate) forwarding_work: u64,
    /// 已确认 producer epoch 的 owner 数。
    pub(crate) producer_epoch_confirmed: u64,
    /// 应确认 producer epoch 的 owner 总数。
    pub(crate) producer_epoch_total: u64,
}

/// 收敛条件；顺序即契约 `conditions` 顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MarkCondition {
    LocalWorklist = 0,
    PublishedBatch = 1,
    Mailbox = 2,
    BarrierBuffer = 3,
    ProducerEpoch = 4,
    ForwardingWork = 5,
    PendingCredit = 6,
}

impl MarkCondition {
    /// 全部条件；顺序即判定顺序。
    pub(crate) const ALL: [Self; 7] = [
        Self::LocalWorklist,
        Self::PublishedBatch,
        Self::Mailbox,
        Self::BarrierBuffer,
        Self::ProducerEpoch,
        Self::ForwardingWork,
        Self::PendingCredit,
    ];

    /// 返回稠密编号。
    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    /// 返回登记名。
    pub(crate) const fn name(self) -> &'static str {
        MARK_CONVERGENCE_CONDITIONS[self.index()]
    }
}

/// 一次 cycle 的终止记录；七个条件全部为 0 才允许 remark。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MarkTermination {
    cycle: u64,
    topology: u32,
    conditions: [u64; 7],
}

impl MarkTermination {
    /// 返回 cycle epoch。
    pub(crate) const fn cycle(&self) -> u64 {
        self.cycle
    }

    /// 返回 topology epoch。
    pub(crate) const fn topology(&self) -> u32 {
        self.topology
    }

    /// 按条件取值。
    pub(crate) const fn get(&self, condition: MarkCondition) -> u64 {
        self.conditions[condition.index()]
    }

    /// 七个条件是否全部为 0。
    pub(crate) fn converged(&self) -> bool {
        self.conditions.iter().all(|value| *value == 0)
    }

    /// 返回仍非 0 的条件名列表。
    pub(crate) fn blocking(&self) -> Vec<&'static str> {
        MarkCondition::ALL
            .iter()
            .filter(|condition| self.get(**condition) != 0)
            .map(|condition| condition.name())
            .collect()
    }
}

/// mark cycle 的状态；顺序即契约 `cycle_states`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MarkCycleState {
    Idle = 0,
    Snapshot = 1,
    Marking = 2,
    Converging = 3,
    Remark = 4,
    Complete = 5,
}

impl MarkCycleState {
    /// 返回稠密编号。
    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    /// 返回登记名。
    pub(crate) const fn name(self) -> &'static str {
        super::mark_schema::MARK_CYCLE_STATES[self.index()]
    }
}

/// mark 平面的累计统计。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MarkStats {
    /// 已完成的 cycle 数。
    pub(crate) cycles: u64,
    /// 已打开的 snapshot 数。
    pub(crate) snapshots: u64,
    /// 已发布的 ticket 数。
    pub(crate) tickets_published: u64,
    /// 已消费的 ticket 数。
    pub(crate) tickets_consumed: u64,
    /// 已转发的 ticket 数。
    pub(crate) tickets_forwarded: u64,
    /// 已归还的 credit 数。
    pub(crate) credits_returned: u64,
    /// 已发布的 edge delta 数。
    pub(crate) edge_deltas_published: u64,
    /// 已应用的 edge delta 数。
    pub(crate) edge_deltas_consumed: u64,
    /// cycle 内累计标记的对象数。
    pub(crate) marks: u64,
    /// cycle 内累计确认的 snapshot 参与者数。
    pub(crate) snapshot_participants: u64,
}

/// mark 平面的不变量失败分类。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MarkError {
    /// owner 编号越过配置的 owner 数量或 credit id 位宽。
    UnknownOwner { owner: u32 },
    /// 一个 owner 的 credit 池已经耗尽。
    PoolExhausted { owner: u32, granted: u64 },
    /// 引用了从未 acquire 的 credit，或重放了 generation 已过期的旧 id。
    CreditNotIssued { owner: u32, credit: GcCreditId },
    /// credit 不在 InFlight 状态；重复 ticket 落在这里。
    CreditNotInFlight { owner: u32, credit: GcCreditId },
    /// credit 不在 Done 状态，不能归还。
    CreditNotDone { owner: u32, credit: GcCreditId },
    /// credit 的持有族与当前消息族不符。
    CreditFamilyMismatch {
        credit: GcCreditId,
        expected: &'static str,
    },
    /// credit 的目标 owner 与当前消费者不符。
    CreditTargetMismatch { credit: GcCreditId, target: u32 },
    /// 进入新 cycle 时仍有 credit 未归还。
    CycleOutstanding { pending: u64 },
    /// ticket 的 cycle 与当前 cycle 不符。
    StaleCycle { ticket: u64, current: u64 },
    /// ticket 的 topology 与当前 topology 不符。
    StaleTopology { ticket: u32, current: u32 },
    /// ticket 的目标 owner 就是源 owner。
    SelfTicket { owner: u32 },
    /// mailbox 为空时仍尝试消费或转发。
    MailboxEmpty { owner: u32 },
    /// 同一参与者重复确认。
    DuplicateConfirm { owner: u32, kind: &'static str },
    /// gate 未打开时的确认或快照操作。
    SnapshotNotOpen,
    /// snapshot 未收齐全部参与者确认。
    SnapshotIncomplete { missing: Vec<(u32, &'static str)> },
    /// cycle 状态机不接受当前迁移。
    CycleState {
        expected: &'static str,
        actual: &'static str,
    },
}

impl std::fmt::Display for MarkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOwner { owner } => write!(formatter, "mark owner {owner} 未登记"),
            Self::PoolExhausted { owner, granted } => {
                write!(
                    formatter,
                    "owner {owner} 的 credit 池已耗尽（授权 {granted}）"
                )
            }
            Self::CreditNotIssued { owner, credit } => {
                write!(
                    formatter,
                    "owner {owner} 引用了未签发的 credit {:#x}",
                    credit.raw()
                )
            }
            Self::CreditNotInFlight { owner, credit } => {
                write!(
                    formatter,
                    "owner {owner} 的 credit {:#x} 不在 InFlight 状态（重复 ticket）",
                    credit.raw()
                )
            }
            Self::CreditNotDone { owner, credit } => {
                write!(
                    formatter,
                    "owner {owner} 的 credit {:#x} 不在 Done 状态，不能归还",
                    credit.raw()
                )
            }
            Self::CreditFamilyMismatch { credit, expected } => {
                write!(
                    formatter,
                    "credit {:#x} 的持有族与 {expected} 不符",
                    credit.raw()
                )
            }
            Self::CreditTargetMismatch { credit, target } => {
                write!(
                    formatter,
                    "credit {:#x} 的目标 owner 不是 {target}",
                    credit.raw()
                )
            }
            Self::CycleOutstanding { pending } => {
                write!(formatter, "进入新 cycle 前仍有 {pending} 个 credit 未归还")
            }
            Self::StaleCycle { ticket, current } => {
                write!(
                    formatter,
                    "mark ticket 的 cycle 已过期：ticket {ticket}，当前 {current}"
                )
            }
            Self::StaleTopology { ticket, current } => {
                write!(
                    formatter,
                    "mark ticket 的 topology 已过期：ticket {ticket}，当前 {current}"
                )
            }
            Self::SelfTicket { owner } => {
                write!(
                    formatter,
                    "owner {owner} 不能向自己的 mailbox 发布 mark ticket"
                )
            }
            Self::MailboxEmpty { owner } => write!(formatter, "owner {owner} 的 MarkMailbox 为空"),
            Self::DuplicateConfirm { owner, kind } => {
                write!(
                    formatter,
                    "owner {owner} 的 snapshot 参与者 {kind} 重复确认"
                )
            }
            Self::SnapshotNotOpen => formatter.write_str("root snapshot gate 尚未打开"),
            Self::SnapshotIncomplete { missing } => write!(
                formatter,
                "root snapshot gate 未收齐全部参与者确认：缺 {} 项",
                missing.len()
            ),
            Self::CycleState { expected, actual } => {
                write!(
                    formatter,
                    "mark cycle 状态非法：期望 {expected}，实际 {actual}"
                )
            }
        }
    }
}

/// mark 平面：共享 credit 池、mailbox、snapshot gate 与终止判定。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MarkPlane {
    contract: MarkRuntimeContract,
    cycle: u64,
    topology: u32,
    state: MarkCycleState,
    credits: CreditPool,
    mailboxes: Vec<MarkMailbox>,
    /// 已转发但尚未被最终目标 consume 的 credit 身份；`forwarding-work` 的真实在飞量。
    forwarded_in_flight: Vec<GcCreditId>,
    gate: Option<RootSnapshotGate>,
    stats: MarkStats,
    last: Option<MarkTermination>,
}

impl MarkPlane {
    /// 按契约创建平面；credit 池按「同时在飞」的授权上界一次配置，不随每轮 issue 增长。
    pub(crate) fn new(contract: &MarkRuntimeContract, owners: u32) -> Result<Self, MarkError> {
        if owners == 0 {
            return Err(MarkError::UnknownOwner { owner: owners });
        }
        let credits = CreditPool::new(contract.credit_pool(), owners);
        let mailboxes = (0..owners).map(MarkMailbox::new).collect();
        Ok(Self {
            contract: contract.clone(),
            cycle: 0,
            topology: 0,
            state: MarkCycleState::Idle,
            credits,
            mailboxes,
            forwarded_in_flight: Vec::new(),
            gate: None,
            stats: MarkStats::default(),
            last: None,
        })
    }

    /// 返回契约。
    pub(crate) const fn contract(&self) -> &MarkRuntimeContract {
        &self.contract
    }

    /// 返回当前 cycle 状态。
    pub(crate) const fn state(&self) -> MarkCycleState {
        self.state
    }

    /// 返回当前 cycle epoch。
    pub(crate) const fn cycle(&self) -> u64 {
        self.cycle
    }

    /// 返回当前 topology epoch。
    pub(crate) const fn topology(&self) -> u32 {
        self.topology
    }

    /// 返回累计统计。
    pub(crate) const fn stats(&self) -> MarkStats {
        self.stats
    }

    /// 返回最近一次终止记录。
    pub(crate) const fn last(&self) -> Option<MarkTermination> {
        self.last
    }

    /// 返回 owner 数量。
    pub(crate) fn owner_count(&self) -> u32 {
        u32::try_from(self.mailboxes.len()).expect("owner 数适配 u32")
    }

    /// 返回共享 credit 池。
    pub(crate) const fn credits(&self) -> &CreditPool {
        &self.credits
    }

    /// 返回共享 credit 池的可变引用；世界级发布与结算路径使用。
    pub(crate) fn credits_mut(&mut self) -> &mut CreditPool {
        &mut self.credits
    }

    /// 返回一个 owner 的分类计数。
    pub(crate) fn credit(&self, owner: u32) -> Result<OwnerCredit, MarkError> {
        self.credits.owner(owner)
    }

    /// 返回一个 owner 的 mailbox。
    pub(crate) fn mailbox(&self, owner: u32) -> Result<&MarkMailbox, MarkError> {
        self.mailboxes
            .get(owner as usize)
            .ok_or(MarkError::UnknownOwner { owner })
    }

    /// 开始一个 cycle：固定 cycle/topology、打开 gate。
    ///
    /// credit 池的授权在构造时固定，因此这里只要求上一轮已经全部归还；池不因 cycle 推进而
    /// 增长，也不清空已经归还的 slot。
    pub(crate) fn begin_cycle(&mut self, cycle: u64, topology: u32) -> Result<(), MarkError> {
        if !matches!(self.state, MarkCycleState::Idle | MarkCycleState::Complete) {
            return Err(MarkError::CycleState {
                expected: MarkCycleState::Idle.name(),
                actual: self.state.name(),
            });
        }
        let pending = self.mark_credit_pending();
        if pending != 0 {
            return Err(MarkError::CycleOutstanding { pending });
        }
        self.cycle = cycle;
        self.topology = topology;
        for mailbox in &mut self.mailboxes {
            mailbox.reset(cycle, topology);
        }
        self.gate = Some(RootSnapshotGate::open(cycle, topology, self.owner_count()));
        self.forwarded_in_flight.clear();
        self.last = None;
        self.state = MarkCycleState::Snapshot;
        self.stats.snapshots += 1;
        Ok(())
    }

    /// 返回某个 owner 的某项 snapshot 确认是否已经登记。
    pub(crate) fn snapshot_confirmed(&self, owner: u32, kind: MarkParticipant) -> bool {
        self.gate
            .as_ref()
            .is_some_and(|gate| gate.confirmed(owner, kind))
    }

    /// 登记一个 owner 的一项 snapshot 确认。
    pub(crate) fn confirm_snapshot(
        &mut self,
        owner: u32,
        kind: MarkParticipant,
    ) -> Result<(), MarkError> {
        let gate = self.gate.as_mut().ok_or(MarkError::SnapshotNotOpen)?;
        gate.confirm(owner, kind)?;
        self.stats.snapshot_participants += 1;
        Ok(())
    }

    /// root snapshot 是否已经收齐全部参与者确认。
    pub(crate) fn snapshot_ready(&self) -> bool {
        self.gate.as_ref().is_some_and(RootSnapshotGate::ready)
    }

    /// 关闭 snapshot gate 并进入 mark 阶段；未收齐时失败。
    pub(crate) fn release_snapshot(&mut self) -> Result<(), MarkError> {
        let gate = self.gate.as_mut().ok_or(MarkError::SnapshotNotOpen)?;
        if !gate.ready() {
            return Err(MarkError::SnapshotIncomplete {
                missing: gate.missing(),
            });
        }
        if self.state != MarkCycleState::Snapshot {
            return Err(MarkError::CycleState {
                expected: MarkCycleState::Snapshot.name(),
                actual: self.state.name(),
            });
        }
        gate.close();
        self.state = MarkCycleState::Marking;
        Ok(())
    }

    /// 为一个 owner acquire 一个 mark ticket credit。
    ///
    /// mark ticket 仍拒绝自投递：源与目标相同意味着本 owner 自己就能标记，跨 owner 通道只会
    /// 浪费一个 slot 与一次 grace。
    pub(crate) fn acquire_ticket(
        &mut self,
        source: u32,
        target: u32,
    ) -> Result<GcCreditId, MarkError> {
        if source == target {
            return Err(MarkError::SelfTicket { owner: source });
        }
        self.credits.acquire_work(
            source,
            target,
            MessageFamilyTag::MarkTicket,
            self.cycle,
            self.topology,
        )
    }

    /// 为一个跨 block 边差量 acquire 一个 credit；同一 owner 内的跨 block 边是合法输入。
    pub(crate) fn acquire_edge_delta(
        &mut self,
        source: u32,
        target: u32,
    ) -> Result<GcCreditId, MarkError> {
        self.credits.acquire_work(
            source,
            target,
            MessageFamilyTag::EdgeDelta,
            self.cycle,
            self.topology,
        )
    }

    /// 发布一条跨 owner ticket：源 owner acquire credit，目标 mailbox 入队。
    pub(crate) fn publish_ticket(
        &mut self,
        source: u32,
        target: u32,
    ) -> Result<GcCreditId, MarkError> {
        let credit = self.acquire_ticket(source, target)?;
        self.mailboxes
            .get_mut(target as usize)
            .ok_or(MarkError::UnknownOwner { owner: target })?
            .publish(credit);
        self.stats.tickets_published += 1;
        Ok(credit)
    }

    /// 目标 owner 消费一条 ticket：credit 身份校验先于 mailbox 状态改变。
    ///
    /// 顺序是刻意的：错误族、错误目标、过期 generation 或重复 consume 都不允许改动 mailbox
    /// 计数，否则一次非法投递就会让 pending 与 credit 账本互相矛盾。
    pub(crate) fn consume_ticket(
        &mut self,
        owner: u32,
        credit: GcCreditId,
        cycle: u64,
        topology: u32,
    ) -> Result<(), MarkError> {
        self.credits.validate_consume(
            credit,
            MessageFamilyTag::MarkTicket,
            owner,
            cycle,
            topology,
        )?;
        self.mailboxes
            .get_mut(owner as usize)
            .ok_or(MarkError::UnknownOwner { owner })?
            .consume(credit)?;
        self.credits
            .consume_work(owner, credit, MessageFamilyTag::MarkTicket, cycle, topology)?;
        self.drop_forwarded(credit);
        self.stats.tickets_consumed += 1;
        Ok(())
    }

    /// 目标 owner 消费一条 edge delta：与 ticket 共用同一信用原语，但允许同一 owner 内跨 block。
    pub(crate) fn consume_edge_delta(
        &mut self,
        owner: u32,
        credit: GcCreditId,
        cycle: u64,
        topology: u32,
    ) -> Result<(), MarkError> {
        self.credits.validate_consume(
            credit,
            MessageFamilyTag::EdgeDelta,
            owner,
            cycle,
            topology,
        )?;
        self.credits
            .consume_work(owner, credit, MessageFamilyTag::EdgeDelta, cycle, topology)?;
        self.drop_forwarded(credit);
        self.stats.edge_deltas_consumed += 1;
        Ok(())
    }

    /// 把一个 owner 的 ticket 转发给另一个 owner；复用同一个 credit。
    pub(crate) fn forward_ticket(
        &mut self,
        owner: u32,
        target: u32,
        credit: GcCreditId,
    ) -> Result<(), MarkError> {
        // 转发是源 owner 侧的操作：只要求 credit 仍是本 cycle 的 MarkTicket，
        // 不把目标 owner 当作校验条件。
        self.credits.validate_in_flight(
            credit,
            MessageFamilyTag::MarkTicket,
            self.cycle,
            self.topology,
        )?;
        // 目标随转发改变：credit 的 target 必须跟着走，否则最终目标无法通过目标校验。
        self.credits.retarget(credit, target)?;
        self.mailboxes
            .get_mut(owner as usize)
            .ok_or(MarkError::UnknownOwner { owner })?
            .forward(credit)?;
        self.mailboxes
            .get_mut(target as usize)
            .ok_or(MarkError::UnknownOwner { owner: target })?
            .publish(credit);
        if !self.forwarded_in_flight.contains(&credit) {
            self.forwarded_in_flight.push(credit);
        }
        self.stats.tickets_forwarded += 1;
        Ok(())
    }

    /// 把一条在飞 edge credit 转投给新的目标 owner；管理权转移路径使用。
    pub(crate) fn forward_edge_delta(
        &mut self,
        credit: GcCreditId,
        target: u32,
    ) -> Result<(), MarkError> {
        self.credits.validate_in_flight(
            credit,
            MessageFamilyTag::EdgeDelta,
            self.cycle,
            self.topology,
        )?;
        self.credits.retarget(credit, target)?;
        self.stats.tickets_forwarded += 1;
        Ok(())
    }

    /// 被转发的记录走到最终目标才离开在飞集合；未转发过的身份不在集合里。
    fn drop_forwarded(&mut self, credit: GcCreditId) {
        if let Some(index) = self
            .forwarded_in_flight
            .iter()
            .position(|candidate| *candidate == credit)
        {
            self.forwarded_in_flight.swap_remove(index);
        }
    }

    /// 归还一个 owner 全部已 consume 的 credit；返回归还数量。
    pub(crate) fn settle_owner(&mut self, owner: u32) -> Result<u64, MarkError> {
        let returned = self.credits.settle_owner(owner)?;
        self.stats.credits_returned = self.stats.credits_returned.saturating_add(returned);
        Ok(returned)
    }

    /// 归还一个具体的 credit；边差量在应用完成后立即还款，不等 cycle 末尾。
    pub(crate) fn return_credit(&mut self, credit: GcCreditId) -> Result<(), MarkError> {
        self.credits.return_work(credit)?;
        self.stats.credits_returned = self.stats.credits_returned.saturating_add(1);
        Ok(())
    }

    /// 返回全部 owner 尚未归还的 credit 数。
    pub(crate) fn mark_credit_pending(&self) -> u64 {
        self.credits.pending()
    }

    /// 返回全部 mailbox 尚未消费的 ticket 数。
    pub(crate) fn mailbox_pending(&self) -> u64 {
        self.mailboxes
            .iter()
            .map(MarkMailbox::pending)
            .fold(0_u64, u64::saturating_add)
    }

    /// 返回因转发而仍在途、尚未被最终目标消费的 ticket 数。
    pub(crate) fn forwarded_pending(&self) -> u64 {
        self.forwarded_in_flight.len() as u64
    }

    /// 登记 cycle 内真实标记的对象数。
    pub(crate) fn note_marks(&mut self, count: u64) {
        self.stats.marks = self.stats.marks.saturating_add(count);
    }

    /// 按真实观测与 credit 账本推导七个收敛条件。
    pub(crate) fn termination(&self, observations: MarkObservations) -> MarkTermination {
        let mut conditions = [0_u64; 7];
        conditions[MarkCondition::LocalWorklist.index()] = observations.worklist_items;
        conditions[MarkCondition::PublishedBatch.index()] = observations.published_batches;
        conditions[MarkCondition::Mailbox.index()] = self.mailbox_pending();
        conditions[MarkCondition::BarrierBuffer.index()] = observations.barrier_buffer_keys;
        conditions[MarkCondition::ProducerEpoch.index()] = observations
            .producer_epoch_total
            .saturating_sub(observations.producer_epoch_confirmed);
        conditions[MarkCondition::ForwardingWork.index()] = observations
            .forwarding_work
            .saturating_add(self.forwarded_pending());
        conditions[MarkCondition::PendingCredit.index()] = self.mark_credit_pending();
        MarkTermination {
            cycle: self.cycle,
            topology: self.topology,
            conditions,
        }
    }

    /// 记录本 cycle 的终止判定并进入 converging。
    pub(crate) fn remember(&mut self, termination: MarkTermination) -> Result<(), MarkError> {
        if !matches!(
            self.state,
            MarkCycleState::Marking | MarkCycleState::Converging
        ) {
            return Err(MarkError::CycleState {
                expected: MarkCycleState::Marking.name(),
                actual: self.state.name(),
            });
        }
        self.last = Some(termination);
        self.state = MarkCycleState::Converging;
        Ok(())
    }

    /// 宣布 cycle 完成；要求终止记录已收敛。
    pub(crate) fn complete(&mut self) -> Result<(), MarkError> {
        if !matches!(
            self.state,
            MarkCycleState::Converging | MarkCycleState::Remark
        ) {
            return Err(MarkError::CycleState {
                expected: MarkCycleState::Converging.name(),
                actual: self.state.name(),
            });
        }
        let termination = self.last.ok_or(MarkError::CycleState {
            expected: "remembered-termination",
            actual: self.state.name(),
        })?;
        if !termination.converged() {
            return Err(MarkError::CycleState {
                expected: "converged-termination",
                actual: "blocking-conditions",
            });
        }
        self.state = MarkCycleState::Remark;
        self.state = MarkCycleState::Complete;
        self.state = MarkCycleState::Idle;
        self.stats.cycles += 1;
        Ok(())
    }

    /// 返回固定文本 dump；不含地址与宿主信息。
    pub(crate) fn dump(&self) -> String {
        use std::fmt::Write;
        let mut output = String::new();
        writeln!(
            output,
            "mark-plane cycle={} topology={} state={} owners={} credits={} slots={} mailboxes={}",
            self.cycle,
            self.topology,
            self.state.name(),
            self.owner_count(),
            self.credits.capacity(),
            self.credits.slot_count(),
            self.mailboxes.len(),
        )
        .expect("String写入");
        for owner in 0..self.owner_count() {
            let credit = self.credits.owner(owner).expect("owner 已登记");
            writeln!(
                output,
                "mark-plane-credit {} granted={} issued={} pending={} done={} returned={}",
                owner,
                self.credits.capacity(),
                credit.issued(),
                credit.pending(),
                credit.done(),
                credit.returned(),
            )
            .expect("String写入");
        }
        for (owner, mailbox) in self.mailboxes.iter().enumerate() {
            writeln!(
                output,
                "mark-plane-mailbox {} cycle={} topology={} pending={} consumed={} forwarded={}",
                owner,
                mailbox.cycle(),
                mailbox.topology(),
                mailbox.pending(),
                mailbox.consumed(),
                mailbox.forwarded(),
            )
            .expect("String写入");
        }
        if let Some(termination) = self.last {
            writeln!(
                output,
                "mark-plane-terminal cycle={} topology={} converged={} blocking={}",
                termination.cycle(),
                termination.topology(),
                termination.converged(),
                termination.blocking().join(","),
            )
            .expect("String写入");
        }
        writeln!(
            output,
            "mark-plane-stats cycles={} snapshots={} published={} consumed={} forwarded={} returned={} marks={} participants={}",
            self.stats.cycles,
            self.stats.snapshots,
            self.stats.tickets_published,
            self.stats.tickets_consumed,
            self.stats.tickets_forwarded,
            self.stats.credits_returned,
            self.stats.marks,
            self.stats.snapshot_participants,
        )
        .expect("String写入");
        output
    }
}
