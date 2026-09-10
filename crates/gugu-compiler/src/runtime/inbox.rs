//! owner inbox：多 producer、单 owner consumer 的 append-only batch queue。
//!
//! 一个 producer 发布完整 chain 的顺序是：先把 chain 的最后一个 `next` 清为 null 并以
//! Release 发布 chain 内容；对目标 tail 执行一次 AcqRel exchange；若 exchange 返回旧
//! tail，则以 Release 把旧 tail 的 `next` 链到 chain first；若旧 tail 表示空队列，则发布
//! front。consumer 只从自己的 front 开始，以 Acquire 读取 next，并且只更新自己的 front。
//!
//! producer 正在完成第三步时，consumer 可以暂时看到 null；这代表“稍后可见”，不是空队列，
//! 也不是错误。队列不是线性化 queue，不提供跨 producer 的全局顺序，也不能用于同步。

use std::sync::atomic::{AtomicU64, Ordering};

use super::message::{NULL_LINK, ReturnNodeId, ReturnNodePool, StagedChain, decode_node};
use super::slab::{Epoch, RawInvariant};

/// shard 的稠密编号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ShardIndex(u32);

impl ShardIndex {
    /// 返回编号原值。
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }

    /// 返回作为下标的编号。
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }

    /// 由编号原值还原。
    pub(crate) fn from_raw(raw: u32) -> Option<Self> {
        (raw < super::OWNER_INBOX_SHARDS).then_some(Self(raw))
    }
}

/// 独占一条 cache line 的填充包装。
///
/// front、tail 与统计计数必须分离到独立 cache line，沿用 Gugu 的 64/128-byte padding
/// 规则；`offset_of!` 断言在测试中固定这一点。
#[repr(align(64))]
#[derive(Debug, Default)]
struct Padded<T>(T);

impl<T> Padded<T> {
    fn get(&self) -> &T {
        &self.0
    }
}

/// 单个 shard 的累计统计；只用于诊断，不参与线性化。
#[derive(Debug, Default)]
pub(crate) struct ShardStats {
    published_batches: AtomicU64,
    published_items: AtomicU64,
    drained_items: AtomicU64,
    forwarded_items: AtomicU64,
    phantom_nulls: AtomicU64,
}

impl ShardStats {
    /// 返回已发布的 batch 数量。
    pub(crate) fn published_batches(&self) -> u64 {
        self.published_batches.load(Ordering::Relaxed)
    }

    /// 返回已发布的 item 数量。
    pub(crate) fn published_items(&self) -> u64 {
        self.published_items.load(Ordering::Relaxed)
    }

    /// 返回已消费的 item 数量。
    pub(crate) fn drained_items(&self) -> u64 {
        self.drained_items.load(Ordering::Relaxed)
    }

    /// 返回被转发的 item 数量。
    pub(crate) fn forwarded_items(&self) -> u64 {
        self.forwarded_items.load(Ordering::Relaxed)
    }

    /// 返回观察到 phantom null 的次数。
    pub(crate) fn phantom_nulls(&self) -> u64 {
        self.phantom_nulls.load(Ordering::Relaxed)
    }
}

/// 一个 shard 的队列头尾与统计。
#[derive(Debug)]
struct ShardQueue {
    /// producer-visible tail：指向链上最后一个可见 node。
    tail: Padded<AtomicU64>,
    /// consumer-only front：指向下一个待处理 node。
    front: Padded<AtomicU64>,
    stats: Padded<ShardStats>,
}

impl Default for ShardQueue {
    fn default() -> Self {
        Self {
            tail: Padded(AtomicU64::new(NULL_LINK)),
            front: Padded(AtomicU64::new(NULL_LINK)),
            stats: Padded(ShardStats::default()),
        }
    }
}

/// 一次发布的会话状态；四步之间可以暂停，恢复后继续同一份代码路径。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PublishSession {
    shard: ShardIndex,
    first: ReturnNodeId,
    last: ReturnNodeId,
    count: u32,
    bytes: u64,
    old_tail: u64,
    prepared: bool,
    exchanged: bool,
    linked: bool,
}

impl PublishSession {
    /// 返回目标 shard。
    pub(crate) const fn shard(&self) -> ShardIndex {
        self.shard
    }

    /// 返回 chain 首节点。
    pub(crate) const fn first(&self) -> ReturnNodeId {
        self.first
    }

    /// 返回 chain 尾节点。
    pub(crate) const fn last(&self) -> ReturnNodeId {
        self.last
    }

    /// 返回 batch 的 item 数。
    pub(crate) const fn count(&self) -> u32 {
        self.count
    }

    /// 返回 batch 的字节数。
    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 返回 chain 发布前队列是否为空。
    pub(crate) const fn was_empty(&self) -> bool {
        self.old_tail == NULL_LINK
    }
}

/// consumer 每次 service 的 item/byte 上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ServiceBudget {
    pub(crate) items: u32,
    pub(crate) bytes: u64,
}

impl ServiceBudget {
    /// 创建正常 service 预算。
    pub(crate) const fn new(items: u32, bytes: u64) -> Self {
        Self { items, bytes }
    }

    /// pressure service 提高预算后的取值；仍受 poll 与 scheduler budget 约束。
    pub(crate) const fn pressure(items: u32, bytes: u64) -> Self {
        Self { items, bytes }
    }
}

/// bounded chain snapshot 停止的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DrainStop {
    /// 队列当时为空。
    Empty,
    /// 达到 item 或 byte 上限。
    Budget,
    /// 碰到尚未发布的 next；稍后可见，不是错误。
    NullLink,
}

/// 一次 bounded chain 快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChainSnapshot {
    nodes: Vec<ReturnNodeId>,
    next: Option<ReturnNodeId>,
    bytes: u64,
    stop: DrainStop,
}

impl ChainSnapshot {
    /// 返回快照中的 node 顺序。
    pub(crate) fn nodes(&self) -> &[ReturnNodeId] {
        &self.nodes
    }

    /// 返回快照之后尚未处理的 node；只有预算上限才会有已知的下一节点。
    pub(crate) const fn next(&self) -> Option<ReturnNodeId> {
        self.next
    }

    /// 返回快照的字节数。
    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 返回停止原因。
    pub(crate) const fn stop(&self) -> DrainStop {
        self.stop
    }

    /// 返回快照是否为空。
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// 一次 drain 的统计。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DrainReport {
    pub(crate) items: u32,
    pub(crate) bytes: u64,
    pub(crate) forwarded: u32,
    pub(crate) stop: DrainStop,
}

/// consumer 侧的 per-shard 记账；只由 owner 读写。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct OwnerConsumer {
    last: Vec<Option<ReturnNodeId>>,
}

impl OwnerConsumer {
    /// 为给定 shard 数创建 consumer 记账。
    pub(crate) fn new(shards: u32) -> Self {
        Self {
            last: vec![None; shards as usize],
        }
    }

    /// 返回某个 shard 已完整处理的最后一个 node。
    pub(crate) fn last(&self, shard: ShardIndex) -> Option<ReturnNodeId> {
        self.last[shard.index()]
    }

    fn set_last(&mut self, shard: ShardIndex, node: Option<ReturnNodeId>) {
        self.last[shard.index()] = node;
    }

    /// 通过 queue-page grace 后丢弃全部记账：node 复用前必须重新建立 front/last。
    pub(crate) fn reset(&mut self) {
        self.last.fill(None);
    }
}

/// 多 producer、单 owner consumer 的 append-only batch queue。
#[derive(Debug)]
pub(crate) struct OwnerInbox {
    shards: Vec<ShardQueue>,
    gate: EpochGate,
}

impl OwnerInbox {
    /// 创建固定 shard 数量的 owner inbox。
    pub(crate) fn new(shards: u32) -> Self {
        Self {
            shards: (0..shards).map(|_| ShardQueue::default()).collect(),
            gate: EpochGate::new(),
        }
    }

    /// 返回 shard 数量。
    pub(crate) fn shards(&self) -> u32 {
        self.shards.len() as u32
    }

    /// 返回 producer gate。
    pub(crate) const fn gate(&self) -> &EpochGate {
        &self.gate
    }

    /// 返回某个 shard 的统计。
    pub(crate) fn stats(&self, shard: ShardIndex) -> &ShardStats {
        self.shards[shard.index()].stats.get()
    }

    /// 第一步：清空 chain 尾节点的 `next` 并以 Release 发布 chain 内容。
    pub(crate) fn prepare_chain(
        &self,
        chain: &StagedChain,
        pool: &ReturnNodePool,
    ) -> Result<PublishSession, RawInvariant> {
        let shard = chain
            .shard
            .ok_or_else(|| RawInvariant::new("发布 chain 缺少目标 shard"))?;
        if chain.count == 0 || chain.count > super::BATCH_MAX {
            return Err(RawInvariant::new("batch item 数超出登记上限"));
        }
        pool.link(chain.last, None);
        Ok(PublishSession {
            shard,
            first: chain.first,
            last: chain.last,
            count: chain.count,
            bytes: chain.bytes,
            old_tail: NULL_LINK,
            prepared: true,
            exchanged: false,
            linked: false,
        })
    }

    /// 第二步：对目标 tail 执行一次 AcqRel exchange。
    pub(crate) fn exchange_tail(&self, session: &mut PublishSession) -> Result<(), RawInvariant> {
        if !session.prepared {
            return Err(RawInvariant::new("发布步骤顺序非法：先准备 chain"));
        }
        let shard = &self.shards[session.shard.index()];
        session.old_tail = shard
            .tail
            .get()
            .swap(node_word(session.last), Ordering::AcqRel);
        session.exchanged = true;
        Ok(())
    }

    /// 第三步：旧 tail 非空时以 Release 把旧 tail 的 `next` 链到 chain first。
    pub(crate) fn link_old_tail(
        &self,
        session: &mut PublishSession,
        pool: &ReturnNodePool,
    ) -> Result<(), RawInvariant> {
        if !session.exchanged {
            return Err(RawInvariant::new("发布步骤顺序非法：先交换 tail"));
        }
        if let Some(old) = decode_node(session.old_tail) {
            pool.link(old, Some(session.first));
        }
        session.linked = true;
        Ok(())
    }

    /// 第四步：旧 tail 为空时发布 owner-only front。
    pub(crate) fn publish_front(&self, session: &PublishSession) -> Result<(), RawInvariant> {
        if !session.linked {
            return Err(RawInvariant::new("发布步骤顺序非法：先链接旧 tail"));
        }
        let shard = &self.shards[session.shard.index()];
        if session.old_tail == NULL_LINK {
            shard
                .front
                .get()
                .store(node_word(session.first), Ordering::Release);
        }
        Ok(())
    }

    /// 一次完整发布：四步的组合调用。
    pub(crate) fn publish_batch(
        &self,
        chain: &StagedChain,
        pool: &ReturnNodePool,
    ) -> Result<(), RawInvariant> {
        let mut session = self.prepare_chain(chain, pool)?;
        self.exchange_tail(&mut session)?;
        self.link_old_tail(&mut session, pool)?;
        self.publish_front(&session)?;
        let shard = &self.shards[session.shard.index()];
        shard
            .stats
            .get()
            .published_batches
            .fetch_add(1, Ordering::Relaxed);
        shard
            .stats
            .get()
            .published_items
            .fetch_add(u64::from(session.count), Ordering::Relaxed);
        Ok(())
    }

    /// consumer 第一步：取得 bounded chain snapshot。
    ///
    /// 碰到尚未发布的 `next` 时只记录 `NullLink` 并停止遍历：该 node 仍留在队列语义内，
    /// consumer 记账保留最后处理的 node，下一次 service 从它的 `next` 继续观察。
    pub(crate) fn snapshot(
        &self,
        shard: ShardIndex,
        consumer: &OwnerConsumer,
        budget: &ServiceBudget,
        pool: &ReturnNodePool,
    ) -> ChainSnapshot {
        let queue = &self.shards[shard.index()];
        let mut nodes = Vec::new();
        let mut bytes = 0_u64;
        let resume = consumer.last(shard);
        let mut current = match resume {
            Some(last) => pool.next(last),
            None => decode_node(queue.front.get().load(Ordering::Acquire)),
        };
        if current.is_none() {
            let stop = if resume.is_some() {
                queue
                    .stats
                    .get()
                    .phantom_nulls
                    .fetch_add(1, Ordering::Relaxed);
                DrainStop::NullLink
            } else {
                DrainStop::Empty
            };
            return ChainSnapshot {
                nodes,
                next: None,
                bytes,
                stop,
            };
        }
        let mut stop = DrainStop::NullLink;
        let mut pending = None;
        while let Some(node) = current {
            if nodes.len() >= budget.items as usize {
                stop = DrainStop::Budget;
                pending = Some(node);
                break;
            }
            let message_bytes = pool.node_bytes(node);
            if bytes + message_bytes > budget.bytes && !nodes.is_empty() {
                stop = DrainStop::Budget;
                pending = Some(node);
                break;
            }
            nodes.push(node);
            bytes += message_bytes;
            match pool.next(node) {
                Some(next) => current = Some(next),
                None => {
                    queue
                        .stats
                        .get()
                        .phantom_nulls
                        .fetch_add(1, Ordering::Relaxed);
                    stop = DrainStop::NullLink;
                    break;
                }
            }
        }
        ChainSnapshot {
            nodes,
            next: pending,
            bytes,
            stop,
        }
    }

    /// consumer 第六步：消息全部离开 queue 语义后推进 front 与 consumer 记账。
    pub(crate) fn advance_front(
        &self,
        shard: ShardIndex,
        consumer: &mut OwnerConsumer,
        snapshot: &ChainSnapshot,
    ) {
        if let Some(last) = snapshot.nodes.last().copied() {
            consumer.set_last(shard, Some(last));
            self.shards[shard.index()]
                .stats
                .get()
                .drained_items
                .fetch_add(snapshot.nodes.len() as u64, Ordering::Relaxed);
        }
        let front = snapshot.next.map_or(NULL_LINK, node_word);
        self.shards[shard.index()]
            .front
            .get()
            .store(front, Ordering::Release);
    }

    /// 记录一次转发的 item。
    pub(crate) fn record_forward(&self, shard: ShardIndex, items: u64) {
        self.shards[shard.index()]
            .stats
            .get()
            .forwarded_items
            .fetch_add(items, Ordering::Relaxed);
    }
}

fn node_word(id: ReturnNodeId) -> u64 {
    u64::from(id.raw())
}

/// queue-page grace 的收敛结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GraceOutcome {
    /// 全部旧 epoch participant 已确认，且没有 participant 处于 publish 区间。
    Converged,
    /// 仍有 participant 未确认。
    Pending,
    /// gate 尚未开启。
    Idle,
}

/// producer gate：publish_active 登记、epoch 发布与 queue-page grace 确认。
///
/// 协议：producer 开始 batch 前先登记 `active`，再 Acquire 读取 control epoch；epoch 发布
/// 后 gate 开启，此时 inactive 的 participant 不能越过 gate 开始新 batch。coordinator 记录
/// 发布时刻的 active 数量，等待 `active == 0` 且全部旧 participant 确认到达不持有 raw queue
/// pointer 的 checkpoint，然后才允许复用 page。
#[derive(Debug)]
pub(crate) struct EpochGate {
    control: Padded<AtomicU64>,
    active: Padded<AtomicU64>,
    confirmed: Padded<AtomicU64>,
    grace_pending: Padded<AtomicU64>,
}

/// 一次 publish 区间的登记凭据。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PublishTicket {
    pub(crate) epoch: Epoch,
}

impl EpochGate {
    /// 创建开放 gate。
    pub(crate) fn new() -> Self {
        Self {
            control: Padded(AtomicU64::new(0)),
            active: Padded(AtomicU64::new(0)),
            confirmed: Padded(AtomicU64::new(0)),
            grace_pending: Padded(AtomicU64::new(u64::MAX)),
        }
    }

    /// 返回当前 control epoch。
    pub(crate) fn epoch(&self) -> Epoch {
        Epoch::from_raw((self.control.get().load(Ordering::Acquire) & 0xFFFF_FFFF) as u32)
    }

    /// 返回 gate 是否开启。
    pub(crate) fn reclaiming(&self) -> bool {
        self.control.get().load(Ordering::Acquire) & (1 << 32) != 0
    }

    /// 返回当前处于 publish 区间的 participant 数量。
    pub(crate) fn active(&self) -> u64 {
        self.active.get().load(Ordering::Acquire)
    }

    /// 开始一个 publish batch；gate 开启时拒绝，调用者必须在 grace 结束后重试。
    pub(crate) fn begin_publish(&self) -> Result<PublishTicket, RawInvariant> {
        let epoch = self.epoch();
        self.active.get().fetch_add(1, Ordering::AcqRel);
        if self.reclaiming() {
            self.active.get().fetch_sub(1, Ordering::AcqRel);
            return Err(RawInvariant::new("queue-page grace 期间不能开始新 batch"));
        }
        Ok(PublishTicket { epoch })
    }

    /// 到达不持有 raw queue pointer 的 checkpoint 后确认。
    pub(crate) fn end_publish(&self, _ticket: PublishTicket) {
        self.confirmed.get().fetch_add(1, Ordering::AcqRel);
        self.active.get().fetch_sub(1, Ordering::AcqRel);
    }

    /// Release 发布新 `slab_epoch` 与 reclaim gate，并阻止新 participant 登记。
    pub(crate) fn open_grace(&self) -> Epoch {
        let pending = self.active.get().load(Ordering::Acquire);
        self.grace_pending.get().store(pending, Ordering::Release);
        self.confirmed.get().store(0, Ordering::Release);
        let control = self.control.get().load(Ordering::Acquire);
        let next = control.wrapping_add(1).wrapping_add(1 << 32);
        self.control.get().store(next, Ordering::Release);
        self.epoch()
    }

    /// Acquire 读取 grace 是否收敛。
    pub(crate) fn grace_outcome(&self) -> GraceOutcome {
        if !self.reclaiming() {
            return GraceOutcome::Idle;
        }
        let active = self.active.get().load(Ordering::Acquire);
        let confirmed = self.confirmed.get().load(Ordering::Acquire);
        let pending = self.grace_pending.get().load(Ordering::Acquire);
        if active == 0 && confirmed >= pending {
            GraceOutcome::Converged
        } else {
            GraceOutcome::Pending
        }
    }

    /// 关闭 gate 并重新开放 participant 登记。
    pub(crate) fn close_grace(&self) {
        let control = self.control.get().load(Ordering::Acquire) & 0xFFFF_FFFF;
        self.control.get().store(control, Ordering::Release);
    }
}
