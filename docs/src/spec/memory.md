# 内存与对象模型

本章规定内存与对象模型：值与引用的存储等级、GC 的可观察语义、Adaptive Resource Leasing、区域分配与 `pin`。实现侧的放置与屏障策略见 [GIR 与 LIR](../internals/gir-lir.md)。

本章“对象”指由受管存储（managed storage）承载、具有语言身份和 GC 寿命的值，不是 OOP 对象。

## 值与引用

按[值、句柄与传递](passing.md)：`f(x)` 始终合法，并按值语义执行位复制、身份共享、COW 快照或资源租约管理；`f(&x)` 引用槽位。Vec、channel、Join 等身份句柄共享权威对象；string、ByteBuffer、Bytes 是 COW/只读值句柄；File、socket 等外部资源共享 ResourceCell。

没有用户可观察的隐式装箱语义；大位结构体按值传递仍产生独立值（lint `large_copy`，不是类型错误）。实现可以选择寄存器、栈、区域或 GC 存储，但不能改变复制/身份/COW/资源结果，也必须让仍合法的逃逸引用持续有效。官方编译器的放置算法见 [GIR 与 LIR](../internals/gir-lir.md)。

`static` 项具有进程寿命；`#[coroutine_local] static` 每个协程一份，`#[os_thread_local] static` 每个操作系统线程一份。只要这些槽按语言规则仍存活，它们直接或间接引用的受管值就不能被回收。

`static` 的初始化是惰性的：初始化器在声明所在域首次被求值时执行，其值只有在构造完成后才对读者可见；构造期间发生 panic 时该槽保持未初始化，读者不会观察到半构造内容。初始化完成后对静态槽的读都返回同一存储的值，写入对所有读者可见，而不是每个读者各持一份副本。

`MaybeUninit[T]` 在 `assume_init` 前不使其字节中碰巧出现的地址成为活引用。`union` 只允许位类型，重叠字段不产生多个受管根。

## `TypeId` 与 `dyn Any`

闭世界程序中的 `TypeId` 唯一性、整数范围、名称和 downcast 结果由[类型系统](types.md#typeid-dyn-any)规定。`T → dyn Any` 保存 T 的语义副本：身份句柄继续共享身份，COW 值先完成规范要求的封存，资源值取得相应租约；容器死亡时这些值按普通生命周期规则处理。

类型身份、装箱载荷、vtable和扫描/管理 metadata 都是 compiler/runtime 私有表示，见 [GC 元数据](../internals/gc-metadata.md)，不形成额外的语言布局或可解析镜像接口。`dyn Any` 的公开表示只服从[类型系统](types.md)和[平台 ABI](platform-abi.md)。

## 存储与寿命

语义值可以由实现放入寄存器、栈、区域或受管堆。存储等级、逃逸分析、提升时点和闭包环境布局不可由程序观察；内部区域停止分配或被替换，不得使仍由安全引用或活动 pin 可达的对象及其字段失效。只有本章明确提供的 arena、pin、raw pointer 和 FFI 边界对存储寿命提出额外前置条件。

## GC 语义

受管对象只要能从仍存活的语言值、static、coroutine-local、OS-thread-local、channel/Join/闭包/arena 等安全引用链到达，就必须保持存活；不可达只表示可以回收，不承诺本次分配、某个时间点或进程退出前一定回收。

收集器可以移动受管对象。每次 safepoint、等待、恢复和跨线程共享后，所有安全引用仍必须指向原语义对象；安全代码不能通过地址观察移动。`pin` 覆盖的对象在回调期间不得移动，raw pointer 只受 [unsafe](unsafe.md) 的显式规则保护。

用户安全代码不调用 `free`，也不能观察收集器内部阶段、空间分区、根编码、屏障或回收线程。内存申请无法满足时按[运行时](runtime.md#fatal-resource-exhaustion)进入 `OutOfMemory` fatal；GC 不运行任意用户 finalizer。
LocalHeap 的运行时布局版本在引入独立大映射记录后递增；镜像与编译器必须拒绝不匹配的 `local_heap_schema`，不能把旧版 side metadata 当作新版解释。

实现可以把超过普通分配单元容量或需要更高对齐的受管对象放入独立的 non-moving range；这不改变对象的可达性、初始化、`pin` 或 OOM 语义。该 range 不是用户可观察的地址稳定承诺：只有规范明确的 `pin`/FFI 生命周期才允许依赖地址有效性，普通安全引用仍由收集器按其内部表示维护。独立映射中的受管字段同样参与追踪，写入后的年轻引用必须在下一次收集期间保持有效；指向 payload 内部的引用保留整个 allocation。最后一个根或 pin 租约消失后，大对象可以被回收，其提交字节必须从堆和共同映射账本一并扣除。

本章不规定收集器算法、heap 参数、root 编码或写屏障实现。官方 compiler/runtime 的当前契约见 [GC 元数据](../internals/gc-metadata.md)与[栈图](../internals/stack-maps.md)；替代实现可以不同，但必须满足本节全部可观察约束。

LocalHeap 的 collector 阶段不属于用户可观察 API：minor 可以复制 nursery 对象并更新全部安全根，
major 必须以精确 trace 的 root/object-start/mark 结果决定存活；raw pointer 不会因为数值碰巧落在
对象 payload 内而成为 root。`static`、dynamic root、闭包捕获、interior root 和 compressed root
按各自登记的 root kind 处理；非法 root kind、过期 generation、越界 compressed offset、损坏
descriptor 或重复 return ticket 都进入 `RuntimeInvariant`/`OutOfMemory` 对应 fatal 路径，不得
静默按普通 direct pointer 处理。

runtime 可以在不改变语言语义的前提下，为地址稳定的 control、stack、resource 和 GC range descriptor 使用 owner-directed return message。该机制不产生用户级 `free`，不使受管地址稳定，不承诺 processor affinity 或回收时刻，也不能让安全代码观察 raw slab、route key 或 batch 延迟。

跨 owner 发布过的受管对象以稳定句柄暴露：句柄由 `table/slot/generation` 组成，table 是逻辑表身份而不是宿主地址，每次 slot 复用都会推进 generation，因此携带旧 generation 的句柄必须被拒绝，而不允许按 slot 猜测对象。句柄必须经 access guard 解析后才可访问，稳定身份本身不能用于地址算术；guard 期间该句柄捕获的 payload 保持有效；`pin` 把访问提升为不移动租约。活动 guard 或 pin 不得被 GC 回收越过，租约结束后保留的过期身份不能访问复用后的对象。发布之后不得把任何 direct pointer 当作长期身份继续使用——它既不是句柄，也不能跨 owner 传递。

## Adaptive Resource Leasing {#adaptive-resource-leasing}

ResourceCell 是外部资源的共享逻辑身份，保存 raw resource、open/closed 状态、受限 release 操作和租约状态。创建后尚未发布时只由创建协程访问；发布到 global、channel、async 捕获或其它共享图前必须单向进入共享状态并建立 happens-before，不能再撤销这次发布。每次 slot 复用都推进 cell generation；携带旧 generation 的 ResourceHandle 即使再次命中相同 descriptor/index 也必须被拒绝。

资源值的复制、传参、返回、覆盖和退出遵循[值传递](passing.md)。最后一个租约结束时恰触发一次受限 release；显式幂等 close 可以更早把 ResourceCell 原子切到 closed。closed、已进入 release 队列、cleanup 已完成或正在回收的 cell 都不能新增租约。panic 展开、正常作用域退出和覆盖资源槽都必须产生相同可观察结果；聚合或 projection 覆盖按字段结束旧租约，不能因外层结构赋值而遗漏字段。收集器移动承载租约的受管值不改变租约身份或计数。

若最后的租约只存在于不可达受管容器环内，release 可以推迟到 GC 发现该环；普通作用域中的最后租约不依赖 GC 周期。清理可以异步执行，语言不承诺具体执行单元或时刻，只承诺一次性和本节限制。shutdown 必须排空 owner inbox、grace node、pending release ticket 与 active Resource slot，再释放没有 live slot 的 resource range；当前租约动作与回收队列见 [GIR/LIR](../internals/gir-lir.md)和 [GC 元数据](../internals/gc-metadata.md)。

第三方 FFI package 只能通过 `std.resource` 的受限构造接口登记 release。raw state 必须是无 GC 引用的位值；release 不能捕获 owner、访问 GC 图、复活对象、分配、panic、获取 Gugu 锁、等待 channel 或启动协程。语言不提供任意 `Finalize` trait。
收集器执行的编译器生成资源遍历同样不得分配受管对象或递归进入收集；该限制覆盖数组、枚举和嵌套聚合的字段解释过程，不只覆盖最终 release 回调。
ResourceCell 的受限 cleanup 完成后可以进入 raw owner return；租约的一次性线性化、close 结果和 panic/作用域一致性仍由本章规定，不能由 message queue 的消费时刻决定。标准库侧的封装与 `std.resource` 接口见[标准库](standard-library.md#adaptive-resource-leasing)。

## 移动与 FFI

- 收集器可以移动受管对象，但必须更新全部安全引用。
- 把引用交给外部函数之前必须 `pin`，或把所需值拷贝到明确的非移动缓冲。
- `#[repr(C)]` 只规定 payload 布局，不自动使承载它的受管对象地址稳定。

## LocalArena、SyncArena 与 pin

`std.mem.LocalArena` 与 `std.mem.SyncArena` 是两个不同的 lang item 句柄；不存在未区分二者的 `std.mem.Arena`。二者接收不含资源字段的 T 并返回区域槽的 `&T`；含资源字段的 T 是编译错误，避免 reset/destroy 绕过租约 release。只要区域句柄仍可达且未 destroy，区域槽中的受管引用就保持其目标可达；句柄不可达后区域可以由 GC 回收。

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

`pin` 可以在调用 `f` 之前执行一次收集或提升；这期间调用方的安全引用必须由精确根保持并更新，不能提前缓存裸地址。`f` 开始后，活动租约保持该 allocation 可达且地址不变；正常返回和 panic 展开都必须结清本次取得的租约；内部采用普通函数调用或其它降级形式不得改变这一语义。pin 不递归固定字段引用的对象，但这些字段继续遵守普通 GC 可达性与写屏障规则。

区域分配与 pin 的用法：

<!-- docs-example: parse -->
```gugu
fn demo() {
    let arena = LocalArena::new()
    let node = arena.alloc(Point { x: 1, y: 2 })
    _ = node.x
}

fn export(p: &Point) {
    std.mem.pin(p, fn() {
        write_record(p)
    })
}
```

## 引用有效性与初始化

安全引用 `&T` 必须始终非空、对齐、指向仍存活且已初始化的 `T` 槽。引用逃逸时实现必须延长目标存储寿命，收集器移动对象时必须更新安全引用；因此安全代码不观察地址变化。原始指针不享有此保证，见[unsafe 与 intrinsic](unsafe.md)。

每个类型字段、数组元素和被读取的绑定都必须已经写入有效位模式；读取未初始化值是编译错误，除非本规范明确允许的纯 ZST 例外。把安全引用写入仍存活的受管值必须保持目标可达；写入 `MaybeUninit` 或位 union 的普通字节不以目标静态类型建立活引用。

精确根只包含活动 enum 变体中已经初始化的引用字段，不能把变体的根偏移并集直接视作活引用：同一偏移在另一变体中可能是整数或判别值。整值复制和入口按值参数同样按活动变体更新根有效位。内部引用归属解析必须先证明地址落在 arena 的有效 payload 范围；arena 起点及首个对象 header 中的地址不是 payload，不能经无符号减法下溢变成位图下标。
同一根字在安全点前的多次初始化/失效事件以最后一次为准；本块无事件时只能使用所有前驱共同保证的初始化事实。
分配游标允许等于已打开块的末端；收集器仍必须把该游标归属到原 arena，不能把 one-past-end 地址误当成下一 arena 并清除另一组存活对象。

## 逃逸与闭包

见 [函数与闭包](functions.md)。捕获可以延长对象的语义寿命；栈/堆/区域选择与提升由 [GIR 与 LIR](../internals/gir-lir.md)规定，不是用户可观察的所有权检查。

## 与所有权语言的关系 {#ownership-relation}

没有用户级借用检查器、任意 Drop 或 Finalize。寿命不够长时实现延长存储寿命，而不是仅因借用分析拒绝程序；具体提升位置见 [GIR 与 LIR](../internals/gir-lir.md)。普通程序清理逻辑使用 `defer` / `defer ret`；File、socket、Child、管道、锁守卫与第三方外部资源由 Adaptive Resource Leasing 自动执行受限 release。需要观察 flush、commit、shutdown 或 wait 错误时仍必须显式调用相应接口。
