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

`std.ptr` 提供（lang item，必须存在）：

```
unsafe fn ptr_read[T](p: *T) T
unsafe fn ptr_write[T](p: *T, v: T)
unsafe fn volatile_load[T](p: *T) T
unsafe fn volatile_store[T](p: *T, v: T)
```

`ptr_read` 按位拷出，不把源当成「已移走」。`ptr_write` 按位写入，不运行析构（语言默认没有 `Drop`）。`volatile_*` 禁止编译器删掉或合并该访存。误用（未对齐、悬空、别名破坏、把无效位当成 `T`）是未定义行为。

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

`std.mem.MaybeUninit[T]` 是 lang item，布局与 `T` 相同。编译器不扫描其中的 GC 引用，直到 `assume_init`。

```
fn uninit[T]() MaybeUninit[T]
fn new[T](v: T) MaybeUninit[T]
fn as_ptr(self: &Self) *T
fn write(self: &Self, v: T)
unsafe fn assume_init(self) T
```

`uninit` 不初始化。`write` 按位写入（覆盖前一个位模式，不析构）。`assume_init` 把位当成已初始化的 `T`：若尚未写入有效值，未定义行为。`as_ptr` 本身安全；解引用仍要 `unsafe`。

ZST 的 `MaybeUninit` 与 `T` 一样不占空间；`assume_init` 对纯 ZST 在安全子集里也可视为已初始化。

## `transmute` 与 `unreachable`

`std.mem.transmute[T, U](x: T) U`：按位重解释。必须在 `unsafe` 里。`size_of[T]()` 必须等于 `size_of[U]()`，否则编译错误。结果对 `U` 无效（含破坏对象头、UTF-8、niche）是未定义行为。

`std.hint.unreachable() !`：告诉编译器不可达。若运行到此处，未定义行为。必须在 `unsafe` 里调用。安全的发散用 `panic`。

## intrinsic

绑定到 IR 原语，不是「内联汇编包装函数」所能替代：

| 职责 | 说明 |
|------|------|
| 裸分配 / arena | GC 堆或区域上的未初始化内存；OS `mmap` / `VirtualAlloc` |
| 写屏障 | 手写 GC 字段赋值 |
| 栈切换 | 保存 callee-saved 与栈指针 |
| 栈边界 / SP | GC 与溢出探测 |
| 栈图 / 类型元数据 | 根遍历 |
| 原子 | `xchg`、`cas`、acquire/release/seqcst；channel 与调度握手 |
| 系统调用 | Linux `syscall`；Windows 对导入符号的调用 |
| 无检查索引 / 转换 | 误用即未定义行为 |
| pin | 禁止移动，供 FFI |
| comptime 嵌入文件 | `embed_file`，只在 comptime 合法 |
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

- 第一个实参是 comptime `string`（或 `raw"..."`），AT&T 或 Intel 语法由实现选定一种并在整份编译里固定；tier-1 用 AT&T 表面、与 GNU as 常见记法兼容。
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

- ABI 字符串目前必须是 `"C"`。其它字符串是编译错误。
- 无函数体的 `extern` 是导入：库名与符号必须在编译配置里显式登记。编译器自己把导入写进镜像（Windows IAT；Linux 动态导入表或内建桩）。禁止靠系统 `ld` 事后扫一堆 `.o` 来解析。
- 有函数体的 `pub extern "C" fn` 是导出。可用 `#[export_name]` 改符号。
- Linux System V AMD64，Windows x64。C 字符串用 `*byte` 或 `c"..."`；与 `string` 显式转换。交给外部代码的 GC 对象必须 `std.mem.pin` 或先拷到非移动缓冲。
- `i128` / `u128` 在 `extern "C"` 里：Linux 按 `__int128`；Windows 禁止，见 [类型](types.md)。
- `TypeId`、`dyn Trait`、句柄类型不能出现在 `extern "C"` 签名里。
- `!` 可作为 `extern "C"` 的返回类型（C 的 `_Noreturn` / `noreturn`）。
- 导出函数若发生 panic：必须在导出边界用 `std.panic.catch`，否则 runtime **abort 进程**，禁止把 Gugu 展开推进外部帧。
- **调出：** 导入的外部函数一律视为可能阻塞。调用前让出逻辑处理器，返回后再拿回（与系统调用同一条路）。
- **调入：** 若当前操作系统线程还不是运行时的工作线程，临时把它登记为工作线程并配逻辑处理器，导出函数返回后拆掉。已经在跑 Gugu 的线程直接进，不再套一层。
