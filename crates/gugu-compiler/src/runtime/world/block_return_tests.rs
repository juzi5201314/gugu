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
use crate::runtime::local_heap_schema::{HEAP_BLOCK_RETURN_QUEUED, HeapBlockState};
use crate::runtime::model::GRACE_STEPS;
use crate::runtime::slab::RawInvariant;

fn old_leaf(world: &mut RawWorld) -> (u64, ManagedBlockId) {
    let address = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Old, false)
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
        .allocate_managed(0, 1, 8, ManagedPlacement::Nursery, false)
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
            .allocate_managed(0, 1, 8, ManagedPlacement::Old, false)
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
            .allocate_managed(0, 1, 8, ManagedPlacement::Old, false)
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
    assert!(
        dump.contains(
            "block-return-gates \
             incoming-lease,allocator-lease,scanner-lease,evacuation-lease,pin,resource,\
             live-lines,candidate-job"
        ),
        "gate 目录必须与 consume_heap_block 实际核对的八项一致"
    );
}

#[test]
fn large_mapping_returns_as_one_unit() {
    let mut world = heap_world();
    let big = u64::from(GC_BLOCK_BYTES) + 64;
    let address = world
        .allocate_managed(0, 0, big, ManagedPlacement::Old, false)
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
        .allocate_managed(0, 1, 8, ManagedPlacement::Resource, false)
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
        .allocate_managed(0, 1, 8, ManagedPlacement::Old, false)
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

/// 分配直到出现两个不同的 block；返回它们，块内对象全部未标记。
fn fill_two_blocks(world: &mut RawWorld) -> (ManagedBlockId, ManagedBlockId) {
    let first = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Old, false)
        .expect("leaf 可分配");
    let first_block = world
        .managed_block_ref(0, first)
        .expect("block 身份可解析")
        .id;
    loop {
        let next = world
            .allocate_managed(0, 1, 8, ManagedPlacement::Old, false)
            .expect("leaf 可分配");
        let block = world
            .managed_block_ref(0, next)
            .expect("block 身份可解析")
            .id;
        if block != first_block {
            return (first_block, block);
        }
    }
}

/// 释放一个 block 只能清掉它自己的元数据：同一 arena 里更早的 block 必须保持完好。
///
/// 走真实归还路径（候选 → sweep → owner inbox → consume），而不是伪造 `OwnedFree`：
/// `release_block` 曾经用 line 下标除以 granule 字节数来算 block 的起始 granule，于是释放
/// block 1 会清掉 block 0 的 object-start 位，让存活对象变成不可解析。
#[test]
fn releasing_a_block_preserves_earlier_block_object_start_bits() {
    let mut world = heap_world();
    let first = world
        .allocate_managed(0, 1, 8, ManagedPlacement::Old, false)
        .expect("leaf 可分配");
    let first_block = world
        .managed_block_ref(0, first)
        .expect("block 身份可解析")
        .id;
    // 继续分配直到进入下一个 block；`survivor` 始终是留在早块高地址区间的对象，它的 granule
    // 正是错误换算会误清的位置。
    let mut survivor = first;
    let (second, second_block) = loop {
        let next = world
            .allocate_managed(0, 1, 8, ManagedPlacement::Old, false)
            .expect("leaf 可分配");
        let block = world
            .managed_block_ref(0, next)
            .expect("block 身份可解析")
            .id;
        if block != first_block {
            break (next, block);
        }
        survivor = next;
    };
    assert_ne!(second_block, first_block, "分配必须真的换到下一个 block");
    world
        .heap_mut(0)
        .expect("堆可写")
        .mark_object(first)
        .expect("对象可标记");
    world
        .heap_mut(0)
        .expect("堆可写")
        .mark_object(survivor)
        .expect("对象可标记");
    world
        .note_candidate_dirty(second_block)
        .expect("候选通知成功");
    drive_until_pending(&mut world, second_block);
    drain(&mut world, 0);
    let after = world
        .heap(0)
        .expect("堆可读")
        .block_record(second_block)
        .expect("记录可读");
    assert_eq!(after.state, HeapBlockState::Free.raw(), "晚块必须完整归还");
    assert!(
        world.managed_object(first).is_ok(),
        "释放其它 block 不得抹掉更早 block 的 object-start 位"
    );
    assert!(
        world.managed_object(survivor).is_ok(),
        "更早 block 的高地址对象同样必须保持可解析"
    );
    assert!(
        world.managed_object(second).is_err(),
        "被释放 block 的对象必须不再可解析"
    );
    world.managed_ledger_invariant(0).expect("账本守恒");
}

#[test]
fn returned_block_pages_are_recommitted_on_reuse() {
    let mut world = heap_world();
    let (_address, block) = old_leaf(&mut world);
    let committed_before = world.provider_stats().committed_bytes;
    drive_until_pending(&mut world, block);
    drain(&mut world, 0);
    advance_grace(&mut world, 0);
    assert_eq!(
        world.provider_stats().committed_bytes,
        committed_before - u64::from(GC_BLOCK_BYTES),
        "归还的物理页必须真的 decommit"
    );
    // 复用刚归还的块：分配必须重新提交 32 KiB，否则 committed 会小于 live。
    let (address, reused) = old_leaf(&mut world);
    assert_eq!(reused, block, "新分配必须命中刚归还的块");
    assert!(world.managed_object(address).is_ok());
    world.managed_ledger_invariant(0).expect("复用后账本守恒");
    assert!(
        world.managed_accounting(0).committed_bytes() >= u64::from(GC_BLOCK_BYTES),
        "复用块必须重新计入 committed"
    );
}

#[test]
fn return_pending_block_is_not_allocatable() {
    let mut world = heap_world();
    let (_address, block) = old_leaf(&mut world);
    // 只发布归还、先不 drain：块处于 ReturnPending，必须被分配扫描排除。
    drive_until_pending(&mut world, block);
    let (address, next) = old_leaf(&mut world);
    assert_ne!(next, block, "待归还的块不得再次成为分配目标");
    assert!(world.managed_object(address).is_ok());
    let pending = world
        .heap(0)
        .expect("堆可读")
        .block_record(block)
        .expect("记录可读");
    assert_eq!(
        pending.state,
        HeapBlockState::ReturnPending.raw(),
        "发布之后块必须仍保持 ReturnPending"
    );
    drain(&mut world, 0);
    advance_grace(&mut world, 0);
    world.managed_ledger_invariant(0).expect("账本守恒");
}

#[test]
fn large_span_members_stay_out_of_candidates() {
    let mut world = heap_world();
    let big = u64::from(GC_BLOCK_BYTES) + 64;
    let address = world
        .allocate_managed(0, 1, big, ManagedPlacement::Old, false)
        .expect("大对象可分配");
    let start = world
        .managed_block_ref(0, address)
        .expect("block 身份可解析")
        .id;
    let member = ManagedBlockId::new(start.arena(), start.index() + 1).expect("成员身份");
    let heap = world.heap(0).expect("堆可读");
    assert_eq!(heap.block_live_lines(start).expect("line 计数"), 256);
    assert_eq!(heap.block_live_lines(member).expect("line 计数"), 256);
    world
        .heap_mut(0)
        .expect("堆可写")
        .mark_object(address)
        .expect("大对象可标记");
    world
        .note_candidate_dirty(member)
        .expect("成员 dirty 通知必须折回起始块");
    let _ = world.drive_candidates(4096).expect("候选推进成功");
    assert!(
        world.managed_object(address).is_ok(),
        "存活大对象不得被成员块的试验删除判死"
    );
    let second = world
        .allocate_managed(0, 1, big, ManagedPlacement::Old, false)
        .expect("第二个大对象可分配");
    let second_start = world
        .managed_block_ref(0, second)
        .expect("block 身份可解析")
        .id;
    assert_ne!(second_start, start, "仍在使用的 span 不得被复用");
    world.managed_ledger_invariant(0).expect("账本守恒");
}

#[test]
fn failed_publish_leaves_no_pending_bytes() {
    let contract = gc_contract();
    // node 容量 1：第二条归还消息无法发布；发布失败必须回滚刚记入的 pending 字节。
    let mut world = configured_world(&contract, 23, 1, 1);
    let (first, second) = fill_two_blocks(&mut world);
    world.note_candidate_dirty(first).expect("候选通知成功");
    world.note_candidate_dirty(second).expect("候选通知成功");
    // 候选平面按相位推进：第一条归还先占用唯一的 node，第二条的发布必须失败。
    let mut failed = false;
    for _ in 0..64 {
        if world.drive_candidates(4096).is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed, "node 用尽后的发布必须失败");
    let mut queued = 0_u64;
    for id in [first, second] {
        let record = world
            .heap(0)
            .expect("堆可读")
            .block_record(id)
            .expect("记录可读");
        if record.reserved & HEAP_BLOCK_RETURN_QUEUED != 0 {
            queued += 1;
        }
    }
    assert_eq!(
        world.managed_accounting(0).pending_return_bytes(),
        queued * u64::from(GC_BLOCK_BYTES),
        "失败发布不得留下没有消息对应的 pending 字节"
    );
}

#[test]
fn arena_return_retires_the_arena() {
    let mut world = heap_world();
    let big = u64::from(GC_BLOCK_BYTES) + 64;
    // 每个大对象占 2 个 block，32 个恰好填满一个 64-block arena。
    for _ in 0..32 {
        world
            .allocate_managed(0, 1, big, ManagedPlacement::Old, false)
            .expect("大对象可分配");
    }
    let descriptor = world.managed_arenas()[0].descriptor;
    let mut retired = false;
    for _ in 0..8 {
        let cycle = world.run_gc_cycle(true).expect("cycle 可执行");
        assert!(cycle.cycle_completed, "cycle 必须完成");
        drain(&mut world, 0);
        advance_grace(&mut world, 0);
        if world
            .managed_arenas()
            .iter()
            .all(|arena| arena.descriptor != descriptor)
        {
            retired = true;
            break;
        }
    }
    assert!(retired, "整区归还后 arena 必须从登记表摘除");
    world.managed_ledger_invariant(0).expect("摘除后账本守恒");
    // 后续分配必须走新登记的新 arena，且不会命中没有页的旧块。
    let address = world
        .allocate_managed(0, 1, big, ManagedPlacement::Old, false)
        .expect("新 arena 可分配");
    assert!(world.managed_object(address).is_ok());
    world
        .managed_ledger_invariant(0)
        .expect("新 arena 账本守恒");
}
