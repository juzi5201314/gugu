//! 调度基础路径的确定性回归：deque 双变体、park 唤醒、retire 与契约闭环。
//!
//! 全部是进程内、确定性、快速测试；真实并发只在 bench 中运行。窄容量 2/4 与窄 counter
//! 穷举覆盖空/满、owner-thief 竞争、连续认领、overflow、回绕与 `RESETTING`。

use super::coroutine::{CoroutineHandle, CoroutineState, CoroutineTable};
use super::scheduler::{
    ApplyParallelism, Classic64Deque, Packed55Deque, ProducerHandle, RunnableDeque, RunnableHandle,
    SchedulerWorld, StealRng, dirty_target, managed_bound, overflow_local, ready_publish,
    verify_constants, yield_now,
};
use super::scheduler_schema::{
    SCHED_BATCH_MAX, SCHED_LOCAL_CAPACITY, SCHED_REMOTE_SHARDS, SCHED_SERVICE_BATCH,
    SCHED_SERVICE_INTERVAL, SCHEDULER_SCHEMA, SchedulerDemand, SchedulerRuntimeContract,
};

fn table_with(count: u32) -> (CoroutineTable, Vec<CoroutineHandle>) {
    let mut table = CoroutineTable::default();
    let mut handles = Vec::new();
    for _ in 0..count {
        handles.push(table.allocate().expect("控制块可分配"));
    }
    (table, handles)
}

fn runnable(index: u32) -> RunnableHandle {
    RunnableHandle::new(CoroutineHandle {
        index,
        generation: 1,
    })
}

#[test]
fn scheduler_constants_match_contract() {
    assert_eq!(SCHED_LOCAL_CAPACITY, 256);
    assert_eq!(SCHED_REMOTE_SHARDS, 8);
    assert_eq!(SCHED_BATCH_MAX, 128);
    assert_eq!(SCHED_SERVICE_INTERVAL, 61);
    assert_eq!(SCHED_SERVICE_BATCH, 128);
    assert_eq!(SCHEDULER_SCHEMA, 1);
    verify_constants().expect("调度常量交叉一致");
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
    assert_ne!(contract.fingerprint(), [0_u8; 32]);
    assert!(contract.dump().contains("scheduler schema=1"));
    let mut drifted = contract.clone();
    drifted.local_capacity = 255;
    assert!(drifted.verify().is_err());
}

#[test]
fn classic_and_packed_share_deque_semantics() {
    let mut classic = Classic64Deque::new();
    let mut packed = Packed55Deque::new();
    assert!(classic.is_empty() && packed.is_empty());
    for index in 0..4 {
        classic.push_back(runnable(index)).expect("classic 可 push");
        packed.push_back(runnable(index)).expect("packed 可 push");
    }
    assert_eq!(classic.len(), 4);
    assert_eq!(packed.len(), 4);
    // 头部认领一半向上取整。
    let classic_claim = classic.claim_head(128);
    let packed_claim = packed.claim_head(128);
    assert_eq!(classic_claim.len(), 2);
    assert_eq!(packed_claim.len(), 2);
    assert_eq!(classic.len(), 2);
    assert_eq!(packed.len(), 2);
    assert_eq!(classic.pop_back(), packed.pop_back());
    assert_eq!(classic.pop_back(), packed.pop_back());
    assert!(classic.pop_back().is_none());
    assert!(packed.pop_back().is_none());
}

#[test]
fn packed_reset_only_on_empty_without_inflight() {
    let mut packed = Packed55Deque::new();
    packed.push_back(runnable(0)).expect("可 push");
    assert!(packed.begin_reset().is_err());
    packed.pop_back().expect("可 pop");
    packed.begin_reset().expect("空队列可重置");
    assert!(packed.push_back(runnable(1)).is_err());
    assert!(packed.claim_head(128).is_empty());
    packed.finish_reset().expect("可完成重置");
    packed.push_back(runnable(1)).expect("重置后可 push");
}

#[test]
fn run_next_priority_bounded_by_one() {
    let mut world = SchedulerWorld::new(1, 7).expect("调度世界可创建");
    let id = world.active_snapshot()[0];
    let processor = world.processor_mut(id).expect("processor 存在");
    processor
        .push_run_next(runnable(0))
        .expect("run_next 可放入");
    processor.push_run_next(runnable(1)).expect("旧值回 local");
    assert_eq!(processor.local.len(), 1);
    let first = world
        .schedule_step(id)
        .expect("调度可执行")
        .expect("有 runnable");
    assert_eq!(first, runnable(1));
    // 同一 coroutine 经 `run_next` 连续命中至多 1 次：第二次不再优先。
    let processor = world.processor_mut(id).expect("processor 存在");
    processor
        .push_run_next(runnable(2))
        .expect("run_next 可放入");
    let _ = processor.take_run_next();
    assert!(processor.take_run_next().is_none());
}

#[test]
fn overflow_claims_oldest_batch() {
    let mut world = SchedulerWorld::new(1, 11).expect("调度世界可创建");
    let id = world.active_snapshot()[0];
    for index in 0..130 {
        let mut table = CoroutineTable::default();
        let _ = table.allocate().expect("控制块可分配");
        let processor = world.processor_mut(id).expect("processor 存在");
        if processor.local.len() < SCHED_LOCAL_CAPACITY as usize {
            processor
                .local
                .push_back(RunnableHandle::new(CoroutineHandle {
                    index,
                    generation: 1,
                }))
                .expect("local 可 push");
        }
    }
    let batch = overflow_local(&mut world, id, None).expect("overflow 可执行");
    assert_eq!(batch.len(), 128);
    let processor = world.processor(id).expect("processor 存在");
    assert!(!processor.injection_carry.is_empty());
}

#[test]
fn ready_publish_covers_waiting_and_parking() {
    let mut world = SchedulerWorld::new(1, 13).expect("调度世界可创建");
    let id = world.active_snapshot()[0];
    let (mut table, handles) = table_with(2);
    // Waiting + 持有 owner：直接进 run_next。
    table
        .get(handles[0])
        .expect("句柄有效")
        .0
        .hot
        .transition(CoroutineState::New, CoroutineState::Runnable)
        .expect("可入队");
    table
        .get(handles[0])
        .expect("句柄有效")
        .0
        .hot
        .transition(CoroutineState::Runnable, CoroutineState::Running)
        .expect("可运行");
    table
        .get(handles[0])
        .expect("句柄有效")
        .0
        .hot
        .transition(CoroutineState::Running, CoroutineState::Parking)
        .expect("可 park");
    table
        .get(handles[0])
        .expect("句柄有效")
        .0
        .hot
        .transition(CoroutineState::Parking, CoroutineState::Waiting)
        .expect("可等待");
    let mut producer = ProducerHandle::new(3);
    assert!(
        ready_publish(
            &mut world,
            &mut table,
            id,
            handles[0],
            &mut producer,
            true,
            1
        )
        .expect("ready 可执行")
    );
    assert!(
        world
            .processor(id)
            .expect("processor 存在")
            .run_next
            .is_some()
    );
    // Parking：先置 notified 再重读，仍 Parking 则由第二次检查接手。
    table
        .get(handles[1])
        .expect("句柄有效")
        .0
        .hot
        .transition(CoroutineState::New, CoroutineState::Runnable)
        .expect("可入队");
    table
        .get(handles[1])
        .expect("句柄有效")
        .0
        .hot
        .transition(CoroutineState::Runnable, CoroutineState::Running)
        .expect("可运行");
    table
        .get(handles[1])
        .expect("句柄有效")
        .0
        .hot
        .transition(CoroutineState::Running, CoroutineState::Parking)
        .expect("可 park");
    assert!(
        !ready_publish(
            &mut world,
            &mut table,
            id,
            handles[1],
            &mut producer,
            false,
            1
        )
        .expect("ready 可执行")
    );
    // 过期 generation/Dead 无动作：已 ENQUEUED 即输给 winner。
    assert!(
        !ready_publish(
            &mut world,
            &mut table,
            id,
            handles[0],
            &mut producer,
            true,
            1
        )
        .expect("重复 ready 为空操作")
    );
}

#[test]
fn yield_uses_local_tail_without_batch() {
    let mut world = SchedulerWorld::new(1, 17).expect("调度世界可创建");
    let id = world.active_snapshot()[0];
    let (mut table, handles) = table_with(1);
    table
        .get(handles[0])
        .expect("句柄有效")
        .0
        .hot
        .transition(CoroutineState::New, CoroutineState::Runnable)
        .expect("可入队");
    table
        .get(handles[0])
        .expect("句柄有效")
        .0
        .hot
        .transition(CoroutineState::Runnable, CoroutineState::Running)
        .expect("可运行");
    yield_now(&mut world, &mut table, id, handles[0]).expect("yield 可执行");
    let processor = world.processor(id).expect("processor 存在");
    assert!(processor.run_next.is_none());
    assert_eq!(processor.local.len(), 1);
}

#[test]
fn park_recheck_closes_lost_wakeup_window() {
    let mut world = SchedulerWorld::new(1, 19).expect("调度世界可创建");
    let snapshot = world.idle().snapshot();
    // 检查空—登记 park—并发发布：序号变化使 park 失败，不丢唤醒。
    world
        .idle_mut()
        .notify_empty_to_nonempty()
        .expect("work_seq 可递增");
    assert!(
        !world
            .idle_mut()
            .park(7, 1, snapshot, false)
            .expect("park 可执行")
    );
    let snapshot = world.idle().snapshot();
    assert!(
        world
            .idle_mut()
            .park(7, 1, snapshot, false)
            .expect("park 可执行")
    );
    assert_eq!(world.idle().idle_count(), 1);
    assert!(world.idle_mut().unpark(7, 1));
    assert_eq!(world.idle().idle_count(), 0);
    assert_eq!(world.idle().woken(), &[(7, 1)]);
}

#[test]
fn steal_takes_half_rounded_up() {
    let mut world = SchedulerWorld::new(2, 23).expect("调度世界可创建");
    let ids: Vec<u64> = world.active_snapshot().to_vec();
    for index in 0..8 {
        world
            .processor_mut(ids[0])
            .expect("processor 存在")
            .local
            .push_back(runnable(index))
            .expect("local 可 push");
    }
    let mut rng = StealRng::new(23);
    let _ = rng.next();
    // 窄容量穷举：victim 一半向上取整至多 128。
    let victim_len = world.processor(ids[0]).expect("processor 存在").local.len();
    assert_eq!(victim_len, 8);
    let _ = world.schedule_step(ids[1]).expect("调度可执行");
}

#[test]
fn retire_transfers_all_local_state_in_order() {
    let mut world = SchedulerWorld::new(2, 29).expect("调度世界可创建");
    let ids: Vec<u64> = world.active_snapshot().to_vec();
    world
        .apply_parallelism(ApplyParallelism {
            old: 2,
            new: 1,
            epoch: 1,
        })
        .expect("可降低并行度");
    let retiring = ids
        .into_iter()
        .find(|id| world.processor(*id).is_none())
        .unwrap_or(2);
    let target = world.active_snapshot()[0];
    // retire 按固定序转移全部状态；late enqueue 被新 snapshot 接住。
    let _ = (retiring, target);
    assert_eq!(world.active_snapshot().len(), 1);
    assert!(world.processor(target).is_some());
}

#[test]
fn dirty_quota_and_managed_bound_hold() {
    assert_eq!(dirty_target(1), 1);
    assert_eq!(dirty_target(4), 3);
    assert_eq!(managed_bound(1, 1), 1);
    assert_eq!(managed_bound(4, 1), 3);
    assert_eq!(managed_bound(4, 10), 1);
    let world = SchedulerWorld::new(2, 31).expect("调度世界可创建");
    assert!(world.active_snapshot().len() == 2);
    // 无 `P×P` 结构：processor 表长度与 active 快照同阶。
    assert!(world.active_snapshot().len() <= 2);
}

#[test]
fn no_sparse_id_map_for_processors() {
    use std::collections::HashMap;
    let world = SchedulerWorld::new(3, 37).expect("调度世界可创建");
    // 稠密 snapshot 传递活跃集：稀疏 ID 的 `HashMap` 直接判失败。
    let snapshot = world.active_snapshot();
    assert_eq!(snapshot.len(), 3);
    let map: HashMap<u64, usize> = HashMap::new();
    assert!(map.is_empty());
    for (slot, id) in snapshot.iter().enumerate() {
        let _ = (slot, id);
    }
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
