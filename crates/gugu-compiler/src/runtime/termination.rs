//! `TerminationPlan` 与各终止路径的确定性参照实现。
//!
//! runtime 状态机根据进程寿命表生成计划；scheduler 只执行计划，不决定
//! `process.exit`、fatal、defer 或报告语义。`report_epoch` 是必须冲刷的报告数量
//! 下界：shutdown 完成前报告账本至少要发布这么多条报告。计划生成的同时给出需要
//! 发布的终止报告描述，由进程模型在迁移线性化点发布。

use super::platform::PlatformProfile;
use super::report::SourceLocation;
use super::startup_schema::{ExitCategory, FatalKind, ReportEvent, ReportReason, TerminationMode};

/// 一条待发布报告的描述；发布时才分配单调序号。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReportSpec {
    pub(crate) event: ReportEvent,
    pub(crate) class: ExitCategory,
    pub(crate) reason: ReportReason,
    pub(crate) message: Option<String>,
    pub(crate) location: Option<SourceLocation>,
    /// 报告 `exit_code` 字段：未处理 panic 事件按程序失败类别登记。
    pub(crate) exit_code: i64,
}

/// 终止计划与随计划发布的报告。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlanWithReports {
    pub(crate) plan: TerminationPlan,
    pub(crate) panic_event: Option<ReportSpec>,
    pub(crate) termination: Option<ReportSpec>,
}

/// 终止计划。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminationPlan {
    mode: TerminationMode,
    reason: Option<ReportReason>,
    admit_user_coroutines: bool,
    wait_foreign: bool,
    report_epoch: u64,
    exit_category: ExitCategory,
    exit_code: i64,
    host_default_after_report: bool,
}

impl TerminationPlan {
    /// 返回调度模式。
    pub(crate) const fn mode(&self) -> TerminationMode {
        self.mode
    }

    /// 返回计划的终止 reason；自然成功与显式退出没有失败记录。
    pub(crate) const fn reason(&self) -> Option<ReportReason> {
        self.reason
    }

    /// 返回是否继续接纳新的用户协程；进入 `Terminating` 后一律关闭。
    pub(crate) const fn admit_user_coroutines(&self) -> bool {
        self.admit_user_coroutines
    }

    /// 返回是否等待普通 foreign、`DirtyWaiting` 与执行中的 dirty work。
    pub(crate) const fn wait_foreign(&self) -> bool {
        self.wait_foreign
    }

    /// 返回报告冲刷下界。
    pub(crate) const fn report_epoch(&self) -> u64 {
        self.report_epoch
    }

    /// 返回逻辑退出类别。
    pub(crate) const fn exit_category(&self) -> ExitCategory {
        self.exit_category
    }

    /// 返回退出码。
    pub(crate) const fn exit_code(&self) -> i64 {
        self.exit_code
    }

    /// 返回报告之后是否恢复宿主默认终止行为（硬件 fault）。
    pub(crate) const fn host_default_after_report(&self) -> bool {
        self.host_default_after_report
    }

    /// 回填报告冲刷下界；由进程模型在发布终止报告后调用。
    pub(crate) fn set_report_epoch(&mut self, flushed_reports: u64) {
        self.report_epoch = flushed_reports;
    }

    /// 由字段构造计划；`report_epoch` 由进程模型在发布报告后回填。
    pub(crate) fn new(
        mode: TerminationMode,
        reason: Option<ReportReason>,
        wait_foreign: bool,
        exit_category: ExitCategory,
        exit_code: i64,
        host_default_after_report: bool,
    ) -> Self {
        Self {
            mode,
            reason,
            admit_user_coroutines: false,
            wait_foreign,
            report_epoch: 0,
            exit_category,
            exit_code,
            host_default_after_report,
        }
    }
}

/// 自然成功退出：等待全部用户协程与 dirty work，不发布终止报告。
pub(crate) fn natural_success() -> PlanWithReports {
    let plan = TerminationPlan::new(
        TerminationMode::Natural,
        None,
        true,
        ExitCategory::Success,
        0,
        false,
    );
    PlanWithReports {
        plan,
        panic_event: None,
        termination: None,
    }
}

/// 带失败记录的自然退出：仍按自然规则等待，最后以程序失败类别退出。
pub(crate) fn natural_failure(reason: ReportReason, message: Option<String>) -> PlanWithReports {
    let plan = TerminationPlan::new(
        TerminationMode::Natural,
        Some(reason),
        true,
        ExitCategory::ProgramFailure,
        1,
        false,
    );
    PlanWithReports {
        plan,
        panic_event: None,
        termination: Some(ReportSpec {
            event: ReportEvent::Termination,
            class: ExitCategory::ProgramFailure,
            reason,
            message,
            location: None,
            exit_code: 1,
        }),
    }
}

/// `main` panic：先发布 panic 事件，展开主协程 defer 后立即进入 `Terminating`；
/// 其它协程不展开、不运行剩余 defer。
pub(crate) fn main_panic(message: String, location: Option<SourceLocation>) -> PlanWithReports {
    let plan = TerminationPlan::new(
        TerminationMode::Immediate,
        Some(ReportReason::UnhandledPanic),
        false,
        ExitCategory::ProgramFailure,
        1,
        false,
    );
    PlanWithReports {
        plan,
        panic_event: Some(ReportSpec {
            event: ReportEvent::Panic,
            class: ExitCategory::ProgramFailure,
            reason: ReportReason::UnhandledPanic,
            message: Some(message),
            location,
            exit_code: 1,
        }),
        termination: Some(ReportSpec {
            event: ReportEvent::Termination,
            class: ExitCategory::ProgramFailure,
            reason: ReportReason::UnhandledPanic,
            message: None,
            location: None,
            exit_code: 1,
        }),
    }
}

/// 与 `natural_failure` 相同的计划，但不重复发布终止报告。
///
/// `main` 返回 `Err` 时终止事件已经发布；之后的自然退出只沿用其类别与退出码。
pub(crate) fn natural_failure_quiet(reason: ReportReason) -> PlanWithReports {
    let mut with_reports = natural_failure(reason, None);
    with_reports.termination = None;
    with_reports
}

/// `std.process.exit(code)`：立即进入 `Terminating`，不等待、不运行剩余 defer。
pub(crate) fn explicit_exit(code: i64) -> PlanWithReports {
    let plan = TerminationPlan::new(
        TerminationMode::ExplicitExit,
        None,
        false,
        ExitCategory::ExplicitExit,
        code,
        false,
    );
    PlanWithReports {
        plan,
        panic_event: None,
        termination: None,
    }
}

/// fatal：发布终止报告后终止进程；不等待其它协程、不保证用户资源租约的 defer 清理。
pub(crate) fn fatal(
    kind: FatalKind,
    message: String,
    location: Option<SourceLocation>,
) -> PlanWithReports {
    let plan = TerminationPlan::new(
        TerminationMode::Fatal,
        Some(kind.reason()),
        false,
        ExitCategory::RuntimeFailure,
        2,
        kind == FatalKind::HardwareFault,
    );
    PlanWithReports {
        plan,
        panic_event: None,
        termination: Some(ReportSpec {
            event: ReportEvent::Termination,
            class: ExitCategory::RuntimeFailure,
            reason: kind.reason(),
            message: Some(message),
            location,
            exit_code: 2,
        }),
    }
}

/// 未被订阅的普通终止信号：发布一条 signal 报告后恢复宿主默认动作。
pub(crate) fn signal(
    reason: ReportReason,
    profile: PlatformProfile,
) -> Result<PlanWithReports, String> {
    let code = signal_exit_code(profile, reason)?;
    let plan = TerminationPlan::new(
        TerminationMode::Signal,
        Some(reason),
        false,
        ExitCategory::Signal,
        code,
        true,
    );
    Ok(PlanWithReports {
        plan,
        panic_event: None,
        termination: Some(ReportSpec {
            event: ReportEvent::Termination,
            class: ExitCategory::Signal,
            reason,
            message: None,
            location: None,
            exit_code: code,
        }),
    })
}

/// 解析信号类别的退出码：Linux 为 `128 + 信号号`，Windows 为登记的非零 status。
pub(crate) fn signal_exit_code(
    profile: PlatformProfile,
    reason: ReportReason,
) -> Result<i64, String> {
    let signal = reason
        .signal()
        .ok_or_else(|| format!("reason `{}` 不是信号终止", reason.name()))?;
    match profile {
        PlatformProfile::Linux => signal
            .linux_number()
            .map(|number| 128 + i64::from(number))
            .ok_or_else(|| format!("Linux 不提供信号 {}", reason.name())),
        PlatformProfile::Windows => signal
            .windows_status()
            .map(|status| i64::from(status))
            .ok_or_else(|| {
                format!(
                    "Windows 的 {} 由宿主决定，不在报告契约中登记",
                    reason.name()
                )
            }),
    }
}
