# 内存与对象模型

本章「对象」= GC 堆分配单元，不是 OOP。

## 值与引用

按 [值、句柄与传递](passing.md)：`f(x)` 浅拷表示，`f(&x)` 引用槽位。`string`、切片、`chan[T]`、`Vec`、胖 `fn`、`dyn Trait`、`Join[T]` 是句柄——拷贝句柄即共享载荷。用户结构体是位加字段的浅拷，不是「在 GC 里就不能按值传」。

没有隐式装箱；大结构体按值传会真拷贝（lint `large_copy`，不是类型错误）。逃逸的 `&T` 才升到 GC 堆。

`static` 项是进程寿命存储；含堆引用的 `static` 必须出现在 GC 全局根里。`#[coroutine_local] static` 的槽是**每个协程一份**，根挂在该协程上，不进全局根表。`#[os_thread_local] static` 的槽挂在当前操作系统线程上，扫描正在该线程上跑的协程时一并扫当前线程的操作系统线程本地槽。

`MaybeUninit[T]` 在 `assume_init` 之前不当作含活引用；精确扫描必须跳过它。`union` 只含位类型，扫描按其位模式对应的静态类型（整个 union 当一块位，不把重叠字段当多根）。

## 类型表与 `dyn Any` 盒子

镜像有一张以 `TypeId.as_int()` 为下标的只读类型表，至少包含规范名、大小、对齐、以及「若该类型作为值落在 GC 对象里该怎么扫」的描述符。`downcast` 比较的是这套编号，不是对象头哈希。

`T → dyn Any` 的盒子是普通 GC 对象：头 + `T` 的值表示（浅拷）。扫描按 `T` 的描述符走载荷槽。`T` 是句柄时，载荷就是那几个句柄字，继续追到 `VecObj` / channel 对象。盒子的 GC 类型（对象头里实现用的索引）可以与载荷的语言 `TypeId` 不同；语言只保证 vtable（或等价槽）里能读到载荷的 `TypeId`。

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

不采用纯引用计数：原子 RC 在多核上贵，循环仍要追踪。

用户代码在安全子集里不调用 `free`。跨线程「释放」指 collector 并发回收，以及区域的显式 `reset` / `destroy`。

## 写屏障与移动

- 把堆引用写入堆字段必须经过编译器插入的写屏障。
- 年轻代拷贝与 Immix evacuate 会移动对象。栈与寄存器里的指针在 safepoint 更新。
- FFI：把引用交给外部函数之前必须 `pin`（禁止移动）或拷贝到非移动缓冲。`#[repr(C)]` 结构体默认不作为可移动 GC 对象的载荷直传。

## LocalArena、SyncArena 与 pin

`std.mem.LocalArena` 与 `std.mem.SyncArena` 是两个不同的 lang item 句柄；不存在未区分二者的 `std.mem.Arena`。二者都把元素按值浅拷进区域，并返回指向区域槽的 `&T`。只要区域句柄仍可达且未 `destroy`，区域槽就按 `T` 的 GC 描述符扫描；句柄不可达后区域可由 GC 回收。

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

两种区域都不提供隐式析构、自动 reset 或自动 destroy；将句柄丢弃只使区域成为不可达对象，最终由 GC 回收其区域元数据。`alloc` 只浅拷贝 `v`，不要求 `T: Clone`；区域的批量释放不会运行用户析构函数，因为语言没有 `Drop`。

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

没有用户级借用检查器。寿命不够长时对象升到 GC 堆，而不是编译失败。没有 `Drop` / RAII：资源收尾用 `defer` / `defer ret`。
