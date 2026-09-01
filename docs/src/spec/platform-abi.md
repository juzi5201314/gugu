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

Linux 目标的默认镜像不依赖动态解释器或 libc 初始化。Windows 目标的默认镜像不链接 CRT，并通过薄 IAT 使用运行时所需的系统 DLL。额外动态库只能通过显式 FFI 构建配置加入，不能由宿主链接器隐式补齐，详见[程序与编译模型](program-model.md)和[包、依赖与构建模型](packages-builds.md)。

### 新目标登记

增加目标必须同时提供并登记：

- `arch`、`os`、字节序、指针宽度和 C 数据模型；
- 外部 C ABI 的权威文档与 Gugu 类型映射；
- 对应的对象文件格式、入口、重定位、导入/导出和展开信息；
- runtime、标准库底层接口和 `cfg` 组合；
- 目标专属的 FFI、布局、镜像和运行时测试。

在这些内容进入规范和实现之前，目标名不能出现在清单、条件依赖或缓存键中。增加目标是规范变更，不是用户通过自定义 cfg 临时注册目标。

### 宿主与目标

`host` 是执行 `build.gg` 和工具链辅助程序的平台；`target` 是普通程序、测试或 bench 镜像的平台。二者可以不同。目标相关的 `cfg` 和 `[target.'cfg(...)']` 依赖按 target 求值，`build.gg` 的宿主条件按 host 求值。host 不能改变 target 的数据模型、ABI 或镜像格式。

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

### CPU 基线

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

`#[repr(C)]` 只固定布局，不固定对象地址。把 GC 对象的地址交给外部代码前，调用方必须在整个外部保留期间使用 `std.mem.pin`，或复制到不会移动的缓冲；C 不能在允许的生命周期之外保存该指针。

外部函数返回后，编译器必须按声明类型构造 Gugu 值。外部代码返回无效 `bool`、`char`、枚举判别值或违反聚合布局的位模式时，继续把该位模式当作安全值使用是未定义行为。整数、浮点和原始指针允许全部位模式，但地址是否可访问仍由 unsafe 契约约束。

## C 布局规则

### `repr(C)` 结构体

`#[repr(C)]` 结构体按声明顺序排列字段。每个字段的偏移是前一字段结束后向上取整到该字段对齐的位置；结构体大小向上取整到最大字段对齐。尾部填充属于布局的一部分，`size_of` 必须包含它。

字段的自然对齐和大小来自本章机器数据模型；嵌套 `repr(C)` 聚合递归使用相同规则。字段重排、niche 压缩和未声明的填充复用均禁止。

### `union`、枚举与透明包装

`union` 的所有字段偏移均为零。其大小是最大字段大小向上取整到最大字段对齐；它只允许位类型字段，读取未写入字段仍受[unsafe 与 intrinsic](unsafe.md)的有效位模式规则约束。

无载荷整数 `repr` 枚举的存储类型就是该整数 repr。判别值必须落在声明的变体集合内。带载荷枚举即使写了 `#[repr(C)]`，也只有在其判别字段和载荷组成能被显式 C 结构表达时才可跨边界；本版本推荐使用显式 `struct` 加 `union` 表达 tagged union，不能依赖 Gugu 枚举的隐式字段命名。

`#[repr(transparent)]` 类型与其唯一非 ZST 字段具有相同的大小、对齐和 C ABI 分类。纯 ZST 字段不改变透明包装的 ABI，但纯 ZST 聚合本身不能作为 C 值传递。

`#[repr(packed)]` 去掉字段间填充，但不会改变 C 边界对未对齐访问的安全要求。含有自然对齐大于一的字段时，调用方不能取得对齐引用；应使用原始地址和未对齐读写。packed 聚合是否能由平台寄存器直接传递，按下节的目标 ABI 分类处理；无法满足平台对齐约束时必须走内存分类。

## `extern "C"` 调用约定

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

不超过 16 字节的 `repr(C)` 聚合按平台 eightbyte 分类算法递归处理；超过 16 字节、含未对齐字段或被分类为 MEMORY 的聚合通过隐藏存储指针传递。`i128` / `u128` 遵循现有 `__int128` 约定；整数返回使用 `rdx:rax`，参数必须占用连续的 INTEGER 位置。

调用点的栈对齐、栈参数顺序和寄存器耗尽后的回退规则遵循 System V AMD64 psABI。Gugu 生成的调用点在执行 `call` 前保持 16 字节栈对齐；SysV 的 128 字节 red zone 不能被用来保存需要跨 safepoint、阻塞或外部保留的 Gugu 根。

- 恢复 runtime登记、panic边界、GC根状态与调度状态。

### Microsoft x64 桥接

前四个参数槽按位置使用 `rcx`、`rdx`、`r8`、`r9` 或对应的 `xmm0`–`xmm3`；参数不会像 SysV eightbyte 一样拆到多个寄存器。调用方必须为被调用方保留 32 字节 shadow space，并遵循 Microsoft x64 的栈对齐和 prologue/epilogue 约束。

大小为 1、2、4 或 8 字节且符合平台规则的 `repr(C)` 值可按值进入一个参数槽；其它聚合按指针传递。返回值不符合平台按值返回条件时使用隐藏返回指针，隐藏参数占用第一个参数位置，显式参数整体向后移动。

Windows 没有 SysV red zone。被调用方必须保持平台规定的 `rbx`、`rbp`、`rdi`、`rsi`、`r12`–`r15` 和 `xmm6`–`xmm15` 状态；导入桩和导出函数还必须生成可被 PE unwind 表描述的栈帧。

Windows C ABI 不提供与本章兼容的 `__int128` 参数或返回规则，因此 `i128` / `u128` 在该目标的 `extern "C"` 签名中始终是编译错误。不能依赖把它拆成两个寄存器后“碰巧工作”。

### 调用边界上的运行时责任

外部调用一律视为可能阻塞，调度与逻辑处理器交接遵循[运行时](runtime.md)的规则。跨到 C 代码前，runtime 必须使用该目标可识别的连续栈和合法栈边界；C 代码不能观察或依赖 Gugu 的内部协程栈布局。

`extern "C"` 导出函数被非 Gugu 线程调用时，必须按运行时的线程接入规则登记该线程；导出函数返回后才能撤销临时登记。C 线程不能直接操作 Gugu 协程句柄或 GC 元数据。

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
| Gugu 类型与镜像元数据 | `.gugu.types` / `.gugu.meta` | `.gugutyp` / `.ggmeta` |
| 外部导入 | 动态导入表（仅显式 FFI） | `.idata` |

PE 节名长度和节属性必须符合 PE/COFF 目标限制。`#[link_section]` 指定的节必须在目标格式上可表示，且不能覆盖 runtime、栈图、类型表、导入表或展开表的保留节；非法节名、权限组合和对齐要求都是编译错误。`--strip` 不能删除运行时必需的栈图、展开信息或 GC 元数据，详见[工具链与命令行](toolchain-cli.md)。

### 可执行镜像形式

没有动态 FFI 导入的 Linux executable必须是无 `PT_INTERP` 的 static PIE `ET_DYN`，由 rt0完成镜像自身允许的 relative relocation并支持加载基址随机化；不能退化成依赖 libc/系统 linker的启动路径。显式登记动态 `.so` 后才可以加入 `PT_INTERP`、`DT_NEEDED`、GOT/PLT和对应 relocation，解释器与 sysroot必须来自选中的 target/toolchain描述而不是宿主 PATH探测。

Windows executable和 `cdylib` 使用 PE32+，包含合法 base-relocation table并设置 ASLR、high-entropy ASLR和 NX兼容标志；默认不导入 CRT。preferred image base、file/section排列和 padding是当前 writer实现细节，外部代码只能依赖本章登记的导入导出、逻辑节、入口、TLS和展开面。

static PIE自重定位、PE header字段、section排序和 archive编码见[后端内部规范](../internals/backend.md)，不得在该文档扩展本节公开镜像面。

### 入口、重定位与 TLS

Linux 镜像的入口由 ELF `e_entry` 指向 rt0；`_start` 是默认启动约定。Windows 镜像的 PE 入口同样直接指向 rt0 初始化路径。入口函数不经过 Gugu 普通函数 ABI，也不能触发 GC 或依赖已初始化的 runtime。

编译器直接写出 ELF 或 PE，不把未解析的内部符号交给系统 `ld` 事后处理。所有重定位、节偏移、导入、入口和展开信息必须在镜像写出前验证；目标不支持的重定位是编译错误。

`#[coroutine_local]` 与 `#[os_thread_local]` 的语义分别见[声明与模块](declarations.md)和[内存与对象模型](memory.md)。它们的实现可以使用目标 TLS、runtime 控制块或等价机制，但实现细节不是 C ABI；协程本地槽不能伪装成操作系统线程 TLS，操作系统线程槽也不能在协程迁移后继续当作同一槽使用。

## 操作系统接口

### Linux

rt0 和 runtime 直接使用 Linux syscall 约定完成启动、内存建立、输出和退出；默认不经过 libc。syscall 号、内核内部结构和 vDSO 是否存在不是语言 ABI，不能进入可复现的语言语义或缓存 key。

Linux syscall 桩必须遵循内核规定的寄存器和错误返回约定，并在返回后立即把错误状态转换成 Gugu 的 `std.io` / `std.process` 等错误类型。标准库可以提供显式 libc FFI，但这不会改变默认 runtime 的无 libc 模型。

### Windows

rt0 和 runtime 通过 PE 导入表调用目标注册表允许的系统 DLL；默认启动依赖限于薄的 `ntdll` / `kernel32` 接口，不链接 CRT。禁止硬编码 syscall 号、扫描进程导出表或在运行时隐式加载未登记 DLL。

用户额外导入的 DLL、库名和符号必须在构建配置中登记。导入表由 Gugu 编译器直接写出；缺失导出、名称冲突、导入库未登记和不匹配的调用约定都是编译错误。

普通终止信号的 Gugu 映射、订阅队列和默认动作见[运行时与运维语义](runtime.md)。Linux 的 signal handler、备用栈和 fatal fault 路径必须保持 signal-safe，Windows 的 console/SEH 回调不得直接运行 Gugu 用户代码；具体 syscall、导入符号和 handler 数据结构不是稳定 ABI。用户通过 FFI 修改 signal disposition、mask 或 console handler 后，运行时信号契约不再适用。

### 外部错误状态

`errno`、Windows last-error 或其它线程局部错误状态属于外部调用的即时结果。若标准库提供读取接口，调用方必须在同一操作系统线程上、紧接外部调用之后读取；在 `yield`、阻塞、再次调用外部函数或可能迁移协程之后读取，不能要求仍是原状态。

## 原子与机器状态

语言内存序由[并发](concurrency.md)定义，不能因为 x86-64 的强内存序而把数据竞争变成合法程序。两个已支持目标至少保证自然对齐的 1、2、4、8 字节整数和指针原子操作满足该章节的原子语义；16 字节原子操作不属于本版本的无锁 ABI 保证。

编译器可以使用目标指令实现 `Relaxed`、`Acquire`、`Release`、`AcqRel` 和 `SeqCst`，但具体指令选择不是源程序可观察契约。原子对象必须满足[并发](concurrency.md)的无 GC 引用、自然对齐和有效位模式约束；未对齐或含引用的原子类型是编译错误。

外部调用可能修改 caller-saved 寄存器和平台允许修改的机器状态。Gugu 代码不能依赖未由相应 C ABI 明确保留的寄存器、向量寄存器、标志位或线程局部状态。内联汇编的寄存器约束和 runtime 保留寄存器见[unsafe 与 intrinsic](unsafe.md)，不因本章的 C ABI 映射而放宽。

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
8. GC 对象 pin、外部保存指针、线程局部错误状态和 safepoint 前后的生命周期。

这些检查必须使用确定性的 C 对照程序或固定镜像 fixture；不能用削弱声明、改变 fixture 或忽略失败来获得通过。规范测试的运行方式见[测试](testing.md)。

## 外部基准文档

本章使用下列资料补充未逐项复制的机器级算法；它们不是 Gugu 规范的替代品：

- [Rust 平台支持与 tier](https://doc.rust-lang.org/rustc/platform-support.html)
- [Rust 类型布局与 `repr`](https://doc.rust-lang.org/reference/type-layout.html)
- [Rust `extern` 函数限定符](https://doc.rust-lang.org/reference/items/functions.html#extern-function-qualifier)
- [Go 构建约束](https://pkg.go.dev/cmd/go#hdr-Build_constraints)
- [Go 内部 ABI 说明](https://go.dev/src/internal/abi/abi-internal.md)
- [System V AMD64 ABI](https://gitlab.com/x86-psABIs/x86-64-ABI)
- [Microsoft x64 调用约定](https://learn.microsoft.com/en-us/cpp/build/x64-calling-convention)
