//! 协程控制块、stack arena与完成交接的确定性回归；不调用真实线程或平台VM。

use super::RawWorld;
use super::coroutine_impl::CoroutineEntry;
use crate::runtime::coroutine::{
    COLD_COMPACTED, CompletionValue, CoroutineContext, CoroutineState, CoroutineTable,
    POLL_SENTINEL, STACK_SCAN_LOCKED,
};
use crate::runtime::message::BatchLimits;
use crate::runtime::model::RawPlanePolicyV1;
use crate::runtime::platform::{FakePlatform, PlatformProfile};
use crate::runtime::provider::{RangeProvider, RangeState};
use crate::runtime::stack::{
    STACK_ARENA_BYTES, STACK_SPAN_BYTES, StackError, StackImage, growth_capacity, initial_capacity,
};
use crate::runtime::stack_arena::StackAllocator;
use crate::runtime::startup_schema::{FatalKind, LifecycleStateName};
use std::sync::atomic::Ordering;

fn entry(required_frame: usize) -> CoroutineEntry {
    CoroutineEntry {
        pc: 0x1000,
        required_frame,
    }
}

fn world(limit: &str) -> RawWorld {
    let mut world = RawWorld::new(7, 2, 64, BatchLimits::default()).expect("world");
    world
        .boot(
            vec![],
            vec![("GUGU_RUNTIME_STACK_MAX".to_owned(), limit.to_owned())],
            "/".to_owned(),
            2,
            entry(64),
        )
        .expect("boot");
    world
}

fn child(world: &mut RawWorld, required: usize) -> crate::runtime::coroutine::CoroutineHandle {
    let child = world
        .spawn_user_coroutine(0, entry(required))
        .expect("创建")
        .expect("接纳");
    world.enter_coroutine(child).expect("切入");
    child
}

fn park(
    world: &mut RawWorld,
    child: crate::runtime::coroutine::CoroutineHandle,
    bytes: Vec<u8>,
    roots: Vec<u32>,
) {
    let (slot, cold) = world.controls.get(child).expect("control");
    let context = CoroutineContext {
        rsp: slot.stack.stack_high - bytes.len(),
        ..cold.context
    };
    world
        .save_coroutine(
            child,
            context,
            StackImage {
                bytes,
                stack_roots: roots,
                stack_registers: 0,
            },
            true,
        )
        .expect("park");
}

#[test]
fn class_rounding_obeys_the_logical_limit_and_checked_arithmetic() {
    assert_eq!(initial_capacity(0, 65536), Ok(2048));
    assert_eq!(initial_capacity(1537, 65536), Ok(4096));
    assert_eq!(initial_capacity(2048, 3000), Err(StackError::Overflow));
    assert_eq!(growth_capacity(512, 128, 64, 65536), Ok(2048));
    assert_eq!(
        growth_capacity(32768, 1, 1, 65535),
        Err(StackError::Overflow)
    );
    assert_eq!(
        growth_capacity(2048, usize::MAX, 1, usize::MAX),
        Err(StackError::Overflow)
    );
    assert_eq!(StackError::Overflow.fatal(), FatalKind::StackOverflow);
}

#[test]
fn arena_guards_and_shared_subpage_commit_do_not_scale_with_slots() {
    for profile in [PlatformProfile::Linux, PlatformProfile::Windows] {
        let mut provider = FakePlatform::new(profile, 1 << 30);
        let mut stacks = StackAllocator::default();
        let a = stacks.reserve(0, 512, &mut provider).expect("slot a");
        let b = stacks.reserve(0, 512, &mut provider).expect("slot b");
        assert_eq!(provider.stats().committed_bytes, 0);
        assert_eq!(provider.describe_all().len(), 1);
        let range = provider.describe_all()[0];
        assert_eq!((range.guard_low_bytes, range.guard_bytes), (4096, 4096));
        assert!(!range.contains(range.base, 1, 1));
        assert!(!range.contains(range.end() - 1, 1, 1));
        let (low_a, _) = stacks.bounds(a).expect("bounds a");
        let (low_b, _) = stacks.bounds(b).expect("bounds b");
        assert_eq!(low_b - low_a, 512);
        stacks.commit(a, &mut provider).expect("commit a");
        stacks.commit(b, &mut provider).expect("commit b");
        assert_eq!(stacks.stats().committed_bytes, 4096);
        stacks.release_global(a, &mut provider).expect("归还a");
        assert_eq!(stacks.stats().committed_bytes, 4096, "b保活同一个宿主页");
        stacks.recycle(b, 0, &mut provider).expect("cache b");
        assert_eq!(stacks.stats().committed_bytes, 4096, "cache仍保活宿主页");
        stacks.trim_cache(0, 0, &mut provider).expect("flush cache");
        assert_eq!(provider.stats().committed_bytes, 0);
        assert_eq!(
            stacks.stats().reserved_bytes,
            u64::try_from(STACK_ARENA_BYTES + 8192).unwrap()
        );
    }
}

#[test]
fn cache_hysteresis_invalidates_old_handles_without_extra_mapping() {
    let mut provider = FakePlatform::new(PlatformProfile::Linux, 1 << 30);
    let mut stacks = StackAllocator::default();
    let a = stacks.acquire(0, 32768, &mut provider).unwrap();
    let b = stacks.acquire(0, 32768, &mut provider).unwrap();
    let c = stacks.acquire(0, 32768, &mut provider).unwrap();
    stacks.recycle(a, 0, &mut provider).unwrap();
    assert!(
        stacks.recycle(a, 0, &mut provider).is_err(),
        "旧generation不得二次归还"
    );
    stacks.recycle(b, 0, &mut provider).unwrap();
    assert_eq!(stacks.stats().cache_bytes, 65536);
    stacks.recycle(c, 0, &mut provider).unwrap();
    assert_eq!(stacks.stats().cache_bytes, 32768);
    let reused = stacks.acquire(0, 32768, &mut provider).unwrap();
    assert_ne!(reused, a);
    assert_eq!(stacks.stats().cache_hits, 1);
    assert_eq!(stacks.stats().mappings, 1);
    assert_eq!(stacks.stats().cache_bytes, 0);
}

#[test]
fn empty_span_changes_class_and_buddy_extent_coalesces() {
    let mut provider = FakePlatform::new(PlatformProfile::Linux, 1 << 30);
    let mut stacks = StackAllocator::default();
    let a = stacks.acquire(0, 4096, &mut provider).unwrap();
    let old_low = stacks.bounds(a).unwrap().0;
    stacks.release_global(a, &mut provider).unwrap();
    let b = stacks.reserve(0, 65536, &mut provider).unwrap();
    assert_eq!(stacks.bounds(b).unwrap().0, old_low);
    stacks.release_global(b, &mut provider).unwrap();
    let left = stacks
        .reserve(0, STACK_SPAN_BYTES * 2, &mut provider)
        .unwrap();
    let right = stacks
        .reserve(0, STACK_SPAN_BYTES * 2, &mut provider)
        .unwrap();
    stacks.release_global(left, &mut provider).unwrap();
    stacks.release_global(right, &mut provider).unwrap();
    let joined = stacks
        .reserve(0, STACK_SPAN_BYTES * 4, &mut provider)
        .unwrap();
    assert_eq!(stacks.bounds(joined).unwrap().0, old_low);
    assert_eq!(stacks.stats().mappings, 1);
}

#[test]
fn large_reservation_and_extra_empty_arena_are_released() {
    let mut provider = FakePlatform::new(PlatformProfile::Windows, 2 << 30);
    let mut stacks = StackAllocator::default();
    let half_a = stacks
        .reserve(0, STACK_ARENA_BYTES / 2, &mut provider)
        .unwrap();
    let half_b = stacks
        .reserve(0, STACK_ARENA_BYTES / 2, &mut provider)
        .unwrap();
    let half_c = stacks
        .reserve(0, STACK_ARENA_BYTES / 2, &mut provider)
        .unwrap();
    assert_eq!(stacks.stats().mappings, 2);
    stacks.release_global(half_a, &mut provider).unwrap();
    stacks.release_global(half_b, &mut provider).unwrap();
    stacks.release_global(half_c, &mut provider).unwrap();
    assert_eq!(stacks.stats().mappings, 1);
    let dedicated = stacks.reserve(0, STACK_ARENA_BYTES, &mut provider).unwrap();
    assert_eq!(stacks.stats().mappings, 2);
    stacks.release_global(dedicated, &mut provider).unwrap();
    assert_eq!(provider.count_in(RangeState::Released), 2);
    assert_eq!(provider.stats().committed_bytes, 0, "只分配虚拟容量");
}

#[test]
fn segmented_control_records_keep_addresses_and_reject_stale_handles() {
    let mut table = CoroutineTable::default();
    let first = table.allocate().unwrap();
    let (hot, cold) = table.get(first).unwrap();
    let before = (std::ptr::from_ref(hot), std::ptr::from_ref(cold));
    // 513项跨越hot页与cold页，不触碰用户栈或真实VM。
    for _ in 1..513 {
        table.allocate().unwrap();
    }
    let (hot, cold) = table.get(first).unwrap();
    assert_eq!(before, (std::ptr::from_ref(hot), std::ptr::from_ref(cold)));
    assert_eq!(hot.hot.cold_index, u64::from(first.index));
    assert!(table.release(first).is_err(), "不能释放live控制块");
    let (slot, cold) = table.get_mut(first).unwrap();
    slot.hot
        .state
        .store(CoroutineState::Dead as u64, Ordering::Release);
    cold.join_state.join_leases = 0;
    table.release(first).unwrap();
    let next = table.allocate().unwrap();
    assert_eq!(next.index, first.index);
    assert_ne!(next.generation, first.generation);
    assert!(table.get(first).is_err());
}

#[test]
fn stack_growth_relocates_only_declared_interiors_and_preserves_poison() {
    let mut world = world("64KiB");
    let child = child(&mut world, 64);
    let old_high = world.controls.get(child).unwrap().0.stack.stack_high;
    let mut bytes = vec![0; 32];
    bytes[..8].copy_from_slice(&(old_high - 16).to_le_bytes());
    bytes[8..16].copy_from_slice(&(old_high - 16).to_le_bytes());
    park(&mut world, child, bytes, vec![0]);
    world.controls.get(child).unwrap().0.stack.poison();
    world.grow_coroutine(child, 4096, 1).unwrap();
    let (slot, cold) = world.controls.get(child).unwrap();
    assert_eq!(slot.stack.capacity, 8192);
    assert_eq!(
        slot.stack.stack_check.load(Ordering::Acquire),
        POLL_SENTINEL
    );
    assert_eq!(cold.context.rsp, slot.stack.stack_high - 32);
    let image = &world.coroutine_storage[usize::try_from(child.index).unwrap()]
        .as_ref()
        .unwrap()
        .image;
    assert_eq!(
        usize::from_le_bytes(image.bytes[..8].try_into().unwrap()),
        slot.stack.stack_high - 16
    );
    assert_eq!(
        usize::from_le_bytes(image.bytes[8..16].try_into().unwrap()),
        old_high - 16,
        "NonRoot数值不重定位"
    );
    assert!(
        !slot.stack.allows_frame(cold.context.rsp, 16),
        "poison共用入口taken边"
    );
    slot.stack.clear_poll();
    assert!(slot.stack.allows_frame(cold.context.rsp, 16));
    assert!(!slot.stack.allows_frame(8, 64), "下溢按有符号candidate拒绝");
}

#[test]
fn invalid_relocation_keeps_old_stack_and_enters_fatal() {
    let mut world = world("64KiB");
    let child = child(&mut world, 64);
    let before = world.controls.get(child).unwrap().0.stack.stack_low;
    park(&mut world, child, 1_usize.to_le_bytes().to_vec(), vec![0]);
    assert!(world.grow_coroutine(child, 4096, 1).is_err());
    assert_eq!(world.controls.get(child).unwrap().0.stack.stack_low, before);
    assert_eq!(world.rt0_state().unwrap(), LifecycleStateName::Terminating);
    assert_eq!(world.rt0_plan().unwrap().unwrap().exit_code(), 2);
    assert_eq!(
        world.stacks.stats().live_bytes,
        4096,
        "新stack已撤销，main与旧stack仍在"
    );
}

#[test]
fn waiting_stack_requires_four_distinct_gc_windows_before_cold_compaction() {
    let mut world = world("64KiB");
    let child = child(&mut world, 4096);
    park(&mut world, child, vec![0; 128], vec![]);
    for epoch in 1..4 {
        assert!(!world.shrink_coroutine(child, epoch, false).unwrap());
        assert!(
            !world.shrink_coroutine(child, epoch, false).unwrap(),
            "同一GC不得重复计窗"
        );
    }
    assert!(world.shrink_coroutine(child, 4, false).unwrap());
    let (slot, _) = world.controls.get(child).unwrap();
    assert_eq!(slot.stack.capacity, 512);
    assert_eq!(slot.stack.flags & COLD_COMPACTED, COLD_COMPACTED);
    world.grow_coroutine(child, 256, 5).unwrap();
    let (slot, _) = world.controls.get(child).unwrap();
    assert_eq!(slot.stack.capacity, 2048);
    assert_eq!(slot.stack.flags & COLD_COMPACTED, 0);
    assert!(
        !world.shrink_coroutine(child, 5, true).unwrap(),
        "本周期增长不能被pressure立即反转"
    );
}

#[test]
fn logical_overflow_and_platform_exhaustion_are_distinct_fatal_paths() {
    let mut world = world("64KiB");
    let before = world.stack_stats();
    assert!(world.spawn_user_coroutine(0, entry(65536)).is_err());
    assert_eq!(world.stack_stats(), before, "超过逻辑上限不发起平台分配");
    assert!(
        world.rt0_reports().unwrap()[0]
            .text()
            .contains("stack-overflow")
    );
    let mut provider = FakePlatform::new(PlatformProfile::Linux, 4096);
    let error = StackAllocator::default()
        .acquire(0, 2048, &mut provider)
        .unwrap_err();
    assert_eq!(error.fatal(), FatalKind::OutOfMemory);
    assert_eq!(provider.stats().committed_bytes, 0);
}

#[test]
fn completion_precedes_stack_detach_and_scanner_handoff() {
    let mut world = world("64KiB");
    let child = child(&mut world, 64);
    world.clone_join(child).unwrap();
    let value = CompletionValue::Managed {
        handle: 7,
        descriptor: 9,
    };
    let ticket = world.stage_coroutine_finish(child, value).unwrap();
    assert_eq!(world.completion_barriers, vec![(child, 7, 9)]);
    let (slot, cold) = world.controls.get(child).unwrap();
    assert_eq!(cold.join_state.read().unwrap(), value);
    assert_eq!(slot.stack.capacity, 2048);
    slot.hot.state.fetch_or(STACK_SCAN_LOCKED, Ordering::AcqRel);
    assert!(world.finish_coroutine_on_system(&ticket, 0).is_err());
    let (slot, _) = world.controls.get(child).unwrap();
    assert_eq!(slot.stack.capacity, 2048, "scanner拥有root时不能摘根");
    slot.hot
        .state
        .fetch_and(!STACK_SCAN_LOCKED, Ordering::Release);
    world.finish_coroutine_on_system(&ticket, 0).unwrap();
    let (slot, cold) = world.controls.get(child).unwrap();
    assert_eq!(
        (slot.stack.capacity, cold.context.rsp, cold.context.rip),
        (0, 0, 0)
    );

    assert_eq!(slot.hot.lifecycle().unwrap(), CoroutineState::Dead);
    assert_eq!(world.read_completion(child).unwrap(), value);
    assert!(
        world.finish_coroutine_on_system(&ticket, 0).is_err(),
        "stack只归还一次"
    );
    world.release_join(child).unwrap();
    assert_eq!(world.read_completion(child).unwrap(), value);
    world.release_join(child).unwrap();
    assert!(world.controls.get(child).is_err());
}

#[test]
fn last_join_can_disappear_before_finish_without_leaking_control() {
    let mut world = world("64KiB");
    let child = child(&mut world, 64);
    world.release_join(child).unwrap();
    assert!(
        world.controls.get(child).is_ok(),
        "运行中的协程仍由scheduler保活"
    );
    assert!(world.clone_join(child).is_err(), "已归还的Join不能重新复制");
    world
        .coroutine_finished(child, 0, CompletionValue::Bits(42))
        .unwrap();
    assert!(
        world.controls.get(child).is_err(),
        "完成且无Join后回收控制块"
    );
    assert_eq!(world.stack_stats().live_bytes, 2048);
    let next = world.spawn_user_coroutine(0, entry(64)).unwrap().unwrap();
    assert_eq!(next.index, child.index);
    assert_ne!(next.generation, child.generation);
}

#[test]
fn remote_finish_uses_existing_owner_inbox_and_does_not_retain_stack_for_join() {
    let mut world = world("64KiB");
    let child = child(&mut world, 64);
    world
        .coroutine_finished(
            child,
            1,
            CompletionValue::Panic {
                handle: 7,
                descriptor: 9,
            },
        )
        .unwrap();
    assert_eq!(world.stack_stats().pending_bytes, 2048);
    assert_eq!(world.stack_stats().live_bytes, 2048, "只剩main栈");
    let first = world.read_completion(child).unwrap();
    let second = world.read_completion(child).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        world
            .controls
            .get(child)
            .unwrap()
            .1
            .join_state
            .status
            .load(Ordering::Acquire),
        7
    );
    world
        .drain_all(0, &RawPlanePolicyV1::default().service_budget())
        .unwrap();
    assert_eq!(world.stack_stats().pending_bytes, 0);
    assert_eq!(world.stack_stats().cache_bytes, 2048);
    assert_eq!(world.read_completion(child).unwrap(), first);
}

#[test]
fn retired_owner_forwards_pending_stack_return_once() {
    let mut world = world("64KiB");
    let child = child(&mut world, 64);
    world
        .coroutine_finished(child, 1, CompletionValue::Bits(9))
        .unwrap();
    let target = world.token(1);
    world
        .retire(0, target, &RawPlanePolicyV1::default().service_budget())
        .unwrap();
    world
        .drain_all(1, &RawPlanePolicyV1::default().service_budget())
        .unwrap();
    assert_eq!(world.stack_stats().pending_bytes, 0);
    assert_eq!(
        world.read_completion(child).unwrap(),
        CompletionValue::Bits(9)
    );
}

#[test]
fn layout_and_context_corruption_are_rejected_before_backend_consumption() {
    use crate::runtime::{CoroutineDemand, CoroutineRuntimeContract};
    let contract = CoroutineRuntimeContract::build(CoroutineDemand::default()).unwrap();
    let mut bad = contract.clone();
    bad.records[2].fields[1].offset = 32;
    assert!(bad.verify().is_err());
    let mut bad = contract.clone();
    bad.context.restore_offset += 1;
    assert!(bad.verify().is_err());
    let mut bad = contract.clone();
    bad.context.bytes[0] ^= 1;
    assert!(bad.verify().is_err());
    let mut changed = contract.clone();
    changed.demand.creation_sites = 1;
    assert_ne!(changed.fingerprint(), contract.fingerprint());
}

#[test]
fn source_coroutines_feed_validated_layouts_context_and_query_identity() {
    use crate::{CompileRequest, Compiler, TargetName};
    let compiler = Compiler::new();
    let source = include_str!("fixtures/coroutines.gg");
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let compile = || compiler.compile(CompileRequest::single_file("main.gg", source, target));
        let cold = compile();
        let warm = compile();
        assert!(cold.is_success(), "{:?}", cold.diagnostics().items());
        let contract = cold.image_plan().unwrap().coroutine_runtime();
        assert_eq!(contract.demand.creation_sites, 2);
        assert!(contract.demand.checked_entries > 0);
        assert_eq!(cold.action_key(), warm.action_key());
        assert_eq!(contract, warm.image_plan().unwrap().coroutine_runtime());
    }
}
