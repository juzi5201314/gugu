//! rt0 布局、启动输入与 raw 世界终止边界的确定性校验。
//! 用户 `main` 的运行、panic 落地和栈增长由真实镜像烟测执行。

use super::coroutine::{CompletionValue, CoroutineHandle};
use super::inbox::ServiceBudget;
use super::message::{BatchLimits, FlushTrigger, ProducerStaging, ReturnKind, stage_message};
use super::model::RawPlanePolicyV1;
use super::size_class::RuntimeSizeClassId;
use super::startup_schema::{
    ExitCategory, FatalKind, LifecycleStateName, Rt0Step, ShutdownFacility,
};
use super::world::RawWorld;
use super::world::coroutine_impl::CoroutineEntry;

fn entry() -> CoroutineEntry {
    CoroutineEntry {
        pc: 0x1000,
        required_frame: 64,
    }
}

fn spawn(world: &mut RawWorld) -> CoroutineHandle {
    let handle = world
        .spawn_user_coroutine(0, entry())
        .expect("接纳")
        .expect("返回协程handle");
    world.enter_coroutine(handle).expect("首次切入");
    handle
}

fn finish(world: &mut RawWorld, handle: CoroutineHandle) {
    world
        .coroutine_finished(handle, 0, CompletionValue::Bits(42))
        .expect("协程结束");
}

fn budget() -> ServiceBudget {
    RawPlanePolicyV1::default().service_budget()
}

fn world() -> RawWorld {
    RawWorld::new(7, 2, 64, BatchLimits::default()).expect("raw world 可创建")
}

fn boot(world: &mut RawWorld) {
    let report = world
        .boot(
            vec!["gugu".to_owned()],
            vec![],
            "/work".to_owned(),
            4,
            entry(),
        )
        .expect("boot 不产生模型不变量");
    assert!(report.started, "默认配置必须合法");
    assert!(report.errors.is_empty());
}

#[test]
fn boot_publishes_running_after_four_steps() {
    let mut world = world();
    boot(&mut world);
    assert_eq!(world.rt0_state().expect("rt0"), LifecycleStateName::Running);
    let steps = world.rt0_boot_steps().expect("rt0");
    let names: Vec<&str> = steps.iter().map(|step| step.name()).collect();
    assert_eq!(
        names,
        vec![
            "fix-snapshot",
            "parse-config",
            "establish-runtime",
            "publish-running"
        ]
    );
    assert_eq!(Rt0Step::all().len(), 5);
    let config = world.rt0_config().expect("rt0").expect("配置存在");
    assert_eq!(config.parallelism(), 4);
    assert_eq!(world.rt0_host_parallelism().expect("rt0"), 4);
}

#[test]
fn boot_cannot_run_twice() {
    let mut world = world();
    boot(&mut world);
    assert!(
        world
            .boot(vec![], vec![], "/".to_owned(), 1, entry())
            .is_err()
    );
}

#[test]
fn invalid_configuration_is_fatal_before_running() {
    let mut world = world();
    let report = world
        .boot(
            vec![],
            vec![("GUGU_RUNTIME_PROCS".to_owned(), "0".to_owned())],
            "/work".to_owned(),
            4,
            entry(),
        )
        .expect("boot 不产生模型不变量");
    assert!(!report.started);
    assert_eq!(report.errors[0].variable(), "GUGU_RUNTIME_PROCS");
    assert_eq!(
        world.rt0_state().expect("rt0"),
        LifecycleStateName::Terminating
    );
    assert!(world.rt0_config().expect("rt0").is_none());
    let plan = world.rt0_plan().expect("rt0").expect("终止计划");
    assert_eq!(plan.exit_category(), ExitCategory::RuntimeFailure);
    assert_eq!(plan.exit_code(), 2);
    let reports = world.rt0_reports().expect("rt0");
    assert_eq!(reports.len(), 1);
    assert!(
        reports[0]
            .text()
            .starts_with("termination[invalid-configuration]"),
        "{}",
        reports[0].text()
    );
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.category, ExitCategory::RuntimeFailure);
    assert_eq!(outcome.code, 2);
    assert_eq!(world.rt0_closed_facilities().expect("rt0"), 6);
}

#[test]
fn invalid_diagnostics_config_uses_emergency_plain_text() {
    let mut world = world();
    world
        .boot(
            vec![],
            vec![
                ("GUGU_RUNTIME_DIAGNOSTICS".to_owned(), "yaml".to_owned()),
                ("GUGU_BACKTRACE".to_owned(), "2".to_owned()),
            ],
            "/work".to_owned(),
            4,
            entry(),
        )
        .expect("boot 不产生模型不变量");
    let reports = world.rt0_reports().expect("rt0");
    assert_eq!(reports.len(), 1);
    assert!(reports[0].text().starts_with("gugu emergency report\n"));
    assert!(!reports[0].text().contains('{'));
}

#[test]
fn valid_diagnostics_format_survives_other_config_failures() {
    let mut world = world();
    world
        .boot(
            vec![],
            vec![
                ("GUGU_RUNTIME_DIAGNOSTICS".to_owned(), "json".to_owned()),
                ("GUGU_RUNTIME_STACK_MAX".to_owned(), "1KiB".to_owned()),
            ],
            "/work".to_owned(),
            4,
            entry(),
        )
        .expect("boot 不产生模型不变量");
    let reports = world.rt0_reports().expect("rt0");
    assert_eq!(reports.len(), 1);
    assert!(
        reports[0]
            .text()
            .starts_with("{\"schema\":\"gugu-runtime-report-v1\"")
    );
}

#[test]
fn admission_follows_lifecycle_states() {
    let mut world = world();
    // rt0 启动前没有可运行的进程状态，接纳协程是模型不变量。
    assert!(world.spawn_user_coroutine(0, entry()).is_err());
    boot(&mut world);
    let first = spawn(&mut world);
    let second = spawn(&mut world);
    finish(&mut world, first);
    finish(&mut world, second);
    world.request_exit(0).expect("显式终止");
    assert_eq!(
        world.rt0_state().expect("rt0"),
        LifecycleStateName::Terminating
    );
    assert!(
        world
            .spawn_user_coroutine(0, entry())
            .expect("Terminating拒绝")
            .is_none()
    );
}

#[test]
fn fatal_boundary_cannot_run_user_code_or_be_caught() {
    let mut world = world();
    boot(&mut world);
    world
        .fatal(FatalKind::OutOfMemory, "heap exhausted".to_owned(), None)
        .expect("fatal");
    assert!(world.run_defer(1).is_err());
    assert!(
        world
            .spawn_user_coroutine(0, entry())
            .expect("状态查询")
            .is_none()
    );
    assert_eq!(world.rt0_reports().expect("rt0").len(), 1);
    world
        .fatal(FatalKind::StackOverflow, "second".to_owned(), None)
        .expect("Terminating 中的 fatal 被抑制");
    assert_eq!(world.rt0_suppressed_failures().expect("rt0"), 1);
    assert_eq!(world.rt0_reports().expect("rt0").len(), 1);
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.code, 2);
}

#[test]
fn explicit_exit_terminates_without_waiting_or_reports() {
    let mut world = world();
    boot(&mut world);
    let _child = spawn(&mut world);
    world.request_exit(7).expect("显式退出");
    assert_eq!(
        world.rt0_state().expect("rt0"),
        LifecycleStateName::Terminating
    );
    assert!(world.rt0_reports().expect("rt0").is_empty());
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.category, ExitCategory::ExplicitExit);
    assert_eq!(outcome.code, 7);
}

#[test]
fn detached_panic_during_running_does_not_override_explicit_exit() {
    let mut world = world();
    boot(&mut world);
    let child = spawn(&mut world);
    world
        .detached_panic("detached".to_owned(), None)
        .expect("分离 panic");
    world
        .coroutine_finished(
            child,
            0,
            CompletionValue::Panic {
                handle: 1,
                descriptor: 1,
            },
        )
        .expect("panic完成");
    world.request_exit(0).expect("显式终止");
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.category, ExitCategory::ExplicitExit);
    assert_eq!(outcome.code, 0);
    assert_eq!(world.rt0_reports().expect("rt0").len(), 1);
}

#[test]
fn termination_runs_exactly_once_in_facility_order() {
    let mut world = world();
    boot(&mut world);
    world.request_exit(0).expect("显式终止");
    world.execute_termination(&budget()).expect("首次终止");
    assert_eq!(
        world.rt0_closed_facilities().expect("rt0"),
        ShutdownFacility::all().len()
    );
    assert!(world.execute_termination(&budget()).is_err());
    assert!(world.rt0_outcome().expect("rt0").is_some());
}

#[test]
fn foreign_work_is_waited_only_when_plan_requests_it() {
    let mut world = world();
    boot(&mut world);
    world.enter_foreign(0).expect("登记外部工作");
    world.enter_foreign(0).expect("登记外部工作");
    world.request_exit(0).expect("显式终止");
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.category, ExitCategory::ExplicitExit);
}

#[test]
fn pending_returns_are_flushed_before_facilities_close() {
    let mut world = world();
    let class = RuntimeSizeClassId::from_raw(0);
    let allocation = world.allocate(0, class).expect("分配成功");
    let stride = world
        .descriptor(allocation.slot.descriptor)
        .expect("描述符存在")
        .slot_stride;
    world
        .queue_return(0, allocation.slot, u64::from(stride))
        .expect("排队 return");
    let message = world
        .message(world.token(1), ReturnKind::RawSlot, allocation.slot, stride)
        .expect("构造 return message");
    let inbox = world.inbox(1);
    let mut staging = ProducerStaging::new(BatchLimits::default());
    let outcome = stage_message(
        &world.pool(),
        Some(&inbox),
        &mut staging,
        &message,
        super::inbox::ShardIndex::from_raw(0).expect("shard 合法"),
        Some(FlushTrigger::OwnerPressure),
    )
    .expect("发布成功");
    assert!(outcome.is_some());
    boot(&mut world);
    world.request_exit(0).expect("显式终止");
    let result = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(result.category, ExitCategory::ExplicitExit);
    let descriptor = world
        .descriptor(allocation.slot.descriptor)
        .expect("描述符存在");
    assert_eq!(descriptor.queued, 0);
    world.ledger_invariant(0).expect("账本守恒");
}
