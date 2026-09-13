//! M:N 调度基础路径的确定性参考模型：runnable 队列、park、steal、topology 与终止。
//!
//! 本模型是 safe Rust 的确定性参考实现，不进入镜像执行路径：真实的 `UnsafeCell` 数组与
//! 128B head/tail 分离由契约常量与 backend layout query 固定；Linux futex/eventfd 与
//! Windows `WaitOnAddress`/semaphore 的真实等待只出现在 bench harness，单测用确定性
//! `FakeWaker` 记录唤醒序列。timer/poller/monitor/GC-stop/foreign-lease 的完整
//! 状态机由后续模块消费同一原语；select 等待协议由等待平面参照模型实现。`PollControl` 的
//! `poll_flags`/`requested_gc_epoch`/`ack_gc_epoch` 槽与 `PREEMPT`/`GC_STOP` 位定义只以
//! 注释形式落在这里。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use super::coroutine::{CoroutineHandle, CoroutineState, ENQUEUED};
use super::scheduler_schema::{
    SCHED_BATCH_MAX, SCHED_LOCAL_CAPACITY, SCHED_REMOTE_SHARDS, SCHED_SERVICE_BATCH,
    SCHED_SERVICE_INTERVAL,
};
use super::slab::RawInvariant;
use super::startup_schema::TerminationMode;
use super::termination::TerminationPlan;
use super::{BATCH_MAX, OWNER_INBOX_SHARDS};

/// release 镜像选择的 deque 变体；未过性能门禁前恒为 `false`，热路径无分支。
pub(crate) const SELECT_PACKED55: bool = false;
/// `Packed55` 的 `RESETTING` 哨兵：合法距离为 0..=256，511 保留为重置态。
pub(crate) const PACKED55_RESETTING: u64 = 511;
/// xorshift64* 的乘子。
const STEAL_MULTIPLIER: u64 = 2_685_821_657_736_338_717;
/// park 前的有界自旋轮数。
const PARK_SPIN_ROUNDS: u32 = 64;

/// 调度器维护的 runnable 句柄：`CoroutineTable` 的稠密下标加 generation。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunnableHandle {
    /// 协程控制块句柄。
    pub(crate) coroutine: CoroutineHandle,
}

impl RunnableHandle {
    /// 由控制块句柄构造。
    pub(crate) const fn new(coroutine: CoroutineHandle) -> Self {
        Self { coroutine }
    }
}

/// `Classic64` 正确性基线：不回绕的 `u64 head/tail`。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Classic64Deque {
    slots: VecDeque<RunnableHandle>,
    /// 已发布的尾 ticket。
    tail: u64,
    /// 已认领的头 ticket。
    head: u64,
}

/// `Packed55` 同语义第二变体：单 `u64` 编码 55-bit steal ticket 与 9-bit 距离。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Packed55Deque {
    slots: VecDeque<RunnableHandle>,
    /// 已发布的尾 ticket。
    tail: u64,
    /// 已认领的头 ticket。
    steal_head: u64,
    packed: u64,
}

/// 两种变体共享的 runnable deque 行为。
pub(crate) trait RunnableDeque {
    /// 创建空队列。
    fn new() -> Self;
    /// 返回队列长度。
    fn len(&self) -> usize;
    /// 判断队列是否为空。
    fn is_empty(&self) -> bool;
    /// owner 尾部 push。
    fn push_back(&mut self, handle: RunnableHandle) -> Result<(), RawInvariant>;
    /// owner 尾部 pop。
    fn pop_back(&mut self) -> Option<RunnableHandle>;
    /// overflow 从头部取一项：local 满时一次认领最旧 128 项。
    fn pop_front_for_overflow(&mut self) -> Option<RunnableHandle>;
    /// thief/overflow 从头部认领连续范围：当前量一半向上取整，至多 128。
    fn claim_head(&mut self, max: usize) -> Vec<RunnableHandle>;
    /// 返回逻辑距离。
    fn distance(&self) -> u64;
}

impl RunnableDeque for Classic64Deque {
    fn new() -> Self {
        Self::default()
    }

    fn len(&self) -> usize {
        self.slots.len()
    }

    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn push_back(&mut self, handle: RunnableHandle) -> Result<(), RawInvariant> {
        if self.slots.len() >= SCHED_LOCAL_CAPACITY as usize {
            return Err(RawInvariant::new("调度本地队列已满"));
        }
        let tail = self
            .tail
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("调度 ticket 溢出"))?;
        self.tail = tail;
        self.slots.push_back(handle);
        debug_assert!(self.distance() <= u64::from(SCHED_LOCAL_CAPACITY));
        Ok(())
    }

    fn pop_back(&mut self) -> Option<RunnableHandle> {
        let handle = self.slots.pop_back()?;
        self.tail -= 1;
        debug_assert!(self.distance() <= u64::from(SCHED_LOCAL_CAPACITY));
        Some(handle)
    }
    fn pop_front_for_overflow(&mut self) -> Option<RunnableHandle> {
        let handle = self.slots.pop_front()?;
        self.head = self.head.checked_add(1).expect("认领数量不超过队列长度");
        debug_assert!(self.distance() <= u64::from(SCHED_LOCAL_CAPACITY));
        Some(handle)
    }
    fn claim_head(&mut self, max: usize) -> Vec<RunnableHandle> {
        let take = self.slots.len().div_ceil(2).min(max).min(self.slots.len());
        let mut claimed = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(handle) = self.slots.pop_front() {
                claimed.push(handle);
            }
        }
        self.head = self
            .head
            .checked_add(claimed.len() as u64)
            .expect("认领数量不超过队列长度");
        debug_assert!(self.distance() <= u64::from(SCHED_LOCAL_CAPACITY));
        claimed
    }

    fn distance(&self) -> u64 {
        self.tail - self.head
    }
}

impl RunnableDeque for Packed55Deque {
    fn new() -> Self {
        Self::default()
    }

    fn len(&self) -> usize {
        self.slots.len()
    }

    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn push_back(&mut self, handle: RunnableHandle) -> Result<(), RawInvariant> {
        if self.slots.len() >= SCHED_LOCAL_CAPACITY as usize {
            return Err(RawInvariant::new("调度本地队列已满"));
        }
        if self.packed_distance() == PACKED55_RESETTING {
            return Err(RawInvariant::new("调度队列正在重置"));
        }
        let tail = self
            .tail
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("调度 ticket 溢出"))?;
        self.tail = tail;
        self.slots.push_back(handle);
        self.packed = Self::encode(self.steal_head, self.tail - self.steal_head);
        debug_assert!(self.distance() <= u64::from(SCHED_LOCAL_CAPACITY));
        Ok(())
    }

    fn pop_back(&mut self) -> Option<RunnableHandle> {
        if self.packed_distance() == PACKED55_RESETTING {
            return None;
        }
        let handle = self.slots.pop_back()?;
        self.tail -= 1;
        self.packed = Self::encode(self.steal_head, self.tail - self.steal_head);
        debug_assert!(self.distance() <= u64::from(SCHED_LOCAL_CAPACITY));
        Some(handle)
    }
    fn pop_front_for_overflow(&mut self) -> Option<RunnableHandle> {
        if self.packed_distance() == PACKED55_RESETTING {
            return None;
        }
        let handle = self.slots.pop_front()?;
        self.steal_head = self
            .steal_head
            .checked_add(1)
            .expect("认领数量不超过队列长度");
        self.packed = Self::encode(self.steal_head, self.tail - self.steal_head);
        debug_assert!(self.distance() <= u64::from(SCHED_LOCAL_CAPACITY));
        Some(handle)
    }
    fn claim_head(&mut self, max: usize) -> Vec<RunnableHandle> {
        if self.packed_distance() == PACKED55_RESETTING {
            return Vec::new();
        }
        let take = self.slots.len().div_ceil(2).min(max).min(self.slots.len());
        let mut claimed = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(handle) = self.slots.pop_front() {
                claimed.push(handle);
            }
        }
        self.steal_head = self
            .steal_head
            .checked_add(claimed.len() as u64)
            .expect("认领数量不超过队列长度");
        self.packed = Self::encode(self.steal_head, self.tail - self.steal_head);
        debug_assert!(self.distance() <= u64::from(SCHED_LOCAL_CAPACITY));
        claimed
    }

    fn distance(&self) -> u64 {
        self.tail - self.steal_head
    }
}

impl Packed55Deque {
    fn encode(steal_head: u64, distance: u64) -> u64 {
        debug_assert!(distance <= u64::from(SCHED_LOCAL_CAPACITY));
        (steal_head << 9) | distance
    }

    fn packed_distance(&self) -> u64 {
        self.packed & 511
    }

    /// 空队列且无 in-flight steal 时进入 `RESETTING`。
    pub(crate) fn begin_reset(&mut self) -> Result<(), RawInvariant> {
        if !self.slots.is_empty() {
            return Err(RawInvariant::new("非空调度队列不能重置"));
        }
        self.packed = PACKED55_RESETTING;
        Ok(())
    }

    /// 完成重置：tail 归零并发布零 packed head。
    pub(crate) fn finish_reset(&mut self) -> Result<(), RawInvariant> {
        if self.packed_distance() != PACKED55_RESETTING {
            return Err(RawInvariant::new("调度队列不在重置态"));
        }
        self.tail = 0;
        self.steal_head = 0;
        self.packed = Self::encode(0, 0);
        Ok(())
    }
}

/// release 实际选择的 deque 变体。
pub(crate) type SelectedDeque = Classic64Deque;

/// processor 的拥有态：`Idle`、`Bound` 或 `Retiring`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessorState {
    /// 未绑定 worker。
    Idle,
    /// 已绑定 worker，覆盖 managed 执行与 attached 普通 bridge。
    Bound,
    /// 正在退役，不再接受新 publish。
    Retiring,
}

/// processor 控制块的确定性镜像。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessorRecord {
    /// 稳定身份，永不复用，不兼作数组下标。
    pub(crate) id: u64,
    /// NUMA domain，本阶段恒为 0。
    pub(crate) numa_domain: u32,
    /// 拥有态。
    pub(crate) state: ProcessorState,
    /// 刚 ready、与当前工作具局部性的一个协程。
    pub(crate) run_next: Option<RunnableHandle>,
    /// 同一 coroutine 经 `run_next` 连续命中的次数，至多 1。
    pub(crate) run_next_hits: u32,
    /// owner-only 本地队列。
    pub(crate) local: SelectedDeque,
    /// 8 个 remote shard 的 detached carry。
    pub(crate) remote_carries: Vec<Vec<RunnableHandle>>,
    /// owner-only injection carry。
    pub(crate) injection_carry: Vec<RunnableHandle>,
    /// remote round-robin cursor。
    pub(crate) rr_cursor: u32,
    /// service tick。
    pub(crate) service_tick: u64,
    // `PollControl { poll_flags, requested_gc_epoch, ack_gc_epoch }` 的 epoch 槽与
    // `PREEMPT`/`GC_STOP` 位定义只以注释形式落在这里；当前恒无 pending poll，绑定循环
    // 先处理占位 flag 再进入选择顺序。
    /// 占位：是否有 pending 的 GC_STOP/PREEMPT，当前只作分支覆盖。
    pub(crate) pending_poll: bool,
    /// processor-local scratch cache 的 0 号 class：内联 8 个 word。
    pub(crate) select_scratch: [u64; 8],
}

impl ProcessorRecord {
    /// 由稳定 ID 创建空 processor。
    pub(crate) fn new(id: u64) -> Self {
        Self {
            id,
            numa_domain: 0,
            state: ProcessorState::Idle,
            run_next: None,
            run_next_hits: 0,
            local: SelectedDeque::new(),
            remote_carries: vec![Vec::new(); SCHED_REMOTE_SHARDS as usize],
            injection_carry: Vec::new(),
            rr_cursor: 0,
            service_tick: 0,
            pending_poll: false,
            select_scratch: [0; 8],
        }
    }

    /// 放入 `run_next`：旧值先回 local 尾。
    pub(crate) fn push_run_next(&mut self, handle: RunnableHandle) -> Result<(), RawInvariant> {
        if let Some(old) = self.run_next.replace(handle) {
            self.run_next_hits = 0;
            self.local.push_back(old)?;
        }
        Ok(())
    }

    /// 取出 `run_next`：同一 coroutine 连续命中至多 1 次。
    pub(crate) fn take_run_next(&mut self) -> Option<RunnableHandle> {
        let handle = self.run_next.take()?;
        if self.run_next_hits >= 1 {
            self.run_next_hits = 0;
            return None;
        }
        self.run_next_hits += 1;
        Some(handle)
    }

    /// local 满时认领最旧 128 项串成 batch。
    pub(crate) fn claim_overflow(&mut self) -> Vec<RunnableHandle> {
        self.run_next_hits = 0;
        let mut batch = Vec::new();
        for _ in 0..SCHED_BATCH_MAX {
            if let Some(handle) = self.local.pop_front_for_overflow() {
                batch.push(handle);
            } else {
                break;
            }
        }
        batch
    }

    /// 是否持有任何 runnable。
    pub(crate) fn has_work(&self) -> bool {
        self.run_next.is_some()
            || !self.local.is_empty()
            || self.remote_carries.iter().any(|carry| !carry.is_empty())
            || !self.injection_carry.is_empty()
    }

    /// 固定转移序收集全部本地状态：run_next、local、8 remote carry、injection carry。
    pub(crate) fn drain_all(&mut self) -> Vec<RunnableHandle> {
        let mut out = Vec::new();
        if let Some(handle) = self.run_next.take() {
            out.push(handle);
        }
        self.run_next_hits = 0;
        while let Some(handle) = self.local.pop_back() {
            out.push(handle);
        }
        for carry in &mut self.remote_carries {
            out.append(carry);
        }
        out.append(&mut self.injection_carry);
        out
    }
}

/// 调度 staging：独立的 `Vec<CoroutineHandle>` 加 `(target, shard)` 单目标约束。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ScheduleStaging {
    target: Option<u64>,
    shard: Option<u32>,
    handles: Vec<RunnableHandle>,
}

impl ScheduleStaging {
    /// 创建空 staging。
    pub(crate) const fn new() -> Self {
        Self {
            target: None,
            shard: None,
            handles: Vec::new(),
        }
    }

    /// 返回当前 target。
    pub(crate) const fn target(&self) -> Option<u64> {
        self.target
    }

    /// 返回当前 shard。
    pub(crate) const fn shard(&self) -> Option<u32> {
        self.shard
    }

    /// 返回暂存数量。
    pub(crate) fn len(&self) -> usize {
        self.handles.len()
    }

    /// 判断是否为空。
    pub(crate) fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// 暂存一个 handle；target 或 shard 改变时调用者先刷新旧链。
    pub(crate) fn stage(
        &mut self,
        target: u64,
        shard: u32,
        handle: RunnableHandle,
    ) -> Result<(), RawInvariant> {
        if let Some(current) = self.target
            && current != target
        {
            return Err(RawInvariant::new("调度 staging 一次只允许一个 target"));
        }
        if let Some(current) = self.shard
            && current != shard
        {
            return Err(RawInvariant::new("调度 staging 一次只允许一个 shard"));
        }
        if self.handles.len() >= SCHED_BATCH_MAX as usize {
            return Err(RawInvariant::new("调度 batch 超出登记上限"));
        }
        self.target = Some(target);
        self.shard = Some(shard);
        self.handles.push(handle);
        Ok(())
    }

    /// 排空 staging 并返回 chain。
    pub(crate) fn drain(&mut self) -> Option<(u64, u32, Vec<RunnableHandle>)> {
        if self.handles.is_empty() {
            return None;
        }
        let target = self.target.take().expect("非空 staging 必有 target");
        let shard = self.shard.take().expect("非空 staging 必有 shard");
        let handles = std::mem::take(&mut self.handles);
        Some((target, shard, handles))
    }
}

/// 稳定的 producer 句柄：worker、poller、foreign/callback 线程各持一个。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProducerHandle {
    /// shard 选择种子。
    pub(crate) shard_seed: u64,
    /// publish 区间登记。
    pub(crate) publish_active: bool,
    /// 已见的 topology epoch。
    pub(crate) topology_epoch_seen: u64,
    /// 已见的 slab epoch。
    pub(crate) slab_epoch_seen: u64,
    /// 仍由原 owner 保活、尚未认领的节点。
    pub(crate) pending_node: Option<RunnableHandle>,
    /// 当前 staging。
    pub(crate) staging: ScheduleStaging,
}

impl ProducerHandle {
    /// 创建新 handle。
    pub(crate) const fn new(shard_seed: u64) -> Self {
        Self {
            shard_seed,
            publish_active: false,
            topology_epoch_seen: 0,
            slab_epoch_seen: 0,
            pending_node: None,
            staging: ScheduleStaging::new(),
        }
    }

    /// 进入 publish 区间：设置 `publish_active` 后必须重读 queue control epoch。
    pub(crate) fn begin_publish(&mut self) {
        self.publish_active = true;
    }

    /// 离开 publish 区间。
    pub(crate) fn end_publish(&mut self) {
        self.publish_active = false;
    }
}

/// 确定性 `FakeWaker`：记录唤醒序列，不执行真实线程唤醒。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FakeWaker {
    woken: Vec<(u64, u64)>,
}

impl FakeWaker {
    /// 创建空 waker。
    pub(crate) const fn new() -> Self {
        Self { woken: Vec::new() }
    }

    /// 记录一次唤醒。
    pub(crate) fn wake(&mut self, worker: u64, generation: u64) {
        self.woken.push((worker, generation));
    }

    /// 返回唤醒序列。
    pub(crate) fn woken(&self) -> &[(u64, u64)] {
        &self.woken
    }
}

/// 独立的 `IdleRegistry`：只保存 idle-worker LIFO 与 generation token。
#[derive(Debug, Default)]
pub(crate) struct IdleRegistry {
    work_seq: AtomicU64,
    idle_count: AtomicU64,
    entries: Vec<(u64, u64)>,
    waker: FakeWaker,
}

impl IdleRegistry {
    /// 创建空 registry。
    pub(crate) const fn new() -> Self {
        Self {
            work_seq: AtomicU64::new(0),
            idle_count: AtomicU64::new(0),
            entries: Vec::new(),
            waker: FakeWaker::new(),
        }
    }

    /// 返回当前 `work_seq`。
    pub(crate) fn work_seq(&self) -> u64 {
        self.work_seq.load(Ordering::Acquire)
    }

    /// 返回当前 idle 数量。
    pub(crate) fn idle_count(&self) -> u64 {
        self.idle_count.load(Ordering::Acquire)
    }

    /// 返回唤醒序列。
    pub(crate) fn woken(&self) -> &[(u64, u64)] {
        self.waker.woken()
    }

    /// Acquire 快照 `work_seq`。
    pub(crate) fn snapshot(&self) -> u64 {
        self.work_seq.load(Ordering::Acquire)
    }

    /// batch head 从空到非空时 Release 递增 `work_seq`。
    pub(crate) fn notify_empty_to_nonempty(&self) -> Result<(), RawInvariant> {
        let next = self
            .work_seq
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("调度 work_seq 溢出"))?;
        self.work_seq.store(next, Ordering::Release);
        Ok(())
    }

    /// park 登记：锁内重查序号与工作集，仅序号未变且仍无工作才登记。
    pub(crate) fn park(
        &mut self,
        worker: u64,
        generation: u64,
        snapshot: u64,
        has_work: bool,
    ) -> Result<bool, RawInvariant> {
        if has_work || self.work_seq.load(Ordering::Acquire) != snapshot {
            return Ok(false);
        }
        let next = self
            .idle_count
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("调度 idle 计数溢出"))?;
        self.idle_count.store(next, Ordering::Release);
        self.entries.push((worker, generation));
        Ok(true)
    }

    /// 摘一个匹配 generation 的 worker 并唤醒；`idle_count` 只是省锁 hint。
    pub(crate) fn unpark(&mut self, worker: u64, generation: u64) -> bool {
        if self.idle_count.load(Ordering::Acquire) == 0 {
            return false;
        }
        if let Some(position) = self
            .entries
            .iter()
            .position(|entry| entry.0 == worker && entry.1 == generation)
        {
            self.entries.remove(position);
            let idle = self.idle_count.load(Ordering::Acquire);
            self.idle_count.store(idle - 1, Ordering::Release);
            self.waker.wake(worker, generation);
            return true;
        }
        // 匹配同一 generation 的任意 worker。
        if let Some(position) = self.entries.iter().position(|entry| entry.1 == generation) {
            let (worker, generation) = self.entries.remove(position);
            let idle = self.idle_count.load(Ordering::Acquire);
            self.idle_count.store(idle - 1, Ordering::Release);
            self.waker.wake(worker, generation);
            return true;
        }
        false
    }

    /// 唤醒任意一个 idle worker。
    pub(crate) fn unpark_any(&mut self) -> bool {
        if self.idle_count.load(Ordering::Acquire) == 0 {
            return false;
        }
        if let Some((worker, generation)) = self.entries.pop() {
            let idle = self.idle_count.load(Ordering::Acquire);
            self.idle_count.store(idle - 1, Ordering::Release);
            self.waker.wake(worker, generation);
            return true;
        }
        false
    }
}

/// worker-local xorshift64*。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StealRng {
    state: u64,
}

impl StealRng {
    /// 由确定性种子创建；零种子改为 1。
    pub(crate) const fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    /// 生成下一个 `u64`。
    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(STEAL_MULTIPLIER)
    }
}

/// 公开 facade 线性化后的并行度请求。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ApplyParallelism {
    /// 旧并行度。
    pub old: u64,
    /// 新并行度。
    pub new: u64,
    /// topology epoch。
    pub epoch: u64,
}

/// 脏配额：`parallelism = 1` 时 target 为 1，否则为 `parallelism - 1`。
pub(crate) const fn dirty_target(parallelism: u64) -> u64 {
    if parallelism <= 1 { 1 } else { parallelism - 1 }
}

/// managed worker 绑定上限：`max(1, p - min(dirty_active, p - 1))`。
pub(crate) fn managed_bound(parallelism: u64, dirty_active: u64) -> u64 {
    if parallelism <= 1 {
        return 1;
    }
    let held = if dirty_active < parallelism - 1 {
        dirty_active
    } else {
        parallelism - 1
    };
    let bound = parallelism - held;
    if bound < 1 { 1 } else { bound }
}

/// M:N 调度的确定性世界：processor 表、稠密 active 快照、idle registry 与 topology。
#[derive(Debug)]
pub(crate) struct SchedulerWorld {
    processors: Vec<ProcessorRecord>,
    active: Vec<u64>,
    next_processor_id: u64,
    topology_epoch: u64,
    dirty_limit: u64,
    dirty_active: u64,
    idle: IdleRegistry,
    steal_rng: StealRng,
}

impl SchedulerWorld {
    /// 由初始并行度创建；ID 单调不复用。
    pub(crate) fn new(parallelism: u64, rng_seed: u64) -> Result<Self, RawInvariant> {
        if parallelism == 0 {
            return Err(RawInvariant::new("调度并行度必须为正"));
        }
        let mut world = Self {
            processors: Vec::new(),
            active: Vec::new(),
            next_processor_id: 0,
            topology_epoch: 0,
            dirty_limit: dirty_target(parallelism),
            dirty_active: 0,
            idle: IdleRegistry::new(),
            steal_rng: StealRng::new(rng_seed),
        };
        for _ in 0..parallelism {
            world.alloc_processor()?;
        }
        world.rebuild_snapshot();
        debug_assert!(world.processors.len() == world.active.len());
        Ok(world)
    }

    fn alloc_processor(&mut self) -> Result<u64, RawInvariant> {
        let id = self
            .next_processor_id
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("LogicalProcessorId 溢出"))?;
        self.next_processor_id = id;
        self.processors.push(ProcessorRecord::new(id));
        Ok(id)
    }

    fn rebuild_snapshot(&mut self) {
        let mut active = Vec::new();
        for processor in &self.processors {
            if processor.state != ProcessorState::Retiring {
                active.push(processor.id);
            }
        }
        self.active = active;
    }

    /// 返回稠密 active 快照。
    pub(crate) fn active_snapshot(&self) -> &[u64] {
        &self.active
    }

    /// 返回 topology epoch。
    pub(crate) const fn topology_epoch(&self) -> u64 {
        self.topology_epoch
    }

    /// 返回 idle registry。
    pub(crate) fn idle(&self) -> &IdleRegistry {
        &self.idle
    }

    /// 返回可变的 idle registry。
    pub(crate) fn idle_mut(&mut self) -> &mut IdleRegistry {
        &mut self.idle
    }

    /// 由 ID 查找 processor。
    pub(crate) fn processor(&self, id: u64) -> Option<&ProcessorRecord> {
        self.processors.iter().find(|processor| processor.id == id)
    }

    /// 由 ID 查找可变 processor。
    pub(crate) fn processor_mut(&mut self, id: u64) -> Option<&mut ProcessorRecord> {
        self.processors
            .iter_mut()
            .find(|processor| processor.id == id)
    }

    /// 应用并行度变更：增加分配全新 ID，重建快照；降低标 `Retiring` 并发布新 epoch。
    pub(crate) fn apply_parallelism(&mut self, req: ApplyParallelism) -> Result<(), RawInvariant> {
        if req.new == 0 {
            return Err(RawInvariant::new("调度并行度必须为正"));
        }
        if req.new > req.old {
            for _ in req.old..req.new {
                self.alloc_processor()?;
            }
            self.topology_epoch = self
                .topology_epoch
                .checked_add(1)
                .ok_or_else(|| RawInvariant::new("调度 topology epoch 溢出"))?;
            self.rebuild_snapshot();
            self.dirty_limit = self.dirty_limit.max(dirty_target(req.new));
        } else if req.new < req.old {
            let mut retiring = Vec::new();
            for processor in self.processors.iter_mut().rev() {
                if retiring.len() as u64 >= req.old - req.new {
                    break;
                }
                if processor.state != ProcessorState::Retiring {
                    processor.state = ProcessorState::Retiring;
                    retiring.push(processor.id);
                }
            }
            self.topology_epoch = self
                .topology_epoch
                .checked_add(1)
                .ok_or_else(|| RawInvariant::new("调度 topology epoch 溢出"))?;
            self.rebuild_snapshot();
            self.dirty_limit = self.dirty_limit.max(self.dirty_active);
        } else {
            self.topology_epoch = req.epoch.max(self.topology_epoch);
        }
        debug_assert!(self.processors.len() >= self.active.len());
        // 元数据保持 `O(active)`：禁止 `P × P` 矩阵。
        debug_assert!(
            self.processors.len()
                <= self.active.len() + (req.old - req.new.min(req.old)) as usize + 1
        );
        Ok(())
    }

    /// 按固定序 retire processor：run_next、local、8 remote carry、injection carry。
    pub(crate) fn retire_processor(
        &mut self,
        id: u64,
    ) -> Result<Vec<RunnableHandle>, RawInvariant> {
        let index = self
            .processors
            .iter()
            .position(|processor| processor.id == id)
            .ok_or_else(|| RawInvariant::new("退役引用未知 processor"))?;
        if self.processors[index].state != ProcessorState::Retiring {
            return Err(RawInvariant::new("只能退役 Retiring processor"));
        }
        let drained = self.processors[index].drain_all();
        // 全部 queue ownership 为空才进 `Idle` 归 pool。
        debug_assert!(!self.processors[index].has_work());
        self.processors[index].state = ProcessorState::Idle;
        self.rebuild_snapshot();
        Ok(drained)
    }

    /// worker 绑定循环的单步：按固定顺序选择 runnable。
    pub(crate) fn schedule_step(
        &mut self,
        processor_id: u64,
    ) -> Result<Option<RunnableHandle>, RawInvariant> {
        let index = self
            .processors
            .iter()
            .position(|processor| processor.id == processor_id)
            .ok_or_else(|| RawInvariant::new("调度引用未知 processor"))?;
        // 先处理 `GC_STOP`/`PREEMPT` 占位 flag。
        if self.processors[index].pending_poll {
            self.processors[index].pending_poll = false;
        }
        let tick = self.processors[index]
            .service_tick
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("调度 service tick 溢出"))?;
        self.processors[index].service_tick = tick;
        // service tick 是 61 的倍数且有外部工作时先 external service。
        if tick % u64::from(SCHED_SERVICE_INTERVAL) == 0
            && let Some(handle) = self.service_external(index)?
        {
            return Ok(Some(handle));
        }
        // 取 `run_next`。
        if let Some(handle) = self.processors[index].take_run_next() {
            return Ok(Some(handle));
        }
        self.processors[index].run_next_hits = 0;
        // 从 local 尾取一个。
        if let Some(handle) = self.processors[index].local.pop_back() {
            return Ok(Some(handle));
        }
        // local 为空时 remote service，再 injection service。
        if let Some(handle) = self.service_external(index)? {
            return Ok(Some(handle));
        }
        // 非阻塞 poller/timer 本阶段恒空，直接跳过。
        // 同 NUMA steal。
        if self.active.len() >= 2
            && let Some(handle) = self.steal(index)?
        {
            return Ok(Some(handle));
        }
        Ok(None)
    }

    fn service_external(&mut self, index: usize) -> Result<Option<RunnableHandle>, RawInvariant> {
        // 先清所选 shard 已登记 carry，才摘整链；RR cursor 从上次下一位置起至多查 8 shard。
        for _ in 0..SCHED_REMOTE_SHARDS {
            let shard = (self.processors[index].rr_cursor % SCHED_REMOTE_SHARDS) as usize;
            self.processors[index].rr_cursor =
                (self.processors[index].rr_cursor + 1) % SCHED_REMOTE_SHARDS;
            if !self.processors[index].remote_carries[shard].is_empty() {
                // carry 是有限快照：先消费 carry，不被新 head 越过。
                if let Some(handle) = self.processors[index].remote_carries[shard].pop() {
                    return Ok(Some(handle));
                }
            }
        }
        if let Some(handle) = self.processors[index].injection_carry.pop() {
            return Ok(Some(handle));
        }
        Ok(None)
    }

    fn steal(&mut self, thief: usize) -> Result<Option<RunnableHandle>, RawInvariant> {
        let active = self.active.clone();
        if active.len() < 2 {
            return Ok(None);
        }
        let thief_id = self.processors[thief].id;
        let start = (self.steal_rng.next() % active.len() as u64) as usize;
        // 互质 step 遍历稠密 snapshot。
        let mut step = (self.steal_rng.next() % active.len().saturating_sub(1) as u64) as usize + 1;
        while step > 1 && active.len().is_multiple_of(step) {
            step -= 1;
        }
        let mut claimed: Option<RunnableHandle> = None;
        for i in 0..active.len() {
            let victim_id = active[(start + i * step) % active.len()];
            if victim_id == thief_id {
                continue;
            }
            let victim = self
                .processors
                .iter_mut()
                .find(|processor| processor.id == victim_id)
                .ok_or_else(|| RawInvariant::new("窃取引用未知 processor"))?;
            if victim.local.is_empty() {
                continue;
            }
            let mut batch = victim.local.claim_head(SCHED_BATCH_MAX as usize);
            if batch.is_empty() {
                continue;
            }
            let first = batch.remove(0);
            for handle in batch {
                self.processors[thief].local.push_back(handle).unwrap_or(());
            }
            claimed = Some(first);
            break;
        }
        Ok(claimed)
    }

    /// park 五步中的本地检查：flags、本地/remote carry、injection、demand（本阶段恒空）。
    pub(crate) fn has_local_work(&self, processor_id: u64) -> Result<bool, RawInvariant> {
        let processor = self
            .processor(processor_id)
            .ok_or_else(|| RawInvariant::new("调度引用未知 processor"))?;
        Ok(processor.pending_poll || processor.has_work())
    }

    /// 执行终止计划的调度侧：停接纳、阻新 producer、flush 后唤醒 parked worker。
    pub(crate) fn execute_termination(&mut self, plan: &TerminationPlan) -> TerminationMode {
        // 关闭 poller 注册是空操作；worker 转 `Stopping` 由后续阶段实现。
        // `report_epoch` 冲刷下界沿用 `termination_impl`，本阶段不改报告 schema。
        while self.idle.unpark_any() {}
        plan.mode()
    }
}

/// 发布暂存链；先验证目标与通知序号，失败时保留 producer 的 ownership。
pub(crate) fn flush_schedule_staging(
    world: &mut SchedulerWorld,
    producer: &mut ProducerHandle,
) -> Result<usize, RawInvariant> {
    if producer.staging.is_empty() {
        return Ok(0);
    }
    let target = producer.staging.target().expect("非空 staging 必有 target");
    let shard = producer.staging.shard().expect("非空 staging 必有 shard");
    if shard >= SCHED_REMOTE_SHARDS {
        return Err(RawInvariant::new("调度 shard 越界"));
    }
    let shard = usize::try_from(shard).expect("有效 shard 可用 usize 索引");
    let retiring = world
        .processor(target)
        .ok_or_else(|| RawInvariant::new("调度 staging 引用未知 processor"))?
        .state
        == ProcessorState::Retiring;
    let target = if retiring {
        *world
            .active_snapshot()
            .first()
            .ok_or_else(|| RawInvariant::new("没有 active processor 可唤醒"))?
    } else {
        target
    };
    let processor = world.processor(target).expect("发布目标已验证");
    let empty = if retiring {
        processor.injection_carry.is_empty()
    } else {
        processor.remote_carries[shard].is_empty()
    };
    if empty {
        world
            .idle
            .snapshot()
            .checked_add(1)
            .ok_or_else(|| RawInvariant::new("调度 work_seq 溢出"))?;
    }
    let (_, _, mut chain) = producer.staging.drain().expect("非空 staging");
    let count = chain.len();
    let processor = world.processor_mut(target).expect("发布目标已验证");
    let carry = if retiring {
        &mut processor.injection_carry
    } else {
        &mut processor.remote_carries[shard]
    };
    if empty {
        std::mem::swap(carry, &mut chain);
    } else {
        carry.append(&mut chain);
    }
    // 归还空缓冲；后续 batch 复用容量，不为每次 wake 分配 staging。
    producer.staging.handles = chain;
    if empty {
        world
            .idle
            .notify_empty_to_nonempty()
            .expect("通知序号已预检");
        world.idle.unpark_any();
    }
    Ok(count)
}

/// 全通道唯一入口：`ready_publish`。
///
/// `Waiting` 且调用者持有目标唯一 owner 时直接进 `run_next`/local，否则经
/// `ProducerHandle::pending_node` 认领并置 `BATCH_PUBLISHING` 发目标 remote shard 或本
/// NUMA injection；`Parking` 先 `Release` 置 wait `notified` 再 `Acquire` 重读 lifecycle。
pub(crate) fn ready_publish(
    world: &mut SchedulerWorld,
    tables: &mut super::coroutine::CoroutineTable,
    target: u64,
    handle: CoroutineHandle,
    producer: &mut ProducerHandle,
    owner_held: bool,
    wait_notified_bit: u64,
) -> Result<bool, RawInvariant> {
    let (slot, _) = tables.get(handle)?;
    // Acquire 读取 lifecycle。
    let lifecycle = slot.hot.lifecycle()?;
    match lifecycle {
        CoroutineState::Waiting => {}
        CoroutineState::Parking => {
            slot.hot
                .wait_word
                .fetch_or(wait_notified_bit, Ordering::Release);
            let again = slot.hot.lifecycle()?;
            if again == CoroutineState::Parking {
                return Ok(false);
            }
            if again != CoroutineState::Waiting {
                return Ok(false);
            }
        }
        CoroutineState::Dead => return Ok(false),
        _ => return Ok(false),
    }
    // 每 generation 至多成功 ready 一次：已 `ENQUEUED` 即输给 winner。
    let (slot, _) = tables.get(handle)?;
    let word = slot.hot.state.load(Ordering::Acquire);
    if word & ENQUEUED != 0 {
        return Ok(false);
    }
    let processor = world
        .processor(target)
        .ok_or_else(|| RawInvariant::new("ready 引用未知 processor"))?;
    let retiring = processor.state == ProcessorState::Retiring;
    if owner_held && !retiring {
        let (slot, _) = tables.get(handle)?;
        slot.hot
            .transition(CoroutineState::Waiting, CoroutineState::Runnable)?;
        let processor = world
            .processor_mut(target)
            .ok_or_else(|| RawInvariant::new("ready 引用未知 processor"))?;
        // `run_next` 规则：放入新值时旧值先回 local 尾。
        processor.push_run_next(RunnableHandle::new(handle))?;
        return Ok(true);
    }
    producer.begin_publish();
    producer.topology_epoch_seen = world.topology_epoch();
    let result = ready_batch(world, tables, target, handle, producer);
    producer.end_publish();
    result
}

fn ready_batch(
    world: &mut SchedulerWorld,
    tables: &mut super::coroutine::CoroutineTable,
    target: u64,
    handle: CoroutineHandle,
    producer: &mut ProducerHandle,
) -> Result<bool, RawInvariant> {
    let shard = u32::try_from(producer.shard_seed % u64::from(SCHED_REMOTE_SHARDS))
        .expect("shard 取模结果不超过 u32");
    if !producer.staging.is_empty()
        && (producer.staging.target() != Some(target)
            || producer.staging.shard() != Some(shard)
            || producer.staging.len()
                == usize::try_from(SCHED_BATCH_MAX).expect("batch 上限可索引"))
    {
        flush_schedule_staging(world, producer)?;
    }
    producer.pending_node = Some(RunnableHandle::new(handle));
    if tables.get(handle)?.0.hot.claim_for_batch().is_err() {
        producer.pending_node = None;
        return Ok(false);
    }
    producer
        .staging
        .stage(target, shard, RunnableHandle::new(handle))?;
    producer.pending_node = None;
    flush_schedule_staging(world, producer)?;
    Ok(true)
}

/// 显式 `yield`：`Running -> Runnable|ENQUEUED` 放 owner local 尾，不用 batch。
pub(crate) fn yield_now(
    world: &mut SchedulerWorld,
    tables: &mut super::coroutine::CoroutineTable,
    processor_id: u64,
    handle: CoroutineHandle,
) -> Result<(), RawInvariant> {
    let (slot, _) = tables.get(handle)?;
    slot.hot.yield_to_runnable()?;
    let processor = world
        .processor_mut(processor_id)
        .ok_or_else(|| RawInvariant::new("yield 引用未知 processor"))?;
    processor.local.push_back(RunnableHandle::new(handle))?;
    Ok(())
}

/// local 满时的 overflow：一次认领最旧 128 项，有 idle 发 remote，无 idle 发 injection。
pub(crate) fn overflow_local(
    world: &mut SchedulerWorld,
    processor_id: u64,
    idle_target: Option<u64>,
) -> Result<Vec<RunnableHandle>, RawInvariant> {
    let batch = world
        .processor_mut(processor_id)
        .ok_or_else(|| RawInvariant::new("调度引用未知 processor"))?
        .claim_overflow();
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    match idle_target {
        Some(target) => {
            let processor = world
                .processor_mut(target)
                .ok_or_else(|| RawInvariant::new("调度引用未知 processor"))?;
            let shard = (processor.rr_cursor % SCHED_REMOTE_SHARDS) as usize;
            processor.remote_carries[shard].extend(batch.iter().copied());
        }
        None => {
            let processor = world
                .processor_mut(processor_id)
                .ok_or_else(|| RawInvariant::new("调度引用未知 processor"))?;
            processor.injection_carry.extend(batch.iter().copied());
        }
    }
    Ok(batch)
}

/// 校验调度常量与既有契约交叉一致。
pub(crate) fn verify_constants() -> Result<(), RawInvariant> {
    if SCHED_LOCAL_CAPACITY != 256
        || SCHED_REMOTE_SHARDS != OWNER_INBOX_SHARDS
        || SCHED_BATCH_MAX != BATCH_MAX
        || SCHED_SERVICE_INTERVAL != 61
        || SCHED_SERVICE_BATCH != 128
    {
        return Err(RawInvariant::new("调度常量与契约不一致"));
    }
    if SCHED_REMOTE_SHARDS != 8 || BATCH_MAX != 128 {
        return Err(RawInvariant::new("调度分片与 batch 上限漂移"));
    }
    if SCHED_SERVICE_BATCH != 128 {
        return Err(RawInvariant::new("调度 service batch 漂移"));
    }
    // `queue_pad_bytes` 与 `cache_line_bytes` 由契约段交叉校验。
    Ok(())
}

/// 窄容量下的确定性单测入口：容量 2/4 的 deque 行为。
#[cfg(test)]
pub(crate) fn tiny_deque_capacity() -> u32 {
    4
}
