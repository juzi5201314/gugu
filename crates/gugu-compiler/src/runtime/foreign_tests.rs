use super::coroutine::CoroutineState;
use super::cstring::{scan, terminate};
use super::foreign::{
    AdmitOutcome, BridgeMode, ForeignWorld, IoClass, ManagedRoot, PollerFamily, ReturnPath,
};
use super::foreign_schema::MAX_BLOCKING_WORKERS;
use crate::{CompileRequest, Compiler, TargetName};

fn root(pinned: bool) -> ManagedRoot {
    ManagedRoot { handle: 1, pinned }
}

fn world() -> ForeignWorld {
    ForeignWorld::new(2).expect("并行度")
}

#[test]
fn ordinary_bridge_keeps_lease_until_pressure_grace_and_captures_error() {
    let mut world = world();
    let task = world.spawn_running().expect("协程");
    let admitted = world
        .admit(task, BridgeMode::Ordinary, &[root(true)], 8, 16, 32)
        .expect("admission");
    assert_eq!(admitted, AdmitOutcome::Attached { credit: 1 });
    assert!(!world.detached(task).expect("lease"));
    world.record_safepoint(task).expect("bridge root");
    assert_eq!(world.safepoints(task), 1);
    world.arm_pressure(task, 0).expect("登记压力");
    assert!(!world.retake_pressure(task, 19).expect("宽限内"));
    assert!(world.retake_pressure(task, 20).expect("宽限后"));
    assert!(world.detached(task).expect("retake"));
    world.begin_callback(task).expect("普通 bridge 可以回调");
    world.finish_callback(true).expect("panic 边界");
    assert!(world.panic_abort());
    assert_eq!(
        world.lifecycle(task).expect("状态"),
        CoroutineState::Foreign
    );
    let path = world.complete(task, 22).expect("返回");
    assert_eq!(path, ReturnPath::IdleProcessor);
    assert_eq!(world.captured_error(), 22);
}

#[test]
fn missing_credit_waits_without_running_native_and_reuses_one_credit() {
    let mut world = ForeignWorld::new(u64::from(MAX_BLOCKING_WORKERS) + 1).expect("并行度");
    let mut tasks = Vec::new();
    for _ in 0..MAX_BLOCKING_WORKERS {
        let task = world.spawn_running().expect("协程");
        let admitted = world
            .admit(task, BridgeMode::Ordinary, &[], 1, 0, 8)
            .expect("额度");
        assert!(matches!(admitted, AdmitOutcome::Attached { .. }));
        tasks.push(task);
    }
    let waiter = world.spawn_running().expect("等待者");
    assert_eq!(
        world
            .admit(waiter, BridgeMode::Ordinary, &[], 1, 0, 8)
            .expect("排队"),
        AdmitOutcome::Waiting
    );
    assert_eq!(
        world.lifecycle(waiter).expect("状态"),
        CoroutineState::Waiting
    );
    assert_eq!(world.active_blocking(), MAX_BLOCKING_WORKERS);
    world.complete(tasks[0], 0).expect("归还额度");
    assert_eq!(
        world.lifecycle(waiter).expect("取得额度后"),
        CoroutineState::Foreign
    );
    assert!(world.detached(waiter).expect("blocking worker"));
    let again = world.complete(tasks[0], 0);
    assert!(again.is_err(), "额度不能归还两次");
}

#[test]
fn dirty_credit_releases_processor_and_rejects_forged_stack_map() {
    let mut world = ForeignWorld::new(1).expect("并行度");
    let first = world.spawn_running().expect("协程");
    assert_eq!(
        world
            .admit(
                first,
                BridgeMode::Dirty { opaque: true },
                &[root(true)],
                4,
                8,
                8
            )
            .expect("dirty"),
        AdmitOutcome::DirtyActive
    );
    assert!(world.detached(first).expect("立即放开"));
    assert!(world.record_safepoint(first).is_err());
    let second = world.spawn_running().expect("排队");
    assert_eq!(
        world
            .admit(second, BridgeMode::Dirty { opaque: true }, &[], 4, 0, 8)
            .expect("等待额度"),
        AdmitOutcome::DirtyWaiting
    );
    world.set_parallelism(4).expect("提高目标");
    assert_eq!(
        world.lifecycle(second).expect("FIFO"),
        CoroutineState::Foreign
    );
    world.set_parallelism(1).expect("降低目标");
    assert_eq!(world.dirty_active(), 2, "已运行的 dirty work 不强杀");
}

#[test]
fn leaf_stays_running_and_callback_from_leaf_or_dirty_is_rejected() {
    let mut world = world();
    let task = world.spawn_running().expect("协程");
    assert_eq!(
        world
            .admit(task, BridgeMode::Leaf, &[root(true)], 0, 0, 0)
            .expect("leaf"),
        AdmitOutcome::Leaf
    );
    assert_eq!(
        world.lifecycle(task).expect("状态"),
        CoroutineState::Running
    );
    world.capture_leaf_error(7);
    assert_eq!(world.leaf_error(), 7);
    assert!(world.begin_callback(task).is_err());
    let dirty = world.spawn_running().expect("dirty");
    world
        .admit(dirty, BridgeMode::Dirty { opaque: true }, &[], 1, 0, 8)
        .expect("dirty");
    assert!(world.begin_callback(dirty).is_err());
}

#[test]
fn unpinned_root_is_rejected_before_native() {
    let mut world = world();
    let task = world.spawn_running().expect("协程");
    assert!(
        world
            .admit(task, BridgeMode::Ordinary, &[root(false)], 1, 0, 8)
            .is_err()
    );
    assert_eq!(
        world.lifecycle(task).expect("状态"),
        CoroutineState::Running
    );
}

#[test]
fn external_thread_cannot_touch_coroutine_or_gc_metadata() {
    let mut world = world();
    let unattached = world.attach_external();
    assert!(world.operate_coroutine(unattached).is_err());
    let thread = world.attach_external();
    assert!(world.operate_gc(thread).is_err());
    assert!(world.operate_coroutine(thread).is_err());
}

#[test]
fn poller_accepts_sockets_and_sends_files_and_bridges_to_blocking() {
    let mut world = world();
    for family in [PollerFamily::Linux, PollerFamily::Windows] {
        assert_ne!(family.opcode(), "");
        assert!(world.submit(family, IoClass::RegularFile, 1).is_err());
        assert!(world.submit(family, IoClass::ForeignCall, 2).is_err());
        world.submit(family, IoClass::Socket, 3).expect("socket");
        world.submit(family, IoClass::Socket, 4).expect("socket");
        world.complete_io(family, 3).expect("完成");
        assert_eq!(world.ready(family), &[3]);
    }
    assert_eq!(PollerFamily::Linux.opcode(), "epoll");
    assert_eq!(PollerFamily::Windows.opcode(), "iocp");
}

#[test]
fn c_string_rejects_interior_nul_and_scans_one_terminator() {
    assert!(terminate(b"a\0b").is_err());
    assert_eq!(terminate(b"ab").expect("终止"), b"ab\0");
    assert!(scan(b"ab").is_err());
    assert_eq!(scan(b"ab\0cd").expect("窗口"), b"ab");
    assert!(scan(b"").is_err());
}

#[test]
fn compiled_foreign_sites_enter_the_runtime_contract() {
    let source = "extern \"C\" fn write(n: int) int\n#[ffi(leaf(stack = 16))] extern \"C\" fn leaf(n: int) int\n#[ffi(dirty_cpu)] extern \"C\" fn dirty(n: int) int\nfn main() { _ = write(1)\n _ = leaf(1)\n _ = dirty(1) }";
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.foreign_ordinary_sites(), 1);
    assert_eq!(plan.foreign_dirty_sites(), 1);
    assert_eq!(plan.foreign_leaf_sites(), 1);
    assert_eq!(plan.foreign_max_blocking_workers(), MAX_BLOCKING_WORKERS);
    let again = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    let again = again.image_plan().expect("重放");
    assert_eq!(
        again.foreign_contract_fingerprint(),
        plan.foreign_contract_fingerprint()
    );
}
