//! mark 平面的确定性单元测试：credit 状态机、mailbox 单 consumer、snapshot gate 与终止判定。

use super::super::MarkDemand;
use super::super::barrier_schema::MessageFamilyTag;
use super::super::mark::{
    MarkCondition, MarkCycleState, MarkError, MarkObservations, MarkParticipant, MarkPlane,
};
use super::super::mark_schema::{GcCreditId, MarkRuntimeContract};

fn contract() -> MarkRuntimeContract {
    MarkRuntimeContract::build(MarkDemand::default(), 8).expect("契约可构建")
}

fn small_pool() -> MarkRuntimeContract {
    MarkRuntimeContract::build(MarkDemand::default(), 2).expect("契约可构建")
}

#[test]
fn plane_rejects_an_empty_owner_set() {
    let contract = contract();
    assert_eq!(
        MarkPlane::new(&contract, 0),
        Err(MarkError::UnknownOwner { owner: 0 })
    );
    assert!(MarkPlane::new(&contract, 4).is_ok());
}

#[test]
fn credit_lifecycle_acquire_consume_return_is_strict() {
    let contract = contract();
    let mut plane = MarkPlane::new(&contract, 2).expect("平面可创建");
    plane.begin_cycle(1, 0).expect("cycle 可开始");
    // publish 内部 acquire；未 consume 前 pending 非零，且不能直接 return。
    let credit = plane.publish_ticket(0, 1).expect("ticket 可发布");
    assert_eq!(credit.slot(), 0, "首个 credit 的 slot 从 0 开始");
    assert_eq!(credit.generation(), 1, "slot 的初始 generation 从 1 开始");
    assert_eq!(plane.mark_credit_pending(), 1);
    assert_eq!(plane.mailbox_pending(), 1);
    assert_eq!(plane.credit(0).expect("账本可读").pending(), 1);
    assert_eq!(
        plane.return_credit(credit),
        Err(MarkError::CreditNotDone { owner: 0, credit }),
        "InFlight 的 credit 不能归还"
    );
    plane.consume_ticket(1, credit, 1, 0).expect("可消费");
    assert_eq!(plane.credit(0).expect("账本可读").done(), 1);
    assert_eq!(plane.mailbox_pending(), 0);
    assert_eq!(plane.settle_owner(0).expect("可归还"), 1);
    assert_eq!(plane.credit(0).expect("账本可读").returned(), 1);
    assert_eq!(plane.credit(0).expect("账本可读").pending(), 0);
    assert_eq!(plane.mark_credit_pending(), 0);
    // 重复消费同一 ticket：credit 已不是 InFlight，校验先于 mailbox 状态改变。
    assert_eq!(
        plane.consume_ticket(1, credit, 1, 0),
        Err(MarkError::CreditNotInFlight { owner: 0, credit })
    );
    // 第二个 credit 复用第一个归还的 slot，但 generation 已推进。
    let second = plane.publish_ticket(0, 1).expect("ticket 可发布");
    assert_eq!(second.slot(), 0, "归还的 slot 必须被复用");
    assert_eq!(second.generation(), 2, "复用推进 generation");
    plane.consume_ticket(1, second, 1, 0).expect("可消费");
    plane.settle_owner(0).expect("可归还");
    // 重放第一个 credit 的旧 generation：必须被拒绝，且不能伤到池的状态。
    assert_eq!(
        plane.consume_ticket(1, credit, 1, 0),
        Err(MarkError::CreditNotIssued { owner: 0, credit }),
        "旧 generation 的 credit 不能消耗"
    );
    assert_eq!(plane.mark_credit_pending(), 0, "失败的重放不得改动在飞计数");
    let third = plane.publish_ticket(0, 1).expect("ticket 可发布");
    assert_eq!(third.generation(), 3, "重放失败后 slot 仍可继续复用");
    plane.consume_ticket(1, third, 1, 0).expect("可消费");
    assert_eq!(plane.settle_owner(0).expect("可归还"), 1);
}

#[test]
fn edge_delta_shares_the_pool_and_allows_intra_owner_edges() {
    let contract = contract();
    let mut plane = MarkPlane::new(&contract, 2).expect("平面可创建");
    plane.begin_cycle(1, 0).expect("cycle 可开始");
    // 同 owner 内的跨 block 边是合法输入：不像 ticket 那样拒绝自投递。
    let credit = plane.acquire_edge_delta(0, 0).expect("可 acquire");
    assert!(plane.acquire_ticket(0, 0).is_err(), "ticket 仍拒绝自投递");
    assert_eq!(plane.mark_credit_pending(), 1);
    // 另一族不能消耗这个 credit：族校验先于任何状态改变。
    assert_eq!(
        plane.consume_ticket(0, credit, 1, 0),
        Err(MarkError::CreditFamilyMismatch {
            credit,
            expected: "edge-delta"
        })
    );
    assert_eq!(plane.mark_credit_pending(), 1);
    plane
        .consume_edge_delta(0, credit, 1, 0)
        .expect("同族可消费");
    assert_eq!(plane.return_credit(credit), Ok(()));
    assert_eq!(plane.mark_credit_pending(), 0);
    // 归还后池仍然可用，且累计 issue 不受容量上界限制。
    for _ in 0..32 {
        let credit = plane.acquire_edge_delta(0, 1).expect("可继续 acquire");
        plane.consume_edge_delta(1, credit, 1, 0).expect("可消费");
        plane.return_credit(credit).expect("可归还");
    }
    assert_eq!(plane.mark_credit_pending(), 0);
    assert_eq!(plane.credits().slot_count(), 1, "池只按真实并发需求增长");
}

#[test]
fn credit_pool_exhaustion_is_reported_not_wrapped() {
    let contract = small_pool();
    let mut plane = MarkPlane::new(&contract, 2).expect("平面可创建");
    plane.begin_cycle(1, 0).expect("cycle 可开始");
    assert!(plane.publish_ticket(0, 1).is_ok());
    assert!(plane.publish_ticket(0, 1).is_ok());
    assert_eq!(
        plane.publish_ticket(0, 1),
        Err(MarkError::PoolExhausted {
            owner: 0,
            granted: 2
        })
    );
    assert_eq!(
        plane.mark_credit_pending(),
        2,
        "耗尽的 acquire 不得留下半个 slot"
    );
}

#[test]
fn snapshot_gate_requires_every_participant_and_rejects_duplicates() {
    let contract = contract();
    let mut plane = MarkPlane::new(&contract, 2).expect("平面可创建");
    plane.begin_cycle(3, 7).expect("cycle 可开始");
    assert_eq!(plane.state(), MarkCycleState::Snapshot);
    assert!(!plane.snapshot_ready());
    assert!(matches!(
        plane.release_snapshot(),
        Err(MarkError::SnapshotIncomplete { .. })
    ));
    plane
        .confirm_snapshot(0, MarkParticipant::RootSlice)
        .expect("首次确认");
    assert_eq!(
        plane.confirm_snapshot(0, MarkParticipant::RootSlice),
        Err(MarkError::DuplicateConfirm {
            owner: 0,
            kind: "root-slice"
        })
    );
    assert_eq!(
        plane.confirm_snapshot(9, MarkParticipant::RootSlice),
        Err(MarkError::UnknownOwner { owner: 9 })
    );
    for owner in 0..2 {
        for kind in MarkParticipant::ALL {
            if owner == 0 && kind == MarkParticipant::RootSlice {
                continue;
            }
            plane.confirm_snapshot(owner, kind).expect("确认可登记");
        }
    }
    assert!(plane.snapshot_ready());
    plane.release_snapshot().expect("收齐后进入 mark");
    assert_eq!(plane.state(), MarkCycleState::Marking);
    // gate 已关闭：后续确认必须失败。
    assert_eq!(
        plane.confirm_snapshot(0, MarkParticipant::LocalWorklist),
        Err(MarkError::SnapshotNotOpen)
    );
}

#[test]
fn seven_conditions_drive_termination_and_completion() {
    let contract = contract();
    let mut plane = MarkPlane::new(&contract, 2).expect("平面可创建");
    plane.begin_cycle(1, 0).expect("cycle 可开始");
    for owner in 0..2 {
        for kind in MarkParticipant::ALL {
            plane.confirm_snapshot(owner, kind).expect("确认可登记");
        }
    }
    plane.release_snapshot().expect("可进入 mark");
    let credit = plane.publish_ticket(0, 1).expect("ticket 可发布");
    let busy = plane.termination(MarkObservations {
        worklist_items: 2,
        published_batches: 1,
        barrier_buffer_keys: 3,
        forwarding_work: 4,
        producer_epoch_confirmed: 0,
        producer_epoch_total: 2,
    });
    assert_eq!(busy.get(MarkCondition::LocalWorklist), 2);
    assert_eq!(busy.get(MarkCondition::PublishedBatch), 1);
    assert_eq!(busy.get(MarkCondition::Mailbox), 1);
    assert_eq!(busy.get(MarkCondition::BarrierBuffer), 3);
    assert_eq!(busy.get(MarkCondition::ProducerEpoch), 2);
    assert_eq!(busy.get(MarkCondition::ForwardingWork), 4);
    assert_eq!(busy.get(MarkCondition::PendingCredit), 1);
    assert!(!busy.converged());
    assert_eq!(
        busy.blocking(),
        vec![
            "local-worklist",
            "published-batch",
            "mailbox",
            "barrier-buffer",
            "producer-epoch",
            "forwarding-work",
            "pending-credit"
        ]
    );
    // mailbox 为空不是完成条件：credit 未归还前仍不收敛。
    plane.consume_ticket(1, credit, 1, 0).expect("可消费");
    let credit_only = plane.termination(MarkObservations {
        producer_epoch_confirmed: 2,
        producer_epoch_total: 2,
        ..MarkObservations::default()
    });
    assert_eq!(credit_only.blocking(), vec!["pending-credit"]);
    plane.settle_owner(0).expect("可归还");
    let done = plane.termination(MarkObservations {
        producer_epoch_confirmed: 2,
        producer_epoch_total: 2,
        ..MarkObservations::default()
    });
    assert!(done.converged());
    assert!(done.blocking().is_empty());
    plane.remember(done).expect("可记录终止");
    plane.complete().expect("收敛后可宣布完成");
    assert_eq!(plane.state(), MarkCycleState::Idle);
    assert_eq!(plane.stats().cycles, 1);
    assert_eq!(plane.cycle(), 1);
}

#[test]
fn ticket_rejects_stale_identity_self_delivery_and_empty_mailbox() {
    let contract = contract();
    let mut plane = MarkPlane::new(&contract, 3).expect("平面可创建");
    let absent = GcCreditId::from_raw(0);
    assert_eq!(
        plane.forward_ticket(0, 1, absent),
        Err(MarkError::CreditNotIssued {
            owner: 0,
            credit: absent
        }),
        "转发同样先校验 credit，再改动 mailbox"
    );
    plane.begin_cycle(1, 0).expect("cycle 可开始");
    assert_eq!(
        plane.publish_ticket(0, 0),
        Err(MarkError::SelfTicket { owner: 0 })
    );
    let credit = plane.publish_ticket(0, 1).expect("ticket 可发布");
    assert_eq!(
        plane.consume_ticket(1, credit, 9, 0),
        Err(MarkError::StaleCycle {
            ticket: 9,
            current: 1
        })
    );
    assert_eq!(
        plane.consume_ticket(1, credit, 1, 7),
        Err(MarkError::StaleTopology {
            ticket: 7,
            current: 0
        })
    );
    assert_eq!(
        plane.consume_ticket(2, credit, 1, 0),
        Err(MarkError::CreditTargetMismatch { credit, target: 1 }),
        "目标 owner 不符必须先于任何状态改变",
    );
    assert_eq!(
        plane.consume_ticket(1, credit, 1, 0),
        Ok(()),
        "身份校验通过后同一票必须能被消费"
    );
    // 转发把在飞集合记上；最终目标既不是源 owner 也不是中间 owner 时才算真正落地。
    let forwarded = plane.publish_ticket(0, 2).expect("ticket 可发布");
    plane.forward_ticket(2, 1, forwarded).expect("可转发");
    assert_eq!(plane.forwarded_pending(), 1);
    // 中间 owner 不再是目标：转发后原目标不能消费同一 credit（目标校验先于 mailbox）。
    assert_eq!(
        plane.consume_ticket(2, forwarded, 1, 0),
        Err(MarkError::CreditTargetMismatch {
            credit: forwarded,
            target: 1
        }),
        "转发后 credit 的目标已经改到新目标"
    );
    plane
        .consume_ticket(1, forwarded, 1, 0)
        .expect("最终目标可消费");
    assert_eq!(plane.forwarded_pending(), 0);
    // 消耗记录携带的族身份也会被校验：edge delta 的 credit 不能冒充 ticket。
    let edge_credit = plane
        .acquire_edge_delta(0, 1)
        .expect("edge credit 可 acquire");
    let pending_before = plane.mark_credit_pending();
    assert_eq!(
        plane.consume_ticket(1, edge_credit, 1, 0),
        Err(MarkError::CreditFamilyMismatch {
            credit: edge_credit,
            expected: "edge-delta"
        }),
        "族不符必须被拒绝",
    );
    assert_eq!(
        plane.mark_credit_pending(),
        pending_before,
        "失败的错误族不得改动在飞计数"
    );
    assert_eq!(
        plane.mailbox_pending(),
        0,
        "失败的错误族不得消耗任何 mailbox 项"
    );
    plane
        .consume_edge_delta(1, edge_credit, 1, 0)
        .expect("同族可消费");
    assert_eq!(plane.return_credit(edge_credit), Ok(()));
    assert_eq!(
        MessageFamilyTag::EdgeDelta.raw(),
        4,
        "edge delta 的族判别值参与车道编码",
    );
}
