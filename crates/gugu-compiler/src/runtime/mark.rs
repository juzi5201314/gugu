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

use super::mark_schema::{
    MARK_CONVERGENCE_CONDITIONS, MARK_CREDIT_COUNTER_BITS, MARK_CREDIT_OWNER_BITS,
    MARK_MAILBOX_CONSUMERS, MARK_SNAPSHOT_PARTICIPANTS, MarkRuntimeContract,
};

/// 一个 owner credit 的生命周期状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreditState {
    /// 已 acquire，尚未被任何 owner consume。
    InFlight,
    /// 已被目标 owner consume，等待归还。
    Done,
    /// 已归还，本 cycle 内不再复用。
    Returned,
}

/// 一个 owner 的 credit 账本。
///
/// `states` 按 acquire 顺序稠密排列，下标就是局部 credit 编号；一次 cycle 内编号不复用，
/// 因此同一编号不会先归还再被重新发出——重复 ticket 才能被可靠地判成 `CreditNotInFlight`。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MarkCredit {
    owner: u32,
    granted: u64,
    states: Vec<CreditState>,
    in_flight: u64,
    done: u64,
    returned: u64,
}

impl MarkCredit {
    /// 创建一个 owner 的账本。
    pub(crate) const fn new(owner: u32) -> Self {
        Self {
            owner,
            granted: 0,
            states: Vec::new(),
            in_flight: 0,
            done: 0,
            returned: 0,
        }
    }

    /// 以新的授权上界开始一个 cycle，清空全部状态。
    pub(crate) fn reset(&mut self, granted: u64) {
        self.granted = granted;
        self.states.clear();
        self.in_flight = 0;
        self.done = 0;
        self.returned = 0;
    }

    /// 返回本 cycle 授权的 credit 上界。
    pub(crate) const fn granted(&self) -> u64 {
        self.granted
    }

    /// 返回尚未 acquire 的额度。
    pub(crate) fn available(&self) -> u64 {
        self.granted.saturating_sub(self.states.len() as u64)
    }

    /// acquire 一个 credit，返回局部稠密编号。
    pub(crate) fn acquire(&mut self) -> Result<u32, MarkError> {
        if self.states.len() as u64 >= self.granted {
            return Err(MarkError::PoolExhausted {
                owner: self.owner,
                granted: self.granted,
            });
        }
        let local = u32::try_from(self.states.len()).map_err(|_| MarkError::PoolExhausted {
            owner: self.owner,
            granted: self.granted,
        })?;
        self.states.push(CreditState::InFlight);
        self.in_flight += 1;
        Ok(local)
    }

    /// 目标 owner 消费一个 credit：InFlight → Done。
    pub(crate) fn consume(&mut self, local: u32) -> Result<(), MarkError> {
        match self.states.get(local as usize) {
            None => Err(MarkError::CreditNotIssued {
                owner: self.owner,
                credit: local,
            }),
            Some(CreditState::InFlight) => {
                self.states[local as usize] = CreditState::Done;
                self.in_flight -= 1;
                self.done += 1;
                Ok(())
            }
            Some(_) => Err(MarkError::CreditNotInFlight {
                owner: self.owner,
                credit: local,
            }),
        }
    }

    /// 归还一个已 consume 的 credit：Done → Returned。
    pub(crate) fn return_credit(&mut self, local: u32) -> Result<(), MarkError> {
        match self.states.get(local as usize) {
            Some(CreditState::Done) => {
                self.states[local as usize] = CreditState::Returned;
                self.done -= 1;
                self.returned += 1;
                Ok(())
            }
            _ => Err(MarkError::CreditNotDone {
                owner: self.owner,
                credit: local,
            }),
        }
    }

    /// 返回已 acquire 且尚未归还的 credit 数。
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

    /// 本 owner 的 credit 是否全部归还。
    pub(crate) fn converged(&self) -> bool {
        self.in_flight == 0 && self.done == 0
    }

    /// 把一个局部编号编码成跨 owner 的 credit id：`owner(8) | counter(24)`。
    pub(crate) fn credit_id(&self, owner_index: u32, local: u32) -> Result<u32, MarkError> {
        if owner_index >= 1 << MARK_CREDIT_OWNER_BITS || local >= 1 << MARK_CREDIT_COUNTER_BITS {
            return Err(MarkError::PoolExhausted {
                owner: self.owner,
                granted: self.granted,
            });
        }
        Ok((owner_index << MARK_CREDIT_COUNTER_BITS) | local)
    }
}

/// 从 credit id 解出源 owner 编号。
pub(crate) const fn credit_source(credit: u32) -> u32 {
    credit >> MARK_CREDIT_COUNTER_BITS
}

/// 从 credit id 解出源 owner 的局部编号。
pub(crate) const fn credit_local(credit: u32) -> u32 {
    credit & ((1 << MARK_CREDIT_COUNTER_BITS) - 1)
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
    last_credit: Option<u32>,
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
    pub(crate) fn publish(&mut self, credit: u32) {
        self.pending += 1;
        self.last_credit = Some(credit);
    }

    /// 单 consumer 消费一条 ticket。
    pub(crate) fn consume(&mut self, credit: u32) -> Result<(), MarkError> {
        if self.pending == 0 {
            return Err(MarkError::MailboxEmpty { owner: self.owner });
        }
        self.pending -= 1;
        self.consumed += 1;
        self.last_credit = Some(credit);
        Ok(())
    }

    /// 转发一条 ticket：从本 owner 的 mailbox 移除。
    pub(crate) fn forward(&mut self, _credit: u32) -> Result<(), MarkError> {
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

    /// 返回最近一次消费或发布的 credit 编号。
    pub(crate) const fn last_credit(&self) -> Option<u32> {
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
    /// 引用了从未 acquire 的 credit。
    CreditNotIssued { owner: u32, credit: u32 },
    /// credit 不在 InFlight 状态；重复 ticket 落在这里。
    CreditNotInFlight { owner: u32, credit: u32 },
    /// credit 不在 Done 状态，不能归还。
    CreditNotDone { owner: u32, credit: u32 },
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
                write!(formatter, "owner {owner} 引用了未签发的 credit {credit}")
            }
            Self::CreditNotInFlight { owner, credit } => {
                write!(
                    formatter,
                    "owner {owner} 的 credit {credit} 不在 InFlight 状态（重复 ticket）"
                )
            }
            Self::CreditNotDone { owner, credit } => {
                write!(
                    formatter,
                    "owner {owner} 的 credit {credit} 不在 Done 状态，不能归还"
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

/// mark 平面：credit 账本、mailbox、snapshot gate 与终止判定。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MarkPlane {
    contract: MarkRuntimeContract,
    cycle: u64,
    topology: u32,
    state: MarkCycleState,
    credits: Vec<MarkCredit>,
    mailboxes: Vec<MarkMailbox>,
    /// 已转发但尚未被最终目标 consume 的 credit 编号；`forwarding-work` 的真实在飞量。
    forwarded_in_flight: Vec<u32>,
    gate: Option<RootSnapshotGate>,
    stats: MarkStats,
    last: Option<MarkTermination>,
}

impl MarkPlane {
    /// 按契约创建平面；owner 数越过 credit id 的 owner 位宽时报错。
    pub(crate) fn new(contract: &MarkRuntimeContract, owners: u32) -> Result<Self, MarkError> {
        if owners >= 1 << contract.credit_owner_bits() {
            return Err(MarkError::UnknownOwner { owner: owners });
        }
        let credits = (0..owners).map(MarkCredit::new).collect();
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
        u32::try_from(self.credits.len()).expect("owner 数适配 u32")
    }

    /// 返回一个 owner 的 credit 账本。
    pub(crate) fn credit(&self, owner: u32) -> Result<&MarkCredit, MarkError> {
        self.credits
            .get(owner as usize)
            .ok_or(MarkError::UnknownOwner { owner })
    }

    /// 返回一个 owner 的 credit 账本可变引用；状态机测试与偿还路径使用。
    pub(crate) fn credit_mut(&mut self, owner: u32) -> Result<&mut MarkCredit, MarkError> {
        self.credits
            .get_mut(owner as usize)
            .ok_or(MarkError::UnknownOwner { owner })
    }

    /// 返回一个 owner 的 mailbox。
    pub(crate) fn mailbox(&self, owner: u32) -> Result<&MarkMailbox, MarkError> {
        self.mailboxes
            .get(owner as usize)
            .ok_or(MarkError::UnknownOwner { owner })
    }

    /// 开始一个 cycle：固定 cycle/topology、按契约授权 credit、打开 gate。
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
        let granted = self.contract.credit_pool();
        for credit in &mut self.credits {
            credit.reset(granted);
        }
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

    /// 为一个 owner acquire 一个 credit，返回跨 owner 编码的 credit id。
    pub(crate) fn acquire(&mut self, owner: u32) -> Result<u32, MarkError> {
        let credit = self
            .credits
            .get_mut(owner as usize)
            .ok_or(MarkError::UnknownOwner { owner })?;
        let local = credit.acquire()?;
        credit.credit_id(owner, local)
    }

    /// 发布一条跨 owner ticket：源 owner acquire credit，目标 mailbox 入队。
    pub(crate) fn publish_ticket(&mut self, source: u32, target: u32) -> Result<u32, MarkError> {
        if source == target {
            return Err(MarkError::SelfTicket { owner: source });
        }
        let credit = self.acquire(source)?;
        self.mailboxes
            .get_mut(target as usize)
            .ok_or(MarkError::UnknownOwner { owner: target })?
            .publish(credit);
        self.stats.tickets_published += 1;
        Ok(credit)
    }

    /// 目标 owner 消费一条 ticket：校验 cycle/topology 与源 owner 后收口 credit。
    pub(crate) fn consume_ticket(
        &mut self,
        owner: u32,
        credit: u32,
        cycle: u64,
        topology: u32,
    ) -> Result<(), MarkError> {
        if cycle != self.cycle {
            return Err(MarkError::StaleCycle {
                ticket: cycle,
                current: self.cycle,
            });
        }
        if topology != self.topology {
            return Err(MarkError::StaleTopology {
                ticket: topology,
                current: self.topology,
            });
        }
        let source = credit_source(credit);
        if source as usize >= self.credits.len() {
            return Err(MarkError::UnknownOwner { owner: source });
        }
        if source == owner {
            return Err(MarkError::SelfTicket { owner });
        }
        self.mailboxes
            .get_mut(owner as usize)
            .ok_or(MarkError::UnknownOwner { owner })?
            .consume(credit)?;
        self.credits[source as usize].consume(credit_local(credit))?;
        // 被转发的 ticket 走到最终目标才离开在飞集合；未转发过的编号不在集合里。
        if let Some(index) = self
            .forwarded_in_flight
            .iter()
            .position(|candidate| *candidate == credit)
        {
            self.forwarded_in_flight.swap_remove(index);
        }
        self.stats.tickets_consumed += 1;
        Ok(())
    }

    /// 把一个 owner 的 ticket 转发给另一个 owner。
    pub(crate) fn forward_ticket(
        &mut self,
        owner: u32,
        target: u32,
        credit: u32,
    ) -> Result<(), MarkError> {
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

    /// 归还一个 owner 全部已 consume 的 credit；返回归还数量。
    pub(crate) fn settle_owner(&mut self, owner: u32) -> Result<u64, MarkError> {
        let credit = self
            .credits
            .get_mut(owner as usize)
            .ok_or(MarkError::UnknownOwner { owner })?;
        let done: Vec<u32> = (0..credit.states.len())
            .filter(|index| credit.states[*index] == CreditState::Done)
            .map(|index| u32::try_from(index).expect("credit 下标适配 u32"))
            .collect();
        let mut returned = 0_u64;
        for local in done {
            credit.return_credit(local)?;
            returned += 1;
        }
        self.stats.credits_returned = self.stats.credits_returned.saturating_add(returned);
        Ok(returned)
    }

    /// 返回全部 owner 尚未归还的 credit 数。
    pub(crate) fn mark_credit_pending(&self) -> u64 {
        self.credits
            .iter()
            .map(MarkCredit::pending)
            .fold(0_u64, u64::saturating_add)
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
            "mark-plane cycle={} topology={} state={} owners={} credits={} mailboxes={}",
            self.cycle,
            self.topology,
            self.state.name(),
            self.owner_count(),
            self.credits.len(),
            self.mailboxes.len(),
        )
        .expect("String写入");
        for (owner, credit) in self.credits.iter().enumerate() {
            writeln!(
                output,
                "mark-plane-credit {} granted={} pending={} done={} returned={}",
                owner,
                credit.granted(),
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
