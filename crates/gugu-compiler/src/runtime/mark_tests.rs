//! mark 平面的确定性单元测试：credit 状态机、mailbox 单 consumer、snapshot gate 与终止判定。

use super::super::MarkDemand;
use super::super::mark::{
    MarkCondition, MarkCycleState, MarkError, MarkObservations, MarkParticipant, MarkPlane,
    credit_local,
};
use super::super::mark_schema::MarkRuntimeContract;

fn contract() -> MarkRuntimeContract {
    MarkRuntimeContract::build(MarkDemand::default(), 8).expect("契约可构建")
}

fn small_pool() -> MarkRuntimeContract {
    MarkRuntimeContract::build(MarkDemand::default(), 2).expect("契约可构建")
}

#[test]
fn new_rejects_owners_beyond_the_owner_id_width() {
    let contract = contract();
    // owner 位宽为 8：编号 256 越界，255 仍在范围内。
    assert_eq!(
        MarkPlane::new(&contract, 1 << contract.credit_owner_bits()),
        Err(MarkError::UnknownOwner {
            owner: 1 << contract.credit_owner_bits()
        })
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
    assert_eq!(credit_local(credit), 0, "首个 credit 的局部编号从 0 开始");
    assert_eq!(plane.mark_credit_pending(), 1);
    assert_eq!(plane.mailbox_pending(), 1);
    assert_eq!(plane.credit(0).expect("账本可读").pending(), 1);
    assert_eq!(
        plane.credit_mut(0).expect("账本可读").return_credit(0),
        Err(MarkError::CreditNotDone {
            owner: 0,
            credit: 0
        }),
        "InFlight 的 credit 不能归还"
    );
    plane.consume_ticket(1, credit, 1, 0).expect("可消费");
    assert_eq!(plane.credit(0).expect("账本可读").done(), 1);
    assert_eq!(plane.mailbox_pending(), 0);
    assert_eq!(plane.settle_owner(0).expect("可归还"), 1);
    assert_eq!(plane.credit(0).expect("账本可读").returned(), 1);
    assert_eq!(plane.credit(0).expect("账本可读").pending(), 0);
    assert_eq!(plane.mark_credit_pending(), 0);
    // 重复消费同一 ticket：mailbox 已空。
    assert_eq!(
        plane.consume_ticket(1, credit, 1, 0),
        Err(MarkError::MailboxEmpty { owner: 1 })
    );
    // 同一 credit 再次投递而 mailbox 非空：Returned 状态必须被拒绝，不能静默丢弃。
    let second = plane.publish_ticket(0, 1).expect("ticket 可发布");
    plane.settle_owner(0).expect("可归还");
    assert_eq!(
        plane.consume_ticket(1, credit, 1, 0),
        Err(MarkError::CreditNotInFlight {
            owner: 0,
            credit: 0
        })
    );
    assert_eq!(credit_local(second), 1, "credit 编号在一次 cycle 内不复用");
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
    assert_eq!(
        plane.forward_ticket(0, 1, 0),
        Err(MarkError::MailboxEmpty { owner: 0 })
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
        plane.consume_ticket(1, credit, 1, 0),
        Ok(()),
        "身份校验通过后同一票必须能被消费"
    );
    // 转发把在飞集合记上；最终目标既不是源 owner 也不是中间 owner 时才算真正落地。
    let forwarded = plane.publish_ticket(0, 2).expect("ticket 可发布");
    plane.forward_ticket(2, 1, forwarded).expect("可转发");
    assert_eq!(plane.forwarded_pending(), 1);
    plane
        .consume_ticket(1, forwarded, 1, 0)
        .expect("最终目标可消费");
    assert_eq!(plane.forwarded_pending(), 0);
}
