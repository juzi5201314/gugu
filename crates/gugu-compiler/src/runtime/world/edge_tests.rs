//! `EdgeDelta` 的世界级回归：跨 owner 往返、乱序保留、generation 校验与管理权移交。

use crate::runtime::barrier::EdgeDeltaRecord;
use crate::runtime::inbox::{ServiceBudget, ShardIndex};
use crate::runtime::local_heap::BlockRef;
use crate::runtime::mark::MarkError;
use crate::runtime::mark_schema::GcCreditId;
use crate::runtime::message::{
    EdgeDelta, FlushTrigger, IntegrityTag, MessageState, RETURN_NODE_BYTES, stage_edge_delta,
};
use crate::runtime::world::RawWorld;
use crate::runtime::world::heap_impl::ManagedPlacement;
use crate::runtime::world::heap_tests::{configured_world, gc_contract};

/// 两个 owner、每个 owner 都有 managed arena 的世界。
fn two_owner_world() -> RawWorld {
    let contract = gc_contract();
    configured_world(&contract, 29, 2, 64)
}

/// 在 `owner` 上分配一个 old generation 的双指针对象。
fn node(world: &mut RawWorld, owner: u32) -> u64 {
    world
        .allocate_managed(owner, 0, 16, ManagedPlacement::Old)
        .expect("object 可分配")
}

fn budget() -> ServiceBudget {
    ServiceBudget::new(64, 1 << 20)
}

/// 返回当前在飞（含已 consume 待归还）的 GC credit 数。
fn pending_credits(world: &RawWorld) -> u64 {
    world.mark_plane().expect("mark 平面").mark_credit_pending()
}

/// 返回某个 block 当前 manager 的稳定身份。
fn manager_owner_id(world: &RawWorld, block: BlockRef) -> crate::runtime::slab::OwnerId {
    world
        .managed_arena_by_descriptor(block.id.arena())
        .expect("arena 已登记")
        .manager
        .owner_id
}

/// 测试专用的注入入口：直接构造一条指定序号的 `EdgeDelta` 并投递给 `target`。
///
/// 真实的发布路径总是按序号递增发出，因此乱序保留只有注入才能构造；这里复用生产代码的
/// 构造、integrity 计算与 staging 路径，不另写一套消息格式。
impl RawWorld {
    fn acquire_edge_credit(&mut self, source: u32, target: u32) -> GcCreditId {
        self.mark_plane_mut()
            .expect("mark 平面")
            .acquire_edge_delta(source, target)
            .expect("credit 可签发")
    }

    fn note_edge_dirty(&mut self, block: crate::runtime::local_heap::ManagedBlockId) {
        self.edge_plane_mut().expect("边平面").note_dirty(block);
    }

    fn stage_edge_record(
        &mut self,
        source: BlockRef,
        destination: BlockRef,
        sequence: u64,
        delta: i64,
        credit: GcCreditId,
        drain: bool,
    ) -> Result<(), crate::runtime::slab::RawInvariant> {
        let manager = self
            .managed_arena_by_descriptor(destination.id.arena())?
            .manager;
        let target_owner = manager.owner_id;
        let source_owner = self
            .managed_arena_by_descriptor(source.id.arena())?
            .heap_owner;
        let mut record = EdgeDelta {
            next: None,
            target: manager,
            source,
            destination,
            cycle_epoch: self.mark_plane()?.cycle(),
            topology_epoch: self.mark_plane()?.topology(),
            sequence,
            delta,
            credit,
            bytes: RETURN_NODE_BYTES,
            state: MessageState::Staged,
            integrity: IntegrityTag {
                generation: crate::runtime::slab::SlabGeneration::from_raw(
                    manager.generation.raw(),
                ),
                class: crate::runtime::size_class::RuntimeSizeClassId::from_raw(0),
                owner_id: target_owner,
                route_key: manager.route_key,
                checksum: 0,
            },
        };
        record.integrity.checksum =
            IntegrityTag::compute_edge_delta(self.integrity_secret(), &record);
        let slot = self
            .owners
            .iter()
            .position(|owner| owner.token().owner_id == target_owner)
            .expect("目标 owner 必须存在");
        let shard =
            ShardIndex::from_raw(u32::try_from(slot).expect("槽位适配 u32")).expect("shard 合法");
        let inbox = self.inbox(u32::try_from(slot).expect("槽位适配 u32"));
        stage_edge_delta(
            &self.pool,
            Some(&inbox),
            &mut self.return_stagings[source_owner as usize],
            &record,
            shard,
            Some(FlushTrigger::OwnerPressure),
        )?;
        if drain {
            self.drain_all(u32::try_from(slot).expect("槽位适配 u32"), &budget())?;
        }
        Ok(())
    }
}

#[test]
fn cross_owner_edge_delta_round_trips_and_settles_its_credit() {
    let mut world = two_owner_world();
    let source = node(&mut world, 0);
    let target = node(&mut world, 1);
    let _ = world.publish_edge_deltas().expect("可发布");

    world
        .store_managed_field(0, 0, source, 0, target)
        .expect("跨 owner 写入");
    let published: Vec<EdgeDeltaRecord> = world.publish_edge_deltas().expect("可发布");
    assert_eq!(published.len(), 1, "一次写入产生一条跨 owner 差量");
    assert_eq!(published[0].delta, 1);
    assert_eq!(pending_credits(&world), 1, "发布后 credit 在飞到目标 owner");
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(published[0].target),
        0,
        "发布不改变已应用计数：只有 target 消费后才应用"
    );

    // 目标 owner 消费：计数应用、credit 归还、dirty 位置位。
    let (_, consumed) = world.drain_all(1, &budget()).expect("目标 owner 可排空");
    assert!(consumed > 0, "目标 owner 必须真的消费了一条消息");
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(published[0].target),
        1,
        "target 侧已应用计数是候选判定的输入"
    );
    assert_eq!(pending_credits(&world), 0, "应用完成即结算 credit");
    assert!(
        world.edge_plane().expect("边平面").dirty_count() > 0,
        "应用必须置 dirty"
    );
    assert!(
        world.pending_grace_nodes() > 0,
        "已应用的 node 必须走既有 grace 路径"
    );
    world.release_graced_nodes().expect("node 可释放");
    world.ledger_invariant(0).expect("账本仍互斥");
    world.ledger_invariant(1).expect("账本仍互斥");
}

#[test]
fn intra_owner_cross_block_edge_is_published_and_applied_locally() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 31, 1, 64);
    let source = node(&mut world, 0);
    let source_block = world.managed_block_ref(0, source).expect("block 身份").id;
    // 填满 source 所在的 block，使 target 落到另一个 block。
    loop {
        let candidate = node(&mut world, 0);
        if world
            .managed_block_ref(0, candidate)
            .expect("block 身份")
            .id
            != source_block
        {
            break;
        }
    }
    let target = node(&mut world, 0);
    let _ = world.publish_edge_deltas().expect("可发布");
    world.note_edge_dirty(source_block);
    world
        .store_managed_field(0, 0, source, 0, target)
        .expect("同 owner 跨 block 写入");
    let published = world.publish_edge_deltas().expect("可发布");
    assert_eq!(published.len(), 1);
    // 同一 owner 内的跨 block 边同样走真实消息：drain 自己之后才能应用。
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(published[0].target),
        0
    );
    let (_, consumed) = world.drain_all(0, &budget()).expect("可排空");
    assert!(consumed > 0);
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(published[0].target),
        1
    );
    assert_eq!(pending_credits(&world), 0);
}

#[test]
fn out_of_order_sequence_is_held_and_released_when_filled() {
    let mut world = two_owner_world();
    let source = node(&mut world, 0);
    let target = node(&mut world, 1);
    let _ = world.publish_edge_deltas().expect("可发布");
    let block_source = world.managed_block_ref(0, source).expect("block 身份");
    let block_target = world.managed_block_ref(1, target).expect("block 身份");
    // 序号 2 先到：target 侧必须保留它，而不是应用或丢弃。
    let credit = world.acquire_edge_credit(0, 1);
    world
        .stage_edge_record(block_source, block_target, 2, 5, credit, true)
        .expect("可注入乱序记录");
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(block_target),
        0,
        "未来序号不得提前应用"
    );
    assert_eq!(pending_credits(&world), 1, "乱序记录继续占用它的 credit");
    assert_eq!(
        world.pending_grace_nodes(),
        0,
        "乱序记录的 node 不得提前 grace"
    );
    // 补齐缺口后，序号 2 与序号 1 一起应用，credit 全部结算。
    let credit = world.acquire_edge_credit(0, 1);
    world
        .stage_edge_record(block_source, block_target, 1, 1, credit, true)
        .expect("可注入缺口记录");
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(block_target),
        6,
        "缺口补齐时连带应用保留的记录"
    );
    assert_eq!(pending_credits(&world), 0, "两条记录的 credit 都已结算");
    world.release_graced_nodes().expect("node 可释放");
    world.ledger_invariant(0).expect("账本仍互斥");
    world.ledger_invariant(1).expect("账本仍互斥");
}

#[test]
fn stale_generation_and_wrong_family_are_rejected_before_any_state_change() {
    let mut world = two_owner_world();
    let source = node(&mut world, 0);
    let target = node(&mut world, 1);
    let _ = world.publish_edge_deltas().expect("可发布");
    let block_source = world.managed_block_ref(0, source).expect("block 身份");
    let block_target = world.managed_block_ref(1, target).expect("block 身份");
    let stale = BlockRef {
        id: block_target.id,
        generation: block_target.generation + 7,
    };
    let credit = world.acquire_edge_credit(0, 1);
    // 过期的 destination generation 必须在触碰平面状态之前被拒绝，并且不消耗队列项：
    // 这类消息是真正的不变量失败，drain 必须向上报错而不是静默跳过。
    assert!(
        world
            .stage_edge_record(block_source, stale, 1, 1, credit, true)
            .is_err(),
        "过期 generation 的记录不能被消费"
    );
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(block_target),
        0
    );
    // 错误族同样不能消耗 edge credit。
    assert_eq!(
        world
            .mark_plane_mut()
            .expect("mark 平面")
            .consume_ticket(1, credit, 1, 0),
        Err(MarkError::CreditFamilyMismatch {
            credit,
            expected: "edge-delta"
        })
    );
    assert_eq!(pending_credits(&world), 1, "失败的错误族不得改动状态");
    // 过期消息留在队列里：这类失败是真正的不变量破坏，不能靠“跳过一条”掩盖，
    // 因此后续 drain 仍然失败，平面状态保持不变。
    assert!(
        world.drain_all(1, &budget()).is_err(),
        "过期消息必须让 drain 持续失败"
    );
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(block_target),
        0
    );
}

#[test]
fn malformed_record_aborts_the_drain_instead_of_skipping_the_queue() {
    let mut world = two_owner_world();
    let source = node(&mut world, 0);
    let target = node(&mut world, 1);
    let _ = world.publish_edge_deltas().expect("可发布");
    let block_source = world.managed_block_ref(0, source).expect("block 身份");
    let block_target = world.managed_block_ref(1, target).expect("block 身份");
    let stale = BlockRef {
        id: block_target.id,
        generation: block_target.generation + 7,
    };
    let credit = world.acquire_edge_credit(0, 1);
    assert!(
        world
            .stage_edge_record(block_source, stale, 1, 1, credit, true)
            .is_err()
    );
    // 合法记录排在错误消息之后也无法被消费：队列不会被静默跳过。
    let credit = world.acquire_edge_credit(0, 1);
    let _ = world.stage_edge_record(block_source, block_target, 1, 2, credit, false);
    assert!(
        world.drain_all(1, &budget()).is_err(),
        "错误消息必须让 drain 失败"
    );
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(block_target),
        0,
        "整条队列都不得被部分应用"
    );
}

#[test]
fn retired_owner_forwards_in_flight_edges_to_the_new_manager() {
    let mut world = two_owner_world();
    let source = node(&mut world, 0);
    let target = node(&mut world, 1);
    let _ = world.publish_edge_deltas().expect("可发布");
    let block_target = world.managed_block_ref(1, target).expect("block 身份");
    let new_manager = world.token(0);
    // owner 1 退役到 owner 0：它的 managed arena 管理权随之转移。
    world
        .retire(1, new_manager, &budget())
        .expect("owner 可退役");
    assert_eq!(
        manager_owner_id(&world, block_target),
        new_manager.owner_id,
        "管理权必须已经转移"
    );
    // 转移之后写入 source→target：差量必须发给新 manager，而不是已退役的 owner 1。
    world
        .store_managed_field(0, 0, source, 0, target)
        .expect("写入");
    let published = world.publish_edge_deltas().expect("可发布");
    assert_eq!(published.len(), 1);
    let (_, consumed) = world.drain_all(0, &budget()).expect("新 manager 可排空");
    assert!(consumed > 0, "差量必须投递给新 manager");
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(published[0].target),
        1
    );
    assert_eq!(pending_credits(&world), 0);
}

#[test]
fn stale_manager_envelope_is_forwarded_to_the_current_manager() {
    let mut world = two_owner_world();
    let source = node(&mut world, 0);
    let target = node(&mut world, 1);
    let _ = world.publish_edge_deltas().expect("可发布");
    let block_source = world.managed_block_ref(0, source).expect("block 身份");
    let block_target = world.managed_block_ref(1, target).expect("block 身份");
    // 先发出记录（目标 owner 1），再转移管理权，最后才消费：消费时必须转投给新 manager。
    let credit = world.acquire_edge_credit(0, 1);
    world
        .stage_edge_record(block_source, block_target, 1, 3, credit, false)
        .expect("可注入记录");
    let new_manager = world.token(0);
    let owner_one = world.token(1);
    world
        .handover_managed_arenas(1, new_manager)
        .expect("管理权可转移");
    let (_, consumed) = world.drain_all(1, &budget()).expect("旧 owner 可排空");
    assert!(consumed > 0, "旧 manager 必须先接住这条消息再转发");
    assert_eq!(
        pending_credits(&world),
        1,
        "转投后 credit 仍在飞，等新 manager 消费"
    );
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(block_target),
        0,
        "转投不等于应用"
    );
    let (_, consumed) = world.drain_all(0, &budget()).expect("新 manager 可排空");
    assert!(consumed > 0, "新 manager 必须收到转投的消息");
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(block_target),
        3
    );
    assert_eq!(pending_credits(&world), 0);
    assert_ne!(new_manager.owner_id, owner_one.owner_id);
}

/// 真实源码派生的 EdgeNode 夹具必须在 Old placement 上驱动跨 owner 边增量。
#[test]
fn source_edge_node_fixture_drives_old_placement_edge_deltas() {
    use crate::runtime::gc_metadata_section::decode_sections;
    use crate::{CompileRequest, Compiler, TargetName};

    let source = include_str!("../fixtures/edge_nodes.gg");
    let compiler = Compiler::new();
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let compile = || compiler.compile(CompileRequest::single_file("main.gg", source, target));
        let cold = compile();
        let warm = compile();
        assert!(cold.is_success(), "{:?}", cold.diagnostics().items());
        let plan = cold.image_plan().expect("成功编译有 image-plan");
        // 夹具的每个 managed store 都必须进入边需求，否则用例覆盖不到边协议本身。
        assert_eq!(
            plan.edge_demand().edge_sites,
            plan.barrier_demand().edge_summary_sites,
            "边站点需求必须与 barrier 需求同源"
        );
        assert_eq!(plan.edge_demand().edge_sites, 6, "夹具的写入站点进入边需求");
        assert_ne!(plan.edge_contract_fingerprint(), [0_u8; 32]);
        assert_eq!(
            cold.action_key(),
            warm.action_key(),
            "内容寻址 key 必须一致"
        );
        let types = decode_sections(plan.gc_type_section(), plan.gc_metadata_section())
            .expect("section 可解码");
        // 类型表同时登记同名占位项（size 0），这里取真正带布局的记录。
        let candidates: Vec<(usize, &str, u64)> = types
            .types()
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.contains("Edge"))
            .map(|(index, entry)| (index, entry.name.as_str(), entry.size))
            .collect();
        let node = types
            .types()
            .iter()
            .filter(|entry| entry.name.ends_with("EdgeNode"))
            .max_by_key(|entry| entry.size)
            .unwrap_or_else(|| panic!("真实类型表必须包含夹具的 EdgeNode：{candidates:?}"));
        // `struct EdgeNode { value: uint, next: &EdgeNodeTail }`：uint 在前，引用字段在 offset 8。
        assert_eq!(node.size, 16, "夹具布局必须是 uint 加一个指针字");
        assert!(
            types
                .types()
                .iter()
                .any(|entry| entry.name.ends_with("EdgeNodeTail")),
            "引用目标类型必须同样进入类型表"
        );
    }

    // 真实契约驱动 world：用夹具派生的类型在 old generation 建立跨 owner 引用。
    let compilation = compiler.compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let plan = compilation.image_plan().expect("镜像计划");
    let types = decode_sections(plan.gc_type_section(), plan.gc_metadata_section())
        .expect("section 可解码");
    // 与计划断言一致：同名占位项不参与，取带布局的记录。
    let node_entry = types
        .types()
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.name.ends_with("EdgeNode"))
        .max_by_key(|(_, entry)| entry.size)
        .expect("EdgeNode 必须存在");
    let tail_entry = types
        .types()
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.name.ends_with("EdgeNodeTail"))
        .max_by_key(|(_, entry)| entry.size)
        .expect("EdgeNodeTail 必须存在");
    let node_type = u32::try_from(node_entry.0).expect("类型下标适配 u32");
    let tail_type = u32::try_from(tail_entry.0).expect("类型下标适配 u32");
    let node_size = node_entry.1.size;
    let tail_size = tail_entry.1.size;
    assert_eq!(
        (node_size, tail_size),
        (16, 8),
        "夹具布局：EdgeNode 是 uint 加一个引用字，EdgeNodeTail 只有一个 uint"
    );
    let contract = compilation.raw_contract().expect("真实契约");
    let mut world = RawWorld::new(37, 2, 64, crate::runtime::message::BatchLimits::default())
        .expect("world 可创建");
    world.configure_gc(contract).expect("真实契约可配置");
    let source_address = world
        .allocate_managed(0, node_type, node_size, ManagedPlacement::Old)
        .expect("EdgeNode 可在 old 分配");
    let target_address = world
        .allocate_managed(1, tail_type, tail_size, ManagedPlacement::Old)
        .expect("EdgeNodeTail 可在 old 分配");
    let _ = world.publish_edge_deltas().expect("基线可排空");
    // 跨 owner 的引用字段写入：必须经过 hybrid barrier 并产生边增量。
    world
        .store_managed_field(0, 0, source_address, 8, target_address)
        .expect("跨 owner 引用字段可写");
    let published: Vec<EdgeDeltaRecord> = world.publish_edge_deltas().expect("可发布");
    assert_eq!(published.len(), 1, "一次跨 owner 写入产生一条差量");
    assert_eq!(published[0].delta, 1);
    let (_, consumed) = world.drain_all(1, &budget()).expect("目标 owner 可排空");
    assert!(consumed > 0, "目标 owner 必须消费差量");
    assert_eq!(
        world
            .edge_plane()
            .expect("边平面")
            .incoming_applied(published[0].target),
        1,
        "目标侧已应用计数必须来自真实消费"
    );
    assert_eq!(pending_credits(&world), 0, "应用完成即结算 credit");
    world.release_graced_nodes().expect("node 可释放");
    world.ledger_invariant(0).expect("账本仍互斥");
    world.ledger_invariant(1).expect("账本仍互斥");
}
