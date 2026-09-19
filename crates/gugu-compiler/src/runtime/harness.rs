//! owner-directed return 的进程内可运行切片：真实多 producer、单 owner consumer。
//!
//! 这个 harness 与确定性测试共用同一份 inbox、node pool、link 编码与账本实现：producer
//! 线程只接触自己的 staging、node pool 与目标 inbox，owner 本地的 descriptor、free
//! structure 与账本只由 consumer 线程读写。它不进入默认测试套件，供 `cargo bench` 与手工
//! 验证使用。

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use super::inbox::ShardIndex;
use super::local_heap::{BlockRef, ManagedBlockId};
use super::message::{
    BatchLimits, FlushTrigger, MessageState, ProducerStaging, ReturnKind, ReturnMessage,
    stage_message,
};
use super::slab::Epoch;
use super::slab::{OwnerToken, RawInvariant, RawSlot};
use super::world::{RawWorld, ResourceShape};
use super::{
    BATCH_MAX, OWNER_INBOX_SHARDS, RawPlaneDemand, RawPlanePolicyV1, RuntimeRawContractV1,
    SchedulerDemand, inbox::ServiceBudget, size_class::RuntimeSizeClassId,
};

/// harness 一轮运行的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HarnessReport {
    /// producer 线程数。
    pub producers: u32,
    /// 每个 producer 发布的 item 数。
    pub items_per_producer: u32,
    /// 发布的 item 总数。
    pub published_items: u64,
    /// consumer 消费的 item 总数。
    pub consumed_items: u64,
    /// consumer 观察到的 phantom null 次数。
    pub phantom_nulls: u64,
    /// 从平台预留并 commit 的 span range 数量。
    pub span_ranges: u32,
    /// 契约登记的 class 数量。
    pub size_classes: u32,
    /// exactly-once 与账本不变量是否保持。
    pub invariants_hold: bool,
    /// 运行耗时（微秒）。
    pub elapsed_micros: u64,
}

/// 真实并发的 owner return harness。
#[derive(Clone, Copy, Debug)]
pub struct OwnerReturnHarness {
    producers: u32,
    items_per_producer: u32,
}

impl OwnerReturnHarness {
    /// 创建 harness；每个 producer 至少发布一个 item。
    pub fn new(producers: u32, items_per_producer: u32) -> Self {
        Self {
            producers: producers.max(1),
            items_per_producer: items_per_producer.max(1),
        }
    }

    /// 运行一轮并发 return，并校验 exactly-once 与账本不变量。
    pub fn run(self) -> HarnessReport {
        let policy = RawPlanePolicyV1::default();
        let contract = RuntimeRawContractV1::build(
            super::super::TargetName::X86_64Linux,
            policy,
            RawPlaneDemand::default(),
            super::RawResourceDemand::default(),
            super::Rt0Demand::default(),
            SchedulerDemand::default(),
            super::WaitDemand::default(),
            super::SyncDemand::default(),
            super::StackMapDemand::default(),
            super::GcMetadataDemand::empty(),
            super::BarrierDemand::default(),
            super::GcPacingDemand::default(),
            super::MarkDemand::default(),
            super::LocalHeapDemand::default(),
            super::CompressionDemand::default(),
            super::PlatformProfile::from(super::super::TargetName::X86_64Linux),
        )
        .expect("runtime raw 契约可构建");
        let node_capacity = contract
            .message_node_capacity()
            .max(self.producers * self.items_per_producer + BATCH_MAX);
        let mut world =
            RawWorld::new(1, 1, node_capacity, BatchLimits::default()).expect("raw world 可创建");
        // region plane 由同一份契约配置：容量阶梯与对象上界只有一个来源。
        world
            .configure_regions(contract.region())
            .expect("region plane 可配置");
        let target = world.token(0);
        let inbox = world.inbox(0);
        let pool = world.pool();
        let secret = *world.integrity_secret();
        let class = RuntimeSizeClassId::from_raw(0);
        let stride = world
            .classes()
            .get(class)
            .expect("0 号 class 必须已登记")
            .slot_stride;

        let mut assignments = Vec::new();
        for producer in 0..self.producers {
            let mut slots = Vec::with_capacity(self.items_per_producer as usize);
            for _ in 0..self.items_per_producer {
                let allocation = world.allocate(0, class).expect("pre-allocation 必须成功");
                world
                    .queue_return(0, allocation.slot, u64::from(stride))
                    .expect("record 完成必须赢得唯一 return 点");
                slots.push(allocation.slot);
            }
            assignments.push(slots);
            let _ = producer;
        }

        let running = Arc::new(AtomicBool::new(true));
        let published = Arc::new(AtomicU64::new(0));
        let start = Instant::now();
        let mut handles = Vec::with_capacity(self.producers as usize);
        for (producer, slots) in assignments.into_iter().enumerate() {
            let inbox = Arc::clone(&inbox);
            let pool = Arc::clone(&pool);
            let published = Arc::clone(&published);
            let running = Arc::clone(&running);
            let shard = ShardIndex::from_raw((producer as u32) % OWNER_INBOX_SHARDS)
                .expect("shard 编号合法");
            let epoch = world.epoch();
            handles.push(thread::spawn(move || {
                let mut staging = ProducerStaging::new(BatchLimits::default());
                let mut count = 0_u64;
                for (index, slot) in slots.iter().enumerate() {
                    let message = build_message(target, secret, *slot, stride, epoch, index as u32);
                    let forced = (index as u32 + 1)
                        .is_multiple_of(BATCH_MAX)
                        .then_some(FlushTrigger::ItemLimit);
                    match stage_message(&pool, Some(&inbox), &mut staging, &message, shard, forced)
                    {
                        Ok(_) => count += 1,
                        Err(error) => {
                            eprintln!("owner-return harness: 发布失败：{}", error.message());
                            running.store(false, Ordering::Release);
                            return count;
                        }
                    }
                }
                if let Err(error) = super::message::flush_staging(
                    &pool,
                    &inbox,
                    &mut staging,
                    FlushTrigger::ProducerStopping,
                ) {
                    eprintln!("owner-return harness: 收尾发布失败：{}", error.message());
                    running.store(false, Ordering::Release);
                }
                let _ = running;
                published.fetch_add(count, Ordering::Relaxed);
                count
            }));
        }

        let budget = policy.service_budget();
        let mut consumed = 0_u64;
        let mut clean = true;
        let mut idle = 0_u32;
        while handles.iter().any(|handle| !handle.is_finished()) || idle < 4 {
            let mut serviced = 0_u64;
            for index in 0..OWNER_INBOX_SHARDS {
                let shard = ShardIndex::from_raw(index).expect("shard 编号合法");
                match world.service(0, shard, &budget) {
                    Ok(report) => {
                        serviced += u64::from(report.items);
                        consumed += u64::from(report.items - report.forwarded);
                    }
                    Err(error) => {
                        eprintln!("owner-return harness: service 失败：{}", error.message());
                        clean = false;
                        break;
                    }
                }
            }
            if serviced == 0 {
                idle += 1;
                thread::yield_now();
            } else {
                idle = 0;
            }
            if !clean {
                break;
            }
        }
        for handle in handles {
            if handle.join().is_err() {
                clean = false;
            }
        }
        if world.release_graced_nodes().is_err() {
            clean = false;
        }
        let elapsed = start.elapsed();
        let published_items = published.load(Ordering::Acquire);
        if world.ledger_invariant(0).is_err() || world.verify_links().is_err() {
            clean = false;
        }
        let phantom_nulls = (0..OWNER_INBOX_SHARDS)
            .map(|index| {
                world
                    .inbox(0)
                    .stats(ShardIndex::from_raw(index).expect("shard 编号合法"))
                    .phantom_nulls()
            })
            .sum();
        HarnessReport {
            producers: self.producers,
            items_per_producer: self.items_per_producer,
            published_items,
            consumed_items: consumed,
            phantom_nulls,
            span_ranges: world.provider_ranges().len() as u32,
            size_classes: contract.class_count(),
            invariants_hold: clean
                && consumed == u64::from(self.producers) * u64::from(self.items_per_producer),
            elapsed_micros: u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
        }
    }
}

/// 资源 lease burst 一轮运行的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceReleaseReport {
    /// producer 线程数。
    pub producers: u32,
    /// 每个 producer 发布的消息数。
    pub items_per_producer: u32,
    /// 构造的 release 消息总数。
    pub released_items: u64,
    /// owner 消费的 item 总数。
    pub consumed_items: u64,
    /// 受限 cleanup 次数。
    pub cleanups: u64,
    /// consumer 观察到的 phantom null 次数。
    pub phantom_nulls: u64,
    /// exactly-once 与账本不变量是否保持。
    pub invariants_hold: bool,
    /// 运行耗时（微秒）。
    pub elapsed_micros: u64,
}

/// ResourceRelease 消息的真实并发发布 harness：多 producer 发布，单 owner 消费。
/// lease 与受限 cleanup 在计时前构造，计时只覆盖消息发布、消费与 slot 回收。
#[derive(Clone, Copy, Debug)]
pub struct ResourceReleaseHarness {
    producers: u32,
    items_per_producer: u32,
}

impl ResourceReleaseHarness {
    /// 创建 harness；每个 producer 至少发布一个 item。
    pub fn new(producers: u32, items_per_producer: u32) -> Self {
        Self {
            producers: producers.max(1),
            items_per_producer: items_per_producer.max(1),
        }
    }

    /// 运行一轮 ResourceRelease 消息发布与消费，并校验 exactly-once cleanup 与账本不变量。
    pub fn run(self) -> ResourceReleaseReport {
        let policy = RawPlanePolicyV1::default();
        let contract = RuntimeRawContractV1::build(
            super::super::TargetName::X86_64Linux,
            policy,
            RawPlaneDemand::default(),
            super::RawResourceDemand::default(),
            super::Rt0Demand::default(),
            SchedulerDemand::default(),
            super::WaitDemand::default(),
            super::SyncDemand::default(),
            super::StackMapDemand::default(),
            super::GcMetadataDemand::empty(),
            super::BarrierDemand::default(),
            super::GcPacingDemand::default(),
            super::MarkDemand::default(),
            super::LocalHeapDemand::default(),
            super::CompressionDemand::default(),
            super::PlatformProfile::from(super::super::TargetName::X86_64Linux),
        )
        .expect("runtime raw 契约可构建");
        let total = self.producers * self.items_per_producer;
        let node_capacity = contract.message_node_capacity().max(total + BATCH_MAX);
        let mut world =
            RawWorld::new(7, 2, node_capacity, BatchLimits::default()).expect("raw world 可创建");
        world
            .configure_regions(contract.region())
            .expect("region plane 可配置");
        let inbox = world.inbox(0);
        let pool = world.pool();
        let shape = ResourceShape {
            kind_id: 0,
            payload_bytes: 8,
            alignment: 8,
        };
        let mut messages = Vec::with_capacity(total as usize);
        for _ in 0..total {
            let handle = world.allocate_resource(0, shape).expect("资源分配成功");
            messages.push(
                world
                    .prepare_foreign_release(1, handle)
                    .expect("release 消息可构造"),
            );
        }

        let start = Instant::now();
        let mut assignments: Vec<Vec<ReturnMessage>> = vec![Vec::new(); self.producers as usize];
        for (index, message) in messages.into_iter().enumerate() {
            assignments[index % self.producers as usize].push(message);
        }
        let running = Arc::new(AtomicBool::new(true));
        let published = Arc::new(AtomicU64::new(0));
        let mut producers = Vec::with_capacity(self.producers as usize);
        for (producer, batch) in assignments.into_iter().enumerate() {
            let inbox = Arc::clone(&inbox);
            let pool = Arc::clone(&pool);
            let published = Arc::clone(&published);
            let running = Arc::clone(&running);
            let shard = ShardIndex::from_raw((producer as u32) % OWNER_INBOX_SHARDS)
                .expect("shard 编号合法");
            producers.push(thread::spawn(move || {
                let mut staging = ProducerStaging::new(BatchLimits::default());
                let mut count = 0_u64;
                for (index, message) in batch.iter().enumerate() {
                    let forced = (index as u32 + 1)
                        .is_multiple_of(BATCH_MAX)
                        .then_some(FlushTrigger::ItemLimit);
                    match stage_message(&pool, Some(&inbox), &mut staging, message, shard, forced) {
                        Ok(_) => count += 1,
                        Err(error) => {
                            eprintln!("resource-release harness: 发布失败：{}", error.message());
                            running.store(false, Ordering::Release);
                            return count;
                        }
                    }
                }
                if let Err(error) = super::message::flush_staging(
                    &pool,
                    &inbox,
                    &mut staging,
                    FlushTrigger::ProducerStopping,
                ) {
                    eprintln!(
                        "resource-release harness: 收尾发布失败：{}",
                        error.message()
                    );
                    running.store(false, Ordering::Release);
                }
                let _ = running;
                published.fetch_add(count, Ordering::Release);
                count
            }));
        }

        let budget = policy.service_budget();
        let mut consumed = 0_u64;
        let mut clean = true;
        let mut idle = 0_u32;
        while running.load(Ordering::Acquire)
            && (producers.iter().any(|handle| !handle.is_finished())
                || published.load(Ordering::Acquire) < u64::from(total)
                || (consumed < u64::from(total) && idle < 1024)
                || idle < 4)
        {
            let mut serviced = 0_u64;
            for index in 0..OWNER_INBOX_SHARDS {
                let shard = ShardIndex::from_raw(index).expect("shard 编号合法");
                match world.service(0, shard, &budget) {
                    Ok(report) => serviced += u64::from(report.items),
                    Err(error) => {
                        eprintln!(
                            "resource-release harness: service 失败：{}",
                            error.message()
                        );
                        clean = false;
                        break;
                    }
                }
            }
            consumed += serviced;
            if serviced == 0 {
                idle += 1;
                thread::yield_now();
            } else {
                idle = 0;
            }
            if !clean {
                break;
            }
        }
        for handle in producers {
            if handle.join().is_err() {
                clean = false;
            }
        }
        if published.load(Ordering::Acquire) != u64::from(total) {
            clean = false;
        }
        if world.release_graced_nodes().is_err()
            || world.verify_resource_cells().is_err()
            || world.resource_ledger_invariant(0).is_err()
            || world.verify_links().is_err()
        {
            clean = false;
        }
        let elapsed = start.elapsed();
        let cleanups = world.resource_cleanups();
        let phantom_nulls = (0..OWNER_INBOX_SHARDS)
            .map(|index| {
                world
                    .inbox(0)
                    .stats(ShardIndex::from_raw(index).expect("shard 编号合法"))
                    .phantom_nulls()
            })
            .sum();
        ResourceReleaseReport {
            producers: self.producers,
            items_per_producer: self.items_per_producer,
            released_items: u64::from(total),
            consumed_items: consumed,
            cleanups,
            phantom_nulls,
            invariants_hold: clean
                && consumed == u64::from(total)
                && cleanups == u64::from(total)
                && world.pending_release_requests() == 0,
            elapsed_micros: u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
        }
    }
}

fn build_message(
    target: OwnerToken,
    secret: [u8; 32],
    slot: RawSlot,
    stride: u32,
    epoch: Epoch,
    sequence: u32,
) -> ReturnMessage {
    let mut message = ReturnMessage {
        next: None,
        target,
        kind: ReturnKind::RawSlot,
        descriptor: slot.descriptor,
        unit: slot.index,
        bytes: stride,
        source_epoch: epoch,
        state: MessageState::Staged,
        integrity: super::message::IntegrityTag {
            generation: slot.generation,
            class: RuntimeSizeClassId::from_raw(0),
            owner_id: target.owner_id,
            route_key: target.route_key,
            checksum: 0,
        },
    };
    let _ = sequence;
    message.integrity.checksum = super::message::IntegrityTag::compute(&secret, &message);
    message
}

/// channel ping-pong 与 select 提交的真实并发冒烟结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelWaitReport {
    /// 发送成功次数。
    pub sent: u64,
    /// 接收成功次数。
    pub received: u64,
    /// select 提交次数。
    pub select_commits: u64,
    /// 去重后的 payload 数。
    pub unique: u64,
    /// 耗时微秒。
    pub elapsed_micros: u64,
    /// exactly-once 与账本是否成立。
    pub invariants_hold: bool,
}

/// 多 producer ping-pong 与 select 提交；只校验守恒，不针对吞吐作弊。
///
/// `RawWorld` 含协程槽裸指针，不能跨线程移动。producer/consumer 只发送 `Send` 请求，
/// owner 线程在本地创建世界并串行执行 `try_send`/`try_recv`/`select`。
#[derive(Clone, Copy, Debug)]
pub struct ChannelWaitHarness {
    producers: u32,
    items: u32,
}

impl ChannelWaitHarness {
    /// 创建 harness；每个 producer 至少发送一个 item。
    pub fn new(producers: u32, items: u32) -> Self {
        Self {
            producers: producers.max(1),
            items: items.max(1),
        }
    }

    /// 运行一轮真实线程 ping-pong 与 select 提交。
    pub fn run(self) -> ChannelWaitReport {
        let start = Instant::now();
        let (tx, rx) = mpsc::channel();
        let owner = thread::spawn(move || run_wait_owner(rx, self.producers, self.items));
        let joins = spawn_wait_workers(tx.clone(), self.producers, self.items);
        drop(tx);
        let mut clean = true;
        for join in joins {
            clean &= join.join().is_ok();
        }
        let stats = match owner.join() {
            Ok(stats) => stats,
            Err(_) => {
                return ChannelWaitReport {
                    sent: 0,
                    received: 0,
                    select_commits: 0,
                    unique: 0,
                    elapsed_micros: elapsed_micros(start),
                    invariants_hold: false,
                };
            }
        };
        let total = u64::from(self.producers * self.items);
        ChannelWaitReport {
            sent: stats.sent,
            received: stats.received,
            select_commits: stats.select_commits,
            unique: stats.unique,
            elapsed_micros: elapsed_micros(start),
            invariants_hold: clean
                && stats.sent == total
                && stats.received == total
                && stats.unique == total
                && stats.select_commits == u64::from(self.items)
                && stats.ledger,
        }
    }
}

#[derive(Clone, Copy)]
enum WaitOp {
    Send(u64),
    Recv,
}

enum WaitReply {
    Sent,
    Received,
    Retry,
    Failed,
}

struct OwnerStats {
    sent: u64,
    received: u64,
    unique: u64,
    select_commits: u64,
    ledger: bool,
}

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn spawn_wait_workers(
    tx: mpsc::Sender<(WaitOp, mpsc::Sender<WaitReply>)>,
    producers: u32,
    items: u32,
) -> Vec<thread::JoinHandle<()>> {
    let total = u64::from(producers * items);
    let received = Arc::new(AtomicU64::new(0));
    let mut joins = Vec::with_capacity((producers * 2) as usize);
    for producer in 0..producers {
        let tx = tx.clone();
        joins.push(thread::spawn(move || pump_sends(&tx, producer, items)));
    }
    for _ in 0..producers {
        let tx = tx.clone();
        let received = Arc::clone(&received);
        joins.push(thread::spawn(move || pump_recvs(&tx, &received, total)));
    }
    joins
}

fn pump_sends(tx: &mpsc::Sender<(WaitOp, mpsc::Sender<WaitReply>)>, producer: u32, items: u32) {
    let base = u64::from(producer) * u64::from(items);
    for offset in 0..items {
        let payload = base + u64::from(offset) + 1;
        loop {
            match submit_wait(tx, WaitOp::Send(payload)) {
                WaitReply::Sent => break,
                WaitReply::Retry => thread::yield_now(),
                WaitReply::Received | WaitReply::Failed => return,
            }
        }
    }
}

fn pump_recvs(
    tx: &mpsc::Sender<(WaitOp, mpsc::Sender<WaitReply>)>,
    received: &AtomicU64,
    total: u64,
) {
    while received.load(Ordering::Acquire) < total {
        match submit_wait(tx, WaitOp::Recv) {
            WaitReply::Received => {
                received.fetch_add(1, Ordering::AcqRel);
            }
            WaitReply::Retry => thread::yield_now(),
            WaitReply::Sent | WaitReply::Failed => return,
        }
    }
}

fn submit_wait(tx: &mpsc::Sender<(WaitOp, mpsc::Sender<WaitReply>)>, op: WaitOp) -> WaitReply {
    let (reply_tx, reply_rx) = mpsc::channel();
    if tx.send((op, reply_tx)).is_err() {
        return WaitReply::Failed;
    }
    reply_rx.recv().unwrap_or(WaitReply::Failed)
}

fn run_wait_owner(
    rx: mpsc::Receiver<(WaitOp, mpsc::Sender<WaitReply>)>,
    producers: u32,
    items: u32,
) -> OwnerStats {
    use super::select::{SelectCase, SelectOp, SelectOutcome};

    let (mut world, channel, ready, selector) = boot_wait_world(producers);
    let mut unique = HashSet::new();
    let mut stats = OwnerStats {
        sent: 0,
        received: 0,
        unique: 0,
        select_commits: 0,
        ledger: false,
    };
    while let Ok((op, reply)) = rx.recv() {
        let message = apply_wait_op(&mut world, channel, op, &mut unique, &mut stats);
        let _ = reply.send(message);
    }
    stats.unique = unique.len() as u64;
    let cases = [SelectCase {
        op: SelectOp::Recv { channel: ready },
        index: 0,
    }];
    for item in 0..items {
        let payload = u64::from(item);
        if world.channel_try_send(ready, payload).ok() == Some(Ok(()))
            && world.select(selector, &cases, true).ok() == Some(SelectOutcome::Case(0))
            && world.take_wait_result(selector).ok()
                == Some(Some(super::wait::WaitResult::Recv(payload)))
            && world.channel_try_recv(ready).ok() == Some(Err(super::channel::TryRecvErr::Empty))
        {
            stats.select_commits += 1;
        }
    }
    stats.ledger = world.ledger_invariant(0).is_ok();
    stats
}

fn boot_wait_world(
    producers: u32,
) -> (
    RawWorld,
    super::channel::ChannelHandle,
    super::channel::ChannelHandle,
    super::coroutine::CoroutineHandle,
) {
    use super::world::coroutine_impl::CoroutineEntry;

    let mut world = RawWorld::new(11, 1, BATCH_MAX * 4, BatchLimits::default()).expect("world");
    world
        .boot(
            vec![],
            vec![("GUGU_RUNTIME_STACK_MAX".to_owned(), "64KiB".to_owned())],
            "/".to_owned(),
            producers,
            CoroutineEntry {
                pc: 0x1000,
                required_frame: 64,
            },
        )
        .expect("boot");
    let channel = world.channel_new(i64::from(producers)).expect("channel");
    let ready = world.channel_new(1).expect("select 源");
    let selector = world
        .spawn_user_coroutine(
            0,
            CoroutineEntry {
                pc: 0x2000,
                required_frame: 64,
            },
        )
        .expect("selector")
        .expect("接纳");
    world.enter_coroutine(selector).expect("切入");
    (world, channel, ready, selector)
}

fn apply_wait_op(
    world: &mut RawWorld,
    channel: super::channel::ChannelHandle,
    op: WaitOp,
    unique: &mut HashSet<u64>,
    stats: &mut OwnerStats,
) -> WaitReply {
    match op {
        WaitOp::Send(payload) => match world.channel_try_send(channel, payload) {
            Ok(Ok(())) => {
                stats.sent += 1;
                WaitReply::Sent
            }
            Ok(Err(_)) => WaitReply::Retry,
            Err(_) => WaitReply::Failed,
        },
        WaitOp::Recv => match world.channel_try_recv(channel) {
            Ok(Ok(payload)) => {
                unique.insert(payload);
                stats.received += 1;
                WaitReply::Received
            }
            Ok(Err(_)) => WaitReply::Retry,
            Err(_) => WaitReply::Failed,
        },
    }
}

/// std.sync 互斥锁与原子状态机多线程争用 harness。
#[derive(Clone, Copy, Debug)]
pub struct SyncLockHarness {
    threads: u32,
    iterations: u32,
}

/// SyncLockHarness 运行报告。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncLockReport {
    /// 工作线程数。
    pub threads: u32,
    /// 每线程迭代次数。
    pub iterations: u32,
    /// 执行的同步操作总数。
    pub total_operations: u64,
    /// 最终累计计数器。
    pub final_counter: u64,
    /// 运行耗时（微秒）。
    pub elapsed_micros: u64,
    /// 不变量是否守恒。
    pub invariants_hold: bool,
}

impl SyncLockHarness {
    /// 创建 harness。
    pub fn new(threads: u32, iterations: u32) -> Self {
        Self {
            threads: threads.max(1),
            iterations: iterations.max(1),
        }
    }

    /// 执行一轮真实并发争用测试。
    pub fn run(self) -> SyncLockReport {
        let start = Instant::now();
        let (tx, rx) = mpsc::channel();
        let owner = thread::spawn(move || run_sync_owner(rx, self.threads, self.iterations));
        let mut joins = Vec::with_capacity(self.threads as usize);
        for thread_id in 0..self.threads {
            let tx = tx.clone();
            let iters = self.iterations;
            joins.push(thread::spawn(move || {
                for _ in 0..iters {
                    loop {
                        let (reply_tx, reply_rx) = mpsc::channel();
                        if tx.send((thread_id, reply_tx)).is_err() {
                            break;
                        }
                        if reply_rx.recv().unwrap_or(false) {
                            break;
                        }
                        thread::yield_now();
                    }
                }
            }));
        }
        drop(tx);
        let mut clean = true;
        for join in joins {
            clean &= join.join().is_ok();
        }
        let (final_count, invariants_hold) = owner.join().unwrap_or((0, false));
        let expected = u64::from(self.threads * self.iterations);
        SyncLockReport {
            threads: self.threads,
            iterations: self.iterations,
            total_operations: expected,
            final_counter: final_count,
            elapsed_micros: elapsed_micros(start),
            invariants_hold: clean && invariants_hold && final_count == expected,
        }
    }
}

fn run_sync_owner(
    rx: mpsc::Receiver<(u32, mpsc::Sender<bool>)>,
    threads: u32,
    iterations: u32,
) -> (u64, bool) {
    use super::sync::{MemoryOrdering, MutexLockOutcome};
    use super::world::coroutine_impl::CoroutineEntry;

    let mut world = RawWorld::new(7, 2, 64, BatchLimits::default()).expect("world");
    world
        .boot(
            vec![],
            vec![("GUGU_RUNTIME_STACK_MAX".to_owned(), "64KiB".to_owned())],
            "/".to_owned(),
            2,
            CoroutineEntry {
                pc: 0x1000,
                required_frame: 64,
            },
        )
        .expect("boot");
    let mutex = world.mutex_new();
    let mut atomic = super::sync::AtomicStateMachine::new(0);
    let mut counter = 0_u64;

    let mut coroutines = Vec::new();
    for _ in 0..threads {
        let c = world
            .spawn_user_coroutine(
                0,
                CoroutineEntry {
                    pc: 0x1000,
                    required_frame: 64,
                },
            )
            .expect("spawn")
            .expect("admit");
        world.enter_coroutine(c).expect("enter");
        coroutines.push(c);
    }

    while let Ok((thread_id, reply)) = rx.recv() {
        let c = coroutines[(thread_id % threads) as usize];
        let outcome = world.mutex_lock(mutex, c).expect("lock");
        match outcome {
            MutexLockOutcome::Acquired => {
                counter += 1;
                let _ = atomic.store(u64::from(c.index), counter, MemoryOrdering::Release);
                let loaded = atomic
                    .load(u64::from(c.index), MemoryOrdering::Acquire)
                    .unwrap_or(0);
                world.mutex_unlock(mutex, c).expect("unlock");
                let _ = reply.send(loaded == counter);
            }
            MutexLockOutcome::Contended { .. } => {
                let _ = reply.send(false);
            }
        }
    }
    let expected = u64::from(threads * iterations);
    let ledger_ok = world.ledger_invariant(0).is_ok();
    (counter, counter == expected && ledger_ok)
}

/// hybrid write barrier card-mark 记账与 flush 的进程内 harness。
///
/// 与确定性测试共用同一份 `BarrierPlane`：线程只写自己 processor 的 owner-local buffer，
/// card table 只在 flush 之后由 arena owner 写入。它不进入默认测试套件，供 `cargo bench`
/// 与手工验证使用。
#[derive(Clone, Copy, Debug)]
pub struct CardMarkHarness {
    /// 参与记账的 processor 数。
    processors: u32,
    /// 每个 processor 的写入次数。
    iterations: u32,
}

/// CardMarkHarness 运行报告。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CardMarkReport {
    /// 参与记账的 processor 数。
    pub processors: u32,
    /// 每个 processor 的写入次数。
    pub iterations: u32,
    /// processor-local 记账总次数。
    pub card_marks: u64,
    /// dedup 命中总次数。
    pub slot_reuses: u64,
    /// 发布的 batch 总数。
    pub batches: u64,
    /// 消费的 batch 总数。
    pub consumed_batches: u64,
    /// 最终 dirty card 数。
    pub dirty_cards: u32,
    /// 运行耗时（微秒）。
    pub elapsed_micros: u64,
    /// 不变量是否守恒。
    pub invariants_hold: bool,
}

impl CardMarkHarness {
    /// 创建 harness。
    pub fn new(processors: u32, iterations: u32) -> Self {
        Self {
            processors: processors.max(1),
            iterations: iterations.max(1),
        }
    }

    /// 执行一轮 card-mark 记账、flush 与 owner 消费。
    pub fn run(self) -> CardMarkReport {
        let start = Instant::now();
        let mut plane = super::barrier::BarrierPlane::new(1);
        let manager = OwnerToken {
            domain: super::slab::MemoryDomainId::RUNTIME_RAW,
            owner_id: super::slab::OwnerId::from_raw(1),
            generation: super::slab::OwnerGeneration::from_raw(1),
            route_key: super::slab::RouteKey::from_raw(1),
        };
        let arena = 1_u64;
        let registered = plane
            .register_arena(
                arena,
                manager,
                1,
                super::gc_metadata_contract::GC_ARENA_BYTES,
            )
            .is_ok();
        for processor in 0..self.processors as usize {
            for index in 0..self.iterations {
                // 每个 processor 只落在一个 card 上：命中 dedup 槽并不产生新键。
                let offset = u64::from(self.processors) * 512 + u64::from(index) % 512;
                plane
                    .perform_barrier(
                        processor,
                        super::barrier::BarrierSite {
                            arena_descriptor: arena,
                            arena_generation: 1,
                            offset,
                            cycle_epoch: 1,
                            // source 与 target 落在不同 block：harness 因此同时覆盖
                            // owner-local edge summary 的聚合与取走路径。
                            source: BlockRef {
                                id: ManagedBlockId(0),
                                generation: 1,
                            },
                            old: None,
                            new: Some(BlockRef {
                                id: ManagedBlockId(
                                    u32::try_from(processor + 1).expect("block 编号适配 u32"),
                                ),
                                generation: 1,
                            }),
                            new_in_nursery: true,
                            owner_old: true,
                            marking: true,
                            stack_grey: true,
                        },
                    )
                    .expect("写屏障成功");
            }
        }
        let mut batches = 0_u64;
        for processor in 0..self.processors as usize {
            let drafts = plane
                .flush_processor(processor, super::barrier::BarrierFlushReason::BufferFull)
                .expect("flush 可执行");
            batches += u64::try_from(drafts.len()).unwrap_or(u64::MAX);
            for draft in &drafts {
                let _ = plane.consume_locally(arena, draft);
            }
        }
        let mut card_marks = 0_u64;
        let mut slot_reuses = 0_u64;
        for processor in 0..self.processors as usize {
            if let Some(record) = plane.processor(processor) {
                card_marks += record.card_marks();
                slot_reuses += record.card_slot_reuses();
            }
        }
        let dirty_cards = plane
            .table(arena)
            .map_or(0, super::barrier::CardTable::dirty);
        // edge summary 是 owner-local 聚合：每个 processor 的 target block 不同，因此每个
        // processor 恰好留下一条待发布 delta；发布后挂起数必须归零。
        let edge_deltas =
            u64::try_from(plane.publish_edges(1).expect("边差量可发布").len()).unwrap_or(u64::MAX);
        let edge_pending_after_drain = plane.edges().pending();
        let expected = u64::from(self.processors) * u64::from(self.iterations);
        let invariants_hold = registered
            && card_marks == expected
            && slot_reuses + u64::try_from(self.processors).unwrap_or(0) >= card_marks
            && dirty_cards > 0
            && edge_deltas == u64::from(self.processors)
            && edge_pending_after_drain == 0;
        CardMarkReport {
            processors: self.processors,
            iterations: self.iterations,
            card_marks,
            slot_reuses,
            batches,
            consumed_batches: batches,
            dirty_cards,
            elapsed_micros: u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX),
            invariants_hold,
        }
    }
}

/// EdgeNode 夹具源码：harness 与回归测试共用同一份真实编译输入。
const EDGE_NODE_SOURCE: &str = include_str!("fixtures/edge_nodes.gg");

/// 真实 `Compilation` 消费者的边与候选 harness。
///
/// 与其余 harness 不同，这里的输入是**编译器真实产物**：`Compiler` 编译 EdgeNode 夹具，harness
/// 用镜像计划的 LocalHeap/GC metadata/edge 契约配置 `RawWorld`，再按真实类型表建立跨 owner
/// 引用、跑标记与候选判定。它不参数化契约常量，只参数化轮数与每轮的节点数。
#[derive(Clone, Copy, Debug)]
pub struct EdgeCandidateHarness {
    /// 完整 cycle 轮数。
    rounds: u32,
    /// 每个 owner 每轮的节点数。
    nodes: u32,
}

/// EdgeCandidateHarness 运行报告。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EdgeCandidateReport {
    /// 编译轮次。
    pub rounds: u32,
    /// 每轮每 owner 的节点数。
    pub nodes: u32,
    /// 夹具真实类型表里的 managed 类型数。
    pub managed_types: u64,
    /// 建立的跨 owner 引用总数。
    pub cross_owner_stores: u64,
    /// 发布的 `EdgeDelta` 总数。
    pub edge_deltas: u64,
    /// 目标 owner 应用后的入边计数总和。
    pub applied_edges: i64,
    /// 发布的 mark ticket 总数。
    pub mark_tickets: u64,
    /// 候选平面消耗的工作单位。
    pub candidate_work_units: u64,
    /// 候选释放的 block 数。
    pub blocks_released: u64,
    /// 运行耗时（微秒）。
    pub elapsed_micros: u64,
    /// 不变量是否守恒。
    pub invariants_hold: bool,
}

impl EdgeCandidateHarness {
    /// 创建 harness；轮数与节点数至少为 1。
    pub fn new(rounds: u32, nodes: u32) -> Self {
        Self {
            rounds: rounds.max(1),
            nodes: nodes.max(1),
        }
    }

    /// 编译夹具、按真实契约驱动 `rounds` 轮边发布、标记与候选判定。
    pub fn run(self) -> EdgeCandidateReport {
        use crate::runtime::gc_metadata_section::decode_sections;
        use crate::runtime::world::heap_impl::ManagedPlacement;
        use crate::{CompileRequest, Compiler, TargetName};

        let start = Instant::now();
        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            EDGE_NODE_SOURCE,
            TargetName::X86_64Linux,
        ));
        let plan = compilation.image_plan().expect("夹具必须编译成功");
        let contract = compilation.raw_contract().expect("真实契约必须存在");
        let types = decode_sections(plan.gc_type_section(), plan.gc_metadata_section())
            .expect("section 可解码");
        // 与计划断言同一口径：同名占位项不参与，取带布局的记录。
        let node = types
            .types()
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.ends_with("EdgeNode"))
            .max_by_key(|(_, entry)| entry.size)
            .expect("EdgeNode 必须存在");
        let tail = types
            .types()
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.ends_with("EdgeNodeTail"))
            .max_by_key(|(_, entry)| entry.size)
            .expect("EdgeNodeTail 必须存在");
        let node_type = u32::try_from(node.0).expect("类型下标适配 u32");
        let tail_type = u32::try_from(tail.0).expect("类型下标适配 u32");
        let (node_size, tail_size) = (node.1.size, tail.1.size);

        let budget = super::inbox::ServiceBudget::pressure(u32::MAX, u64::MAX);
        let mut report = EdgeCandidateReport {
            rounds: self.rounds,
            nodes: self.nodes,
            managed_types: u64::from(plan.local_heap_demand().managed_types),
            cross_owner_stores: 0,
            edge_deltas: 0,
            applied_edges: 0,
            mark_tickets: 0,
            candidate_work_units: 0,
            blocks_released: 0,
            elapsed_micros: 0,
            invariants_hold: true,
        };
        let trace = std::env::var_os("GUGU_BENCH_TRACE").is_some();
        let mut clean = true;
        for round in 0..self.rounds {
            // 每轮用一个新 world：harness 度量的是同一条真实路径的重复执行，跨 cycle 的信用与
            // 候选状态由世界级测试覆盖，不在这里耦合进吞吐口径。
            let node_capacity = self.nodes.saturating_mul(4).max(64);
            let mut world = RawWorld::new(
                53 + u64::from(round),
                2,
                node_capacity,
                BatchLimits::default(),
            )
            .expect("world 可创建");
            world.configure_gc(contract).expect("真实契约可配置");
            let mut world_stores = 0_u64;
            // 每轮重建引用：owner 0 的每个节点指向 owner 1 的对应节点并反向指回。
            let mut sources = Vec::with_capacity(self.nodes as usize);
            let mut targets = Vec::with_capacity(self.nodes as usize);
            for _ in 0..self.nodes {
                let source = match world.allocate_managed(
                    0,
                    node_type,
                    node_size,
                    ManagedPlacement::Old,
                    false,
                ) {
                    Ok(address) => address,
                    Err(error) => {
                        if trace {
                            eprintln!("edge-candidates 第 {round} 轮分配 source 失败: {error:?}");
                        }
                        clean = false;
                        break;
                    }
                };
                let target = match world.allocate_managed(
                    1,
                    tail_type,
                    tail_size,
                    ManagedPlacement::Old,
                    false,
                ) {
                    Ok(address) => address,
                    Err(error) => {
                        if trace {
                            eprintln!("edge-candidates 第 {round} 轮分配 target 失败: {error:?}");
                        }
                        clean = false;
                        break;
                    }
                };
                if let Err(error) = world.store_managed_field(0, 0, source, 8, target) {
                    if trace {
                        eprintln!("edge-candidates 第 {round} 轮正向写入失败: {error:?}");
                    }
                    clean = false;
                    break;
                }
                if let Err(error) = world.store_managed_field(1, 0, target, 0, source) {
                    if trace {
                        eprintln!("edge-candidates 第 {round} 轮反向写入失败: {error:?}");
                    }
                    clean = false;
                    break;
                }
                report.cross_owner_stores += 2;
                world_stores += 2;
                sources.push(source);
                targets.push(target);
            }
            if sources.len() as u32 != self.nodes {
                clean = false;
                break;
            }
            // 只有本轮的第一个节点对挂根：其余节点会成为真实的垃圾，用来覆盖候选回收路径。
            let slot_source = match world
                .register_managed_root(super::gc_metadata_schema::GcRootKindV1::CoroutineFrame, 0)
            {
                Ok(slot) => slot,
                Err(error) => {
                    if trace {
                        eprintln!("edge-candidates 第 {round} 轮登记 owner 0 根槽失败: {error:?}");
                    }
                    clean = false;
                    break;
                }
            };
            let slot_target = match world
                .register_managed_root(super::gc_metadata_schema::GcRootKindV1::CoroutineFrame, 1)
            {
                Ok(slot) => slot,
                Err(error) => {
                    if trace {
                        eprintln!("edge-candidates 第 {round} 轮登记 owner 1 根槽失败: {error:?}");
                    }
                    clean = false;
                    break;
                }
            };
            clean &= world.set_managed_root(slot_source, sources[0]).is_ok();
            clean &= world.set_managed_root(slot_target, targets[0]).is_ok();
            // 每轮按 block 对核对多重计数：同一 block 对里的写入必须精确累加。
            let source_ref = match world.managed_block_ref(0, sources[0]) {
                Ok(reference) => reference,
                Err(error) => {
                    if trace {
                        eprintln!("edge-candidates 第 {round} 轮解析 source block 失败: {error:?}");
                    }
                    clean = false;
                    break;
                }
            };
            let target_ref = match world.managed_block_ref(1, targets[0]) {
                Ok(reference) => reference,
                Err(error) => {
                    if trace {
                        eprintln!("edge-candidates 第 {round} 轮解析 target block 失败: {error:?}");
                    }
                    clean = false;
                    break;
                }
            };
            let published = match world.publish_edge_deltas() {
                Ok(records) => records,
                Err(error) => {
                    if trace {
                        eprintln!("edge-candidates 第 {round} 轮发布边差量失败: {error:?}");
                    }
                    clean = false;
                    break;
                }
            };
            report.edge_deltas += u64::try_from(published.len()).unwrap_or(u64::MAX);
            for owner in 0..2 {
                if let Err(error) = world.drain_all(owner, &budget) {
                    if trace {
                        eprintln!(
                            "edge-candidates 第 {round} 轮排空 owner {owner} 失败: {error:?}"
                        );
                    }
                    clean = false;
                    break;
                }
            }
            // 真实 cycle 顺序：两个 owner 一起标记 → 候选判定 → 收尾并推进 epoch。
            let mark = match world.run_mark_pass(&[0, 1]) {
                Ok(pass) => pass,
                Err(error) => {
                    if trace {
                        eprintln!("edge-candidates 第 {round} 轮 mark pass 失败: {error:?}");
                    }
                    clean = false;
                    break;
                }
            };
            report.mark_tickets += mark.tickets_published;
            clean &= mark.termination.converged();
            clean &= mark.tickets_published == mark.tickets_consumed;
            if trace
                && (!mark.termination.converged()
                    || mark.tickets_published != mark.tickets_consumed)
            {
                eprintln!("edge-candidates 第 {round} 轮标记未结清: {mark:?}");
            }
            // 票据先落 producer staging：排空两个 owner 让对端真正消费本轮发布的跨 owner 工作。
            for owner in 0..2 {
                if let Err(error) = world.drain_all(owner, &budget) {
                    if trace {
                        eprintln!(
                            "edge-candidates 第 {round} 轮标记后排水 owner {owner} 失败: {error:?}"
                        );
                    }
                    clean = false;
                    break;
                }
            }
            let mut actions = Vec::new();
            loop {
                let driven = match world.drive_candidates(plan.edge_runtime().candidate_quantum) {
                    Ok(report) => report,
                    Err(error) => {
                        if trace {
                            eprintln!("edge-candidates 第 {round} 轮候选推进失败: {error:?}");
                        }
                        clean = false;
                        break;
                    }
                };
                report.candidate_work_units += u64::from(driven.work_units);
                actions.extend(driven.actions.iter().cloned());
                let stats = world.candidate_stats().expect("统计可读");
                if stats.jobs_started == stats.jobs_completed + stats.jobs_invalidated {
                    break;
                }
            }
            if trace && !actions.is_empty() {
                eprintln!(
                    "edge-candidates 第 {round} 轮候选动作 {} 个，挂根节点对存活: {}",
                    actions.len(),
                    world.managed_object(sources[0]).is_ok()
                        && world.managed_object(targets[0]).is_ok(),
                );
            }
            clean &= world.mark_worklist_items() == 0;
            clean &= world.release_graced_nodes().is_ok();
            clean &= world.finish_mark_cycle().is_ok();
            clean &= world.advance_cycle_epoch(0).is_ok();
            // 只有本轮挂根的第一个节点对必须存活；未挂根的节点会被候选回收，这是真实行为。
            let rooted_alive = world.managed_object(sources[0]).is_ok()
                && world.managed_object(targets[0]).is_ok()
                && world
                    .managed_block_ref(0, sources[0])
                    .is_ok_and(|current| current == source_ref)
                && world
                    .managed_block_ref(1, targets[0])
                    .is_ok_and(|current| current == target_ref);
            if trace && !rooted_alive {
                eprintln!(
                    "edge-candidates 第 {round} 轮挂根节点对被回收或搬迁: source_alive={} target_alive={} source_block={:?} target_block={:?}",
                    world.managed_object(sources[0]).is_ok(),
                    world.managed_object(targets[0]).is_ok(),
                    world.managed_block_ref(0, sources[0]).ok(),
                    world.managed_block_ref(1, targets[0]).ok(),
                );
            }
            clean &= rooted_alive;
            if world.mark_worklist_items() != 0 && trace {
                eprintln!(
                    "edge-candidates 第 {round} 轮残留工作项: {}",
                    world.mark_worklist_items()
                );
            }
            // 全局守恒：所有活跃 block 对的已应用入边计数之和必须等于累计成功的跨 owner 写入数。
            // 这条不变量与 block 拓扑无关：计数既不能塌成布尔标志，也不能丢边或多记。
            let pairs = match world.edge_pairs() {
                Ok(pairs) => pairs,
                Err(error) => {
                    if trace {
                        eprintln!("edge-candidates 第 {round} 轮读取边对失败: {error:?}");
                    }
                    clean = false;
                    break;
                }
            };
            let applied: i64 = pairs.iter().map(|(_, _, count)| *count).sum();
            let expected = i64::try_from(world_stores).expect("计数适配 i64");
            clean &= applied == expected;
            if trace && applied != expected {
                let plane = world.edge_plane().expect("边平面");
                eprintln!(
                    "edge-candidates 第 {round} 轮计数不守恒: applied={applied} expected={expected} held={} pending={} pairs={pairs:?}",
                    plane.held_records(),
                    plane.pending_credits(),
                );
            }
            report.applied_edges = applied;
            let stats = world.candidate_stats().expect("统计可读");
            report.blocks_released = stats.blocks_released;
            clean &= stats.jobs_started == stats.jobs_completed + stats.jobs_invalidated;
            if !clean {
                break;
            }
        }
        report.elapsed_micros = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        report.invariants_hold = clean;
        report
    }
}

/// TurnRegion 生命周期与 `RegionTransfer` 往返的吞吐 smoke。
///
/// 只断言不变量并打印吞吐；确定性正确性由 region 单测承担。不进 `nextest`。
#[derive(Clone, Copy, Debug)]
pub struct RegionTransferHarness {
    /// 每轮建立的 region 数。
    regions: u32,
    /// 每个 region 的 bump 次数。
    bumps: u32,
}

/// RegionTransferHarness 运行报告。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionTransferReport {
    /// 建立的 region 总数。
    pub regions: u64,
    /// bump 总次数。
    pub bumps: u64,
    /// 整区回收的 region 数。
    pub resets: u64,
    /// 保留的 region 数。
    pub promotions: u64,
    /// 发出的 transfer 数。
    pub transfers: u64,
    /// 采纳的 transfer 数。
    pub adopted: u64,
    /// 运行耗时（微秒）。
    pub elapsed_micros: u64,
    /// 不变量是否守恒。
    pub invariants_hold: bool,
}

impl RegionTransferHarness {
    /// 创建 harness；region 数与 bump 次数至少为 1。
    pub fn new(regions: u32, bumps: u32) -> Self {
        Self {
            regions: regions.max(1),
            bumps: bumps.max(1),
        }
    }

    /// 执行一轮 region 建立、bump、发布、回收与移交往返。
    pub fn run(self) -> RegionTransferReport {
        let start = Instant::now();
        let contract = super::region_schema::TurnRegionRuntimeContract::build(
            super::region_schema::TurnRegionDemand::default(),
        )
        .expect("TurnRegion 契约可构建");
        let owner = |owner_id: u64| OwnerToken {
            domain: super::slab::MemoryDomainId::RUNTIME_RAW,
            owner_id: super::slab::OwnerId::from_raw(owner_id),
            generation: super::slab::OwnerGeneration::from_raw(1),
            route_key: super::slab::RouteKey::from_raw(owner_id),
        };
        let sender = owner(1);
        let receiver = owner(2);
        let secret = [7_u8; 32];
        let mut plane = super::region::RegionPlane::new(&[sender, receiver], &contract);
        let pool = super::message::ReturnNodePool::new(self.regions + 1);
        let mut resets = 0_u64;
        let mut promotions = 0_u64;
        let mut transfers = 0_u64;
        let mut adopted = 0_u64;
        let mut bumps = 0_u64;
        let mut invariants = true;
        for index in 0..self.regions {
            let region = match plane.registry_mut(0).open(64) {
                Ok(region) => region,
                Err(_) => {
                    invariants = false;
                    break;
                }
            };
            let mut offset = 0;
            for _ in 0..self.bumps {
                match plane.registry_mut(0).bump(region, 1, 1) {
                    Ok(next) if next == offset => {
                        offset += 1;
                        bumps += 1;
                    }
                    _ => {
                        invariants = false;
                        break;
                    }
                }
            }
            if plane.registry_mut(0).publish(region, 0).is_err() {
                invariants = false;
                break;
            }
            // 三条结束路径轮流覆盖：整区回收、事实不闭合导致的保留、跨 owner 移交。
            match index % 3 {
                0 => {
                    match plane.registry_mut(0).reset(region) {
                        Ok(super::region::ResetOutcome::Reset { .. }) => resets += 1,
                        _ => invariants = false,
                    }
                    continue;
                }
                1 => {
                    // runtime 观察到 resource lease：门禁拒绝 reset，调用方转为保留。
                    if plane
                        .registry_mut(0)
                        .observe(
                            region,
                            super::region_schema::RegionExport::ResourceLease.bit(),
                        )
                        .is_err()
                    {
                        invariants = false;
                        break;
                    }
                    match plane.registry_mut(0).reset(region) {
                        Ok(super::region::ResetOutcome::Refused(_)) => {
                            match plane
                                .registry_mut(0)
                                .promote(region, super::region::PromoteReason::Summary)
                            {
                                Ok(_) => promotions += 1,
                                Err(_) => invariants = false,
                            }
                        }
                        _ => invariants = false,
                    }
                    continue;
                }
                _ => {}
            }
            let batch = match plane
                .registry_mut(0)
                .transfer(region, receiver, 1, 1, &secret)
            {
                Ok(batch) => batch,
                Err(_) => {
                    invariants = false;
                    break;
                }
            };
            let node = match pool.allocate() {
                Ok(node) => node,
                Err(_) => {
                    invariants = false;
                    break;
                }
            };
            pool.store_region_transfer(node, &batch, batch.integrity.checksum);
            if pool.load_region_transfer(node) != batch {
                invariants = false;
            }
            transfers += 1;
            if plane.enqueue(batch).is_err() {
                invariants = false;
                break;
            }
            let Some(pending) = plane.take(receiver) else {
                invariants = false;
                break;
            };
            let adopted_region = match plane.registry_mut(1).receive(&pending, &secret) {
                Ok(region) => region,
                Err(_) => {
                    invariants = false;
                    break;
                }
            };
            adopted += 1;
            if plane.registry_mut(0).confirm(region).is_err() {
                invariants = false;
            }
            match plane.registry_mut(1).receive_reset(adopted_region) {
                Ok(super::region::ResetOutcome::Reset { .. }) => resets += 1,
                _ => invariants = false,
            }
        }
        invariants &= plane.pending() == 0;
        invariants &= plane.active(0) == 0 && plane.active(1) == 0;
        invariants &= resets + promotions == u64::from(self.regions);
        invariants &= transfers == adopted;
        let elapsed_micros = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        RegionTransferReport {
            regions: u64::from(self.regions),
            bumps,
            resets,
            promotions,
            transfers,
            adopted,
            elapsed_micros,
            invariants_hold: invariants,
        }
    }
}

/// 共享环境夹具源码：harness、bench 与 LIR 回归共用同一份真实编译输入。
const SHARED_SENDER_SOURCE: &str = include_str!("fixtures/shared_sender.gg");

/// owner service 的节奏：每发布这么多条搬迁通知排空一次目标 owner。
const DRAIN_INTERVAL: u32 = 32;

/// 真实 `Compilation` 消费者的共享搬迁闭环 harness。
///
/// 输入是编译器对共享环境夹具的真实产物：harness 用镜像计划契约配置 `RawWorld`，逐个 handle
/// 走完「分配 → 字段写入 → 搬迁 → 通知消费 → grace 结清」，最后跑一轮真实 cycle 观察 sweep 与
/// block 搬迁。它不参数化契约常量，只参数化 owner 数与搬迁次数。
#[derive(Clone, Copy, Debug)]
pub struct SharedForwardHarness {
    /// owner 数量；至少 2（写入方与 payload owner 分开）。
    owners: u32,
    /// 搬迁次数。
    forwards: u32,
}

/// SharedForwardHarness 运行报告。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedForwardReport {
    /// 请求的搬迁次数。
    pub forwards: u64,
    /// 累计复制的搬迁字节数。
    pub forwarded_bytes: u64,
    /// 累计释放的 payload 字节数。
    pub freed_bytes: u64,
    /// cycle 结束时已封口且无存活 payload 的共享 block 数。
    pub empty_blocks: u64,
    /// 运行耗时（微秒）。
    pub elapsed_micros: u64,
    /// 不变量是否守恒。
    pub invariants_hold: bool,
}

impl SharedForwardHarness {
    /// 创建 harness；owner 数至少 2，搬迁次数至少 1。
    pub fn new(owners: u32, forwards: u32) -> Self {
        Self {
            owners: owners.max(2),
            forwards: forwards.max(1),
        }
    }

    /// 驱动全部搬迁闭环并跑一轮真实 cycle。
    pub fn run(self) -> SharedForwardReport {
        let start = Instant::now();
        let mut report = SharedForwardReport {
            forwards: u64::from(self.forwards),
            forwarded_bytes: 0,
            freed_bytes: 0,
            empty_blocks: 0,
            elapsed_micros: 0,
            invariants_hold: false,
        };
        match self.drive() {
            Ok((forwarded_bytes, freed_bytes, empty_blocks)) => {
                report.forwarded_bytes = forwarded_bytes;
                report.freed_bytes = freed_bytes;
                report.empty_blocks = empty_blocks;
                report.invariants_hold = true;
            }
            Err(error) => eprintln!("shared-forward harness 失败: {error:?}"),
        }
        report.elapsed_micros = elapsed_micros(start);
        report
    }

    /// 返回 `(搬迁字节, 释放字节, 空 block 数)`；任何一步违反不变量都返回错误。
    fn drive(self) -> Result<(u64, u64, u64), RawInvariant> {
        use crate::runtime::gc_metadata_contract::GC_BLOCK_BYTES;
        use crate::runtime::gc_metadata_section::decode_sections;
        use crate::runtime::world::heap_impl::ManagedPlacement;
        use crate::runtime::world::shared_forward_impl::SharedForwardOutcome;
        use crate::{CompileRequest, Compiler, TargetName};

        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            SHARED_SENDER_SOURCE,
            TargetName::X86_64Linux,
        ));
        let plan = compilation.image_plan().expect("夹具必须编译成功");
        let contract = compilation.raw_contract().expect("真实契约必须存在");
        let types = decode_sections(plan.gc_type_section(), plan.gc_metadata_section())
            .map_err(|error| RawInvariant::new(error.message().to_owned()))?;
        let (type_index, type_size) = types
            .types()
            .iter()
            .enumerate()
            .map(|(index, entry)| (index, entry.size))
            .next()
            .ok_or_else(|| RawInvariant::new("夹具必须冻结至少一个 managed 类型"))?;
        let type_index =
            u32::try_from(type_index).map_err(|_| RawInvariant::new("类型下标超出 u32"))?;
        let demand = plan.shared_heap_demand();
        if demand.alloc_sites == 0 {
            return Err(RawInvariant::new("夹具必须产生共享分配"));
        }
        // payload 必须能装进一个共享 block：契约上界来自冻结类型表的最大类型（含 arena metadata
        // 这类运行时记录），按它分配会让每个 payload 独占一个 block，既不是真实负载。
        // large-object 路径已由 managed block return 接入；本 harness 仍限制在 GC_BLOCK_BYTES/8
        // 以保持现有吞吐口径。
        let payload_bytes = u32::try_from(demand.max_payload_bytes)
            .map_err(|_| RawInvariant::new("共享 payload 上界超出 u32"))?
            .min(GC_BLOCK_BYTES / 8);
        let worker = 0_u32;
        let payload_owner = 1_u32;
        // 在飞通知会一直占着 node 直到 owner 排空，因此容量必须容下一个 drain 周期的在飞量。
        let nodes = self.forwards.saturating_mul(4).saturating_add(256);
        let mut world = RawWorld::new(53, self.owners, nodes, BatchLimits::default())?;
        world.configure_gc(contract)?;
        let budget = ServiceBudget::pressure(u32::MAX, u64::MAX);
        let mut consumed_total = 0_u32;
        for index in 0..self.forwards {
            let handle = world.allocate_shared_object(payload_owner, payload_bytes)?;
            let child = world.allocate_managed(
                worker,
                type_index,
                type_size,
                ManagedPlacement::Nursery,
                false,
            )?;
            world.store_shared_managed_field(worker, 0, handle, 0, child, Some(worker))?;
            let SharedForwardOutcome::Forwarded(_) =
                world.forward_shared_payload(worker, handle)?
            else {
                return Err(RawInvariant::new("无 pin 的搬迁必须成功"));
            };
            // owner service 是节奏点而不是每条消息一次：按固定间隔排空，与真实 runtime 的
            // pacing 点一致，也让吞吐口径反映搬迁本身而不是 per-message 的 owner 切换。
            let last = index + 1 == self.forwards;
            if index % DRAIN_INTERVAL == DRAIN_INTERVAL - 1 || last {
                let (_, consumed) = world.drain_all(payload_owner, &budget)?;
                consumed_total = consumed_total.saturating_add(consumed);
            }
        }
        if u64::from(consumed_total) < u64::from(self.forwards) {
            return Err(RawInvariant::new("每一次搬迁通知都必须被目标 owner 消费"));
        }
        if world.shared_forward_pending() != 0 {
            return Err(RawInvariant::new("返回前所有在飞搬迁都必须结清"));
        }
        // 一轮真实 cycle：未标记的共享对象被 sweep 释放，大部分已死的 block 被搬迁。
        let cycle = world.run_gc_cycle(true).map_err(|error| {
            RawInvariant::new(format!("共享平面 cycle 失败: {}", error.message()))
        })?;
        let totals = world.shared_forward_totals();
        if totals.forwards != u64::from(self.forwards) {
            return Err(RawInvariant::new("搬迁计数必须与请求一致"));
        }
        if world.shared_forward_pending() != 0 {
            return Err(RawInvariant::new("cycle 结束时不得有在飞搬迁"));
        }
        if world.shared_registry().len() != 0 {
            return Err(RawInvariant::new("未标记的共享对象必须在 cycle 内全部释放"));
        }
        // 每个对象释放一次 current payload，加上搬迁淘汰的旧 payload：正好两倍搬迁字节。
        let expected = u64::from(payload_bytes)
            .saturating_mul(2)
            .saturating_mul(u64::from(self.forwards));
        if totals.freed_bytes != expected {
            return Err(RawInvariant::new(
                "释放字节必须等于每个对象两次 payload 字节之和",
            ));
        }
        if cycle.shared_released != u64::from(self.forwards) {
            return Err(RawInvariant::new("cycle 必须释放全部未标记共享对象"));
        }
        Ok((
            totals.forwarded_bytes,
            totals.freed_bytes,
            cycle.shared_empty_blocks,
        ))
    }
}

/// 真实 `Compilation` 消费者的 managed block return harness。
///
/// 用镜像计划契约配置 world，多 owner 分配 → 标死 → cycle → drain，断言 exactly-once、
/// 账本守恒与 provider committed 下降。large-object 路径已由本阶段接入；payload 仍限制在
/// `GC_BLOCK_BYTES/8` 以保持现有吞吐口径。
#[derive(Clone, Copy, Debug)]
pub struct BlockReturnHarness {
    owners: u32,
    leaves: u32,
}

/// BlockReturnHarness 运行报告。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockReturnReport {
    /// 分配的 leaf 数。
    pub leaves: u64,
    /// 本轮归还的 block 字节。
    pub returned_bytes: u64,
    /// 运行耗时（微秒）。
    pub elapsed_micros: u64,
    /// 不变量是否守恒。
    pub invariants_hold: bool,
}

impl BlockReturnHarness {
    /// 创建 harness；owner 数至少 2，leaf 数至少 1。
    pub fn new(owners: u32, leaves: u32) -> Self {
        Self {
            owners: owners.max(2),
            leaves: leaves.max(1),
        }
    }

    /// 驱动分配、cycle 与 drain，并校验账本守恒。
    pub fn run(self) -> BlockReturnReport {
        let start = Instant::now();
        let mut report = BlockReturnReport {
            leaves: u64::from(self.leaves),
            returned_bytes: 0,
            elapsed_micros: 0,
            invariants_hold: false,
        };
        match self.drive() {
            Ok(returned_bytes) => {
                report.returned_bytes = returned_bytes;
                report.invariants_hold = true;
            }
            Err(error) => eprintln!("block-return harness 失败: {error:?}"),
        }
        report.elapsed_micros = elapsed_micros(start);
        report
    }

    fn drive(self) -> Result<u64, RawInvariant> {
        use crate::runtime::gc_metadata_contract::GC_BLOCK_BYTES;
        use crate::runtime::world::heap_impl::ManagedPlacement;
        use crate::{CompileRequest, Compiler, TargetName};

        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            SHARED_SENDER_SOURCE,
            TargetName::X86_64Linux,
        ));
        let contract = compilation
            .raw_contract()
            .ok_or_else(|| RawInvariant::new("真实契约必须存在"))?;
        let mut world = RawWorld::new(19, self.owners, 256, BatchLimits::default())?;
        world.configure_gc(contract)?;
        for _ in 0..self.leaves {
            world.allocate_managed(0, 1, 8, ManagedPlacement::Old, false)?;
        }
        let before = world.provider_stats().committed_bytes;
        let cycle = world.run_gc_cycle(true)?;
        if !cycle.cycle_completed {
            return Err(RawInvariant::new("cycle 必须完成"));
        }
        // managed return 的 consume 只把 extent 交给 grace，物理页要等 GRACE_STEPS 个 epoch 才
        // trim；报告要求 provider committed 真的下降，因此这里按 owner service 的真实节奏推进
        // grace。
        let inbox = world.inbox(0);
        for _ in 0..crate::runtime::model::GRACE_STEPS {
            world.open_grace(&inbox);
            world.close_grace(&inbox);
            world.advance_pending_extent_trims()?;
        }
        for owner in 0..self.owners {
            world.managed_ledger_invariant(owner)?;
        }
        let after = world.provider_stats().committed_bytes;
        let returned = before.saturating_sub(after);
        if returned < u64::from(GC_BLOCK_BYTES) {
            return Err(RawInvariant::new("至少归还一个 managed block"));
        }
        Ok(returned)
    }
}

/// checked pointer compression 的进程内可运行切片。
///
/// 输入是编译器对共享环境夹具的真实产物：默认（关闭）profile 下验证 full-pointer 等价，
/// 随后以显式 cage profile 重建契约，让 managed arena 从 cage island 切出、压缩根经 checked
/// 解码参与 root slice/mark seeding、minor 搬迁后重新编码，并逐条拒绝非法 FFI 交接。
#[derive(Clone, Copy, Debug)]
pub struct CompressionHarness {
    owners: u32,
    objects: u32,
}

/// CompressionHarness 运行报告。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompressionReport {
    /// owner 数。
    pub owners: u32,
    /// 每个 owner 分配的 managed 对象数。
    pub objects: u32,
    /// 成功解码的压缩引用数（含吞吐测量的解码）。
    pub decodes: u64,
    /// 被拒绝的解码数。
    pub rejections: u64,
    /// 成功的 FFI 保存数。
    pub foreign_saves: u64,
    /// 已切出的 cage island 数。
    pub islands: u64,
    /// 吞吐测量每个安全点上的压缩槽数。
    pub decode_slots: u32,
    /// 吞吐测量中成功解码的槽数。
    pub decode_words: u64,
    /// 吞吐测量耗时（微秒）。
    pub decode_micros: u64,
    /// 不变量是否守恒。
    pub invariants_hold: bool,
    /// 运行耗时（微秒）。
    pub elapsed_micros: u64,
}

/// 解码吞吐测量的压缩槽数。
const DECODE_THROUGHPUT_SLOTS: u32 = 64;
/// 解码吞吐测量的重复轮数；总解码数为 `DECODE_THROUGHPUT_SLOTS * DECODE_THROUGHPUT_ROUNDS`。
const DECODE_THROUGHPUT_ROUNDS: u32 = 64;

/// 在一个已配置 cage profile 的世界里测量 `scan_roots` 的压缩槽解码吞吐。
///
/// 栈布局是合成的（固定函数/安全点/map 与按当前 generation 编码的字数组），但 cage 是真实
/// 预留、解码走真实 checked 路径，因此 decode/us 反映解码成本而不是位运算循环。
fn measure_decode_throughput(world: &mut RawWorld) -> Result<(u64, u64), RawInvariant> {
    use super::stackmap::{WalkFunction, WalkMap, WalkSafepoint, WalkWorld, scan_roots};

    let descriptor = world
        .compression()
        .and_then(|plane| plane.descriptor(0).ok())
        .ok_or_else(|| RawInvariant::new("吞吐测量需要已登记的 cage"))?;
    let word = world.compression_mut()?.encode(descriptor.base + 0x40)?;
    let functions = vec![WalkFunction {
        code_rva: 0x1000,
        code_size: 64,
        frame_size: 24,
    }];
    let safepoints = vec![WalkSafepoint {
        function: 0,
        pc_offset: 8,
        kind: 0,
        map: 0,
    }];
    let maps = vec![WalkMap {
        compressed: (0..DECODE_THROUGHPUT_SLOTS).collect(),
        ..WalkMap::default()
    }];
    let words = vec![word; DECODE_THROUGHPUT_SLOTS as usize];
    let walk = WalkWorld {
        functions: &functions,
        safepoints: &safepoints,
        maps: &maps,
        handles: &[],
        landings: &[],
    };
    let mut decodes = 0_u64;
    let start = Instant::now();
    for _ in 0..DECODE_THROUGHPUT_ROUNDS {
        let roots = scan_roots(&walk, 0, &words, world.compression_mut()?)
            .map_err(|error| RawInvariant::new(error.message().to_owned()))?;
        decodes += u64::try_from(roots.len()).expect("槽数适配 u64");
    }
    Ok((decodes, elapsed_micros(start)))
}

impl CompressionHarness {
    /// 创建 harness；owner 数至少 1，对象数至少 1。
    pub fn new(owners: u32, objects: u32) -> Self {
        Self {
            owners: owners.max(1),
            objects: objects.max(1),
        }
    }

    /// 驱动 full-pointer 对照、cage 配置、压缩根 cycle 与非法 FFI 路径。
    pub fn run(self) -> CompressionReport {
        let start = Instant::now();
        let mut report = CompressionReport {
            owners: self.owners,
            objects: self.objects,
            decodes: 0,
            rejections: 0,
            foreign_saves: 0,
            islands: 0,
            decode_slots: DECODE_THROUGHPUT_SLOTS,
            decode_words: 0,
            decode_micros: 0,
            invariants_hold: false,
            elapsed_micros: 0,
        };
        match self.drive() {
            Ok((decodes, rejections, foreign_saves, islands, decode_words, decode_micros)) => {
                report.decodes = decodes;
                report.rejections = rejections;
                report.foreign_saves = foreign_saves;
                report.islands = islands;
                report.decode_words = decode_words;
                report.decode_micros = decode_micros;
                report.invariants_hold = true;
            }
            Err(error) => eprintln!("compression harness 失败: {error:?}"),
        }
        report.elapsed_micros = elapsed_micros(start);
        report
    }

    fn drive(self) -> Result<(u64, u64, u64, u64, u64, u64), RawInvariant> {
        use crate::runtime::cage::ForeignPin;
        use crate::runtime::compression_schema::{CompressionDemand, CompressionPolicyV1};
        use crate::runtime::gc_metadata_contract::GC_ARENA_BYTES;
        use crate::runtime::world::heap_impl::ManagedPlacement;
        use crate::{CompileRequest, Compiler, TargetName};

        // 步骤 1：真实编译的默认契约不启用 cage，关闭态世界也不得登记任何 cage。
        let compilation = Compiler::new().compile(CompileRequest::single_file(
            "main.gg",
            SHARED_SENDER_SOURCE,
            TargetName::X86_64Linux,
        ));
        let raw = compilation
            .raw_contract()
            .ok_or_else(|| RawInvariant::new("真实契约必须存在"))?;
        if raw.compression().enabled() || raw.compression().cage_bytes() != 0 {
            return Err(RawInvariant::new("默认编译不得启用 cage profile"));
        }
        let mut pristine = RawWorld::new(23, self.owners, 256, BatchLimits::default())?;
        pristine.configure_gc(raw)?;
        let cage_registered = pristine
            .compression()
            .is_some_and(|plane| plane.enabled() || plane.descriptor(0).is_ok());
        if cage_registered {
            return Err(RawInvariant::new("关闭态世界不得登记 cage"));
        }
        let plain = pristine.allocate_managed(0, 1, 8, ManagedPlacement::Nursery, false)?;
        if pristine
            .compression()
            .is_some_and(|plane| plane.cage_of(plain).is_some())
        {
            return Err(RawInvariant::new("关闭态地址不得落在 cage 内"));
        }

        // 步骤 2：在真实契约上以显式 cage profile 重建契约；cage 容量取 4 个 arena 粒度。
        let cage_bytes = 4 * GC_ARENA_BYTES;
        let contract = raw
            .clone()
            .with_compression(
                CompressionPolicyV1::cage(cage_bytes),
                CompressionDemand {
                    decode_sites: 2,
                    compressed_root_slots: 2,
                },
            )
            .map_err(|error| RawInvariant::new(error.message().to_owned()))?;
        let mut world = RawWorld::new(29, self.owners, 256, BatchLimits::default())?;
        world.configure_gc(&contract)?;

        // 步骤 3：managed arena 必须落在 cage 内；压缩根写入编码字并参与 root slice 与 seed。
        let mut addresses = Vec::with_capacity(self.objects as usize);
        for _ in 0..self.objects {
            addresses.push(world.allocate_managed(0, 1, 8, ManagedPlacement::Old, false)?);
        }
        let cage = world
            .compression()
            .and_then(|plane| plane.descriptor(0).ok())
            .ok_or_else(|| RawInvariant::new("cage 必须已登记"))?;
        for arena in world.managed_arenas() {
            let inside = arena.base >= cage.base && arena.base - cage.base < cage.len;
            if !inside {
                return Err(RawInvariant::new("managed arena 基址必须落在 cage 内"));
            }
        }
        if world.managed_arenas().is_empty() {
            return Err(RawInvariant::new("必须至少登记一个 managed arena"));
        }
        let mut slots = Vec::with_capacity(self.objects as usize);
        for address in &addresses {
            slots.push(world.register_compressed_root(1, *address)?);
        }
        let report = world.run_mark_pass(&[0])?;
        let stats = world
            .compression_stats()
            .ok_or_else(|| RawInvariant::new("压缩统计必须存在"))?;
        if stats.decodes < u64::from(self.objects) {
            return Err(RawInvariant::new("root slice 与 seed 必须解码每个压缩根"));
        }
        if report.marked < u64::from(self.objects) {
            return Err(RawInvariant::new("压缩根必须 seed 到全部对象"));
        }

        // 步骤 4：minor 搬迁后压缩字重新编码；非法 FFI 路径逐条被拒绝。
        let nursery = world.allocate_managed(0, 1, 8, ManagedPlacement::Nursery, false)?;
        let slot = world.register_compressed_root(1, nursery)?;
        let before = world.compressed_root_word(slot)?;
        world.collect_minor(0)?;
        let after = world.compressed_root_word(slot)?;
        if after == before {
            return Err(RawInvariant::new("搬迁后压缩根字必须重新编码"));
        }
        let forged = ForeignPin {
            cage: 0,
            offset: 0x40,
            generation: 1,
            sequence: 0,
        };
        if world.save_for_foreign(forged).is_ok() {
            return Err(RawInvariant::new("无 pin 的 FFI 保存必须失败"));
        }
        let foreign = world.allocate_managed(0, 1, 8, ManagedPlacement::Old, false)?;
        let pin = world.pin_for_foreign(0, foreign)?;
        let saved = world.save_for_foreign(pin)?;
        world.release_for_foreign(0, saved, pin)?;
        if world.save_for_foreign(pin).is_ok() {
            return Err(RawInvariant::new("释放后的 FFI 保存必须失败"));
        }
        let pin = world.pin_for_foreign(0, foreign)?;
        let advanced = world.compression_mut()?.advance_generation(0)?;
        if advanced == forged.generation {
            return Err(RawInvariant::new("generation 推进必须改变当前值"));
        }
        if world.save_for_foreign(pin).is_ok() {
            return Err(RawInvariant::new("过期 generation 的 FFI 保存必须失败"));
        }
        let stats = world
            .compression_stats()
            .ok_or_else(|| RawInvariant::new("压缩统计必须存在"))?;
        if stats.foreign_saves == 0 || stats.foreign_rejections < 3 {
            return Err(RawInvariant::new("FFI 统计必须记录成功保存与拒绝"));
        }
        let islands = world.compression().map_or(0, |plane| plane.island_count());
        // 最后在真实 cage 上测解码吞吐：字按当前 generation 重新编码，走与栈扫描相同的
        // checked 路径。
        let (decode_words, decode_micros) = measure_decode_throughput(&mut world)?;
        let stats = world
            .compression_stats()
            .ok_or_else(|| RawInvariant::new("压缩统计必须存在"))?;
        Ok((
            stats.decodes,
            stats.rejections,
            stats.foreign_saves,
            islands,
            decode_words,
            decode_micros,
        ))
    }
}
