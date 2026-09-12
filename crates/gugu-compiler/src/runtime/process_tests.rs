//! rt0 进程模型接入 raw world 的确定性测试。
//!
//! 覆盖五步启动、`InvalidConfiguration` fatal、生命周期单向迁移、各终止路径的计划与
//! 退出码、`Terminating` 的用户代码闸门、报告形态选择、`PanicDuringUnwind` 升级、
//! producer flush 与设施关闭的 exactly-once。

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
use super::world::termination_impl::MainOutcome;

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
            .contains("reason: invalid-configuration\n")
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
    world.call_main(MainOutcome::Returned).expect("main 返回");
    let descendant = spawn(&mut world);
    finish(&mut world, second);
    finish(&mut world, descendant);
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
fn natural_success_exits_zero_without_reports() {
    let mut world = world();
    boot(&mut world);
    let child = spawn(&mut world);
    world.call_main(MainOutcome::Returned).expect("main 返回");
    assert_eq!(world.rt0_state().expect("rt0"), LifecycleStateName::Waiting);
    finish(&mut world, child);
    assert_eq!(
        world.rt0_state().expect("rt0"),
        LifecycleStateName::Terminating
    );
    let plan = world.rt0_plan().expect("rt0").expect("终止计划");
    assert_eq!(plan.exit_category(), ExitCategory::Success);
    assert_eq!(plan.exit_code(), 0);
    assert!(plan.wait_foreign());
    assert!(world.rt0_reports().expect("rt0").is_empty());
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.category, ExitCategory::Success);
    assert_eq!(outcome.code, 0);
}

#[test]
fn main_error_still_waits_for_user_coroutines() {
    let mut world = world();
    boot(&mut world);
    let first = spawn(&mut world);
    let second = spawn(&mut world);
    world
        .call_main(MainOutcome::ReturnedErr("boom".to_owned()))
        .expect("main 返回");
    assert_eq!(world.rt0_state().expect("rt0"), LifecycleStateName::Waiting);
    let reports = world.rt0_reports().expect("rt0");
    assert_eq!(reports.len(), 1);
    assert!(reports[0].text().contains("reason: main-error\n"));
    finish(&mut world, first);
    finish(&mut world, second);
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.category, ExitCategory::ProgramFailure);
    assert_eq!(outcome.code, 1);
    assert_eq!(world.rt0_reports().expect("rt0").len(), 1);
}

#[test]
fn main_panic_runs_only_main_defers_and_terminates_immediately() {
    let mut world = world();
    boot(&mut world);
    let _child = spawn(&mut world);
    world
        .main_panicked("main failed".to_owned(), None)
        .expect("记录主协程 panic");
    world.run_defer(2).expect("主协程 defer 在展开中运行");
    world.complete_main_panic().expect("展开完成");
    assert_eq!(
        world.rt0_state().expect("rt0"),
        LifecycleStateName::Terminating
    );
    assert!(world.run_defer(1).is_err());
    let reports = world.rt0_reports().expect("rt0");
    assert_eq!(reports.len(), 2);
    assert!(reports[0].text().contains("event: panic\n"));
    assert!(reports[1].text().contains("reason: unhandled-panic\n"));
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.category, ExitCategory::ProgramFailure);
    assert_eq!(outcome.code, 1);
    assert_eq!(world.rt0_defer_runs().expect("rt0"), 2);
}

#[test]
fn fatal_during_main_unwind_escalates_to_panic_during_unwind() {
    let mut world = world();
    boot(&mut world);
    world
        .main_panicked("first".to_owned(), None)
        .expect("记录主协程 panic");
    world
        .fatal(
            FatalKind::RuntimeInvariant,
            "barrier violated".to_owned(),
            None,
        )
        .expect("展开窗口内的 fatal");
    world.complete_main_panic().expect("展开完成");
    let plan = world.rt0_plan().expect("rt0").expect("终止计划");
    assert_eq!(plan.exit_category(), ExitCategory::RuntimeFailure);
    assert_eq!(plan.exit_code(), 2);
    let reports = world.rt0_reports().expect("rt0");
    assert_eq!(reports.len(), 2);
    assert!(reports[1].text().contains("reason: panic-during-unwind\n"));
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.code, 2);
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
fn detached_panic_in_waiting_downgrades_natural_exit() {
    let mut world = world();
    boot(&mut world);
    let child = spawn(&mut world);
    world.call_main(MainOutcome::Returned).expect("main 返回");
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
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.category, ExitCategory::ProgramFailure);
    assert_eq!(outcome.code, 1);
    let reports = world.rt0_reports().expect("rt0");
    assert_eq!(reports.len(), 2);
    assert!(reports[0].text().contains("event: panic\n"));
    assert!(reports[1].text().contains("reason: unhandled-panic\n"));
}

#[test]
fn detached_panic_during_running_keeps_success_exit() {
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
    world.call_main(MainOutcome::Returned).expect("main 返回");
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.category, ExitCategory::Success);
    assert_eq!(outcome.code, 0);
    assert_eq!(world.rt0_reports().expect("rt0").len(), 1);
}

#[test]
fn termination_runs_exactly_once_in_facility_order() {
    let mut world = world();
    boot(&mut world);
    world.call_main(MainOutcome::Returned).expect("main 返回");
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
    world.enter_foreign().expect("登记外部工作");
    world.enter_foreign().expect("登记外部工作");
    world.leave_foreign().expect("外部工作完成");
    world.call_main(MainOutcome::Returned).expect("main 返回");
    let outcome = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(outcome.category, ExitCategory::Success);
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
    world.call_main(MainOutcome::Returned).expect("main 返回");
    let result = world.execute_termination(&budget()).expect("终止执行");
    assert_eq!(result.category, ExitCategory::Success);
    let descriptor = world
        .descriptor(allocation.slot.descriptor)
        .expect("描述符存在");
    assert_eq!(descriptor.queued, 0);
    world.ledger_invariant(0).expect("账本守恒");
}
