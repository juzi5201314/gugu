# 运行时与运维语义

本章规定 Gugu 程序进入、运行、诊断和退出时的可观察运行时语义。运行时与程序、调度器、并发 GC 一起参加闭世界编译；它不是可以被用户替换的第二套标准库实现。

本章中的启动顺序、状态转换、panic 边界、fatal 分类、退出类别、信号订阅和运行时控制接口是规范性契约。调度队列的具体结构、OS 工作线程数量、GC 内部页布局、系统调用号和诊断文本的空白格式不是源程序可依赖的契约。

## 运行时状态

一次进程运行只沿下列方向转换状态，不会回到已经离开的状态：

| 状态 | 进入条件 | 允许的用户行为 |
|------|----------|----------------|
| `Booting` | rt0 开始执行，尚未调用用户代码 | 只执行 runtime 初始化、启动配置解析和平台探测；不得运行用户函数 |
| `Running` | runtime 已建立普通分配、调度、GC 和 panic 边界 | `main` 与用户协程正常执行；可以使用标准库和运行时控制接口 |
| `Waiting` | `main` 已返回，仍有用户协程存活 | 现存用户协程继续运行，也可以创建后代协程；runtime 继续处理调度、GC、控制和已注册信号 |
| `Terminating` | 显式退出、主协程 panic、fatal 或自然退出收尾开始 | 不再启动用户代码；runtime 只完成允许的报告、句柄回收和进程退出 |

`Booting`、`Running` 和 `Waiting` 中的运行时控制调用是进程级的；不是 coroutine-local 状态。`Terminating` 一旦开始，任何尚未发布的用户协程结果、defer 和信号事件都不再得到语言层保证。

## rt0 与启动

每个可执行镜像都有 rt0 入口。用户代码开始前必须依次完成：

1. 固定 argv、环境和初始工作目录快照；
2. 解析所有启动配置，非法配置按本章 fatal规则结束；
3. 建立足以安全执行分配、协程、panic、信号转交和 runtime API的运行环境；
4. 把 runtime状态从 `Booting` 发布为 `Running`；
5. 调用编译器已经解析的 `main` 入口。

ELF/PE自重定位、TLS、metadata验证、heap/scheduler建立和平台 fault handler的内部次序分别由[后端](../internals/backend.md)、[GC 元数据](../internals/gc-metadata.md)和[调度器](../internals/scheduler.md)规定，不构成额外的用户启动钩子。

`main` 之前必须达到普通代码可以安全分配、启动 `async`、访问 `std.env` 和进入 panic 边界的状态。rt0 不能调用用户定义的 `static` 初始化函数、`defer` 或普通协程；这些机制在 runtime 进入 `Running` 后才有效。

启动配置解析失败属于 `InvalidConfiguration` fatal，不能以 `Panic`、`Result` 或 `main` 的错误值交给用户处理；此时 runtime 只输出报告并终止。若诊断配置本身非法，必须使用不依赖该配置的固定纯文本 emergency report。

## 启动配置与动态控制

启动配置在 `Booting` 中从进程环境读取一次。环境变量不存在时使用表中的默认值；`std.env.set` 或 `std.env.remove` 后续改变的是宿主环境快照，不会重新配置 runtime。动态 API 的设置覆盖启动值，并按调用在线性化点的先后顺序生效。

| 环境变量 | 取值 | 默认值 | 作用 |
|----------|------|--------|------|
| `GUGU_RUNTIME_PROCS` | 大于 0 的十进制整数 | 启动时的可用并行度，至少为 1 | 初始并行度目标 |
| `GUGU_RUNTIME_GC_TARGET` | `off` 或十进制百分数 | `100` | 自动 GC 的堆增长目标；`100` 表示相对上次存活堆允许增长 100% |
| `GUGU_RUNTIME_MEMORY_LIMIT` | `off` 或无空白的字节量 | `off` | runtime 管理内存的软上限 |
| `GUGU_RUNTIME_STACK_MAX` | `64KiB` 至 `isize::MAX` 的字节量 | `1GiB` | 每个用户协程的逻辑栈上限；未提交整个上限的虚拟空间 |
| `GUGU_RUNTIME_TRACE` | `off`、`all` 或逗号分隔 `scheduler`、`gc`、`signal`、`panic` | `off` | 启动时打开 runtime trace |
| `GUGU_RUNTIME_DIAGNOSTICS` | `text`、`json` 或 `both` | `text` | fatal、`main` 返回 `Err` 和未处理 panic 的报告格式 |
| `GUGU_BACKTRACE` | `0`、`1` 或 `full` | `1` | 报告中包含的回溯范围 |

字节量使用十进制无符号整数，后面可以跟 `B`、`KiB`、`MiB`、`GiB` 或 `TiB`；乘法溢出、零值、未知后缀和其它拼写都是 `InvalidConfiguration`。`GUGU_RUNTIME_PROCS` 不允许 `0`。`GUGU_RUNTIME_TRACE` 的类别名只能是 `scheduler`、`gc`、`signal`、`panic`，重复项合并；`all` 等价于四类全部打开，未知类别属于 `InvalidConfiguration`。`GUGU_RUNTIME_DIAGNOSTICS` 和 `GUGU_BACKTRACE` 只能使用表中列出的值，否则同样属于 `InvalidConfiguration`。`GUGU_RUNTIME_GC_TARGET=off` 只关闭按堆增长触发的自动周期，内存上限和分配失败仍可以强制启动 GC。

运行时环境变量是已编译程序的运行时输入，不参与 package 依赖解析、源码 `cfg` 或编译缓存 key。工具链只负责把环境传给 `gugu run` 启动的程序；直接运行镜像时由宿主环境提供这些值。

## 并行度与阻塞边界

`parallelism` 是同时执行 Gugu 用户代码的目标并行度，不等同于 OS 线程数量，也不赋予程序可观察的处理器身份。阻塞 I/O、计时器等待、channel/锁等待和 `ForeignBridge` 外调会挂起当前协程；`ForeignLeaf` 外调不释放当前 `LogicalProcessor`，也不会把当前协程转换为 `Foreign`。在[并发公平规则](concurrency.md#抢占)允许的范围内，其它 runnable 协程仍须能够取得执行机会。

`parallelism` 的初始值来自 `GUGU_RUNTIME_PROCS`。`std.runtime.set_parallelism(n)` 要求 `n > 0`，成功时发布新目标并返回旧值。增加目标允许 runtime 按需增加并行执行能力；降低目标不中断正在执行的协程、系统调用或外部函数。调用返回表示新目标已经发布，不表示底层 OS 线程数量已经立即收敛。超过宿主 CPU 数量的值合法，但不产生吞吐量保证。

runtime 可以使用多于 `parallelism` 的 OS 线程处理阻塞系统调用和 `ForeignBridge`；`ForeignLeaf` 始终在当前 worker 和 processor 上直接执行，不触发这项线程扩张。各种外调怎样与协程和并行执行槽映射只见[调度器内部规范](../internals/scheduler.md)。外部调用期间函数仍在原 OS 线程执行，返回后协程按普通调度恢复；外部代码回调 Gugu 必须遵守[平台与 ABI 参考](platform-abi.md)的线程进入和回调边界。

用户协程不能把并行执行槽、工作线程编号或当前 OS 线程身份当作调度稳定性的一部分。`#[os_thread_local]` 只表示 OS 线程槽，`#[coroutine_local]` 只表示协程槽；动态并行度、阻塞等待和 `ForeignBridge` 都可能改变二者的使用时机。

调度公平、抢占、`yield`、channel 和同步原语的可见性见[并发与调度](concurrency.md)。safepoint、队列、context保存与外调交接只见[调度器](../internals/scheduler.md)和[栈图](../internals/stack-maps.md)；这些机制不能改变既有 happens-before、原子内存序或资源租约规则。

## 进程寿命

用户协程是由 `async` 创建的协程，加上运行 `main` 的主协程。只有用户协程参与自然退出等待；runtime 的内部活动不延长程序寿命。

| 情况 | 行为 |
|------|------|
| `main` 正常返回 `()` 或 `Ok` | 主协程的 `defer` / `defer ret` 已完成；runtime 转入 `Waiting`，挂起等待所有仍存活的用户协程结束，然后自然退出 |
| `main` 返回 `Err(e)` | 打印 `e`，记录 `MainError`，仍按相同规则等待所有仍存活的用户协程，最后以程序失败类别退出 |
| `main` panic | 只展开主协程并运行它的 defer，随后立即进入 `Terminating`；其它协程不展开、不运行剩余 defer |
| `std.process.exit(code)` | 立即进入 `Terminating` 并把 code 交给宿主；不运行其它协程的剩余 defer，也不等待其它协程 |
| 所有用户协程自然结束 | 若没有未处理分离 panic，则记录 `Success`；否则记录 `UnhandledPanic`，然后退出 |

主协程正常返回后，runtime 不隐式取消用户协程，也不因为 channel、锁、`select {}` 或外部调用永久阻塞而推断死锁。需要提前结束必须由程序显式取消任务、关闭资源或调用 `std.process.exit`。

`Child.detach()` 或最后一个 Child lease 释放后，子进程不受 Gugu 自然退出规则强制终止；runtime supervisor 只保留必要的 OS 观察能力并回收终止记录。显式 `Child.close()` 仍按[标准库](standard-library.md)的强制终止语义执行。

逻辑终止类别与宿主退出状态如下：

| 逻辑类别 | 典型原因 | Gugu 约定退出码 |
|----------|----------|----------------|
| `success` | `main` 成功且所有用户协程完成 | `0` |
| `program-failure` | `main` 返回 `Err`、未处理 panic 或测试/运行器报告失败 | `1` |
| `runtime-failure` | OOM、栈溢出、runtime invariant、外部 unwind、硬件 fault 或无效启动配置 | `2` |
| `explicit-exit` | `std.process.exit(code)` | 使用调用方给出的 code |
| `signal` | 未被订阅的普通终止信号导致宿主终止 | Linux 使用宿主 signal status；Windows 使用目标平台非零 status |

跨目标代码只能依赖成功为零、失败为非零和结构化 `reason`；Linux 的 `128 + signal` 等宿主惯例不是 Windows ABI 的一部分。显式退出码的完整整数转换仍由目标的 `std.process` 规则定义。

## panic 与恢复

panic 表示程序 bug（越界、对已关闭 channel `send`、显式 `panic(...)`），不是 `Result` 表示的可预期失败。`panic` 与 `std.process.exit` 的类型是 `!`，且必须 `#[track_caller]`。

```text
#[track_caller]
fn panic(msg: string) !

#[track_caller]
fn exit(code: int) !
```

Gugu 不提供 Go 式 `recover`。恢复边界是显式的 `std.panic.catch` 或子协程的 `Join.wait()`：

```text
let r = async { dangerous() }.wait()
match r {
    Ok(v) => v
    Err(p) => log(p)
}
```

`catch` 的签名是 `fn catch[T, F: Fn() T](f: F) Result[T, Panic]`。它在当前协程、当前栈上执行 `f`；panic 展开到 `catch` 边界时，边界以内的 defer 仍运行，边界以外的 defer 不运行。不能从任意 defer 中隐式取得当前 panic。

导出为 `extern "C"` 的函数若有 panic 逃出且未被 `catch`，必须 abort 进程，禁止把 Gugu 展开继续推进外部帧。外部异常或 C++ 风格 unwind 也不得穿过 Gugu 边界。

每个子协程只有一个完成记录：正常返回保存 `Ok(T)`，panic 展开完成保存 `Err(Panic)`。任意 `Join.wait()` 都从记录产生对应的语义值；第一次成功读取 `Err` 即把 panic 标记为已处理，重复读取不重复打印，也不改变退出类别。

分离协程 panic 时打印一份未处理 panic 报告并结束该协程；主协程仍运行时不因此立即终止进程。主协程已经返回、runtime 正在 `Waiting` 时，分离协程 panic 会把自然退出类别改为 `UnhandledPanic`。主协程 panic 立即终止进程。展开期间 defer 再 panic 属于 `PanicDuringUnwind` fatal。

`Panic` 是预导入 lang item，用户不能再定义同名类型：

```text
struct Panic {
    pub message: string
    pub location: Location
}
```

`Panic.location` 是原始 panic 调用点，不是 `catch`、`Join.wait` 或报告点。被 `catch` 或 `Join.wait` 处理的 panic 不自动产生 runtime 报告；报告格式见下文。

## fatal 与资源耗尽

fatal 是 runtime 无法安全恢复的进程级故障。fatal 不进入 `Panic`，不能被 `catch`、`Join.wait` 或用户 defer 截获；所有用户协程停止调度，runtime 尽力输出报告后终止整个进程。

| fatal 原因 | 触发条件 | 额外规则 |
|------------|----------|----------|
| `OutOfMemory` | 受限或未受限的 runtime 管理内存无法满足分配 | 先完成一次可行的 GC 重试；仍失败才终止。不得调用用户 hook 或分配普通报告对象 |
| `StackOverflow` | 用户协程需要的逻辑栈超过 `GUGU_RUNTIME_STACK_MAX` | 不展开用户栈，不运行该栈上尚未执行的 defer |
| `RuntimeInvariant` | 根表、调度器、GC 屏障或资源状态违反 runtime 内部不变量 | 这是实现故障；报告应带实现诊断，但程序不能恢复 |
| `ForeignUnwind` | 外部异常、C++ unwind 或未登记外部线程越过 Gugu 边界 | 立即停止跨边界展开，不能把外部异常映射成 `Panic` |
| `PanicDuringUnwind` | panic 展开或 fatal 报告期间再次进入不可恢复 panic | 终止路径不得递归执行用户代码 |
| `HardwareFault` | 不可安全转换的同步硬件 fault，例如非法指令或保护页之外的访问 | 只允许 best-effort 报告，然后恢复宿主默认终止行为 |
| `InvalidConfiguration` | 启动环境变量格式、范围或目标组合非法 | `main` 尚未调用，退出类别为 `runtime-failure` |

`GUGU_RUNTIME_MEMORY_LIMIT` 是软上限，不是操作系统硬隔离。它覆盖 managed heap、runtime私有 metadata、用户协程栈等 runtime管理的已提交内存，不覆盖镜像代码、操作系统内核资源或外部库自行分配的内存。runtime可以因当前对象、页粒度和并发回收周期暂时超过该值；超过后应提高 collection频率并回收不可达对象。

分配请求触发软上限时，runtime请求并等待一次完整 collection后再尝试分配；仍不能满足时进入 `OutOfMemory`。显式 `std.runtime.collect()` 也不保证所有内存返还宿主，不改变任何存活值或安全引用的语义，也不运行用户 finalizer。具体 safepoint、heap和 cycle阶段见[GC内部规范](../internals/gc-metadata.md)。

栈增长失败与栈上限的区分是：请求超过逻辑上限属于 `StackOverflow`；请求未超过上限但 runtime 页面分配失败属于 `OutOfMemory`。runtime 栈保护和 emergency report 缓冲区不得依赖当前用户栈仍然可写。

## 诊断、回溯与报告

runtime 报告只写 stderr，不写 stdout，不调用用户格式化 trait、异步 I/O 或普通分配器。默认 `GUGU_RUNTIME_DIAGNOSTICS=text` 输出人类可读报告；`json` 输出稳定的逐行 JSON；`both` 依次输出文本报告和 JSON 报告。报告覆盖 fatal、`main` 返回 `Err` 和未处理 panic。`main` 的 `Err(e)` 自身仍按 `Print` 规则先渲染为程序错误文本；该文本不属于 runtime report。

`GUGU_BACKTRACE` 的含义：

- `0`：不输出回溯；
- `1`：输出触发故障的用户协程或主协程回溯；
- `full`：在可取得时输出所有用户协程、工作线程和 runtime 边界帧。

回溯在栈损坏、早期启动故障和硬件 fault 时是 best-effort；缺失帧用空列表表示，不把报告失败升级成新的用户 panic。文本报告的缩进和颜色不是稳定接口，但必须包含事件类别、reason、消息（存在时）、源位置（存在时）和退出类别。

JSON 报告使用 schema `gugu-runtime-report-v1`，每个事件一行，字段如下：

```text
{
    "schema": "gugu-runtime-report-v1",
    "event": "termination",
    "class": "runtime-failure",
    "reason": "out-of-memory",
    "message": "GC heap limit exceeded while allocating 4096 bytes",
    "location": null,
    "backtrace": [],
    "exit_code": 2
}
```

`event` 是 `panic` 或 `termination`；`class` 是 `success`、`program-failure`、`runtime-failure`、`explicit-exit` 或 `signal`；`reason` 是稳定的小写短名，例如 `main-error`、`unhandled-panic`、`out-of-memory`、`stack-overflow`、`runtime-invariant`、`foreign-unwind`、`panic-during-unwind`、`hardware-fault`、`invalid-configuration` 和 `signal-terminate`。`message`、`location`、`backtrace` 按事件可为空，但字段必须存在。`location` 使用 `file`、`line`、`column` 字段；`backtrace` 是按报告时可解析出的帧数组。

被 `catch` 或 `Join.wait()` 处理的 panic 没有 `event`。未处理的分离 panic 先产生 `event = "panic"`；如果它影响最终自然退出，runtime 随后产生一个 `termination` 事件。`main` 返回 `Err` 直接产生 `termination` 事件。fatal 报告不等待其它协程、不保证用户资源租约的 defer 清理；操作系统最终回收进程资源。

本版本不提供可以替换默认报告、抑制 fatal 或改变退出类别的全局 panic hook。需要机器采集时使用 JSON 模式；需要业务级优雅退出时使用显式 signal 订阅和 `CancelToken`。

## 信号与外部终止

`std.signal` 是显式订阅接口。普通终止信号没有订阅者时遵循目标 OS 默认动作；runtime 不自动把 SIGINT/SIGTERM 转成根取消，也不启动隐藏的 graceful shutdown。用户可以在一个受监督协程中接收事件，再显式取消工作并等待已有用户协程。

可订阅的跨目标信号集合为：

| Gugu 信号 | Linux | Windows |
|-----------|-------|---------|
| `Interrupt` | `SIGINT` | console Ctrl+C |
| `Terminate` | `SIGTERM` | console close、logoff 或 shutdown 的终止通知 |
| `Hangup` | `SIGHUP` | 不提供 |
| `User1` | `SIGUSR1` | 不提供 |
| `User2` | `SIGUSR2` | 不提供 |
| `Break` | 不提供 | console Ctrl+Break |

`SIGKILL`、`SIGSTOP`、非法指令、保护页 fault 和其它同步 fatal fault 不是可订阅信号。平台不提供等价事件的组合必须在编译期通过 `cfg` 排除，而不是运行时伪造另一种信号。

信号订阅的标准接口为：

```text
enum Signal {
    Interrupt,
    Terminate,
    #[cfg(os = "linux")] Hangup,
    #[cfg(os = "linux")] User1,
    #[cfg(os = "linux")] User2,
    #[cfg(os = "windows")] Break,
}

struct SignalEvent {
    pub signal: Signal,
    pub occurrences: uint,
}

struct SignalSubscription

fn subscribe(signals: &[Signal]) Result[SignalSubscription, signal.Error]
fn subscribe_with_capacity(signals: &[Signal], capacity: uint)
    Result[SignalSubscription, signal.Error]
fn SignalSubscription.recv(self: &Self) Result[SignalEvent, signal.Error]
fn SignalSubscription.try_recv(self: &Self)
    Result[SignalEvent, signal.Error]
fn SignalSubscription.close(self: &Self)
fn SignalSubscription.dropped(self: &Self) uint
```

默认订阅队列容量为 16。一个进程可以有多个订阅；每个订阅都收到匹配信号的独立副本。相同信号在同一订阅尚未取走时合并为一个 `SignalEvent`，`occurrences` 递增并在 `uint::MAX` 饱和；计数饱和后的额外到达也计入 `dropped()`。不同信号占用不同队列项。队列满时新信号不阻塞 OS 信号处理路径，计入 `dropped()` 并继续保留已经排队的事件。

`recv` 只挂起当前协程，`try_recv` 不阻塞；关闭且取尽队列后返回 `signal.Error::Closed`。关闭订阅不影响其它订阅；某个信号没有任何订阅者后，runtime 恢复该目标的默认处理动作。订阅的建立和关闭都在线性化点生效，信号处理器绝不直接运行 Gugu 用户代码。最后一个 `SignalSubscription` 句柄 lease 释放等价于 `close()`。

信号到达时如果程序已有订阅，runtime 把事件投递给匹配订阅，不触发隐式取消、panic 或进程退出。用户可以把 `SignalEvent` 映射到 `CancelSource.cancel()`，随后按[并发与调度](concurrency.md)和[资源租约](standard-library.md)规则完成清理。没有订阅者时，普通信号的默认 OS 动作生效；runtime 在能够使用 signal-safe 输出时先尝试一条 `signal` termination 报告，再恢复默认动作。报告失败不得阻止或改变默认动作，也不能把默认终止改成等待用户协程。

signal 报告的 `reason` 使用 `signal-interrupt`、`signal-terminate`、`signal-hangup`、`signal-user1`、`signal-user2` 或 `signal-break`，分别对应可订阅的 Gugu 信号。宿主 signal status 仍按目标平台规则保留，逻辑终止类别按 `signal` 记录。

外部 FFI 或其它库直接修改 signal disposition、signal mask 或 Windows console handler 后，`std.signal` 的行为不再受 Gugu 保证；信号 API 不提供跨边界的 handler 链接或任意 signal number。

## GC、栈与运行时控制 API

`std.runtime` 公开的是 runtime 的控制与观测 facade，不公开 scheduler、collector、栈图、TLS 或 GC 元数据对象。所有 setter 都是进程级、线程安全的；多个协程并发调用时，按各自调用的线性化顺序采用最后发布的值，不为业务数据提供同步关系。

```text
enum RuntimeError {
    InvalidValue,
    Terminating,
}

enum GcTarget {
    Automatic(uint),
    Off,
}

struct TraceConfig {
    pub scheduler: bool,
    pub gc: bool,
    pub signal: bool,
    pub panic: bool,
}

struct RuntimeStats {
    pub live_coroutines: uint,
    pub live_os_threads: uint,
    pub parallelism: uint,
    pub heap_committed_bytes: uint,
    pub heap_live_bytes: uint,
    pub stack_committed_bytes: uint,
    pub gc_cycles: uint,
    pub gc_pause_total: Duration,
    pub signal_events_dropped: uint,
    pub trace_events_dropped: uint,
}

fn available_parallelism() uint
fn parallelism() uint
fn set_parallelism(value: uint) Result[uint, RuntimeError]
fn gc_target() GcTarget
fn set_gc_target(value: GcTarget) Result[GcTarget, RuntimeError]
fn memory_limit() Option[uint]
fn set_memory_limit(value: Option[uint]) Result[Option[uint], RuntimeError]
fn stack_limit() uint
fn collect()
fn stats() RuntimeStats
fn trace_config() TraceConfig
fn set_trace(value: TraceConfig) Result[TraceConfig, RuntimeError]
```

`available_parallelism()` 是启动时探测到的正数，失败时返回 1；它不是可用 CPU 的永久承诺。`set_parallelism(0)` 返回 `InvalidValue`；在 `Terminating` 中调用任何 setter 返回 `Terminating`。新值发布后，正在运行的协程不会绑定到某个可观察执行槽，runtime只在后续调度点收敛。

`GcTarget::Automatic(p)` 的 `p` 是非负百分数：下一次自动周期的目标堆量为上一次周期结束时存活堆量加上该百分比；`Automatic(100)` 是默认行为。`GcTarget::Off` 关闭增长触发，但不关闭显式 `collect()`、内存上限触发或分配失败前的强制回收。setter 返回旧值，已经开始的 GC 周期不因设置改变而回滚。

`set_memory_limit(Some(n))` 设置 runtime 管理内存的软上限，`None` 恢复无上限；setter 返回旧上限。它不能撤销已经提交的页，也不能终止外部库分配。`collect()` 请求一个从调用线性化点之后开始的完整 GC 周期，并挂起当前协程直到该周期完成；其它可运行协程可以继续。调用不保证空闲页归还 OS，也不运行 finalizer。

`stack_limit()` 返回当前每协程逻辑栈上限。栈上限只能由 `GUGU_RUNTIME_STACK_MAX` 在启动时设置；本版本不提供降低活动栈上限的动态 setter，因为那会使已存在的栈无法满足安全返回条件。

`RuntimeStats` 是逐字段快照，不是业务同步原语。计数器在进程内单调递增，当前量可以因并发回收而在采样后改变；`heap_live_bytes` 不等同于可立即返还 OS 的页数。`signal_events_dropped` 和 `trace_events_dropped` 覆盖 runtime 所有订阅和 trace 队列。

`TraceConfig` 的事件写入 stderr，使用 `gugu-runtime-trace-v1` 的逐行 JSON。事件包含类别、事件名、相对 runtime 启动的单调纳秒时间和可用的协程/不透明执行槽标识；trace 只影响诊断输出，不改变调度、GC 或信号语义。trace 缓冲区满时丢弃事件并增加 `trace_events_dropped`，不能阻塞用户代码或进入用户 panic。

## 局部静态初始化

`#[coroutine_local]` 和 `#[os_thread_local]` 的初始化都允许在运行时发生，但二者不是进程启动钩子。局部槽第一次被该协程或 OS 线程读取时初始化；初始化表达式只执行一次，递归读取同一槽属于 runtime fatal，不返回半初始化值。初始化 panic 按当前协程的普通 panic 规则传播；若它发生在 runtime 无法建立 panic 边界的 `Booting` 阶段，则是 `RuntimeInvariant` fatal。

局部静态的释放顺序不构成跨协程或跨 OS 线程契约。协程自然完成时可以释放其 coroutine-local 槽；OS 线程因动态并行度、阻塞外调或进程终止而结束时可以释放其 thread-local 槽。`std.process.exit`、主协程 panic 和 fatal 不保证局部槽的用户清理。

## 实现边界

本章只规定 runtime状态、配置、公开 facade、报告和进程寿命。compiler/runtime私有共同契约分别位于 [GIR/LIR](../internals/gir-lir.md)、[栈图](../internals/stack-maps.md)、[GC 元数据](../internals/gc-metadata.md)、[调度器](../internals/scheduler.md)和[后端](../internals/backend.md)：这些文档固定 safepoint、frame、root/barrier、metadata、队列和外调交接，但不能为本章增加新的 fatal类别、清理保证、同步关系或退出行为。

## 设计参考

本章的分层边界参考以下官方资料；这些资料用于解释设计取舍，不会覆盖本章已经固定的 Gugu 语义：

- [Rust Reference：Runtime](https://doc.rust-lang.org/reference/runtime.html)
- [Rust Reference：Panic](https://doc.rust-lang.org/reference/panic.html)
- [`std::process::exit`](https://doc.rust-lang.org/std/process/fn.exit.html)
- [`std::alloc::handle_alloc_error`](https://doc.rust-lang.org/std/alloc/fn.handle_alloc_error.html)
- [Go 规范：Program initialization and execution](https://go.dev/ref/spec#Program_initialization_and_execution)
- [Go 规范：Handling panics](https://go.dev/ref/spec#Handling_panics)
- [Go `runtime` package](https://pkg.go.dev/runtime)
- [Go GC guide](https://go.dev/doc/gc-guide)
- [Go `os/signal` package](https://pkg.go.dev/os/signal)
