//! world 级 mark cycle 闭环测试：跨 owner ticket、credit 轨迹与收敛判定都走真实路径。

use super::heap_impl::ManagedPlacement;
use super::heap_tests::{configured_world, gc_contract};
use crate::runtime::gc_metadata_schema::GcRootKindV1;
use crate::runtime::mark::MarkCondition;

/// ticket 的来源身份必须是可解析的全局块身份，且真实跨 owner 路径必须带着它完成校验。
#[test]
fn mark_ticket_source_identity_is_global_and_resolved() {
    use crate::runtime::local_heap::ManagedBlockId;

    let contract = gc_contract();
    let mut world = configured_world(&contract, 13, 2, 64);
    let holder = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Old, false)
        .expect("holder 可分配");
    let child = world
        .allocate_managed(1, 0, 16, ManagedPlacement::Old, false)
        .expect("child 可分配");
    let source_id = world.managed_block_ref(0, holder).expect("源 block").id;
    // 正向：真实存在的 block 身份必须解析回同一个身份。
    assert_eq!(
        world
            .resolve_source_block(source_id.raw())
            .expect("全局身份必须可解析"),
        source_id
    );
    assert!(
        source_id.raw() == source_id.arena() * 64 + source_id.index(),
        "全局身份必须同时编码 arena 与下标"
    );
    // 反向 A：descriptor 超出已登记范围，无法解析出 owner。
    let error = world
        .resolve_source_block(u32::MAX)
        .expect_err("无法解析的身份必须被拒绝");
    assert!(
        error.to_string().contains("来源 block 身份无法解析"),
        "失败原因必须点名来源身份：{error}"
    );
    // 反向 B：身份可解析，但该 block 没有提交在本 heap 里。
    let uncommitted = ManagedBlockId::new(source_id.arena(), 63).expect("身份可构造");
    let error = world
        .resolve_source_block(uncommitted.raw())
        .expect_err("未提交的 block 必须被拒绝");
    assert!(
        error.to_string().contains("来源 block 已不存在"),
        "失败原因必须点名来源块：{error}"
    );
    // 真实路径：跨 owner 引用必须发布并消费，消费过程会校验来源身份。
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 0)
        .expect("根槽可登记");
    world.set_managed_root(slot, holder).expect("根可写");
    world
        .store_managed_field(0, 0, holder, 0, child)
        .expect("跨 owner store 可执行");
    let pass = world.run_mark_pass(&[0, 1]).expect("mark pass 可执行");
    assert_eq!(pass.tickets_published, 1, "跨 owner 引用必须走 ticket");
    assert_eq!(pass.tickets_consumed, 1);
    assert_eq!(world.mark_worklist_items(), 0, "工作项必须被消费掉");
    assert!(world.managed_object(child).is_ok());
}

/// 共享 handle ticket：车道往返后消费端只经 handle 标记，lease 在消费处结清。
#[test]
fn shared_mark_ticket_round_trips_and_settles_handle_lease() {
    use crate::runtime::message::MarkTarget;

    let contract = gc_contract();
    let mut world = configured_world(&contract, 23, 2, 64);
    // 来源身份必须是可解析的全局 block 身份：用 owner 0 上真实分配的对象所在 block。
    let holder = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Nursery, false)
        .expect("holder 可分配");
    let source_block = world
        .managed_block_ref(0, holder)
        .expect("源 block")
        .id
        .raw();
    // 共享对象走世界级 registry：block 身份与 LocalHeap arena descriptor 不得重叠。
    let handle = world.allocate_shared_object(1, 16).expect("共享对象可分配");
    let record = world.shared_payload_block(handle).expect("登记项可读");
    assert_eq!(record.owner, 1);
    assert!(
        record.block.id.arena() >= super::shared_heap_impl::SHARED_DESCRIPTOR_BASE,
        "共享 block descriptor 必须来自独立编号段"
    );
    assert!(
        world
            .managed_arenas()
            .iter()
            .all(|arena| arena.descriptor < super::shared_heap_impl::SHARED_DESCRIPTOR_BASE),
        "LocalHeap arena descriptor 不得进入共享编号段"
    );
    // credit 只能在 cycle 内 acquire：先打开 cycle，再入队共享 ticket。
    world.begin_mark_cycle(&[0, 1]).expect("mark cycle 可开始");
    let credit = world
        .mark_plane_mut()
        .expect("mark 平面已配置")
        .publish_ticket(0, 1)
        .expect("credit 可 acquire");
    world
        .stage_ticket(
            0,
            1,
            credit,
            source_block,
            MarkTarget::Shared {
                handle_table: handle.table(),
                handle_slot: handle.slot(),
                handle_generation: handle.generation(),
            },
            16,
        )
        .expect("共享 ticket 可入队");
    let pass = world.run_mark_pass(&[0, 1]).expect("mark pass 可执行");
    assert_eq!(
        pass.tickets_consumed, 1,
        "共享 ticket 必须被目标 owner 消费"
    );
    let record = world
        .shared_heap()
        .expect("SharedHeap 已配置")
        .slot_record(handle)
        .expect("slot 可读");
    assert_eq!(record.mark_tickets, 0, "mark lease 必须在消费处结清");
    assert_eq!(record.access_guards, 0);
    assert_eq!(
        world
            .mark_plane()
            .expect("mark 平面已配置")
            .mark_credit_pending(),
        0
    );
    // 反向：过期 handle generation 的 ticket 必须在写任何标记之前失败。
    // 换一个新的 world：上一个 cycle 的 credit 已经消费，未归还的 credit 不允许开新 cycle。
    let mut stale = configured_world(&contract, 23, 2, 64);
    let stale_source = stale
        .allocate_managed(0, 0, 16, ManagedPlacement::Nursery, false)
        .expect("holder 可分配");
    let stale_block = stale
        .managed_block_ref(0, stale_source)
        .expect("源 block")
        .id
        .raw();
    stale
        .begin_mark_cycle(&[0, 1])
        .expect("stale world 的 mark cycle 可开始");
    let credit = stale
        .mark_plane_mut()
        .expect("mark 平面已配置")
        .publish_ticket(0, 1)
        .expect("credit 可 acquire");
    stale
        .stage_ticket(
            0,
            1,
            credit,
            stale_block,
            MarkTarget::Shared {
                handle_table: handle.table(),
                handle_slot: handle.slot(),
                handle_generation: handle.generation() + 1,
            },
            16,
        )
        .expect("过期 ticket 仍可入队");
    let error = stale
        .run_mark_pass(&[0, 1])
        .expect_err("过期 handle 必须被拒绝");
    assert!(
        error.to_string().contains("registry"),
        "失败原因必须点名共享 handle 身份来源：{error}"
    );
    // 世界 registry 与 SharedHeap 各自拒绝一次：把过期 handle 换成未知 slot 也一样。
    let error = stale
        .shared_payload_block(
            crate::runtime::shared_heap_schema::SharedHandle::new(0, handle.slot() + 9, 1)
                .expect("身份可构造"),
        )
        .expect_err("未登记的 slot 必须被拒绝");
    assert!(
        error.to_string().contains("未在世界 registry 登记")
            || error.to_string().contains("registry")
    );
}

/// 2 owner：owner 0 的 holder 指向 owner 1 的 child，mark pass 必须跨 owner 完成标记。
#[test]
fn mark_pass_traces_cross_owner_tickets_and_records_credit_trace() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 5, 2, 64);
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 0)
        .expect("根槽可登记");
    let holder = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Nursery, false)
        .expect("holder 可分配");
    let child = world
        .allocate_managed(1, 0, 16, ManagedPlacement::Nursery, false)
        .expect("child 可分配");
    world.set_managed_root(slot, holder).expect("根可写");
    world
        .store_managed_field(0, 0, holder, 0, child)
        .expect("跨 owner store 可执行");
    let pass = world.run_mark_pass(&[0, 1]).expect("mark pass 可执行");
    assert_eq!(pass.marked, 2, "两个 owner 的对象都必须被标记");
    assert_eq!(pass.tickets_published, 1, "跨 owner 引用必须走 ticket");
    assert_eq!(pass.tickets_consumed, 1);
    assert!(
        pass.termination.converged(),
        "未收敛条件 {:?}",
        pass.termination.blocking()
    );
    // credit 轨迹：源 owner 0 acquire → 目标 consume → 归还，无遗留。
    let plane = world.mark_plane().expect("mark 平面已配置");
    assert_eq!(plane.credit(0).expect("账本可读").returned(), 1);
    assert_eq!(plane.credit(0).expect("账本可读").pending(), 0);
    assert_eq!(plane.mailbox(1).expect("mailbox 可读").consumed(), 1);
    assert_eq!(plane.mailbox(1).expect("mailbox 可读").pending(), 0);
    assert_eq!(plane.mark_credit_pending(), 0);
    assert_eq!(plane.mailbox_pending(), 0);
    assert_eq!(plane.forwarded_pending(), 0);
    // 四个 mark credit 来源在收敛后必须全部归零。
    let snapshot = world.credit_snapshot();
    assert_eq!(snapshot.mark_credit, 0);
    assert_eq!(snapshot.mark_mailbox, 0);
    assert_eq!(snapshot.mark_worklist, 0);
    assert_eq!(snapshot.forwarding_work, 0);
    world.finish_mark_cycle().expect("收敛后可完成 cycle");
    assert_eq!(world.mark_worklist_items(), 0, "完成后 worklist 必须清空");
    assert!(world.managed_object(holder).is_ok());
    assert!(world.managed_object(child).is_ok());
}

/// 1 owner：owner 内环不产生 ticket，且重复 pass 必须收敛并推进 cycle epoch。
#[test]
fn cyclic_owner_local_graph_terminates_without_tickets() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 9, 1, 64);
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 0)
        .expect("根槽可登记");
    let first = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Nursery, false)
        .expect("对象可分配");
    let second = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Nursery, false)
        .expect("对象可分配");
    world.set_managed_root(slot, first).expect("根可写");
    world
        .store_managed_field(0, 0, first, 0, second)
        .expect("store 可执行");
    world
        .store_managed_field(0, 0, second, 0, first)
        .expect("store 可执行");
    let pass = world.run_mark_pass(&[0]).expect("mark pass 可执行");
    assert_eq!(pass.marked, 2, "环上两个对象都必须被标记");
    assert_eq!(pass.tickets_published, 0, "owner 内环不产生 ticket");
    assert_eq!(pass.cycle, 1);
    assert!(pass.termination.converged());
    world.finish_mark_cycle().expect("收敛后可完成 cycle");
    // 图未变，但第二个 cycle 仍必须重新走完并收敛，epoch 必须推进。
    let pass = world.run_mark_pass(&[0]).expect("第二个 mark pass 可执行");
    assert_eq!(pass.cycle, 2);
    assert_eq!(pass.marked, 2);
    assert!(pass.termination.converged());
    assert_eq!(
        world
            .mark_plane()
            .expect("mark 平面已配置")
            .mark_credit_pending(),
        0
    );
}

/// 只有七个条件同时为 0 才允许宣布完成；有 owner 未参与时不得推进。
#[test]
fn mark_pass_refuses_completion_while_a_mailbox_is_occupied() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 17, 2, 64);
    let slot = world
        .register_managed_root(GcRootKindV1::CoroutineFrame, 0)
        .expect("根槽可登记");
    let holder = world
        .allocate_managed(0, 0, 16, ManagedPlacement::Nursery, false)
        .expect("holder 可分配");
    let child = world
        .allocate_managed(1, 0, 16, ManagedPlacement::Nursery, false)
        .expect("child 可分配");
    world.set_managed_root(slot, holder).expect("根可写");
    world
        .store_managed_field(0, 0, holder, 0, child)
        .expect("跨 owner store 可执行");
    // scope 只含 owner 0：ticket 投给 owner 1 却无人消费，mailbox 与 pending-credit 非 0。
    let pass = world.run_mark_pass(&[0]).expect("mark pass 可执行");
    assert_eq!(pass.termination.get(MarkCondition::Mailbox), 1);
    assert!(pass.termination.get(MarkCondition::PendingCredit) >= 1);
    assert!(!pass.termination.converged());
    assert!(
        world.finish_mark_cycle().is_err(),
        "mailbox 非空时不得宣布 cycle 完成"
    );
    // 把 owner 1 纳入 scope 后同一 cycle 必须继续并收敛。
    let pass = world.run_mark_pass(&[0, 1]).expect("补齐 owner 后可执行");
    assert!(pass.termination.converged());
    assert_eq!(pass.tickets_consumed, 1);
    world.finish_mark_cycle().expect("收敛后可完成");
    assert!(world.managed_object(child).is_ok());
}
