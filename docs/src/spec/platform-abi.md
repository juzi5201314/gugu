# 平台与 ABI 参考

本章是 Gugu 平台支持与外部 ABI 的规范性参考。它把目标注册、目标键、机器数据模型、C 类型映射、`extern "C"` 边界、镜像格式和操作系统接口放在同一个查阅入口；已有章节仍负责语言语义，本章不重复定义所有权、GC、并发或包解析规则。

本章采用 Rust Reference 对布局和外部 ABI 的窄承诺方式，并采用 Go 对 `os` / `arch` 分离建模的方式。平台 ABI 的通用算法不在本章复制；本章固定 Gugu 类型如何进入该算法，以及 Gugu 与平台约定之间的差异。

## 规范范围与优先级

本章中的“必须”“禁止”“可以”沿用[概述](overview.md)的规范含义。

对于一次编译，以下规则按顺序生效：

1. Gugu 语言语义和[类型系统](types.md)优先；
2. 本章的目标表、映射表和边界限制优先；
3. 本章引用的目标 ABI 文档只补充本章没有逐项列出的机器级细节。

平台文档的名称或版本变化不能自动改变 Gugu 语义。若平台 ABI 的新版本与本章的规范性表格冲突，目标实现必须继续遵守本章；要采用新约定，必须先修订本规范。

本章不承诺 Gugu 内部 ABI。默认调用约定、闭包环境、runtime私有对象 metadata、vtable、GC 根编码、内部符号名和未标注 `repr` 的字段布局都属于编译器实现，可随编译器构建身份改变。
Windows runtime 的高对齐堆预约保持既有 NT API 兼容范围，不要求新增 `VirtualAlloc2` 等系统入口。高于宿主预约粒度的对齐由 Gugu 映射归属层整块过量预约并选择内部对齐区；原始 reservation、padding 和登记页都必须由同一所有者持有，释放时返还整个原块，不能对内部对齐地址执行 `MEM_RELEASE`。

## 目标模型

### 目标名

目标由一个架构枚举值和一个操作系统枚举值组成，规范形式为：

```text
target_name ::= arch "-" os
```

目标名具有以下限制：

- 只允许小写 ASCII 字母、数字和连字符；
- `arch` 与 `os` 必须来自本章目标注册表；
- 本版本不接受 LLVM 四段 target triple，也不接受 `unknown` 占位段；
- `--target`、`GUGU_BUILD_TARGET`、条件依赖和编译缓存使用同一个规范化目标名；
- 未登记的架构与操作系统组合是编译错误，不能按宿主平台猜测。

“目标三元组”不是 Gugu 的规范术语。其他章节中历史上出现的该词均指本章定义的目标名，工具链实现不得把二段目标名补成另一个字符串后再解析。

### 当前目标注册表

本版本只登记以下两个目标。表中的派生属性是目标 ABI 的组成部分，不是可由 `build.gg` 覆盖的配置项。

| 目标名 | `arch` | `os` | 字节序 | 指针宽度 | C 数据模型 | C 调用约定 | 镜像格式 |
|---|---|---|---|---:|---|---|---|
| `x86_64-linux` | `x86_64` | `linux` | little-endian | 64 | LP64 | System V AMD64 | ELF64 |
| `x86_64-windows` | `x86_64` | `windows` | little-endian | 64 | LLP64 | Microsoft x64 | PE/COFF |

两行均为“已支持目标”。本版本不定义 tier-2、tier-3 或“尽力支持”目标；未登记组合没有部分支持状态。
后端内部的 [A64 编码库与运行时机器片段](../internals/backend.md)不构成目标登记或可用的运行时；`aarch64` 仍不是本版本可选择的架构。
后端共享寄存器分配核心属于内部实现复用，不改变现有目标的 ABI、机器码产物或目标登记条件，也不使未登记架构成为可选目标。

A64 内部整数叶 lowering 使用私有寄存器约定与共享分配核心；它不登记新目标、不提供外部 C ABI，也不代表已接通 runtime 或可生成 AArch64 用户镜像。
内部 lowering 的调用参数只在函数进入时物化；回到 LIR 入口块的控制流边必须使用该边传入的参数值，不能重新读取初始 ABI 参数。
内部 A64 直接调用与尾调只在显式隔离、预留栈容量的边界可用，必须保留 LIR 的 poll-free 证明；需要 checked entry 或 stack map 的生产调用仍被拒绝。这不是目标登记、外部 C ABI 或 runtime 接通的声明。

内部 A64 runtime-site lowering 可为无根整数函数生成 checked entry 与 poll：入口在写栈前按当前水位检查，morestack 返回后恢复 LR 并重试；poll 慢路前保存所有跨点用户值。此分阶段接口尚无序列化栈图/展开及协程集成，不允许带根或普通调用的生产函数，也不声明 AArch64 用户镜像可运行。

内部栈图 codec/reference walker 可显式区分 A64 格式，按物理 xN 编号登记入口寄存器根并按精确返回 PC 扫描；现有 x64 格式与输出保持不变。该格式支持本身不等于 A64 lowering 已接通 GC、展开或协程，也不新增可选目标。

内部 A64 runtime 编译边界可为标量根和带权威 LIR map 的直接 managed call 输出栈图及精确 safepoint 的 caller-frame 恢复记录；它不放宽隔离叶入口，也不支持尚未接通的 aggregate signature/sret、挂起、协程换栈或生产镜像 decoder。该分阶段能力不是 AArch64 用户镜像或平台异常展开可用性的声明。

A64 runtime 中省略 CallReturn map 的 poll-free managed leaf 不得进入 morestack/poll 慢路径。
此类叶的物理帧上限为 4096 字节；直接调用它的 caller 在自己的入口检查中另预留这份容量，
叶自身不发射自动入口检查。超过预算或叶接口与运行时操作矛盾时报告编译错误，不静默插入
可能回收的检查，也不通过给所有调用补图来掩盖契约错误。该预算只适用于 A64 runtime 边界。

内部 A64 runtime 编译也可登记局部槽中的根字；多变体根和未能静态证明初始化的根必须由逐根初始化字节门控，未初始化或生命周期已结束的根不得被扫描。此能力不等于协程换栈或生产镜像 decoder 已接通。

局部根的静态初始化证明只能消费该安全点之前或同位置的块内初始化事件，不能把块的排序编号当作初始化位置；后续写入不得把先前安全点的未初始化字变成固定根。

A64 栈图函数的栈内引用标志必须包含受初始化字节门控的栈内引用，不能因为固定槽位图和寄存器掩码为空而省略。

Linux 目标在没有动态 FFI、系统 Resolver 或可达环境 API 需求时不依赖动态解释器或 libc 初始化。闭世界实际可达的系统 Resolver、环境 API 及通过环境快照依赖它的 Command.spawn 自动采用目标工具链登记的 libc、pthread 与解释器。args/current_dir/set_current_dir、普通 fs、数字网络和空程序仍使用静态 syscall 路径。Windows 目标不链接 CRT，通过薄 IAT 使用运行时所需的系统 DLL；可达的网络功能按需加入 Winsock，进程创建遵守 [CreateProcessW](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw) 的 application、命令行、环境与继承句柄契约。其它动态库必须通过显式 FFI 构建配置加入，不能由宿主链接器隐式补齐。系统需求与显式配置合并后的库、符号、Function/Data 种类和解释器均进入 action key；同名符号来自冲突库或具有冲突种类时必须拒绝，不能替换调用方配置。详见[程序与编译模型](program-model.md)和[包、依赖与构建模型](packages-builds.md)。

A64 内部挂起调用使用权威 SuspendResume 栈图，跨点指针必须落槽。Context 的 SP、PC、x19–x26、x29 和 d8–d15 使用共享布局；x27/x28 由恢复方提供。stackless handoff 的复制输入必须由配对的精确 PC 栈图与 CFA 元数据产生，修复 StackInterior、保存的 FP 链和 Context FP；非根整数、代码地址和浮点位型不得猜测性修复。这仍不是生产协程调度或 A64 用户镜像已接通的声明。

A64 元数据的数据 ABI 可通过现有 Rt0Payload 显式绑定：写入前必须联合校验 v5 栈图、每函数 CFA 和规范化目录，拒绝错架构、越界、重叠及非精确返回 PC。Gugu 扫描入口只消费已验证且不变的镜像，按调用方提供的栈界限与帧数上限枚举根；失败后必须丢弃整个输出，容量不足必须保留完整 needed 计数。帧链只能在零 PC 或载荷中声明的 main-return 哨兵结束，不得跳过未知 helper。A64 Context、scratch、mailbox 与 snapshot 的 Gugu/Rust 布局必须一致；scratch 和保存的 Context 绑定到不移动的 owner 存储。A64 runtime 源声明只通过现有 arch cfg 在 descriptor 架构名为 aarch64 时启用，不进入 x64 编译域。此数据接口不选择 A64 目标，也不替代尚未接通的 A64 启动、调度及平台镜像 writer。

A64 门控根的根字与初始化字节都按完整 frame_size 校验，不能以固定槽位图的 slot_count 作为帧上界；固定槽位图为空也可以存在合法门控根。
A64 显式逐帧与整栈扫描入口必须拒绝 scan_allowed=false 的站点，不得枚举根、增加根数或帧数，即使实际根集为空。

挂起调用中，已被权威逻辑根登记的值若另有传出栈实参副本，这些副本也必须进入同一来源种类的物理栈图；只登记原 spill 不足以保证被调方入口搬栈后的参数有效。

A64 内部标量调用分别使用 x0–x7 与 v0–v7 传参，返回最多两个整数和八个 FP/SIMD 车道；溢出标量使用 8-byte 栈槽，V128 使用 16-byte 对齐栈槽。d8–d15 仅低 64 位 callee-saved，V128 跨调用不得依赖其高半部；morestack 保留完整 q0–q7，poll 可破坏所有 caller-saved FP。FP/SIMD 值不携带指针 provenance，不进入 xN 栈图掩码。该内部约定尚不声明 AAPCS64 外部调用或 Darwin variadic/system-stack 协议已接通。

### 新目标登记

目标描述符的架构、操作系统、镜像格式、rt0 边界、CPU 基线与指针压缩能力是独立维度。实现可以预先表示 `aarch64`、`macos`、`macho64`、libSystem/dyld 启动边界和 `armv8.0-a` 基线；这些维度值不构成已登记目标，也不使其任意组合成为合法目标名。`armv8.0-a` 不接受任何 x86 CPU 特性；AArch64 指针压缩能力声明采用 48 位虚拟地址、最大 4 GiB cage 与 2 MiB 最小对齐，不表示已有对应后端或运行时。目标注册表及宿主目标选择仍只接受上述两行，`aarch64-linux` 与 `aarch64-macos` 均必须拒绝。

增加目标必须同时提供并登记：

- `arch`、`os`、字节序、指针宽度和 C 数据模型；
- 外部 C ABI 的权威文档与 Gugu 类型映射；
- 对应的对象文件格式、入口、重定位、导入/导出和展开信息；
- runtime、标准库底层接口和 `cfg` 组合；
- 目标专属的 FFI、布局、镜像和运行时测试。

在这些内容进入规范和实现之前，目标名不能出现在清单、条件依赖或缓存键中。增加目标是规范变更，不是用户通过自定义 cfg 临时注册目标。

### 宿主与目标

`host` 是执行 `build.gg` 和工具链辅助程序的平台；`target` 是普通程序、测试或 bench 镜像的平台。二者可以不同。目标相关的 `cfg` 和 `[target.'cfg(...)']` 依赖按 target 求值，`build.gg` 的宿主条件按 host 求值。host 不能改变 target 的数据模型、ABI 或镜像格式。

Gugu 源码仓库的 Rust workspace 全目标构建不要求宿主为 `x86_64`。直接在宿主 CPU 上执行 x86_64 机器码的 `coroutine_context`、`morestack_entry`、`x64_encoding` 和 `x64_frame` 基准只在 Rust 的 `target_arch = "x86_64"` 时执行；其它构建架构保留空入口。这不新增 Gugu 编译目标。

## 目标键与条件编译

本版本提供两个内建平台键：

| 键 | 当前值 | 语义 |
|---|---|---|
| `os` | `"linux"`、`"windows"` | 目标操作系统 |
| `arch` | `"x86_64"` | 目标指令集架构 |

因此下列写法是规范的：

```text
#[cfg(os = "linux")]
#[cfg(arch = "x86_64")]
#[cfg(all(os = "windows", arch = "x86_64"))]
```

`pointer_width`、`endian`、C ABI 名称、对象格式、libc 或 CRT 不是本版本的内建 `cfg` 键。它们由目标注册表推导，程序不能用自定义 cfg 伪造另一目标的 ABI。`build.gg` 注册的自定义 cfg 只能增加 package 条件，不能覆盖 `os` 或 `arch`。

目标选择在源码解析和依赖解析前确定。未知键、未知值、非法组合和把 host 属性用于 target 条件都是编译错误。具体谓词组合见[词法结构 · `cfg`](lexical.md)，条件依赖见[包、依赖与构建模型](packages-builds.md)。

## 机器数据模型

### 共通属性

两个已支持目标均使用 64 位 little-endian 数据模型。机器整数与指针的宽度不随 `int` 的运行时值或构建模式改变。

| Gugu 类型 | 大小 | 自然对齐 | 说明 |
|---|---:|---:|---|
| `bool` | 1 | 1 | 有效位模式只有 `0` 和 `1` |
| `byte` / `u8` / `i8` | 1 | 1 | 8 位整数 |
| `i16` / `u16` | 2 | 2 | 16 位整数 |
| `i32` / `u32` | 4 | 4 | 32 位整数 |
| `int` / `i64` / `isize` | 8 | 8 | 有符号 64 位整数 |
| `uint` / `u64` / `usize` | 8 | 8 | 无符号 64 位整数 |
| `i128` / `u128` | 16 | 16 | 128 位整数 |
| `f32` | 4 | 4 | IEEE 754 binary32 |
| `float` / `f64` | 8 | 8 | IEEE 754 binary64 |
| `char` | 4 | 4 | Unicode 标量值，不是 C `char` |
| 原始指针 `*T` | 8 | 8 | 地址位模式；不携带 GC 句柄语义 |

`int`、`uint`、`float`、`isize` 和 `usize` 的别名关系以及数值转换规则见[类型系统](types.md)。`()` 与 `!` 没有值；`()` 只能在代表 `void` 的返回位置使用，`!` 只能作为不返回函数的返回类型。

引用、切片、字符串、句柄和擦除函数句柄的语言表示不是 C ABI 布局。即使某个实现当前把它们编码成一个或两个机器字，也不能据此形成 FFI 契约。

### CPU 基线 {#cpu-baseline}

两个目标都要求 x86-64-v1 指令集与 SSE2 浮点。未新增公开 target feature 前，普通程序和 runtime不能要求 AVX、AVX2、BMI、FMA或其它更高扩展；在只满足该基线的 CPU上运行不得触发非法指令。具体 instruction selection不是外部 ABI，见[后端内部规范](../internals/backend.md)。

### C 数据模型

目标的 C 数据模型只影响 C 别名和外部声明，不改变 Gugu 内建 `int` / `uint` 的定义。Linux 使用 LP64，Windows 使用 LLP64：两者的指针均为 64 位，但 `long` 的宽度不同。

C 的位域、柔性数组成员、编译器私有向量类型和 `long double` 不属于本版本的可移植 Gugu C 边界。需要这些能力时，必须在 C 侧提供固定布局的包装函数或字节缓冲接口。

## `std.ffi` C 类型

`std.ffi` 提供下列透明类型别名。别名不创建新类型，不产生新的 `TypeId`，也不改变底层布局；它们的目标差异只来自本表。

| Gugu 别名 | `x86_64-linux` | `x86_64-windows` | 对应的 C 概念 |
|---|---|---|---|
| `c_char` | `i8` | `i8` | plain `char` 的一个字节表示 |
| `c_schar` | `i8` | `i8` | `signed char` |
| `c_uchar` | `u8` | `u8` | `unsigned char` |
| `c_short` | `i16` | `i16` | `short` |
| `c_ushort` | `u16` | `u16` | `unsigned short` |
| `c_int` | `i32` | `i32` | `int` |
| `c_uint` | `u32` | `u32` | `unsigned int` |
| `c_long` | `i64` | `i32` | `long` |
| `c_ulong` | `u64` | `u32` | `unsigned long` |
| `c_longlong` | `i64` | `i64` | `long long` |
| `c_ulonglong` | `u64` | `u64` | `unsigned long long` |
| `c_size` | `usize` | `usize` | `size_t` |
| `c_ssize` | `isize` | `isize` | signed size type |
| `c_intptr` | `isize` | `isize` | `intptr_t` |
| `c_uintptr` | `usize` | `usize` | `uintptr_t` |
| `c_ptrdiff` | `isize` | `isize` | `ptrdiff_t` |
| `c_wchar` | `i32` | `u16` | target C `wchar_t` |
| `c_bool` | `bool` | `bool` | C `_Bool` / compatible one-byte boolean |
| `c_float` | `f32` | `f32` | `float` |
| `c_double` | `f64` | `f64` | `double` |

`c_char` 只表示 C 的单字节字符存储，不表示 Unicode。文本接口应使用 `CString` / `CStr` 或显式的字节与编码转换。`c_wchar` 也不等于 Gugu `char`：前者在两个目标上的宽度和编码约定不同，后者始终是四字节 Unicode 标量。

本版本不提供 `c_longdouble`。含 C `long double` 的外部声明是编译错误；不能用 `c_double` 伪装成该类型。C `void*` 使用 `*byte` 表示，调用方必须自行维护有效地址、对齐和寿命。

## C 边界可表示性

### 允许的类型

`extern "C"` 参数和返回类型必须递归满足以下条件：

- `bool`、精确宽度整数、`int` / `uint`、`f32` / `f64` 和 `std.ffi` C 别名；
- 原始指针；
- `#[repr(C)]` 结构体或 `union`，其每个非 ZST 字段递归满足本节条件；
- `#[repr(transparent)]` 结构体或 newtype，其唯一非 ZST 字段满足本节条件；
- 使用整数 `repr` 的无载荷枚举；
- `()` 作为 `void` 返回值；
- `!` 作为不返回函数的返回值。

类型别名按展开后的实际类型检查。泛型、未确定的 TAIT、默认布局结构体和带有未定义目标布局的枚举不能直接出现在 C 签名中。

### 禁止的类型

以下类型不能直接出现在 C 签名中：

- 引用、切片、`string`、`Bytes` 和任何 GC 句柄；
- 闭包、函数环境、擦除函数句柄、`dyn Trait` 和 `TypeId`；
- `Option`、`Result`、channel、`Join`、`LocalArena`、`SyncArena` 或含有这些类型的聚合；
- `long double`、未声明布局的 SIMD 类型、位域和柔性数组；
- 纯 ZST 结构体、带载荷且没有明确 C 组成布局的枚举；
- C 可变参数声明。当前版本的 `std.ffi` 不提供 C varargs，`extern "C"` 的省略号签名是编译错误。

C 数组可以作为 `#[repr(C)]` 聚合的字段；数组不能作为独立的 C 参数按值传递。C 侧的数组参数通常已经调整为指针，Gugu 声明必须显式写原始指针和长度。

`fn` 项和闭包转换成的 Gugu 函数句柄不是 C 函数指针。本版本不把该擦除句柄作为 C 回调类型；需要回调时，必须在 C 侧提供固定签名包装并通过本章允许的导入/导出边界调用，不能把现有 `fn` 值强转后传出。

### 生命周期与位模式

`#[repr(C)]` 只固定布局，不固定对象地址。把 GC 对象的地址交给外部代码前，调用方必须在整个外部保留期间使用 `std.mem.pin`，或复制到不会移动的缓冲；C 不能在允许的生命周期之外保存该指针。pin只固定该 allocation，不递归固定其 managed referent。native 可解引用的内存不得含 managed reference slot；需要表达对象关系时，改用 C ABI raw pointer并分别 pin每个 target，或复制为无 managed reference 的 C buffer。外部代码写 managed reference slot无法满足 compiler write barrier，因此是未定义行为。

外部函数返回后，compiler 必须按声明类型构造 Gugu 值。外部代码返回无效 `bool`、`char`、枚举判别值或违反聚合布局的位模式时，继续把该位模式当作安全值使用是未定义行为。整数、浮点和原始指针允许全部位模式，但地址是否可访问仍由 unsafe 契约约束。

## C 布局规则

### `repr(C)` 结构体 {#repr-c-struct}

`#[repr(C)]` 结构体按声明顺序排列字段。每个字段的偏移是前一字段结束后向上取整到该字段对齐的位置；结构体大小向上取整到最大字段对齐。尾部填充属于布局的一部分，`size_of` 必须包含它。

```gugu
#[repr(C)]
struct Header {
    magic: u32
    flags: u16
    version: u16
    payload: *byte
}
```

在两个已支持目标上：`magic` 偏移 0，`flags` 偏移 4，`version` 偏移 6，`payload` 偏移 8，`size_of[Header]()` 为 16（尾部 8 字节对齐填充到指针对齐）。

字段的自然对齐和大小来自本章机器数据模型；嵌套 `repr(C)` 聚合递归使用相同规则。字段重排、niche 压缩和未声明的填充复用均禁止。

### `union`、枚举与透明包装

`union` 的所有字段偏移均为零。其大小是最大字段大小向上取整到最大字段对齐；它只允许位类型字段，读取未写入字段仍受[unsafe 与 intrinsic](unsafe.md)的有效位模式规则约束。

无载荷整数 `repr` 枚举的存储类型就是该整数 repr。判别值必须落在声明的变体集合内。带载荷枚举即使写了 `#[repr(C)]`，也只有在其判别字段和载荷组成能被显式 C 结构表达时才可跨边界；本版本推荐使用显式 `struct` 加 `union` 表达 tagged union，不能依赖 Gugu 枚举的隐式字段命名。

`#[repr(transparent)]` 类型与其唯一非 ZST 字段具有相同的大小、对齐和 C ABI 分类。纯 ZST 字段不改变透明包装的 ABI，但纯 ZST 聚合本身不能作为 C 值传递。

`#[repr(packed)]` 去掉字段间填充，但不会改变 C 边界对未对齐访问的安全要求。含有自然对齐大于一的字段时，调用方不能取得对齐引用；应使用原始地址和未对齐读写。packed 聚合是否能由平台寄存器直接传递，按下节的目标 ABI 分类处理；无法满足平台对齐约束时必须走内存分类。

## `extern "C"` 调用约定

编译器必须保留原始 C 签名的整数符号性、标量/聚合类别、大小与对齐，以及同质浮点聚合的成员类型和位置，不能从目标已展平的寄存器车道反推。所有目标遵守同一描述契约；它不改变现有 x64 的机器传参。为后续 A64 lowering 保留固定参数边界，但本版本前端仍拒绝 C 可变参数声明。A64 的描述与内部参数包布局不构成目标登记、完整外调实现或可运行镜像保证。

### 总则

ABI 字符串目前只有 `"C"`。无函数体的 `extern` 是导入，有函数体且公开的 `pub extern "C" fn` 是导出；库名、符号和链接方式必须在构建配置中显式登记。导出边界不能把 Gugu panic 展开到 C 帧：未被 `std.panic.catch` 收集的 panic 必须按[运行时](runtime.md)规则终止进程。

Linux 使用 System V AMD64 psABI，Windows 使用 Microsoft x64 calling convention。以下表格固定 Gugu 类型进入平台 ABI 的方式；没有列出的寄存器分配、栈参数槽和展开编码遵循相应平台文档。

| Gugu 类型或形状 | `x86_64-linux` / SysV | `x86_64-windows` / MS x64 |
|---|---|---|
| 整数、指针、`bool`、整数 repr 枚举 | `INTEGER` eightbyte | 一个整数参数槽；按位置使用 GPR |
| `f32`、`f64` | `SSE` eightbyte | 标量浮点参数按位置使用 XMM 槽 |
| `i128`、`u128` | 等价于两个连续 `INTEGER` eightbyte；不足寄存器时按平台内存规则 | 编译错误 |
| `repr(transparent)` | 采用唯一非 ZST 字段的分类 | 采用唯一非 ZST 字段的分类 |
| `repr(C)` 聚合 | 先按 C 布局，再按 SysV 的 eightbyte 分类 | 不拆分参数；只有平台允许的 1/2/4/8 字节值按值进入一个槽，其余按引用传递 |
| MEMORY 分类返回值 | 隐藏的返回存储指针占用第一个整数参数位置 | 隐藏的返回存储指针占用第一个参数位置 |

### System V AMD64 桥接

Gugu 的整数参数和原始指针按 `rdi, rsi, rdx, rcx, r8, r9` 的顺序使用通用寄存器；浮点 eightbyte 按 `xmm0` 至 `xmm7` 的顺序使用向量寄存器。返回值遵循 SysV 的 `rax` / `rdx` 与 `xmm0` / `xmm1` 返回位置。

不超过 16 字节的 `repr(C)` 聚合按平台 eightbyte 分类算法递归处理；超过 16 字节、含未对齐字段或被分类为 MEMORY 的聚合参数在调用者栈参数区按值传递，MEMORY 返回值使用隐藏返回存储指针。同一参数需要多个 INTEGER/SSE 位置时，任何一组寄存器不足都必须让整个参数回退到栈，并保留未使用的寄存器给后续参数；栈副本保留对象大小、填充与所需对齐。`i128` / `u128` 遵循现有 `__int128` 约定；整数返回使用 `rdx:rax`，参数必须占用连续的 INTEGER 位置。

调用点的栈对齐、栈参数顺序和寄存器耗尽后的回退规则遵循 System V AMD64 psABI。Gugu 生成的调用点在执行 `call` 前保持 16 字节栈对齐；SysV 的 128 字节 red zone 不能被用来保存需要跨 safepoint、阻塞或外部保留的 Gugu 根。

### Microsoft x64 桥接

前四个参数槽按位置使用 `rcx`、`rdx`、`r8`、`r9` 或对应的 `xmm0`–`xmm3`；参数不会像 SysV eightbyte 一样拆到多个寄存器。调用方必须为被调用方保留 32 字节 shadow space，并遵循 Microsoft x64 的栈对齐和 prologue/epilogue 约束。

大小为 1、2、4 或 8 字节且符合平台规则的 `repr(C)` 值可按值进入一个参数槽；其它聚合按指针传递。返回值不符合平台按值返回条件时使用隐藏返回指针，隐藏参数占用第一个参数位置，显式参数整体向后移动。

Windows 没有 SysV red zone。被调用方必须保持平台规定的 `rbx`、`rbp`、`rdi`、`rsi`、`r12`–`r15` 和 `xmm6`–`xmm15` 状态；导入桩和导出函数还必须生成可被 PE unwind 表描述的栈帧。

Windows C ABI 不提供与本章兼容的 `__int128` 参数或返回规则，因此 `i128` / `u128` 在该目标的 `extern "C"` 签名中始终是编译错误。不能依赖把它拆成两个寄存器后“碰巧工作”。

### 调用边界上的运行时责任

C ABI 的寄存器、栈槽、布局和符号规则与 runtime 的调度效应正交。compiler 在 lowering metadata 中保留普通 `ForeignBridge`、`ForeignBridge[DirtyCpu]` 或 `ForeignLeaf`，但不能把该模式编码成 C ABI 可观察的额外参数或返回值。

未标注导入和 effect未知的间接调用使用普通 `ForeignBridge`：跨到 C代码前，runtime必须使用该目标可识别的连续 system stack和合法栈边界，保存 Gugu context并登记精确根；调用先经过有界 `BlockingBridge` admission并取得 `BridgeCredit`，runtime可以在 native快速返回期间保留可被 retake的 processor lease，GC、回调、退役或持续 runnable压力可以打破 lease并把 processor交给其它 managed work。没有 credit时调用方不能执行 native code，只能在不持有 processor的 admission waiter中挂起。是否直接恢复或重新入队完全属于 runtime内部，C代码不能观察 processor身份或依赖其不变。

普通 bridge 的按值聚合、栈参数和隐藏返回存储由 runtime 建立非移动副本；建立副本之前以及 C 执行期间，调用者参数区中保留的所有可移动地址副本都必须登记精确根，包括原 C ABI 寄存器参数的存储副本。相对栈顶的深度和字节数属于标量，不能作为地址根扫描。确认原生线程退出后才回写结果并归还 credit。原生函数返回并不等于线程退出：线程本地析构仍可能执行，不能提前复用请求或系统栈。C 返回时捕获的 `errno`/`GetLastError` 随调用结果保存在原协程中，不能被线程创建、等待或清理的状态覆盖；外调缓冲释放后仍保留该错误状态。raw pointer 参数仍由调用方遵守已有 pin 与借用契约，按值复制不改变其指向对象的生命周期。Linux 的 glibc 线程实现要求链接输入显式提供目标解释器以及 `pthread_create`、`pthread_tryjoin_np`、`pthread_attr_init`、`pthread_attr_setstack`、`pthread_attr_destroy`、`__errno_location`；缺失时编译失败，禁止探测宿主或用退出桩替代普通外调。普通及 DirtyCpu 调用的外部目标也必须在 native 链接输入中登记；缺失时诊断点名该符号，不生成可执行镜像。外部整数返回只消费其类型规定的有效低位；`u8/u16/u32` 在进入 Gugu 表示时零扩展，不能依赖 native 返回寄存器未定义的高位。

`#[ffi(dirty_cpu)]` 导入或 native definition、相应调用点，以及 managed `#[naked]` 使用 `ForeignBridge[DirtyCpu]`：持久 bridge state定位 coroutine stack上的 ABI frame和精确 roots，调用释放 `LogicalProcessor`，native stack不进入 Gugu stack map，且 native body不能回调 Gugu。`global_asm` 符号的模式来自显式 extern 声明；dirty CPU额度和等待状态属于 runtime 内部，不进入 C ABI。

满足 `#[ffi(leaf)]` 契约的直接调用使用 `ForeignLeaf`：调用留在当前 worker、processor和 coroutine stack上，不建立 bridge state、不释放 processor，也不允许 C回调 Gugu。属性的 `stack = N`（省略时为0）计入 caller函数的 `max_leaf_reserve`；caller prologue以所有 direct leaf调用预算的最大值只执行一次 `StackCheck`，不在每个调用点重复检查。leaf调用仍须遵守本章的 C类型、pointer pin、内存别名和展开边界规则。

`extern "C"` 导出函数被非 Gugu 线程调用时，必须按运行时的线程接入规则登记该线程；导出函数返回后才能撤销临时登记。外部线程等待期间，参数与返回存储必须保持有效；managed 函数在受调度器管理的协程上执行，因此可以分配、挂起和参与 GC。完成发布必须晚于协程根与栈登记的结清，等待线程观察完成之后才可读取返回值并复用入口参数区。运行时关闭入口后不再接受新的线程登记。回调登记有固定 profile 上界，耗尽时在发布节点之前终止入口，不扩张无界链表或创建额外线程。回调提交必须通知 managed 安全点；长时间运行的协程在安全点让出后仍保留精确根。持有外层普通 worker credit 的回调等待线程可以借用执行同一普通 admission FIFO 中的请求；同线程嵌套回调继承该资格，独立外部线程不自带 credit，必须继续通过普通 admission。外层 credit 保留到外层线程退出，借用不建立额外 worker；回调协程的参数根、登记和结果交付全部结清后，其控制存储与栈才能复用；压力期间将空闲存储实际归还 provider，并扣除 owner cache 统计；DirtyCpu 不参与借用。回调协程与借用请求都结清后才能发布完成。线程数按 OS 线程身份计数：已有 worker 不重复计入，同一外部线程的嵌套回调只计一次，最后一次返回确认后撤销登记。C 线程不能直接操作 Gugu 协程句柄或 GC 元数据。DirtyCpu 与 ForeignLeaf 均不提供从 C 调入 Gugu 的回调桥。

## 符号、镜像与节

### 符号命名

C 导入符号默认使用 `extern` 声明名；`#[link_name = "..."]` 可指定精确外部名。C 导出符号默认使用声明名；`#[export_name = "..."]` 可指定精确外部名。C 边界不使用 Gugu 内部 mangling，也不额外添加平台无关的前导下划线。

符号名不能含 NUL；目标对象格式不接受的名称是编译错误。重复导出、同一导出名指向多个定义、导入名与登记库不匹配都是编译错误。内部 Gugu mangling只要求单次编译确定且无冲突，编码由[后端](../internals/backend.md)版本化，用户不得引用或拼接。

### 逻辑节

编译器先生成逻辑节，再根据目标对象格式写出镜像：

| 逻辑内容 | ELF64 Linux | PE/COFF Windows |
|---|---|---|
| 可执行代码 | `.text` | `.text` |
| 只读数据 | `.rodata` | `.rdata` |
| 已初始化可写数据 | `.data` | `.data` |
| 零初始化数据 | `.bss` | `.bss` |
| 展开描述 | `.eh_frame` | `.pdata` / `.xdata` |
| Gugu 栈图 | `.gugu.stackmap` | `.gugustk` |
| 源码位置 | `.gugu.src` | `.gugusrc` |
| Gugu 类型与镜像元数据 | `.gugu.types` / `.gugu.meta` | `.gugutyp` / `.ggmeta` |
| 外部导入 | 动态导入表（仅显式 FFI） | `.idata` |

PE 节名长度和节属性必须符合 PE/COFF 目标限制。`#[link_section]` 指定的节必须在目标格式上可表示，且不能覆盖 runtime、栈图、类型表、导入表或展开表的保留节；非法节名、权限组合和对齐要求都是编译错误。`--strip` 不能删除运行时必需的栈图、展开信息、源码位置或 GC 元数据，详见[工具链与命令行](toolchain-cli.md)。

### 可执行镜像形式 {#executable-image-forms}

没有动态 FFI、系统 Resolver 或可达环境 API（含 Command.spawn 的环境快照）需求的 Linux executable 必须是无 `PT_INTERP` 的 static PIE `ET_DYN`，由 rt0 完成镜像自身允许的 relative relocation 并支持加载基址随机化。运行时只应用 relative relocation，并把加载偏移加到 addend 上；PC 相对位移与 RVA 在镜像写出前修完。显式登记动态 `.so` 或可达内建功能引入目标系统库后，才加入 `PT_INTERP`、`DT_NEEDED`、GOT/PLT 和对应 relocation。Function 导入使用 PLT/GOT；Data 导入使用 STT_OBJECT 与 GLOB_DAT，不生成 PLT，也不能作为普通外调目标。解释器与 sysroot 必须来自选中的 target/toolchain 描述而不是宿主 PATH 探测。

启动载荷（`Rt0Payload`，`REVISION = 5`，144 字节）里的镜像内地址字段全部参与这批 relative relocation：`main_entry`、`initial_slot`、`runtime_state`、`main_return`、`stackmap`、`fragment_base`、`unwind`，以及 `gc_types`（类型段）与 `gc_meta`（GC metadata blob）。两段不存在时对应字段保持 0，且**不得**登记重定位——否则运行期会把加载基址当成有效地址。偏移 120 的 `boot_environment` 是 rt0 捕获的 native 环境块地址：Linux 保存初始栈 envp，Windows 保存 GetEnvironmentStringsW 返回的 UTF-16 块；runtime 完成启动配置读取后释放 Windows 副本。偏移 128 的 `boot_argc` 与偏移 136 的 `boot_argv` 在 Linux 保存初始 argc/argv，Windows 分别为 0 和 GetCommandLineW 原始 UTF-16 命令行地址；按目标 cfg 解释，不按 argc 是否为零猜测操作系统。三个 native 字段在镜像内初值为 0，不参加 relative/base relocation。runtime 在 main 前复制 argv 和宿主 cwd 到独立计费 raw 存储。ELF 若导入 environ 数据符号，rt0 在相对重定位后仅当其值为 NULL 时以启动 envp 初始化，保留 loader/构造器已经设置的非空环境；没有此导入时不发射初始化路径。`gc_types_len`、`gc_meta_len`、`stackmap_len`、`unwind_len` 是镜像字节数，不参与重定位。

Windows executable和 `cdylib` 使用 PE32+，包含合法 base-relocation table并设置 ASLR、high-entropy ASLR和 NX兼容标志；默认不导入 CRT。preferred image base、file/section排列和 padding是当前 writer实现细节，外部代码只能依赖本章登记的导入导出、逻辑节、入口、TLS和展开面。
PE 的 C 导出函数必须登记可解析的异常展开地址；入口载荷里的镜像内函数地址使用 RVA，而不是 `.text` 内偏移。

macOS 镜像格式为 Mach-O 64（`arm64` 与 `x86_64`）的 PIE `MH_EXECUTE`：由 `/usr/lib/dyld` 加载并经 `LC_MAIN` 进入，导入只经 dylib 两级命名空间绑定，`libSystem.B.dylib` 总是第一个 dylib；镜像必须带覆盖全部签名前字节的 ad-hoc `LC_CODE_SIGNATURE`，因为 arm64 内核拒绝执行未签名代码。macOS 目标尚未登记，这一镜像形式在目标登记前不对外承诺。

macOS 的 C 导入声明使用 C 名字，由写出器加一个前导下划线形成 dyld 名字。每个 Function 或 Data 导入有一个 dyld bind 槽；只有 Function 可以生成可调用入口，且入口在镜像布局确定后以指向该槽的地址写入。仅声明 dylib 依赖不创建 bind 槽。

内部验证目标 `x86_64-macos` 使用 Darwin x86-64 C 调用约定；启动入口接收 dyld 提供的 `argc`、`argv` 与环境指针。运行时经 libSystem 访问文件、套接字和子进程，并按 Darwin errno 解释原生错误。内部验证入口不改变目标名解析、公开目标列表或宿主默认选择。

内部 aarch64 Linux 镜像使用 ELF64 static PIE：`ET_DYN`、`EM_AARCH64 = 183`，无 `PT_INTERP`，具有 `PT_PHDR` 和分别为 R-X、R、RW 的三个 `PT_LOAD`，段按 64 KiB 对齐以兼容 4 KiB、16 KiB 和 64 KiB 内核页；不存在 W+X 映射。绝对指针槽只允许位于可写数据，登记为 `R_AARCH64_RELATIVE = 1027`，由镜像入口自行加上加载偏移，不依赖内核执行动态重定位。此形式不登记新目标，也不扩大 CLI 或闭世界编译的支持范围。

内部 aarch64 Linux `_start` 从内核初始栈取得 `argc`、`argv`、`envp`，由参数块的运行时地址减去其链接期地址得到加载偏移，完成 RELATIVE 重定位后，以 x0–x4 = 参数块、加载偏移、`envp`、`argc`、`argv` 调用共享 rt0。rt0 初始化 x27/x28 并将原生启动参数写入 `Rt0Payload`，调用 `rt0_start`；正常返回后以 x0 为状态、x8 = 94 执行 `svc #0`（`exit_group`）。调用方提供载荷、初始槽、processor 和 `rt0_start`；入口只补写载荷版本与初始槽指针，其余镜像内载荷指针仍须由调用方登记 RELATIVE 重定位。此路径不声明 Linux 调度器、网络、信号、进程或 VM 适配器已接通。

内部 aarch64 Linux 文件适配器接收 x0 = `NativeRequest*`，在原生栈上使用现有 Linux 请求协议（x86_64 syscall 编号与参数顺序），不使用 Darwin 的连续操作号。文件请求 0/1/2/3/5/8/72/74/75/77/82/83/84/86/87/88/89/217/262 分别为 read/write/open/close/fstat/lseek/fcntl/fsync/fdatasync/ftruncate/rename/mkdir/rmdir/link/unlink/symlink/readlink/getdents64/newfstatat；另支持 4 = stat、6 = lstat。路径请求按需补入 `AT_FDCWD = -100`，转换为 arm64 的 *at 系统调用；x8 为 asm-generic syscall 编号，以 `svc #0` 调用。open 与 fcntl 的 `F_SETFL`/`F_GETFL` 转换两种架构的 `O_DIRECT`、`O_LARGEFILE`、`O_DIRECTORY`、`O_NOFOLLOW` 位。成功清零 ERROR；失败保留负返回值于 VALUE，并写入正 errno；未支持请求写 VALUE = 0、ERROR = `EINVAL`（22）。此文件适配器不负责线程、调度或发布请求完成，信号请求 14/289 留给信号适配器。

Linux 文件请求中的 stat 输出缓冲为调用方提供的至少 144 字节有效可写原生内存，使用 x86_64 kernel stat 布局；arm64 适配器在系统调用成功后规范化原生 stat，失败不改写调用方缓冲。mode/size 与修改时间秒/纳秒分别位于 +24/+48/+88/+96，运行时由这些偏移构造文件元数据；内核目录项沿用 Linux dirent64 布局，无需转换。内部片段不登记公开目标或接通完整编译路径。

内部 aarch64 Linux 网络适配器沿用 `net_call` 协议：x0 = 操作号，x1 = 128 字节网络请求地址，九个参数槽后接 +72 的 value 和 +80 的 error；这不是 `NativeRequest` 布局。操作 0–10、21–25 分别为 socket、setsockopt、bind、listen、accept4、getsockname、getpeername、close、epoll_create1、epoll_ctl、epoll_wait、connect、shutdown、getsockopt、recvfrom、sendto，参数顺序保持 Linux runtime 原样。操作 19 返回同一片段内的 C ABI 关闭回调，接收 x0 = fd、返回正 errno 或 0。网络调用成功返回并存储 value，失败 value = 0、error = 正 errno，未支持操作返回 EINVAL。所有系统调用经 x8 和 `svc #0`，不引入 C 导入。

网络请求维持 x86_64 打包 epoll_event 协议（12 字节，events +0、data +4）；适配器在原生栈上转换为 arm64 的 16 字节布局（data +8），epoll_wait 经 epoll_pwait 且不临时改动信号掩码。每次至多返回 128 个事件（不超过请求 maxevents）；非正 maxevents 返回 EINVAL。超时、EINTR 和其他错误保持现有 Linux 语义。sockaddr、socket flags、选项和 Linux errno 沿用现有协议；所有被读取或写入的非空原生缓冲由调用方保证有效。

内部 aarch64 Linux 信号适配器接收 `NativeRequest*`，支持 x86_64 请求号 13/14/289 的 rt_sigaction/rt_sigprocmask/signalfd4，以及 39/62/186/200/234 的 getpid/kill/gettid/tkill/tgkill。文件 worker 同样接受并分派这组请求，使现有 signal runtime 的 mask/signalfd 请求无需改变路由。signalfd4 参数为 fd、mask、sigsetsize（8）、flags；rt_sigaction 的 32 字节 kernel action 为 handler、flags、restorer、mask，arm64 不需要 x86_64 的 SA_RESTORER，适配器清除此输入位并使用 arm64 内核返回路径。新 action 和旧 action 缓冲由调用方保证有效；失败不复制旧 action。信号编号和 signalfd_siginfo 与 Linux runtime 一致；订阅、投递队列和 worker 线程管理仍由 runtime 负责。

Linux resolver 当前不是运行时 UDP DNS：它使用 resolver_symbol(0/1) 的 libc getaddrinfo/freeaddrinfo 地址，通过 operation = ~uint(0) 的 foreign worker 调用，保留系统地址顺序和取消后释放迟到结果的契约。内部 aarch64 raw-syscall 镜像尚无 libc 导入和 foreign-call worker，因此此阶段不接通 resolver，也不以自建 DNS 或静默返回空结果替代；不新增 resolver 占位入口或公开目标。

static PIE自重定位、PE header字段、section排序和 archive编码见[后端内部规范](../internals/backend.md)，不得在该文档扩展本节公开镜像面。

### 入口、重定位与 TLS {#entry-relocation-tls}

Linux 镜像的入口由 ELF `e_entry` 指向 rt0；`_start` 是默认启动约定。Windows 镜像的 PE 入口同样直接指向 rt0 初始化路径。入口函数不经过 Gugu 普通函数 ABI，也不能触发 GC 或依赖已初始化的 runtime。

编译器直接写出 ELF、PE 或内部验证用的 Mach-O，不把未解析的内部符号交给系统 `ld` 事后处理。所有重定位、节偏移、导入、入口和展开信息必须在镜像写出前验证；目标不支持的重定位是编译错误。

`#[coroutine_local]` 与 `#[os_thread_local]` 的语义分别见[声明与模块](declarations.md)和[内存与对象模型](memory.md)。它们的实现可以使用目标 TLS、runtime 控制块或等价机制，但实现细节不是 C ABI；协程本地槽不能伪装成操作系统线程 TLS，操作系统线程槽也不能在协程迁移后继续当作同一槽使用。

## 操作系统接口

### Linux

rt0 和 runtime 直接使用 Linux syscall 约定完成启动、内存建立、输出和退出；默认不经过 libc。syscall 号、内核内部结构和 vDSO 是否存在不是语言 ABI，不能进入可复现的语言语义或缓存 key。

Linux syscall 桩必须遵循内核规定的寄存器和错误返回约定，并在返回后立即把错误状态转换成 Gugu 的 `std.io` / `std.process` 等错误类型。标准库可以提供显式 libc FFI，但这不会改变默认 runtime 的无 libc 模型。

### Windows

rt0 和 runtime 通过 PE 导入表调用目标注册表允许的系统 DLL；默认启动依赖限于薄的 `ntdll` / `kernel32` 接口，不链接 CRT。禁止硬编码 syscall 号、扫描进程导出表或在运行时隐式加载未登记 DLL。镜像写出前修完 PC 相对重定位；绝对地址只进入 `IMAGE_REL_BASED_DIR64`，不在 rt0 里扫描 syscall 号。调用前必须留出 32 字节 shadow space。

用户额外导入的 DLL、库名和符号必须在构建配置中登记。导入表由 Gugu 编译器直接写出；缺失导出、名称冲突、导入库未登记和不匹配的调用约定都是编译错误。

普通终止信号的 Gugu 映射、订阅队列和默认动作见[运行时与运维语义](runtime.md)。Linux 的 signal handler、备用栈和 fatal fault 路径必须保持 signal-safe，Windows 的 console/SEH 回调不得直接运行 Gugu 用户代码；具体 syscall、导入符号和 handler 数据结构不是稳定 ABI。用户通过 FFI 修改 signal disposition、mask 或 console handler 后，运行时信号契约不再适用。

### macOS（内部验证）

内部 arm64 Mach-O 的 Function 导入通过调用桩跳转到 dyld 绑定的地址；Data 导入没有调用桩。调用桩不负责建立 AAPCS64 调用现场或切换栈。

内部 arm64 Mach-O 的 `LC_MAIN` 将 Darwin 的 `argc`、`argv` 和 `envp` 传给 rt0；`rt0_start` 正常返回时，进程以其返回值调用 libSystem 的 `exit`。如果镜像已导入 libSystem 的 `_exit`，入口复用该导入。添加入口不改变已有调用桩的绑定目标。这一入口不构成目标登记。

rt0 和运行时通过 libSystem 调用 Darwin 接口；文件系统、套接字、进程和信号订阅使用同一标准库语义，但原生结构、信号编号和错误码不与 Linux 互换。普通信号由 kqueue 收集；订阅期间的原生处理器不执行用户代码，也不把忽略信号的 disposition 传给通过 exec 启动的子进程。

内部 arm64 文件适配器在原生栈上处理描述符、目录和路径请求。传给 `open` 和文件描述符 `fcntl` 的可变整数实参位于栈上，而非固定参数寄存器；`stat`、`lstat`、`fstat`、`opendir` 和 `readdir` 绑定到 arm64 的无后缀 libSystem 导出。失败把 Darwin errno 写回请求；适配器不负责调度或发布请求完成。

每次调用 `readdir` 前清零线程的 errno；返回空指针且 errno 为零表示读完，空指针且 errno 非零表示失败。非空目录项指针借自对应目录流，在下一次读取或关闭该流前有效。

Darwin 的对齐地址预留先将请求长度向上取整为整页，再裁剪对齐所需的额外地址空间；裁剪失败时不得报告预留成功。

内部 arm64 进程适配器在原生栈上通过 libSystem 创建进程及管道，并支持非阻塞观察、终止和管道 I/O。`posix_spawn` 及其文件动作直接返回错误码；其他入口在失败时读取 Darwin errno。管道两端设置 close-on-exec，父进程端设置非阻塞并抑制 SIGPIPE。重定向用的描述符副本也设置 close-on-exec；传给 `fcntl` 的可变整数实参位于栈上。

Darwin datagram 接收使用 `recvmsg` 的返回标志识别 `MSG_TRUNC`，不能将写满缓冲的报文误判为完整报文。

内部 arm64 套接字和 kqueue 适配器在原生栈上通过 libSystem 执行网络请求，使用与 x86_64 Darwin 相同的请求结果和 Darwin errno。网络 `fcntl` 的可变整数实参在栈上传递；`sendto`、`recvfrom` 和 `recvmsg` 的字节计数保留完整的 `ssize_t` 宽度。原生 socket 关闭回调正常返回零，失败返回 Darwin errno。

内部 arm64 信号订阅与 x86_64 Darwin 共用同一运行时协议：用 kqueue 的 `EVFILT_SIGNAL` 收集信号计数，通过 libSystem 的 `sigaction` 安装只返回的原生处理器并保存旧 disposition。最后一个订阅关闭时，运行时同步恢复旧 disposition，不等待调度器下一次轮询。

内部 arm64 Darwin 的 DNS 符号查询沿用 x86_64 的操作号：0 返回 `getaddrinfo` 的可调用地址，1 返回 `freeaddrinfo` 的可调用地址。查询本身不阻塞；解析由调用方在原生栈上执行，保留 libSystem 的返回状态，结果链必须用配对的 `freeaddrinfo` 释放。

Darwin 用 `disconnectx` 解除通配地址 UDP socket 的 peer，保留本地端口。Darwin 会清除已连接 UDP socket 的本地 IP，且同一 socket 无法重新绑定；原先绑定具体地址的 UDP socket 在调用原生断开前返回 `Unsupported`，保留绑定和 peer。Unix datagram 的 `AF_UNSPEC` 被拒绝时，只有确认 native socket 已不再关联 peer 才提交断开状态。

Darwin 在 UDP bind 后用 `getsockname` 记录是否绑定具体 IP，以内核返回的地址为准，而非 bind 的请求参数；例如，内核可将带 scope 的 `::` 和双栈的 `::ffff:0.0.0.0` 规范化为通配地址。

### 外部错误状态

`errno`、Windows last-error 或其它线程局部错误状态属于外部调用的即时结果。`ForeignLeaf` 返回后可在同一 OS thread 上紧接读取；普通 `ForeignBridge` 与 `ForeignBridge[DirtyCpu]` 必须在 native 调用返回、协程重新入队之前由执行 worker 捕获该值，并由标准库通过本次调用结果读取。调用方在 `yield`、再次调用外部函数或可能迁移协程之后，不能要求仍是未捕获的原线程状态。

## 原子与机器状态

语言内存序由[并发](concurrency.md)定义，不能因为 x86-64 的强内存序而把数据竞争变成合法程序。两个已支持目标至少保证自然对齐的 1、2、4、8 字节整数和指针原子操作满足该章节的原子语义；16 字节原子操作不属于本版本的无锁 ABI 保证。

编译器可以使用目标指令实现 `Relaxed`、`Acquire`、`Release`、`AcqRel` 和 `SeqCst`，但具体指令选择不是源程序可观察契约。原子对象必须满足[并发](concurrency.md)的无 GC 引用、自然对齐和有效位模式约束；未对齐或含引用的原子类型是编译错误。

外部调用可能修改 caller-saved 寄存器和平台允许修改的机器状态。Gugu 代码不能依赖未由相应 C ABI 明确保留的寄存器、向量寄存器、标志位或线程局部状态。内联汇编的寄存器约束和 runtime 保留寄存器见[unsafe 与 intrinsic](unsafe.md)，不因本章的 C ABI 映射而放宽。

尚未登记的 A64 内部 lowering 支持同样自然对齐的 1、2、4、8 字节原子值，不要求 LSE；读改写必须保持不可分割性和并发规范的成功/失败内存序。强 CAS 只在排他存储失败时内部重试，比较不等返回实际值与失败标志，不执行写入；fetch 操作返回线性化时的旧值。后端不得在排他读写区间插入 spill、调用或 safepoint。该能力不构成 A64 目标登记或可运行镜像保证。

## ABI 稳定性

下列项目构成本版本承诺的外部稳定面：

- 已登记目标的 `extern "C"` 参数、返回和 `repr(C)` / `repr(transparent)` 布局；
- `std.ffi` 别名在目标表中的宽度、对齐和符号性；
- `#[export_name]`、`#[link_name]` 指定的外部符号；
- 默认入口、逻辑节的目标格式映射和显式导入/导出规则。

下列项目不稳定，不能用于跨编译器或跨版本互链：

- Gugu 默认 ABI、内部寄存器分配和内部函数符号；
- 无 `repr` 类型的字段顺序、padding、niche 和 enum 编码；
- 闭包环境、函数句柄、runtime私有对象 metadata、vtable、`TypeId` 编号和 GC metadata；
- 协程栈、根编码、runtime 私有 TLS 和 panic 展开表内部编码；
- 未由目标注册表明确列出的系统调用号、DLL 导出或编译器优化选择。

Gugu 对象文件、静态库和动态库之间不提供独立的 Gugu-to-Gugu ABI。跨编译器版本发布时，C ABI 外的 Gugu 依赖必须重新编译；仅把旧对象文件放入新镜像不是受支持的兼容方式。

## 目标 ABI 一致性检查

目标实现和编译器发布前必须覆盖以下边界：

1. 所有 C 标量别名的大小、对齐、符号扩展和返回值；
2. `repr(C)` 结构体的字段偏移、尾部填充、嵌套聚合、`union` 和透明包装；
3. SysV 的 0、1、2、16、17 字节聚合边界，以及整数与浮点混合字段；
4. Microsoft x64 的 1、2、4、8 字节按值边界、shadow space 和隐藏返回指针；
5. Linux `i128` 的参数/返回与 Windows 对 `i128` 的编译错误；
6. 栈对齐、寄存器保存、导入桩、导出 panic 和非 Gugu 线程接入；
7. ELF/PE 的入口、重定位、保留节、导入表、展开信息和 strip 后必需元数据；
8. GC 对象 pin、外部保存指针、普通 `ForeignBridge`/`ForeignBridge[DirtyCpu]`/`ForeignLeaf` 调用模式、线程局部错误状态和 safepoint 前后的生命周期。

其中 `i128` 与更大的按值聚合按 8 字节 lane 拆分，归入第 3、4 项的聚合边界矩阵：SysV 记两个 `INTEGER` lane、占用相邻整数寄存器，超过单 lane 可表示宽度的值不产生新的槽位类别；Microsoft x64 超出 8 字节的按值参数按位置规则落到共享槽或栈。这些检查必须使用确定性的 C 对照程序或固定镜像 fixture；不能用削弱声明、改变 fixture 或忽略失败来获得通过。规范测试的运行方式见[测试](testing.md)。

## 外部基准文档

本章使用下列资料补充未逐项复制的机器级算法；它们不是 Gugu 规范的替代品：

- [Rust 平台支持与 tier](https://doc.rust-lang.org/rustc/platform-support.html)
- [Rust 类型布局与 `repr`](https://doc.rust-lang.org/reference/type-layout.html)
- [Rust `extern` 函数限定符](https://doc.rust-lang.org/reference/items/functions.html#extern-function-qualifier)
- [Go 构建约束](https://pkg.go.dev/cmd/go#hdr-Build_constraints)
- [Go 内部 ABI 说明](https://go.dev/src/internal/abi/abi-internal.md)
- [System V AMD64 ABI](https://gitlab.com/x86-psABIs/x86-64-ABI)
- [Microsoft x64 调用约定](https://learn.microsoft.com/en-us/cpp/build/x64-calling-convention)
- [Rust Tokio `spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)
- [Go cgo 调用约束](https://go.dev/cmd/cgo/)
- [Go runtime `cgocall`](https://go.dev/src/runtime/cgocall.go)
- [Zig 语言参考中的外部函数与调用约定](https://ziglang.org/documentation/0.15.2/)
- [GHC FFI safety](https://ghc.gitlab.haskell.org/ghc/doc/users_guide/exts/ffi.html)
- [OCaml C 接口与 blocking section](https://ocaml.org/manual/5.3/intfc.html)
- [Erlang NIF 与 dirty scheduler](https://www.erlang.org/doc/apps/erts/erl_nif.html)
