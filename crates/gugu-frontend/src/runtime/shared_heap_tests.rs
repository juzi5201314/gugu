//! SharedHeap stable handle、access guard 与 forwarding grace 的确定性行为回归。

use super::shared_heap::{ForwardDeferred, SharedForward, SharedHeap};
use super::shared_heap_schema::{
    SHARED_FORWARDING_GRACE_STEPS, SharedHeapDemand, SharedHeapRuntimeContract, SharedPayloadId,
    SharedSlotState,
};

fn contract() -> SharedHeapRuntimeContract {
    SharedHeapRuntimeContract::build(SharedHeapDemand::default())
        .expect("默认需求可构建 SharedHeap 契约")
}

/// 建立一个已发布的 shared 对象：返回堆、handle 与 payload identity。
fn published(
    bytes: u32,
) -> (
    SharedHeap,
    super::shared_heap_schema::SharedHandle,
    SharedPayloadId,
) {
    let contract = contract();
    let mut heap = SharedHeap::new(1, &contract);
    let source = heap
        .allocate_payload(7, 3, 0, bytes)
        .expect("fresh payload 可建立");
    let handle = heap.resolve_payload(source).expect("fresh payload 可发布");
    (heap, handle, source.id)
}

#[test]
fn grace_keeps_old_payload_until_guards_settle() {
    let (mut heap, handle, old) = published(16);
    heap.begin_access(1, handle).expect("guard 可建立");
    let access = heap.resolve_access(1).expect("guard 可解析");
    heap.store(access, 0, &[1, 2, 3, 4])
        .expect("guard 内可写入");
    assert_eq!(
        heap.load(access, 0, 4).expect("guard 内可读取"),
        [1, 2, 3, 4]
    );
    // 搬迁：新 payload 由调用者（世界）分配，旧 payload 进入 grace。
    let destination = heap
        .allocate_payload(7, 3, 8, 16)
        .expect("目标 payload 可建立");
    let SharedForward::Forwarded(record) = heap
        .forward(handle, destination.id, 9, 1)
        .expect("forward 可成功")
    else {
        panic!("没有 pin lease 时 forward 必须成功");
    };
    assert_eq!(record.old_payload, old);
    assert_ne!(record.new_payload, old);
    assert_eq!(record.forward_generation, 1);
    // guard 在 forward 之前解析：它仍读到旧 payload 的字节。
    assert_eq!(
        heap.load(access, 0, 4).expect("guard 仍可读旧 payload"),
        [1, 2, 3, 4]
    );
    heap.end_access(1).expect("guard 可结束");
    heap.end_forward_lease(handle, 9)
        .expect("forward lease 可结清");
    // 四步 grace 之前旧 payload 不会被回收。
    heap.advance_grace(handle, SHARED_FORWARDING_GRACE_STEPS - 1)
        .expect("grace 可推进");
    assert!(
        heap.payload_exists(old),
        "grace 未走满时旧 payload 必须保留"
    );
    heap.advance_grace(handle, 1).expect("grace 可走满");
    assert!(!heap.payload_exists(old), "grace 走满后旧 payload 必须释放");
    assert_eq!(
        heap.slot_state(handle),
        Some(SharedSlotState::Live),
        "旧 payload 回收后 slot 回到 live"
    );
    // guard 已经结束，它的 payload 也已释放：访问必须失败而不是读到别人的字节。
    assert!(heap.load(access, 0, 4).is_err());
}

#[test]
fn grace_waits_for_access_guard_and_mark_ticket() {
    let (mut heap, handle, old) = published(8);
    let destination = heap
        .allocate_payload(7, 4, 0, 8)
        .expect("目标 payload 可建立");
    let _ = heap
        .forward(handle, destination.id, 1, 1)
        .expect("forward 可成功");
    heap.end_forward_lease(handle, 1).expect("lease 可结清");
    heap.begin_access(1, handle).expect("guard 可建立");
    let access = heap.resolve_access(1).expect("guard 可解析");
    heap.begin_mark_cycle(1).expect("mark cycle 可开始");
    assert!(heap.mark_ticket(handle).expect("可发出票据"));
    heap.advance_grace(handle, SHARED_FORWARDING_GRACE_STEPS)
        .expect("grace 可推进");
    assert!(
        heap.payload_exists(old),
        "guard 与票据未结清时旧 payload 不能释放"
    );
    assert_eq!(heap.slot_state(handle), Some(SharedSlotState::Reclaimable));
    heap.finish_mark_ticket(handle).expect("票据可结清");
    heap.end_access(1).expect("guard 可结束");
    let _ = access;
    heap.advance_grace(handle, 1).expect("grace 可收口");
    assert!(
        !heap.payload_exists(old),
        "全部 lease 结清后旧 payload 释放"
    );
    assert_eq!(heap.slot_state(handle), Some(SharedSlotState::Live));
}

#[test]
fn pin_defers_forward_without_changing_generations() {
    let (mut heap, handle, old) = published(8);
    heap.pin(handle).expect("pin 可获取");
    let destination = heap
        .allocate_payload(7, 5, 0, 8)
        .expect("目标 payload 可建立");
    assert_eq!(
        heap.forward(handle, destination.id, 3, 1)
            .expect("被 pin 的 forward 是推迟而不是失败"),
        SharedForward::Deferred(ForwardDeferred::Pinned)
    );
    let record = heap.slot_record(handle).expect("slot 可读");
    assert_eq!(record.forward_generation, 0);
    assert_eq!(record.state, SharedSlotState::Live.raw());
    assert_eq!(heap.current_payload(handle), Some(old));
    assert_eq!(heap.old_payload(handle), None);
    heap.unpin(handle).expect("pin 可释放");
    assert!(matches!(
        heap.forward(handle, destination.id, 3, 1)
            .expect("forward 可成功"),
        SharedForward::Forwarded(_)
    ));
}

#[test]
fn duplicate_and_stale_identity_operations_are_rejected() {
    let (mut heap, handle, _) = published(8);
    let source = heap
        .allocate_payload(7, 6, 0, 8)
        .expect("fresh payload 可建立");
    let second = heap.resolve_payload(source).expect("fresh payload 可发布");
    assert!(
        heap.resolve_payload(source).is_err(),
        "同一 payload 不能重复解析"
    );
    assert!(
        heap.begin_access(0, second).is_err(),
        "token 0 不是合法身份"
    );
    heap.begin_access(2, second).expect("guard 可建立");
    assert!(heap.resolve_access(1).is_err(), "未建立的 token 必须失败");
    let access = heap.resolve_access(2).expect("guard 可解析");
    assert!(heap.resolve_access(2).is_err(), "同一 token 不能重复解析");
    let _ = access;
    heap.end_access(2).expect("guard 可结束");
    assert!(heap.end_access(2).is_err(), "重复结束必须失败");
    // 过期 table / slot / generation 都拒绝，而不是按 slot 猜测对象。
    assert!(
        heap.slot_record(
            super::shared_heap_schema::SharedHandle::new(2, 0, 1).expect("身份可构造")
        )
        .is_none()
    );
    assert!(heap.pin(handle).is_ok());
    assert!(heap.unpin(handle).is_ok());
    assert!(heap.unpin(handle).is_err(), "没有租约时 unpin 必须失败");
    assert!(
        heap.forward(
            handle,
            super::shared_heap_schema::SharedPayloadId::from_raw(0),
            1,
            1
        )
        .is_err()
    );
    assert!(
        heap.forward(
            handle,
            super::shared_heap_schema::SharedPayloadId::from_raw(0),
            0,
            1
        )
        .is_err()
    );
}

#[test]
fn release_advances_generation_on_reuse() {
    let (mut heap, handle, old) = published(8);
    heap.release(handle).expect("无 lease 时可释放");
    assert_eq!(heap.slot_state(handle), Some(SharedSlotState::OwnedFree));
    assert!(!heap.payload_exists(old), "释放同时回收 current payload");
    let source = heap
        .allocate_payload(7, 7, 0, 8)
        .expect("fresh payload 可建立");
    let reused = heap.resolve_payload(source).expect("槽可复用");
    assert_eq!(reused.slot(), handle.slot(), "复用同一个 slot");
    assert_ne!(
        reused.generation(),
        handle.generation(),
        "复用必须推进 handle generation"
    );
    assert!(heap.slot_record(handle).is_none(), "旧 handle 必须失败");
    assert!(heap.release(handle).is_err());
}

#[test]
fn forward_copies_bytes_and_linearizes_current_payload() {
    let (mut heap, handle, old) = published(16);
    heap.begin_access(1, handle).expect("guard 可建立");
    let access = heap.resolve_access(1).expect("guard 可解析");
    heap.store(access, 0, &[1, 2, 3, 4, 5, 6, 7, 8])
        .expect("payload 可写入");
    heap.end_access(1).expect("guard 可结束");
    let destination = heap
        .allocate_payload(7, 8, 0, 16)
        .expect("目标 payload 可建立");
    let _ = heap
        .forward(handle, destination.id, 4, 1)
        .expect("forward 可成功");
    assert_eq!(
        heap.payload_content(destination.id),
        heap.payload_content(old),
        "搬迁必须保持 payload 字节"
    );
    assert_eq!(heap.current_payload(handle), Some(destination.id));
    assert_eq!(heap.old_payload(handle), Some(old));
    heap.end_forward_lease(handle, 4).expect("lease 可结清");
    heap.advance_grace(handle, SHARED_FORWARDING_GRACE_STEPS)
        .expect("grace 可走满");
    // 线性化后新的解析看到新 payload。
    heap.begin_access(5, handle).expect("guard 可建立");
    let access = heap.resolve_access(5).expect("guard 可解析");
    assert_eq!(access.payload, destination.id);
    assert!(heap.store(access, 0, &[9]).is_ok());
    heap.end_access(5).expect("guard 可结束");
    // forward generation 必须严格递增。
    let next = heap
        .allocate_payload(7, 9, 0, 16)
        .expect("目标 payload 可建立");
    assert!(
        heap.forward(handle, next.id, 6, 3).is_err(),
        "forward generation 必须等于当前值加一"
    );
    assert!(matches!(
        heap.forward(handle, next.id, 6, 2).expect("forward 可成功"),
        SharedForward::Forwarded(_)
    ));
}

#[test]
fn mark_cycle_limits_one_side_mark_per_object() {
    let (mut heap, handle, _) = published(8);
    assert!(heap.mark_ticket(handle).is_err(), "没有 cycle 时不能发票据");
    heap.begin_mark_cycle(1).expect("cycle 可开始");
    assert!(heap.mark_ticket(handle).expect("首次可发出"));
    assert!(!heap.mark_ticket(handle).expect("同一 cycle 只能一次"));
    heap.finish_mark_ticket(handle).expect("票据可结清");
    assert!(
        heap.finish_mark_ticket(handle).is_err(),
        "票据必须恰好结清一次"
    );
    assert!(
        heap.begin_mark_cycle(1).is_err(),
        "cycle epoch 必须单调递增"
    );
    heap.begin_mark_cycle(2).expect("新 cycle 可开始");
    assert!(heap.mark_ticket(handle).expect("新 cycle 可再次发出"));
}
