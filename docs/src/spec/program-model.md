# 程序与编译模型

## 编译形态

Gugu 官方工具链把程序 AOT 编译成本地镜像；字节码 VM、运行时 `eval` 和开世界代码加载不属于本规范。实验性 JIT 只能作为同一闭世界程序的执行实现，不能扩大程序可见的函数、类型或 ABI；具体 compiler/runtime实现语言和 IR/后端结构见 [`internals/`](../internals/ast-hir.md)。

## 闭世界与全程序编译

一次产生可执行文件或 C 导出库的编译必须看见全部可达的 Gugu 代码：所选 target、其解析后的 package 依赖图、标准库和用 Gugu 写的 runtime。项目工具按 `gugu.toml` 与 `gugu.lock` 选择 target 入口，底层编译器从该入口开始闭世界可达性，见[包、依赖与构建模型](packages-builds.md)与[声明与模块](declarations.md)。命令行入口与参数见[工具链与命令行](toolchain-cli.md)。

因此，程序可观察模型满足：

- 执行开始前，可达的 Gugu 函数、具体类型、impl、vtable、闭包和导出集合已经封闭；
- 每个拥有 `TypeId` 的具体类型在该镜像中有唯一稠密编号，运行时不能追加类型；
- 运行期间不能引入新的 Gugu 函数、类型布局或 impl 选择；
- 运行期间不存在 `eval`、字符串转代码或对任意值执行动态函数调用；编译期间的显式 `comptime source` 只能在闭世界中生成并重新解析源码，不能把动态代码加载能力带入运行时。
- 跨语言执行只通过规范规定的 C ABI、系统调用和平台入口。

可达性收集、单态化、源码宏展开、抽象分析、布局、栈图与管理动作的内部顺序见[单态化与编译缓存](../internals/monomorphization-cache.md)、[comptime 与抽象分析](../internals/comptime-analysis.md)、[GIR/LIR](../internals/gir-lir.md)和[GC 元数据](../internals/gc-metadata.md)，不在本章重复定义。

## 支持目标

本版本钉死必须支持的两个目标；目标名、目标键、数据模型和平台 ABI 的完整登记见[平台与 ABI 参考](platform-abi.md)：

| 目标名 | 备注 |
|------|------|
| `x86_64-linux` | System V AMD64 ABI；系统调用用 `syscall` 指令，不依赖动态链接器才能运行。 |
| `x86_64-windows` | Microsoft x64 ABI；PE 导入表只挂显式登记的系统 DLL，禁止把 CRT 当默认运行时。 |

同一套语言语义必须在两个目标上成立。调用约定、可执行文件格式和对齐可以因目标而变；类型布局若无 `#[repr(...)]` 约束，也可以因目标而变，但在给定目标上必须确定。

`i128` / `u128` 的语言布局在两个目标上都是 16 字节、对齐 16；`extern "C"` 的平台差异见[平台与 ABI 参考](platform-abi.md)和[类型](types.md)。

按目标裁代码用 `#[cfg]`，见[词法 · cfg](lexical.md)。被裁掉的项在该次编译中不存在。

## 诊断

编译器必须提供错误与 lint 两条通道，见 [词法 · 诊断](lexical.md)。错误阻止产出镜像。lint 默认按各级别处理：`warn` 不阻止产出，`deny` / `forbid` 阻止。禁止用类型错误冒充 lint（大拷贝、丢掉 `Result` 都不是类型错误）。

## 测试模式

编译器必须提供测试构建（与生产镜像分开），见 [测试](testing.md)。测试构建里 `cfg(test)` 为真，并链入测试运行器而不是只调用户 `main`。

## 不使用系统链接器

发布版 Gugu 工具链必须能从声明的 Gugu/native输入直接产生目标镜像，不把成功建立在调用宿主 `ld`、`link.exe`、`lld` 或其它系统链接器上。该保证只覆盖 Gugu 支持的闭世界镜像和[包构建](packages-builds.md)允许的 native输入，不承诺通用链接器的任意 `.o`、脚本或平台扩展语义。

内部符号解析、dead-code elimination、section布局、重定位和 ELF/PE直接写出由[x86_64 后端](../internals/backend.md)唯一规定。开发测试可以把系统工具用作结果对照，但不能成为发布命令的执行依赖或改变产物。

## 不用 libc 当语义层

语言的默认运行模型必须是：

- Linux：rt0 直接 `mmap` / `write` / `exit` 等 syscall，用户程序默认不链接 `libc`。
- Windows：rt0 / 最薄 runtime 通过 PE 导入表调 `ntdll`/`kernel32`，默认不链接 CRT。
- 标准库可以提供薄的 libc 绑定，供明确选择 FFI 的代码使用；这是库，不是语言的内存或字符串语义。

默认没有外部共享库导入时，Linux 产物是无动态解释器依赖的静态 ELF。程序显式声明并配置 `extern "C"` 共享库导入时，编译器可以改为写出带动态解释器、`DT_NEEDED` 和重定位表的 ELF；这属于显式 FFI 构建，不改变默认运行模型，且仍由编译器直接写镜像而不调用系统链接器。Windows 的 kernel32/ntdll 薄 IAT 属于默认启动依赖，额外 DLL 同样必须显式配置。

## 目标镜像

可执行镜像包含目标 rt0、与该编译器构建配套的 runtime、标准库和闭世界用户程序。rt0 是平台入口而不是普通 Gugu函数；它只负责把宿主进程交给满足[运行时启动契约](runtime.md#rt0-与启动)的环境。

镜像是否含动态解释器、默认系统导入、保留 metadata节和外部 ABI由[平台与 ABI 参考](platform-abi.md)唯一规定；内部 fragment、relocation、stack map和启动编码见[后端](../internals/backend.md)。主协程返回、panic、`process.exit`和 fatal之后的状态转换只见[运行时](runtime.md#进程寿命)。

## 编译器内部表示

AST、HIR、GIR、LIR、query、单态化实例、精确根、stack switch、写屏障、object metadata和后端 relocation都是官方实现内部契约，分别见 [AST/HIR](../internals/ast-hir.md)、[comptime 与抽象分析](../internals/comptime-analysis.md)、[GIR/LIR](../internals/gir-lir.md)、[单态化与缓存](../internals/monomorphization-cache.md)、[栈图](../internals/stack-maps.md)、[GC 元数据](../internals/gc-metadata.md)和[后端](../internals/backend.md)。它们不是用户语法、库调用约定或跨编译器 ABI。

实验性 JIT若存在也必须消费同一闭世界结果并满足本章公开语义；其分层编译、patch point和执行缓存只属于 internals，不能成为新的加载/反射能力。

## 编译闭包与产物确定性

闭世界集合从入口、标准库/runtime、测试/导出/`used` 根、comptime 显式引用和每个 late
comptime 的静态 callee 闭包得到；`cfg` 删除项不在集合中，而 `type_id[T]()`、导出或
`#[used]` 引用仍使对应定义可达。具体图遍历和实例键见[单态化与编译缓存](../internals/monomorphization-cache.md)。

实例图闭合后，编译器冻结具体类型集合与稠密 `TypeId`，再执行只读该集合的 late
comptime。`type_id_count()` 与 comptime `TypeId.as_int()` 不能反向参与类型形成、源码宏、
impl 选择或可达性；完整限制见[编译期执行](comptime.md#早期与-late-comptime)。无法形成
有限闭世界、late 求值试图新增依赖、缺失 lang item 或依赖无法解析都是编译错误。

同一编译器构建身份、同一目标名、同一 target/harness/插桩、相同 feature 与相同输入字节
必须产生语义等价的镜像；源文件遍历顺序、哈希表随机种子、操作系统目录枚举顺序不能改变
符号选择、特化结果、测试收集顺序或 `TypeId.name()`。实现可以在非语义节中写入构建标识，
但可复现构建不得引入时间戳和随机标识。

编译输入包括所选 target 源树、解析后的锁图与 feature、标准库/runtime 源码、目标配置、
test/bench/插桩选择、build.gg 的已声明输入和输出、显式 FFI 导入配置、`embed_file` 读取的
文件、源码宏脚本/展开属性/生成文本、comptime capability registry、冻结 type universe、
late 常量结果以及被消费的跨 package 公共分析摘要。未由这些输入覆盖的环境变量、当前工作
目录、网络、系统时间和宿主进程状态不得影响语言级 comptime 结果。

## 目标 ABI 与镜像错误

只有 `extern "C"`、导出符号、平台登记的镜像面和 rt0入口受[平台与 ABI 参考](platform-abi.md)约束。Gugu internal call、闭包环境、runtime私有对象 metadata、enum优化和默认字段布局都不是跨编译器 ABI；当前表示由[后端](../internals/backend.md)与[GC 元数据](../internals/gc-metadata.md)版本化。

ELF/PE 写出前必须验证所有重定位、节偏移、导入符号、栈展开信息和入口点均可表示；溢出、重复导出、缺失导入库或符号、非法节属性和目标不支持的重定位都是编译错误，不能产出部分镜像冒充成功。




