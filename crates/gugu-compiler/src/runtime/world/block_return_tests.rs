//! owner-directed managed block return 的确定性回归。
//!
//! 全部在进程内运行：断言的是块状态、账本分类、inbox 消费与 provider committed，
//! 不是内部调用顺序。

use super::RawWorld;
use super::heap_impl::ManagedPlacement;
use super::heap_tests::{configured_world, gc_contract, heap_world};
use crate::runtime::gc_metadata_contract::GC_BLOCK_BYTES;
use crate::runtime::inbox::ServiceBudget;
use crate::runtime::local_heap::ManagedBlockId;
use crate::runtime::local_heap_schema::HeapBlockState;
use crate::runtime::model::GRACE_STEPS;
use crate::runtime::slab::RawInvariant;

fn old_leaf(world: &mut RawWorld) -> (u64, ManagedBlockId) {
    let address = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Old)
        .expect("leaf 可分配");
    let block = world
        .managed_block_ref(0, address)
        .expect("block 身份可解析")
        .id;
    (address, block)
}

fn drive_until_pending(world: &mut RawWorld, block: ManagedBlockId) {
    world.note_candidate_dirty(block).expect("候选通知成功");
    for _ in 0..64 {
        let _ = world.drive_candidates(4096).expect("候选推进成功");
        if world.managed_accounting(0).pending_return_bytes() == 0 {
            continue;
        }
        let record = world
            .heap(0)
            .expect("堆可读")
            .block_record(block)
            .expect("记录可读");
        if HeapBlockState::from_raw(record.state) == Some(HeapBlockState::ReturnPending) {
            return;
        }
        if let Ok(Some((start, span))) = world.heap(0).expect("堆可读").large_span_covering(block)
        {
            let all_pending = (0..span).all(|step| {
                let member = ManagedBlockId::new(block.arena(), start + step).expect("成员身份");
                let record = world
                    .heap(0)
                    .expect("堆可读")
                    .block_record(member)
                    .expect("记录可读");
                HeapBlockState::from_raw(record.state) == Some(HeapBlockState::ReturnPending)
            });
            if all_pending {
                return;
            }
        }
    }
}

fn drain(world: &mut RawWorld, owner: u32) {
    world
        .drain_inboxes(owner, &ServiceBudget::pressure(u32::MAX, u64::MAX), true)
        .expect("inbox 可排空");
}

fn advance_grace(world: &mut RawWorld, owner: u32) {
    let inbox = world.inbox(owner);
    for _ in 0..GRACE_STEPS {
        world.open_grace(&inbox);
        world.close_grace(&inbox);
        world
            .advance_pending_extent_trims()
            .expect("grace trim 可推进");
    }
}

#[test]
fn empty_old_block_returns_through_inbox_once() {
    let mut world = heap_world();
    let (address, block) = old_leaf(&mut world);
    let before = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    let committed_before = world.provider_stats().committed_bytes;
    drive_until_pending(&mut world, block);
    let pending = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(
        pending.state,
        HeapBlockState::ReturnPending.raw(),
        "候选提交后必须进入 ReturnPending"
    );
    assert!(
        world.managed_object(address).is_err(),
        "sweep 后对象地址必须不可解析"
    );
    assert_eq!(
        world.provider_stats().committed_bytes,
        committed_before,
        "consume 前不得 decommit"
    );
    assert_eq!(
        world.managed_accounting(0).pending_return_bytes(),
        u64::from(GC_BLOCK_BYTES)
    );
    drain(&mut world, 0);
    assert_eq!(world.managed_accounting(0).pending_return_bytes(), 0);
    let after_consume = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(
        after_consume.state,
        HeapBlockState::Free.raw(),
        "consume 后必须进入 Free"
    );
    assert_eq!(
        after_consume.generation,
        before.generation + 1,
        "consume 必须推进世代"
    );
    advance_grace(&mut world, 0);
    assert_eq!(
        world.provider_stats().committed_bytes,
        committed_before - u64::from(GC_BLOCK_BYTES),
        "grace 走完后必须 decommit 一个 block"
    );
    drain(&mut world, 0);
    assert_eq!(
        world.managed_accounting(0).pending_return_bytes(),
        0,
        "二次 drain 不得再 consume"
    );
    world.managed_ledger_invariant(0).expect("账本守恒");
}

#[test]
fn live_block_does_not_enter_return_pending_for_low_ratio() {
    let mut world = heap_world();
    let (address, block) = old_leaf(&mut world);
    world
        .heap_mut(0)
        .expect("堆可写")
        .mark_object(address)
        .expect("对象可标记");
    world.note_candidate_dirty(block).expect("候选通知成功");
    let _ = world.drive_candidates(4096).expect("候选推进成功");
    let record = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_ne!(
        record.state,
        HeapBlockState::ReturnPending.raw(),
        "仍有 live object 的块不得因 live ratio 进入 ReturnPending"
    );
    assert!(world.managed_object(address).is_ok(), "标记对象必须存活");
}

#[test]
fn pin_rejects_heap_block_publish() {
    let mut world = heap_world();
    let (address, block) = old_leaf(&mut world);
    world.pin_managed(0, address).expect("对象可 pin");
    world.note_candidate_dirty(block).expect("候选通知成功");
    let _ = world.drive_candidates(4096).expect("候选推进成功");
    let record = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(
        record.state,
        HeapBlockState::Allocating.raw(),
        "pin 必须把候选退回 Allocating"
    );
    assert_eq!(world.managed_accounting(0).pending_return_bytes(), 0);
}

#[test]
fn nursery_block_never_queues_heap_block() {
    let mut world = heap_world();
    let address = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Nursery)
        .expect("nursery 对象可分配");
    let block = world
        .managed_block_ref(0, address)
        .expect("block 身份可解析")
        .id;
    world.note_candidate_dirty(block).expect("候选通知成功");
    let _ = world.drive_candidates(4096).expect("候选推进成功");
    let err = world.queue_heap_block_return(block);
    assert!(err.is_err(), "nursery 必须拒绝 HeapBlock 归还");
}

#[test]
fn cross_owner_return_lands_on_manager_ledger() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 11, 2, 64);
    let (address, block) = {
        let address = world
            .allocate_managed(0, 1, 8, ManagedPlacement::Old)
            .expect("leaf 可分配");
        let block = world
            .managed_block_ref(0, address)
            .expect("block 身份可解析")
            .id;
        (address, block)
    };
    drive_until_pending(&mut world, block);
    assert!(world.managed_object(address).is_err());
    drain(&mut world, 0);
    assert_eq!(world.managed_accounting(0).pending_return_bytes(), 0);
    world.managed_ledger_invariant(0).expect("源侧账本守恒");
}

#[test]
fn forwarding_token_forwards_managed_return() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 13, 2, 64);
    let (_address, block) = {
        let address = world
            .allocate_managed(0, 1, 8, ManagedPlacement::Old)
            .expect("leaf 可分配");
        let block = world
            .managed_block_ref(0, address)
            .expect("block 身份可解析")
            .id;
        (address, block)
    };
    drive_until_pending(&mut world, block);
    let target = world.token(1);
    world.begin_forwarding(0, target).expect("可发布转发");
    drain(&mut world, 0);
    drain(&mut world, 1);
    world.managed_ledger_invariant(1).expect("目标侧账本守恒");
}

#[test]
fn queue_page_grace_defers_decommit() {
    let mut world = heap_world();
    let (_address, block) = old_leaf(&mut world);
    let committed_before = world.provider_stats().committed_bytes;
    drive_until_pending(&mut world, block);
    drain(&mut world, 0);
    assert_eq!(
        world.provider_stats().committed_bytes,
        committed_before,
        "grace 未走完时不得 decommit"
    );
    advance_grace(&mut world, 0);
    assert_eq!(
        world.provider_stats().committed_bytes,
        committed_before - u64::from(GC_BLOCK_BYTES)
    );
}

#[test]
fn shared_empty_block_returns_after_shared_plane() {
    let contract = gc_contract();
    let mut world = configured_world(&contract, 17, 2, 64);
    let handle = world.allocate_shared_object(1, 64).expect("共享对象可分配");
    let _ = handle;
    let committed_before = world.provider_stats().committed_bytes;
    let report = world.run_gc_cycle(true).expect("cycle 可执行");
    assert!(report.cycle_completed);
    assert!(
        world.provider_stats().committed_bytes <= committed_before,
        "共享空块归还后 committed 不得上升"
    );
}

#[test]
fn checksum_and_repeat_consume_are_rejected() {
    let mut world = heap_world();
    let (_address, block) = old_leaf(&mut world);
    drive_until_pending(&mut world, block);
    drain(&mut world, 0);
    let snapshot = world.managed_accounting(0).clone();
    let err: Result<(), RawInvariant> = world.queue_heap_block_return(block);
    assert!(err.is_err(), "重复发布必须拒绝");
    assert_eq!(world.managed_accounting(0), &snapshot);
}

#[test]
fn cli_dump_contains_block_return_schema() {
    let source =
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }";
    let compilation = crate::Compiler::new().compile(crate::CompileRequest::single_file(
        "main.gg",
        source,
        crate::TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let plan = compilation.image_plan().expect("镜像计划");
    let dump = compilation.raw_contract().expect("契约").dump();
    assert!(dump.contains("block-return schema=1"));
    assert_eq!(plan.block_return_runtime().unit_count(), 4);
    assert_eq!(plan.block_return_runtime().gate_count(), 8);
    assert_eq!(plan.block_return_runtime().grace_steps(), 4);
}

#[test]
fn large_mapping_returns_as_one_unit() {
    let mut world = heap_world();
    let big = u64::from(GC_BLOCK_BYTES) + 64;
    let address = world
        .allocate_managed(0, 0, big, ManagedPlacement::Old)
        .expect("大对象可分配");
    let start = world
        .managed_block_ref(0, address)
        .expect("block 身份可解析")
        .id;
    drive_until_pending(&mut world, start);
    drain(&mut world, 0);
    let after = world
        .heap(0)
        .expect("堆可读")
        .block_record(start)
        .expect("记录可读");
    assert_eq!(
        after.state,
        HeapBlockState::Free.raw(),
        "LargeMapping consume 后起始块必须进入 Free"
    );
    world.managed_ledger_invariant(0).expect("账本守恒");
}

#[test]
fn resource_arena_never_queues_heap_arena() {
    let mut world = heap_world();
    let address = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Resource)
        .expect("resource 对象可分配");
    let block = world
        .managed_block_ref(0, address)
        .expect("block 身份可解析")
        .id;
    drive_until_pending(&mut world, block);
    drain(&mut world, 0);
    let after = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(
        after.state,
        HeapBlockState::Free.raw(),
        "resource 块可以按 HeapBlock 归还"
    );
    world.managed_ledger_invariant(0).expect("账本守恒");
}

#[test]
fn line_run_queues_on_partial_sweep() {
    let mut world = heap_world();
    let (live, block) = old_leaf(&mut world);
    let extra = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Old)
        .expect("第二对象可分配");
    let extra_block = world
        .managed_block_ref(0, extra)
        .expect("block 身份可解析")
        .id;
    if extra_block != block {
        return;
    }
    world
        .heap_mut(0)
        .expect("堆可写")
        .mark_object(live)
        .expect("存活对象可标记");
    world.note_candidate_dirty(block).expect("候选通知成功");
    let _ = world.drive_candidates(4096).expect("候选推进成功");
    drain(&mut world, 0);
    let record = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(
        record.state,
        HeapBlockState::Allocating.raw(),
        "半空块必须退回 Allocating"
    );
    assert!(world.managed_object(live).is_ok(), "标记对象必须存活");
    world.managed_ledger_invariant(0).expect("账本守恒");
}
