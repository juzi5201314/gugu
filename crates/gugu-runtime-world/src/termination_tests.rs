//! 终止计划构造与退出码解析的确定性测试。

use super::platform::PlatformProfile;
use super::startup_schema::{ExitCategory, FatalKind, ReportEvent, ReportReason, TerminationMode};
use super::termination::{
    explicit_exit, fatal, main_panic, natural_failure, natural_success, signal, signal_exit_code,
};

#[test]
fn natural_success_waits_and_exits_zero() {
    let with_reports = natural_success();
    assert_eq!(with_reports.plan.mode(), TerminationMode::Natural);
    assert!(with_reports.plan.wait_foreign());
    assert!(!with_reports.plan.admit_user_coroutines());
    assert_eq!(with_reports.plan.exit_category(), ExitCategory::Success);
    assert_eq!(with_reports.plan.exit_code(), 0);
    assert!(with_reports.plan.reason().is_none());
    assert!(with_reports.panic_event.is_none());
    assert!(with_reports.termination.is_none());
}

#[test]
fn natural_failure_keeps_natural_wait_and_reports_once() {
    let with_reports = natural_failure(ReportReason::UnhandledPanic, None);
    assert_eq!(with_reports.plan.mode(), TerminationMode::Natural);
    assert!(with_reports.plan.wait_foreign());
    assert_eq!(
        with_reports.plan.exit_category(),
        ExitCategory::ProgramFailure
    );
    assert_eq!(with_reports.plan.exit_code(), 1);
    let termination = with_reports.termination.as_ref().expect("终止报告");
    assert_eq!(termination.event, ReportEvent::Termination);
    assert_eq!(termination.reason, ReportReason::UnhandledPanic);
    assert_eq!(termination.exit_code, 1);
    assert!(with_reports.panic_event.is_none());
}

#[test]
fn main_panic_does_not_wait_and_emits_two_events() {
    let with_reports = main_panic("boom".to_owned(), None);
    assert_eq!(with_reports.plan.mode(), TerminationMode::Immediate);
    assert!(!with_reports.plan.wait_foreign());
    let panic_event = with_reports.panic_event.as_ref().expect("panic 事件");
    assert_eq!(panic_event.event, ReportEvent::Panic);
    assert_eq!(panic_event.message.as_deref(), Some("boom"));
    let termination = with_reports.termination.as_ref().expect("终止报告");
    assert_eq!(termination.reason, ReportReason::UnhandledPanic);
    assert_eq!(termination.exit_code, 1);
}

#[test]
fn explicit_exit_uses_caller_code_without_reports() {
    let with_reports = explicit_exit(7);
    assert_eq!(with_reports.plan.mode(), TerminationMode::ExplicitExit);
    assert_eq!(
        with_reports.plan.exit_category(),
        ExitCategory::ExplicitExit
    );
    assert_eq!(with_reports.plan.exit_code(), 7);
    assert!(!with_reports.plan.wait_foreign());
    assert!(with_reports.termination.is_none());
    assert!(with_reports.panic_event.is_none());
}

#[test]
fn fatal_kinds_map_to_runtime_failure_with_stable_reasons() {
    let expectations = [
        (FatalKind::OutOfMemory, "out-of-memory"),
        (FatalKind::StackOverflow, "stack-overflow"),
        (FatalKind::RuntimeInvariant, "runtime-invariant"),
        (FatalKind::ForeignUnwind, "foreign-unwind"),
        (FatalKind::PanicDuringUnwind, "panic-during-unwind"),
        (FatalKind::HardwareFault, "hardware-fault"),
        (FatalKind::InvalidConfiguration, "invalid-configuration"),
    ];
    for (kind, reason) in expectations {
        let with_reports = fatal(kind, kind.name().to_owned(), None);
        let plan = &with_reports.plan;
        assert_eq!(plan.mode(), TerminationMode::Fatal, "{}", kind.name());
        assert!(!plan.wait_foreign(), "{}", kind.name());
        assert_eq!(
            plan.exit_category(),
            ExitCategory::RuntimeFailure,
            "{}",
            kind.name()
        );
        assert_eq!(plan.exit_code(), 2, "{}", kind.name());
        assert_eq!(
            plan.reason().map(super::startup_schema::ReportReason::name),
            Some(reason),
            "{}",
            kind.name()
        );
        assert_eq!(
            plan.host_default_after_report(),
            kind == FatalKind::HardwareFault,
            "{}",
            kind.name()
        );
        let termination = with_reports.termination.as_ref().expect("终止报告");
        assert_eq!(termination.reason.name(), reason);
    }
}

#[test]
fn signal_exit_codes_follow_target_semantics() {
    let linux = [
        (ReportReason::SignalInterrupt, 130),
        (ReportReason::SignalTerminate, 143),
        (ReportReason::SignalHangup, 129),
        (ReportReason::SignalUser1, 138),
        (ReportReason::SignalUser2, 140),
    ];
    for (reason, code) in linux {
        assert_eq!(signal_exit_code(PlatformProfile::Linux, reason), Ok(code));
    }
    let windows = signal_exit_code(PlatformProfile::Windows, ReportReason::SignalInterrupt);
    assert_eq!(windows, Ok(0xC000_013A));
    assert!(signal_exit_code(PlatformProfile::Windows, ReportReason::SignalTerminate).is_err());
    assert!(signal_exit_code(PlatformProfile::Linux, ReportReason::SignalBreak).is_err());
    assert!(signal_exit_code(PlatformProfile::Linux, ReportReason::MainError).is_err());
}

#[test]
fn signal_plan_reports_and_uses_signal_category() {
    let with_reports =
        signal(ReportReason::SignalInterrupt, PlatformProfile::Linux).expect("Linux 提供 SIGINT");
    assert_eq!(with_reports.plan.mode(), TerminationMode::Signal);
    assert_eq!(with_reports.plan.exit_category(), ExitCategory::Signal);
    assert_eq!(with_reports.plan.exit_code(), 130);
    assert!(with_reports.plan.host_default_after_report());
    let termination = with_reports.termination.as_ref().expect("终止报告");
    assert_eq!(termination.reason, ReportReason::SignalInterrupt);
    assert_eq!(termination.exit_code, 130);
}
