//! 固定三笔在途调用与三名等待者，不运行 OS 线程。
use super::*;

#[test]
fn pressure_reduction_keeps_credits_until_completion_then_restores_fifo() {
    let mut world = ForeignWorld::new(4).unwrap();
    let active = std::array::from_fn::<_, 3, _>(|_| {
        let handle = world.spawn_running().unwrap();
        assert!(matches!(
            world
                .admit(handle, BridgeMode::Ordinary, &[], 1, 16, 16)
                .unwrap(),
            AdmitOutcome::Attached { .. }
        ));
        handle
    });
    let credits = active.map(|handle| world.bridge(handle).unwrap().credit());
    world.set_memory_pressure(true).unwrap();
    let waiting = std::array::from_fn::<_, 3, _>(|_| {
        let handle = world.spawn_running().unwrap();
        assert_eq!(
            world
                .admit(handle, BridgeMode::Ordinary, &[], 1, 16, 16)
                .unwrap(),
            AdmitOutcome::Waiting
        );
        assert_eq!(world.bridge(handle).unwrap().credit(), 0);
        handle
    });
    assert_eq!(world.active_blocking(), 3);
    assert_eq!(
        active.map(|handle| world.bridge(handle).unwrap().credit()),
        credits
    );
    for handle in &active[..2] {
        world.complete(*handle, 0).unwrap();
        assert_eq!(world.waiting_len(), 3, "未降到目标以下不能补位");
    }
    world.complete(active[2], 0).unwrap();
    assert_eq!(world.active_blocking(), 1);
    assert_eq!(world.waiting_len(), 2);
    assert_eq!(
        world.lifecycle(waiting[0]).unwrap(),
        CoroutineState::Foreign
    );
    assert_eq!(
        world.lifecycle(waiting[1]).unwrap(),
        CoroutineState::Waiting
    );
    let first_credit = world.bridge(waiting[0]).unwrap().credit();
    world.set_memory_pressure(false).unwrap();
    assert_eq!(world.active_blocking(), 3);
    assert_eq!(world.waiting_len(), 0);
    let granted = waiting.map(|handle| world.bridge(handle).unwrap().credit());
    assert_eq!(granted[0], first_credit);
    assert!(granted[0] < granted[1] && granted[1] < granted[2]);
    for handle in waiting {
        world.complete(handle, 0).unwrap();
    }
    assert_eq!(world.active_blocking(), 0);
}
