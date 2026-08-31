# 内存与对象模型

本章「对象」= GC 堆分配单元，不是 OOP。

## 值与引用

按[值、句柄与传递](passing.md)：`f(x)` 始终合法，并按具体值描述符执行位复制、身份句柄共享、COW seal 或 resource lease 管理；`f(&x)` 引用槽位。Vec、channel、Join 等身份句柄共享权威对象；string、ByteBuffer、Bytes 是 COW/只读值句柄；File、socket 等外部资源共享 ResourceCell。

没有隐式装箱；大位结构体按值传会真拷贝（lint `large_copy`，不是类型错误）。逃逸的 `&T` 才升到 GC 堆。COW seal 与 resource dup/drop/publish 是固定编译器管理动作，不是用户 Drop。

`static` 项是进程寿命存储；含堆引用的 `static` 必须出现在 GC 全局根里。`#[coroutine_local] static` 的槽是**每个协程一份**，根挂在该协程上，不进全局根表。`#[os_thread_local] static` 的槽挂在当前操作系统线程上，扫描正在该线程上跑的协程时一并扫当前线程的操作系统线程本地槽。

`MaybeUninit[T]` 在 `assume_init` 之前不当作含活引用；精确扫描必须跳过它。`union` 只含位类型，扫描按其位模式对应的静态类型（整个 union 当一块位，不把重叠字段当多根）。

## 类型表与 `dyn Any` 盒子

镜像有一张以 `TypeId.as_int()` 为下标的只读类型表，至少包含规范名、大小、对齐、GC 扫描描述符、COW 管理描述符与 resource descriptor。`downcast` 比较的是这套编号，不是对象头哈希。

`T → dyn Any` 的盒子是普通 GC 对象：头 + T 的值表示。写入载荷按 T 的值描述符产生语义副本；扫描按 GC 描述符走引用槽，死亡时按 resource descriptor 释放仍存活的 lease。T 是身份句柄时载荷继续指向 VecObj / channel；T 是 string/ByteBuffer 时 backing 已密封；T 含资源时盒子属于可逐对象回收的资源区域。

`dyn Any` 本身是胖指针句柄，规则与其它 `dyn Trait` 相同。

## 分配等级

1. 寄存器 / 栈槽（标量、不逃逸的小聚合、ZST）
2. 栈上的不逃逸聚合与闭包环境（逃逸分析）
3. `LocalArena` / `SyncArena`：成批分配、成批释放；二者的并发边界见本章。
4. GC 堆：不规则寿命、图结构、跨协程共享

禁止把 1–3 默认降成 4。

## GC：并发分代 Immix

必须同时满足：高吞吐（接近手写分配器）、低占用（能挪动/整理）、高并发（多条操作系统线程同时分配与回收）、与百万级有栈协程共存。

因此算法钉死为：

**精确、分代、可移动、并发标记、并发回收、按逻辑处理器分配。**

具体义务：

| 项 | 要求 |
|----|------|
| 精确 | 编译器生成栈图、寄存器图、全局根。禁止 conservative 扫描。 |
| 分配 | 每个正在运行的操作系统工作线程有线程本地分配缓冲（TLAB，thread-local allocation buffer）/ 线（Immix 把堆分成 block，block 再分成 line；小对象在当前 line 上 bump）。无锁。TLAB 耗尽后从全局堆取 span，全局路径用细粒度锁或无锁结构，禁止「全堆一把大锁」当快路径。 |
| 年轻代 | 并行拷贝（evacuate：把活对象搬到新区域）。大多数对象夭折。 |
| 老年代 | Immix：按 line / block 标记，机会性 evacuate 做碎片整理。低占用靠这个，而不是靠永不移动。 |
| 标记 | 并发、多线程。mutator 与 collector 同时跑。 |
| 回收 | 并发 sweep / 回收空 block；空页可以还回 OS。 |
| 屏障 | **写屏障**是 IR 原语（分代 + 并发标记需要）。热路径的**读**是普通 load，**不上读屏障**。 |
| 根 | 所有可运行与挂起的协程栈、寄存器、全局、`#[coroutine_local]` 槽、当前操作系统线程的 `#[os_thread_local]`、channel 缓冲、TLAB，以及仍可从这些根到达且未 `destroy` 的 `LocalArena` / `SyncArena` 区域。 |
| 握手 | 与抢占共用 safepoint。正在跑的协程到达 safepoint 后才扫描其寄存器；阻塞的协程栈已经冻结。 |

不采用 ZGC/Shenandoah 那种**每条引用读都加 load barrier / 着色指针**作为默认：它把暂停压到亚毫秒，但会把「接近 C++」的指针追逐打出可见缺口。本语言的暂停目标靠年轻代短、并发标记老年代、以及少分配来达成，而不是靠给每次 load 加税。

不采用纯引用计数：普通 GC 引用在多核上使用原子 RC 会使所有指针更新变贵，循环仍需 tracing。Adaptive Resource Leasing 只给稀少的外部资源句柄计 lease，不改变普通对象分配与引用写路径。

用户代码在安全子集里不调用 `free`。跨线程“释放”包括 collector 并发回收、区域显式 reset/destroy，以及 ResourceCell 最后 lease 的受限 release。

## Adaptive Resource Leasing

ResourceCell 从不移动、无 GC 引用的专用 slab 分配，保存 raw resource、open/closed 状态、受限 release 函数和 lease 状态。创建时为 `Local(owner_coroutine)`；发布到 global、channel、async 捕获或共享 GC 图前，必须单向转换为 `Shared` 并建立 happens-before。Local lease 使用非原子计数，Shared lease 使用原子计数，不再降回 Local。

编译器用闭世界摘要选择 borrow、transfer、dup、drop 与 publish。最后一个局部或共享 lease 结束时触发一次 release；显式幂等 close 可以更早把 ResourceCell 原子切到 closed。panic 展开、正常作用域退出和覆盖资源槽都必须运行相同 drop glue。collector 搬迁对象只更新地址，不改变 lease。

含 resource 字段的堆对象不能进入会整区丢弃死亡对象而不逐对象访问的 nursery。它们进入可并发逐对象 sweep 的资源区域；类型表的 resource descriptor 在对象死亡时 drop 字段。普通对象继续使用 TLAB、年轻代并行复制和无读屏障路径。

若 ResourceCell 的最后 lease 只存在于不可达 GC 容器环内，release 可以延迟到 tracing GC 发现并 sweep 该环。常见栈上最后 lease 不依赖 GC。collector 线程只执行固定 descriptor 和无用户代码的计数操作；可能等待的底层清理由 runtime 清理队列执行。

第三方 FFI package 只能通过 `std.resource` 的受限构造接口登记 release。raw state 必须是无 GC 引用的位值；release 不能捕获 owner、访问 GC 图、复活对象、分配、panic、获取 Gugu 锁、等待 channel 或启动协程。语言不提供任意 `Finalize` trait。

## 写屏障与移动

- 把堆引用写入堆字段必须经过编译器插入的写屏障。
- 年轻代拷贝与 Immix evacuate 会移动对象。栈与寄存器里的指针在 safepoint 更新。
- FFI：把引用交给外部函数之前必须 `pin`（禁止移动）或拷贝到非移动缓冲。`#[repr(C)]` 结构体默认不作为可移动 GC 对象的载荷直传。

## LocalArena、SyncArena 与 pin

`std.mem.LocalArena` 与 `std.mem.SyncArena` 是两个不同的 lang item 句柄；不存在未区分二者的 `std.mem.Arena`。二者把不含 resource 字段的元素按值描述符写进区域，并返回指向区域槽的 `&T`。`T` 的 descriptor 含 resource 是编译错误，避免 reset/destroy 绕过 lease drop。只要区域句柄仍可达且未 destroy，区域槽就按 `T` 的 GC 描述符扫描；句柄不可达后区域可由 GC 回收。

`LocalArena` 在创建它的协程中确定唯一所有者。句柄值可以在该协程内复制；其它协程即使与所有者不并发，也不能调用它的 `alloc`、`reset` 或 `destroy`。区域内元素的只读引用可以传递给其它协程，但所有者在这些引用可能被使用期间不得 reset/destroy，且任何并发写入都必须使用同步原语。违反这些约束是未定义行为。

```text
struct LocalArena

fn new() LocalArena
fn with_capacity(n: int) LocalArena
fn alloc[T](self: &Self, v: T) &T
unsafe fn reset(self: &Self)
unsafe fn destroy(self: &Self)
```

`SyncArena` 的分配状态可由多个协程和操作系统线程共享；`alloc` 内部同步，返回的引用在 `reset` / `destroy` 前保持有效。`reset` 与 `destroy` 仍是 `unsafe`，调用者必须保证没有并发的 `alloc`、没有正在访问的区域槽，并且之后不再使用被返回的任何 `&T` 或该区域句柄。二者完成后，区域可以重新 `alloc`（`destroy` 除外）；`destroy` 后句柄与旧引用均不可使用。`with_capacity` 的 `n < 0` 在 comptime 是编译错误，运行时 panic。

```text
struct SyncArena

fn new() SyncArena
fn with_capacity(n: int) SyncArena
fn alloc[T](self: &Self, v: T) &T
unsafe fn reset(self: &Self)
unsafe fn destroy(self: &Self)
```

两种区域都不提供隐式析构、自动 reset 或自动 destroy；将句柄丢弃只使区域成为不可达对象，最终由 GC 回收其区域元数据。`alloc` 不要求 `T: Clone`，但拒绝含 resource descriptor 的 T；区域批量释放不会运行用户代码。

`std.mem.pin` 用于 GC 移动对象与 FFI：

```text
fn pin[T, R, F: Fn() R](p: &T, f: F) R
```

`f` 执行期间 `*p` 不会被移动；pin 可以嵌套，固定计数在最外层调用返回或 panic 展开清理完毕后解除。`f` 只能通过传入的槽或闭包捕获访问 `p`；把 `p` 交给外部函数必须在 pin 期间完成，或先拷贝到非移动缓冲。`pin` 不改变引用的类型，也不使 `reset` / `destroy` 后的区域引用重新有效。

## 引用有效性与初始化

安全引用 `&T` 必须始终非空、对齐、指向仍存活且已初始化的 `T` 槽。栈槽在引用逃逸时升到 GC 堆，GC 移动对象时更新所有安全引用；因此安全代码不观察地址变化。原始指针不享有此保证，见[unsafe 与 intrinsic](unsafe.md)。

每个类型字段、数组元素和被读取的绑定都必须已经写入有效位模式；读取未初始化值是编译错误，除非本规范明确允许的纯 ZST 例外。写入一个已初始化的堆字段必须经过写屏障；写入 `MaybeUninit` 或位 union 不以该字段的静态类型作为活引用根。


## 逃逸与闭包

见 [函数与闭包](functions.md)。语义上捕获可以延长对象寿命（GC）；优化上不逃逸则栈分配。

## 与所有权

没有用户级借用检查器、任意 Drop 或 Finalize。寿命不够长时对象升到 GC 堆，而不是编译失败。普通程序清理逻辑使用 `defer` / `defer ret`；File、socket、Child、管道、锁守卫与第三方外部资源由 Adaptive Resource Leasing 自动执行受限 release。需要观察 flush、commit、shutdown 或 wait 错误时仍必须显式调用相应接口。
