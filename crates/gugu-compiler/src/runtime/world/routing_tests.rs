//! radix routing profile 的世界级确定性回归。
//!
//! 全部在进程内运行：radix 模式下 return batch 进入 routing 平面并逐跳交付到目标
//! owner inbox；direct 模式不分配 bucket 且路径不变；旧 topology 沿转发记录进入新
//! token；maintenance 期间发布被冻结、pending bytes 保持可见。

use super::super::routing::MaintenancePhase;
use super::RawWorld;
use super::heap_tests::{configured_world, gc_contract, gc_contract_with};
use crate::runtime::inbox::{ServiceBudget, ShardIndex};
use crate::runtime::message::{FlushTrigger, ReturnKind, RingCloseReason};
use crate::runtime::model::RawPlanePolicyV1;
use crate::runtime::routing_schema::{MAX_RADIX_LEVELS, RADIX_BUCKETS, RouteMode, RoutingPolicyV1};
use crate::runtime::size_class::RuntimeSizeClassId;
use crate::runtime::slab::OwnerToken;
use crate::runtime::{CompressionDemand, OWNER_INBOX_SHARDS, RuntimeRawContractV1};

/// 构建一个已配置 radix 路由的整体契约。
fn radix_contract() -> RuntimeRawContractV1 {
    gc_contract_with(
        RawPlanePolicyV1 {
            routing: RoutingPolicyV1 {
                mode: RouteMode::Radix,
            },
            ..RawPlanePolicyV1::default()
        },
        CompressionDemand::default(),
    )
}

/// 构建一个 radix 世界的服务预算。
fn budget() -> ServiceBudget {
    ServiceBudget::pressure(u32::MAX, u64::MAX)
}

/// 排空一个 owner 的全部 shard；返回累计 items 与 forwarded。
fn service_all(world: &mut RawWorld, owner: u32) -> (u32, u32) {
    let mut items = 0;
    let mut forwarded = 0;
    for shard in 0..OWNER_INBOX_SHARDS {
        let shard = ShardIndex::from_raw(shard).expect("shard 合法");
        let report = world
            .service(owner, shard, &budget())
            .expect("service 可运行");
        items += report.items;
        forwarded += report.forwarded;
    }
    (items, forwarded)
}

fn stride(world: &RawWorld, class: RuntimeSizeClassId) -> u32 {
    world
        .classes()
        .get(class)
        .expect("class 已登记")
        .slot_stride
}

/// 在 radix 世界里让 producer 0 向 `target` 发布一个 raw slot return，强制进入 radix staging。
fn publish_radix_return(world: &mut RawWorld, target: OwnerToken) {
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    world
        .queue_return(0, allocation.slot, u64::from(stride(world, class)))
        .expect("归还成功");
    let message = world
        .message(
            target,
            ReturnKind::RawSlot,
            allocation.slot,
            stride(world, class),
        )
        .expect("消息可构造");
    let outcomes = world
        .publish_message(
            0,
            &message,
            ShardIndex::from_raw(0).expect("shard 合法"),
            Some(FlushTrigger::Maintenance),
        )
        .expect("radix 发布成功");
    assert_eq!(outcomes.len(), 1, "forced 触发必须产生一次 staging flush");
}

/// radix 世界的 return batch 经两跳进入目标 owner inbox，被恰好消费一次。
#[test]
fn radix_world_delivers_returns_exactly_once() {
    let mut world = configured_world(&radix_contract(), 11, 2, 64);
    assert_eq!(
        world.routing_bucket_capacity(),
        (RADIX_BUCKETS * MAX_RADIX_LEVELS) as usize,
        "radix 模式分配分层 bucket 表"
    );
    let target = world.token(1);
    publish_radix_return(&mut world, target);
    assert_eq!(world.routing_pending_batches(), 1);
    assert_eq!(world.routing_stats().radix_batches, 1);
    // batch 还在 radix staging：目标 inbox 不可见。
    let (items, forwarded) = service_all(&mut world, 1);
    assert_eq!((items, forwarded), (0, 0));
    // 在飞字节计入 credit 快照；两跳交付后清零。
    assert!(world.routing_pending_bytes() > 0);
    let published = world.drain_routing().expect("radix 排空");
    assert_eq!(published, 1);
    assert_eq!(world.routing_pending_batches(), 0);
    assert_eq!(world.routing_pending_bytes(), 0);
    assert_eq!(world.routing_stats().remote_return_hops, 2);
    let (items, forwarded) = service_all(&mut world, 1);
    assert_eq!((items, forwarded), (1, 0), "目标 owner 恰好消费一次");
}

/// direct 世界的路径零改动：bucket 容量为 0，发布直达 inbox，路由统计保持全零。
#[test]
fn direct_world_keeps_zero_buckets_and_unchanged_path() {
    let mut world = configured_world(&gc_contract(), 7, 2, 64);
    assert_eq!(world.routing_bucket_capacity(), 0);
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    world
        .queue_return(0, allocation.slot, u64::from(stride(&world, class)))
        .expect("归还成功");
    let message = world
        .message(
            world.token(1),
            ReturnKind::RawSlot,
            allocation.slot,
            stride(&world, class),
        )
        .expect("消息可构造");
    world
        .publish_message(
            0,
            &message,
            ShardIndex::from_raw(0).expect("shard 合法"),
            Some(FlushTrigger::Maintenance),
        )
        .expect("direct 发布成功");
    assert_eq!(
        world.routing_pending_batches(),
        0,
        "direct 不经过 radix staging"
    );
    let (items, forwarded) = service_all(&mut world, 1);
    assert_eq!((items, forwarded), (1, 0), "目标 owner 消费");
    let stats = world.routing_stats();
    assert_eq!(
        (
            stats.radix_batches,
            stats.remote_return_hops,
            stats.maintenance_switches
        ),
        (0, 0, 0)
    );
}

/// 旧 topology：目标 owner 转发后，radix 在飞 batch 沿转发记录进入新 token。
#[test]
fn radix_world_forwards_pending_batches_into_new_token() {
    let mut world = configured_world(&radix_contract(), 11, 3, 64);
    let stale = world.token(1);
    publish_radix_return(&mut world, stale);
    world
        .begin_forwarding(1, world.token(2))
        .expect("转发目标已发布");
    let published = world.drain_routing().expect("radix 排空");
    assert_eq!(published, 1);
    assert_eq!(world.routing_stats().old_topology_drained_batches, 1);
    // 新 token 的 owner 消费改写后的消息；stale owner 的 inbox 保持为空。
    let (items, _) = service_all(&mut world, 2);
    assert_eq!(items, 1, "新 token 消费转发 batch");
    let (items, _) = service_all(&mut world, 1);
    assert_eq!(items, 0, "stale owner 无消息");
}

/// maintenance：radix → direct 切换排空在飞 batch，切换期间发布被冻结，之后恢复 direct。
#[test]
fn switch_to_direct_drains_and_restores_direct_path() {
    let mut world = configured_world(&radix_contract(), 11, 2, 64);
    let target = world.token(1);
    publish_radix_return(&mut world, target);
    assert_eq!(world.routing_pending_batches(), 1);
    let epoch = world
        .switch_routing_mode(RouteMode::Direct)
        .expect("切换到 direct");
    assert_eq!(epoch, 2, "maintenance epoch 在 publish-mode 前进一次");
    assert_eq!(world.routing_bucket_capacity(), 0);
    assert_eq!(world.routing_pending_batches(), 0);
    assert_eq!(world.routing_stats().maintenance_switches, 1);
    let (items, _) = service_all(&mut world, 1);
    assert_eq!(items, 1, "排空的 batch 已在目标 inbox");
    // direct 模式发布直达 inbox，不产生新的 radix 统计。
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    world
        .queue_return(0, allocation.slot, u64::from(stride(&world, class)))
        .expect("归还成功");
    let message = world
        .message(
            target,
            ReturnKind::RawSlot,
            allocation.slot,
            stride(&world, class),
        )
        .expect("消息可构造");
    world
        .publish_message(
            0,
            &message,
            ShardIndex::from_raw(0).expect("shard 合法"),
            Some(FlushTrigger::Maintenance),
        )
        .expect("direct 发布成功");
    assert_eq!(world.routing_pending_batches(), 0, "direct 发布不进平面");
    assert_eq!(world.routing_stats().radix_batches, 1, "radix 统计不再增长");
    let (items, _) = service_all(&mut world, 1);
    assert_eq!(items, 1, "direct 消费");
}

/// radix 模式的 ring 关闭批次同样进入 routing 平面并最终交付给聚合 owner。
#[test]
fn radix_world_routes_closed_rings() {
    let mut world = configured_world(&radix_contract(), 11, 2, 64);
    let class = RuntimeSizeClassId::from_raw(0);
    let stride = u64::from(stride(&world, class));
    for _ in 0..3_u32 {
        let allocation = world.allocate(0, class).expect("分配成功");
        world
            .queue_return(0, allocation.slot, stride)
            .expect("归还成功");
        let generation = world
            .descriptor(allocation.slot.descriptor)
            .expect("描述符存在")
            .generation;
        world
            .cache_return_slot(
                0,
                (allocation.slot.descriptor, generation),
                allocation.slot.index,
                stride,
            )
            .expect("cache 聚合成功");
    }
    assert_eq!(
        world.routing_pending_batches(),
        0,
        "ring 命中时只聚合不发布"
    );
    let closed = world
        .close_cache(0, RingCloseReason::Maintenance)
        .expect("ring 可关闭");
    assert_eq!(closed, 3);
    assert_eq!(
        world.routing_pending_batches(),
        1,
        "ring 关闭批次进入 radix staging"
    );
    let published = world.drain_routing().expect("radix 排空");
    assert_eq!(published, 1);
    // ring 批次的 target 是聚合 owner 自己。
    let (items, _) = service_all(&mut world, 0);
    assert_eq!(items, 3);
}

/// 世界构建入口的默认平面：direct、idle、不分配 bucket。
#[test]
fn world_construction_defaults_to_direct_plane() {
    let world = RawWorld::new(11, 2, 64, crate::runtime::message::BatchLimits::default())
        .expect("world 可创建");
    assert_eq!(
        world.routing_bucket_capacity(),
        0,
        "默认 direct 不分配 bucket"
    );
    assert_eq!(world.routing_maintenance(), MaintenancePhase::Idle);
    assert_eq!(world.routing_pending_batches(), 0);
}
