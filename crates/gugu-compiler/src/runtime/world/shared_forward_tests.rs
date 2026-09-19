//! world 级 SharedHeap forwarding 闭环测试：搬迁、grace、sweep 与 block 搬迁都走真实路径。
//!
//! 全部在进程内运行：共享 block 的物理页由 extent 替身按 32 KiB 提交，不读镜像、不起子进程。

use super::RawWorld;
use super::heap_impl::ManagedPlacement;
use super::heap_tests::{configured_world, gc_contract};
use super::shared_forward_impl::{SharedForwardOutcome, SharedForwardTotals, SharedPlaneReport};
use crate::runtime::gc_metadata_contract::GC_BLOCK_BYTES;
use crate::runtime::inbox::ServiceBudget;
use crate::runtime::mark::MarkCycleState;
use crate::runtime::message::{HandleForward, IntegrityTag, MarkTarget, MessageState};
use crate::runtime::shared_heap::ForwardDeferred;
use crate::runtime::shared_heap_schema::{SharedHandle, SharedSlotState};
use crate::runtime::size_class::RuntimeSizeClassId;
use crate::runtime::slab::SlabGeneration;

/// pressure 预算：这些用例都用无界额度，让一轮就走到收敛。
fn budget() -> ServiceBudget {
    ServiceBudget::pressure(u32::MAX, u64::MAX)
}

/// 在一个 access guard 内读取共享字段并结清 guard。
fn read_shared_field(world: &mut RawWorld, handle: SharedHandle, offset: u32) -> u64 {
    let token = world
        .begin_shared_access(handle)
        .expect("共享 guard 可建立");
    let value = world
        .load_shared_field_with(token, offset)
        .expect("共享字段可读");
    world.end_shared_access(token).expect("共享 guard 可结清");
    value
}

/// 走一遍完整的「mark 收敛 → 关闭 cycle → 推进共享平面」。
///
/// `marked` 里的 handle 先经真实 `MarkTicket` 标记，因此「未标记」是真实标记结果，而不是
/// 直接改写 side mark 得来的假象。
fn mark_and_run_shared_plane(world: &mut RawWorld, marked: &[SharedHandle]) -> SharedPlaneReport {
    let scope: Vec<u32> = (0..world.owner_count()).collect();
    if matches!(
        world.mark_plane().expect("mark 平面已配置").state(),
        MarkCycleState::Idle | MarkCycleState::Complete
    ) {
        world.begin_mark_cycle(&scope).expect("mark cycle 可开始");
    }
    // 来源身份必须是可解析的全局 block 身份：用 owner 0 上真实分配的 LocalHeap 对象所在 block。
    let holder = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Nursery, false)
        .expect("holder 可分配");
    let source_block = world
        .managed_block_ref(0, holder)
        .expect("源 block")
        .id
        .raw();
    for handle in marked {
        let record = *world.shared_payload_block(*handle).expect("登记项可读");
        let credit = world
            .mark_plane_mut()
            .expect("mark 平面已配置")
            .publish_ticket(0, record.owner)
            .expect("credit 可 acquire");
        world
            .stage_ticket(
                0,
                record.owner,
                credit,
                source_block,
                MarkTarget::Shared {
                    handle_table: handle.table(),
                    handle_slot: handle.slot(),
                    handle_generation: handle.generation(),
                },
                record.payload_bytes,
            )
            .expect("共享 ticket 可入队");
    }
    let pass = world.run_mark_pass(&scope).expect("mark pass 可执行");
    assert!(
        pass.termination.converged(),
        "mark 必须收敛，仍有未归零的条件: {:?}",
        pass.termination.blocking()
    );
    world.finish_mark_cycle().expect("mark cycle 可关闭");
    world.run_shared_plane(&scope).expect("共享平面可推进")
}

/// 组装一条与当前在飞搬迁一致的搬迁通知；调用方可以逐项篡改后重新签名。
fn notice_for(world: &RawWorld, handle: SharedHandle) -> HandleForward {
    let record = *world.shared_payload_block(handle).expect("登记项可读");
    let pending = record.pending_forward.expect("必须在飞");
    let target = world.token(record.owner);
    let plane = world.mark_plane().expect("mark 平面已配置");
    let mut message = HandleForward {
        next: None,
        target,
        handle_table: handle.table(),
        handle_slot: handle.slot(),
        handle_generation: handle.generation(),
        forward_generation: pending.forward_generation,
        old_payload: pending.old_payload,
        new_payload: record.payload,
        cycle_epoch: plane.cycle(),
        topology_epoch: plane.topology(),
        bytes: pending.bytes,
        state: MessageState::Staged,
        integrity: IntegrityTag {
            generation: SlabGeneration::from_raw(target.generation.raw()),
            class: RuntimeSizeClassId::from_raw(0),
            owner_id: target.owner_id,
            route_key: target.route_key,
            checksum: 0,
        },
    };
    resign(world, &mut message);
    message
}

/// 重新计算 integrity：篡改身份字段之后必须重签，否则失败会被归因到校验和而不是被测字段。
fn resign(world: &RawWorld, message: &mut HandleForward) {
    message.integrity.checksum =
        IntegrityTag::compute_handle_forward(world.integrity_secret(), message);
}

/// 一次搬迁必须发布通知、切换 current payload，并在目标 owner 消费后结清 lease 与 grace。
#[test]
fn forward_publishes_handle_forward_and_settles_grace_in_cycle() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 31, 2, 64);
    let handle = world.allocate_shared_object(1, 32).expect("共享对象可分配");
    let child = world
        .allocate_managed(1, 0, 16, ManagedPlacement::Nursery, false)
        .expect("child 可分配");
    world
        .store_shared_managed_field(1, 0, handle, 8, child, Some(1))
        .expect("共享字段可写");
    let before = *world.shared_payload_block(handle).expect("登记项可读");
    let SharedForwardOutcome::Forwarded(forwarded) =
        world.forward_shared_payload(0, handle).expect("搬迁可执行")
    else {
        panic!("无 pin 的搬迁必须成功");
    };
    assert_eq!(forwarded.old_payload, before.payload);
    assert_eq!(forwarded.bytes, 32);
    // 登记项已经指向新位置，旧位置进入待结清记录。
    let after = *world.shared_payload_block(handle).expect("登记项可读");
    assert_ne!(
        after.payload, before.payload,
        "搬迁必须建立新的 payload 身份"
    );
    assert_ne!(
        after.block_offset, before.block_offset,
        "新 payload 必须落在新的偏移"
    );
    let pending = after.pending_forward.expect("必须留下待结清记录");
    assert_eq!(pending.old_payload, before.payload);
    assert_eq!(pending.old_block, before.block);
    assert_eq!(pending.forward_generation, 1);
    // grace 状态：current 已切换、旧 payload 仍在、lease 尚未结清。
    let slot = world
        .shared_heap()
        .expect("SharedHeap 已配置")
        .slot_record(handle)
        .expect("slot 可读");
    assert_eq!(
        SharedSlotState::from_raw(slot.state),
        Some(SharedSlotState::Grace)
    );
    assert_eq!(slot.forward_generation, 1);
    assert_eq!(slot.forwarding_leases, 1);
    assert_eq!(world.shared_forward_pending(), 1);
    {
        let heap = world.shared_heap().expect("SharedHeap 已配置");
        assert!(
            heap.payload_exists(before.payload),
            "旧 payload 必须仍在 grace"
        );
        assert_eq!(
            heap.payload_content(before.payload),
            heap.payload_content(after.payload),
            "搬迁必须复制全部字节"
        );
    }
    assert_eq!(read_shared_field(&mut world, handle, 8), child);
    // 通知已经在目标 owner 的 inbox 里：消费它才结清 lease 与 grace。
    let graced_before = world.pending_grace_nodes();
    let (_, consumed) = world
        .drain_inboxes(1, &budget(), true)
        .expect("目标 owner 可排空");
    assert_eq!(consumed, 1, "搬迁通知必须由目标 owner 消费");
    assert_eq!(
        world.pending_grace_nodes(),
        graced_before + 1,
        "通知 node 必须进入 grace"
    );
    let slot = world
        .shared_heap()
        .expect("SharedHeap 已配置")
        .slot_record(handle)
        .expect("slot 可读");
    assert_eq!(slot.forwarding_leases, 0);
    assert_eq!(
        SharedSlotState::from_raw(slot.state),
        Some(SharedSlotState::Live)
    );
    assert!(
        !world
            .shared_heap()
            .expect("SharedHeap 已配置")
            .payload_exists(before.payload),
        "lease 结清且 grace 走满后旧 payload 必须消失"
    );
    assert!(
        world
            .shared_payload_block(handle)
            .expect("登记项可读")
            .pending_forward
            .is_none()
    );
    assert_eq!(world.shared_forward_pending(), 0);
    assert_eq!(world.shared_forward_totals().freed_bytes, 32);
}

/// guard 结束前旧 payload 必须保持有效，且 guard 内读到的是它当时解析到的字节。
#[test]
fn guard_keeps_old_payload_alive_until_end() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 37, 2, 64);
    let handle = world.allocate_shared_object(1, 32).expect("共享对象可分配");
    let child = world
        .allocate_managed(1, 0, 16, ManagedPlacement::Nursery, false)
        .expect("child 可分配");
    world
        .store_shared_managed_field(1, 0, handle, 8, child, Some(1))
        .expect("共享字段可写");
    let before = *world.shared_payload_block(handle).expect("登记项可读");
    // guard 在搬迁之前打开：它捕获的 payload 必须在 guard 结束前一直有效。
    let token = world.begin_shared_access(handle).expect("guard 可建立");
    let SharedForwardOutcome::Forwarded(_) =
        world.forward_shared_payload(0, handle).expect("搬迁可执行")
    else {
        panic!("无 pin 的搬迁必须成功");
    };
    let (_, consumed) = world
        .drain_inboxes(1, &budget(), true)
        .expect("目标 owner 可排空");
    assert_eq!(consumed, 1);
    // lease 已经在消费处结清；在飞记录保留到旧 payload 真正释放，因此它仍是 GC 工作。
    assert_eq!(
        world
            .shared_heap()
            .expect("SharedHeap 已配置")
            .slot_record(handle)
            .expect("slot 可读")
            .forwarding_leases,
        0,
        "lease 必须在消费处结清"
    );
    assert_eq!(
        world.shared_forward_pending(),
        1,
        "旧 payload 未释放时记录必须保留"
    );
    assert!(
        world
            .shared_heap()
            .expect("SharedHeap 已配置")
            .payload_exists(before.payload),
        "guard 未结束，旧 payload 必须保持有效"
    );
    assert_eq!(
        world
            .load_shared_field_with(token, 8)
            .expect("guard 内可读"),
        child,
        "guard 必须读到它当时解析到的 payload"
    );
    assert_eq!(
        SharedSlotState::from_raw(
            world
                .shared_heap()
                .expect("SharedHeap 已配置")
                .slot_record(handle)
                .expect("slot 可读")
                .state
        ),
        Some(SharedSlotState::Reclaimable),
        "grace 走满但 guard 仍持有旧 payload"
    );
    // 结束 guard 之后 settle 才允许回收。
    world.end_shared_access(token).expect("guard 可结束");
    world.settle_shared_forwards().expect("settle 可执行");
    assert!(
        !world
            .shared_heap()
            .expect("SharedHeap 已配置")
            .payload_exists(before.payload),
        "guard 结束后旧 payload 才消失"
    );
    assert!(
        world
            .shared_payload_block(handle)
            .expect("登记项可读")
            .pending_forward
            .is_none()
    );
}

/// pin 推迟搬迁且不改动任何状态；被 pin 的 payload 同时让 block 无法归零。
#[test]
fn pin_defers_forward_and_evacuation_skips_pinned_payloads() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 41, 2, 64);
    // 三个受害者加一个被 pin 的存活 payload 正好填满一个共享 block；随后的分配打开新 block，
    // 因此这个 block 就此封口，而它唯一的存活 payload 就是被 pin 的那个。
    let chunk = GC_BLOCK_BYTES / 4;
    for _ in 0..3 {
        world
            .allocate_shared_object(1, chunk)
            .expect("共享对象可分配");
    }
    let survivor = world
        .allocate_shared_object(1, chunk)
        .expect("共享对象可分配");
    // 封口对象也要标记：它是一个真实的存活对象，本用例只考察被 pin 的 payload 阻碍搬迁。
    let sealer = world.allocate_shared_object(1, 16).expect("共享对象可分配");
    world.pin_shared_handle(survivor).expect("可 pin");
    let outcome = world
        .forward_shared_payload(0, survivor)
        .expect("pinned 搬迁返回推迟");
    assert_eq!(
        outcome,
        SharedForwardOutcome::Deferred(ForwardDeferred::Pinned)
    );
    let record = *world.shared_payload_block(survivor).expect("登记项可读");
    assert!(record.pending_forward.is_none(), "推迟不得留下在飞记录");
    let slot = world
        .shared_heap()
        .expect("SharedHeap 已配置")
        .slot_record(survivor)
        .expect("slot 可读");
    assert_eq!(
        slot.forward_generation, 0,
        "推迟不得推进 forward generation"
    );
    assert_eq!(
        world
            .shared_heap()
            .expect("SharedHeap 已配置")
            .current_payload(survivor),
        Some(record.payload)
    );
    // 只标记 survivor 与封口对象：其余三个被 sweep 释放，第一个 block 因此变成「死字节多于
    // 活字节」，而它唯一剩下的存活 payload 是被 pin 的那个。
    let report = mark_and_run_shared_plane(&mut world, &[survivor, sealer]);
    assert_eq!(report.released, 3, "只有未标记对象被释放");
    assert_eq!(report.deferred, 1, "被 pin 的 payload 必须计入推迟");
    let block = *world
        .shared_registry()
        .block_record(record.block.id.arena())
        .expect("block 记录可读");
    assert!(block.sealed && block.dead_bytes > block.live_bytes);
    assert!(!block.is_empty(), "被 pin 的 payload 让 block 无法归零");
    // unpin 之后同一个 payload 可以正常搬迁。
    world.unpin_shared_handle(survivor).expect("可 unpin");
    assert!(matches!(
        world
            .forward_shared_payload(0, survivor)
            .expect("搬迁可执行"),
        SharedForwardOutcome::Forwarded(_)
    ));
}

/// 错误目标、篡改校验、过期 handle、跳号 generation 与重放都必须干净拒绝。
#[test]
fn handle_forward_rejections_leave_state_untouched() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 43, 2, 64);
    let handle = world.allocate_shared_object(1, 32).expect("共享对象可分配");
    assert!(matches!(
        world.forward_shared_payload(0, handle).expect("搬迁可执行"),
        SharedForwardOutcome::Forwarded(_)
    ));
    // 快照：每次失败之后 lease、grace 与在飞记录都必须与失败前逐项一致。
    let snapshot = |world: &RawWorld| {
        (
            world
                .shared_heap()
                .expect("SharedHeap 已配置")
                .slot_record(handle),
            world
                .shared_payload_block(handle)
                .expect("登记项可读")
                .pending_forward,
            world.shared_forward_pending(),
        )
    };
    let baseline = snapshot(&world);
    let notice = notice_for(&world, handle);
    // (a) 目标 owner 错误。
    let error = world
        .service_handle_forward(0, &notice)
        .expect_err("错误目标必须被拒绝");
    assert!(error.message().contains("错误 owner"), "{error:?}");
    assert_eq!(snapshot(&world), baseline);
    // (b) checksum 被篡改。
    let mut tampered = notice_for(&world, handle);
    tampered.integrity.checksum ^= 0x5A5A_5A5A;
    let error = world
        .service_handle_forward(1, &tampered)
        .expect_err("篡改必须被拒绝");
    assert!(error.message().contains("integrity"), "{error:?}");
    assert_eq!(snapshot(&world), baseline);
    // (c) 过期 handle generation：重签之后仍然必须被登记项拒绝。
    let mut stale = notice_for(&world, handle);
    stale.handle_generation += 1;
    resign(&world, &mut stale);
    let error = world
        .service_handle_forward(1, &stale)
        .expect_err("过期 handle 必须被拒绝");
    assert!(
        error.message().contains("未在世界 registry 登记"),
        "{error:?}"
    );
    assert_eq!(snapshot(&world), baseline);
    // (d) 跳号的 forward generation。
    let mut skipped = notice_for(&world, handle);
    skipped.forward_generation += 1;
    resign(&world, &mut skipped);
    let error = world
        .service_handle_forward(1, &skipped)
        .expect_err("跳号 generation 必须被拒绝");
    assert!(error.message().contains("不一致"), "{error:?}");
    assert_eq!(snapshot(&world), baseline);
    // 真正消费一次之后，重放同一条消息必须被拒绝：已经没有在飞记录了。
    let (_, consumed) = world
        .drain_inboxes(1, &budget(), true)
        .expect("目标 owner 可排空");
    assert_eq!(consumed, 1);
    assert_eq!(world.shared_forward_pending(), 0);
    let error = world
        .service_handle_forward(1, &notice)
        .expect_err("重放必须被拒绝");
    assert!(error.message().contains("没有对应的在飞搬迁"), "{error:?}");
}

/// 只释放本 cycle 未标记的对象：标记过的存活对象必须原样保留。
#[test]
fn shared_sweep_releases_unmarked_objects_only() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 47, 2, 64);
    let live = world.allocate_shared_object(1, 16).expect("共享对象可分配");
    let dead = world.allocate_shared_object(1, 16).expect("共享对象可分配");
    let dead_payload = world
        .shared_payload_block(dead)
        .expect("登记项可读")
        .payload;
    let block = world.shared_payload_block(dead).expect("登记项可读").block;
    let report = mark_and_run_shared_plane(&mut world, &[live]);
    assert_eq!(report.released, 1, "只有未标记对象被释放");
    assert!(world.shared_registry().get(live).is_some());
    assert!(world.shared_registry().get(dead).is_none());
    assert!(
        !world
            .shared_heap()
            .expect("SharedHeap 已配置")
            .payload_exists(dead_payload)
    );
    assert_eq!(
        world
            .shared_heap()
            .expect("SharedHeap 已配置")
            .slot_state(dead),
        Some(SharedSlotState::OwnedFree)
    );
    let record = *world
        .shared_registry()
        .block_record(block.id.arena())
        .expect("block 记录可读");
    assert_eq!(record.dead_bytes, 16, "释放的字节必须计入 dead");
    assert_eq!(record.live_bytes, 16, "存活对象仍占 live");
    assert_eq!(report.freed_bytes, 16);
}

/// guard 或未结清的搬迁都会推迟释放，且推迟之后仍能在后续 cycle 释放。
#[test]
fn sweep_defers_release_while_guard_or_forward_in_flight() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 53, 2, 64);
    let guarded = world.allocate_shared_object(1, 16).expect("共享对象可分配");
    let token = world.begin_shared_access(guarded).expect("guard 可建立");
    let first = mark_and_run_shared_plane(&mut world, &[]);
    assert_eq!(first.released, 0, "guard 未结束时不得释放");
    assert!(
        world
            .shared_payload_block(guarded)
            .expect("登记项可读")
            .returned,
        "无法释放时必须已经交还"
    );
    world.end_shared_access(token).expect("guard 可结束");
    let second = mark_and_run_shared_plane(&mut world, &[]);
    assert_eq!(second.released, 1, "guard 结束后下一次 cycle 必须释放");
    assert!(world.shared_registry().get(guarded).is_none());
    // 在飞搬迁：搬迁通知尚未被目标 owner 消费时不得释放。
    let in_flight = world.allocate_shared_object(1, 16).expect("共享对象可分配");
    assert!(matches!(
        world
            .forward_shared_payload(0, in_flight)
            .expect("搬迁可执行"),
        SharedForwardOutcome::Forwarded(_)
    ));
    let scope: Vec<u32> = (0..world.owner_count()).collect();
    let third = world.run_shared_plane(&scope).expect("共享平面可推进");
    assert_eq!(third.released, 0, "在飞搬迁的死亡对象必须先结清");
    assert!(world.shared_registry().get(in_flight).is_some());
    assert_eq!(world.shared_forward_pending(), 0, "同一轮已经消费并结清");
    let fourth = world.run_shared_plane(&scope).expect("共享平面可推进");
    assert_eq!(fourth.released, 1, "结清后的死亡对象才释放");
    assert!(world.shared_registry().get(in_flight).is_none());
    assert_eq!(
        world.shared_forward_totals().freed_bytes,
        48,
        "两次对象释放各 16 字节，加上搬迁淘汰的旧 payload 16 字节"
    );
}

/// 死字节不少于活字节的封口 block 必须被搬空：存活 payload 落在正在填充的 block。
#[test]
fn mostly_dead_shared_block_is_evacuated_and_becomes_empty() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 59, 2, 64);
    let chunk = GC_BLOCK_BYTES / 4;
    let mut victims = Vec::new();
    for _ in 0..3 {
        victims.push(
            world
                .allocate_shared_object(1, chunk)
                .expect("共享对象可分配"),
        );
    }
    let survivor = world
        .allocate_shared_object(1, chunk)
        .expect("共享对象可分配");
    // 第五个分配打开新 block，因此存活 payload 所在的 block 就此封口；它同样必须标记。
    let sealer = world.allocate_shared_object(1, 16).expect("共享对象可分配");
    let block = world
        .shared_payload_block(survivor)
        .expect("登记项可读")
        .block;
    let report = mark_and_run_shared_plane(&mut world, &[survivor, sealer]);
    assert_eq!(
        report.released,
        u64::try_from(victims.len()).expect("受害者数")
    );
    assert_eq!(report.deferred, 0);
    assert_eq!(report.forwarded, 1, "存活 payload 必须被搬走");
    assert_eq!(report.forwarded_bytes, u64::from(chunk));
    assert_eq!(report.empty_blocks, 1, "搬空后旧 block 必须归零");
    let record = *world
        .shared_registry()
        .block_record(block.id.arena())
        .expect("block 记录可读");
    assert!(record.is_empty(), "旧 block 必须封口且无存活 payload");
    // 存活 payload 已经落在另一个 block，登记项与内容都跟着走。
    let moved = *world.shared_payload_block(survivor).expect("登记项可读");
    assert_ne!(moved.block, block, "搬迁必须换到正在填充的 block");
    assert!(moved.pending_forward.is_none(), "通知已被同一轮消费");
    assert!(
        world
            .shared_heap()
            .expect("SharedHeap 已配置")
            .payload_exists(moved.payload)
    );
}

/// 管理权转移必须同时带走共享 block 的 registry owner 与 card table manager。
#[test]
fn handover_moves_shared_block_manager_and_registry_owner() {
    use crate::runtime::barrier::BarrierFlushReason;

    let contract = gc_contract();
    let mut world = configured_world(&contract, 61, 2, 64);
    let handle = world.allocate_shared_object(0, 32).expect("共享对象可分配");
    let block = world
        .shared_payload_block(handle)
        .expect("登记项可读")
        .block;
    let descriptor = u64::from(block.id.arena());
    let manager = world.token(1);
    let child = world
        .allocate_managed(1, 0, 16, ManagedPlacement::Nursery, false)
        .expect("child 可分配");
    // owner 0 退役到 owner 1：管理权随之转移。
    world.retire(0, manager, &budget()).expect("owner 可退役");
    assert_eq!(
        world
            .shared_payload_block(handle)
            .expect("登记项可读")
            .owner,
        1,
        "registry owner 必须跟随新 manager"
    );
    assert_eq!(
        world
            .barrier()
            .table(descriptor)
            .expect("共享 block 的 card table 已登记")
            .manager(),
        manager,
        "card table manager 必须跟随新 manager"
    );
    // 转移之后写入共享字段：card 批次必须投给新 manager，而不是已退役的 owner 0。
    world
        .store_shared_managed_field(0, 0, handle, 8, child, Some(1))
        .expect("共享字段可写");
    let published = world
        .flush_barrier(0, 0, BarrierFlushReason::ProducerStopGate)
        .expect("冲刷可执行");
    assert_eq!(published, 1, "card 批次必须以新 manager 为目标");
    let (_, consumed) = world
        .drain_inboxes(1, &budget(), true)
        .expect("新 manager 可排空");
    assert_eq!(consumed, 1);
    assert!(
        world
            .barrier()
            .table(descriptor)
            .expect("共享 block 的 card table 已登记")
            .dirty()
            > 0,
        "新 manager 必须真的把 card 写进自己的表"
    );
}

/// 在飞搬迁是 GC 工作：未结清时 mark termination 不收敛，结清后收敛。
#[test]
fn pending_forward_blocks_mark_termination() {
    use crate::runtime::mark::MarkCondition;

    let contract = gc_contract();
    let mut world = configured_world(&contract, 67, 2, 64);
    // cycle 必须先打开：snapshot 边界会排空全部 inbox，因此搬迁通知只能在 cycle 内发布。
    world.begin_mark_cycle(&[0, 1]).expect("mark cycle 可开始");
    let handle = world.allocate_shared_object(1, 16).expect("共享对象可分配");
    assert!(matches!(
        world.forward_shared_payload(0, handle).expect("搬迁可执行"),
        SharedForwardOutcome::Forwarded(_)
    ));
    // 只推进 owner 0：通知仍在 owner 1 的 inbox 里，因此 forwarding work 非零。
    let blocked = world.run_mark_pass(&[0]).expect("mark pass 可执行");
    assert!(!blocked.termination.converged(), "在飞搬迁必须阻塞收敛");
    assert_eq!(
        blocked.termination.get(MarkCondition::ForwardingWork),
        1,
        "在飞搬迁必须计入 forwarding work"
    );
    // 消费并结清之后同一个 cycle 才能收敛。
    let (_, consumed) = world
        .drain_inboxes(1, &budget(), true)
        .expect("目标 owner 可排空");
    assert_eq!(consumed, 1);
    assert_eq!(world.shared_forward_pending(), 0);
    let converged = world.run_mark_pass(&[0, 1]).expect("mark pass 可执行");
    assert!(
        converged.termination.converged(),
        "结清之后必须收敛，仍有未归零的条件: {:?}",
        converged.termination.blocking()
    );
}

/// 验收项「共享访问额外成本不扩散到 LocalHeap」：只有 LocalHeap 对象的世界零共享状态。
#[test]
fn local_heavy_world_stays_free_of_shared_state() {
    use crate::runtime::gc_metadata_schema::GcRootKindV1;

    let contract = gc_contract();
    let mut world = configured_world(&contract, 71, 2, 64);
    let holder = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Old, false)
        .expect("holder 可分配");
    let child = world
        .allocate_managed(1, 0, 16, ManagedPlacement::Old, false)
        .expect("child 可分配");
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 0)
        .expect("根槽可登记");
    world.set_managed_root(slot, holder).expect("根可写");
    world
        .store_managed_field(0, 0, holder, 0, child)
        .expect("跨 owner store 可执行");
    let report = world.run_gc_cycle(true).expect("真实 cycle 可完成");
    assert!(report.cycle_completed, "cycle 必须真的完成");
    assert_eq!(report.forced_cycles, 1);
    assert_eq!(report.shared_released, 0);
    assert_eq!(report.shared_forwarded, 0);
    assert_eq!(report.shared_forwarded_bytes, 0);
    assert_eq!(report.shared_deferred_forwards, 0);
    assert_eq!(report.shared_empty_blocks, 0);
    assert_eq!(world.shared_registry().len(), 0);
    assert_eq!(world.shared_forward_pending(), 0);
    assert_eq!(
        world.shared_forward_totals(),
        SharedForwardTotals::default(),
        "没有共享访问时全部累计计数必须保持 0"
    );
    let heap = world.shared_heap().expect("SharedHeap 已配置");
    assert_eq!(heap.slot_count(), 0, "没有共享对象时不得分配 handle slot");
    assert_eq!(heap.payload_count(), 0);
    assert!(
        world
            .shared_registry()
            .descriptor_high_water()
            .eq(&super::shared_heap_impl::SHARED_DESCRIPTOR_BASE),
        "没有共享对象时不得占用共享编号段"
    );
}
