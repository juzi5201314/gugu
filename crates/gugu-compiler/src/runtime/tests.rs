//! runtime raw 平面的确定性测试替身。
//!
//! 覆盖 dense size class、reciprocal 除法、本地 fast path、MPSC 交错、远程批量、generation
//! mismatch、owner retire、链完整性与账本互斥分类。全部是进程内、确定性、快速测试；真实
//! 并发只在 bench 中运行。

use std::collections::BTreeSet;

use super::extent::EXTENT_CLASS_LADDER;
use super::inbox::{DrainStop, GraceOutcome, OwnerInbox, ServiceBudget, ShardIndex};
use super::message::{
    BatchLimits, FlushTrigger, IntegrityTag, LinkCodec, LinkError, MessageState, ProducerStaging,
    PublishOutcome, ReturnKind, ReturnMessage, ReturnNodePool, ReturnSlabCache, RingCloseReason,
    stage_message,
};
use super::model::{CellHeaderSchemaV1, ResourceSchemaV1};
use super::model::{
    FieldKind, MessageFieldSchema, MessageSchemaV1, RawPlaneDemand, RawPlanePolicyV1,
    RawResourceDemand, RuntimeRawContractV1,
};
use super::owner::AllocationLevel;
use super::platform::{FakePlatform, PlatformProfile};
use super::provider::{ProviderError, RangeProvider, RangeState};
use super::resource::{
    self, CloseOutcome, LeaseOutcome, ReleaseDescriptor, ReleaseFlags, ReleaseRegistry,
    ResourceCell, ResourceCellTable,
};
use super::scheduler_schema::SchedulerDemand;
use super::size_class::{
    ClearField, DropScanPolicy, RuntimeSizeClassId, RuntimeSizeClassTable, StrideDivision,
};
use super::slab::{
    Epoch, MemoryDomainId, OwnerToken, RawInvariant, RuntimeSeed, SlabDescriptorId, SlabGeneration,
    SlotState,
};
use super::sync_schema::SyncDemand;
use super::wait_schema::WaitDemand;
use super::world::{RawWorld, ResourceShape};
use super::{OWNER_INBOX_SHARDS, RAW_SLAB_PAGE_BYTES, RETURN_SLAB_CACHE_SETS, Rt0Demand};
use crate::TargetName;

fn world(owners: u32, nodes: u32) -> RawWorld {
    RawWorld::new(7, owners, nodes, BatchLimits::default()).expect("raw world 可创建")
}

fn shard(index: u32) -> ShardIndex {
    ShardIndex::from_raw(index).expect("shard 编号合法")
}

/// 测试用发布：单条消息立即冲入目标 inbox。
fn publish(world: &RawWorld, owner: u32, message: &ReturnMessage) {
    let inbox = world.inbox(owner);
    let pool = world.pool();
    let mut staging = ProducerStaging::new(BatchLimits::default());
    let outcome = stage_message(
        &pool,
        Some(&inbox),
        &mut staging,
        message,
        shard(0),
        Some(FlushTrigger::OwnerPressure),
    )
    .expect("发布成功");
    assert_eq!(outcome.map(|outcome| outcome.items), Some(1));
}

#[test]
fn size_class_ladder_is_dense_and_verifies() {
    let table = RuntimeSizeClassTable::ladder(MemoryDomainId::RUNTIME_RAW).expect("阶梯可构建");
    assert_eq!(table.classes().len(), super::RAW_CLASS_LADDER.len());
    for (index, class) in table.classes().iter().enumerate() {
        assert_eq!(class.id.index(), index);
        assert_eq!(class.slot_stride, super::RAW_CLASS_LADDER[index]);
        assert_eq!(
            u64::from(class.slots_per_span) * u64::from(class.slot_stride)
                + u64::from(class.metadata_bytes),
            RAW_SLAB_PAGE_BYTES
        );
        assert!(class.link_usable);
    }
    assert_eq!(
        table.lookup(80, 8).map(|class| class.slot_stride),
        Some(128)
    );
    table.verify().expect("阶梯必须通过校验");
}

#[test]
fn size_class_rejects_mixed_invariants_and_managed_policy() {
    let mut classes = RuntimeSizeClassTable::ladder(MemoryDomainId::RUNTIME_RAW)
        .expect("阶梯可构建")
        .classes()
        .to_vec();
    let mut mixed = classes[0];
    mixed.id = RuntimeSizeClassId::from_raw(classes.len() as u16);
    mixed.policy = DropScanPolicy::ResourceLease;
    classes.push(mixed);
    assert!(RuntimeSizeClassTable::from_classes(classes).is_err());

    let mut managed = RuntimeSizeClassTable::ladder(MemoryDomainId::RUNTIME_RAW)
        .expect("阶梯可构建")
        .classes()
        .to_vec();
    managed[0].policy = DropScanPolicy::Managed;
    assert!(RuntimeSizeClassTable::from_classes(managed).is_err());
}

#[test]
fn stride_division_matches_exact_division() {
    for stride in [64_u32, 96, 4096, 4097] {
        let division = StrideDivision::new(stride);
        division
            .verify(stride)
            .expect("reciprocal 必须等价于精确除法");
        for offset in [0_u64, 1, 47, 48, 4095, 65535] {
            assert_eq!(
                division.index_of(offset),
                offset / u64::from(stride.min(4096)),
                "stride {stride} 偏移 {offset}"
            );
        }
    }
}

#[test]
fn link_codec_rejects_null_corruption_and_stale_generation() {
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let first = world.allocate(0, class).expect("分配成功");
    let second = world.allocate(0, class).expect("分配成功");
    let descriptor = world
        .descriptor(first.slot.descriptor)
        .expect("描述符存在")
        .clone();
    let codec = LinkCodec::new([9; 32]);
    let word = codec
        .encode(&descriptor, descriptor.slot_offset(first.slot.index))
        .expect("编码成功");
    assert_eq!(
        codec.decode(&descriptor, word).expect("解码成功"),
        first.slot.index
    );
    assert_eq!(
        codec.decode(&descriptor, LinkCodec::NULL),
        Err(LinkError::Null)
    );
    let corrupted = word ^ 1;
    assert_eq!(
        codec.decode(&descriptor, corrupted),
        Err(LinkError::Checksum)
    );
    let stale = codec
        .encode(&descriptor, descriptor.slot_offset(second.slot.index))
        .expect("编码成功");
    let mut bumped = descriptor.clone();
    bumped.generation = SlabGeneration::from_raw(descriptor.generation.raw() + 1);
    assert!(matches!(
        codec.decode(&bumped, stale),
        Err(LinkError::Generation { .. })
    ));
    let mut foreign = descriptor.clone();
    foreign.slot_stride = 4096;
    assert!(matches!(
        codec.decode(&foreign, word),
        Err(LinkError::Checksum | LinkError::Foreign { .. } | LinkError::Alignment { .. })
    ));
}

#[test]
fn local_allocation_follows_fixed_order() {
    let mut world = world(1, 256);
    let class = RuntimeSizeClassId::from_raw(0);
    let first = world.allocate(0, class).expect("首次分配");
    assert_eq!(first.level, AllocationLevel::RangeRequest);
    assert_eq!(world.provider_stats().rejected_requests, 0);
    let second = world.allocate(0, class).expect("第二次分配");
    assert_eq!(second.level, AllocationLevel::SpanBump);
    // 只有被发放的 extent 提交物理页；arena 的其余部分仍计入 range_reserved_bytes。
    assert_eq!(world.provider_stats().committed_bytes, RAW_SLAB_PAGE_BYTES);
    world
        .local_return(0, first.slot, u64::from(descriptor_stride(&world, class)))
        .expect("本地归还成功");
    let third = world.allocate(0, class).expect("第三次分配");
    assert_eq!(third.level, AllocationLevel::FreeList);
    assert_eq!(third.slot.index, first.slot.index);
    assert_eq!(world.provider_stats().committed_bytes, RAW_SLAB_PAGE_BYTES);
}

fn descriptor_stride(world: &RawWorld, class: RuntimeSizeClassId) -> u32 {
    world
        .classes()
        .get(class)
        .expect("class 已登记")
        .slot_stride
}

#[test]
fn span_exhaustion_refills_through_domain_cache() {
    let mut world = world(1, 8);
    let class = RuntimeSizeClassId::from_raw(0);
    let stride = u64::from(descriptor_stride(&world, class));
    let slots_per_span = world
        .classes()
        .get(class)
        .expect("class 已登记")
        .slots_per_span;
    for _ in 0..slots_per_span {
        let allocation = world.allocate(0, class).expect("span 内分配");
        world
            .queue_return(0, allocation.slot, stride)
            .expect("归还成功");
        let _ = &allocation.slot;
    }
    assert_eq!(world.provider_stats().committed_bytes, RAW_SLAB_PAGE_BYTES);
    let allocation = world.allocate(0, class).expect("新 span 分配");
    assert_eq!(allocation.level, AllocationLevel::RangeRequest);
    assert_eq!(
        world.provider_stats().committed_bytes,
        RAW_SLAB_PAGE_BYTES * 2
    );
}

#[test]
fn provider_rejects_illegal_sequences_stably() {
    let mut provider = FakePlatform::new(PlatformProfile::Linux, 1 << 20);
    assert_eq!(
        provider.reserve_aligned(0, 64, MemoryDomainId::RUNTIME_RAW),
        Err(ProviderError::ZeroBytes)
    );
    assert_eq!(
        provider.reserve_aligned(64, 48, MemoryDomainId::RUNTIME_RAW),
        Err(ProviderError::NonPowerOfTwoAlignment)
    );
    let range = provider
        .reserve_aligned(4096, 64, MemoryDomainId::RUNTIME_RAW)
        .expect("预留成功");
    assert_eq!(provider.stats().reserved_bytes, 4096);
    assert_eq!(provider.stats().committed_bytes, 0);
    assert_eq!(provider.commit(range), Ok(()));
    assert_eq!(provider.stats().reserved_bytes, 0);
    assert_eq!(provider.stats().committed_bytes, 4096);
    assert_eq!(provider.commit(range), Err(ProviderError::AlreadyCommitted));
    assert_eq!(provider.decommit(range), Ok(()));
    assert_eq!(provider.stats().reserved_bytes, 4096);
    assert_eq!(provider.stats().committed_bytes, 0);
    assert_eq!(provider.decommit(range), Err(ProviderError::NotCommitted));
    assert_eq!(provider.release(range), Ok(()));
    assert_eq!(provider.release(range), Err(ProviderError::DoubleRelease));
    assert_eq!(provider.stats().reserved_bytes, 0);
    let descriptor = provider.describe(range).expect("描述符存在");
    assert_eq!(descriptor.state, RangeState::Released);
    assert_eq!(
        provider.describe_all()[0].base % 64,
        0,
        "range 必须满足对齐要求"
    );
}

#[test]
fn exactly_once_return_rejects_double_transitions() {
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let stride = u64::from(descriptor_stride(&world, class));
    let allocation = world.allocate(0, class).expect("分配成功");
    world
        .queue_return(0, allocation.slot, stride)
        .expect("首次归还必须成功");
    assert_eq!(
        world
            .table()
            .state(allocation.slot.descriptor, allocation.slot.index),
        Ok(SlotState::ReturnQueued)
    );
    assert!(world.queue_return(0, allocation.slot, stride).is_err());
    let message = world
        .message(
            world.token(0),
            ReturnKind::RawSlot,
            allocation.slot,
            descriptor_stride(&world, class),
        )
        .expect("消息可构造");
    publish(&world, 0, &message);
    let budget = ServiceBudget::new(64, 1 << 16);
    let report = world.service(0, shard(0), &budget).expect("service 成功");
    assert_eq!(report.items, 1);
    assert_eq!(
        world
            .table()
            .state(allocation.slot.descriptor, allocation.slot.index),
        Ok(SlotState::Returned)
    );
    world.ledger_invariant(0).expect("账本必须互斥");
    assert!(
        world.service(0, shard(0), &budget).is_err() || !world.table().descriptors().is_empty()
    );
}

#[test]
fn mpsc_interleaving_keeps_chains_and_observes_phantom_null() {
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let first = world.allocate(0, class).expect("分配成功");
    let second = world.allocate(0, class).expect("分配成功");
    let target = world.token(0);
    let inbox = OwnerInbox::new(OWNER_INBOX_SHARDS);
    let pool = ReturnNodePool::new(16);
    let consumer = super::inbox::OwnerConsumer::new(OWNER_INBOX_SHARDS);

    let message_a = ReturnMessage {
        next: None,
        target,
        kind: ReturnKind::RawSlot,
        descriptor: first.slot.descriptor,
        unit: first.slot.index,
        bytes: descriptor_stride(&world, class),
        source_epoch: Epoch::from_raw(0),
        state: MessageState::Staged,
        integrity: IntegrityTag {
            generation: first.slot.generation,
            class,
            owner_id: target.owner_id,
            route_key: target.route_key,
            checksum: 0,
        },
    };
    let message_b = ReturnMessage {
        unit: second.slot.index,
        descriptor: second.slot.descriptor,
        integrity: IntegrityTag {
            generation: second.slot.generation,
            ..message_a.integrity
        },
        ..message_a
    };

    let mut staging_a = ProducerStaging::new(BatchLimits::default());
    let mut staging_b = ProducerStaging::new(BatchLimits::default());
    stage_message(&pool, None, &mut staging_a, &message_a, shard(1), None).expect("暂存 A");
    stage_message(&pool, None, &mut staging_b, &message_b, shard(1), None).expect("暂存 B");

    let chain_a = staging_a.drain().expect("chain A");
    let chain_b = staging_b.drain().expect("chain B");
    assert_ne!(
        chain_a.first, chain_b.first,
        "两个 producer 使用各自独立的 node"
    );

    let mut session_b = inbox.prepare_chain(&chain_b, &pool).expect("准备 B");
    inbox.exchange_tail(&mut session_b).expect("交换 B");
    let mut session_a = inbox.prepare_chain(&chain_a, &pool).expect("准备 A");
    inbox.exchange_tail(&mut session_a).expect("交换 A");
    inbox.link_old_tail(&mut session_a, &pool).expect("链接 A");
    inbox.publish_front(&session_a).expect("发布 A");
    inbox.link_old_tail(&mut session_b, &pool).expect("链接 B");
    inbox.publish_front(&session_b).expect("发布 B");

    let snapshot = inbox.snapshot(shard(1), &consumer, &ServiceBudget::new(8, 1 << 16), &pool);
    assert_eq!(snapshot.nodes().len(), 2);
    let seen: BTreeSet<u32> = snapshot.nodes().iter().map(|node| node.raw()).collect();
    assert_eq!(seen.len(), 2, "两个 producer 的 node 都必须可见");

    let mut consumer = consumer;
    inbox.advance_front(shard(1), &mut consumer, &snapshot);
    let second_snapshot =
        inbox.snapshot(shard(1), &consumer, &ServiceBudget::new(8, 1 << 16), &pool);
    assert!(second_snapshot.is_empty());
    assert_eq!(second_snapshot.stop(), DrainStop::NullLink);
    assert!(inbox.stats(shard(1)).phantom_nulls() >= 1);
}

#[test]
fn batch_flush_limits_and_target_change() {
    let limits = BatchLimits {
        items: 12,
        batch_soft_bytes: 1 << 20,
    };
    let mut world = world(2, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let mut staging = ProducerStaging::new(limits);
    let first = world.token(0);
    let second = world.token(1);
    let mut outcomes: Vec<PublishOutcome> = Vec::new();
    for index in 0..16_u32 {
        let allocation = world.allocate(0, class).expect("分配成功");
        world
            .queue_return(
                0,
                allocation.slot,
                u64::from(descriptor_stride(&world, class)),
            )
            .expect("归还成功");
        let target = if index < 15 { first } else { second };
        let message = world
            .message(
                target,
                ReturnKind::RawSlot,
                allocation.slot,
                descriptor_stride(&world, class),
            )
            .expect("消息可构造");
        outcomes.extend(
            world
                .publish_message(&mut staging, &message, shard(0), None)
                .expect("发布成功"),
        );
    }
    assert!(
        outcomes
            .iter()
            .any(|outcome| outcome.trigger == FlushTrigger::ItemLimit),
        "item 上限必须触发发布"
    );
    assert!(
        outcomes
            .iter()
            .any(|outcome| outcome.trigger == FlushTrigger::TargetChanged),
        "目标改变必须强制刷新旧 chain"
    );

    let mut staging = ProducerStaging::new(BatchLimits {
        items: 1024,
        batch_soft_bytes: 64,
    });
    let mut byte_trigger = None;
    for _ in 0..4_u32 {
        let allocation = world.allocate(0, class).expect("分配成功");
        world
            .queue_return(
                0,
                allocation.slot,
                u64::from(descriptor_stride(&world, class)),
            )
            .expect("归还成功");
        let message = world
            .message(
                first,
                ReturnKind::RawSlot,
                allocation.slot,
                descriptor_stride(&world, class),
            )
            .expect("消息可构造");
        let produced = world
            .publish_message(&mut staging, &message, shard(0), None)
            .expect("发布成功");
        if let Some(outcome) = produced.last() {
            byte_trigger = Some(outcome.trigger);
            break;
        }
    }
    assert_eq!(byte_trigger, Some(FlushTrigger::ByteLimit));
}

#[test]
fn return_slab_cache_batches_by_source_slab() {
    let mut cache = ReturnSlabCache::new();
    let generation = SlabGeneration::from_raw(1);
    let first = (SlabDescriptorId::from_raw(0), generation);
    let second = (SlabDescriptorId::from_raw(8), generation);
    let third = (SlabDescriptorId::from_raw(16), generation);
    assert!(cache.insert(first, 0, 64).is_none());
    assert!(cache.insert(first, 1, 64).is_none());
    assert_eq!(cache.hits(), 1);
    assert!(cache.insert(second, 0, 4096).is_none());
    let closed = cache
        .insert(third, 0, 64)
        .expect("同一 set 的第三个 key 必须挤出积累量最大的 victim");
    assert_eq!(closed.reason, RingCloseReason::Victim);
    assert_eq!(closed.key, second);
    assert_eq!(cache.pending_bytes(), 128 + 64);
    assert_eq!(cache.misses(), 3);
    let all = cache.close_all(RingCloseReason::Maintenance);
    assert_eq!(all.len(), 2);
    assert!(
        all.iter()
            .all(|ring| ring.reason == RingCloseReason::Maintenance)
    );
    assert_eq!(cache.pending_bytes(), 0);
    assert_eq!(RETURN_SLAB_CACHE_SETS, 8);
}

#[test]
fn ring_close_batches_reach_owner_inbox() {
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let stride = u64::from(descriptor_stride(&world, class));
    let first = world.allocate(0, class).expect("分配成功");
    let second = world.allocate(0, class).expect("分配成功");
    world.queue_return(0, first.slot, stride).expect("归还");
    world.queue_return(0, second.slot, stride).expect("归还");
    let mut staging = ProducerStaging::new(BatchLimits::default());
    let target = world.token(0);
    let pool = world.pool();
    let node = pool.allocate().expect("node 可用");
    let message = world
        .message(
            target,
            ReturnKind::RawSlot,
            first.slot,
            descriptor_stride(&world, class),
        )
        .expect("消息可构造");
    pool.store(node, &message, message.integrity.checksum);
    pool.link(node, None);
    staging.stage(node, &message, shard(0)).expect("暂存成功");
    let mut cache = ReturnSlabCache::new();
    assert!(
        cache
            .insert(
                (second.slot.descriptor, second.slot.generation),
                second.slot.index,
                stride
            )
            .is_none()
    );
    let published = world
        .close_cache(0, &mut staging, &mut cache, RingCloseReason::PressureDrain)
        .expect("ring 关闭必须发布 batch");
    assert_eq!(published, 1);
    let report = world
        .service(0, shard(0), &ServiceBudget::new(8, 1 << 16))
        .expect("service 成功");
    assert_eq!(report.items, 1);
}

#[test]
fn stale_token_is_forwarded_to_published_target() {
    let mut world = world(2, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let stride = u64::from(descriptor_stride(&world, class));
    let allocation = world.allocate(0, class).expect("分配成功");
    world
        .queue_return(0, allocation.slot, stride)
        .expect("归还成功");
    let old = world.token(0);
    let new = world.token(1);
    world.begin_forwarding(0, new).expect("发布转发目标");
    let message = world
        .message(
            old,
            ReturnKind::RawSlot,
            allocation.slot,
            descriptor_stride(&world, class),
        )
        .expect("消息可构造");
    publish(&world, 0, &message);
    let report = world
        .service(0, shard(0), &ServiceBudget::new(8, 1 << 16))
        .expect("旧 owner 必须转发而不是消费");
    assert_eq!(report.forwarded, 1);
    assert_eq!(report.items, 1);
    let report = world
        .service(1, shard(0), &ServiceBudget::new(8, 1 << 16))
        .expect("新 owner 消费转发的消息");
    assert_eq!(report.items, 1);
    assert_eq!(report.forwarded, 0);
    assert_eq!(old.owner_id, world.token(0).owner_id);
}

#[test]
fn owner_retire_waits_for_queue_page_grace() {
    let mut world = world(2, 64);
    let inbox = world.inbox(0);
    let ticket = inbox.gate().begin_publish().expect("publish 区间可开启");
    let report = world
        .retire(0, world.domain_owner(), &ServiceBudget::new(8, 1 << 16))
        .expect("retire 必须返回 grace 状态");
    assert_eq!(report.grace, GraceOutcome::Pending);
    assert_eq!(
        inbox
            .gate()
            .begin_publish()
            .err()
            .map(|error| error.message().to_owned()),
        Some("queue-page grace 期间不能开始新 batch".to_owned())
    );
    inbox.gate().end_publish(ticket);
    let report = world
        .retire(0, world.domain_owner(), &ServiceBudget::new(8, 1 << 16))
        .expect("grace 收敛后 retire 完成");
    assert_eq!(report.grace, GraceOutcome::Converged);
    assert_eq!(
        world
            .directory()
            .record(world.token(0).owner_id)
            .expect("记录存在")
            .state
            .name(),
        "Retired"
    );
    let replacement = {
        let mut seed = RuntimeSeed::new(7);
        seed.next();
        seed.next()
    };
    let _ = replacement;
    assert!(
        world.token(1).owner_id.raw() > world.token(0).owner_id.raw(),
        "owner 编号不复用"
    );
    assert!(
        world.resource_token(1).owner_id.raw() > world.resource_token(0).owner_id.raw(),
        "resource owner 编号同样不复用"
    );
}

#[test]
fn ledger_categories_stay_mutually_exclusive() {
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let stride = u64::from(descriptor_stride(&world, class));
    let allocation = world.allocate(0, class).expect("分配成功");
    world.ledger_invariant(0).expect("分配后账本成立");
    world
        .queue_return(0, allocation.slot, stride)
        .expect("归还成功");
    let accounting = world.accounting(0);
    assert_eq!(accounting.pending_return_bytes(), stride);
    assert_eq!(accounting.committed_bytes(), RAW_SLAB_PAGE_BYTES);
    let message = world
        .message(
            world.token(0),
            ReturnKind::RawSlot,
            allocation.slot,
            descriptor_stride(&world, class),
        )
        .expect("消息可构造");
    publish(&world, 0, &message);
    world
        .service(0, shard(0), &ServiceBudget::new(8, 1 << 16))
        .expect("service 成功");
    let accounting = world.accounting(0);
    assert_eq!(accounting.pending_return_bytes(), 0);
    assert_eq!(accounting.owner_cache_bytes(), RAW_SLAB_PAGE_BYTES);
    world.ledger_invariant(0).expect("消费后账本成立");
}

#[test]
fn contract_rejects_address_fields_and_policy_drift() {
    let contract = RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        RawPlanePolicyV1::default(),
        RawPlaneDemand {
            coroutine_sites: 3,
            checked_entries: 2,
            suspend_points: 1,
            resource_sites: 1,
            runtime_raw_sites: 2,
            owners: 2,
            message_nodes: 0,
        },
        RawResourceDemand::default(),
        Rt0Demand::default(),
        SchedulerDemand {
            spawn_sites: 3,
            yield_sites: 0,
            suspend_points: 1,
        },
        WaitDemand::default(),
        SyncDemand::default(),
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    contract.verify().expect("契约必须自洽");
    assert_eq!(contract.class_count(), super::RAW_CLASS_LADDER.len() as u32);
    assert_eq!(contract.shard_count(), OWNER_INBOX_SHARDS);
    assert_eq!(contract.batch_limits().items, super::BATCH_MAX);
    assert_eq!(contract.grace_steps(), super::model::GRACE_STEPS);
    assert_eq!(contract.ledger_categories().len(), 5);
    assert_eq!(contract.schema(), super::model::RAW_MODEL_SCHEMA);
    assert_eq!(contract.platform().profile(), "linux");
    assert_eq!(contract.platform().op_count(), 13);
    assert_eq!(
        contract.platform().class_count(),
        EXTENT_CLASS_LADDER.len() as u32
    );
    assert!(!contract.dump().contains("managed-address"));

    let mut schema = MessageSchemaV1::runtime_raw();
    schema.fields.push(MessageFieldSchema {
        name: "zzz.payload".to_owned(),
        kind: FieldKind::ManagedAddress,
    });
    assert!(schema.verify().is_err());

    let mut missing = MessageSchemaV1::runtime_raw();
    missing
        .fields
        .retain(|field| field.kind != FieldKind::Integrity);
    assert!(missing.verify().is_err());

    let drifted = RawPlanePolicyV1 {
        shards: 4,
        ..RawPlanePolicyV1::default()
    };
    assert!(
        RuntimeRawContractV1::build(
            TargetName::X86_64Linux,
            drifted,
            RawPlaneDemand::default(),
            RawResourceDemand::default(),
            Rt0Demand::default(),
            SchedulerDemand::default(),
            WaitDemand::default(),
            SyncDemand::default(),
            PlatformProfile::from(TargetName::X86_64Linux),
        )
        .is_err()
    );
}

#[test]
fn contract_fingerprint_is_deterministic_and_policy_sensitive() {
    let demand = RawPlaneDemand {
        coroutine_sites: 1,
        checked_entries: 1,
        suspend_points: 0,
        resource_sites: 0,
        runtime_raw_sites: 1,
        owners: 1,
        message_nodes: 0,
    };
    let scheduler = SchedulerDemand {
        spawn_sites: 1,
        yield_sites: 0,
        suspend_points: 0,
    };
    let first = RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        RawPlanePolicyV1::default(),
        demand,
        RawResourceDemand::default(),
        Rt0Demand::default(),
        scheduler,
        WaitDemand::default(),
        SyncDemand::default(),
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    let second = RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        RawPlanePolicyV1::default(),
        demand,
        RawResourceDemand::default(),
        Rt0Demand::default(),
        scheduler,
        WaitDemand::default(),
        SyncDemand::default(),
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    assert_eq!(first.fingerprint(), second.fingerprint());
    assert_eq!(first.canonical_bytes(), second.canonical_bytes());
    let revised = RawPlanePolicyV1 {
        revision: 2,
        ..RawPlanePolicyV1::default()
    };
    let third = RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        revised,
        demand,
        RawResourceDemand::default(),
        Rt0Demand::default(),
        scheduler,
        WaitDemand::default(),
        SyncDemand::default(),
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    assert_ne!(first.fingerprint(), third.fingerprint());
    let windows = RuntimeRawContractV1::build(
        TargetName::X86_64Windows,
        RawPlanePolicyV1::default(),
        demand,
        RawResourceDemand::default(),
        Rt0Demand::default(),
        scheduler,
        WaitDemand::default(),
        SyncDemand::default(),
        PlatformProfile::from(TargetName::X86_64Windows),
    )
    .expect("契约可构建");
    assert_ne!(first.fingerprint(), windows.fingerprint());
}

#[test]
fn node_pool_reports_double_release() {
    let pool = ReturnNodePool::new(4);
    let node = pool.allocate().expect("node 可用");
    assert!(pool.in_use(node));
    pool.release(node).expect("首次释放成功");
    assert!(pool.release(node).is_err());
    let nodes: Vec<_> = (0..4).map(|_| pool.allocate().expect("容量足够")).collect();
    assert_eq!(nodes.len(), 4);
    assert!(pool.allocate().is_err());
}

#[test]
fn raw_invariant_uses_the_registered_diagnostic_code() {
    assert_eq!(
        crate::DiagnosticCode::RuntimeRawInvariant.to_string(),
        "E0058"
    );
    let error = super::model::RawModelError::from(RawInvariant::new(
        "span extent 不是 class stride 的整数倍",
    ));
    assert_eq!(
        error.diagnostic().code(),
        crate::DiagnosticCode::RuntimeRawInvariant
    );
}

#[test]
fn clear_mask_covers_pointer_and_secret_fields() {
    let table = RuntimeSizeClassTable::ladder(MemoryDomainId::RUNTIME_RAW).expect("阶梯可构建");
    for class in table.classes() {
        assert_eq!(
            class.clear_mask,
            ClearField::POINTER
                | ClearField::LENGTH
                | ClearField::SECRET
                | ClearField::RESOURCE_STATE
        );
        assert!(class.poison);
    }
}

#[test]
fn token_resolution_covers_forwarding_and_unknown_owners() {
    let mut seed = RuntimeSeed::new(3);
    let mut directory = super::slab::OwnerDirectory::new(Epoch::from_raw(0));
    let first = directory.register(&mut seed, MemoryDomainId::RUNTIME_RAW, Epoch::from_raw(0));
    let second = directory.register(&mut seed, MemoryDomainId::RUNTIME_RAW, Epoch::from_raw(0));
    assert_eq!(directory.resolve(&first), super::slab::Resolution::Match);
    directory
        .begin_drain(&first, Epoch::from_raw(1))
        .expect("drain 成功");
    assert_eq!(directory.resolve(&first), super::slab::Resolution::Match);
    directory
        .begin_forward(&first, second, Epoch::from_raw(2))
        .expect("forward 成功");
    assert_eq!(
        directory.resolve(&first),
        super::slab::Resolution::Forward(second)
    );
    let unknown = OwnerToken {
        owner_id: super::slab::OwnerId::from_raw(99),
        ..second
    };
    assert_eq!(
        directory.resolve(&unknown),
        super::slab::Resolution::Unknown
    );
    assert!(directory.retire(&second, Epoch::from_raw(3)).is_err());
    directory
        .retire(&first, Epoch::from_raw(3))
        .expect("retire 成功");
    assert_eq!(directory.resolve(&first), super::slab::Resolution::Retired);
}

#[test]
fn invalid_message_is_rejected_without_dropping_it() {
    let mut world = world(1, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let stride = u64::from(descriptor_stride(&world, class));
    let allocation = world.allocate(0, class).expect("分配成功");
    world
        .queue_return(0, allocation.slot, stride)
        .expect("归还成功");
    let mut message = world
        .message(
            world.token(0),
            ReturnKind::RawSlot,
            allocation.slot,
            descriptor_stride(&world, class),
        )
        .expect("消息可构造");
    message.integrity.checksum ^= 0xdead_beef;
    publish(&world, 0, &message);
    let error = world
        .service(0, shard(0), &ServiceBudget::new(8, 1 << 16))
        .expect_err("被破坏的消息必须进入不变量失败");
    assert!(error.message().contains("integrity"));
    assert_eq!(
        world
            .table()
            .state(allocation.slot.descriptor, allocation.slot.index),
        Ok(SlotState::ReturnQueued),
        "拒绝的消息不得丢弃 slot 状态"
    );
}

/// 资源分配样例：8-byte payload 的 File 句柄。
fn resource_shape() -> ResourceShape {
    ResourceShape {
        kind_id: 0,
        payload_bytes: 8,
        alignment: 8,
    }
}

#[test]
fn resource_class_ladder_uses_lease_policy_and_dedicated_header() {
    let world = world(2, 16);
    let classes = world.resource_classes();
    assert_eq!(classes.classes().len(), super::RESOURCE_CLASS_LADDER.len());
    for class in classes.classes() {
        assert!(class.is_resource());
        assert_eq!(
            class.header_bytes,
            super::size_class::RESOURCE_SLOT_HEADER_BYTES
        );
        assert_eq!(class.payload_bytes, class.slot_stride - class.header_bytes);
        assert_eq!(class.domain, MemoryDomainId::RESOURCE);
    }
    let adapted = classes
        .lookup(8, 8)
        .expect("存在可承载 8-byte payload 的 class");
    assert_eq!(adapted.slot_stride, 128);
}

#[test]
fn resource_copy_shares_cell_without_double_close() {
    let mut world = world(2, 16);
    let handle = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    world.resource_acquire(handle).expect("复制增加 lease");
    assert_eq!(world.resource_leases(handle).expect("lease 可读"), 2);
    assert_eq!(
        world.release_lease(0, handle).expect("结束一个 lease"),
        LeaseOutcome::StillLeased
    );
    assert_eq!(world.resource_cleanups(), 0);
    assert_eq!(
        world.release_lease(0, handle).expect("结束最后 lease"),
        LeaseOutcome::LastLease
    );
    assert_eq!(world.resource_cleanups(), 1);
    assert!(
        world.release_lease(0, handle).is_err(),
        "已回收的 cell 不能再结束 lease"
    );
    world.verify_resource_cells().expect("cell 表自洽");
}

#[test]
fn close_is_idempotent_and_shares_release_point() {
    let mut world = world(2, 16);
    let handle = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    assert_eq!(
        world.resource_close(0, handle).expect("首次 close"),
        CloseOutcome::ClosedNow
    );
    assert_eq!(
        world.resource_close(0, handle).expect("重复 close 幂等"),
        CloseOutcome::AlreadyClosed
    );
    assert_eq!(
        world.resource_cleanups(),
        1,
        "close 只建立一次 release 线性化点"
    );
    assert_eq!(
        world.release_lease(0, handle).expect("结束 lease"),
        LeaseOutcome::LastLease
    );
    assert_eq!(world.resource_cleanups(), 1);
    assert_eq!(
        world
            .table()
            .state(handle.descriptor, handle.index)
            .expect("slot 状态"),
        SlotState::Returned
    );
}

#[test]
fn close_before_last_lease_cleans_once_then_reclaims() {
    let mut world = world(2, 16);
    let handle = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    world.resource_acquire(handle).expect("复制增加 lease");
    assert_eq!(
        world.release_lease(0, handle).expect("结束一个 lease"),
        LeaseOutcome::StillLeased
    );
    assert_eq!(
        world.resource_close(0, handle).expect("close 建立关闭点"),
        CloseOutcome::ClosedNow
    );
    assert_eq!(world.resource_cleanups(), 1, "关闭时执行一次受限 cleanup");
    assert_eq!(
        world
            .table()
            .state(handle.descriptor, handle.index)
            .expect("slot 状态"),
        SlotState::Live,
        "仍有 lease 时不能回收 slot"
    );
    assert_eq!(
        world.release_lease(0, handle).expect("结束最后 lease"),
        LeaseOutcome::LastLease
    );
    assert_eq!(world.resource_cleanups(), 1);
    assert_eq!(
        world
            .table()
            .state(handle.descriptor, handle.index)
            .expect("slot 状态"),
        SlotState::Returned
    );
}

#[test]
fn publish_is_single_direction() {
    let mut world = world(2, 16);
    let handle = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    world.resource_publish(handle).expect("首次发布成功");
    assert!(world.resource_cell(handle).expect("cell 可读").is_shared());
    assert!(
        world.resource_publish(handle).is_err(),
        "状态不能回到 Local"
    );
    assert_eq!(
        world.release_lease(0, handle).expect("结束 lease"),
        LeaseOutcome::LastLease
    );
    assert_eq!(world.resource_cleanups(), 1);
}

#[test]
fn lease_overflow_is_rejected() {
    let registry = ReleaseRegistry::builtin().expect("release 目录可构建");
    let descriptor = registry.for_kind(0).expect("File 描述符");
    let id = SlabDescriptorId::from_raw(0);
    let mut cells = ResourceCellTable::new();
    cells.ensure(id, 1);
    cells
        .place(id, 0, &descriptor, 8, 3, 128, 0, 7)
        .expect("cell 登记成功");
    let saturated = ResourceCell {
        leases: u64::MAX,
        ..*cells.get(id, 0).expect("cell 可读")
    };
    cells.restore(id, 0, saturated).expect("恢复边界状态");
    assert!(cells.acquire(id, 0).is_err(), "lease 不能越过 u64 上界");
}

#[test]
fn stale_release_generation_is_rejected() {
    let registry = ReleaseRegistry::builtin().expect("release 目录可构建");
    let descriptor = registry.for_kind(0).expect("File 描述符");
    let id = SlabDescriptorId::from_raw(0);
    let mut cells = ResourceCellTable::new();
    cells.ensure(id, 1);
    cells
        .place(id, 0, &descriptor, 8, 3, 128, 0, 7)
        .expect("cell 登记成功");
    assert!(cells.request_release(id, 0).expect("入队点可用"));
    assert!(
        cells
            .complete_release(id, 0, SlabGeneration::from_raw(8), false)
            .is_err(),
        "过期 generation 的 release 必须被拒"
    );
    assert!(
        cells
            .complete_release(id, 0, SlabGeneration::from_raw(7), false)
            .expect("匹配 generation")
    );
    assert!(
        !cells
            .complete_release(id, 0, SlabGeneration::from_raw(7), false)
            .expect("重复调用"),
        "受限 cleanup 不得重复执行"
    );
}

#[test]
fn restricted_release_descriptor_rejects_forbidden_capabilities() {
    let registry = ReleaseRegistry::builtin().expect("release 目录可构建");
    let descriptor = registry.for_kind(0).expect("File 描述符");
    descriptor.verify(&registry).expect("登记描述符必须自洽");
    for flag in [
        ReleaseFlags::HAS_MANAGED_PAYLOAD,
        ReleaseFlags::CAPTURES_OWNER,
        ReleaseFlags::MAY_ALLOCATE,
        ReleaseFlags::MAY_PANIC,
        ReleaseFlags::ACQUIRES_LOCK,
        ReleaseFlags::AWAITS_CHANNEL,
        ReleaseFlags::SPAWNS,
    ] {
        let forbidden = ReleaseDescriptor {
            flags: flag,
            ..descriptor
        };
        assert!(forbidden.verify(&registry).is_err());
    }
    let unregistered = ReleaseDescriptor {
        release_glue: 9999,
        ..descriptor
    };
    assert!(unregistered.verify(&registry).is_err());
}

#[test]
fn resource_descriptor_cannot_enter_region_reset() {
    let mut world = world(2, 16);
    let handle = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    assert!(
        resource::reject_region_reset(world.table(), handle.descriptor).is_err(),
        "资源 slot 不能进入整区 reset"
    );
    let raw = world
        .allocate(0, RuntimeSizeClassId::from_raw(0))
        .expect("raw 分配成功");
    resource::reject_region_reset(world.table(), raw.slot.descriptor)
        .expect("raw 记录不受资源隔离约束");
}

#[test]
fn panic_unwind_releases_only_unshared_cells() {
    let mut world = world(2, 16);
    let first = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    let shared = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    world.resource_publish(shared).expect("共享发布");
    assert_eq!(world.panic_unwind(0).expect("展开释放"), 1);
    assert_eq!(world.resource_cleanups(), 1);
    assert_eq!(world.resource_leases(first).expect("lease 可读"), 0);
    assert!(world.resource_cell(shared).expect("cell 可读").is_shared());
    assert_eq!(world.resource_leases(shared).expect("lease 可读"), 1);
}

#[test]
fn detach_ends_lease_with_detached_cleanup() {
    let mut world = world(2, 16);
    let handle = world
        .allocate_resource(
            0,
            ResourceShape {
                kind_id: 2,
                ..resource_shape()
            },
        )
        .expect("进程资源分配成功");
    assert_eq!(
        world.resource_detach(0, handle).expect("detach 结束 lease"),
        LeaseOutcome::LastLease
    );
    assert_eq!(world.resource_cleanups(), 1);
    let record = world.release_records().first().expect("存在 release 记录");
    assert!(record.detached, "detach 语义必须进入受限 release 记录");
}

#[test]
fn shutdown_reclaims_every_resource_slot() {
    let mut world = world(2, 16);
    let first = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    let second = world
        .allocate_resource(1, resource_shape())
        .expect("资源分配成功");
    world.resource_publish(second).expect("共享发布");
    assert_eq!(world.shutdown().expect("进程终止排空"), 2);
    assert_eq!(world.resource_cleanups(), 2);
    assert_eq!(world.pending_release_requests(), 0);
    for handle in [first, second] {
        assert_eq!(
            world
                .table()
                .state(handle.descriptor, handle.index)
                .expect("slot 状态"),
            SlotState::Returned
        );
    }
    world.verify_resource_cells().expect("cell 表自洽");
    world.resource_ledger_invariant(0).expect("资源账本守恒");
    world.resource_ledger_invariant(1).expect("资源账本守恒");
}

#[test]
fn remote_release_waits_for_owner_service_and_grace() {
    let mut world = world(2, 32);
    let handle = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    assert_eq!(
        world.release_lease(1, handle).expect("跨 owner 结束 lease"),
        LeaseOutcome::LastLease
    );
    assert_eq!(
        world.resource_cleanups(),
        1,
        "入队点赢家执行一次受限 cleanup"
    );
    assert_eq!(
        world
            .table()
            .state(handle.descriptor, handle.index)
            .expect("slot 状态"),
        SlotState::ReturnQueued
    );
    let report = world
        .service(0, shard(0), &ServiceBudget::new(8, 1 << 16))
        .expect("owner service 成功");
    assert_eq!(report.items, 1);
    assert_eq!(
        world
            .table()
            .state(handle.descriptor, handle.index)
            .expect("slot 状态"),
        SlotState::Returned
    );
    assert_eq!(world.resource_cleanups(), 1);
    world.release_graced_nodes().expect("grace 后可复用 node");
    world.verify_resource_cells().expect("cell 表自洽");
    world.resource_ledger_invariant(0).expect("资源账本守恒");
}

#[test]
fn dedicated_mapping_rounds_to_whole_page() {
    let mut world = world(2, 16);
    let shape = ResourceShape {
        kind_id: 4,
        payload_bytes: 8192,
        alignment: 8,
    };
    let handle = world
        .allocate_resource(0, shape)
        .expect("专用 mapping 分配成功");
    let descriptor = world
        .table()
        .descriptor(handle.descriptor)
        .expect("描述符存在");
    assert_eq!(descriptor.slot_count(), 1);
    assert_eq!(descriptor.slot_stride as u64, super::RAW_SLAB_PAGE_BYTES);
    assert_eq!(descriptor.domain, MemoryDomainId::RESOURCE);
    assert_eq!(
        world.release_lease(0, handle).expect("结束 lease"),
        LeaseOutcome::LastLease
    );
    assert_eq!(world.resource_cleanups(), 1);
    assert_eq!(
        world
            .table()
            .state(handle.descriptor, handle.index)
            .expect("slot 状态"),
        SlotState::Returned
    );
    let committed = world.provider_stats().committed_bytes;
    let reused = world
        .allocate_resource(0, shape)
        .expect("dedicated slot 可复用");
    assert_eq!(
        (reused.descriptor, reused.index),
        (handle.descriptor, handle.index)
    );
    assert_eq!(world.provider_stats().committed_bytes, committed);
    world.release_lease(0, reused).expect("释放复用 slot");
}

#[test]
fn stale_resource_handle_is_rejected_after_slot_reuse() {
    let mut world = world(2, 16);
    let first = world
        .allocate_resource(0, resource_shape())
        .expect("首次资源分配成功");
    world.release_lease(0, first).expect("首次资源释放成功");
    let second = world
        .allocate_resource(0, resource_shape())
        .expect("复用资源 slot 成功");
    assert_eq!(
        (first.descriptor, first.index),
        (second.descriptor, second.index)
    );
    assert_ne!(first.cell_generation, second.cell_generation);
    assert!(world.resource_acquire(first).is_err());
    assert!(world.resource_close(0, first).is_err());
    assert!(world.resource_publish(first).is_err());
    assert!(world.release_lease(0, first).is_err());
    assert_eq!(world.resource_leases(second).expect("新句柄可读"), 1);
}

#[test]
fn closed_resource_rejects_new_lease() {
    let mut world = world(2, 16);
    let handle = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    world.resource_acquire(handle).expect("复制 lease");
    world
        .resource_close(0, handle)
        .expect("建立 close 线性化点");
    assert!(world.resource_acquire(handle).is_err());
}

#[test]
fn shutdown_drains_pending_remote_resource_release() {
    let mut world = world(2, 16);
    let handle = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    world.release_lease(1, handle).expect("跨 owner 结束 lease");
    assert_eq!(
        world.table().state(handle.descriptor, handle.index),
        Ok(SlotState::ReturnQueued)
    );
    assert_eq!(world.shutdown().expect("shutdown 排空远程 release"), 1);
    assert_eq!(
        world.table().state(handle.descriptor, handle.index),
        Ok(SlotState::Returned)
    );
    world.verify_resource_cells().expect("cell 表自洽");
    world.resource_ledger_invariant(0).expect("资源账本守恒");
}

#[test]
fn remote_release_publish_failure_rolls_back_slot() {
    let mut world = world(2, 0);
    let handle = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    assert!(world.release_lease(1, handle).is_err());
    assert_eq!(
        world.table().state(handle.descriptor, handle.index),
        Ok(SlotState::Live)
    );
    assert_eq!(world.resource_leases(handle).expect("回滚后 lease 可读"), 0);
    assert_eq!(world.shutdown().expect("shutdown 回收回滚 slot"), 1);
}

#[test]
fn foreign_release_reuses_cleanup_after_close() {
    let mut world = world(2, 16);
    let handle = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    world.resource_close(1, handle).expect("外来 close");
    let message = world
        .prepare_foreign_release(1, handle)
        .expect("复用既有 cleanup");
    world
        .publish_release_message(&message, shard(0))
        .expect("发布 foreign release");
    world
        .service(0, shard(0), &ServiceBudget::new(8, 1 << 16))
        .expect("owner 消费 release");
    assert_eq!(
        world.table().state(handle.descriptor, handle.index),
        Ok(SlotState::Returned)
    );
}

#[test]
fn resource_ledger_stays_mutually_exclusive() {
    let mut world = world(2, 16);
    let first = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    let _second = world
        .allocate_resource(0, resource_shape())
        .expect("资源分配成功");
    world.resource_ledger_invariant(0).expect("分配后账本守恒");
    world.release_lease(0, first).expect("结束 lease");
    world.resource_ledger_invariant(0).expect("回收后账本守恒");
    world.verify_resource_cells().expect("cell 表自洽");
}

#[test]
fn resource_header_layout_is_contiguous() {
    resource::verify_header_layout().expect("header 字段连续");
    let schema = CellHeaderSchemaV1::fixed();
    schema.verify().expect("header schema 自洽");
    assert_eq!(schema.header_bytes, resource::CELL_HEADER_BYTES);
    assert_eq!(schema.fields.len(), 12);
}

#[test]
fn contract_schema_three_carries_resource_and_platform_sections() {
    let contract = RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        RawPlanePolicyV1::default(),
        RawPlaneDemand::default(),
        RawResourceDemand {
            resource_sites: 2,
            acquire_sites: 3,
            release_sites: 2,
            transfer_sites: 1,
            finalize_sites: 0,
            owners: 1,
            kinds: 0,
        },
        Rt0Demand::default(),
        SchedulerDemand::default(),
        WaitDemand::default(),
        SyncDemand::default(),
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    assert_eq!(contract.schema(), super::model::RAW_MODEL_SCHEMA);
    assert_eq!(
        contract.resource_class_count(),
        super::RESOURCE_CLASS_LADDER.len() as u32
    );
    assert_eq!(
        contract.resource_kind_count(),
        resource::RESOURCE_KINDS.len() as u32
    );
    assert_eq!(
        contract.resources().header.header_bytes,
        resource::CELL_HEADER_BYTES
    );
    assert_eq!(
        contract.unified_release_entry(),
        resource::UNIFIED_RELEASE_ENTRY
    );
    assert_eq!(contract.resource_demand().release_sites, 2);
    contract.resources().verify().expect("资源契约段自洽");
    assert!(contract.dump().contains("resource-cell leases"));
    let mut drifted = contract.resources().release.clone();
    drifted.fields.push(MessageFieldSchema {
        name: "zzz.raw_pointer".to_owned(),
        kind: FieldKind::RawPointer,
    });
    assert!(drifted.verify().is_err(), "release 描述符不能携带地址");
}

#[test]
fn resource_contract_fingerprint_tracks_demand() {
    let base = RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        RawPlanePolicyV1::default(),
        RawPlaneDemand::default(),
        RawResourceDemand::default(),
        Rt0Demand::default(),
        SchedulerDemand::default(),
        WaitDemand::default(),
        SyncDemand::default(),
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    let revised = RuntimeRawContractV1::build(
        TargetName::X86_64Linux,
        RawPlanePolicyV1::default(),
        RawPlaneDemand::default(),
        RawResourceDemand {
            release_sites: 4,
            ..RawResourceDemand::default()
        },
        Rt0Demand::default(),
        SchedulerDemand::default(),
        WaitDemand::default(),
        SyncDemand::default(),
        PlatformProfile::from(TargetName::X86_64Linux),
    )
    .expect("契约可构建");
    assert_ne!(base.fingerprint(), revised.fingerprint());
}

#[test]
fn resource_schema_rejects_malformed_fixed_metadata() {
    let mut header = ResourceSchemaV1::fixed().header;
    header.fields[0].kind = super::model::CellFieldKind::PayloadSize;
    assert!(header.verify().is_err());

    let mut state = ResourceSchemaV1::fixed().states;
    state.transitions[0].to = "Bogus".to_owned();
    assert!(state.verify().is_err());

    let mut kinds = ResourceSchemaV1::fixed().kinds;
    kinds.kinds[0].name = "Bogus".to_owned();
    assert!(kinds.verify().is_err());

    let mut release = ResourceSchemaV1::fixed().release;
    release.fields[0].name = "Bogus".to_owned();
    assert!(release.verify().is_err());

    let mut overflow = ResourceSchemaV1::fixed().header;
    overflow.fields[0].bytes = u32::MAX;
    overflow.fields[1].offset = u32::MAX;
    overflow.fields[1].bytes = 1;
    let result = std::panic::catch_unwind(|| overflow.verify());
    assert!(result.is_ok());
    assert!(result.expect("header verify 不应 panic").is_err());
}

fn _raw_invariant_is_reported(error: RawInvariant) -> String {
    error.message().to_owned()
}
