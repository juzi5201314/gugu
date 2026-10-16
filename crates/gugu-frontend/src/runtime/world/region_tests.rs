//! TurnRegion 契约、门禁、`RegionTransfer` 消息与 world 接入的确定性测试。

use super::RawWorld;
use super::region_impl::RegionEnd;
use crate::runtime::barrier_schema::MessageFamilyTag;
use crate::runtime::message::{
    BatchLimits, IntegrityTag, MessageState, RegionTransferBatch, ReturnNodePool,
};
use crate::runtime::region::{
    PromoteReason, RegionId, RegionPlane, RegionRegistry, RegionState, ResetOutcome, ResetRefusal,
};
use crate::runtime::region_schema::{
    REGION_CAPACITY_CLASSES, REGION_EXPORT_ALL, REGION_EXPORT_BITS, REGION_OBJECT_LIMIT,
    REGION_SCHEMA, REGION_STATE_NAMES, RegionExport, TurnRegionDemand, TurnRegionRuntimeContract,
    region_transfer_fields,
};
use crate::runtime::slab::{MemoryDomainId, OwnerGeneration, OwnerId, OwnerToken, RouteKey};

fn contract() -> TurnRegionRuntimeContract {
    TurnRegionRuntimeContract::build(TurnRegionDemand::default()).expect("契约可构建")
}

fn token(owner_id: u32) -> OwnerToken {
    OwnerToken {
        domain: MemoryDomainId::RUNTIME_RAW,
        owner_id: OwnerId::from_raw(u64::from(owner_id)),
        generation: OwnerGeneration::from_raw(0),
        route_key: RouteKey::from_raw(u64::from(owner_id) + 1),
    }
}

fn registry(owner_id: u32) -> RegionRegistry {
    RegionRegistry::new(token(owner_id), &contract())
}

#[test]
fn contract_registers_capacity_ladder_and_closed_summary() {
    let contract = contract();
    assert_eq!(contract.schema(), REGION_SCHEMA);
    assert_eq!(contract.object_limit(), REGION_OBJECT_LIMIT);
    assert_eq!(
        contract.capacity_class_count() as usize,
        REGION_CAPACITY_CLASSES.len()
    );
    assert_eq!(
        contract.export_bit_count() as usize,
        REGION_EXPORT_BITS.len()
    );
    assert_eq!(
        contract
            .capacity_class_bytes()
            .iter()
            .copied()
            .find(|class| *class >= 65),
        Some(128)
    );
    assert_eq!(contract.capacity_class_bytes().last(), Some(&32_768));
    assert_eq!(
        contract.states,
        REGION_STATE_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<String>>()
    );
    assert_eq!(contract.transfer_fields, region_transfer_fields());
    assert!(
        contract
            .dump()
            .contains("region-capacity-classes 64,128,256,512,1024,2048,4096,8192,16384,32768")
    );
    assert!(contract.dump().contains(
        "region-export-bits external-alias,resource-lease,ffi-address,pending-transfer,live-root"
    ));
    assert!(contract.dump().contains("region-states private,publishing"));
}

#[test]
fn contract_rejects_capacity_demand_beyond_ladder() {
    let demand = TurnRegionDemand {
        regions: 1,
        allocations: 1,
        max_region_bytes: 64 * 1024,
        ..TurnRegionDemand::default()
    };
    let error = TurnRegionRuntimeContract::build(demand).expect_err("超出阶梯必须被拒绝");
    assert!(error.to_string().contains("容量阶梯上界"));
}

#[test]
fn transfer_fields_carry_no_addresses() {
    let fields = region_transfer_fields();
    assert!(fields.iter().all(|field| !field.kind.carries_address()));
    let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "bytes",
            "cycle_epoch",
            "export_state",
            "family",
            "integrity",
            "region",
            "region_generation",
            "state",
            "target.domain",
            "target.generation",
            "target.owner_id",
            "target.route_key",
            "type_summary",
        ]
    );
}

#[test]
fn private_region_resets_only_when_summary_is_closed() {
    let mut registry = registry(0);
    let region = registry.open(96).expect("region 可建立");
    let descriptor = registry.descriptor(region).expect("descriptor 存在");
    assert_eq!(descriptor.capacity_bytes, 128);
    assert_eq!(descriptor.state, RegionState::Private);
    assert_eq!(registry.bump(region, 96, 1).expect("bump"), 0);
    registry.publish(region, 0).expect("闭合 summary 可发布");
    assert_eq!(
        registry.reset(region).expect("reset 可执行"),
        ResetOutcome::Reset {
            bytes: 96,
            objects: 1
        }
    );
    let descriptor = registry.descriptor(region).expect("descriptor 保留");
    assert_eq!(descriptor.state, RegionState::Reset);
    assert_eq!(registry.counters().resets, 1);
    assert_eq!(registry.counters().reset_bytes, 96);
    assert_eq!(registry.active(), 0);
}

#[test]
fn every_export_bit_refuses_reset_and_turns_into_promotion() {
    for export in RegionExport::ALL {
        let mut registry = registry(0);
        let region = registry.open(64).expect("region 可建立");
        registry.bump(region, 32, 1).expect("bump 可执行");
        registry.publish(region, 0).expect("发布");
        registry.observe(region, export.bit()).expect("记录事实");
        match registry.reset(region).expect("门禁可执行") {
            ResetOutcome::Refused(ResetRefusal::Summary(bits)) => {
                assert_eq!(bits, export.bit(), "{}", export.name());
            }
            other => panic!("{} 必须拒绝 reset：{other:?}", export.name()),
        }
        assert_eq!(registry.active(), 1);
        assert_eq!(registry.counters().refusals[2], 1);
        let bytes = registry
            .promote(region, PromoteReason::Summary)
            .expect("拒绝后可以保留");
        assert_eq!(bytes, 32);
        assert_eq!(
            registry.descriptor(region).expect("descriptor").state,
            RegionState::LocalPromote
        );
        assert_eq!(registry.counters().promotions, 1);
        assert_eq!(registry.counters().promoted_bytes, 32);
    }
}

#[test]
fn reset_is_refused_before_publish_and_while_lease_is_open() {
    let mut registry = registry(0);
    let region = registry.open(64).expect("region 可建立");
    match registry.reset(region).expect("门禁可执行") {
        ResetOutcome::Refused(ResetRefusal::State(RegionState::Private)) => {}
        other => panic!("未发布必须按状态拒绝：{other:?}"),
    }
    registry.publish(region, 0).expect("发布");
    let batch = registry
        .transfer(region, token(1), 7, 3, &[9; 32])
        .expect("移交可建立");
    assert_eq!(
        registry.descriptor(region).expect("descriptor").state,
        RegionState::RegionTransfer
    );
    match registry.reset(region).expect("门禁可执行") {
        ResetOutcome::Refused(ResetRefusal::State(RegionState::RegionTransfer)) => {}
        other => panic!("在途移交必须拒绝 reset：{other:?}"),
    }
    assert_eq!(registry.counters().refusals[0], 2);
    assert_eq!(registry.counters().transfer_bytes, u64::from(batch.bytes));
    assert_eq!(batch.source, OwnerId::from_raw(0));
    assert_eq!(batch.target, token(1));
    assert_eq!(batch.capacity_class, 0);
}

#[test]
fn transfer_round_trip_moves_generation_and_lease() {
    let mut sender = registry(0);
    let mut receiver = registry(1);
    let region = sender.open(200).expect("region 可建立");
    sender.bump(region, 200, 2).expect("bump");
    sender.publish(region, 0).expect("发布");
    let batch = sender
        .transfer(region, token(1), 5, 11, &[7; 32])
        .expect("移交可建立");
    assert_eq!(batch.capacity_class, 2);
    assert_eq!(batch.bytes, 200);
    assert_eq!(
        IntegrityTag::compute_region_transfer(&[7; 32], &batch),
        batch.integrity.checksum
    );
    let mut tampered = batch;
    tampered.integrity.checksum = 0;
    assert!(receiver.receive(&tampered, &[7; 32]).is_err());
    let adopted = receiver.receive(&batch, &[7; 32]).expect("接收方采纳消息");
    let descriptor = receiver.descriptor(adopted).expect("descriptor");
    assert_eq!(descriptor.state, RegionState::Received);
    assert_eq!(descriptor.used_bytes, 200);
    assert_eq!(descriptor.declared, 0);
    assert_eq!(receiver.counters().received, 1);
    assert_eq!(
        receiver.receive_reset(adopted).expect("接收方回收"),
        ResetOutcome::Reset {
            bytes: 200,
            objects: 0
        }
    );
    sender.confirm(region).expect("发送方确认");
    assert_eq!(sender.counters().transfer_bytes, 0);
}

#[test]
fn received_region_can_be_published_and_bumped_again() {
    let mut sender = registry(0);
    let mut receiver = registry(1);
    let region = sender.open(64).expect("region");
    sender.publish(region, 0).expect("发布");
    let batch = sender
        .transfer(region, token(1), 1, 1, &[3; 32])
        .expect("移交");
    let adopted = receiver.receive(&batch, &[3; 32]).expect("采纳");
    receiver.bump(adopted, 1, 1).expect("接收方可以继续分配");
    receiver.publish(adopted, 0).expect("接收方可以重新发布");
    assert!(matches!(
        receiver.reset(adopted).expect("接收方回收"),
        ResetOutcome::Reset { bytes: 1, .. }
    ));
}

#[test]
fn bump_and_open_enforce_capacity_and_object_limits() {
    let mut registry = registry(0);
    let error = registry.open(0).expect_err("空容量必须被拒绝");
    assert!(error.to_string().contains("空容量"));
    let error = registry.open(64 * 1024).expect_err("超出阶梯必须被拒绝");
    assert!(error.to_string().contains("容量超过登记阶梯上界"));
    let region = registry.open(64).expect("region");
    let error = registry
        .bump(region, 65, 1)
        .expect_err("超过容量必须被拒绝");
    assert!(error.to_string().contains("容量 class"));
    let error = registry
        .bump(region, 1, REGION_OBJECT_LIMIT + 1)
        .expect_err("对象上界必须被拒绝");
    assert!(error.to_string().contains("对象数超过上界"));
    for _ in 0..REGION_OBJECT_LIMIT {
        registry.bump(region, 1, 1).expect("容量内可分配");
    }
    assert_eq!(
        registry.descriptor(region).expect("descriptor").objects,
        REGION_OBJECT_LIMIT
    );
}

#[test]
fn publish_rejects_unregistered_export_bits() {
    let mut registry = registry(0);
    let region = registry.open(64).expect("region");
    let error = registry
        .publish(region, REGION_EXPORT_ALL + 1)
        .expect_err("未登记位必须被拒绝");
    assert!(error.to_string().contains("未登记位"));
    registry.publish(region, 0).expect("发布");
    let error = registry.publish(region, 0).expect_err("重复发布必须被拒绝");
    assert!(error.to_string().contains("重复发布"));
}

#[test]
fn plane_refuses_duplicate_in_flight_transfer() {
    let contract = contract();
    let mut plane = RegionPlane::new(&[token(0), token(1)], &contract);
    let region = plane.registry_mut(0).open(64).expect("region");
    plane.registry_mut(0).publish(region, 0).expect("发布");
    let batch = plane
        .registry_mut(0)
        .transfer(region, token(1), 1, 1, &[1; 32])
        .expect("移交");
    plane.enqueue(batch).expect("登记在途消息");
    assert_eq!(plane.pending(), 1);
    assert_eq!(plane.pending_bytes(), u64::from(batch.bytes));
    assert_eq!(plane.front(), Some(batch));
    let error = plane
        .enqueue(batch)
        .expect_err("同一 region 不能有两条在途消息");
    assert!(error.to_string().contains("在途 transfer"));
    assert_eq!(plane.take(token(1)), Some(batch));
    assert_eq!(plane.pending(), 0);
    assert_eq!(plane.active(1), 0);
}

#[test]
fn message_node_round_trip_preserves_region_transfer() {
    let pool = ReturnNodePool::new(4);
    let mut sender = registry(0);
    let region = sender.open(96).expect("region");
    sender.bump(region, 96, 3).expect("bump");
    sender.publish(region, 0).expect("发布");
    let batch: RegionTransferBatch = sender
        .transfer(region, token(1), 42, 7, &[5; 32])
        .expect("移交");
    let node = pool.allocate().expect("node 可分配");
    pool.store_region_transfer(node, &batch, batch.integrity.checksum);
    assert_eq!(pool.family_of(node), MessageFamilyTag::RegionTransfer);
    let loaded = pool.load_region_transfer(node);
    assert_eq!(loaded, batch);
    assert_eq!(loaded.state, MessageState::Staged);
    assert_eq!(loaded.type_summary.raw(), 42);
    assert_eq!(loaded.cycle_epoch, 7);
    assert_eq!(loaded.capacity_class, 1);
    assert_eq!(loaded.source, OwnerId::from_raw(0));
}

#[test]
fn world_region_end_commits_and_releases_owner_bytes() {
    let mut world = RawWorld::new(7, 2, 64, BatchLimits::default()).expect("world");
    world.configure_regions(&contract()).expect("配置 region");
    let before = world.accounting(0).committed_bytes();
    let region = world.region_open(0, 96).expect("region 可建立");
    assert_eq!(world.accounting(0).committed_bytes(), before + 128);
    world.region_bump(0, region, 96, 1).expect("bump");
    world.region_publish(0, region, 0).expect("发布");
    assert_eq!(
        world.region_end(0, region).expect("结束"),
        RegionEnd::Reset {
            bytes: 96,
            objects: 1
        }
    );
    assert_eq!(world.accounting(0).committed_bytes(), before);
    assert_eq!(world.region_active(0).expect("活跃数"), 0);
    assert_eq!(world.region_counters(0).expect("计数器").resets, 1);
    assert_eq!(world.region_states(0).expect("状态表").len(), 1);
}

#[test]
fn world_region_confine_turns_reset_into_promotion() {
    let mut world = RawWorld::new(7, 2, 64, BatchLimits::default()).expect("world");
    world.configure_regions(&contract()).expect("配置 region");
    let region = world.region_open(0, 64).expect("region");
    world.region_bump(0, region, 64, 1).expect("bump");
    world.region_publish(0, region, 0).expect("发布");
    world
        .region_confine(0, region, RegionExport::ResourceLease)
        .expect("记录 resource lease");
    match world.region_end(0, region).expect("结束") {
        RegionEnd::Promoted {
            bytes,
            reason: PromoteReason::Refused(ResetRefusal::Summary(bits)),
        } => {
            assert_eq!(bytes, 64);
            assert_eq!(bits, RegionExport::ResourceLease.bit());
            assert_eq!(
                RawWorld::region_refusal_name(ResetRefusal::Summary(bits)),
                "summary"
            );
        }
        other => panic!("resource lease 必须转 promotion：{other:?}"),
    }
    assert_eq!(world.region_counters(0).expect("计数器").promotions, 1);
    assert!(!world.region_is_open(0, region).expect("状态"));
    // 事实消失后同一个位不应再影响后续 region。
    let region = world.region_open(0, 64).expect("region");
    world.region_publish(0, region, 0).expect("发布");
    world
        .region_confine(0, region, RegionExport::FfiAddress)
        .expect("记录 FFI 地址");
    world
        .region_release_confine(0, region, RegionExport::FfiAddress)
        .expect("清除事实");
    assert_eq!(
        world.region_end(0, region).expect("结束"),
        RegionEnd::Reset {
            bytes: 0,
            objects: 0
        }
    );
}

#[test]
fn world_region_promotes_declared_open_summary() {
    let mut world = RawWorld::new(7, 2, 64, BatchLimits::default()).expect("world");
    world.configure_regions(&contract()).expect("配置 region");
    let region = world.region_open(0, 64).expect("region");
    world.region_bump(0, region, 64, 1).expect("bump");
    world
        .region_publish(0, region, RegionExport::ExternalAlias.bit())
        .expect("声明未闭合 summary");
    match world.region_end(0, region).expect("结束") {
        RegionEnd::Promoted {
            bytes,
            reason: PromoteReason::Summary,
        } => assert_eq!(bytes, 64),
        other => panic!("未闭合声明必须整区保留：{other:?}"),
    }
}

#[test]
fn world_region_transfer_moves_bytes_between_owners() {
    let mut world = RawWorld::new(7, 2, 64, BatchLimits::default()).expect("world");
    world.configure_regions(&contract()).expect("配置 region");
    let source_before = world.accounting(0).committed_bytes();
    let target_before = world.accounting(1).committed_bytes();
    let region = world.region_open(0, 96).expect("region");
    world.region_bump(0, region, 96, 1).expect("bump");
    world.region_publish(0, region, 0).expect("发布");
    world.region_transfer(0, region, 1, 3).expect("移交可发布");
    assert_eq!(world.region_pending_transfers().expect("在途数"), 1);
    // 发送方在确认前仍持有字节；接收方尚未采纳。
    assert_eq!(world.accounting(0).committed_bytes(), source_before + 128);
    let batch = world.region_pending_batch().expect("在途消息可取");
    assert_eq!(batch.bytes, 96);
    assert_eq!(batch.target, world.token(1));
    let adoption = world
        .service_region_transfer(1, &batch)
        .expect("接收方采纳");
    assert_eq!(adoption.bytes, 96);
    assert_eq!(adoption.source, world.token(0).owner_id);
    assert_eq!(world.accounting(0).committed_bytes(), source_before);
    assert_eq!(world.accounting(1).committed_bytes(), target_before + 128);
    assert_eq!(world.region_pending_transfers().expect("在途数"), 0);
    world.region_confirm(0, region).expect("发送方确认");
    assert_eq!(
        world
            .region_receive_end(1, adoption.region)
            .expect("接收方回收"),
        RegionEnd::Reset {
            bytes: 96,
            objects: 0
        }
    );
    assert_eq!(world.accounting(1).committed_bytes(), target_before);
    assert_eq!(world.region_active(1).expect("活跃数"), 0);
}

#[test]
fn world_region_requires_contract_configuration() {
    let world = RawWorld::new(7, 1, 64, BatchLimits::default()).expect("world");
    let error = world.region_active(0).expect_err("未配置必须失败");
    assert!(error.to_string().contains("未按契约配置"));
    let error = world.region_pending_batch().expect_err("未配置必须失败");
    assert!(error.to_string().contains("未按契约配置"));
}

#[test]
fn world_region_registry_index_matches_owner_table() {
    let mut world = RawWorld::new(7, 2, 64, BatchLimits::default()).expect("world");
    world.configure_regions(&contract()).expect("配置 region");
    let region = world.region_open(1, 64).expect("region");
    assert_eq!(world.region_active(0).expect("活跃数"), 0);
    assert_eq!(world.region_active(1).expect("活跃数"), 1);
    assert_eq!(
        world.region_plane().expect("plane").registry(1).owner(),
        world.token(1)
    );
    assert!(world.region_bump(0, region, 1, 1).is_err());
    let _: RegionId = region;
}
