# unsafe 与 intrinsic

没有 `unsafe`，GC、调度、channel 就必须用另一种语言写。`unsafe` 是语言的一部分。

## 安全子集

不进入 `unsafe` 的代码必须：

- 不把整数当指针解引用
- 不绕过写屏障
- 不越界（无检查下标只在 `unsafe`）
- 不破坏 `string` 的 UTF-8
- 不把未初始化内存当已初始化值读（ZST 与 `MaybeUninit` 除外）
- 不读 `union` 里当前未写入的字段
- 不对自然对齐不足的 `#[repr(packed)]` 字段取 `&T` 当已对齐引用用
- 不制造数据竞争

## `unsafe fn` 与 `unsafe` 块

- `unsafe fn`：调用方必须在 `unsafe` 里调。
- `unsafe { }`：此处由程序员维持不变量。
- 安全函数可以内含 `unsafe` 块，用来封装原语。
- `#[naked]` 函数必须是 `unsafe fn`。

`unsafe` 不关闭 GC，也不关闭类型检查。

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

`addr_of` 是接收 place 的特殊 intrinsic：只计算槽地址，不读取值，也不构造可能未对齐的 `&T`；非 place 实参是编译错误。`ptr_read` 按位拷出，不把源当成“已移走”；`ptr_write` 按位写入，不运行析构。二者要求自然对齐。`read_unaligned` / `write_unaligned` 只接受位类型，允许未对齐地址并逐字节等价复制。`volatile_*` 仍要求自然对齐，只保证该次访存不被删除、合并或移出其它 volatile 访存的顺序；volatile 不是原子操作，不建立 happens-before。悬空、范围不足、无效位模式、数据竞争或错误写屏障仍是未定义行为。

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

`std.mem.MaybeUninit[T]` 是 lang item（见 [概述 · 术语](overview.md#术语)），布局与 `T` 相同。编译器不扫描其中的 GC 引用，直到 `assume_init`。

```
fn uninit[T]() MaybeUninit[T]
fn new[T](v: T) MaybeUninit[T]
fn as_ptr(self: &Self) *T
fn write(self: &Self, v: T)
unsafe fn assume_init(self) T
```

`uninit` 不初始化。`write` 按位写入（覆盖前一个位模式，不析构）。`assume_init` 把位当成已初始化的 `T`：若尚未写入有效值，未定义行为。`as_ptr` 本身安全；解引用仍要 `unsafe`。ZST 的 `MaybeUninit` 与 `T` 一样不占空间。

`assume_init` 始终是 `unsafe fn`，调用必须在 `unsafe` 块里。对纯 ZST（见 [类型](types.md)），安全前置条件恒成立——没有待初始化的位——因此调用不是未定义行为。这不是安全重载，也不免除 `unsafe` 块。安全子集允许直接读取未初始化的纯 ZST（本章开头那条例外），不必经过 `MaybeUninit`。

## `transmute` 与 `unreachable`

`std.mem.transmute[T, U](x: T) U`：按位重解释。必须在 `unsafe` 里。`size_of[T]()` 必须等于 `size_of[U]()`，否则编译错误。结果对 `U` 无效（含破坏对象头、UTF-8、niche）是未定义行为。

`std.hint.unreachable() !`：告诉编译器不可达。若运行到此处，未定义行为。必须在 `unsafe` 里调用。安全的发散用 `panic`。

## intrinsic

绑定到 IR 原语，不是「内联汇编包装函数」所能替代：

| 职责 | 说明 |
|------|------|
| 裸分配 / 区域 | GC 堆、`LocalArena` / `SyncArena` 上的未初始化内存；OS `mmap` / `VirtualAlloc` |
| 写屏障 | 手写 GC 字段赋值 |
| 栈切换 | 保存 callee-saved 与栈指针 |
| 栈边界 / SP | GC 与溢出探测 |
| 栈图 / 类型元数据 | 根遍历 |
| 原子 | `xchg`、`cas`、acquire/release/seqcst；channel 与调度握手 |
| 系统调用 | Linux `syscall`；Windows 对导入符号的调用 |
| 无检查索引 / 转换 | 误用即未定义行为 |
| pin | 禁止移动，供 FFI |
| comptime 嵌入文件 | `std.mem.embed_file`，只在 comptime 合法；签名见 [编译期执行](comptime.md) |
| `size_of` / `align_of` / `offset_of` / `type_id` / `type_id_count` | 见 [类型](types.md) |
| volatile / 指针读写 | 见上 |
| `transmute` | 见上 |

未定义行为包括：野指针、数据竞争、破坏 UTF-8 或对象头、漏写屏障、在非 safepoint 认为栈图有效、对 `union` / `MaybeUninit` / `transmute` 的无效位模式。调试器可以抓一部分；没炸不是定义。

## `asm` 与 `global_asm`

内联汇编是表达式，必须在 `unsafe` 里：

```
asm(
    "syscall",
    in("rax") n,
    in("rdi") a,
    lateout("rax") ret,
    clobber("rcx", "r11", "memory")
)
```

- 第一个实参是 comptime `string`（或 `raw"..."`）。tier-1 目标统一使用 AT&T 表面语法，与 GNU as 常见记法兼容；同一份源码不能按实现选择切换到 Intel 语法。
- `in("reg") expr`：进入时该寄存器保存 `expr` 的值。
- `out("reg") place` / `lateout("reg") place`：退出时写进可赋值位置。
- `clobber("reg"...)`：这些寄存器与 `memory` / `cc` 被破坏。必须声明，否则栈图与寄存器分配无效。
- 类型是 `()`。禁止在 `#[naked]` 以外靠它「返回」值而不走 `out`。

`global_asm("...")` 是模块顶层声明。字符串必须 comptime。汇编进镜像，不经 Gugu 函数 prologue。用于 rt0 入口。

`#[naked] unsafe fn`：编译器不生成 prologue / epilogue / 栈图里的普通帧。函数体必须是**恰好一次** `asm(...)` 调用（可带 `clobber`）。调用约定由程序员与链接属性保证。

## 链接属性

| 属性 | 用在 | 含义 |
|------|------|------|
| `#[export_name = "sym"]` | 有函数体的 `fn` | 导出符号名。可与 `pub extern "C"` 一起用。 |
| `#[link_name = "sym"]` | 无体 `extern` 项 | 导入符号名，覆盖声明名。 |
| `#[link_section = ".text.foo"]` | `fn`、`static`、`global_asm` | 放入指定节。 |
| `#[used]` | `static`、`fn` | 即使未被引用也不得裁掉。 |
| `#[naked]` | `unsafe fn` | 见上。 |

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

- ABI 字符串必须是 `"C"`。其它字符串是编译错误。
- 无函数体的 `extern` 是导入：库名与符号必须在编译配置里显式登记。编译器自己把导入写进镜像（Windows 导入地址表 IAT；Linux 动态导入表或内建桩）。禁止靠系统 `ld` 事后扫一堆 `.o` 来解析。
- 有函数体的 `pub extern "C" fn` 是导出。可用 `#[export_name]` 改符号。
- Linux System V AMD64，Windows x64。C 字符串用 `*byte` 或 `c"..."`；与 `string` 显式转换。交给外部代码的 GC 对象必须 `std.mem.pin` 或先拷到非移动缓冲。
- `i128` / `u128` 在 `extern "C"` 里：Linux 按 `__int128`；Windows 禁止，见 [类型](types.md)。
- `TypeId`、`dyn Trait`、句柄类型不能出现在 `extern "C"` 签名里。
- `!` 可作为 `extern "C"` 的返回类型（C 的 `_Noreturn` / `noreturn`）。
- 导出函数若发生 panic：必须在导出边界用 `std.panic.catch`，否则 runtime **abort 进程**，禁止把 Gugu 展开推进外部帧。
- **调出：** 导入的外部函数一律视为可能阻塞。调用前让出逻辑处理器，返回后再拿回（与系统调用同一条路）。
- **调入：** 若当前操作系统线程还不是运行时的工作线程，临时把它登记为工作线程并配逻辑处理器，导出函数返回后拆掉。已经在跑 Gugu 的线程直接进，不再套一层。

## 原始指针、位模式与别名契约

将 `&T` 转成 `*T` 只暴露当前地址，不固定对象；若原对象可以被 GC 移动，则该原始指针只能在下一次 safepoint 前使用，或必须在 `std.mem.pin` 的动态范围内使用。整数转指针不建立来源、对齐或寿命；解引用前调用者必须证明地址属于可访问对象且覆盖完整 `T`。

对 `*T` 的 `ptr_read` / `ptr_write` 和 volatile 操作要求地址按 `align_of[T]()` 对齐、范围有效、位模式对 `T` 有效，并遵守并发同步。`#[repr(packed)]` 未对齐字段必须通过专门的未对齐 intrinsic 或按字节复制；把未对齐地址直接交给上述对齐 API 是未定义行为。

有效位模式至少要求：`bool` 只能为 0/1；`char` 是合法 Unicode 标量；引用非空且有效；`string` 保持 UTF-8 和合法长度；枚举判别值对应有效变体；`TypeId` 在表范围内；句柄对象头与 vtable 指向当前镜像的合法对象。整数、浮点和原始指针接受全部位模式。构造无效位模式后即使尚未读取，只要把它当作已初始化的安全类型传播就是未定义行为。

unsafe 不豁免数据竞争或 GC 写屏障。通过原始指针写入 GC 引用槽时必须调用写屏障 intrinsic；漏屏障是未定义行为。别名本身合法，但两个操作系统线程无同步地访问同一位置且至少一方写入仍是数据竞争。

## `asm` 的求值与约束

`asm` 的输入表达式按书写顺序求值一次，随后进入汇编；所有输出 place 在进入前完成定位，汇编返回后按书写顺序写回。输入/输出寄存器约束冲突、同一输出 place 被多个输出覆盖、未知寄存器、目标不支持的寄存器宽度或未声明的固定寄存器破坏都是编译错误。

普通 `asm` 不是 safepoint，不能在模板内部调用会分配、阻塞、展开 panic、触发 GC 或切换协程栈的 Gugu 函数；需要这些行为必须使用编译器认识的 intrinsic/ABI 边界。`memory` clobber 阻止编译器跨越该 asm 重排普通内存访问，`cc` clobber声明状态标志被破坏；省略真实 clobber 导致的错误结果属于未定义行为。

`global_asm` 和 `#[naked]` 函数不拥有普通栈图或展开信息。它们若建立可被 GC 或 panic 看到的帧，必须通过目标专用 intrinsic 提供完整元数据，否则不得进入 safepoint或展开路径。

## FFI 值与展开边界

`extern "C"` 参数和返回类型只允许 C ABI 可表示的整数、浮点、原始指针、`#[repr(C)]`/`#[repr(transparent)]` 聚合以及 `!` 返回；聚合的每个非 ZST 字段也必须递归满足该条件。引用、`string`、切片、函数环境、闭包、`dyn Trait`、`TypeId`、GC 句柄、`LocalArena`、`SyncArena`、channel 和 Join 不能直接出现在签名中。

调用外部函数前，参数按普通左到右规则求值并完成 ABI 转换；返回后再构造 Gugu 值。C 返回无效 `bool`、`char`、枚举或违反 repr 的位模式时，继续把它当安全值使用是未定义行为。外部代码保留的 GC 地址必须在整个保留期间 pin；仅在调用期间临时使用则 pin 覆盖该调用即可。

外部异常、SEH 或 C++ 异常不得穿过 Gugu 帧，Gugu panic 也不得穿过外部帧。未在边界内转换的跨边界展开必须立即 abort 进程。
