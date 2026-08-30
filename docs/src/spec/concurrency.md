# 并发与调度

Gugu 是高并发语言。并发原语是语言与 runtime 的一部分，不是「以后再加的库」。

模型对齐 Go 那种「多协程跑在多个操作系统线程上」，语义上：

- **协程**：有栈绿色协程。用户代码里的并发单位。数量目标：同一进程内百万级。
- **操作系统线程**：真正执行机器码。
- **逻辑处理器**：绑定线程本地分配缓冲（TLAB）、本地运行队列、GC 本地状态。操作系统线程必须持有逻辑处理器才跑协程。

诊断信息也必须用这三个全称，不用单字母缩写。

不是 Rust/JS 的 `async fn`：函数没有颜色，没有 `.await` 传染。关键字 `async` **只用来启动新协程**（见下）。普通函数里直接 `ch.recv()` / `h.wait()` 就会挂起当前协程，操作系统线程去跑别的工作。

## 有栈协程

- 每个协程有一块**连续**栈，初始很小，不够则**拷贝增长**（禁止分段栈热分裂）。
- 切换是 IR 原语（寄存器 + 栈指针）。
- 创建：`async 调用` 或 `async { 块 }`，见 [表达式](expressions.md)。得到 `Join[T]`。
- `yield`：主动让出。
- 进程何时等用户协程、何时杀掉，见 [运行时 · 进程寿命](runtime.md)。

## 抢占

不是纯协作。每个协程连续运行超过时间片后必须被切走，即使它在算 CPU、没有主动 `yield`。

抽象机：

1. 调度器给正在跑的协程一个时间量子。
2. 编译器保证协程在有界的工作之后进入 **safepoint**（函数入口、循环回边、调用点）。无调用的热循环也必须有回边轮询。
3. 量子耗尽后，下一次 safepoint 切走该协程。
4. 实现可以用定时器 + 异步中断（Linux 信号、Windows APC）把「无轮询的长指令序列」拉回 safepoint；**不能**只靠程序员插 `yield`。
5. 「指令量」用 safepoint 密度近似，不在每条指令上计数（那会直接否定性能目标）。

抢占与 GC 共用 safepoint：栈图有效、可扫描、可移动对象。

轮询必须便宜（典型是读一个线程本地标志或保护页）。实现有义务把抢占开销压到与 Go 同类程序可比，而不是「正确但每次循环 +10%」。

## Channel

`chan[T]` 是语言类型，运行时在 GC 堆上（句柄，权威状态在堆对象里）。

- 类型与构造：`chan[T]`、`chan[T](n)`。`n` 是 `int` 缓冲长度，`n == 0` 即无缓冲；`n < 0` 在 comptime 是编译错误，运行时是 panic。`chan` 是关键字，这种调用不是下标。
- 无缓冲：发送与接收会合（一次 `send` 与一次 `recv` 必须配对完成，谁先到谁等）。
- 发送、接收、关闭、`select`、`try_*` 的类型见 [表达式](expressions.md)。方法名固定为 `send` / `recv` / `try_send` / `try_recv` / `close`，不能重载。
- 关闭后收尽，`recv` 返回 `Err(ChanClosed)`；再 `send` 或再 `close` 是 panic。
- 不存在 nil channel。
- 在 `chan` 上阻塞只停当前协程，不绑死操作系统线程（除了持有逻辑处理器进入系统调用或外部函数的那些路径，runtime 必须能把逻辑处理器让给别的操作系统线程）。
- `send` 与对应的 `recv`（含 `select` 选中的那对）建立 happens-before：发送方在 `send` 之前对载荷的写入，接收方在 `recv` 返回之后看得见。

互斥锁、读写锁、条件变量、原子、一次性初始化，放在 `std.sync`，用 intrinsic 实现，不是关键字。

`std.sync.OnceLock[T]` 必须存在：

```
impl OnceLock[T] {
    fn new() OnceLock[T]
    fn get(self: &Self) Option[&T]
    fn get_or_init[F: Fn() T](self: &Self, f: F) &T
    fn set(self: &Self, v: T) Result[(), T]
}
```

`std.sync.Lazy[T]` 必须存在：

```
impl Lazy[T] {
    fn new[F: Fn() T](f: F) Lazy[T]
    fn get(self: &Self) &T
}
```

- `OnceLock::new()` 是 comptime，可放进 `static`。第一次 `get_or_init` / 成功的 `set` 写入槽；并发调用只跑一次 `f`，其它协程等到完成。返回的 `&T` 指向进程寿命槽。之后通过 `&T` 的并发写仍是数据竞争，除非 `T` 自己同步。成功初始化对随后的 `get` 建立 happens-before。
- `set` 在已初始化时返回 `Err(v)`（把 `v` 交还），不覆盖。
- `Lazy::new(f)` 的 `f` 若放进 `static`，必须是不捕获的函数或闭包。第一次 `get` 调用 `f`，规则同 `OnceLock`。
- 不要用 `OnceLock` 模拟「每个协程一份」——那是 `#[coroutine_local] static`，见 [声明](declarations.md)。

## `select`

一次等待多个 channel 的 `send`/`recv` 以及 `Join.wait()`，随机公平选择就绪分支，可有默认 `_`（不阻塞）。求值顺序与作为表达式的类型规则见 [表达式](expressions.md)。没有分支的 `select {}` 永远挂起当前协程。

## 内存序与数据竞争

多个协程可以同时跑在多个操作系统线程上。数据竞争是未定义行为。类型系统**不**静态禁止把 `&T` 送进另一个协程（没有 `Send`/`Sync` 约束）：GC 延命解决的是寿命，不是互斥。跨协程共享可变状态是程序员义务，必须走 `chan`、`std.sync`、原子、或只读共享。编译器可以提供竞态检测器构建，但不保证检出全部数据竞争。
