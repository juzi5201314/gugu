# 运行时

运行时不是宿主 VM。它和用户程序全程序编译进同一镜像，外加 rt0。

## rt0

汇编（或同级、无 GC、无分配器循环依赖）提供：

- Linux `_start` 或 Windows PE 入口
- 最初的 syscall / 导入：`exit`、`mmap`/`VirtualAlloc`、stderr `write`/`WriteFile`
- 第一块堆或固定工作区（GC 尚未就绪）
- 跳入 `runtime.start`

禁止依赖 libc 初始化。Linux 静态 ELF 直接 `execve`。Windows 镜像带薄 IAT，不链 CRT。体积：几百到两千行量级，长期留在底层。

## Gugu runtime

用本语言实现：

- 并发分代 Immix GC（TLAB、写屏障、根枚举、并发标记与回收）
- M:N 调度：G/M/P、拷贝增长栈、抢占 safepoint、`async` / `yield`
- `chan[T]` 与 `select`
- 进程寿命与退出码（见下）
- panic：只展开当前 G；恢复只在监督边界（见下）

`main` 之前必须达到「普通代码可安全分配、可 `async`」的状态。

## 进程寿命

用户 G = 用户用 `async` 创建的协程，加上跑 `main` 的主 G。runtime 内部的 M、sysmon、GC 工作线程不是用户 G，不等待它们。

| 情况 | 行为 |
|------|------|
| `main` **正常返回** | 主 G 的 `defer` / `defer ret` 已跑完。runtime **挂起等待所有仍存活的用户 G 结束**，然后进程自然退出。 |
| `main` **panic** | 只展开主 G（跑它的 defer），然后**立刻终止进程**。其它用户 G 被直接丢掉，**不展开、不跑它们的 defer**（与 Go 里 main panic 导致进程死掉一样）。退出码非 0。 |
| `std.process.exit(code)` | **立刻**终止进程。不跑任何 G 的剩余 defer，不等待其它 G。需要清理就先自己做，或从 `main` `return`。 |

自然退出的退出码：`main` 正常返回且等待期间没有「分离 G 的未处理 panic」则为 0；分离 G 在等待期间 panic 则打印诊断且退出码非 0。已被某次 `Join.wait()` 收成 `Err` 的 panic 算处理过，不污染退出码。

这与 Go **不同**：Go 在 `main` 函数返回后会立刻杀掉其它 goroutine。Gugu 在 `main` 成功返回后会等它们跑完，避免「后台任务写到一半进程没了」。要立刻走，显式 `std.process.exit`。

## panic 与恢复

panic 表示程序 bug（越界、对已关闭 channel `send`、显式 `panic(...)`），不是 `Result` 那种可预期失败。

### 不做 Go 式 `recover`

禁止 `recover()` 这种「必须写在 defer 里才生效、否则静默变成 nop」的原语。它和 `Result` 抢两条错误通道，也容易在错误的栈帧上恢复成功、不变量已经烂了。

Rust 也没有 `recover`：它用线程边界的 `JoinHandle::join() -> Result`，以及显式的 `catch_unwind`。Gugu 对齐这个形状，但把「线程」换成 G。

### 恢复边界 = 监督边界

隔离一次可能崩掉的计算：把它放进子 G，在父 G 上 `wait`。

```
let r = async { dangerous() }.wait()
match r {
    Ok(v) => v
    Err(p) => log(p)
}
```

同一条请求循环里，隔离是 `async { handle(req) }`（丢掉 `Join` 即分离）。某个请求 panic 只杀死那一个 G，监听循环继续。这是监督树，不是在 defer 里捞一把。

### 同栈捕获（不是关键字）

有时不能或不该再开一个 G：导出给 C 的函数禁止把 Gugu panic 展开进 C 栈；测试要断言「这里会 panic」。这些用标准库：

```
std.panic.catch(fn() { might_panic() })
```

签名：`fn catch[T, F: Fn() T](f: F) Result[T, Panic]`。在**当前 G、当前栈**上跑 `f`，panic 则展开到 `catch` 边界（该边界以内的 defer 仍跑），然后变成 `Err`。`catch` 以外的 defer 不跑。不能从任意 defer 里「捞当前 panic」——没有隐式 `recover`。

导出为 `extern "C"` 的函数：若 panic 逃出该函数且未被 `catch`，必须 **abort 进程**，禁止把展开继续推进 C。

### 展开规则

1. panic 只展开**当前 G**。跑该 G 上仍应执行的 `defer` / `defer ret`（LIFO）。
2. 其它 G 不被这条 panic 展开。
3. 子 G 死后，`Join.wait()` 得到 `Result[T, Panic]`：`Ok(T)` 或 `Err(p)`。等待者默认不跟着 panic。
4. 分离 G（`Join` 被丢掉）panic：打印诊断，该 G 结束。若此时主 G 还在跑，**不**因此终止进程。若主 G 已经正常返回、runtime 正在等待用户 G，该 panic 使最终退出码非 0。
5. 主 G panic：见「进程寿命」——展开主 G 后立刻终止进程。
6. 已经在展开时，`defer` 里又 panic：进程 abort（禁止「panic 套 panic」继续走用户代码）。

`Panic` 是标准库结构体（预导入）：可读消息、源位置、可选载荷。

## 启动顺序

```
OS 加载镜像
  → rt0
  → 不经 GC 的分配可用
  → runtime 自举（堆、P、主 G、栈图）
  → 用户 `main`（主 G）
  → 若 main 正常返回：等待其余用户 G
  → 若 main panic 或 process.exit：立即进入收尾/exit
  → runtime 收尾
  → rt0 exit
```

自举可用不扫描分配器；GC 启用后进堆的对象必须有头、有根。

## 与编译器的契约

编译器必须认识：分配点、safepoint、写屏障、换栈、panic 展开与 `catch` 边界、`process.exit`、channel 阻塞点。runtime 仍是普通 Gugu + intrinsic，禁止硬编码一份 C 运行时来充当语义。

## 标准库

`std` 与 runtime 同一闭世界。`print` / `println` 是参数包 + `Print` trait 的普通函数，最终走 syscall / `WriteFile`。`std.process.exit` 与 `std.panic.catch` 必须存在，语义按本章。
