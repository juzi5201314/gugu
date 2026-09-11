//! 进程生命周期状态机与 rt0 启动序列的确定性参照实现。
//!
//! 状态只沿 `LifecycleTransition::all()` 登记的方向迁移；不在表中的迁移一律拒绝。
//! 启动序列按 `Rt0Step::all()` 的固定顺序推进，配置非法时不进入后续步骤。

use super::startup::{self, EnvironmentSnapshot, StartupConfig, StartupError};
use super::startup_schema::{LifecycleStateName, LifecycleTransition, Rt0Step};

/// 生命周期状态机。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Lifecycle {
    state: LifecycleStateName,
}

impl Lifecycle {
    /// rt0 开始执行时进程处于 `Booting`。
    pub(crate) const fn new() -> Self {
        Self {
            state: LifecycleStateName::Booting,
        }
    }

    /// 返回当前状态。
    pub(crate) const fn state(&self) -> LifecycleStateName {
        self.state
    }

    /// 按登记的迁移表推进状态；非法迁移返回错误且不改变状态。
    pub(crate) fn transition(
        &mut self,
        to: LifecycleStateName,
        trigger: &str,
    ) -> Result<(), String> {
        let allowed = LifecycleTransition::all()
            .iter()
            .any(|entry| entry.from == self.state && entry.to == to && entry.trigger == trigger);
        if !allowed {
            return Err(format!(
                "不允许的生命周期迁移 {} -> {} on {trigger}",
                self.state.name(),
                to.name()
            ));
        }
        self.state = to;
        Ok(())
    }

    /// 当前状态是否允许运行用户代码；`Booting` 与 `Terminating` 都不允许。
    pub(crate) const fn user_code_allowed(&self) -> bool {
        matches!(
            self.state,
            LifecycleStateName::Running | LifecycleStateName::Waiting
        )
    }

    /// 当前状态是否接纳新的用户协程；`Booting` 不运行用户函数，`Terminating` 不再启动。
    pub(crate) const fn admission_open(&self) -> bool {
        self.user_code_allowed()
    }
}

/// rt0 启动序列记录；步骤顺序即 `Rt0Step::all()`。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BootSequence {
    steps: Vec<Rt0Step>,
}

impl BootSequence {
    /// 创建空的启动序列记录。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 返回已完成的步骤。
    pub(crate) fn steps(&self) -> &[Rt0Step] {
        &self.steps
    }

    /// 执行前四步：固定快照、解析配置、建立运行环境、发布 `Running`。
    ///
    /// 快照由 `EnvironmentSnapshot::fix` 在进入本函数前固定；配置非法时只完成前两步，
    /// 由调用方进入 `InvalidConfiguration` fatal。外层错误是模型不变量，不属于配置解析。
    pub(crate) fn start(
        &mut self,
        lifecycle: &mut Lifecycle,
        snapshot: &EnvironmentSnapshot,
    ) -> Result<Result<StartupConfig, Vec<StartupError>>, String> {
        self.steps.push(Rt0Step::FixSnapshot);
        self.steps.push(Rt0Step::ParseConfig);
        let config = match startup::parse(snapshot) {
            Ok(config) => config,
            Err(errors) => return Ok(Err(errors)),
        };
        self.steps.push(Rt0Step::EstablishRuntime);
        lifecycle.transition(LifecycleStateName::Running, "runtime-established")?;
        self.steps.push(Rt0Step::PublishRunning);
        Ok(Ok(config))
    }

    /// 第五步：调用编译器已解析的 `main` 入口前记录。
    pub(crate) fn call_main(&mut self) {
        self.steps.push(Rt0Step::CallMain);
    }
}
