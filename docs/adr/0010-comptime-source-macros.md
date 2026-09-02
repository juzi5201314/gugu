# ADR-0010：语言级 comptime 源码宏与全程序抽象分析

## 状态

已接受。

## 背景

Gugu 原有的 comptime 设计只描述在编译期计算普通值，例如常量、数组长度、布局参数和特化输入。这个模型不能表达由编译期脚本自由生成 Gugu 源码的能力，也不能单独解释跨函数的范围证明、边界检查消除和 lint 分析。

目标能力有两个边界：

1. 编译期脚本可以像 procedural macro 一样生成任意合法源码片段；
2. 编译器可以跨函数、模块和 package 分析未知运行时值，但不能把一次具体执行误当成对所有输入的证明。

## 决定

### 源码宏

新增显式的 `comptime source { ... }` source slot。其块体是普通 Gugu comptime 脚本，由轻量解释器执行。脚本自由生成字符串，但源码只有经过 `std.syntax.parse_source` 或指定片段类别的 `parse_*` 函数后，才能成为 `ParsedSource` 并由编译器自动展开。

`ParsedSource` 是 compiler-owned 的不透明编译期值，不能由用户构造，不能直接返回 HIR/GIR/LIR，也不能进入运行时对象或目标镜像。脚本可以返回 `ParsedSource` 或 `Result[ParsedSource, E]`；最终 `Err` 变成编译诊断，语法解析错误在边界内可以被脚本捕获并转换。

解析成功不代表生成程序语义正确。生成片段要重新经过适用的 `cfg`、定义收集、名称解析、类型检查、初始化、trait/impl、unsafe、ABI 和 HIR lowering。生成 item 的宏闭包稳定后，才冻结闭世界可达性、单态化和 `TypeId` 集合。

源码宏允许出现在模块项列表、块语句列表、表达式、类型和模式位置；`ParsedSource` 的片段类别必须适配所在 source slot。生成代码中的自由路径按调用点作用域解析，宏脚本局部绑定不泄漏到生成代码；每个生成节点保留 `ExpansionId` 和展开链以支持诊断。

### 递归和资源

生成源码可以再次包含 `comptime source`。递归不通过语法特判禁止，而由展开深度、总展开次数、生成字节数、AST 节点数、comptime fuel 和 comptime heap 预算共同限制。`#![comptime(expansion_limit = N)]` 或源码宏位置属性可以在 compiler profile 全局硬上限内调整局部深度；达到任一预算时报告带完整展开链的错误。

宏展开 query、生成文本 parser query 和普通 comptime query 分离。展开 round 之间只传递已经冻结的不可变定义和输入，不读取半初始化结果；相同稳定展开栈 key 形成 cycle 时报告 cycle。

### 抽象分析

值 `ConstEval`、源码 `SourceExpand` 和运行时 `AbstractAnalysis` 使用不同值域，但可以共享 typed HIR/GIR 的语义节点。`AbstractAnalysis` 在闭世界可达的 Gugu、标准库和 runtime body 上尽可能做跨函数、跨模块、跨 package 的摘要固定点求解，支持范围、符号关系、别名、内存版本、效果、初始化和控制流事实。

分析结果必须健全但允许不完备：`proved` 才能删除边界或其它运行时检查；别名不明、未知调用、FFI、并发写入、预算耗尽或固定点不收敛都产生 `unknown`，并保留检查。类型推断和 trait/impl 选择仍由类型系统的约束求解完成，lint 只消费分析事实而不改变程序语义。

## 后果

- 语言获得了不依赖 AST builder 的自由源码生成能力，字符串到代码必须经过显式 parser 闸门。
- 生成代码仍由主前端负责语义检查，因此宏不能绕过类型安全、初始化、安全和 ABI 规则。
- 编译器需要维护展开 source map、query/cache 输入、递归预算和宏诊断链。
- 闭世界编译可以提供跨 package 分析和更激进的检查消除，但分析成本必须受预算约束；无法证明时程序保持原有运行时检查。
- parser、展开、摘要和优化结果都必须按稳定输入缓存，不能依赖线程完成顺序、宿主地址或 session-local ID。

相关规范：[编译期执行](../src/spec/comptime.md)、[形式语法](../src/spec/syntax.md)、[comptime 与抽象分析](../src/internals/comptime-analysis.md)、[AST 与 HIR](../src/internals/ast-hir.md)、[单态化与编译缓存](../src/internals/monomorphization-cache.md)。
