//! rt0 环境快照与启动配置解析的确定性参照实现。
//!
//! 快照在 `Booting` 中固定一次；配置解析按 `StartupVar::all()` 的 canonical 顺序消费
//! 全部 7 个变量，收集所有错误并按该顺序报告首个错误。解析规则与
//! `docs/src/spec/runtime.md` 的启动配置表一一对应，非法值都属于
//! `InvalidConfiguration` fatal。

use super::startup_schema::{StartupVar, ValueGrammar};

/// 环境快照：argv、环境变量、初始工作目录与启动时探测的宿主并行度。
///
/// 快照一旦创建就不再随宿主变化重新读取；`std.env.set`/`remove` 只改变宿主快照，
/// 不会重新配置 runtime。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnvironmentSnapshot {
    argv: Vec<String>,
    env: Vec<(String, String)>,
    working_directory: String,
    host_parallelism: u32,
}

impl EnvironmentSnapshot {
    /// 固定环境快照；环境变量按名字稳定排序，并行度探测失败回退为 1。
    pub(crate) fn fix(
        argv: Vec<String>,
        mut env: Vec<(String, String)>,
        working_directory: String,
        host_parallelism: u32,
    ) -> Self {
        env.sort_by(|left, right| left.0.cmp(&right.0));
        Self {
            argv,
            env,
            working_directory,
            host_parallelism: host_parallelism.max(1),
        }
    }

    /// 返回 argv 快照。
    pub(crate) fn argv(&self) -> &[String] {
        &self.argv
    }

    /// 返回环境变量快照。
    pub(crate) fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// 返回初始工作目录。
    pub(crate) fn working_directory(&self) -> &str {
        &self.working_directory
    }

    /// 返回启动时探测到的宿主并行度（至少 1）。
    pub(crate) const fn host_parallelism(&self) -> u32 {
        self.host_parallelism
    }

    fn lookup(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// 自动 GC 的堆增长目标配置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GcTargetConfig {
    /// 相对上次存活堆允许增长的百分数。
    Automatic(u32),
    /// 只关闭按堆增长触发的自动周期。
    Off,
}

/// fatal、`main` Err 与未处理 panic 的报告格式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticsFormat {
    /// 人类可读文本。
    Text,
    /// 稳定的逐行 JSON。
    Json,
    /// 依次输出文本与 JSON。
    Both,
}

/// 报告中包含的回溯范围。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BacktraceMode {
    /// 不输出回溯。
    Off,
    /// 输出触发故障的协程回溯。
    Triggering,
    /// 在可取得时输出全部协程与工作线程回溯。
    Full,
}

/// runtime trace 的类别开关；重复类别合并。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TraceSet {
    scheduler: bool,
    gc: bool,
    signal: bool,
    panic: bool,
}

impl TraceSet {
    /// 返回指定类别是否打开。
    pub(crate) const fn is_open(&self, category: super::startup_schema::TraceCategory) -> bool {
        match category {
            super::startup_schema::TraceCategory::Scheduler => self.scheduler,
            super::startup_schema::TraceCategory::Gc => self.gc,
            super::startup_schema::TraceCategory::Signal => self.signal,
            super::startup_schema::TraceCategory::Panic => self.panic,
        }
    }
}

/// 解析后的启动配置；动态 API 的设置在后续阶段覆盖启动值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StartupConfig {
    parallelism: u32,
    gc_target: GcTargetConfig,
    memory_limit: Option<u64>,
    stack_max: u64,
    trace: TraceSet,
    diagnostics: DiagnosticsFormat,
    backtrace: BacktraceMode,
}

impl StartupConfig {
    /// 返回初始并行度目标。
    pub(crate) const fn parallelism(&self) -> u32 {
        self.parallelism
    }

    /// 返回自动 GC 目标。
    pub(crate) const fn gc_target(&self) -> GcTargetConfig {
        self.gc_target
    }

    /// 返回 runtime 管理内存的软上限。
    pub(crate) const fn memory_limit(&self) -> Option<u64> {
        self.memory_limit
    }

    /// 返回每用户协程的逻辑栈上限。
    pub(crate) const fn stack_max(&self) -> u64 {
        self.stack_max
    }

    /// 返回 trace 类别开关。
    pub(crate) const fn trace(&self) -> &TraceSet {
        &self.trace
    }

    /// 返回报告格式。
    pub(crate) const fn diagnostics(&self) -> DiagnosticsFormat {
        self.diagnostics
    }

    /// 返回回溯范围。
    pub(crate) const fn backtrace(&self) -> BacktraceMode {
        self.backtrace
    }
}

/// 一个启动变量的解析失败；`detail` 面向 fatal 报告的 message。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartupError {
    variable: &'static str,
    detail: String,
}

impl StartupError {
    /// 返回变量名。
    pub(crate) const fn variable(&self) -> &'static str {
        self.variable
    }

    /// 返回失败说明。
    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }
}

/// `GUGU_RUNTIME_STACK_MAX` 的下界：64 KiB。
const STACK_MAX_MIN: u64 = 64 * 1024;

/// 解析全部启动变量；存在错误时返回按 canonical 顺序排列的错误列表。
pub(crate) fn parse(snapshot: &EnvironmentSnapshot) -> Result<StartupConfig, Vec<StartupError>> {
    let mut errors = Vec::new();
    let parallelism = parse_var(snapshot, ValueGrammar::PositiveDecimal)
        .and_then(|text| parse_parallelism(&text, &mut errors));
    let gc_target = parse_var(snapshot, ValueGrammar::GcTargetPercent)
        .and_then(|text| parse_gc_target(&text, &mut errors));
    let memory_limit = parse_var(snapshot, ValueGrammar::ByteQuantity)
        .and_then(|text| parse_memory_limit(&text, &mut errors));
    let stack_max = parse_var(snapshot, ValueGrammar::StackMaxQuantity)
        .and_then(|text| parse_stack_max(&text, &mut errors));
    let trace = parse_var(snapshot, ValueGrammar::TraceSelection)
        .and_then(|text| parse_trace(&text, &mut errors));
    let diagnostics = parse_var(snapshot, ValueGrammar::DiagnosticsFormat)
        .and_then(|text| parse_diagnostics(&text, &mut errors));
    let backtrace = parse_var(snapshot, ValueGrammar::BacktraceMode)
        .and_then(|text| parse_backtrace(&text, &mut errors));
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(StartupConfig {
        parallelism: parallelism.unwrap_or(snapshot.host_parallelism()),
        gc_target: gc_target.unwrap_or(GcTargetConfig::Automatic(100)),
        memory_limit: memory_limit.unwrap_or(None),
        stack_max: stack_max.unwrap_or(1024 * 1024 * 1024),
        trace: trace.unwrap_or_default(),
        diagnostics: diagnostics.unwrap_or(DiagnosticsFormat::Text),
        backtrace: backtrace.unwrap_or(BacktraceMode::Triggering),
    })
}

/// 诊断配置本身非法时必须使用不依赖该配置的固定纯文本 emergency report。
pub(crate) fn needs_emergency_report(errors: &[StartupError]) -> bool {
    errors.iter().any(|error| {
        matches!(
            error.variable(),
            "GUGU_RUNTIME_DIAGNOSTICS" | "GUGU_BACKTRACE"
        )
    })
}

/// 独立解析报告格式；`GUGU_RUNTIME_DIAGNOSTICS` 非法时返回 `None`。
pub(crate) fn parse_diagnostics_var(snapshot: &EnvironmentSnapshot) -> Option<DiagnosticsFormat> {
    let text = snapshot.lookup("GUGU_RUNTIME_DIAGNOSTICS")?;
    let mut sink = Vec::new();
    parse_diagnostics(text, &mut sink)
}

/// 独立解析回溯范围；`GUGU_BACKTRACE` 非法时返回 `None`。
pub(crate) fn parse_backtrace_var(snapshot: &EnvironmentSnapshot) -> Option<BacktraceMode> {
    let text = snapshot.lookup("GUGU_BACKTRACE")?;
    let mut sink = Vec::new();
    parse_backtrace(text, &mut sink)
}

fn parse_var(snapshot: &EnvironmentSnapshot, grammar: ValueGrammar) -> Option<String> {
    StartupVar::all()
        .into_iter()
        .find(|var| var.grammar == grammar)
        .and_then(|var| snapshot.lookup(&var.name).map(str::to_owned))
}

fn push_error(errors: &mut Vec<StartupError>, variable: &'static str, detail: String) {
    if !errors.iter().any(|error| error.variable() == variable) {
        errors.push(StartupError { variable, detail });
    }
}

fn parse_parallelism(text: &str, errors: &mut Vec<StartupError>) -> Option<u32> {
    let variable = "GUGU_RUNTIME_PROCS";
    match parse_decimal(text) {
        Ok(value) => match u32::try_from(value) {
            Ok(0) => {
                push_error(errors, variable, "GUGU_RUNTIME_PROCS 不允许 0".to_owned());
                None
            }
            Ok(value) => Some(value),
            Err(_) => {
                push_error(errors, variable, "并行度超出 32 位范围".to_owned());
                None
            }
        },
        Err(detail) => {
            push_error(errors, variable, detail);
            None
        }
    }
}

fn parse_gc_target(text: &str, errors: &mut Vec<StartupError>) -> Option<GcTargetConfig> {
    let variable = "GUGU_RUNTIME_GC_TARGET";
    if text == "off" {
        return Some(GcTargetConfig::Off);
    }
    match parse_decimal(text) {
        Ok(value) => match u32::try_from(value) {
            Ok(percent) => Some(GcTargetConfig::Automatic(percent)),
            Err(_) => {
                push_error(errors, variable, "百分数超出 32 位范围".to_owned());
                None
            }
        },
        Err(detail) => {
            push_error(errors, variable, detail);
            None
        }
    }
}

fn parse_memory_limit(text: &str, errors: &mut Vec<StartupError>) -> Option<Option<u64>> {
    let variable = "GUGU_RUNTIME_MEMORY_LIMIT";
    if text == "off" {
        return Some(None);
    }
    match parse_byte_quantity(text) {
        Ok(bytes) => Some(Some(bytes)),
        Err(detail) => {
            push_error(errors, variable, detail);
            None
        }
    }
}

fn parse_stack_max(text: &str, errors: &mut Vec<StartupError>) -> Option<u64> {
    let variable = "GUGU_RUNTIME_STACK_MAX";
    match parse_byte_quantity(text) {
        Ok(bytes) if bytes < STACK_MAX_MIN => {
            push_error(errors, variable, "栈上限不能低于 64KiB".to_owned());
            None
        }
        Ok(bytes) if bytes > isize::MAX as u64 => {
            push_error(errors, variable, "栈上限超出目标 isize 范围".to_owned());
            None
        }
        Ok(bytes) => Some(bytes),
        Err(detail) => {
            push_error(errors, variable, detail);
            None
        }
    }
}

fn parse_trace(text: &str, errors: &mut Vec<StartupError>) -> Option<TraceSet> {
    use super::startup_schema::TraceCategory;
    let variable = "GUGU_RUNTIME_TRACE";
    if text == "off" {
        return Some(TraceSet::default());
    }
    if text == "all" {
        return Some(TraceSet {
            scheduler: true,
            gc: true,
            signal: true,
            panic: true,
        });
    }
    let mut set = TraceSet::default();
    for item in text.split(',') {
        let category = match item {
            "scheduler" => TraceCategory::Scheduler,
            "gc" => TraceCategory::Gc,
            "signal" => TraceCategory::Signal,
            "panic" => TraceCategory::Panic,
            "" => {
                push_error(errors, variable, "trace 类别不能为空".to_owned());
                return None;
            }
            other => {
                push_error(errors, variable, format!("未知 trace 类别 `{other}`"));
                return None;
            }
        };
        match category {
            TraceCategory::Scheduler => set.scheduler = true,
            TraceCategory::Gc => set.gc = true,
            TraceCategory::Signal => set.signal = true,
            TraceCategory::Panic => set.panic = true,
        }
    }
    Some(set)
}

fn parse_diagnostics(text: &str, errors: &mut Vec<StartupError>) -> Option<DiagnosticsFormat> {
    let variable = "GUGU_RUNTIME_DIAGNOSTICS";
    match text {
        "text" => Some(DiagnosticsFormat::Text),
        "json" => Some(DiagnosticsFormat::Json),
        "both" => Some(DiagnosticsFormat::Both),
        other => {
            push_error(errors, variable, format!("非法报告格式 `{other}`"));
            None
        }
    }
}

fn parse_backtrace(text: &str, errors: &mut Vec<StartupError>) -> Option<BacktraceMode> {
    let variable = "GUGU_BACKTRACE";
    match text {
        "0" => Some(BacktraceMode::Off),
        "1" => Some(BacktraceMode::Triggering),
        "full" => Some(BacktraceMode::Full),
        other => {
            push_error(errors, variable, format!("非法回溯范围 `{other}`"));
            None
        }
    }
}

/// 解析十进制无符号整数；拒绝空串、符号、空白与其它拼写。
fn parse_decimal(text: &str) -> Result<u64, String> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("`{text}` 不是十进制无符号整数"));
    }
    text.parse::<u64>()
        .map_err(|_| format!("`{text}` 超出无符号整数范围"))
}

/// 解析字节量：十进制无符号整数加可选 `B`/`KiB`/`MiB`/`GiB`/`TiB` 后缀。
///
/// 乘法溢出、零值、未知后缀和其它拼写都返回错误。
fn parse_byte_quantity(text: &str) -> Result<u64, String> {
    let (digits, unit) = split_byte_quantity(text)?;
    let value = parse_decimal(digits)?;
    let multiplier = unit.multiplier();
    value
        .checked_mul(multiplier)
        .filter(|bytes| *bytes != 0)
        .ok_or_else(|| {
            if value == 0 {
                "字节量不允许 0".to_owned()
            } else {
                format!("`{text}` 的字节量乘法溢出")
            }
        })
}

fn split_byte_quantity(text: &str) -> Result<(&str, super::startup_schema::ByteUnit), String> {
    use super::startup_schema::ByteUnit;
    let split_at = text
        .find(|byte: char| !byte.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, suffix) = text.split_at(split_at);
    let unit = match suffix {
        "" | "B" => ByteUnit::B,
        "KiB" => ByteUnit::KiB,
        "MiB" => ByteUnit::MiB,
        "GiB" => ByteUnit::GiB,
        "TiB" => ByteUnit::TiB,
        other => return Err(format!("未知字节量后缀 `{other}`")),
    };
    Ok((digits, unit))
}
