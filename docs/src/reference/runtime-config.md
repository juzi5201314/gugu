# 运行时配置

启动配置在 `Booting` 阶段从进程环境读一次；之后 `std.env.set` 不影响 runtime。权威语义与动态 API 见[运行时与运维语义](../spec/runtime.md)。

## 环境变量

| 变量 | 取值 | 默认 | 作用 |
| --- | --- | --- | --- |
| `GUGU_RUNTIME_PROCS` | >0 的十进制整数 | 可用并行度（≥1） | 初始并行度目标 |
| `GUGU_RUNTIME_GC_TARGET` | `off` 或百分数 | `100` | 自动 GC 堆增长目标 |
| `GUGU_RUNTIME_MEMORY_LIMIT` | `off` 或字节量 | `off` | runtime 管理内存软上限 |
| `GUGU_RUNTIME_STACK_MAX` | `64KiB`–`isize::MAX` 字节量 | `1GiB` | 每协程逻辑栈上限 |
| `GUGU_RUNTIME_TRACE` | `off`/`all`/`scheduler,gc,signal,panic` | `off` | runtime trace 类别 |
| `GUGU_RUNTIME_DIAGNOSTICS` | `text`/`json`/`both` | `text` | 致命与 panic 报告格式 |
| `GUGU_BACKTRACE` | `0`/`1`/`full` | `1` | 报告回溯范围 |

字节量后缀 `B`/`KiB`/`MiB`/`GiB`/`TiB`；非法值是 `InvalidConfiguration`。`GC_TARGET=off` 只关堆增长触发，内存上限与分配失败仍可强制 GC。

## 动态 API

`std.runtime.set_parallelism(n)`（`n>0`）发布新的并行度目标并返回旧值；已在运行的协程不迁移，调度器在线性化点调整 processor 池。GC/栈的控制入口见[运行时与运维语义 · GC、栈与运行时控制 API](../spec/runtime.md#gc-stack-runtime-control-api)。

## 阻塞与外调边界

阻塞 I/O、计时器、channel/锁等待与普通 `ForeignBridge` 挂起协程不挂起 runtime；`max_blocking_workers`、BridgeCredit、dirty CPU 额度等上限是编译期 runtime 契约的一部分，改契约即改指纹。规则见[运行时与运维语义 · 并行度与阻塞边界](../spec/runtime.md)与[外部调用效应与桥接](../spec/unsafe.md#foreign-call-effects-bridging)。
