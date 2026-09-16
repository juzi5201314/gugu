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
use super::message::{
    BatchLimits, FlushTrigger, MessageState, ProducerStaging, ReturnKind, ReturnMessage,
    stage_message,
};
use super::slab::Epoch;
use super::slab::{OwnerToken, RawSlot};
use super::world::{RawWorld, ResourceShape};
use super::{
    BATCH_MAX, OWNER_INBOX_SHARDS, RawPlaneDemand, RawPlanePolicyV1, RuntimeRawContractV1,
    SchedulerDemand, size_class::RuntimeSizeClassId,
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
                            old_present: true,
                            new_present: true,
                            new_in_nursery: true,
                            owner_old: true,
                            marking: true,
                            stack_grey: true,
                            // 每个 processor 写入不同 target block：harness 因此同时覆盖
                            // owner-local edge summary 的聚合与取走路径。
                            new_block: Some(
                                u32::try_from(processor + 1).expect("block 编号适配 u32"),
                            ),
                            source_block: 0,
                            new_owner: 0,
                            source_owner: 0,
                        },
                    )
                    .expect("写屏障成功");
            }
        }
        let mut batches = 0_u64;
        for processor in 0..self.processors as usize {
            let drafts =
                plane.flush_processor(processor, super::barrier::BarrierFlushReason::BufferFull);
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
        // processor 恰好留下一条待取走 delta；取走后挂起数必须归零。
        let edge_deltas = u64::try_from(plane.drain_edges().len()).unwrap_or(u64::MAX);
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
