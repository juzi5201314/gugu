# unsafe 与 intrinsic

没有 `unsafe`，GC、调度、channel 就必须用另一种语言写。`unsafe` 是语言的一部分。

## 安全子集

不进入 `unsafe` 的代码必须：

- 不把整数当指针解引用
- 不绕过 compiler/runtime 对受管引用更新的要求
- 不越界（无检查下标只在 `unsafe`）
- 不破坏 `string` 的 UTF-8
- 不把未初始化内存当已初始化值读（ZST 与 `MaybeUninit` 除外）
- 不读 `union` 里当前未写入的字段
- 不对自然对齐不足的 `#[repr(packed)]` 字段取 `&T` 当已对齐引用用
- 不制造数据竞争

## `unsafe fn`、`unsafe trait`、`unsafe impl` 与 `unsafe` 块

- `unsafe fn`：调用方必须在 `unsafe` 里调。
- `unsafe trait`：每个肯定实现都必须写 `unsafe impl`；实现者负责维持 trait 文档规定、编译器无法验证的不变量。调用其安全方法不因此需要 unsafe 块。
- `unsafe impl Trait for Type`：只允许实现 unsafe trait。违反所声明的不变量属于未定义行为；unsafe impl 的方法体仍需用显式 unsafe 块执行其它 unsafe 操作。
- `unsafe { }`：此处由程序员维持不变量。
- 安全函数可以内含 `unsafe` 块，用来封装原语。
- `#[naked]` 函数必须是 `unsafe fn`。

`unsafe` 不关闭 GC，也不关闭类型检查。

`std.hash.StableHash` 与 `std.cmp.StableOrd` 是标准 unsafe marker trait。编译器验证通过的 derive 可以生成其 impl，源码无需也不能伪装成普通安全 impl；手写实现必须使用 `unsafe impl`。若键的 Eq、Hash 或 Ord 结果后来能通过外部别名改变，则该 unsafe impl 的证明失效。

## 原始指针 `*T`

绑定默认可变，因此不拆 `*const` / `*mut`。`*T`：

- 可以为 0，可以不对齐，可以悬空
- 拷贝地址
- 解引用必须在 `unsafe` 中

`&T` → `*T`、`uint` → `*T` 必须写成类型构造：`(*T)(&x)`、`(*T)(addr)`。`*T` 转回 `&T` 必须由程序员保证非空、存活、对齐。

`std.ptr` 提供（lang item——编译器按名字挂钩的标准库项，必须存在，见 [概述 · 术语](overview.md#术语)）：

```text
fn addr_of[T](place: T) *T
unsafe fn ptr_read[T](p: *T) T
unsafe fn ptr_write[T](p: *T, v: T)
unsafe fn read_unaligned[T](p: *T) T
unsafe fn write_unaligned[T](p: *T, v: T)
unsafe fn volatile_load[T](p: *T) T
unsafe fn volatile_store[T](p: *T, v: T)
```

`addr_of` 是接收 place 的特殊 intrinsic：只计算槽地址，不读取值，也不构造可能未对齐的 `&T`；非 place 实参是编译错误。`ptr_read` / `ptr_write` 按位访问，只允许不带 COW 或 resource 管理语义的类型；string、ResourceCell 句柄或含资源字段的类型是编译错误，必须使用普通赋值或领域 API。`ptr_read` 不把源当成已移走，`ptr_write` 不运行管理动作，二者要求自然对齐。`read_unaligned` / `write_unaligned` 只接受位类型，允许未对齐地址并逐 byte 等价复制。`volatile_*` 仍要求自然对齐，只保证该次访存不被删除、合并或移出其它 volatile 访存的顺序；volatile 不是原子操作，不建立 happens-before。悬空、范围不足、无效位模式、数据竞争或绕过受管引用更新要求仍是未定义行为。

## `union`

```
union Word {
    i: int
    f: float
}
```

- 所有字段从偏移 0 开始重叠。默认 `#[repr(C)]` 布局（声明顺序、不重排）。可加 `#[repr(align(N))]`。
- 字段默认私有，`pub` 打开。可见性与结构体相同。
- **字段必须是位类型**（标量、只含位的结构体/数组/枚举/newtype）。禁止句柄、`&T`、`string`、`Vec`、`chan`、`dyn`、胖 `fn`：GC 无法知道哪一字段活着。
- 构造：`Word { i: 1 }`，必须恰好写一个字段。
- 读、写字段必须在 `unsafe` 里。读一个不是最近写入的字段：若位模式对该字段的类型无效，未定义行为。
- 有稳定地址。不是句柄。

## `MaybeUninit[T]`

`std.mem.MaybeUninit[T]` 是 lang item（见 [概述 · 术语](overview.md#术语)），布局与 `T` 相同。带 resource 管理语义的 T 不能实例化 MaybeUninit，避免覆盖或未初始化状态绕过 lease release。其它 T 的 GC 扫描与初始化状态由编译器精确跟踪，直到 `assume_init` 前不能把未写入槽当成有效 T 使用。

```
fn uninit[T]() MaybeUninit[T]
fn new[T](v: T) MaybeUninit[T]
fn as_ptr(self: &Self) *T
fn write(self: &Self, v: T)
unsafe fn assume_init(self) T
```

`uninit` 不初始化。`write` 写入一个此前未初始化的槽；对同一实例重复 write 而未先 `assume_init` 是未定义行为，不能借此覆盖活 GC 引用。`assume_init` 把位当成已初始化的 T：若尚未写入有效值，未定义行为。`as_ptr` 本身安全；解引用仍要 unsafe。ZST 的 MaybeUninit 与 T 一样不占空间。

`assume_init` 始终是 `unsafe fn`，调用必须在 `unsafe` 块里。对纯 ZST（见 [类型](types.md)），安全前置条件恒成立——没有待初始化的位——因此调用不是未定义行为。这不是安全重载，也不免除 `unsafe` 块。安全子集允许直接读取未初始化的纯 ZST（本章开头那条例外），不必经过 `MaybeUninit`。

## `transmute` 与 `unreachable`

`std.mem.transmute[T, U](x: T) U`：按位重解释。必须在 `unsafe` 里。`size_of[T]()` 必须等于 `size_of[U]()`，否则编译错误。T 或 U 带 COW 或 resource 管理语义时也是编译错误；transmute 不能伪造、复制或漏掉管理动作。其它结果若对 U 无效（含伪造 runtime 私有状态、破坏 UTF-8 或 niche）是未定义行为。

`std.hint.unreachable() !`：告诉编译器不可达。若运行到此处，未定义行为。必须在 `unsafe` 里调用。安全的发散用 `panic`。

## intrinsic

绑定到 IR 原语，不是「内联汇编包装函数」所能替代：

| 职责 | 说明 |
|------|------|
| 受管分配 / 区域 | managed storage、`LocalArena` / `SyncArena` 上的未初始化内存；OS `mmap` / `VirtualAlloc` |
| 受管引用更新 | 手写 runtime 对 GC 引用槽的更新；当前屏障见 [GC 元数据](../internals/gc-metadata.md#write-barrier-与-remembered-set) |
| 栈切换 | 保存目标 ABI 状态并切换执行栈；当前 context见[调度器](../internals/scheduler.md) |
| 栈边界 / SP | GC 与溢出探测 |
| 调度/GC 轮询 | `std.runtime.safepoint_poll()`；检查抢占与 GC stop，可能挂起当前协程 |
| 根与类型 metadata | 向配套 runtime 登记精确根和类型信息；编码见[栈图](../internals/stack-maps.md)与[GC 元数据](../internals/gc-metadata.md) |
| 原子 | `xchg`、`cas`、acquire/release/seqcst；channel 与调度握手 |
| 系统调用 | Linux `syscall`；Windows 对导入符号的调用 |
| 无检查索引 / 转换 | 误用即未定义行为 |
| pin | 禁止移动，供 FFI |
| comptime 嵌入文件 | `std.mem.embed_file`，只在 comptime 合法；签名见 [编译期执行](comptime.md) |
| `size_of` / `align_of` / `offset_of` / `type_id` / `type_id_count` | 见 [类型](types.md) |
| volatile / 指针读写 | 见上 |
| `transmute` | 见上 |

未定义行为包括：野指针、数据竞争、破坏 UTF-8 或 runtime 私有状态、遗漏受管引用更新、在编译器未登记的停止点读取根 metadata，以及对 `union` / `MaybeUninit` / `transmute` 使用无效位模式。当前官方 metadata与屏障契约只见[栈图](../internals/stack-maps.md)和[GC 元数据](../internals/gc-metadata.md)。调试器可以抓一部分；没炸不是定义。

## `asm` 与 `global_asm`

内联汇编是表达式，必须在 `unsafe` 里：

```text
asm(
    "mov %rdi, %rax; add %rsi, %rax",
    in("rdi") left,
    in("rsi") right,
    lateout("rax") sum,
    clobber("cc")
)
```

- 第一个实参是 comptime `string`（或 `raw"..."`）。tier-1 目标统一使用 AT&T 表面语法，与 GNU as 常见记法兼容；同一份源码不能按实现选择切换到 Intel 语法。
- `in("reg") expr`：进入时该寄存器保存 `expr` 的值。
- `out("reg") place` / `lateout("reg") place`：退出时写进可赋值位置。
- `clobber("reg"...)`：这些寄存器与 `memory` / `cc` 被破坏。必须声明，否则根 metadata 与寄存器分配无效。

- 类型是 `()`。禁止在 `#[naked]` 以外靠它「返回」值而不走 `out`。

`global_asm("...")` 是模块顶层声明。字符串必须 comptime。汇编进镜像，不经 Gugu 函数 prologue。它定义的符号只能通过显式 `extern "C"` 声明从 managed code 调用：未标注声明走普通 `ForeignBridge`，长时间 CPU work 使用 `#[ffi(dirty_cpu)]`，只有满足完整 leaf 契约时才能使用 `#[ffi(leaf(stack = N))]`。compiler 不解析字符串来猜符号与调用模式。

`#[naked] unsafe extern "C" fn`：compiler 不生成 prologue / epilogue 或普通帧的根与展开 metadata。函数体必须是**恰好一次** `asm(...)` 调用（可带 `clobber`）。从 managed context 调用时默认按 `ForeignBridge[DirtyCpu]` 进入；只有显式 `#[ffi(leaf(stack = N))]` 才允许直接按 leaf 调用。runtime/rt0 在不持有用户 coroutine、processor 或 GC root 状态时可以使用 compiler 内部 direct path。

带函数体的 `#[ffi(dirty_cpu)] unsafe extern "C" fn` 是 opaque native definition：允许内部回边、等待指令和不能生成普通 stack map 的 asm，但整个函数不能包含 Gugu managed reference、resource lease、分配、panic、suspend、Gugu 函数调用或需要 compiler safepoint 的操作；参数、返回值和局部值只能是 C ABI 可表示的 bit value/raw pointer。它从 managed context 调用时按 `ForeignBridge[DirtyCpu]` 执行，不能回调 Gugu。没有该属性的普通函数不能借助 asm 隐藏上述操作。

## 链接属性

| 属性 | 用在 | 含义 |
|------|------|------|
| `#[export_name = "sym"]` | 有函数体的 `fn` | 导出符号名。可与 `pub extern "C"` 一起用。 |
| `#[link_name = "sym"]` | 无体 `extern` 项 | 导入符号名，覆盖声明名。 |
| `#[link_section = ".text.foo"]` | `fn`、`static`、`global_asm` | 放入指定节。 |
| `#[used]` | `static`、`fn` | 即使未被引用也不得裁掉。 |
| `#[naked]` | `unsafe extern "C" fn` | 见上。 |

未知节名在目标格式上不合法则编译错误。

## FFI

`extern` 声明导入或导出 C ABI 函数：

```
extern "C" fn puts(s: *byte) int

extern "C" {
    fn puts(s: *byte) int
    fn abort() !
}

pub extern "C" fn gugu_on_load() {
    ...
}

#[link_name = "NtClose"]
extern "C" fn nt_close(h: *byte) i32
```
导入项可以声明调用效应，也可以在调用点覆盖：

```text
extern "C" {
    #[ffi(leaf(stack = 256))]
    fn strlen(s: *byte) uint
    fn read(fd: int, buf: *byte, len: uint) int
}

fn read_once(fd: int, buffer: *byte, length: uint) int {
    let n = #[ffi(bridge)] read(fd, buffer, length)
    n
}
```

`ffi(leaf)` 与 `ffi(bridge)` 的完整约束见下文。

- ABI 字符串必须是 `"C"`。其它字符串是编译错误。
- 无函数体的 `extern` 是导入：库名与符号必须在编译配置里显式登记。编译器自己把导入写进镜像（Windows 导入地址表 IAT；Linux 动态导入表或内建桩）。禁止靠系统 `ld` 事后扫一堆 `.o` 来解析。
- 有函数体的 `pub extern "C" fn` 是导出。可用 `#[export_name]` 改符号。
- Linux System V AMD64，Windows Microsoft x64。C 字符串用 `*byte` 或 `c"..."`；与 `string` 显式转换。交给外部代码的 GC 对象必须 `std.mem.pin` 或先拷到非移动缓冲；native 可解引用的区域不得含 managed reference slot，pin 不递归固定 referent。完整目标映射见[平台与 ABI 参考](platform-abi.md)。
- `i128` / `u128` 在 `extern "C"` 里：Linux 按 `__int128`；Windows 禁止，见[平台与 ABI 参考](platform-abi.md)与[类型](types.md)。
- `TypeId`、`dyn Trait`、句柄类型不能出现在 `extern "C"` 签名里。
- `!` 可作为 `extern "C"` 的返回类型（C 的 `_Noreturn` / `noreturn`）。
- 导出函数若发生 panic：必须在导出边界用 `std.panic.catch`，否则 runtime **abort 进程**，禁止把 Gugu 展开推进外部帧。

### 外部调用效应与桥接

C ABI 只规定参数、返回值和寄存器/栈布局，不携带是否等待、是否回调 Gugu 或是否执行很久的信息。每个导入项在 compiler 的类型检查结果中还带一个不暴露给用户类型系统的 `ForeignEffect`：

- 未标注的导入是普通 `ForeignBridge`。直接调用和无法静态证明为 `ForeignLeaf`/`DirtyCpu` 的间接调用都走完整桥接；即使实现最终不阻塞，也必须切 system stack并发布精确 roots。runtime可以短暂保留一个可被 GC、回调、退役或 runnable压力打破的 processor lease，并在 native快速返回时直接恢复；这只是内部调度优化，不减弱“可能阻塞/回调”的保守效应。
- `#[ffi(leaf(stack = N))]` 可以附着在无函数体的 `extern "C"` 导入项或 `#[naked] unsafe extern "C" fn` 上。`N` 表示 C 调用及其传递调用链在当前 coroutine stack 上额外使用的字节数；必须是非负整数常量，compiler 按目标 stack alignment 向上取整，省略时为 0。它是声明者承担的 unsafe 调度契约，不是性能提示。外部实现必须在固定可接受上界内返回，不依赖不可界定的 I/O、sleep、mutex/futex/condvar、join 或阻塞式 poll，不回调 Gugu，不调用会分配、触发 GC、park、suspend 或改变调度器状态的 runtime 接口，不跨返回保留 Gugu 地址，且不得超过 stack budget 或让异常/`setjmp`/`longjmp` 越过边界。
- `#[ffi(dirty_cpu)]` 可以附着在无函数体的 `extern "C"` 导入项、带函数体的 `unsafe extern "C" fn`，或一次直接 C 调用表达式。导入项和 native definition 的默认模式是 `ForeignBridge[DirtyCpu]`；调用点属性只覆盖该次调用。它适用于输入规模或参数决定运行时间、可能长时间占用 CPU、或 native 控制流无法提供 stack/safepoint metadata 的函数。带函数体时只能包含本章允许的 opaque native operation，且不能被调用点改成 leaf。dirty 调用不允许回调 Gugu，也不提供强制终止；调用可以无限期占用一个 dirty worker，但不能占住 `LogicalProcessor` 或成为 GC stop 的参与者。
- `#[ffi(bridge)]` 是调用点属性，只能附着在直接导入 C 函数的表达式上；它强制当前调用使用普通 `ForeignBridge`，即使声明带有 `ffi(leaf)` 或 `ffi(dirty_cpu)`。需要保留 dirty CPU 分类时使用 `#[ffi(dirty_cpu)]`，不能把两种调用点属性同时写在同一表达式上。
- `ffi(leaf)` 不表示纯函数，也不禁止 C 侧修改外部内存或设置 `errno`/last-error；它只表示该调用不需要释放当前 `LogicalProcessor`。函数项被单态化且保留 leaf effect 时可以保留直调；转换为普通 `fn` 值、经过无法证明 effect 的间接调用或动态分派后，一律按普通 `ForeignBridge` 处理。语言不提供调用点的“强制 leaf”属性；不确定 stack budget 时使用 `#[ffi(bridge)]`。

compiler 不能检查动态库或 opaque asm 的函数体。错误的 `ffi(leaf)` 声明违反 unsafe 契约：实际等待会占住当前 processor；永久不返回会使该 processor永远不能确认 GC stop，从而永久阻止进程完成 GC；错误的 `stack = N` 还可能破坏 coroutine stack。错误的 `ffi(dirty_cpu)` native contract不会让 GC停摆，但可能永久保留 ABI frame roots/pin、耗尽 dirty CPU额度，并使调用方协程永远无法完成。保守地使用普通 bridge时，短暂 lease始终可由 runtime取回，未知 native work不会永久占住 processor；它是正确性路径。

### 外部线程调入

外部线程只能经 compiler 生成的回调桥进入 Gugu；该桥必须建立配套 runtime 状态、精确根和 panic 边界。线程登记与执行槽取得方式属于[调度器内部规范](../internals/scheduler.md)，不形成外部 ABI。`ForeignLeaf` 与 `ForeignBridge[DirtyCpu]` 都不提供回调能力；从它们回调 Gugu 是违反对应 unsafe 契约，而不是另一种隐式桥接。

## 原始指针、位模式与别名契约

将 `&T` 转成 `*T` 只暴露当前地址，不固定对象；若原对象可以被 GC 移动，则该原始指针只能在下一次 safepoint 前使用，或必须在 `std.mem.pin` 的动态范围内使用。整数转指针不建立来源、对齐或寿命；解引用前调用者必须证明地址属于可访问对象且覆盖完整 `T`。

对 `*T` 的 `ptr_read` / `ptr_write` 和 volatile 操作要求地址按 `align_of[T]()` 对齐、范围有效、位模式对 `T` 有效，并遵守并发同步。`#[repr(packed)]` 未对齐字段必须通过专门的未对齐 intrinsic 或按字节复制；把未对齐地址直接交给上述对齐 API 是未定义行为。

有效位模式至少要求：`bool` 只能为 0/1；`char` 是合法 Unicode 标量；引用非空且有效；`string` 保持 UTF-8 和合法长度；枚举判别值对应有效变体；`TypeId` 在表范围内；句柄与 vtable 必须指向当前镜像的合法 runtime状态。整数、浮点和原始指针接受全部位模式。构造无效位模式后即使尚未读取，只要把它当作已初始化的安全类型传播就是未定义行为；runtime私有对象 metadata的具体表示不属于本章。

unsafe 不豁免数据竞争或受管引用更新契约。通过原始指针写入 GC 引用槽时必须调用对应 intrinsic；当前官方 runtime把它实现为[写屏障](../internals/gc-metadata.md#write-barrier-与-remembered-set)，替代实现可以采用满足相同安全结果的机制。遗漏该操作是未定义行为。别名本身合法，但两个操作系统线程无同步地访问同一位置且至少一方写入仍是数据竞争。

## `asm` 的求值与约束

普通 managed `asm` 不是 safepoint，也不能在模板内部调用会分配、阻塞、展开 panic、触发 GC 或切换协程栈的 Gugu 函数。compiler 将模板解析为有限 CFG：内部不得有回到较早指令的回边、无法解析的间接 branch/call、`ret` 或外部符号跳转；所有路径必须到达模板末尾。`syscall`、`sysenter`、`int`、`hlt`、`mwait`、`umwait`、`tpause`、repeat-prefixed string instruction 和其它目标定义的 system/wait instruction 不属于 managed asm；`pause` 本身可用，但不能位于内部循环。无法证明的控制流、`.byte` 形成的未知 opcode/跳转和上述指令是编译错误，诊断应指向拆分 asm并调用 `std.runtime.safepoint_poll()`，或把 native definition 标为 `#[ffi(dirty_cpu)]`。`memory` clobber阻止 compiler跨越该 asm重排普通内存访问，`cc` clobber声明状态标志被破坏；省略真实 clobber导致的错误结果属于未定义行为。这些有限 CFG/opcode限制不适用于 global asm、naked body或 dirty native definition。

`global_asm` 和 `#[naked]` 函数不拥有 compiler生成的普通根与展开 metadata，不能作为 `Running` 中的 opaque frame 停在 safepoint。`global_asm` 符号的调用模式来自显式 extern 声明；managed context 调用 naked 函数默认使用 `ForeignBridge[DirtyCpu]`。若显式声明 `ffi(leaf)`，则由声明者保证整个 native 调用链有限、无等待、无回调，并提供准确的 `stack = N`。它们若建立可被 GC 或 panic 看到的帧，必须通过目标专用 intrinsic 提供配套 runtime要求的完整 metadata，否则不得进入 managed safepoint或展开路径。

`std.runtime.safepoint_poll()` 不能嵌入 asm 模板；poll 点必须由 compiler 看见并拥有对应 stack map。native 循环不能通过把一个未知的 runtime call 字符串写进 asm 来伪造该 metadata。

## FFI 值与展开边界

`extern "C"` 参数和返回类型只允许 C ABI 可表示的整数、浮点、原始指针、`#[repr(C)]`/`#[repr(transparent)]` 聚合以及 `!` 返回；聚合的每个非 ZST 字段也必须递归满足该条件。引用、`string`、切片、函数环境、闭包、`dyn Trait`、`TypeId`、GC 句柄、`LocalArena`、`SyncArena`、channel 和 Join 不能直接出现在签名中。完整的允许/禁止集合与平台分类见[平台与 ABI 参考](platform-abi.md)。

调用外部函数前，参数按普通左到右规则求值并完成 ABI 转换；返回后再构造 Gugu 值。C 返回无效 `bool`、`char`、枚举或违反 repr 的位模式时，继续把它当安全值使用是未定义行为。外部代码保留的 GC 地址必须在整个保留期间 pin；仅在调用期间临时使用则 pin 覆盖该调用即可。传入区域若含 managed reference slot，native 只能把整块内存当不解引用的 opaque token；读取、写入或复制这些 slot 都违反外部边界契约。

外部异常、SEH 或 C++ 异常不得穿过 Gugu 帧，Gugu panic 也不得穿过外部帧。未在边界内转换的跨边界展开必须立即 abort 进程。

`std.ffi.CString` / `CStr` 只提供 C NUL 字符串的拥有与非拥有视图；`CString.as_ptr` 的使用仍须遵守 pin 与 safepoint 规则，`CStr.from_ptr` 的指针有效性、NUL 终止和外部寿命由 unsafe 调用方证明。
