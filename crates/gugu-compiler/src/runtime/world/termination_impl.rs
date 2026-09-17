//! rt0 进程模型在 owner-directed 世界中的接入。
//!
//! 世界构造等价于 rt0 的「建立运行环境」步骤；`boot` 固定环境快照并解析启动配置，
//! `call_main`/`request_exit`/`fatal` 驱动单向状态迁移，`execute_termination` 按
//! 「producer flush → 越过最新 epoch → 按 `wait_foreign` 等待 → 固定顺序关闭设施 →
//! 报告冲刷到 `report_epoch`」收尾并恰好一次。报告只经 emergency buffer 渲染；
//! `Terminating` 中不运行用户 defer、不接纳新协程、不再发布用户失败事件。

use super::super::coroutine::{CompletionValue, CoroutineHandle};
use super::super::inbox::ServiceBudget;
use super::super::lifecycle::{BootSequence, Lifecycle};
use super::super::platform::PlatformProfile;
use super::super::report::{
    EmittedReport, RenderFormat, ReportLedger, SourceLocation, collect_backtrace, render_format,
};
use super::super::slab::RawInvariant;
use super::super::startup::{
    BacktraceMode, EnvironmentSnapshot, StartupConfig, StartupError, needs_emergency_report,
    parse_backtrace_var, parse_diagnostics_var,
};
use super::super::startup_schema::{
    ExitCategory, FatalKind, LifecycleStateName, ReportEvent, ReportReason, Rt0Step,
    ShutdownFacility,
};
use super::super::termination::{self, PlanWithReports, ReportSpec, TerminationPlan};
use super::RawWorld;
use super::coroutine_impl::CoroutineEntry;

/// `main` 入口的结局；panic 由 `main_panicked`/`complete_main_panic` 单独建模。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MainOutcome {
    /// 返回 `()`。
    Returned,
    /// 返回 `Err(e)`；`e` 按 `Print` 规则由程序自行渲染，不属于 runtime report。
    ReturnedErr(String),
}

/// 终止执行结果：交给宿主的退出类别与码。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminationOutcome {
    pub(crate) category: ExitCategory,
    pub(crate) code: i64,
}

/// 启动结果：配置非法时进入 `InvalidConfiguration` fatal，world 已带终止计划。
pub(crate) struct BootReport {
    pub(crate) started: bool,
    pub(crate) config: Option<StartupConfig>,
    pub(crate) errors: Vec<StartupError>,
}

/// rt0 进程模型的状态；`boot` 之前 world 没有 rt0 状态。
#[derive(Debug)]
pub(super) struct Rt0Process {
    snapshot: EnvironmentSnapshot,
    lifecycle: Lifecycle,
    boot: BootSequence,
    ledger: ReportLedger,
    report_format: RenderFormat,
    backtrace_mode: BacktraceMode,
    config: Option<StartupConfig>,
    alive_user_coroutines: u32,
    spawned_user_coroutines: u32,
    defer_runs: u32,
    main_called: bool,
    main_coroutine: Option<CoroutineHandle>,
    main_error: Option<String>,
    detached_panic: bool,
    termination_emitted: bool,
    pending_panic: Option<PlanWithReports>,
    plan: Option<TerminationPlan>,
    facilities_closed: usize,
    foreign_work: u32,
    suppressed_failures: u32,
    terminated: bool,
    outcome: Option<TerminationOutcome>,
}

impl Rt0Process {
    fn new(
        snapshot: EnvironmentSnapshot,
        config: StartupConfig,
        lifecycle: Lifecycle,
        boot: BootSequence,
    ) -> Self {
        Self {
            report_format: render_format(config.diagnostics()),
            backtrace_mode: config.backtrace(),
            config: Some(config),
            snapshot,
            lifecycle,
            boot,
            ledger: ReportLedger::new(),
            alive_user_coroutines: 0,
            spawned_user_coroutines: 0,
            defer_runs: 0,
            main_called: false,
            main_coroutine: None,
            main_error: None,
            detached_panic: false,
            termination_emitted: false,
            pending_panic: None,
            plan: None,
            facilities_closed: 0,
            foreign_work: 0,
            suppressed_failures: 0,
            terminated: false,
            outcome: None,
        }
    }

    /// 配置非法的启动：报告格式按诊断配置能否单独解析决定。
    fn failed_boot(
        snapshot: EnvironmentSnapshot,
        lifecycle: Lifecycle,
        boot: BootSequence,
        errors: &[StartupError],
    ) -> Self {
        let format = if needs_emergency_report(errors) {
            RenderFormat::Emergency
        } else {
            parse_diagnostics_var(&snapshot).map_or(RenderFormat::Emergency, render_format)
        };
        Self {
            report_format: format,
            backtrace_mode: parse_backtrace_var(&snapshot).unwrap_or(BacktraceMode::Off),
            config: None,
            snapshot,
            lifecycle,
            boot,
            ledger: ReportLedger::new(),
            alive_user_coroutines: 0,
            spawned_user_coroutines: 0,
            defer_runs: 0,
            main_called: false,
            main_coroutine: None,
            main_error: None,
            detached_panic: false,
            termination_emitted: false,
            pending_panic: None,
            plan: None,
            facilities_closed: 0,
            foreign_work: 0,
            suppressed_failures: 0,
            terminated: false,
            outcome: None,
        }
    }

    /// 由运行时内部（例如 GC 失败取消）发布一条 fatal 报告；返回报告序号。
    pub(crate) fn emit_runtime_report(&mut self, spec: ReportSpec) -> u64 {
        self.emit(spec)
    }

    fn emit(&mut self, spec: ReportSpec) -> u64 {
        let frames = collect_backtrace(self.backtrace_mode, &[]);
        self.ledger.emit(
            spec.event,
            spec.class,
            spec.reason,
            spec.message,
            spec.location,
            frames,
            spec.exit_code,
            self.report_format,
        )
    }

    /// 进入终止收尾：发布终止报告并回填 `report_epoch`；状态迁移由调用方完成。
    fn enter_termination(&mut self, mut with_reports: PlanWithReports) -> Result<(), String> {
        if self.plan.is_some() {
            return Err("已经处于终止状态".to_owned());
        }
        match with_reports.termination.take() {
            Some(spec) => {
                let epoch = self.emit(spec);
                with_reports.plan.set_report_epoch(epoch + 1);
            }
            None => with_reports.plan.set_report_epoch(self.ledger.next_epoch()),
        }
        self.termination_emitted = true;
        self.plan = Some(with_reports.plan);
        Ok(())
    }
}

impl RawWorld {
    /// rt0 启动：固定快照、解析配置；非法配置按 `InvalidConfiguration` fatal 结束。
    pub(crate) fn boot(
        &mut self,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        working_directory: String,
        host_parallelism: u32,
        main_entry: CoroutineEntry,
    ) -> Result<BootReport, RawInvariant> {
        if self.rt0.is_some() {
            return Err(RawInvariant::new("rt0 不能启动两次"));
        }
        if self.owners.is_empty() {
            return Err(RawInvariant::new("运行环境没有可用的 owner 设施"));
        }
        let snapshot = EnvironmentSnapshot::fix(argv, env, working_directory, host_parallelism);
        let mut lifecycle = Lifecycle::new();
        let mut boot = BootSequence::new();
        let config = match boot.start(&mut lifecycle, &snapshot) {
            Ok(inner) => inner,
            Err(message) => return Err(RawInvariant::new(message)),
        };
        match config {
            Ok(config) => {
                self.rt0 = Some(Rt0Process::new(snapshot, config, lifecycle, boot));
                // 启动配置里的 growth target 与 soft limit 必须立刻进入 pacing 平面，
                // 否则 limit 触发、forced cycle 与 OOM 规则都不生效。
                self.apply_startup_pacing()?;
                let main = self.create_coroutine(0, main_entry, config.stack_max())?;
                self.enter_coroutine(main)?;
                self.rt0_mut()?.main_coroutine = Some(main);
                Ok(BootReport {
                    started: true,
                    config: Some(config),
                    errors: Vec::new(),
                })
            }
            Err(errors) => {
                let message = errors
                    .first()
                    .map(|error| format!("{}: {}", error.variable(), error.detail()))
                    .unwrap_or_else(|| "启动配置非法".to_owned());
                let mut process = Rt0Process::failed_boot(snapshot, lifecycle, boot, &errors);
                let with_reports =
                    termination::fatal(FatalKind::InvalidConfiguration, message, None);
                process
                    .lifecycle
                    .transition(LifecycleStateName::Terminating, "termination-started")
                    .map_err(RawInvariant::new)?;
                process
                    .enter_termination(with_reports)
                    .map_err(RawInvariant::new)?;
                self.rt0 = Some(process);
                Ok(BootReport {
                    started: false,
                    config: None,
                    errors,
                })
            }
        }
    }

    fn rt0_mut(&mut self) -> Result<&mut Rt0Process, RawInvariant> {
        self.rt0
            .as_mut()
            .ok_or_else(|| RawInvariant::new("rt0 尚未启动"))
    }

    fn rt0_ref(&self) -> Result<&Rt0Process, RawInvariant> {
        self.rt0
            .as_ref()
            .ok_or_else(|| RawInvariant::new("rt0 尚未启动"))
    }

    /// 返回当前生命周期状态。
    pub(crate) fn rt0_state(&self) -> Result<LifecycleStateName, RawInvariant> {
        Ok(self.rt0_ref()?.lifecycle.state())
    }

    /// 返回解析后的启动配置；配置非法时为 `None`。
    pub(crate) fn rt0_config(&self) -> Result<Option<&StartupConfig>, RawInvariant> {
        Ok(self.rt0_ref()?.config.as_ref())
    }

    /// 返回已完成的 rt0 启动步骤。
    pub(crate) fn rt0_boot_steps(&self) -> Result<&[Rt0Step], RawInvariant> {
        Ok(self.rt0_ref()?.boot.steps())
    }

    /// 返回已发布报告。
    pub(crate) fn rt0_reports(&self) -> Result<&[EmittedReport], RawInvariant> {
        Ok(self.rt0_ref()?.ledger.emitted())
    }

    /// 返回当前终止计划。
    pub(crate) fn rt0_plan(&self) -> Result<Option<&TerminationPlan>, RawInvariant> {
        Ok(self.rt0_ref()?.plan.as_ref())
    }

    /// 返回终止执行结果。
    pub(crate) fn rt0_outcome(&self) -> Result<Option<TerminationOutcome>, RawInvariant> {
        self.rt0_ref().map(|rt0| rt0.outcome)
    }

    /// 返回用户 defer 执行次数；`Terminating` 中不得增长。
    pub(crate) fn rt0_defer_runs(&self) -> Result<u32, RawInvariant> {
        Ok(self.rt0_ref()?.defer_runs)
    }

    /// 返回存活用户协程数量。
    pub(crate) fn rt0_alive_coroutines(&self) -> Result<u32, RawInvariant> {
        Ok(self.rt0_ref()?.alive_user_coroutines)
    }

    /// 返回被 Terminating 抑制的用户失败事件数量。
    pub(crate) fn rt0_suppressed_failures(&self) -> Result<u32, RawInvariant> {
        Ok(self.rt0_ref()?.suppressed_failures)
    }

    /// 返回已按固定顺序关闭的设施数量。
    pub(crate) fn rt0_closed_facilities(&self) -> Result<usize, RawInvariant> {
        Ok(self.rt0_ref()?.facilities_closed)
    }

    /// 运行用户 defer；`Terminating` 中拒绝，主协程 panic 展开窗口内允许。
    pub(crate) fn run_defer(&mut self, count: u32) -> Result<(), RawInvariant> {
        let rt0 = self.rt0_mut()?;
        if !rt0.lifecycle.user_code_allowed() {
            return Err(RawInvariant::new("Terminating 中不能运行用户 defer"));
        }
        rt0.defer_runs += count;
        Ok(())
    }

    /// 接纳一个新的用户协程；`Booting` 不运行用户函数，`Terminating` 不再启动。
    pub(crate) fn spawn_user_coroutine(
        &mut self,
        owner: u32,
        entry: CoroutineEntry,
    ) -> Result<Option<CoroutineHandle>, RawInvariant> {
        let rt0 = self.rt0_ref()?;
        if !rt0.lifecycle.admission_open() {
            return Ok(None);
        }
        let limit = rt0.config.as_ref().expect("Running配置").stack_max();
        let handle = self.create_coroutine(owner, entry, limit)?;
        let rt0 = self.rt0_mut()?;
        rt0.alive_user_coroutines += 1;
        rt0.spawned_user_coroutines += 1;
        Ok(Some(handle))
    }

    /// 一个用户协程自然结束；`Waiting` 中最后一个结束后进入自然退出。
    pub(crate) fn coroutine_finished(
        &mut self,
        handle: CoroutineHandle,
        owner: u32,
        value: CompletionValue,
    ) -> Result<(), RawInvariant> {
        if self.rt0_ref()?.main_coroutine == Some(handle) {
            return Err(RawInvariant::new("主协程必须使用main终止路径"));
        }
        if self.rt0_ref()?.alive_user_coroutines == 0 {
            return Err(RawInvariant::new("没有存活的用户协程可以结束"));
        }
        let ticket = self.stage_coroutine_finish(handle, value)?;
        self.finish_coroutine_on_system(&ticket, owner)?;
        let rt0 = self.rt0_mut()?;
        rt0.alive_user_coroutines -= 1;
        if rt0.lifecycle.state() != LifecycleStateName::Waiting || rt0.alive_user_coroutines != 0 {
            return Ok(());
        }
        rt0.lifecycle
            .transition(LifecycleStateName::Terminating, "termination-started")
            .map_err(RawInvariant::new)?;
        let with_reports = match (&rt0.main_error, rt0.detached_panic) {
            (Some(_), _) => termination::natural_failure_quiet(ReportReason::MainError),
            (None, true) => termination::natural_failure(ReportReason::UnhandledPanic, None),
            (None, false) => termination::natural_success(),
        };
        rt0.enter_termination(with_reports)
            .map_err(RawInvariant::new)
    }

    /// `main` 正常返回：仍有存活协程时进入 `Waiting`，否则直接自然收尾。
    pub(crate) fn call_main(&mut self, outcome: MainOutcome) -> Result<(), RawInvariant> {
        let rt0 = self.rt0_ref()?;
        if rt0.main_called {
            return Err(RawInvariant::new("main 只能调用一次"));
        }
        if rt0.lifecycle.state() != LifecycleStateName::Running {
            return Err(RawInvariant::new("main 只能在 Running 中调用"));
        }
        self.finish_main_stack(CompletionValue::Bits(u64::from(matches!(
            outcome,
            MainOutcome::ReturnedErr(_)
        ))))?;
        let rt0 = self.rt0_mut()?;
        rt0.main_called = true;
        rt0.boot.call_main();
        match outcome {
            MainOutcome::Returned => {}
            MainOutcome::ReturnedErr(message) => {
                let spec = ReportSpec {
                    event: ReportEvent::Termination,
                    class: ExitCategory::ProgramFailure,
                    reason: ReportReason::MainError,
                    message: Some(message.clone()),
                    location: None,
                    exit_code: 1,
                };
                rt0.main_error = Some(message);
                rt0.emit(spec);
            }
        }
        if rt0.alive_user_coroutines > 0 {
            rt0.lifecycle
                .transition(LifecycleStateName::Waiting, "main-returned")
                .map_err(RawInvariant::new)?;
            return Ok(());
        }
        let with_reports = if rt0.main_error.is_some() {
            termination::natural_failure_quiet(ReportReason::MainError)
        } else {
            termination::natural_success()
        };
        rt0.lifecycle
            .transition(LifecycleStateName::Terminating, "natural-exit")
            .map_err(RawInvariant::new)?;
        rt0.enter_termination(with_reports)
            .map_err(RawInvariant::new)
    }

    /// 主协程 panic：记录 panic 事件并开始展开；defer 运行后调用 `complete_main_panic`。
    pub(crate) fn main_panicked(
        &mut self,
        message: String,
        location: Option<SourceLocation>,
    ) -> Result<(), RawInvariant> {
        let rt0 = self.rt0_mut()?;
        if rt0.main_called {
            return Err(RawInvariant::new("main 只能结束一次"));
        }
        if rt0.lifecycle.state() != LifecycleStateName::Running {
            return Err(RawInvariant::new("main 只能在 Running 中 panic"));
        }
        rt0.main_called = true;
        rt0.boot.call_main();
        let with_reports = termination::main_panic(message, location);
        if let Some(spec) = with_reports.panic_event.clone() {
            rt0.emit(spec);
        }
        rt0.pending_panic = Some(with_reports);
        Ok(())
    }

    /// 主协程展开完成：立即进入 `Terminating`，其它协程不展开、不运行剩余 defer。
    pub(crate) fn complete_main_panic(&mut self) -> Result<(), RawInvariant> {
        let pending = self
            .rt0_mut()?
            .pending_panic
            .take()
            .ok_or_else(|| RawInvariant::new("没有待完成的主协程 panic"))?;
        let panic_handle = self.rt0_ref()?.ledger.next_epoch().max(1);
        self.finish_main_stack(CompletionValue::Panic {
            handle: panic_handle,
            descriptor: 1,
        })?;
        let rt0 = self.rt0_mut()?;
        if rt0.lifecycle.state() != LifecycleStateName::Running {
            return Err(RawInvariant::new("主协程 panic 只能在 Running 中完成展开"));
        }
        rt0.lifecycle
            .transition(LifecycleStateName::Terminating, "termination-started")
            .map_err(RawInvariant::new)?;
        rt0.enter_termination(pending).map_err(RawInvariant::new)
    }

    fn finish_main_stack(&mut self, value: CompletionValue) -> Result<(), RawInvariant> {
        let main = self
            .rt0_ref()?
            .main_coroutine
            .ok_or_else(|| RawInvariant::new("main缺少控制块"))?;
        let ticket = self.stage_coroutine_finish(main, value)?;
        self.finish_coroutine_on_system(&ticket, 0)
    }

    /// 分离协程 panic：发布一份未处理 panic 报告；`Waiting` 中改变自然退出类别。
    pub(crate) fn detached_panic(
        &mut self,
        message: String,
        location: Option<SourceLocation>,
    ) -> Result<(), RawInvariant> {
        let rt0 = self.rt0_mut()?;
        if !rt0.lifecycle.user_code_allowed() {
            rt0.suppressed_failures += 1;
            return Ok(());
        }
        let waiting = rt0.lifecycle.state() == LifecycleStateName::Waiting;
        if waiting {
            rt0.detached_panic = true;
        }
        let spec = ReportSpec {
            event: ReportEvent::Panic,
            class: ExitCategory::ProgramFailure,
            reason: ReportReason::UnhandledPanic,
            message: Some(message),
            location,
            exit_code: 1,
        };
        rt0.emit(spec);
        Ok(())
    }

    /// `std.process.exit(code)`：立即进入 `Terminating` 并把 code 交给宿主。
    pub(crate) fn request_exit(&mut self, code: i64) -> Result<(), RawInvariant> {
        let rt0 = self.rt0_mut()?;
        if rt0.lifecycle.state() != LifecycleStateName::Running
            && rt0.lifecycle.state() != LifecycleStateName::Waiting
        {
            return Err(RawInvariant::new("显式退出只能在用户代码中发起"));
        }
        rt0.lifecycle
            .transition(LifecycleStateName::Terminating, "termination-started")
            .map_err(RawInvariant::new)?;
        rt0.enter_termination(termination::explicit_exit(code))
            .map_err(RawInvariant::new)
    }

    /// fatal：不能被 `catch`、`Join.wait` 或用户 defer 截获。
    ///
    /// 主协程 panic 展开窗口内的 fatal 升级为 `PanicDuringUnwind`；已经处于
    /// `Terminating` 的后续 fatal 被抑制计数，不重复发布报告。
    pub(crate) fn fatal(
        &mut self,
        kind: FatalKind,
        message: String,
        location: Option<SourceLocation>,
    ) -> Result<(), RawInvariant> {
        let rt0 = self.rt0_mut()?;
        if let Some(pending) = &mut rt0.pending_panic {
            if pending
                .termination
                .as_ref()
                .is_some_and(|spec| spec.reason == ReportReason::PanicDuringUnwind)
            {
                rt0.suppressed_failures += 1;
                return Ok(());
            }
            let escalated = termination::fatal(FatalKind::PanicDuringUnwind, message, location);
            *pending = escalated;
            return Ok(());
        }
        if rt0.plan.is_some() {
            rt0.suppressed_failures += 1;
            return Ok(());
        }
        rt0.lifecycle
            .transition(LifecycleStateName::Terminating, "termination-started")
            .map_err(RawInvariant::new)?;
        rt0.enter_termination(termination::fatal(kind, message, location))
            .map_err(RawInvariant::new)
    }

    /// 登记一项外部工作；`wait_foreign` 为真时终止要等待它完成。
    ///
    /// 进入 `ForeignBridge` 是第三个 barrier 触发点：本 processor 的 card 键必须在穿透
    /// native 边界之前离开账本，否则 native 侧可能长时间不再回到 Gugu 代码。
    pub(crate) fn enter_foreign(&mut self, owner: u32) -> Result<(), RawInvariant> {
        self.flush_all_barriers(
            owner,
            super::super::barrier::BarrierFlushReason::ForeignBridge,
        )?;
        let rt0 = self.rt0_mut()?;
        rt0.foreign_work += 1;
        Ok(())
    }

    /// 一项外部工作完成。
    pub(crate) fn leave_foreign(&mut self) -> Result<(), RawInvariant> {
        let rt0 = self.rt0_mut()?;
        if rt0.foreign_work == 0 {
            return Err(RawInvariant::new("外部工作计数下溢"));
        }
        rt0.foreign_work -= 1;
        Ok(())
    }

    /// 执行终止计划；成功后 world 持有交给宿主的退出类别与码。
    pub(crate) fn execute_termination(
        &mut self,
        budget: &ServiceBudget,
    ) -> Result<TerminationOutcome, RawInvariant> {
        let (wait_foreign, report_epoch, category, code) = {
            let rt0 = self.rt0_mut()?;
            if rt0.terminated {
                return Err(RawInvariant::new("终止只能执行一次"));
            }
            let Some(plan) = &rt0.plan else {
                return Err(RawInvariant::new("没有终止计划"));
            };
            if rt0.lifecycle.state() != LifecycleStateName::Terminating {
                return Err(RawInvariant::new("终止计划只能在 Terminating 中执行"));
            }
            (
                plan.wait_foreign(),
                plan.report_epoch(),
                plan.exit_category(),
                plan.exit_code(),
            )
        };
        // producer flush：排空全部 owner inbox 并越过最新 epoch。
        for owner in 0..self.owners.len() {
            let owner = u32::try_from(owner).expect("owner 下标适配 u32");
            self.drain_all(owner, budget)?;
        }
        self.release_graced_nodes()?;
        self.epoch = self.epoch.next();
        self.shutdown_stacks()?;
        let rt0 = self.rt0_mut()?;
        if wait_foreign {
            // 模型中等待是确定性的：外部工作在收尾前全部完成。
            rt0.foreign_work = 0;
        }
        for facility in ShutdownFacility::all() {
            let expected = ShutdownFacility::all()[rt0.facilities_closed];
            if facility != expected {
                return Err(RawInvariant::new("设施关闭顺序违反固定顺序"));
            }
            rt0.facilities_closed += 1;
        }
        if rt0.ledger.next_epoch() < report_epoch {
            return Err(RawInvariant::new("终止报告尚未冲刷到计划下界"));
        }
        rt0.terminated = true;
        rt0.outcome = Some(TerminationOutcome { category, code });
        Ok(TerminationOutcome { category, code })
    }

    /// 返回启动快照的宿主并行度（测试与诊断视图）。
    pub(crate) fn rt0_host_parallelism(&self) -> Result<u32, RawInvariant> {
        Ok(self.rt0_ref()?.snapshot.host_parallelism())
    }

    /// 返回平台的 profile；信号退出码按目标语义解析。
    pub(crate) const fn rt0_profile(&self) -> PlatformProfile {
        self.provider.profile()
    }
}
