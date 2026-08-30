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

## channel 的线性化与公平

每个 `send`、`recv`、`try_send`、`try_recv` 和 `close` 都有一个单一线性化点。阻塞操作在抵达该点前可以挂起当前协程，但不会占用操作系统线程；恢复后操作要么完成，要么按已关闭/无容量的结果返回。`send` 与 `close` 竞态按线性化先后决定：先完成的发送进入缓冲或与接收会合，先完成的关闭使后续发送 panic。关闭不会丢弃在线性化前已完成的缓冲消息。

关闭后的 `recv` 先收完所有已经线性化的缓冲消息，再永久返回 `Err(ChanClosed)`；关闭后的 `try_recv` 立即返回 `Err(TryRecvErr::Closed)`。未关闭且暂时无消息时，`try_recv` 立即返回 `Err(TryRecvErr::Empty)`，绝不等待。无缓冲 `try_send` 只有已有接收者在同一线性化点等待时才成功，否则立即返回 `Err(TrySendErr::Full)`。

`close` 之前的写入与观察到永久关闭结果的 `recv` / `try_recv` 建立 happens-before。父协程在创建 `async` 子协程前完成的写入对该子协程开始执行可见；子协程完成前的写入在 `Join.wait()` 返回后对等待者可见。仅创建 Join 或轮询未完成状态不建立反向同步。

`select` 对就绪分支进行一次原子提交。存在就绪分支时 default 永不获选；不存在就绪分支时才选择 default 或挂起。多个就绪分支的选择是随机的，并满足弱公平：持续保持就绪的分支不能被调度器永久排除。随机种子、具体轮询算法和不同操作系统线程上的执行顺序不是语言可观察保证。

## 内存序与同步 API

`std.sync.Ordering` 至少包含 `Relaxed`、`Acquire`、`Release`、`AcqRel`、`SeqCst`。原子操作只允许无 GC 引用的 `bool`、整数或指针宽度的精确整数类型；不能对 `string`、句柄、含引用结构体或浮点做原子读改写。

```text
enum Ordering { Relaxed, Acquire, Release, AcqRel, SeqCst }

struct Atomic[T]
fn new[T](value: T) Atomic[T]
fn load[T](self: &Atomic[T], order: Ordering) T
fn store[T](self: &Atomic[T], value: T, order: Ordering)
fn swap[T](self: &Atomic[T], value: T, order: Ordering) T
fn compare_exchange[T](self: &Atomic[T], expected: T, desired: T,
                       success: Ordering, failure: Ordering) Result[(), T]
fn fence(order: Ordering)
```

`load` 只接受 `Relaxed`、`Acquire`、`SeqCst`；`store` 只接受 `Relaxed`、`Release`、`SeqCst`；读改写操作接受 `Relaxed`、`Acquire`、`Release`、`AcqRel`、`SeqCst`。CAS 失败序不能是 `Release`/`AcqRel`，且不能强于成功序；违规是编译错误。成功返回 `Ok(())`，失败返回当时的实际值 `Err(actual)`，不会自动重试。`Acquire` 读与对应 `Release` 写建立同步；`SeqCst` 原子操作还参加全局单一顺序；`Relaxed` 只有原子性，不建立非原子数据的可见性关系。

最低锁接口由标准库提供：`Mutex[T]` 提供 `new`、`lock`、`get`、`unlock` 与 `with_lock`；`RwLock[T]` 提供对应的读锁/写锁和 `with_read` / `with_write`；`Condvar` 提供 `wait`、`notify_one`、`notify_all`。锁操作阻塞时只挂起当前协程。锁的成功解锁与随后成功加锁建立 happens-before；`Condvar.wait` 原子地释放关联锁、挂起并在返回前重新取得锁。唤醒可以是虚假的，调用者必须循环检查条件。

```text
struct Mutex[T]
struct MutexGuard[T]
impl Mutex[T] {
    fn new(value: T) Mutex[T]
    fn lock(self: &Self) MutexGuard[T]
    fn with_lock[R, F: Fn(&T) R](self: &Self, f: F) R
}
impl MutexGuard[T] {
    fn get(self: &Self) &T
    fn unlock(self: &Self)
}

struct RwLock[T]
struct RwReadGuard[T]
struct RwWriteGuard[T]
impl RwLock[T] {
    fn new(value: T) RwLock[T]
    fn read(self: &Self) RwReadGuard[T]
    fn write(self: &Self) RwWriteGuard[T]
    fn with_read[R, F: Fn(T) R](self: &Self, f: F) R
    fn with_write[R, F: Fn(&T) R](self: &Self, f: F) R
}
impl RwReadGuard[T] {
    fn snapshot(self: &Self) T
    fn unlock(self: &Self)
}
impl RwWriteGuard[T] {
    fn get(self: &Self) &T
    fn unlock(self: &Self)
}

struct Condvar
impl Condvar {
    fn new() Condvar
    fn wait[T](self: &Self, guard: &MutexGuard[T])
    fn notify_one(self: &Self)
    fn notify_all(self: &Self)
}
```

Gugu 的 `&T` 可写，因此读锁不能暴露内部槽的 `&T`；`snapshot()` 和 `with_read` 只浅拷当前值。若 `T` 含可变句柄，调用者仍须保证不会绕过锁并发写其载荷。每个守卫是共享状态句柄，浅拷守卫仍代表同一次加锁；第一次 `unlock` 释放锁，任何副本再次 `unlock` 都 panic，解锁后再 `get` / `snapshot` / `wait` 也是 panic。

`Atomic::new`、`Mutex::new`、`RwLock::new`、`Condvar::new` 都是 comptime 可求值构造，可用于普通 `static` 初始化。标准库为 `MutexGuard`、`RwReadGuard`、`RwWriteGuard` 提供否定 `Clone` impl；这禁止深拷，但不改变语言按值传递时复制守卫句柄的规则。

守卫 `get` 得到的 `&T` 只允许在对应守卫仍处于已加锁状态时用于同步访问；`unlock` 后继续通过该引用读写属于未同步访问，若与其它访问形成竞争即未定义行为。`Condvar.wait` 释放锁期间不能使用先前取得的引用，返回并重新加锁后可重新获取。锁不提供 poisoning：受保护操作 panic 时 `with_*` 释放锁，但不标记数据是否满足应用不变量。

Mutex 不可重入；同一协程持有守卫时再次 `lock` 会像其它竞争者一样等待，可能造成永久阻塞。标准锁实现必须满足弱公平，持续等待且锁反复可用的协程不能被永久排除。RwLock 不提供隐式读写升级或降级；要切换模式必须先解锁再重新加锁。



锁守卫不可 Clone，不能跨所属锁使用；`with_lock` / `with_read` / `with_write` 在闭包正常返回或 panic 展开时都释放锁。显式 `lock` 得到的守卫必须显式 `unlock`；忘记解锁会使后续请求永久等待，但不是隐式析构或可捕获异常。锁和原子保证同步，不会替程序员把普通 `&T` 的并发写变成安全操作。

任何没有原子、channel、锁、条件变量或其它明确 happens-before 的并发读写，只要至少一方写入同一内存位置，就是数据竞争和未定义行为。GC 延长寿命不改变该规则。
