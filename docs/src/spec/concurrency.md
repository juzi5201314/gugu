# 并发与调度

Gugu 是高并发语言。并发原语是语言与 runtime 的一部分，不是「以后再加的库」。

模型对齐 Go 那种“多协程可以并行运行在多个操作系统线程上”。公开术语为：

- **协程**：用户代码中的并发单位，具有独立调用栈；
- **操作系统线程**：宿主执行线程；
- **逻辑处理器**：runtime 允许同时执行 Gugu 用户代码的并行容量单位。

诊断信息必须使用这三个全称，不用内部单字母缩写。worker绑定与内部调度数据结构见[调度器内部规范](../internals/scheduler.md)，不构成程序语义。

不是 Rust/JS 的 `async fn`：函数没有颜色，没有 `.await` 传染。关键字 `async` **只用来启动新协程**（见下）。普通函数里直接 `ch.recv()` / `h.wait()` 就会挂起当前协程，操作系统线程去跑别的工作。

## 有栈协程

- 协程挂起后保留完整调用栈和局部状态，恢复时从原挂起点继续；栈的分配、增长和复制方式见[调度器内部规范](../internals/scheduler.md)。
- 创建：`async 调用` 或 `async { 块 }`，见 [表达式](expressions.md)。得到 `Join[T]`。
- `yield`：主动让出。
- 进程何时等用户协程、何时杀掉，见 [运行时 · 进程寿命](runtime.md)。

## 抢占

调度保证分为可合作的受管代码与不透明原生代码两层。可合作受管代码的每个可达 frame 都有精确 stack map/unwind metadata；compiler 通过可投毒的函数 `StackCheck`、必须挂起的同步点和按静态工作预算放置的 poll，保证每条无限受管执行路径无限次经过同步 safepoint，并限制相邻 safepoint 之间的编译开销。普通循环不承诺每条回边都读取 runtime 状态；循环 poll 的具体放置、预算与缓存 schema 属于 compiler 实现契约，见[调度器内部规范](../internals/scheduler.md)。只执行有效受管代码的协程即使不主动 `yield`，也不能永久阻止其它 runnable 协程或 GC 获得执行机会。

普通 inline `asm` 不是 safepoint。有限且可返回的 asm 片段可以在 `Running` 中执行，但它造成的延迟持续到片段返回；包含不可证明有限的内部回边、间接控制转移或外部等待的 asm 必须被 compiler 拒绝，或放入带函数体的 `#[ffi(dirty_cpu)] unsafe extern "C" fn`。`global_asm`、默认进入受管上下文的 `#[naked]` 和 dirty native definition 不属于可合作受管代码：从用户协程进入时必须脱离 `LogicalProcessor`，因此不会让它所属的逻辑处理器或 GC stop 永久等待；但 dirty native work 本身可以永不返回，语言不保证该调用完成。

`ForeignLeaf` 是用户承担的 unsafe契约：错误声明会占住当前 `LogicalProcessor`；若调用永久不返回，该 processor无法确认 GC stop，因而可以永久阻止进程完成 GC。这样的程序违反 unsafe契约，不在调度/GC活性保证内。无法证明 leaf时使用普通 `ForeignBridge`；它发布稳定 bridge frame并先取得有界 `BlockingBridge` 的 `BridgeCredit`，短调用可以在 attached路径保留 generation-tagged processor lease，GC、回调、退役或持续 runnable压力能够取回 lease。没有 credit时普通 bridge在不持有 processor的 `Waiting` 状态排队，不能通过每个调用创建无界 OS worker；因此未知 native work不能永久阻止其它 managed work或 stop epoch。

signal/APC 只向当前 `LogicalProcessor` 发布抢占/GC请求、投毒正在运行协程的 `StackCheck`并唤醒 worker；它不在任意机器 PC扫描栈、复制协程栈或运行用户 defer。函数调用在被调方 prologue响应投毒，长循环在预算化 poll响应；对尚未到达同步点的执行，请求保持 pending。

`std.runtime.safepoint_poll()` 是显式同步 safepoint：fast path读取当前逻辑处理器的 poll word，必要时确认 GC stop、处理抢占并让出当前协程。它可以被安全调用，可能挂起或在恢复后继续；必须作为独立 Gugu调用出现，不能写进 inline/global asm模板或 `#[naked]` 函数。把 asm循环拆成有限片段并放回普通 Gugu循环后，compiler的预算化 loop poll已满足活性要求；库也可以在大块工作之间显式调用它，建立更早的合作边界。

CPU自旋仍然是 `Running` 计算：预算化 poll能让它被抢占，但整块工作都会持续占用 CPU。网络等待、channel、计时器和 `std.sync`锁竞争走 `park`，只挂起当前协程，不占住承载它的 worker/processor；这与 CPU抢占是两套机制。poll预算和机器成本表属于 compiler实现与缓存 schema，不是用户可观察的时间片或 wall-clock保证。

## Runnable 顺序与公平

runnable 协程没有进程级 FIFO 顺序。调度优先保持局部性：当前逻辑处理器的本地就绪工作先于跨处理器提交的工作（定时器完成、本地溢出、工作窃取等路径），因此不同提交来源、不同分片和不同逻辑处理器之间允许重排。preferred processor 只是性能 hint，不建立线程亲和性或后续执行位置保证。程序不能用两个独立 wake 的观察顺序替代 channel、原子、Join 或其它同步。

一次等待被成功唤醒，必须已经取得可被调度器消费的 runnable ownership，不能只保留在无人消费的 producer 私有暂存中。提交与挂起交接之间到达的完成通知必须被保留或重新发布，不能让已经完成的等待永久睡下；这不增加跨 producer 的顺序保证。

runtime 提供弱公平而非 wall-clock 时间片：只要进程继续运行、协程持续保持 Runnable 且没有违反 `ForeignLeaf`/unsafe 契约，它不能被持续产生的新本地或远程工作永久排除。实现必须对本地队列的连续命中设限、周期性服务远程提交，并保证跨分片的检查进度；具体的 service interval、batch size 与窃取策略是内部性能 schema，不是可观察的纳秒或调度次数承诺。

显式 `yield` 把当前协程放入本地就绪队列尾部，使当时已有 runnable 至少获得一次被选择机会；它不承诺下一个执行者、全局 FIFO 或迁移到其它逻辑处理器。动态降低 parallelism 时，正在退役的逻辑处理器上尚未运行的就绪工作、携带工作与计时器必须完整转移，但它们与其它逻辑处理器已有工作仍只有上述弱顺序。

## Channel

`chan[T]` 是语言内建的身份句柄类型。

- 类型与构造：`chan[T]`、`chan[T](n)`。`n` 是 `int` 缓冲长度，`n == 0` 即无缓冲；`n < 0` 在 comptime 是编译错误，运行时是 panic。`chan` 是关键字，这种调用不是下标。
- `chan[T](n)` 的 `T` 按类型实参解析，不依赖接收结果的槽是否显式标注类型；例如 `let queue = chan[int](0)` 直接形成 `chan[int]`。
- 无缓冲：发送与接收会合（一次 `send` 与一次 `recv` 必须配对完成，谁先到谁等）。
- 发送、接收、关闭、`select`、`try_*` 的类型见 [表达式](expressions.md)。方法名固定为 `send` / `recv` / `try_send` / `try_recv` / `close`，不能重载。
- 关闭后收尽，`recv` 返回 `Err(ChanClosed)`；再 `send` 或再 `close` 是 panic。
- 不存在 nil channel。
- 在 `chan` 上阻塞只停当前协程，不长期占住承载它的操作系统线程；系统调用或 `ForeignBridge` 外调阻塞时，runtime 必须让其它线程继续利用可用逻辑处理器。`ForeignLeaf` 的 unsafe 契约禁止不可界定的阻塞；`DirtyCpu` 调用即使不等待也不占用逻辑处理器，但会消耗受限的 dirty CPU 执行额度。
- `send` 与对应的 `recv`（含 `select` 选中的那对）建立 happens-before：发送方在 `send` 之前对载荷的写入，接收方在 `recv` 返回之后看得见。

channel 生产者与消费者的用法：

```gugu
fn producer(jobs: chan[int]) {
    for i in 0..8 {
        jobs.send(i)
    }
    jobs.close()
}

fn consumer(jobs: chan[int]) {
    loop {
        match jobs.recv() {
            Ok(job) => print(f"got {job}")
            Err(_) => return
        }
    }
}

fn demo() {
    let jobs = chan[int](4)
    async producer(jobs)
    consumer(jobs)
}
```

互斥锁、读写锁、条件变量、原子、一次性初始化位于 `std.sync`，不是关键字。`Mutex` 与 `RwLock` 不 poisoning：持锁协程 panic 时 guard 仍释放，后续调用正常获得锁，受保护数据是否满足业务不变量由调用方负责。

`std.sync.OnceLock[T]` 必须存在：

```text
impl OnceLock[T] {
    fn new() OnceLock[T]
    fn get(self: &Self) Option[&T]
    fn get_or_init[F: Fn() T](self: &Self, f: F) &T
    fn set(self: &Self, v: T) Result[(), T]
}
```

`std.sync.Lazy[T]` 必须存在：

```text
impl Lazy[T] {
    fn new[F: Fn() T](f: F) Lazy[T]
    fn get(self: &Self) &T
}
```

- `OnceLock::new()` 是 comptime，可放进 `static`。第一次 `get_or_init` / 成功的 `set` 写入槽；并发调用只跑一次 `f`，其它协程等到完成。返回的 `&T` 指向进程寿命槽。之后通过 `&T` 的并发写仍是数据竞争，除非 `T` 自己同步。成功初始化对随后的 `get` 建立 happens-before。
- `set` 在已初始化时返回 `Err(v)`（把 `v` 交还），不覆盖。
- `Lazy::new(f)` 的 `f` 若放进 `static`，必须是不捕获的函数或闭包。第一次 `get` 调用 `f`，规则同 `OnceLock`。
- `OnceLock` / `Lazy` 的初始化闭包 panic 后进入永久 Failed 状态；后续 `get`、`get_or_init` 或等待只传播同一失败，不重跑闭包，也不提供 reset。初始化成功后才进入 Ready 并建立前述 happens-before。
- 不要用 `OnceLock` 模拟「每个协程一份」——那是 `#[coroutine_local] static`，见 [声明](declarations.md)。

## `select`

一次等待多个 channel 的 `send`/`recv` 以及 `Join.wait()`，随机公平选择就绪分支，可有默认 `_`（不阻塞）。求值顺序与作为表达式的类型规则见 [表达式](expressions.md)。没有分支的 `select {}` 永远挂起当前协程。

分支数量不受单个机器字宽度限制；超过 64 个分支仍执行同一就绪检查与原子提交语义。

## 内存序与数据竞争

多个协程可以同时跑在多个操作系统线程上。数据竞争是未定义行为。类型系统**不**静态禁止把 `&T` 送进另一个协程（没有 `Send`/`Sync` 约束）：GC 延命解决的是寿命，不是互斥。跨协程共享可变状态是程序员义务，必须走 `chan`、`std.sync`、原子、或只读共享。编译器可以提供竞态检测器构建，但不保证检出全部数据竞争。

## channel 的线性化与公平

每个 `send`、`recv`、`try_send`、`try_recv` 和 `close` 都有一个单一线性化点。阻塞操作在抵达该点前可以挂起当前协程，但不会占用操作系统线程；恢复后操作要么完成，要么按已关闭/无容量的结果返回。`send` 与 `close` 竞态按线性化先后决定：先完成的发送进入缓冲或与接收会合，先完成的关闭使后续发送 panic。关闭不会丢弃在线性化前已完成的缓冲消息。

关闭后的 `recv` 先收完所有已经线性化的缓冲消息，再永久返回 `Err(ChanClosed)`；关闭后的 `try_recv` 立即返回 `Err(TryRecvErr::Closed)`。未关闭且暂时无消息时，`try_recv` 立即返回 `Err(TryRecvErr::Empty)`，绝不等待。无缓冲 `try_send` 只有已有接收者在同一线性化点等待时才成功，否则立即返回 `Err(TrySendErr::Full)`。

`close` 之前的写入与观察到永久关闭结果的 `recv` / `try_recv` 建立 happens-before。父协程在创建 `async` 子协程前完成的写入对该子协程开始执行可见；子协程完成前的写入在 `Join.wait()` 返回后对等待者可见。仅创建 Join 或轮询未完成状态不建立反向同步。

`select` 对就绪分支进行一次原子提交：胜出、相应 channel 的队列变更与 payload 转移（或 Join 完成记录的读取）以及结果发布必须构成同一个操作。返回选中分支表示该操作已经发生，不能先报告选中再补做 send/recv。阻塞期胜出的分支与立即胜出的分支交付同样完整的结果；未选中的分支不消费消息、不观察 Join panic，也不取消子协程。同一个 waitset 的 send/recv 不能相互会合。存在就绪分支时 default 永不获选；不存在就绪分支时才选择 default 或挂起。多个就绪分支的选择是随机的，并满足弱公平：持续保持就绪的分支不能被调度器永久排除。随机种子、具体轮询算法和不同操作系统线程上的执行顺序不是语言可观察保证。

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

CAS 的失败序按语义能力而非枚举序号判定，合法组合固定如下；非法组合不得修改原子值或可见性状态：

| success | 合法 failure |
| --- | --- |
| Relaxed | Relaxed |
| Acquire | Relaxed、Acquire |
| Release | Relaxed |
| AcqRel | Relaxed、Acquire |
| SeqCst | Relaxed、Acquire、SeqCst |

锁交接后，真正接棒的协程拥有对应锁并可正常解锁；登记等待的标识不能被当作新的持锁者。

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
    fn with_read_ref[R, F: Fn(&T) R](self: &Self, f: F) R
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

Gugu 的 `&T` 可写，因此读锁不能暴露内部槽的普通 `&T`；`snapshot()` 和 `with_read` 只产生当前值的语义副本。`with_read_ref` 的 callback 参数是[函数与闭包](functions.md#scoped-borrowed-view-callback)定义的 `ScopedRead` view，只有读取权限且不能逃逸；它在读锁保持期间直接访问内部值，不复制 T。`with_write` 保持原有写入语义，callback 参数是普通锁守卫提供的可写 `&T`。若 T 含可变身份句柄，调用者仍须保证不会绕过锁并发写其载荷。守卫由 Adaptive Resource Leasing 管理：复制守卫共享同一次加锁的 ResourceCell，最后一个租约结束时自动解锁；显式 `unlock` 幂等并让所有副本立即观察已解锁状态。解锁后再 `get` / `snapshot` / `with_read_ref` / `wait` 是 panic。

`Atomic::new`、`Mutex::new`、`RwLock::new`、`Condvar::new` 都是 comptime 可求值构造，可用于普通 static 初始化。标准库为 MutexGuard、RwReadGuard、RwWriteGuard 提供否定 Clone impl；这禁止复制底层加锁动作，但不改变语言按值传递时共享同一资源租约的规则。

守卫 `get` 得到的 `&T` 只允许在对应守卫仍处于已加锁状态时用于同步访问；`unlock` 后继续通过该引用读写属于未同步访问，若与其它访问形成竞争即未定义行为。`Condvar.wait` 释放锁期间不能使用先前取得的引用，返回并重新加锁后可重新获取。锁不提供 poisoning：受保护操作 panic 时 `with_*` 释放锁，但不标记数据是否满足应用不变量。

Mutex 不可重入；同一协程持有守卫时再次 `lock` 会像其它竞争者一样等待，可能造成永久阻塞。标准锁实现必须满足弱公平，持续等待且锁反复可用的协程不能被永久排除。RwLock 不提供隐式读写升级或降级；要切换模式必须先解锁再重新加锁。RwLock 的读写竞争顺序采用目标平台原生策略，Gugu 不保证写者优先、读者优先或严格 FIFO；调用方不能依赖某种公平策略避免特定锁顺序的阻塞。

锁守卫不可 Clone，不能跨所属锁使用；`with_lock` / `with_read` / `with_read_ref` / `with_write` 在闭包正常返回或 panic 展开时都释放锁。显式 `lock` 得到的守卫在最后一个 lease 结束时自动解锁，也可以提前幂等 `unlock`。锁和原子保证同步，不会替程序员把普通 `&T` 的并发写变成安全操作。

任何没有原子、channel、锁、条件变量或其它明确 happens-before 的并发读写，只要至少一方写入同一内存位置，就是数据竞争和未定义行为。GC 延长寿命不改变该规则。
