# 内存与对象模型

本章“对象”指由 managed storage 承载、具有语言身份和 GC 寿命的值，不是 OOP 对象。

## 值与引用

按[值、句柄与传递](passing.md)：`f(x)` 始终合法，并按值语义执行位复制、身份共享、COW 快照或 resource lease 管理；`f(&x)` 引用槽位。Vec、channel、Join 等身份句柄共享权威对象；string、ByteBuffer、Bytes 是 COW/只读值句柄；File、socket 等外部资源共享 ResourceCell。

没有用户可观察的隐式装箱语义；大位结构体按值传递仍产生独立值（lint `large_copy`，不是类型错误）。实现可以选择寄存器、栈、区域或 GC 存储，但不能改变 copy/identity/COW/resource 结果，也必须让仍合法的逃逸引用持续有效。官方编译器的放置算法见 [GIR 与 LIR](../internals/gir-lir.md)。

`static` 项具有进程寿命；`#[coroutine_local] static` 每个协程一份，`#[os_thread_local] static` 每个操作系统线程一份。只要这些槽按语言规则仍存活，它们直接或间接引用的 managed 值就不能被回收。

`MaybeUninit[T]` 在 `assume_init` 前不使其字节中碰巧出现的地址成为活引用。`union` 只允许位类型，重叠字段不产生多个 managed 根。

## `TypeId` 与 `dyn Any`

闭世界程序中的 `TypeId` 唯一性、整数范围、名称和 downcast 结果由[类型系统](types.md#typeid-与-dyn-any)规定。`T → dyn Any` 保存 T 的语义副本：身份句柄继续共享身份，COW 值先完成规范要求的封存，resource 值取得相应 lease；容器死亡时这些值按普通生命周期规则处理。

类型身份、装箱载荷、vtable和扫描/管理 metadata 都是 compiler/runtime 私有表示，见 [GC 元数据](../internals/gc-metadata.md)，不形成额外的语言布局或可解析镜像接口。`dyn Any` 的公开表示只服从[类型系统](types.md)和[平台 ABI](platform-abi.md)。

## 存储与寿命

语义值可以由实现放入寄存器、stack、arena或 managed heap。存储等级、逃逸分析、提升时点和闭包环境布局不可由程序观察；只有本章明确提供的 arena、pin、raw pointer和 FFI 边界对存储寿命提出额外前置条件。

## GC 语义

managed 对象只要能从仍存活的语言值、static、coroutine-local、OS-thread-local、channel/Join/闭包/arena 等安全引用链到达，就必须保持存活；不可达只表示可以回收，不承诺本次分配、某个时间点或进程退出前一定回收。

collector 可以移动 managed 对象。每次 safepoint、等待、恢复和跨线程共享后，所有安全引用仍必须指向原语义对象；安全代码不能通过地址观察移动。`pin` 覆盖的对象在回调期间不得移动，raw pointer只受 [unsafe](unsafe.md) 的显式规则保护。

用户安全代码不调用 `free`，也不能观察 collector内部阶段、空间分区、根编码、屏障或回收线程。内存申请无法满足时按[运行时](runtime.md#fatal)进入 `OutOfMemory` fatal；GC 不运行任意用户 finalizer。

本章不规定 collector算法、heap参数、root编码或写屏障实现。官方 compiler/runtime的当前契约见 [GC 元数据](../internals/gc-metadata.md)与[栈图](../internals/stack-maps.md)；替代实现可以不同，但必须满足本节全部可观察约束。

## Adaptive Resource Leasing

ResourceCell 是外部资源的共享逻辑身份，保存 raw resource、open/closed 状态、受限 release 操作和 lease 状态。创建后尚未发布时只由创建协程访问；发布到 global、channel、async 捕获或其它共享图前必须单向进入共享状态并建立 happens-before，不能再撤销这次发布。

资源值的复制、传参、返回、覆盖和退出遵循[值传递](passing.md)。最后一个 lease 结束时恰触发一次受限 release；显式幂等 close 可以更早把 ResourceCell 原子切到 closed。panic 展开、正常作用域退出和覆盖资源槽都必须产生相同可观察结果。collector 移动承载 lease 的 managed 值不改变 lease 身份或计数。

若最后的 lease 只存在于不可达 managed 容器环内，release 可以推迟到 GC 发现该环；普通作用域中的最后 lease 不依赖 GC 周期。清理可以异步执行，语言不承诺具体执行单元或时刻，只承诺一次性和本节限制；当前 lease 动作与回收队列见 [GIR/LIR](../internals/gir-lir.md)和 [GC 元数据](../internals/gc-metadata.md)。

第三方 FFI package 只能通过 `std.resource` 的受限构造接口登记 release。raw state 必须是无 GC 引用的位值；release 不能捕获 owner、访问 GC 图、复活对象、分配、panic、获取 Gugu 锁、等待 channel 或启动协程。语言不提供任意 `Finalize` trait。

## 移动与 FFI

- collector 可以移动 managed 对象，但必须更新全部安全引用。
- 把引用交给外部函数之前必须 `pin`，或把所需值拷贝到明确的非移动缓冲。
- `#[repr(C)]` 只规定 payload布局，不自动使承载它的 managed 对象地址稳定。

## LocalArena、SyncArena 与 pin

`std.mem.LocalArena` 与 `std.mem.SyncArena` 是两个不同的 lang item 句柄；不存在未区分二者的 `std.mem.Arena`。二者接收不含 resource 字段的 T 并返回区域槽的 `&T`；含 resource 的 T 是编译错误，避免 reset/destroy 绕过 lease release。只要区域句柄仍可达且未 destroy，区域槽中的 managed 引用就保持其目标可达；句柄不可达后区域可以由 GC 回收。

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

两种区域都不提供隐式析构、自动 reset 或自动 destroy；将句柄丢弃只使区域和值变为可回收。`alloc` 不要求 `T: Clone`，但拒绝含 resource 的 T；区域批量释放不会运行用户代码。

`std.mem.pin` 用于 GC 移动对象与 FFI：

```text
fn pin[T, R, F: Fn() R](p: &T, f: F) R
```

`f` 执行期间 `*p` 不会被移动；pin 可以嵌套，固定计数在最外层调用返回或 panic 展开清理完毕后解除。`f` 只能通过传入的槽或闭包捕获访问 `p`；把 `p` 交给外部函数必须在 pin 期间完成，或先拷贝到非移动缓冲。`pin` 不改变引用的类型，也不使 `reset` / `destroy` 后的区域引用重新有效。

## 引用有效性与初始化

安全引用 `&T` 必须始终非空、对齐、指向仍存活且已初始化的 `T` 槽。引用逃逸时实现必须延长目标存储寿命，collector移动对象时必须更新安全引用；因此安全代码不观察地址变化。原始指针不享有此保证，见[unsafe 与 intrinsic](unsafe.md)。

每个类型字段、数组元素和被读取的绑定都必须已经写入有效位模式；读取未初始化值是编译错误，除非本规范明确允许的纯 ZST 例外。把安全引用写入仍存活的 managed 值必须保持目标可达；写入 `MaybeUninit` 或位 union 的普通字节不以目标静态类型建立活引用。


## 逃逸与闭包

见 [函数与闭包](functions.md)。捕获可以延长对象的语义寿命；stack/heap/arena选择与提升由 [GIR 与 LIR](../internals/gir-lir.md)规定，不是用户可观察的所有权检查。

## 与所有权

没有用户级借用检查器、任意 Drop 或 Finalize。寿命不够长时实现延长存储寿命，而不是仅因借用分析拒绝程序；具体提升位置见 [GIR 与 LIR](../internals/gir-lir.md)。普通程序清理逻辑使用 `defer` / `defer ret`；File、socket、Child、管道、锁守卫与第三方外部资源由 Adaptive Resource Leasing 自动执行受限 release。需要观察 flush、commit、shutdown 或 wait 错误时仍必须显式调用相应接口。
