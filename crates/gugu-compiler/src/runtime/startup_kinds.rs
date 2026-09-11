//! rt0 启动、终止与报告契约的共享目录与枚举。
//!
//! 这里固定启动步骤、生命周期状态与迁移、环境快照字段、启动变量文法、字节量单位、
//! trace 类别、fatal 目录、退出类别与码规则、信号目录、报告事件/reason、终止模式与
//! 关闭设施顺序。`startup`/`lifecycle`/`report`/`termination` 参照实现消费同一组枚举，
//! 不建立平行表示；契约段 [`Rt0SchemaV1`](super::startup_schema) 按这些目录编码。

use serde::{Deserialize, Serialize};

/// 报告 JSON 的 schema 名；输出字段与顺序按规范固定。
pub(crate) const REPORT_SCHEMA_NAME: &str = "gugu-runtime-report-v1";

/// emergency report 缓冲区容量；报告路径不使用普通分配器。
pub(crate) const EMERGENCY_BUFFER_BYTES: u32 = 4096;

/// emergency buffer 的容量下限；低于它无法保证必填字段与合法 JSON。
pub(crate) const EMERGENCY_BUFFER_MIN_BYTES: u32 = 256;

/// rt0 启动序列；顺序即执行顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum Rt0Step {
    /// 固定 argv、环境与初始工作目录快照。
    FixSnapshot = 0,
    /// 解析全部启动配置；非法配置按 `InvalidConfiguration` fatal 结束。
    ParseConfig = 1,
    /// 建立分配、协程、panic、信号转交与 runtime API 的运行环境。
    EstablishRuntime = 2,
    /// 把状态从 `Booting` 发布为 `Running`。
    PublishRunning = 3,
    /// 调用编译器已解析的 `main` 入口。
    CallMain = 4,
}

impl Rt0Step {
    /// 返回步骤的稳定名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::FixSnapshot => "fix-snapshot",
            Self::ParseConfig => "parse-config",
            Self::EstablishRuntime => "establish-runtime",
            Self::PublishRunning => "publish-running",
            Self::CallMain => "call-main",
        }
    }

    /// 返回全部步骤；顺序即 rt0 的固定执行顺序。
    pub(crate) const fn all() -> [Self; 5] {
        [
            Self::FixSnapshot,
            Self::ParseConfig,
            Self::EstablishRuntime,
            Self::PublishRunning,
            Self::CallMain,
        ]
    }
}

/// 进程生命周期状态；一次运行只沿单向迁移。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum LifecycleStateName {
    /// rt0 开始执行，尚未调用用户代码。
    Booting = 0,
    /// runtime 已建立普通分配、调度、GC 与 panic 边界。
    Running = 1,
    /// `main` 已返回，仍有用户协程存活。
    Waiting = 2,
    /// 终止收尾开始；不再启动用户代码。
    Terminating = 3,
}

impl LifecycleStateName {
    /// 返回状态的稳定名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Booting => "booting",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Terminating => "terminating",
        }
    }

    /// 返回全部状态。
    pub(crate) const fn all() -> [Self; 4] {
        [
            Self::Booting,
            Self::Running,
            Self::Waiting,
            Self::Terminating,
        ]
    }
}

/// 一条允许的生命周期迁移；不在表中的迁移一律拒绝。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LifecycleTransition {
    pub(crate) from: LifecycleStateName,
    pub(crate) to: LifecycleStateName,
    pub(crate) trigger: String,
}

/// 迁移表的原始登记；`(from, to, trigger)` 三元组。
const LIFECYCLE_TRANSITIONS: [(LifecycleStateName, LifecycleStateName, &str); 6] = [
    (
        LifecycleStateName::Booting,
        LifecycleStateName::Running,
        "runtime-established",
    ),
    (
        LifecycleStateName::Running,
        LifecycleStateName::Waiting,
        "main-returned",
    ),
    (
        LifecycleStateName::Running,
        LifecycleStateName::Terminating,
        "natural-exit",
    ),
    (
        LifecycleStateName::Booting,
        LifecycleStateName::Terminating,
        "termination-started",
    ),
    (
        LifecycleStateName::Running,
        LifecycleStateName::Terminating,
        "termination-started",
    ),
    (
        LifecycleStateName::Waiting,
        LifecycleStateName::Terminating,
        "termination-started",
    ),
];

impl LifecycleTransition {
    /// 返回全部允许的迁移；顺序即契约登记顺序。
    ///
    /// `main` 返回且仍有用户协程存活时进入 `Waiting`；没有存活协程时直接进入
    /// `Terminating` 自然收尾。fatal、显式退出与主协程 panic 都以
    /// `termination-started` 触发迁移。
    pub(crate) fn all() -> Vec<Self> {
        LIFECYCLE_TRANSITIONS
            .iter()
            .map(|(from, to, trigger)| Self {
                from: *from,
                to: *to,
                trigger: (*trigger).to_owned(),
            })
            .collect()
    }
}

/// 环境快照字段；全部在 rt0 第一步固定一次。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum EnvSnapshotField {
    /// 进程参数向量。
    Argv = 0,
    /// 进程环境变量。
    Environment = 1,
    /// 初始工作目录。
    WorkingDirectory = 2,
}

impl EnvSnapshotField {
    /// 返回字段的稳定名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Argv => "argv",
            Self::Environment => "environment",
            Self::WorkingDirectory => "working-directory",
        }
    }
}

/// 启动变量的取值文法。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum ValueGrammar {
    /// 大于 0 的十进制整数。
    PositiveDecimal = 0,
    /// `off` 或十进制百分数。
    GcTargetPercent = 1,
    /// `off` 或无空白字节量。
    ByteQuantity = 2,
    /// `64KiB` 至目标 `isize::MAX` 的字节量。
    StackMaxQuantity = 3,
    /// `off`、`all` 或逗号分隔的 trace 类别。
    TraceSelection = 4,
    /// `text`、`json` 或 `both`。
    DiagnosticsFormat = 5,
    /// `0`、`1` 或 `full`。
    BacktraceMode = 6,
}

impl ValueGrammar {
    /// 返回文法的稳定名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::PositiveDecimal => "positive-decimal",
            Self::GcTargetPercent => "gc-target",
            Self::ByteQuantity => "byte-quantity",
            Self::StackMaxQuantity => "stack-max",
            Self::TraceSelection => "trace-selection",
            Self::DiagnosticsFormat => "diagnostics-format",
            Self::BacktraceMode => "backtrace-mode",
        }
    }
}

/// 一个启动变量的契约登记项；顺序即解析与首错报告的固定顺序。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct StartupVar {
    pub(crate) name: String,
    pub(crate) grammar: ValueGrammar,
    pub(crate) default: String,
}

/// 启动变量的原始登记；`(name, grammar, default)` 三元组。
const STARTUP_VARS: [(&str, ValueGrammar, &str); 7] = [
    (
        "GUGU_RUNTIME_PROCS",
        ValueGrammar::PositiveDecimal,
        "available-parallelism",
    ),
    (
        "GUGU_RUNTIME_GC_TARGET",
        ValueGrammar::GcTargetPercent,
        "100",
    ),
    (
        "GUGU_RUNTIME_MEMORY_LIMIT",
        ValueGrammar::ByteQuantity,
        "off",
    ),
    (
        "GUGU_RUNTIME_STACK_MAX",
        ValueGrammar::StackMaxQuantity,
        "1GiB",
    ),
    ("GUGU_RUNTIME_TRACE", ValueGrammar::TraceSelection, "off"),
    (
        "GUGU_RUNTIME_DIAGNOSTICS",
        ValueGrammar::DiagnosticsFormat,
        "text",
    ),
    ("GUGU_BACKTRACE", ValueGrammar::BacktraceMode, "1"),
];

impl StartupVar {
    /// 返回全部启动变量；顺序即 canonical 解析顺序。
    pub(crate) fn all() -> Vec<Self> {
        STARTUP_VARS
            .iter()
            .map(|(name, grammar, default)| Self {
                name: (*name).to_owned(),
                grammar: *grammar,
                default: (*default).to_owned(),
            })
            .collect()
    }
}

/// 字节量的单位后缀；乘数是 1024 的幂。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum ByteUnit {
    /// 无后缀或 `B`。
    B = 0,
    /// `KiB`。
    KiB = 1,
    /// `MiB`。
    MiB = 2,
    /// `GiB`。
    GiB = 3,
    /// `TiB`。
    TiB = 4,
}

impl ByteUnit {
    /// 返回单位名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::B => "B",
            Self::KiB => "KiB",
            Self::MiB => "MiB",
            Self::GiB => "GiB",
            Self::TiB => "TiB",
        }
    }

    /// 返回乘数。
    pub(crate) const fn multiplier(self) -> u64 {
        const KIB: u64 = 1024;
        match self {
            Self::B => 1,
            Self::KiB => KIB,
            Self::MiB => KIB * KIB,
            Self::GiB => KIB * KIB * KIB,
            Self::TiB => KIB * KIB * KIB * KIB,
        }
    }
}

/// runtime trace 的类别；重复项合并，`all` 等价于四类全开。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum TraceCategory {
    /// 调度事件。
    Scheduler = 0,
    /// GC 事件。
    Gc = 1,
    /// 信号事件。
    Signal = 2,
    /// panic 事件。
    Panic = 3,
}

impl TraceCategory {
    /// 返回类别名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Scheduler => "scheduler",
            Self::Gc => "gc",
            Self::Signal => "signal",
            Self::Panic => "panic",
        }
    }
}

/// fatal 分类；不能被 `catch`、`Join.wait` 或用户 defer 截获。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum FatalKind {
    /// runtime 管理内存无法满足分配；先完成一次可行的 GC 重试。
    OutOfMemory = 0,
    /// 逻辑栈超过 `GUGU_RUNTIME_STACK_MAX`；不展开用户栈。
    StackOverflow = 1,
    /// 内部不变量被违反；报告带实现诊断。
    RuntimeInvariant = 2,
    /// 外部异常越过 Gugu 边界；立即停止跨边界展开。
    ForeignUnwind = 3,
    /// panic 展开或 fatal 报告期间再次进入不可恢复 panic。
    PanicDuringUnwind = 4,
    /// 不可安全转换的同步硬件 fault；best-effort 报告后恢复宿主默认终止。
    HardwareFault = 5,
    /// 启动环境变量格式、范围或目标组合非法。
    InvalidConfiguration = 6,
}

impl FatalKind {
    /// 返回 fatal 的稳定显示名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::OutOfMemory => "OutOfMemory",
            Self::StackOverflow => "StackOverflow",
            Self::RuntimeInvariant => "RuntimeInvariant",
            Self::ForeignUnwind => "ForeignUnwind",
            Self::PanicDuringUnwind => "PanicDuringUnwind",
            Self::HardwareFault => "HardwareFault",
            Self::InvalidConfiguration => "InvalidConfiguration",
        }
    }

    /// 返回报告使用的 reason 短名。
    pub(crate) const fn reason(self) -> ReportReason {
        match self {
            Self::OutOfMemory => ReportReason::OutOfMemory,
            Self::StackOverflow => ReportReason::StackOverflow,
            Self::RuntimeInvariant => ReportReason::RuntimeInvariant,
            Self::ForeignUnwind => ReportReason::ForeignUnwind,
            Self::PanicDuringUnwind => ReportReason::PanicDuringUnwind,
            Self::HardwareFault => ReportReason::HardwareFault,
            Self::InvalidConfiguration => ReportReason::InvalidConfiguration,
        }
    }

    /// 返回全部 fatal 类别。
    pub(crate) const fn all() -> [Self; 7] {
        [
            Self::OutOfMemory,
            Self::StackOverflow,
            Self::RuntimeInvariant,
            Self::ForeignUnwind,
            Self::PanicDuringUnwind,
            Self::HardwareFault,
            Self::InvalidConfiguration,
        ]
    }
}

/// 逻辑退出类别；报告 `class` 与退出码规则共用该目录。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum ExitCategory {
    /// `main` 成功且所有用户协程完成。
    Success = 0,
    /// `main` 返回 `Err`、未处理 panic 或测试/运行器报告失败。
    ProgramFailure = 1,
    /// OOM、栈溢出、runtime invariant、外部 unwind、硬件 fault 或无效启动配置。
    RuntimeFailure = 2,
    /// `std.process.exit(code)`。
    ExplicitExit = 3,
    /// 未被订阅的普通终止信号导致宿主终止。
    Signal = 4,
}

impl ExitCategory {
    /// 返回类别的稳定名；与报告 JSON 的 `class` 取值一致。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ProgramFailure => "program-failure",
            Self::RuntimeFailure => "runtime-failure",
            Self::ExplicitExit => "explicit-exit",
            Self::Signal => "signal",
        }
    }

    /// 返回 Gugu 约定退出码规则。
    pub(crate) const fn code_rule(self) -> ExitCodeRule {
        match self {
            Self::Success => ExitCodeRule::Fixed(0),
            Self::ProgramFailure => ExitCodeRule::Fixed(1),
            Self::RuntimeFailure => ExitCodeRule::Fixed(2),
            Self::ExplicitExit => ExitCodeRule::Caller,
            Self::Signal => ExitCodeRule::TargetSignal,
        }
    }

    /// 返回全部类别。
    pub(crate) const fn all() -> [Self; 5] {
        [
            Self::Success,
            Self::ProgramFailure,
            Self::RuntimeFailure,
            Self::ExplicitExit,
            Self::Signal,
        ]
    }
}

/// 退出码规则。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum ExitCodeRule {
    /// 固定值。
    Fixed(u32) = 0,
    /// 使用调用方给出的 code；整数转换由目标 `std.process` 规则定义。
    Caller = 1,
    /// 按目标信号状态解析；Linux 为 `128 + 信号号`，Windows 为登记的非零 status。
    TargetSignal = 2,
}

/// 可订阅的跨目标信号；信号报告的 reason 与退出码解析都按它登记。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum GuguSignal {
    /// Linux `SIGINT` / Windows console Ctrl+C。
    Interrupt = 0,
    /// Linux `SIGTERM` / Windows 终止通知。
    Terminate = 1,
    /// Linux `SIGHUP`；Windows 不提供。
    Hangup = 2,
    /// Linux `SIGUSR1`；Windows 不提供。
    User1 = 3,
    /// Linux `SIGUSR2`；Windows 不提供。
    User2 = 4,
    /// Windows console Ctrl+Break；Linux 不提供。
    Break = 5,
}

impl GuguSignal {
    /// 返回 Linux 信号号；目标不提供时为 `None`。
    pub(crate) const fn linux_number(self) -> Option<u16> {
        match self {
            Self::Interrupt => Some(2),
            Self::Terminate => Some(15),
            Self::Hangup => Some(1),
            Self::User1 => Some(10),
            Self::User2 => Some(12),
            Self::Break => None,
        }
    }

    /// 返回 Windows 登记的非零 status；宿主决定或目标不提供时为 `None`。
    pub(crate) const fn windows_status(self) -> Option<u32> {
        match self {
            // Ctrl+C 与 Ctrl+Break 的控制台退出 status。
            Self::Interrupt | Self::Break => Some(0xC000_013A),
            Self::Terminate | Self::Hangup | Self::User1 | Self::User2 => None,
        }
    }
}

/// 报告事件的种类。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum ReportEvent {
    /// 未处理 panic 的事件。
    Panic = 0,
    /// 进程终止的事件。
    Termination = 1,
}

impl ReportEvent {
    /// 返回事件的稳定名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Panic => "panic",
            Self::Termination => "termination",
        }
    }
}

/// 报告 reason；稳定的小写短名清单。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum ReportReason {
    /// `main` 返回 `Err`。
    MainError = 0,
    /// 未处理 panic（分离协程或主协程）。
    UnhandledPanic = 1,
    /// OOM fatal。
    OutOfMemory = 2,
    /// 栈溢出 fatal。
    StackOverflow = 3,
    /// 内部不变量 fatal。
    RuntimeInvariant = 4,
    /// 外部 unwind fatal。
    ForeignUnwind = 5,
    /// 展开期间再入 panic 的 fatal。
    PanicDuringUnwind = 6,
    /// 硬件 fault fatal。
    HardwareFault = 7,
    /// 非法启动配置 fatal。
    InvalidConfiguration = 8,
    /// 未被订阅的终止信号。
    SignalTerminate = 9,
    /// 未被订阅的 `SIGINT`。
    SignalInterrupt = 10,
    /// 未被订阅的 `SIGHUP`。
    SignalHangup = 11,
    /// 未被订阅的 `SIGUSR1`。
    SignalUser1 = 12,
    /// 未被订阅的 `SIGUSR2`。
    SignalUser2 = 13,
    /// 未被订阅的 Ctrl+Break。
    SignalBreak = 14,
}

impl ReportReason {
    /// 返回 reason 的稳定短名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::MainError => "main-error",
            Self::UnhandledPanic => "unhandled-panic",
            Self::OutOfMemory => "out-of-memory",
            Self::StackOverflow => "stack-overflow",
            Self::RuntimeInvariant => "runtime-invariant",
            Self::ForeignUnwind => "foreign-unwind",
            Self::PanicDuringUnwind => "panic-during-unwind",
            Self::HardwareFault => "hardware-fault",
            Self::InvalidConfiguration => "invalid-configuration",
            Self::SignalTerminate => "signal-terminate",
            Self::SignalInterrupt => "signal-interrupt",
            Self::SignalHangup => "signal-hangup",
            Self::SignalUser1 => "signal-user1",
            Self::SignalUser2 => "signal-user2",
            Self::SignalBreak => "signal-break",
        }
    }

    /// 信号 reason 对应的 Gugu 信号；非信号 reason 返回 `None`。
    pub(crate) const fn signal(self) -> Option<GuguSignal> {
        match self {
            Self::SignalTerminate => Some(GuguSignal::Terminate),
            Self::SignalInterrupt => Some(GuguSignal::Interrupt),
            Self::SignalHangup => Some(GuguSignal::Hangup),
            Self::SignalUser1 => Some(GuguSignal::User1),
            Self::SignalUser2 => Some(GuguSignal::User2),
            Self::SignalBreak => Some(GuguSignal::Break),
            _ => None,
        }
    }

    /// 返回全部 reason；顺序即契约登记顺序。
    pub(crate) const fn all() -> [Self; 15] {
        [
            Self::MainError,
            Self::UnhandledPanic,
            Self::OutOfMemory,
            Self::StackOverflow,
            Self::RuntimeInvariant,
            Self::ForeignUnwind,
            Self::PanicDuringUnwind,
            Self::HardwareFault,
            Self::InvalidConfiguration,
            Self::SignalTerminate,
            Self::SignalInterrupt,
            Self::SignalHangup,
            Self::SignalUser1,
            Self::SignalUser2,
            Self::SignalBreak,
        ]
    }
}

/// 报告 JSON 的字段清单；顺序即 NDJSON 的字段输出顺序。
pub(crate) const REPORT_FIELDS: [&str; 8] = [
    "schema",
    "event",
    "class",
    "reason",
    "message",
    "location",
    "backtrace",
    "exit_code",
];

/// `TerminationPlan` 的调度模式。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum TerminationMode {
    /// 自然收尾：等待用户协程结束，dirty work 由 `wait_foreign` 决定。
    Natural = 0,
    /// 立即收尾：`main` panic 等不再等待其余协程的路径。
    Immediate = 1,
    /// `std.process.exit(code)`。
    ExplicitExit = 2,
    /// runtime fatal；不等待、不展开。
    Fatal = 3,
    /// 未被订阅的终止信号。
    Signal = 4,
}

impl TerminationMode {
    /// 返回模式的稳定名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Natural => "natural",
            Self::Immediate => "immediate",
            Self::ExplicitExit => "explicit-exit",
            Self::Fatal => "fatal",
            Self::Signal => "signal",
        }
    }

    /// 返回全部模式。
    pub(crate) const fn all() -> [Self; 5] {
        [
            Self::Natural,
            Self::Immediate,
            Self::ExplicitExit,
            Self::Fatal,
            Self::Signal,
        ]
    }
}

/// `TerminationPlan` 的字段清单。
pub(crate) const TERMINATION_FIELDS: [&str; 4] = [
    "mode",
    "admit_user_coroutines",
    "wait_foreign",
    "report_epoch",
];

/// 主线程关闭内部设施的固定顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum ShutdownFacility {
    /// poller：唤醒 parked worker 后关闭新注册。
    Poller = 0,
    /// processor：等 runtime critical section 到安全边界。
    Processor = 1,
    /// GC。
    Gc = 2,
    /// 栈 arena。
    StackArena = 3,
    /// 协程 cold slab。
    CoroutineColdSlab = 4,
    /// `CoroutineSlot` slab。
    CoroutineSlotSlab = 5,
}

impl ShutdownFacility {
    /// 返回设施的稳定名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Poller => "poller",
            Self::Processor => "processor",
            Self::Gc => "gc",
            Self::StackArena => "stack-arena",
            Self::CoroutineColdSlab => "coroutine-cold-slab",
            Self::CoroutineSlotSlab => "coroutine-slot-slab",
        }
    }

    /// 返回全部设施；顺序即关闭顺序。
    pub(crate) const fn all() -> [Self; 6] {
        [
            Self::Poller,
            Self::Processor,
            Self::Gc,
            Self::StackArena,
            Self::CoroutineColdSlab,
            Self::CoroutineSlotSlab,
        ]
    }
}

/// emergency report 的回退形态；诊断配置非法时使用固定纯文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum EmergencyFallback {
    /// 固定纯文本，不依赖 `GUGU_RUNTIME_DIAGNOSTICS` 与 `GUGU_BACKTRACE`。
    PlainText = 0,
}

/// emergency buffer 策略。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct EmergencyBufferPolicy {
    pub(crate) capacity_bytes: u32,
    pub(crate) min_bytes: u32,
    pub(crate) fallback: EmergencyFallback,
    /// 截断顺序：先截断 message，再从尾部丢弃 backtrace 帧。
    pub(crate) truncation: String,
}
