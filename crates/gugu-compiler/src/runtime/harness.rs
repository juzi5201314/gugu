//! owner-directed return 的进程内可运行切片：真实多 producer、单 owner consumer。
//!
//! 这个 harness 与确定性测试共用同一份 inbox、node pool、link 编码与账本实现：producer
//! 线程只接触自己的 staging、node pool 与目标 inbox，owner 本地的 descriptor、free
//! structure 与账本只由 consumer 线程读写。它不进入默认测试套件，供 `cargo bench` 与手工
//! 验证使用。

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    size_class::RuntimeSizeClassId,
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
        )
        .expect("runtime raw 契约可构建");
        let node_capacity = contract
            .message_node_capacity()
            .max(self.producers * self.items_per_producer + BATCH_MAX);
        let mut world =
            RawWorld::new(1, 1, node_capacity, BatchLimits::default()).expect("raw world 可创建");
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
                    let forced =
                        ((index as u32 + 1) % BATCH_MAX == 0).then_some(FlushTrigger::ItemLimit);
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

/// 真实并发的 ResourceCell release burst harness：多 producer 发布，单 owner 消费。
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

    /// 运行一轮资源 release burst，并校验 exactly-once cleanup 与账本不变量。
    pub fn run(self) -> ResourceReleaseReport {
        let policy = RawPlanePolicyV1::default();
        let contract = RuntimeRawContractV1::build(
            super::super::TargetName::X86_64Linux,
            policy,
            RawPlaneDemand::default(),
            super::RawResourceDemand::default(),
        )
        .expect("runtime raw 契约可构建");
        let total = self.producers * self.items_per_producer;
        let node_capacity = contract.message_node_capacity().max(total + BATCH_MAX);
        let mut world =
            RawWorld::new(7, 2, node_capacity, BatchLimits::default()).expect("raw world 可创建");
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
                    let forced =
                        ((index as u32 + 1) % BATCH_MAX == 0).then_some(FlushTrigger::ItemLimit);
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
                published.fetch_add(count, Ordering::Relaxed);
                count
            }));
        }

        let budget = policy.service_budget();
        let mut consumed = 0_u64;
        let mut clean = true;
        let mut idle = 0_u32;
        while producers.iter().any(|handle| !handle.is_finished()) || idle < 4 {
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
