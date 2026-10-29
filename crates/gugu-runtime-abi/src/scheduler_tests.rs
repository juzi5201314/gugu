//! 调度契约与协程生命周期边的确定性回归。
//!
//! 队列、park、steal 的执行在镜像内 Gugu runtime。这里只核对契约常量和 lifecycle 边。

use super::coroutine::{CoroutineHandle, CoroutineState, CoroutineTable};
use super::scheduler_schema::{
    SCHED_BATCH_MAX, SCHED_LOCAL_CAPACITY, SCHED_REMOTE_SHARDS, SCHED_SERVICE_BATCH,
    SCHED_SERVICE_INTERVAL, SCHEDULER_SCHEMA, SchedulerDemand, SchedulerRuntimeContract,
    dirty_target, managed_bound, verify_constants,
};

fn table_with(count: u32) -> (CoroutineTable, Vec<CoroutineHandle>) {
    let mut table = CoroutineTable::default();
    let mut handles = Vec::new();
    for _ in 0..count {
        handles.push(table.allocate().expect("控制块可分配"));
    }
    (table, handles)
}

#[test]
fn scheduler_constants_match_contract() {
    assert_eq!(SCHED_LOCAL_CAPACITY, 256);
    assert_eq!(SCHED_REMOTE_SHARDS, 8);
    assert_eq!(SCHED_BATCH_MAX, 128);
    assert_eq!(SCHED_SERVICE_INTERVAL, 61);
    assert_eq!(SCHED_SERVICE_BATCH, 128);
    assert_eq!(SCHEDULER_SCHEMA, 2);
    verify_constants().expect("调度常量交叉一致");
}

#[test]
fn tuning_profile_digest_is_field_sensitive() {
    use crate::tuning::RUNTIME_TUNING_PROFILE;

    let baseline = RUNTIME_TUNING_PROFILE.digest();
    let mut changed = RUNTIME_TUNING_PROFILE;
    changed.service_interval = 62;
    assert_ne!(changed.digest(), baseline);
    changed = RUNTIME_TUNING_PROFILE;
    changed.cache_line_bytes *= 2;
    assert_ne!(changed.digest(), baseline);
    changed = RUNTIME_TUNING_PROFILE;
    changed.revision += 1;
    assert_ne!(changed.digest(), baseline);
    assert_eq!(RUNTIME_TUNING_PROFILE.digest(), baseline);
}

#[test]
fn scheduler_contract_rejects_drift() {
    let contract =
        SchedulerRuntimeContract::build(SchedulerDemand::default()).expect("调度契约可构建");
    contract.verify().expect("调度契约自洽");
    assert_eq!(contract.local_capacity(), 256);
    assert_eq!(contract.remote_shards(), 8);
    assert_eq!(contract.batch_max(), 128);
    assert_eq!(contract.service_interval(), 61);
    assert_eq!(contract.service_batch(), 128);
    assert_eq!(contract.poll_flags_offset(), 0);
    assert_eq!(contract.ownership_offset(), 64);
    assert_eq!(contract.tlab_cursor_offset(), 3520);
    assert_eq!(contract.tlab_limit_offset(), 3528);
    assert_eq!(contract.turn_region_cursor_offset(), 3536);
    assert_eq!(contract.turn_region_limit_offset(), 3544);
    assert_ne!(contract.fingerprint(), [0_u8; 32]);
    assert!(contract.dump().contains("scheduler schema=2"));
    assert!(
        contract
            .dump()
            .contains("scheduler-layout poll-flags=0 tlab-cursor=3520 turn-region-cursor=3536")
    );
    let mut drifted = contract.clone();
    drifted.local_capacity = 255;
    assert!(drifted.verify().is_err());
    drifted = contract.clone();
    drifted.tlab_cursor_offset = 0;
    assert!(drifted.verify().is_err());
}

#[test]
fn dirty_quota_and_managed_bound_hold() {
    assert_eq!(dirty_target(1), 1);
    assert_eq!(dirty_target(4), 3);
    assert_eq!(managed_bound(1, 1), 1);
    assert_eq!(managed_bound(4, 1), 3);
    assert_eq!(managed_bound(4, 10), 1);
}

#[test]
fn foreign_lifecycle_edges_hold_generation() {
    let (table, handles) = table_with(1);
    let (slot, _) = table.get(handles[0]).expect("句柄有效");
    slot.hot
        .transition(CoroutineState::New, CoroutineState::Runnable)
        .expect("可入队");
    slot.hot
        .transition(CoroutineState::Runnable, CoroutineState::Running)
        .expect("可运行");
    slot.hot
        .transition(CoroutineState::Running, CoroutineState::Foreign)
        .expect("可进入 bridge");
    slot.hot.retake_detached().expect("可 retake");
    slot.hot
        .transition(CoroutineState::Foreign, CoroutineState::Running)
        .expect("可返回");
    slot.hot
        .transition(CoroutineState::Running, CoroutineState::DirtyWaiting)
        .expect("可等待 dirty");
    slot.hot
        .transition(CoroutineState::DirtyWaiting, CoroutineState::Foreign)
        .expect("可进入 dirty bridge");
    let word = slot.hot.state.load(std::sync::atomic::Ordering::Acquire);
    assert_eq!(word >> 8, 2);
    assert!(slot.hot.claim_for_batch().is_ok());
    assert!(slot.hot.take_running().is_ok());
    assert!(slot.hot.yield_to_runnable().is_ok());
}
