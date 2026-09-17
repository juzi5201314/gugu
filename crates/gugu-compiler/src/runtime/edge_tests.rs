//! `EdgePlane` 的确定性单元测试：按序应用、乱序保留、缺口补齐与 dirty 去重。

use super::edge::{EdgeApply, EdgePlane, HeldRecord};
use super::local_heap::{BlockRef, ManagedBlockId};
use super::mark_schema::GcCreditId;
use super::message::ReturnNodeId;

fn block(id: u32) -> BlockRef {
    BlockRef {
        id: ManagedBlockId(id),
        generation: 1,
    }
}

fn record(sequence: u64, delta: i64, slot: u32) -> HeldRecord {
    HeldRecord {
        sequence,
        delta,
        credit: GcCreditId::new(slot, 1),
        node: ReturnNodeId::from_raw(slot),
    }
}

#[test]
fn in_order_records_apply_and_advance_the_expected_sequence() {
    let mut plane = EdgePlane::new();
    let source = block(1);
    let destination = block(2);
    assert_eq!(
        plane.apply(source, destination, record(1, 1, 0)),
        Ok(EdgeApply::Applied)
    );
    assert_eq!(
        plane.apply(source, destination, record(2, -1, 1)),
        Ok(EdgeApply::Applied)
    );
    assert_eq!(plane.applied_delta(source, destination), 0);
    assert_eq!(plane.incoming_applied(destination), 0);
    assert_eq!(plane.stats().applied, 2);
    assert_eq!(plane.pending_credits(), 0);
    // 已应用的序号不能重放：旧或重复记录都是真正的不变量失败。
    assert!(plane.apply(source, destination, record(2, 5, 2)).is_err());
    assert!(
        plane.apply(source, destination, record(1, 5, 3)).is_err(),
        "旧序号必须被拒绝"
    );
    assert_eq!(
        plane.applied_delta(source, destination),
        0,
        "失败的重放不改动计数"
    );
}

#[test]
fn future_sequences_are_held_until_the_gap_is_filled() {
    let mut plane = EdgePlane::new();
    let source = block(1);
    let destination = block(2);
    // 序号 2 先到：保留记录与 credit，不推进已应用计数。
    assert_eq!(
        plane.apply(source, destination, record(2, 3, 1)),
        Ok(EdgeApply::Held)
    );
    assert_eq!(plane.held_records(), 1);
    assert_eq!(plane.pending_credits(), 1, "保留记录继续占用 credit");
    assert_eq!(plane.applied_delta(source, destination), 0);
    assert!(plane.take_released().is_empty());
    // 同一序号的重复投递不改变任何状态。
    assert_eq!(
        plane.apply(source, destination, record(2, 3, 1)),
        Ok(EdgeApply::Held)
    );
    assert_eq!(plane.held_records(), 1);
    assert_eq!(plane.stats().held, 1, "重复的乱序投递不计入保留数");
    // 缺口补齐：序号 1 到达后连带应用序号 2，并把保留的 node 交出来。
    assert_eq!(
        plane.apply(source, destination, record(1, 1, 0)),
        Ok(EdgeApply::Applied)
    );
    assert_eq!(plane.applied_delta(source, destination), 4);
    assert_eq!(plane.held_records(), 0);
    assert_eq!(plane.pending_credits(), 0, "补齐后不再占用 credit");
    let released = plane.take_released();
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].sequence, 2);
    assert_eq!(released[0].node, record(2, 3, 1).node);
    assert_eq!(plane.stats().released, 1);
    assert_eq!(plane.stats().applied, 2);
}

#[test]
fn dirty_marks_deduplicate_and_clear_per_block() {
    let mut plane = EdgePlane::new();
    let first = ManagedBlockId(4);
    let second = ManagedBlockId(9);
    plane.note_dirty(first);
    plane.note_dirty(first);
    plane.note_dirty(second);
    assert_eq!(plane.dirty_count(), 2, "重复修改只置位一次");
    assert_eq!(
        plane.dirty_blocks().collect::<Vec<_>>(),
        vec![first, second],
        "dirty 集合按 block 身份有序遍历"
    );
    assert!(plane.clear_dirty(first));
    assert!(!plane.clear_dirty(first));
    assert_eq!(plane.dirty_count(), 1);
}

#[test]
fn zero_pairs_retire_but_active_or_held_pairs_stay() {
    let mut plane = EdgePlane::new();
    let source = block(1);
    let empty = block(2);
    let held = block(3);
    plane.apply(source, empty, record(1, 1, 0)).expect("可应用");
    plane
        .apply(source, empty, record(2, -1, 1))
        .expect("可应用");
    // 保留一条乱序记录的 block 对即使已应用计数为零也不能清退。
    plane.apply(source, held, record(2, 4, 2)).expect("可保留");
    assert_eq!(plane.retire_zero_pairs(), 1);
    assert_eq!(plane.applied_delta(source, empty), 0);
    assert_eq!(plane.held_records(), 1, "保留记录必须保留它的 block 对");
    assert_eq!(plane.stats().retired_pairs, 1);
}
