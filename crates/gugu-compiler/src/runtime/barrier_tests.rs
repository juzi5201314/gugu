//! hybrid write barrier、remembered set 与 edge summary 的确定性回归；不睡眠、不读熵。

use super::RawWorld;
use crate::runtime::barrier::{
    BarrierFlushReason, BarrierPlane, BarrierSite, CardKey, CardMarkDraft, EdgeSummary, HybridStep,
};
use crate::runtime::barrier_schema::{
    BARRIER_SCHEMA, BarrierDemand, BarrierRuntimeContract, CARD_GRANULARITY_BYTES,
    CARD_MARK_BUFFER_ENTRIES, CARD_MARK_STAMP_ENTRIES, GC_ARENA_CARDS, HYBRID_BARRIER_STEPS,
    MessageFamilyTag, arena_card, card_index, stamp_slot,
};
use crate::runtime::gc_metadata_contract::{GC_ARENA_BYTES, GcMetadataRuntimeContract};
use crate::runtime::gc_metadata_schema::GcMetadataDemand;
use crate::runtime::inbox::ServiceBudget;
use crate::runtime::message::BatchLimits;
use crate::runtime::size_class::RuntimeSizeClassId;
use crate::runtime::slab::SlabDescriptorId;
use crate::runtime::{GcMetadataDemand as _UnusedGcMetadataDemand, PlatformProfile};
use crate::{CompileRequest, Compiler, TargetName};

fn manager() -> crate::runtime::slab::OwnerToken {
    crate::runtime::slab::OwnerToken {
        domain: crate::runtime::slab::MemoryDomainId::RUNTIME_RAW,
        owner_id: crate::runtime::slab::OwnerId::from_raw(1),
        generation: crate::runtime::slab::OwnerGeneration::from_raw(1),
        route_key: crate::runtime::slab::RouteKey::from_raw(1),
    }
}

fn world(owners: u32) -> RawWorld {
    RawWorld::new(7, owners, 64, BatchLimits::default()).expect("raw world")
}

fn site(arena: u64, generation: u32, offset: u64, epoch: u64) -> BarrierSite {
    BarrierSite {
        arena_descriptor: arena,
        arena_generation: generation,
        offset,
        cycle_epoch: epoch,
        old_present: true,
        new_present: true,
        new_in_nursery: true,
        owner_old: true,
        marking: true,
        stack_grey: true,
        new_block: Some(9),
        source_block: 3,
        new_owner: 0,
        source_owner: 1,
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
            .contains("barrier schema=1 card=512 buffer=256 stamps=256")
    );
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
    let outcome = plane.perform_barrier(0, site(1, 7, 0x40, 4));
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
    let outcome = plane.perform_barrier(0, quiet);
    assert!(!outcome.shaded_old && !outcome.shaded_new);
    assert!(outcome.steps.contains(&HybridStep::Store));
    // 新值不在 nursery 时不产生 card 键。
    let mut old_new = site(1, 7, 0x40, 4);
    old_new.new_in_nursery = false;
    let outcome = plane.perform_barrier(0, old_new);
    assert!(!outcome.card_marked);
    assert!(!outcome.steps.contains(&HybridStep::CardMark));
}

#[test]
fn dedup_reuses_a_slot_and_stamp_conflicts_never_drop_keys() {
    let mut plane = BarrierPlane::new(1);
    let first = site(1, 3, 512, 1);
    plane.perform_barrier(0, first);
    // 同一 (arena, generation, card) 重复写入只占一个 slot。
    plane.perform_barrier(0, first);
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
    plane.perform_barrier(0, site(1, 3, u64::from(collided) * 512, 1));
    let record = plane.processor(0).expect("账本");
    assert_eq!(record.buffer().len(), 2, "stamp 冲突不得丢弃尚未发布的键");
    plane
        .register_arena(1, manager(), 3, GC_ARENA_BYTES)
        .expect("登记 arena");
    let drafts = plane.flush_processor(0, BarrierFlushReason::BufferFull);
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
        plane.perform_barrier(0, site(5, 9, card * 512, 2));
    }
    let drafts = plane.flush_processor(0, BarrierFlushReason::ProducerStopGate);
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
        let outcome = plane.perform_barrier(0, site(2, 4, u64::from(card) * 512, 3));
        assert!(outcome.flush.is_none(), "额度内不得触发 flush");
    }
    let outcome =
        plane.perform_barrier(0, site(2, 4, u64::from(CARD_MARK_BUFFER_ENTRIES) * 512, 3));
    assert_eq!(outcome.flush, Some(BarrierFlushReason::BufferFull));
    assert!(!outcome.card_marked, "超额的写入不能在 fast path 上记账");
    assert!(outcome.steps.contains(&HybridStep::Store), "store 已发生");
    let drafts = plane.flush_processor(0, BarrierFlushReason::BufferFull);
    let total: u32 = drafts.iter().map(|draft| draft.card_count).sum();
    assert_eq!(total, CARD_MARK_BUFFER_ENTRIES);
    // flush 后同一键可以重新进入 buffer。
    let outcome = plane.perform_barrier(0, site(2, 4, 0, 3));
    assert!(outcome.card_marked);
}

#[test]
fn epoch_change_invalidates_old_keys() {
    let mut plane = BarrierPlane::new(1);
    plane.perform_barrier(0, site(1, 1, 512, 1));
    assert_eq!(plane.processor(0).expect("账本").buffer().len(), 1);
    plane.advance_epoch(2);
    assert!(plane.processor(0).expect("账本").buffer().is_empty());
    // 旧 epoch 的键不能再用旧 buffer 的键掩盖新 cycle 的脏度。
    plane.perform_barrier(0, site(1, 1, 512, 2));
    let record = plane.processor(0).expect("账本");
    assert_eq!(record.buffer().len(), 1);
    assert_eq!(record.buffer().cycle_epoch(), 2);
}

#[test]
fn six_flush_reasons_are_reachable() {
    for (index, reason) in BarrierFlushReason::ALL.into_iter().enumerate() {
        let mut plane = BarrierPlane::new(1);
        plane.perform_barrier(0, site(1, 1, 512, 1));
        let drafts = plane.flush_processor(0, reason);
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
    world
        .register_managed_arena(1, SlabDescriptorId::from_raw(3), 11)
        .expect("登记 arena");
    world.perform_barrier(0, site(3, 11, 1024, 5));
    let published = world
        .flush_barrier(0, 0, BarrierFlushReason::ProcessorHandoff)
        .expect("发布 batch");
    assert_eq!(published, 1, "非 owner processor 必须发布 CardMarkBatch");
    let stats = world.barrier_stats(0);
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
    world.perform_barrier(0, site(3, 11, 1024, 5));
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
    world
        .register_managed_arena(0, SlabDescriptorId::from_raw(2), 3)
        .expect("登记 arena");
    world.perform_barrier(0, site(2, 3, 2048, 1));
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
    world
        .register_managed_arena(1, SlabDescriptorId::from_raw(4), 6)
        .expect("登记 arena");

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
    world
        .register_managed_arena(0, SlabDescriptorId::from_raw(2), 1)
        .expect("登记 arena");
    world.perform_barrier(0, site(2, 1, 512, 1));
    world.perform_barrier(1, site(2, 1, 512, 1));
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
fn edge_summary_orders_adds_before_drops_and_cancels_within_epoch() {
    let mut edges = EdgeSummary::default();
    let add = edges.record_add(1, 2, 7, 10);
    assert!(add.add);
    assert_eq!(edges.drain().len(), 1, "add 先被发布");
    // 更早 epoch 的 drop 必须被提升到已发布 add 的 epoch。
    let drop = edges.record_drop(1, 2, 7, 9);
    assert!(!drop.add);
    assert_eq!(drop.epoch, add.epoch, "drop 不得早于已发布的 add");
    let drained = edges.drain();
    assert_eq!(drained.len(), 1);
    assert!(!drained[0].add);

    // 同一 epoch 内 add 与 drop 净零抵消。
    let mut edges = EdgeSummary::default();
    edges.record_add(3, 4, 1, 5);
    edges.record_drop(3, 4, 1, 5);
    assert_eq!(edges.pending(), 0);
    assert!(edges.drain().is_empty());
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
    assert!(dump.contains("barrier schema=1 card=512 buffer=256 stamps=256"));
    assert!(dump.contains("barrier-flush-reasons buffer-full,processor-handoff,foreign-bridge,memory-pressure,minor-stop,producer-stop-gate"));
    assert!(dump.contains(
        "runtime-message return-fields=12 card-mark-fields=13 card-mark-family=card-mark"
    ));
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
    world
        .register_managed_arena(0, SlabDescriptorId::from_raw(2), 1)
        .expect("登记 arena");
    world.perform_barrier(0, site(2, 1, 512, 1));
    let target = world.raw_domain_owner();
    world
        .retire(0, target, &ServiceBudget::new(8, 1 << 16))
        .expect("retire");
    assert_eq!(world.barrier().table(2).expect("card table").dirty(), 1);
    assert!(buffer_empty(&world, 0));
}

#[test]
fn cache_close_flushes_for_gc_handoff_and_pressure() {
    use crate::runtime::message::{ProducerStaging, ReturnSlabCache, RingCloseReason};

    let mut world = world(1);
    world
        .register_managed_arena(0, SlabDescriptorId::from_raw(3), 1)
        .expect("登记 arena");
    let mut staging = ProducerStaging::new(BatchLimits::default());
    let mut cache = ReturnSlabCache::new();
    world.perform_barrier(0, site(3, 1, 512, 1));
    world
        .close_cache(0, &mut staging, &mut cache, RingCloseReason::GcHandoff)
        .expect("GC handoff");
    assert_eq!(world.barrier().table(3).expect("card table").dirty(), 1);
    assert!(buffer_empty(&world, 0));
    world.perform_barrier(0, site(3, 1, 1024, 1));
    world
        .close_cache(0, &mut staging, &mut cache, RingCloseReason::PressureDrain)
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
    world
        .register_managed_arena(0, SlabDescriptorId::from_raw(2), 1)
        .expect("登记 arena");
    world.perform_barrier(0, site(2, 1, 512, 1));
    world.enter_foreign(0).expect("进入 foreign bridge");
    assert_eq!(world.barrier().table(2).expect("card table").dirty(), 1);
    assert!(buffer_empty(&world, 0));
}

#[test]
fn drain_flushes_the_producer_stop_gate() {
    let mut world = world(1);
    world
        .register_managed_arena(0, SlabDescriptorId::from_raw(2), 1)
        .expect("登记 arena");
    world.perform_barrier(0, site(2, 1, 512, 1));
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
