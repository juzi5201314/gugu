//! hybrid write barrier、remembered set 与 edge summary 的确定性回归；不睡眠、不读熵。

use super::RawWorld;
use crate::runtime::barrier::{
    BarrierFlushReason, BarrierPlane, BarrierSite, CardKey, CardMarkDraft, EdgeChange, EdgeSummary,
    HybridStep,
};
use crate::runtime::barrier_schema::{
    BARRIER_SCHEMA, BarrierDemand, BarrierRuntimeContract, CARD_GRANULARITY_BYTES,
    CARD_MARK_BUFFER_ENTRIES, CARD_MARK_STAMP_ENTRIES, EDGE_BUFFER_ENTRIES, EDGE_DELTAS_PER_WRITE,
    GC_ARENA_CARDS, HYBRID_BARRIER_STEPS, MessageFamilyTag, arena_card, card_index, stamp_slot,
};
use crate::runtime::gc_metadata_contract::{GC_ARENA_BYTES, GcMetadataRuntimeContract};
use crate::runtime::gc_metadata_schema::GcMetadataDemand;
use crate::runtime::inbox::ServiceBudget;
use crate::runtime::local_heap::{BlockRef, ManagedBlockId};
use crate::runtime::message::BatchLimits;
use crate::runtime::size_class::RuntimeSizeClassId;
use crate::runtime::slab::SlabDescriptorId;
use crate::runtime::{GcMetadataDemand as _UnusedGcMetadataDemand, PlatformProfile};
use crate::{CompileRequest, Compiler, TargetName};

/// 一个稳定 block 身份；测试用固定 generation。
fn block(id: u32) -> BlockRef {
    BlockRef {
        id: ManagedBlockId(id),
        generation: 1,
    }
}

fn manager() -> crate::runtime::slab::OwnerToken {
    crate::runtime::slab::OwnerToken {
        domain: crate::runtime::slab::MemoryDomainId::RUNTIME_RAW,
        owner_id: crate::runtime::slab::OwnerId::from_raw(1),
        generation: crate::runtime::slab::OwnerGeneration::from_raw(1),
        route_key: crate::runtime::slab::RouteKey::from_raw(1),
    }
}

fn world(owners: u32) -> RawWorld {
    let mut world = RawWorld::new(7, owners, 64, BatchLimits::default()).expect("raw world");
    // 平面 epoch 与站点 epoch 必须一致；测试统一在 cycle 1 上记账。
    world.advance_barrier_epoch(0, 1).expect("推进到 cycle 1");
    world
}

/// 一道卡片测试用的写入：目标与被写对象同 block，因此不产生跨 block 边。
fn site(arena: u64, generation: u32, offset: u64, epoch: u64) -> BarrierSite {
    edge_site(
        arena,
        generation,
        offset,
        epoch,
        Some(block(3)),
        Some(block(3)),
    )
}

/// 一道显式指定 old/new block 身份的写入。
fn edge_site(
    arena: u64,
    generation: u32,
    offset: u64,
    epoch: u64,
    old: Option<BlockRef>,
    new: Option<BlockRef>,
) -> BarrierSite {
    BarrierSite {
        arena_descriptor: arena,
        arena_generation: generation,
        offset,
        cycle_epoch: epoch,
        source: block(3),
        old,
        new,
        new_in_nursery: true,
        owner_old: true,
        marking: true,
        stack_grey: true,
    }
}

#[test]
fn barrier_contract_is_self_consistent_and_rejects_drift() {
    let contract = BarrierRuntimeContract::build(BarrierDemand::default()).expect("契约可构建");
    assert_eq!(contract.schema(), BARRIER_SCHEMA);
    assert_eq!(contract.card_granularity_bytes(), CARD_GRANULARITY_BYTES);
    assert_eq!(
        contract.card_mark_buffer_entries(),
        CARD_MARK_BUFFER_ENTRIES
    );
    assert_eq!(contract.card_mark_stamp_entries(), CARD_MARK_STAMP_ENTRIES);
    assert_eq!(contract.flush_reason_count(), 6);
    assert_eq!(contract.card_mark_batch_field_count(), 13);
    assert_eq!(contract.record_count(), 4);
    // edge scratch 与 shade 额度同源：region 内每条写入最多两条边变更。
    assert_eq!(contract.edge_deltas_per_write, EDGE_DELTAS_PER_WRITE);
    assert_eq!(contract.edge_buffer_entries, EDGE_BUFFER_ENTRIES);
    assert_eq!(
        contract.edge_deltas_per_write,
        contract.shade_slots_per_write
    );
    assert!(contract.edge_buffer_entries >= contract.edge_deltas_per_write);
    assert_eq!(
        GC_ARENA_CARDS,
        GC_ARENA_BYTES / u64::from(CARD_GRANULARITY_BYTES)
    );
    assert_ne!(contract.fingerprint(), [0_u8; 32]);
    contract.verify().expect("契约自洽");

    // 六步序列中 store 必须早于 card-mark：调换后 verifier 拒绝。
    let mut drifted = contract.clone();
    drifted.hybrid_steps.swap(3, 4);
    assert!(drifted.verify().is_err(), "store 晚于账本必须被拒绝");

    let mut drifted = contract.clone();
    drifted.card_granularity_bytes = 256;
    assert!(drifted.verify().is_err(), "card 粒度漂移必须被拒绝");

    let mut drifted = contract.clone();
    drifted.card_mark_buffer_entries = 512;
    assert!(drifted.verify().is_err(), "buffer 容量漂移必须被拒绝");

    let mut drifted = contract.clone();
    drifted.flush_reasons.pop();
    assert!(drifted.verify().is_err(), "flush 原因目录必须完整");

    let mut drifted = contract.clone();
    drifted
        .message
        .fields
        .push(crate::runtime::model::MessageFieldSchema {
            name: "zzz.managed".to_owned(),
            kind: crate::runtime::model::FieldKind::ManagedAddress,
        });
    assert!(drifted.verify().is_err(), "card batch 不允许携带地址");
}

#[test]
fn barrier_demand_changes_fingerprint_and_enters_contract() {
    let base = BarrierRuntimeContract::build(BarrierDemand::default()).expect("契约可构建");
    let demand = BarrierDemand {
        reserved_barriers: 2,
        regions: 2,
        permits: 2,
        max_shades_permit: 2,
        max_card_marks_permit: 1,
        shade_slots: 4,
        card_mark_slots: 2,
        edge_summary_sites: 3,
        card_mark_sites: 2,
        ..BarrierDemand::default()
    };
    let drifted = BarrierRuntimeContract::build(demand).expect("契约可构建");
    assert_ne!(base.fingerprint(), drifted.fingerprint());
    assert_eq!(drifted.demand().reserved_barriers, 2);
    assert!(
        drifted
            .dump()
            .contains("barrier schema=3 card=512 buffer=256 stamps=256")
    );
    assert!(drifted.dump().contains("edges-per-write=2 edge-buffer=512"));
    assert!(drifted.dump().contains("read-old -> shade-old-deleted"));
    assert!(
        drifted
            .dump()
            .contains("buffer-full,processor-handoff,foreign-bridge")
    );
}

#[test]
fn hybrid_barrier_stores_before_it_publishes_the_ledger() {
    let mut plane = BarrierPlane::new(4);
    let outcome = plane
        .perform_barrier(0, site(1, 7, 0x40, 4))
        .expect("写屏障成功");
    let store = outcome
        .steps
        .iter()
        .position(|step| *step == HybridStep::Store)
        .expect("必须执行 store");
    let card_mark = outcome
        .steps
        .iter()
        .position(|step| *step == HybridStep::CardMark)
        .expect("必须执行 card-mark");
    assert!(store < card_mark, "实际 store 必须先于 barrier 账本发布");
    assert!(outcome.shaded_old && outcome.shaded_new);
    assert!(outcome.card_marked);
    assert_eq!(plane.processor(0).expect("账本").card_marks(), 1);
    assert_eq!(
        plane.processor(0).expect("账本").buffer().pending_bytes(),
        u64::from(CARD_GRANULARITY_BYTES)
    );
    // 标记关闭时两个 shade 步被同一 flag 折叠，业务写入仍完成。
    let mut quiet = site(1, 7, 0x40, 4);
    quiet.marking = false;
    let outcome = plane.perform_barrier(0, quiet).expect("写屏障成功");
    assert!(!outcome.shaded_old && !outcome.shaded_new);
    assert!(outcome.steps.contains(&HybridStep::Store));
    // 新值不在 nursery 时不产生 card 键。
    let mut old_new = site(1, 7, 0x40, 4);
    old_new.new_in_nursery = false;
    let outcome = plane.perform_barrier(0, old_new).expect("写屏障成功");
    assert!(!outcome.card_marked);
    assert!(!outcome.steps.contains(&HybridStep::CardMark));
}

#[test]
fn dedup_reuses_a_slot_and_stamp_conflicts_never_drop_keys() {
    let mut plane = BarrierPlane::new(1);
    let first = site(1, 3, 512, 1);
    plane.perform_barrier(0, first).expect("写屏障成功");
    // 同一 (arena, generation, card) 重复写入只占一个 slot。
    plane.perform_barrier(0, first).expect("写屏障成功");
    let record = plane.processor(0).expect("账本");
    assert_eq!(record.buffer().len(), 1);
    assert_eq!(record.card_slot_reuses(), 1);
    assert_eq!(record.card_marks(), 2);

    // 构造两个撞到同一 stamp 槽但 card 不同的键：两个键都必须进入 buffer。
    let slot = stamp_slot(1, 3, 0);
    let mut collided = None;
    for card in 1..4096_u32 {
        if stamp_slot(1, 3, card) == slot {
            collided = Some(card);
            break;
        }
    }
    let collided = collided.expect("256 项直接映射必然能找到冲突 card");
    plane
        .perform_barrier(0, site(1, 3, u64::from(collided) * 512, 1))
        .expect("写屏障成功");
    let record = plane.processor(0).expect("账本");
    assert_eq!(record.buffer().len(), 2, "stamp 冲突不得丢弃尚未发布的键");
    plane
        .register_arena(1, manager(), 3, GC_ARENA_BYTES)
        .expect("登记 arena");
    let drafts = plane
        .flush_processor(0, BarrierFlushReason::BufferFull)
        .expect("flush 可执行");
    assert_eq!(drafts.len(), 2, "两个不相邻 card 各自成一条 batch");
    assert_eq!(
        plane.table(1).expect("card table").dirty(),
        0,
        "flush 尚未发布时 card table 保持干净"
    );
    for draft in &drafts {
        plane.consume_locally(1, draft).expect("owner 本地消费");
    }
    assert_eq!(plane.table(1).expect("card table").dirty(), 2);
}

#[test]
fn repeated_keys_flush_as_one_merged_range() {
    let mut plane = BarrierPlane::new(2);
    plane
        .register_arena(5, manager(), 9, GC_ARENA_BYTES)
        .expect("登记 arena");
    for card in 0..4_u64 {
        plane
            .perform_barrier(0, site(5, 9, card * 512, 2))
            .expect("写屏障成功");
    }
    let drafts = plane
        .flush_processor(0, BarrierFlushReason::ProducerStopGate)
        .expect("flush 可执行");
    assert_eq!(drafts.len(), 1, "连续 card 合并成一条 batch");
    assert_eq!(drafts[0].card_start, 0);
    assert_eq!(drafts[0].card_count, 4);
    assert_eq!(drafts[0].bytes, 4);
    let marked = plane.consume_locally(5, &drafts[0]).expect("消费 batch");
    assert_eq!(marked, 4);
    // 幂等：重复消费同一条 batch 不重复计 dirty。
    let marked = plane.consume_locally(5, &drafts[0]).expect("重复消费");
    assert_eq!(marked, 4);
    assert_eq!(plane.table(5).expect("card table").dirty(), 4);
}

#[test]
fn buffer_bound_forces_a_flush_and_rejects_overflow() {
    let mut plane = BarrierPlane::new(3);
    for card in 0..CARD_MARK_BUFFER_ENTRIES {
        let outcome = plane
            .perform_barrier(0, site(2, 4, u64::from(card) * 512, 3))
            .expect("写屏障成功");
        assert!(outcome.flush.is_none(), "额度内不得触发 flush");
    }
    let outcome = plane
        .perform_barrier(0, site(2, 4, u64::from(CARD_MARK_BUFFER_ENTRIES) * 512, 3))
        .expect("写屏障成功");
    assert_eq!(outcome.flush, Some(BarrierFlushReason::BufferFull));
    assert!(!outcome.card_marked, "超额的写入不能在 fast path 上记账");
    assert!(outcome.steps.contains(&HybridStep::Store), "store 已发生");
    let drafts = plane
        .flush_processor(0, BarrierFlushReason::BufferFull)
        .expect("flush 可执行");
    let total: u32 = drafts.iter().map(|draft| draft.card_count).sum();
    assert_eq!(total, CARD_MARK_BUFFER_ENTRIES);
    // flush 后同一键可以重新进入 buffer。
    let outcome = plane
        .perform_barrier(0, site(2, 4, 0, 3))
        .expect("写屏障成功");
    assert!(outcome.card_marked);

    // 满 buffer 上重复写入有两个合法结局，且都不能静默丢键：stamp 仍指向该键时命中
    // dedup 并返回成功；stamp 已被冲突覆盖时保守地报 `BufferFull`，由调用方在 region
    // 外 flush。两条路径都不新增 slot。
    let mut plane = BarrierPlane::new(3);
    for card in 0..CARD_MARK_BUFFER_ENTRIES {
        plane
            .perform_barrier(0, site(2, 4, u64::from(card) * 512, 3))
            .expect("写屏障成功");
    }
    let repeat = plane
        .perform_barrier(0, site(2, 4, 0, 3))
        .expect("重复键不越界");
    assert!(
        repeat.card_marked || repeat.flush == Some(BarrierFlushReason::BufferFull),
        "满 buffer 上的重复写入要么命中 dedup，要么保守地要求 flush"
    );
    assert_eq!(
        plane.processor(0).expect("账本").buffer().len(),
        CARD_MARK_BUFFER_ENTRIES,
        "重复写入不得新增 slot"
    );
    // 小规模写入时 stamp 不冲突，重复键必然命中 dedup 且不触发 flush。
    let mut plane = BarrierPlane::new(3);
    plane
        .perform_barrier(0, site(2, 4, 512, 3))
        .expect("写屏障成功");
    let repeat = plane
        .perform_barrier(0, site(2, 4, 512, 3))
        .expect("写屏障成功");
    assert!(repeat.card_marked, "无冲突时重复键命中 dedup");
    assert!(repeat.flush.is_none(), "无冲突时不触发 flush");
    assert_eq!(plane.processor(0).expect("账本").buffer().len(), 1);
}

#[test]
fn epoch_change_flushes_and_never_drops_old_keys() {
    let mut plane = BarrierPlane::new(1);
    plane
        .perform_barrier(0, site(1, 1, 512, 1))
        .expect("写屏障成功");
    assert_eq!(plane.processor(0).expect("账本").buffer().len(), 1);
    // epoch 前进必须先交出旧键：草稿属于旧 cycle，绝不能被静默丢弃。
    let drafts = plane
        .advance_epoch(2, BarrierFlushReason::MinorStop)
        .expect("epoch 前进");
    assert_eq!(drafts.len(), 1, "旧 cycle 的键必须由 epoch 前进交出");
    assert_eq!(drafts[0].cycle_epoch, 1);
    assert_eq!(drafts[0].card_start, 1);
    assert!(plane.processor(0).expect("账本").buffer().is_empty());
    assert_eq!(plane.processor(0).expect("账本").buffer().cycle_epoch(), 2);
    assert_eq!(plane.cycle_epoch(), 2);
    // 旧 epoch 的键不能再用旧 buffer 的键掩盖新 cycle 的脏度。
    plane
        .perform_barrier(0, site(1, 1, 512, 2))
        .expect("写屏障成功");
    let record = plane.processor(0).expect("账本");
    assert_eq!(record.buffer().len(), 1);
    assert_eq!(record.buffer().cycle_epoch(), 2);
    // 站点 epoch 落后于平面时拒绝记账，而不是就地改写 epoch 丢掉旧键。
    assert!(plane.perform_barrier(0, site(1, 1, 1024, 1)).is_err());
    // 平面自身也运行时拒绝 epoch 回退，不依赖 debug 断言。
    assert!(
        plane
            .advance_epoch(1, BarrierFlushReason::MinorStop)
            .is_err()
    );
}

#[test]
fn six_flush_reasons_are_reachable() {
    for (index, reason) in BarrierFlushReason::ALL.into_iter().enumerate() {
        let mut plane = BarrierPlane::new(1);
        plane
            .perform_barrier(0, site(1, 1, 512, 1))
            .expect("写屏障成功");
        let drafts = plane.flush_processor(0, reason).expect("flush 可执行");
        assert_eq!(drafts.len(), 1, "{} 必须冲刷已记账的键", reason.name());
        assert_eq!(plane.processor(0).expect("账本").last_flush(), Some(reason));
        assert!(plane.processor(0).expect("账本").buffer().is_empty());
        assert_eq!(reason.index(), index, "原因下标与 ALL 顺序一致");
        let mut expected = [0_u64; 6];
        expected[index] = 1;
        assert_eq!(plane.flushed_by_reason(), expected);
        assert_eq!(plane.flushes(), 1);
        assert_eq!(plane.empty_flushes(), 0);
    }
    // 空 buffer 的 flush 不产生 batch，但同样计入次数与原因分类。
    let mut plane = BarrierPlane::new(1);
    assert!(
        plane
            .flush_processor(0, BarrierFlushReason::MemoryPressure)
            .expect("flush 可执行")
            .is_empty()
    );
    assert_eq!(
        plane.processor(0).expect("账本").last_flush(),
        Some(BarrierFlushReason::MemoryPressure)
    );
    assert_eq!(plane.flushes(), 1);
    assert_eq!(plane.empty_flushes(), 1);
    assert_eq!(
        plane.flushed_by_reason()[BarrierFlushReason::MemoryPressure.index()],
        1
    );
}

#[test]
fn card_batch_crosses_owner_and_is_consumed_once() {
    let mut world = world(2);
    world.register_managed_arena(1, 3, 11).expect("登记 arena");
    // 站点 epoch 必须与平面一致：先推进平面，再记账。
    world.advance_barrier_epoch(0, 5).expect("推进到 cycle 5");
    world
        .perform_barrier(0, site(3, 11, 1024, 5))
        .expect("写屏障成功");
    let published = world
        .flush_barrier(0, 0, BarrierFlushReason::ProcessorHandoff)
        .expect("发布 batch");
    assert_eq!(published, 1, "非 owner processor 必须发布 CardMarkBatch");
    let stats = world.barrier_stats();
    assert_eq!(stats.published_batches, 1);
    assert_eq!(world.barrier().pending_batch_bytes(), 1);
    assert!(
        !world.barrier().minor_scan_ready(),
        "未消费 batch 阻挡 minor 扫描"
    );

    let report = world
        .service(
            1,
            crate::runtime::inbox::ShardIndex::from_raw(1).expect("shard"),
            &ServiceBudget::new(8, 1 << 16),
        )
        .expect("owner 消费 card batch");
    assert_eq!(report.items, 1);
    let table = world.barrier().table(3).expect("card table");
    assert_eq!(table.dirty(), 1);
    assert_eq!(world.barrier().consumed_batches(), 1);
    assert!(world.barrier().minor_scan_ready());
    // 幂等：重复置位不改变 dirty 计数。
    world
        .perform_barrier(0, site(3, 11, 1024, 5))
        .expect("写屏障成功");
    world
        .flush_barrier(0, 0, BarrierFlushReason::MinorStop)
        .expect("再次发布");
    world
        .service(
            1,
            crate::runtime::inbox::ShardIndex::from_raw(1).expect("shard"),
            &ServiceBudget::new(8, 1 << 16),
        )
        .expect("再次消费");
    assert_eq!(world.barrier().table(3).expect("card table").dirty(), 1);
}

#[test]
fn arena_owner_writes_its_card_table_without_a_message() {
    let mut world = world(1);
    world.register_managed_arena(0, 2, 3).expect("登记 arena");
    world
        .perform_barrier(0, site(2, 3, 2048, 1))
        .expect("写屏障成功");
    let published = world
        .flush_barrier(0, 0, BarrierFlushReason::MemoryPressure)
        .expect("本地合并写");
    assert_eq!(published, 0, "arena owner 不经过跨 owner 通道");
    assert_eq!(world.barrier().table(2).expect("card table").dirty(), 1);
    assert_eq!(world.barrier().pending_batch_bytes(), 0);
}

#[test]
fn generation_mismatch_and_wrong_owner_are_invariant_violations() {
    let mut world = world(2);
    world.register_managed_arena(1, 4, 6).expect("登记 arena");

    // generation 漂移：owner 消费必须拒绝。
    let draft = CardMarkDraft {
        arena_descriptor: 4,
        arena_generation: 7,
        card_start: 0,
        card_count: 1,
        cycle_epoch: 1,
        bytes: 1,
    };
    assert!(world.barrier_mut().consume_locally(4, &draft).is_err());

    // 越界 card 区间必须拒绝。
    let draft = CardMarkDraft {
        arena_descriptor: 4,
        arena_generation: 6,
        card_start: u32::try_from(GC_ARENA_CARDS).expect("card 数量适配 u32") - 1,
        card_count: 4,
        cycle_epoch: 1,
        bytes: 4,
    };
    assert!(world.barrier_mut().consume_locally(4, &draft).is_err());

    // 错误 owner：owner 0 不是该 arena 的 manager。
    let batch = crate::runtime::message::CardMarkBatch {
        next: None,
        target: world.token(0),
        arena: SlabDescriptorId::from_raw(4),
        arena_generation: 6,
        card_start: 0,
        card_count: 1,
        cycle_epoch: 1,
        bytes: 1,
        state: crate::runtime::message::MessageState::Published,
        integrity: crate::runtime::message::IntegrityTag {
            generation: crate::runtime::slab::SlabGeneration::from_raw(6),
            class: RuntimeSizeClassId::from_raw(0),
            owner_id: world.token(0).owner_id,
            route_key: world.token(0).route_key,
            checksum: 0,
        },
    };
    assert!(world.service_card_mark(0, &batch).is_err());
}

#[test]
fn minor_stop_gate_requires_every_buffer_flushed() {
    let mut world = world(1);
    world.register_managed_arena(0, 2, 1).expect("登记 arena");
    world
        .perform_barrier(0, site(2, 1, 512, 1))
        .expect("写屏障成功");
    world
        .perform_barrier(1, site(2, 1, 512, 1))
        .expect("写屏障成功");
    // 第二个 processor 仍有记账，minor 请求必须先把两个 buffer 都冲刷。
    let ready = world.request_minor_stop(0).expect("minor stop 请求");
    assert!(ready, "冲刷完成后 minor 门禁开放");
    assert!(
        world
            .barrier()
            .table(2)
            .expect("card table")
            .minor_pending()
    );
    let cards = world
        .barrier_mut()
        .table_mut(2)
        .expect("card table")
        .swap_dirty();
    assert_eq!(cards.len(), 1);
    assert_eq!(world.barrier().table(2).expect("card table").dirty(), 0);
}

#[test]
fn edge_summary_counts_multiple_field_edges_and_publishes_net_delta() {
    let source = block(1);
    let target = block(2);
    let mut edges = EdgeSummary::default();
    // 同一个 block 对的两条字段边必须各自计数：删掉一条之后仍有边。
    assert!(
        edges
            .apply(EdgeChange {
                source,
                target,
                delta: 1
            })
            .expect("加边")
    );
    assert!(
        !edges
            .apply(EdgeChange {
                source,
                target,
                delta: 1
            })
            .expect("加边")
    );
    assert_eq!(
        edges.incoming_leases(target),
        1,
        "同一对 block 只算一份 incoming lease"
    );
    let published = edges.publish(10).expect("发布");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].delta, 2, "两条字段边合并成净差量 2");
    assert_eq!(published[0].sequence, 1);
    assert!(
        edges.publish(10).expect("再发布").is_empty(),
        "净零不再发布"
    );

    // 删掉一条：仍然活跃，但净差量为 -1。
    edges
        .apply(EdgeChange {
            source,
            target,
            delta: -1,
        })
        .expect("删边");
    assert_eq!(edges.incoming_leases(target), 1, "仍有一条活边");
    let published = edges.publish(11).expect("发布");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].delta, -1);
    assert_eq!(published[0].sequence, 2);

    // 删掉最后一条：incoming lease 归零，随后清退零计数对。
    edges
        .apply(EdgeChange {
            source,
            target,
            delta: -1,
        })
        .expect("删边");
    assert_eq!(edges.incoming_leases(target), 0);
    let published = edges.publish(12).expect("发布");
    assert_eq!(published[0].delta, -1);
    assert_eq!(published[0].sequence, 3);
    assert_eq!(edges.retire_zero_pairs(), 1, "零计数对必须被清退");
    assert_eq!(edges.pending(), 0);

    // 计数下溢是真正的不变量失败，不能饱和成零。
    assert!(
        edges
            .apply(EdgeChange {
                source,
                target,
                delta: -1
            })
            .is_err(),
        "未记录的删边必须失败"
    );

    // 同一 epoch 内 add 与 drop 净零：不分配记录也不消耗序号。
    let mut edges = EdgeSummary::default();
    edges
        .apply(EdgeChange {
            source,
            target,
            delta: 1,
        })
        .expect("加边");
    edges
        .apply(EdgeChange {
            source,
            target,
            delta: -1,
        })
        .expect("删边");
    assert_eq!(edges.pending(), 0);
    assert!(edges.publish(5).expect("发布").is_empty());
    assert_eq!(edges.incoming_leases(target), 0);
}

#[test]
fn barrier_plane_counts_arena_metadata_per_unit() {
    let contract = BarrierRuntimeContract::build(BarrierDemand::default()).expect("契约");
    let pressure = &contract.pressure;
    assert_eq!(
        pressure.per_processor_bytes,
        pressure.buffer_bytes + pressure.stamp_bytes
    );
    assert_eq!(pressure.arena_total(2), GC_ARENA_CARDS * 2);
    assert_eq!(
        pressure.processor_total(3),
        pressure.per_processor_bytes * 3
    );
}

#[test]
fn card_mark_message_schema_matches_registered_fields() {
    let contract = BarrierRuntimeContract::build(BarrierDemand::default()).expect("契约");
    assert_eq!(contract.message.family(), MessageFamilyTag::CardMark);
    contract
        .message
        .verify_family(MessageFamilyTag::CardMark)
        .expect("card 族字段完整");
    // return 族的必需字段（unit）不适用于 card 族；跨族校验必须失败。
    let mut schema = contract.message.clone();
    schema
        .fields
        .retain(|field| field.kind != crate::runtime::model::FieldKind::CardCount);
    assert!(schema.verify_family(MessageFamilyTag::CardMark).is_err());
}

#[test]
fn gc_metadata_contract_still_builds_alongside_barrier() {
    let gc = GcMetadataRuntimeContract::build(GcMetadataDemand::empty()).expect("GC 契约");
    let barrier = BarrierRuntimeContract::build(BarrierDemand::default()).expect("barrier 契约");
    let _ = (gc, barrier, PlatformProfile::from(TargetName::X86_64Linux));
    let _ = _UnusedGcMetadataDemand::empty();
}

#[test]
fn compile_slice_reports_barrier_contract_end_to_end() {
    let source =
        "fn main() { let value = 1\n let closure = fn() int { return value }\n _ = closure() }";
    let compilation = Compiler::new().compile(CompileRequest::single_file(
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
    assert_eq!(
        plan.barrier_card_granularity_bytes(),
        CARD_GRANULARITY_BYTES
    );
    assert_eq!(
        plan.barrier_card_mark_buffer_entries(),
        CARD_MARK_BUFFER_ENTRIES
    );
    assert_eq!(
        plan.barrier_card_mark_stamp_entries(),
        CARD_MARK_STAMP_ENTRIES
    );
    assert_eq!(plan.barrier_flush_reason_count(), 6);
    assert_eq!(plan.barrier_card_mark_batch_fields(), 13);
    assert_ne!(plan.barrier_contract_fingerprint(), [0_u8; 32]);
    let dump = compilation.dump_runtime().expect("runtime dump");
    assert!(dump.contains("barrier schema=3 card=512 buffer=256 stamps=256"));
    assert!(dump.contains("barrier-flush-reasons buffer-full,processor-handoff,foreign-bridge,memory-pressure,minor-stop,producer-stop-gate"));
    assert!(dump.contains(
        "runtime-message return-fields=12 card-mark-fields=13 mark-ticket-fields=15 edge-delta-fields=18 handle-forward-fields=16 card-mark-family=card-mark"
    ));
    assert!(dump.contains(
        "edge schema=1 profile=mosaic-edge revision=1 quantum=4096 edge-buffer=512 deltas-per-write=2 trace-executor=2 candidate-schema=1 edge-delta-fields=18"
    ));
    assert!(dump.contains(
        "edge-phases discover,trace,trial,scc,validate,commit,sweep,release,complete,invalidate"
    ));
    assert!(dump.contains("edge-states active,candidate,reclaiming,free"));
    assert!(dump.contains("edge-fingerprint"));
}

#[test]
fn publish_region_slice_exposes_permit_quotas_in_the_contract() {
    let source = "use std.runtime.{ownership_publish, root_publish}\nfn main() {\n let first: chan[int] = chan[int](0)\n let publish = fn() {\n  let a: chan[int] = chan[int](0)\n  ownership_publish(first, a)\n }\n publish()\n _ = first\n}";
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let demand = compilation
        .raw_contract
        .as_ref()
        .expect("runtime 契约")
        .barrier()
        .demand();
    // publish 闭包本身有 3 条 region 外的 managed store，region 内只有 1 条已预留的写入。
    assert_eq!(demand.reserved_barriers, 1);
    assert_eq!(demand.bare_barriers, 3);
    assert_eq!(demand.barrier_sites(), 4);
    assert_eq!(demand.regions, 1);
    assert_eq!(demand.permits, 1);
    // 每个 publish region 只包住一条句柄 Assign：2 个 shade slot、1 个 card-mark slot。
    assert_eq!(demand.max_shades_permit, 2);
    assert_eq!(demand.max_card_marks_permit, 1);
    assert_eq!(demand.shade_slots, 2);
    assert_eq!(demand.card_mark_slots, 1);
    // 只要存在 managed store，card 账本与 edge summary 就共享同一站点集合。
    assert_eq!(demand.card_mark_sites, demand.barrier_sites());
    assert_eq!(demand.edge_summary_sites, demand.barrier_sites());
}

#[test]
fn card_index_helpers_agree_with_the_contract_granularity() {
    assert_eq!(card_index(0x1234_5678), 0x1234_5678 >> 9);
    let base = 0x1_0000;
    assert_eq!(arena_card(base, base), Some(0));
    assert_eq!(arena_card(base, base + 511), Some(0));
    assert_eq!(arena_card(base, base + 512), Some(1));
    assert_eq!(arena_card(base, base - 1), None);
    assert_eq!(arena_card(base, base + GC_ARENA_BYTES), None);
    // 契约登记的 hybrid 步骤名与参照实现枚举一一对应。
    for (index, step) in [
        HybridStep::ReadOld,
        HybridStep::ShadeOldDeleted,
        HybridStep::ShadeNewInserted,
        HybridStep::Store,
        HybridStep::CardMark,
        HybridStep::EdgeSummary,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(step.name(), HYBRID_BARRIER_STEPS[index]);
    }
    let _ = CardKey::new(1, 2, 0, 3);
    let _ = RuntimeSizeClassId::from_raw(0);
}
#[test]
fn owner_retire_flushes_the_processor_ledger() {
    let mut world = world(1);
    world.register_managed_arena(0, 2, 1).expect("登记 arena");
    world
        .perform_barrier(0, site(2, 1, 512, 1))
        .expect("写屏障成功");
    let target = world.raw_domain_owner();
    world
        .retire(0, target, &ServiceBudget::new(8, 1 << 16))
        .expect("retire");
    assert_eq!(world.barrier().table(2).expect("card table").dirty(), 1);
    assert!(buffer_empty(&world, 0));
}

#[test]
fn cache_close_flushes_for_gc_handoff_and_pressure() {
    use crate::runtime::message::RingCloseReason;

    let mut world = world(1);
    world.register_managed_arena(0, 3, 1).expect("登记 arena");
    world
        .perform_barrier(0, site(3, 1, 512, 1))
        .expect("写屏障成功");
    world
        .close_cache(0, RingCloseReason::GcHandoff)
        .expect("GC handoff");
    assert_eq!(world.barrier().table(3).expect("card table").dirty(), 1);
    assert!(buffer_empty(&world, 0));
    world
        .perform_barrier(0, site(3, 1, 1024, 1))
        .expect("写屏障成功");
    world
        .close_cache(0, RingCloseReason::PressureDrain)
        .expect("pressure drain");
    assert_eq!(world.barrier().table(3).expect("card table").dirty(), 2);
    assert!(buffer_empty(&world, 0));
}

#[test]
fn foreign_bridge_flushes_before_native_entry() {
    let mut world = world(1);
    // `enter_foreign` 是 rt0 生命周期的一部分：先启动进程模型再登记外部工作。
    world
        .boot(
            vec!["gugu".to_owned()],
            Vec::new(),
            "/work".to_owned(),
            4,
            crate::runtime::world::coroutine_impl::CoroutineEntry {
                pc: 0x1000,
                required_frame: 64,
            },
        )
        .expect("boot");
    world.register_managed_arena(0, 2, 1).expect("登记 arena");
    world
        .perform_barrier(0, site(2, 1, 512, 1))
        .expect("写屏障成功");
    world.enter_foreign(0).expect("进入 foreign bridge");
    assert_eq!(world.barrier().table(2).expect("card table").dirty(), 1);
    assert!(buffer_empty(&world, 0));
}

#[test]
fn drain_flushes_the_producer_stop_gate() {
    let mut world = world(1);
    world.register_managed_arena(0, 2, 1).expect("登记 arena");
    world
        .perform_barrier(0, site(2, 1, 512, 1))
        .expect("写屏障成功");
    let (forwarded, consumed) = world
        .drain_all(0, &ServiceBudget::new(8, 1 << 16))
        .expect("drain");
    assert_eq!((forwarded, consumed), (0, 0));
    assert_eq!(world.barrier().table(2).expect("card table").dirty(), 1);
    assert!(buffer_empty(&world, 0));
}

fn buffer_empty(world: &RawWorld, processor: usize) -> bool {
    world
        .barrier()
        .processor(processor)
        .expect("账本")
        .buffer()
        .is_empty()
}

#[test]
fn epoch_advance_publishes_old_keys_and_rejects_stale_sites() {
    let mut world = world(1);
    world.register_managed_arena(0, 2, 1).expect("登记 arena");
    world
        .perform_barrier(0, site(2, 1, 512, 1))
        .expect("写屏障成功");
    // epoch 前进必须把旧 cycle 的键交给 arena owner，而不是清空 buffer。
    world.advance_barrier_epoch(0, 2).expect("推进到 cycle 2");
    assert_eq!(world.barrier().table(2).expect("card table").dirty(), 1);
    assert!(buffer_empty(&world, 0));
    assert_eq!(world.barrier().cycle_epoch(), 2);
    // 站点 epoch 落后于平面时拒绝记账，避免就地改写 epoch 丢掉旧键。
    assert!(world.perform_barrier(0, site(2, 1, 1024, 1)).is_err());
    // epoch 只能前进：回退会把两批不同 cycle 的卡键混进同一账本。
    assert!(world.advance_barrier_epoch(0, 1).is_err());
    // 旧键已经发布到 card table，checkout 之后同一 card 的重复写入仍然幂等。
    world
        .perform_barrier(0, site(2, 1, 512, 2))
        .expect("写屏障成功");
    world
        .flush_barrier(0, 0, BarrierFlushReason::MemoryPressure)
        .expect("本地合并写");
    assert_eq!(world.barrier().table(2).expect("card table").dirty(), 1);
}

#[test]
fn edge_summary_aggregates_per_block_pair_for_the_owner() {
    let mut world = world(1);
    world.register_managed_arena(0, 2, 1).expect("登记 arena");

    // 同一 block 对在同一 epoch 内的 add 与 drop 净零抵消，不发布空 delta。
    world
        .perform_barrier(0, edge_site(2, 1, 512, 1, None, Some(block(9))))
        .expect("写屏障成功");
    world
        .perform_barrier(0, edge_site(2, 1, 512, 1, Some(block(9)), None))
        .expect("写屏障成功");
    // scratch 只在 slow edge 合并进聚合表；合并前后统计都必须包含真实记录。
    assert_eq!(
        world.barrier_stats().edge_pending,
        2,
        "尚未合并的变更仍必须计入 pending"
    );
    world
        .flush_barrier(0, 0, BarrierFlushReason::ProducerStopGate)
        .expect("合并边 scratch");
    assert_eq!(
        world.barrier_stats().edge_pending,
        0,
        "同一 epoch 的 add/drop 净零抵消"
    );
    assert_eq!(world.barrier().edges().pending(), 0);

    // 先建立一条 A→B 边，再覆盖到第三个 block：撤销旧边与新增新边各自留在聚合表里。
    world
        .perform_barrier(0, edge_site(2, 1, 1536, 1, None, Some(block(7))))
        .expect("写屏障成功");
    world
        .flush_barrier(0, 0, BarrierFlushReason::ProducerStopGate)
        .expect("合并边 scratch");
    assert_eq!(world.block_incoming_leases(block(7)), 1);
    world
        .perform_barrier(0, edge_site(2, 1, 2048, 1, Some(block(7)), Some(block(9))))
        .expect("写屏障成功");
    world
        .flush_barrier(0, 0, BarrierFlushReason::ProducerStopGate)
        .expect("合并边 scratch");
    let summary = world.barrier().edges();
    // 只有从未发布过的那个 block 对留下待发布差量；被撤销的那对净差量已在同一 epoch 内
    // 抵消（它还没有发布过任何 delta）。
    assert_eq!(summary.pending(), 1, "覆盖写入只对新增目标留下净差量");
    assert_eq!(summary.active_pairs(), 1, "只有仍活跃的那个 block 对计入");
    assert_eq!(world.block_incoming_leases(block(7)), 0, "旧目标边已撤销");
    assert_eq!(world.block_incoming_leases(block(9)), 1);

    // 同一 block 对的两条字段边：删一条之后仍需再删一条才归零。
    world
        .perform_barrier(0, edge_site(2, 1, 2560, 1, None, Some(block(9))))
        .expect("写屏障成功");
    world
        .flush_barrier(0, 0, BarrierFlushReason::ProducerStopGate)
        .expect("合并边 scratch");
    assert_eq!(
        world.block_incoming_leases(block(9)),
        1,
        "同一对 block 的两条字段边只算一份 incoming lease"
    );
    world
        .perform_barrier(0, edge_site(2, 1, 3072, 1, Some(block(9)), None))
        .expect("写屏障成功");
    world
        .flush_barrier(0, 0, BarrierFlushReason::ProducerStopGate)
        .expect("合并边 scratch");
    assert_eq!(world.block_incoming_leases(block(9)), 1, "删掉一条后仍有边");
    world
        .perform_barrier(0, edge_site(2, 1, 3584, 1, Some(block(9)), None))
        .expect("写屏障成功");
    world
        .flush_barrier(0, 0, BarrierFlushReason::ProducerStopGate)
        .expect("合并边 scratch");
    assert_eq!(world.block_incoming_leases(block(9)), 0, "最后一条边已删除");
}
