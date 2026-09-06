# AST 与 HIR

本章规定 Gugu 编译器前端从源码快照到已解析、已解析名称且已完成类型检查的 HIR 的内部表示。这里的“必须”约束同一编译器构建中的前端、查询缓存、诊断器和后续 lowering；它不是跨编译器版本的公开 ABI。语言可观察规则仍以 [`spec/`](../spec/overview.md) 为准。

## 权威边界

[语法](../spec/syntax.md)、[词法](../spec/lexical.md)、[声明](../spec/declarations.md)、[表达式](../spec/expressions.md)、[模式](../spec/patterns.md)和[类型](../spec/types.md)唯一规定程序是否合法及其含义。本章只规定合法源码如何存入 AST/HIR和如何携带已经确定的语义选择；节点列表不是第二份语法，lowering表不得改变上游求值顺序、错误出口或绑定规则。两侧不一致时必须修订 internals，不能用前端实现反向解释 spec。

官方 compiler使用 Rust 2024实现；本章的数据结构、query和 verifier按 Rust实现约束书写，但不能把 Rust类型、借用或 ABI暴露给 Gugu程序。

## 阶段边界

前端具有下列固定阶段脊柱；类型检查、早期/late comptime、源码宏展开和抽象分析是显式
query 依赖，不靠可变的全局 phase 回跳：

1. `source_snapshot` 固定本次 action 可见的源码字节和逻辑路径；
2. `lex` 生成 token、换行和注释 trivia；
3. `parse` 生成 AST，不做名称解析和类型判断；
4. `configure` 求值 `cfg`，删除未启用的模块项；
5. `collect_definitions` 为当前展开轮次的模块项建立定义表；
6. `resolve_imports` 和 `resolve_bodies` 解析路径与局部绑定；
7. `lower_hir` 消除纯语法差异并建立 owner-local HIR；
8. `check_signatures` 固定声明类型，并按需 type-check comptime callee body；
9. `expand_source_macros` 执行当前轮次的 `comptime source` 脚本，解析
   `ParsedSource`，登记展开记录并合并生成 AST；
10. 若第 9 步产生新的源码，回到第 4 步开始下一展开轮次；没有新源码宏时，进入
    类型形成和 body 检查。展开轮次受[编译期执行](../spec/comptime.md)的深度、次数、
    字节数和 AST 节点预算约束；不能把半初始化定义表交给下一轮；
11. `evaluate_type_comptime` 固定数组长度、判别值、repr 和泛型实参等早期类型形成输入；
12. `type_check` 生成普通 body 的表达式类型、调整、trait/impl 选择和效果表；
13. `evaluate_early_body_comptime` 固定除 late comptime 外影响布局和控制流的编译期值，
    并为每个 late 求值闭包建立稳定 `LateConstKey`；
14. `validate_hir` 验证穷尽性、确定初始化、控制流出口、安全边界和 late 闭包不反向影响
    类型、源码宏、impl 选择或可达性；
15. `build_generic_gir` 生成已完成语义选择的 generic GIR；late 值保留为
    `LateConstRef(LateConstKey)`；
16. `collect_mono_roots`、`instantiate_gir` 闭合可达单态化实例图并固定具体布局输入；
17. `freeze_type_universe` 收集具体类型，按稳定类型键分配稠密 `TypeId`，产生不可变
    `TypeUniverseKey`；
18. `evaluate_late_comptime` 只读取冻结 type universe，生成不可变 late 常量表；
19. `abstract_analysis` 对闭世界可达的 monomorphic GIR body 求范围、别名、内存版本、
    效果、可达性和调用摘要固定点，并消费 late 常量表；
20. LIR lowering 和代码生成只消费上述稳定结果，不能重新执行源码宏、早期 comptime，
    或让 late comptime 产生新依赖。

源码宏 query 只能读取当前展开轮次已经冻结的源快照、配置、定义、签名和编译期输入。
生成的定义必须在下一轮重新 configure、收集和解析；同一轮不允许宏读取自己或同轮
后续生成的定义。这样宏依赖始终沿展开轮次单向前进，间接递归由 expansion budget
或显式 cycle 诊断终止。

每个 query 只读取不可变产物。`TypeCheck(owner)` 可以依赖它引用的
`EvaluateEarlyComptime(def,args)`；`ExpandSourceMacro(call,round)` 可以依赖宏脚本的
`TypeCheck`、早期编译期值和 `ParseSource`；`EvaluateLateComptime(key, universe)` 只能依赖
冻结的 `TypeUniverseKey`、早期常量和已登记 capability；`WholeProgramAnalysis(world)` 可以
依赖已验证的 HIR、generic/monomorphic GIR、late 常量表及调用摘要。相同稳定 key 再次出现
在普通依赖栈即按 cycle 诊断，允许的递归只由对应 query 显式求 SCC 或 fixpoint，不能读取
半初始化结果。
诊断收集器可以并发接收消息，但不得回写 AST/HIR。某个 query 失败时，下游只可以处理
显式错误占位以继续产生同一根因附近的诊断；错误占位不得进入 GIR、单态化或持久成功
产物。

阶段 23 起，在 monomorphic GIR 未就绪前，`abstract_analysis` 只消费冻结前的 HIR 模块
（字面量与类型事实自足，不回看 `CheckedSemantics` 侧表）；GIR 就绪后输入升级为
monomorphic GIR，query kind 仍为 `WholeProgramAnalysis`。

## 索引与 arena

前端使用稠密 `u32` 索引和 `Vec`/切片 arena，不使用指针作为节点身份：

| 名称 | 表示 | 作用域 |
|------|------|--------|
| `SourceFileId` | `u32` | 一个编译 action 的源码表 |
| `Symbol` | `u32` | 一个编译 session 的 UTF-8 字符串 interner |
| `AstNodeId` | `{ file: SourceFileId, local: u32 }` | 一个源码文件 |
| `PackageId` | `u32` | 已解析的闭世界 package 图 |
| `DefId` | `u32` | 本次编译的定义表 |
| `LocalHirId` | `u32` | 一个 HIR owner |
| `HirId` | `{ owner: DefId, local: LocalHirId }` | 本次编译的 HIR |
| `TyId` | `u32` | 编译器类型 interner；不同于运行时 `TypeId` |
| `ConstId` | `u32` | 规范化编译期值 interner |

这些表的元素数量必须小于 `u32::MAX`；达到上界是 `implementation-limit` 编译错误，不能截断或复用仍存活的索引。实现中的索引构造函数必须用 `debug_assert!` 检查从 `usize` 到 `u32` 的转换。

`AstNodeId.local` 按语法节点起始 token 的先后顺序分配；同一起始 token 的父节点先于子节点。session 的 AST、Symbol 和 SourceTable 身份不得进入持久 HIR。冻结 HIR 可以编码它自身按稳定定义键和确定性遍历重建的稠密索引；这些索引必须连同完整定义、类型和 owner 表一起验证，不能作为跨 query 的独立身份。跨 query 的持久身份由[单态化与编译缓存](monomorphization-cache.md)定义的稳定键承担。

`PackageId` 按锁图中的 canonical package identity byte序分配，`SourceFileId` 再按 `(PackageId, logical_path bytes)` 分配；目录枚举和并行读取完成顺序不参与。`Symbol` 只在 session内点查，持久编码始终写原 UTF-8 bytes，因此 interner插入时序不构成稳定身份。

每类节点独立存放在连续 arena 中。节点之间只保存索引、短枚举、`Span` 和必要的小型标志；可变长度子项保存为同一 arena 内的连续范围 `{ start: u32, len: u32 }`。这种表示的上界来自前述 `u32` 限制，主要访问模式是按 owner 全量遍历和稠密点查，因此不得把每个节点独立装箱或把稠密 ID 放入 `HashMap`。

## 源码快照、token 与 span

`SourceSnapshot` 固定以下字段：

```text
SourceSnapshot {
    logical_path: normalized package-relative UTF-8 path,
    content: immutable byte buffer,
    content_hash: BLAKE3-256,
    line_starts: sorted u32 byte offsets,
}
```

单个源码文件不得达到或超过 4 GiB。源码必须先通过 UTF-8 与词法换行规则验证；`line_starts[0]` 固定为 0，之后只记录规范化识别出的行首。诊断的行列从原始字节快照计算，不能依赖宿主换行转换。

`Span` 为 `{ file: SourceFileId, start: u32, end: u32, expansion: ExpansionId }`，使用半开字节区间。原始源码的 `ExpansionId` 为 0；源码宏生成节点使用非零展开记录，不伪造为调用点原始 span。内建 lowering 生成节点仍使用父节点 span 和非零的内建 lowering 原因编号。

源码宏的每次成功展开登记一个不可变 `ExpansionRecord`：

```text
ExpansionRecord {
    id: ExpansionId,
    parent: ExpansionId,
    macro_call: Span,
    macro_definition: Span,
    generated_source: SourceFileId,
    fragment_kind: SourceSlot,
    source_hash: BLAKE3-256,
}
```

`ExpansionId` 在一次 action 内按外层调用位置、展开轮次和生成片段顺序确定分配；它不是持久语义身份，不能进入稳定定义键、`MonoKey` 或规范常量值。生成节点的路径解析上下文由语言规范的 source slot 规则决定，不能用 `ExpansionId` 的数值偶然消歧。

lexer 输出一个连续 `TokenBuffer`。每个 token 保存 `kind`、半开字节区间和可选 `Symbol`；空白、换行、行注释（`//` / `///` / `//!`）和可嵌套块注释作为 trivia 连续保存在每个 token 的前导范围里。AST 不复制注释正文。格式化器读取同一 `TokenBuffer` 与 AST，因此注释、raw 字符串和字面量原始拼写不会在解析阶段丢失。

`.` 后接数字、以及整数后的孤立 `.`，都拆成 `Dot` 与 `Int`，不形成 `Float`。行末能否结束语句由记号的 `continues_line` 谓词（二元运算符、`(`、`[`、`{`、`,`、`.`、`::`、`..`、`:`、`=`、`=>`、`?`）加上 parser 的括号深度共同决定；词法器把换行保留为 trivia，不插入 `terminator`。`#[]` / `#![]` 内部是扁平记号；词法器校验闭集属性名与 `repr`/`derive`/lint/`ffi`/`cfg` 记号形状，不求值 feature 或自定义 cfg。

`f"..."` 展开为 `FStringStart`、文本片段、插值开闭与内部表达式记号、可选格式说明和 `FStringEnd`。插值内禁止再写 `f"..."`。

非法 UTF-8 在快照阶段拒绝。未闭合字面量/注释、无法形成 token 的字节、非法转义/数字/属性生成词法错误 token 与诊断 `E0009`–`E0019`；parser 必须消费该 token 并建立只覆盖当前恢复范围的错误节点，保证恢复过程单调前进。Frontend 在存在词法诊断或 `Error` token 时失败，不得把错误占位交给 IR 或 image plan。

## AST

### 文件与公共节点头

一个文件的根为：

```text
AstFile {
    source: SourceFileId,
    inner_attributes: AstRange<Attribute>,
    items: AstRange<ItemId>,
    eof_span: Span,
}
```

所有 AST 节点共享逻辑头 `{ id: AstNodeId, span: Span }`。声明节点另有按源码顺序保存的 `attributes`、`visibility` 和 `name_span`。parser 不展开别名、不解析路径、不选择 impl，也不把标识符字符串改写为定义编号。

`ItemKind` 固定包含：

- `Use`、`Function`、`Struct`、`Enum`、`Union`、`TypeAlias`、`Const`、`Static`；
- `Trait`、肯定或否定 `Impl`、`ExternBlock`、`GlobalAsm`；
- `SourceMacro`：尚未执行的 `comptime source` 节点；
- parser 恢复用的 `Error`。

函数、trait、impl、结构体、枚举和类型别名的泛型参数都保存声明顺序、约束和 comptime 标志。结构体字段、枚举变体、trait 项和 impl 项保存源码顺序；任何确定性重排都推迟到定义收集或布局阶段。

### 表达式、语句、模式与类型

`ExprKind` 固定包含以下语法类别：

- 路径、字面量、括号、元组、数组、重复数组、结构体/枚举构造和块；
- `if`/let 链、`match`、`loop`、`while`、`for`、`try` 和 `select`；
- 闭包、`async` 块或调用、`TypeApp`、带 `type_args` 的普通调用、方法调用、字段、元组字段和下标；
- 一元、二元、比较、逻辑短路、赋值、复合赋值和半开区间；
- `unsafe` 块、`comptime`、`SourceMacro`、`intrinsic` 和 `asm`；
- `return`、`break`、`continue`、后缀 `?` 和字符串插值；
- parser 恢复用的 `Error`。

`StmtKind` 固定为 `Let`、`LetElse`、`Assign`、`Defer`、`Yield`、`Expr` 和 `SourceMacro`。块尾是否产生值由最后一个表达式语句的 terminator 状态记录，不通过查看源码末字节重新推断。

`PatternKind` 固定为通配、绑定、`&P` 引用模式、字面量、范围、元组、数组/切片、结构体、构造器、or、`@`、rest 和 `SourceMacro`；不存在 `ref`/`ref mut` 节点。每个绑定节点只保存名字、可变性和独立 span；绑定动作由 HIR 按[模式规范](../spec/patterns.md)生成。

`TypeKind` 固定为路径、元组、数组、切片、引用、原始指针、函数、`dyn Trait`、`impl Trait`、never、`SourceMacro`、推断占位和错误类型。泛型实参保留类型实参、comptime 实参和参数包展开的语法差异。

`SourceMacro` 节点保存脚本 body、插入上下文的 `SourceSlot`、展开预算句柄和调用点 span；它不是普通运行时表达式。解析成功并插入后，成功 HIR 不得残留 `SourceMacro` 节点。

操作符在 AST 中使用封闭枚举，不保存运算符文本。Pratt 优先级以更大数字表示更紧结合；`..` 与比较运算符不结合。`async` 的非块操作数只解析一次调用（可含路径/字段链），随后才继续后缀链。`f-string` 的文本与插值片段各自保存 span。数值字面量同时保存原始 token span 和不带目标类型的任意精度整数/十进制浮点解析结果；类型相关的范围和舍入只在类型检查或 comptime 中完成。

### 解析与恢复不变量

parser 必须满足：

- 每个非 trivia token 恰好属于一个最内层 AST 节点或一个错误节点；
- 子节点 span 位于父节点 span 内，按源码顺序引用的 range 单调递增；
- 恢复只能在匹配的闭合分隔符、换行 terminator、分号或模块项起始 token 处同步；
- 一个缺失 token 只产生一个零宽合成 token，不得被多个节点重复认领；
- 解析结果不受目录枚举顺序、线程完成顺序或 hash 随机种子影响；
- 节点身份是文件内稠密 `u32`，禁止把指针当地址或 dump 键；结构 dump 只按 arena 下标与源码顺序遍历；
- 前缀 `unary`/`paren` 在相同起点时父节点 id 小于子节点；中缀 Pratt 与后缀 wrap 先分配操作数再分配父节点，因此父节点 id 更大。

比较运算符与 `..` 不结合：连续出现时发出主诊断，并附着 `Note` 次诊断指向被拒绝的结合方向。`()` 与 `[]` 增加 parser 的分隔符深度，其内部换行只作空白；`{` 单独跟踪花括号深度，块内换行可以结束语句、字段或 `select`/`match` 臂。`if`/`while`/`match`/`for` 的条件、scrutinee 与迭代器禁止把紧随路径的 `{` 当成结构体字面量。AST 节点数或表长度达到 `u32` 上界时发出 `E0026`（`ParseImplementationLimit`）。

路径的 `.` 只在下一记号是标识符时继续；因此 `use std.io.{print, println}` 在 `.` 后进入分组列表，而 `ch.send(1)` 在表达式里是路径调用（最后一段为方法名），与字段调用 `recv`/`send`/`wait` 在 `select` 臂上等价。

每个 `PathSegment` 独立保存分隔符种类和泛型实参范围。类型上的 `Box[int]::new` 与函数上的 `new::[int]` 不共享实参槽；配置遍历逐段访问实参，避免在路径解析期间丢失类型前缀的实例信息。

## `cfg`、模块表与定义收集

### 配置视图

`configure` 在完整解析后运行，并以本次编译域的不可变 `CfgContext` 求值。上下文固定以下输入：实际编译平台的 `os/arch`、package 已声明与本次已启用的 feature、`test/bench` 模式，以及 `build.gg` 已登记的自定义 flag 或键值。普通 target 使用目标平台；build target 使用 host 平台。未声明 feature、未登记自定义键、错误值类型和错误组合参数必须在定义收集前诊断。

配置结果不改写 parser arena，而为模块、声明、参数、字段、枚举变体、`use` 列表成员、结构体字面量字段、match/select 臂、块语句和表达式列表成员建立稠密 active 表。后续阶段只消费 active 节点：被裁节点不进入导入、名称、类型、comptime 或 codegen。模块级 `#![cfg(...)]` 为假时，整个文件不进入可导入模块表；父节点已被裁时，不再求值其子树属性。

只有删除后仍是完整序列的节点可以 inactive。参数、记录字段、枚举变体、导入列表成员、match/select 臂、块语句、调用/元组/数组等列表元素可以被删除；调用目标、赋值右侧、函数唯一 body、newtype 唯一字段及其它语法必需的单一表达式不能被删除。`cfg` 自身的语法、未知键和值类型始终诊断。

### 模块声明表与导入

每个 target 在前端开始前一次性快照其 source root 下的 `.gg` 文件；目录枚举先排序，隐藏目录、构建输出和符号链接不进入源码树。模块路径由相对 source-root 的规范逻辑路径产生：`foo.gg` 与 `foo/mod.gg` 都声明 `foo`，二者并存是冲突；每个路径分量必须是 ASCII 标识符。active 模块按模块路径字节序分配稠密 `ModuleId`，仅大小写不同的 active 路径必须诊断，不能依赖宿主文件系统的大小写规则。

定义收集完成后解析 `use/pub use`。本地 module/item、当前锁图解析域内的直接依赖别名和保留根 `std` 分开记录；依赖别名来自 normal/test/build 对应域，不能从未启用域泄漏。brace import 可以同时携带目标实际拥有的多个命名空间绑定。公开再导出沿同一导入图解析；循环、找不到的本地目标、跨模块私有访问和同命名空间别名冲突均在 body 名称解析前失败。`use` 不复制目标定义，也不改变目标可见性。

### 稳定定义身份

类型、值、模块、构造器和字段使用分离命名空间。定义候选按精确锁图 package identity、模块规范路径、父定义、源码位置和封闭定义种类收集；局部变量不进入全局定义表，它们在 HIR 中使用 owner-local `LocalBindingId(u32)`。同一作用域、同一命名空间的重复项在稳定身份分配后统一诊断，并附带首次声明范围。

每个定义同时得到：

- `DefPath`：package identity、模块路径、父定义路径、名字、定义种类和同名消歧序号；匿名字段、impl 和生成定义使用 owner 路径、节点起始偏移与封闭 kind；
- `StableDefKey`：`DefPath` 的长度前缀规范编码经 BLAKE3-256 得到的 32 字节值；
- `DefId`：按 `StableDefKey` 完整字节序排序后的稠密 `u32` 编号。

目录枚举顺序、线程完成顺序、hash 随机种子和宿主绝对路径不能进入上述身份。若两个不同的规范 `DefPath` 得到相同 `StableDefKey`，编译器必须报告 digest collision 并停止，不能合并定义或靠源码顺序消歧。

## 声明、表达式与模式检查结果

`frontend::bootstrap` 在配置、定义收集和导入解析后调用唯一的 `semantics::check`。模型先形成声明签名和透明别名，body checker 再收集数值约束、检查位置和控制流、计算初始化状态与模式覆盖；布局计算消费同一份形成后的类型，不重新扫描 token 推断类型。

阶段 13–20 的版本化结果为 `CheckedSemantics`（schema 6），它在 TypeCheck query 中序列化，包含：

- 每个 active 定义的已类型化表达式表、连续局部槽和模式绑定槽区间；表达式按 arena ID 排序、去重，数值变量必须完成收敛。
- 每个局部槽的规范名称和声明字节范围；闭包捕获及清理路径的重检查可以据此指向同一个源码绑定，HIR 不把语义检查遍历中临时分配的槽编号当作持久绑定身份。
- 模块 const/static 的无环初始化顺序与 Process/Coroutine/OsThread 初始化域，以及函数内 static 的声明和延迟初始化器。
- defer 的注册语句、body、函数出口标记和捕获槽；初始化分析保留“已经注册该 action”的条件路径，不能混入未注册分支。
- 除法、移位、数组和切片边界、UTF-8 边界、浮点转整数与 Unicode scalar 检查。检查引用已求值的表达式；安全模式保留检查，unsafe 的下标操作不登记可省略的边界检查。
- 函数项的模块/FnDecl 身份、全部泛型实例实参和调用签名；只有期望擦除签名时才生成函数句柄。无捕获 callable 的布局直接消费捕获表，使用零大小表示，不分配空环境。
- 闭包与 async 块的 `CapturePlan`，按原始槽编号排序，记录读前置条件、写入、跨协程和体内初始化依赖。函数体有独立 return/loop/try/defer 状态；闭包构造不会改变外层初始化结果，也不会把尚未执行的函数体记入初始化依赖。
- 齐次变参和异构类型包的 `VariadicCall`：保留左到右的实参 ID、固定参数数目和具体尾部类型。齐次尾部存储必须可被 GC 跟踪，只有后续分析证明无逃逸才可放入栈帧；异构包供单态化逐位置展开，不生成动态类型数组或盒子。
- 静态关联调用和用户操作符的 `Dispatch`：保存函数身份、选中 impl、trait 实例和成员序号、规范化 Self/签名，以及接收者解引用次数和借用调整。操作符表达式的结果不会被误记成 callable 值。
- APIT 的独立匿名类型参数，以及 RPIT/TAIT 的声明身份、完整泛型环境和唯一隐藏类型表。函数实例参数按声明上下文的规范键顺序保存；`Self::关联项` 是由 Self 推导的查找缓存，不作为独立实例参数，避免关联 TAIT 产生伪递归。
- `TypeAdjustment` 记录转换前后的类型及 `Erase`、`Opaque`、`ArrayToSlice` 种类；`impl Trait` 的表示转换不等同于擦除。具体值进入 `dyn Value` 后再进入 `dyn Any` 时，第二层 payload 的类型仍是 `dyn Value`；复制已经形成的 `dyn Any` 不生成新容器。
- 动态 `Dispatch` 保存对象安全接口和成员序号，不携带静态 callable/impl。`Reflection` 保存 `is`、`downcast`、`downcast_copy` 的精确目标类型与符号化 TypeId 操作，恢复类型不得穿透既有接口对象。
- `MemoryOperation` 保存标准内存原语、源/目标类型与按源码求值的实参 ID；`MaybeUninit` 有独立语义类型，不伪装成已初始化的 T。按位操作的管理属性及大小条件在同一布局模型检查。
- `BorrowCheck` 保存被借用槽的基类型、完整字段/数组投影及目标类型；类型收敛后由统一聚合布局检查最终自然对齐，显式取引用与方法自动借用共用此检查。动态下标只保留步长条件，不重复执行下标表达式。
- `ForeignDefinition` 与 `ForeignCall` 保存 C 声明身份、naked/imported 标志和按调用点优先级形成的 bridge/dirty/leaf 效应。`Linkage` 独立保存函数、static 与全局汇编的符号名、节和 used 状态；两张声明表在缓存命中时对照当前模型验证。
- `AssemblyPlan` 保存求值后的模板、寄存器宽度/方向、输入与输出位置、clobber 位图及 managed/naked/dirty/global 上下文。managed 模板的有限控制流和所有出口的栈增量在前端验证；寄存器值大小由布局检查，机器编码仍属于后端。
- `FormattingPart` 保存闭集格式码、填充/对齐/标志以及固定计数或已解析的 `int` 局部槽；动态 width/precision 经过读取、初始化和捕获检查，不把 `name$` 文本留给后端解析。

局部槽的存储需求编码为三个位：`ADDRESS_TAKEN`、`CAPTURED`、`CROSS_COROUTINE`。这些位与捕获表一起交给 HIR/GIR 的存储选择；捕获或跨协程槽不能仅因创建它的词法块结束而销毁。分析记录 callable 值在求值时引用的槽，遮蔽或后续函数值赋值不能重新绑定已经形成的闭包环境。

TypeCheck query 输入覆盖规范路径、源码内容、cfg 和稳定名称解析结果。schema verifier 验证后的 `CheckedSemantics` 只用于布局和 HIR 形成，不再作为后端的平行输入。`LowerHir` query v1 登记真实 TypeCheck 依赖 fingerprint，并加入入口和源码展开上下文；成功结果经完整 HIR verifier 后序列化。缓存命中重新验证 Module、输入身份和规范字节，不能从缓存直接恢复 `Validated` 凭据。失败诊断保存级别、顺序、附注、展开身份及逻辑文件字节范围，并在命中时重绑定当前 `SourceMap`。任何检查失败均中止 BuildIr 及后续产物路径。

镜像计划只从冻结 HIR 读取入口、owner 数量和域隔离的 BLAKE3 指纹；原先仅生成 `main -> ReturnUnit` 的 `ir.rs` 已删除。GIR cleanup CFG、外部桥接执行和汇编机器编码分别由路线图对应阶段接入。隐藏类型只向布局和单态化揭露，外部调用按声明约束检查；运行时稠密 TypeId 分配、vtable 物化和实际容器分配分别属于冻结类型集合及后续 lowering 阶段。

trait 表先收集声明和 impl 头，形成关联类型后再检查方法签名；关联项不泄漏到模块值命名空间。特化使用类型模式包含关系和交集检查，重复参数必须保持相等约束。否定 impl 与肯定 impl 共用选择部分序；泛型调用的 trait 义务在实参推断收敛后验证，失败时保留约束或否定实现的源码位置。关联投影保存 Self、trait 实例和成员名称的完整身份，不能仅以短名称等同两个投影。

语言认识的 `Index`、复合赋值、`Try`、`IntoIter` 和 `Iter` 使用同一接口表；`?` 保留 operand 的 `branch` 与目标的 `from_error` 派发，`try` 正常出口保留 `from_value` 派发。用户 `IntoIter` 的关联迭代器必须实现 `Iter`，两侧 `Item` 投影必须一致；具名泛型、APIT 与不透明返回约束共用关联义务闭包。关联常量与数组长度共用类型模型中的常量求值路径；具体值保留类型，并在特化检查中比较求值结果而不是源码拼写。

## HIR

### owner 与节点表示

每个具名函数、闭包、async 块、常量求值体、模块或局部 static 初始化体、带默认实现的 trait 项和全局汇编都有独立 owner。owner 保存连续的表达式、语句、模式、局部绑定和作用域 arena：

```text
HirOwner {
    def: DefId,
    params: HirRange<PatId>,
    body: ExprId,
    exprs: Arena<HirExpr>,
    stmts: Arena<HirStmt>,
    patterns: Arena<HirPattern>,
    scopes: Arena<HirScope>,
}
```

HIR 表达式按确定性的父先子后顺序分配 owner-local `ExprId`；关联变长子项使用连续索引池。父子关系、词法作用域和控制流目标都使用 owner-local 稠密 ID。跨 owner 的定义引用使用 `DefId`；捕获来源必须同时保存父 owner 的 `DefId` 和它的 `LocalId`，不能单独保存另一 owner 的局部编号。

模块级 HIR 另存定义签名、泛型参数、where 约束、字段/变体、trait 项和 impl 头。函数体不会内嵌到调用者 HIR，内联只在单态化 GIR 上发生。

### 名称解析结果

每个 HIR 路径必须解析为封闭的 `Res` 枚举：

- `Def(DefId)`；
- `Local(LocalBindingId)`；
- `Primitive(PrimitiveId)`；
- `Builtin(BuiltinId)`；
- `Associated { definition, self_ty, interface }`。

成功 HIR 的 `Res` 不提供错误占位分支。方法、操作符、下标、调用和关联项的最终选择不以字符串保存。它们记录选中的 `DefId`、内建操作编号或 `dyn` 成员槽；trait 选择记录具体 impl、规范化 Self/签名、泛型实参和接口实例。接收者解引用、自动借用及 UFCS 是否隐含接收者单独记录，后续阶段不得重新按名字搜索一次。

### 语法归一化

HIR 保留对诊断有价值的 `if`、`match`、循环、`try`、`async`、`select`、模式和 `defer` 结构，但消除以下纯语法差异：

| AST 形式 | HIR 表示 |
|----------|----------|
| 表达式体函数 | 与块体相同的单表达式 body |
| 复合赋值 | 单次求值 place、封闭运算符及已选择派发，保留写回目标 |
| 方法调用 | 已固定目标与接收者调整的 `Call` |
| 用户类型下标 | 带已选择 `Index::index` / `index_set` 派发的 `Index` |
| `for pattern in value` | 保留单次迭代源、模式和 body 的 `For`，携带 `into_iter` / `next` 派发 |
| `expr?` | `HirTryExit { operand, branch_slot, from_error_slot, target }`；槽位与结果规则只引用 [`Try` 规范](../spec/traits.md#try) |
| 字符串插值 | 按源码顺序保存解码文本、类型化值和结构化 FormatSpec；动态计数是 int 读取节点 |
| `if let`、`while let`、let 链 | 共享被匹配临时槽的条件/模式节点 |
| 参数位置 `impl Trait` | 独立隐式类型参数 |
| 返回位置 `impl Trait` | owner 下的独立 opaque 定义 |

`async`、`select` 和 `defer` 在 HIR中保留专用节点，并各自携带由[表达式规范](../spec/expressions.md)生成的一次求值、出口和提交/cleanup计划；GIR只能消费该计划，不能按节点名重新解释随机、公平、取消或展开语义。

字符串转义、字节字符串的单字节 `\xHH` 与 Unicode UTF-8 编码、f-string 的双花括号都在此边界解码。格式能力用闭集格式种类携带，不把格式字符串交给 GIR 重解析；标准库格式 trait 执行与 builder lowering 在阶段 61 完成。

## 语义检查输出

阶段 13–15 的 body checker 在 `NameResolution` 完成后消费配置视图。每个函数 owner 建立独立的局部槽表：遮蔽分配新槽，分支状态按所有可达前驱求交，未初始化槽不能作为读操作数。表达式检查产出唯一 `Ty`，对 `never` 采用合流规则；place 检查与赋值定位共享同一接收者和下标求值。

模式检查先生成绑定计划，再执行构造器/标量区间覆盖矩阵。守卫不贡献覆盖，or 模式必须拥有相同绑定集合和类型；覆盖矩阵只拆分边界区间和构造器，不枚举无限标量域。检查失败通过 `Diagnostic` 返回，失败的语义结果不能进入后续 lowering。

### 类型与调整侧表

HIR 节点本体不复制完整类型。每个 owner 的 `expression_inputs` 与 `expression_types` 是与表达式 arena 等长的稠密 `Vec<TypeId>`，分别保存调整前、后的表示；模式和局部绑定只保存 interned `TypeId`。例如结构体先构造再擦除到 `dyn Any` 时，字段检查读取源结构体类型，不能误用擦除后的接口类型。

表达式的 `adjustments: Range<u32>` 指向 owner 级连续池。调整闭集为 `Dereference`、`ArrayToSlice`、`NeverTo`、`Erase`、`Opaque`、`Instantiate`；verifier 从输入类型逐步验证调整并要求最终结果等于输出类型。方法自动借用和解引用次数保存在选中派发中，不复制成另一套类型推断。池和索引必须在 `u32` 上界内，任意合法多层引用不受固定深度限制。

派发、捕获、变参、借用约束、外部调用效应、运行时检查、清理和汇编分别保存在 owner 级连续表。调用参数、模式子项、匹配臂、格式片段与退出作用域链使用范围引用共享池，后续阶段不必重新访问 AST。

`Effects` 的 8 个 compiler 布尔标志使用零分配 `u32` 位掩码，固定为 `PANIC`、`ALLOCATE`、`SAFEPOINT`、`SUSPEND`、`READ`、`WRITE`、`FOREIGN` 和 `UNSAFE`。构造时以 `debug_assert!` 验证已知位，冻结时再次检查。`ForeignCall` 另存调用表达式与已选择的 bridge/dirty/leaf 效应和 leaf stack budget；普通/dirty bridge 携带交接所需 safepoint/suspend，leaf 不因外调本身添加这两个标志。这些是后续 GIR 消费的操作事实，不是用户可观察的效果类型系统。

### 捕获计划

每个闭包/async owner 的 `Capture` 保存环境局部槽、直接父 owner、父局部槽，以及读前置条件、写入和跨协程标志。来源按源码绑定身份归并，遮蔽创建独立局部槽；递归闭包先建立绑定再形成初始化器，因此可以引用自己的槽。冻结校验要求来源 owner 恰为定义父级，来源槽存在且类型一致。

当前 HIR 按共享根槽记录捕获，保留同一绑定的别名语义；投影分拆与物理环境字段布局属于后续存储选择。只读格式计数同样进入捕获表，不能因它只出现在格式说明里而漏记。

HIR只固定哪些源码位置必须共享及其访问摘要，不决定 stack/heap或把只读值复制进环境。`EscapeAndPlacement`依据该计划选择 direct value、parent-environment projection或 shared slot；无论选择什么，都必须满足[函数与闭包](../spec/functions.md#捕获语义)这一唯一公开语义。

### 类型检查顺序

类型检查按定义依赖 SCC 运行：

1. 验证签名和类型形成；
2. 建立 owner-local 类型变量和约束；
3. 解析固有项、trait、关联类型和特化；
4. 求解表达式、模式与返回/错误出口类型；
5. 固定闭包捕获方式和 `impl Trait` 隐藏类型；
6. 运行穷尽性、确定初始化、place 和 unsafe 检查；
7. 规范化所有投影并冻结 `TypeckResults`。

同一 SCC 中只允许函数/trait 签名先声明后检查函数体。需要一个常量值才能形成类型的环、关联类型无法规范化的环以及 impl 选择自依赖都是编译错误。错误恢复用的 `TyId::ERROR` 不能成为成功 query 的输出或缓存键。

## HIR 冻结条件

阶段 12b/20 的 `Module::verify` 独立检查源范围与展开链、定义稳定排序和无环父关系、类型/声明引用、owner 与所有侧表索引、表达式前向子边和可达性、构造器字段域、捕获来源、类型调整链及控制流出口的完整清理作用域链。`Validated` 的字段私有，唯一构造入口只对 frontend 可见；它持有不可变 `Arc<Module>` 和规范序列化指纹，backend 接口只接受 `&Validated`。

一个 owner 只有同时满足适用阶段的以下条件才可以冻结并交给 GIR；EarlyConst、源码宏、LateConst 的专用条件在对应阶段接入同一门禁：

- 不含 `Res::Error`、错误类型或未求解类型变量；
- 所有路径、调用、操作符、关联项和 impl 已唯一选择；
- 所有早期 comptime 实参、数组长度、判别值和布局属性已求值；每个 late 表达式已经验证并归一化为稳定 `LateConstKey`；
- 每个控制流出口对应确定的作用域清理链；
- 每个读取 place 在该点确定初始化，所有 unsafe 操作位于合法边界；
- `async` 捕获、跨 suspend 活跃值和 `select` 分支载荷已经固定；
- owner 的稳定输入摘要已经计算，且不含 session-local 数字 ID。

冻结后的 HIR 与侧表不可修改。late comptime 通过独立结果表解析 `LateConstKey`，不回写 HIR；优化、资源动作展开、协程 lowering、逃逸分析和布局选择都属于后续 GIR/LIR 阶段。

## 诊断与确定性

阶段可以并行处理 owner，但最终诊断按规范路径、起始字节、结束字节、诊断代码和稳定定义键排序。相同位置的主诊断先于附注和建议。类型变量编号、hash 表遍历、线程完成顺序不能出现在用户诊断、HIR dump 或持久摘要中。

调试 dump 使用 owner 的 `DefPath`、`LocalHirId` 和规范化类型文本；默认不打印裸 `DefId`、`TyId` 或地址。dump 只用于实现检查，不进入编译 action 的可观察输出摘要。

## 与后续阶段的契约

HIR 向后续阶段只提供：冻结的定义签名、owner body、类型/调整/impl 选择、早期 comptime 值、`LateConstKey`、控制流作用域和稳定输入摘要。GIR 构造器不能读取 token 文本重新推断语义，也不能绕过 HIR 重新执行名称解析；late 值只能从对应 `TypeUniverseKey` 的不可变结果表读取。具体控制流、资源管理、GC 与调度 intrinsic 见 [GIR 与 LIR](gir-lir.md)。

## 参考实现资料

本章借鉴 Rust 编译器的 AST/HIR owner 分层、稠密索引和 query 边界，以及 Go 编译器按 package 固定输入、在 SSA 前完成类型检查的做法；Gugu 的节点种类和 lowering 仍由自己的语言规范决定：

- [Rust 编译器开发指南：编译器总览](https://rustc-dev-guide.rust-lang.org/overview.html)
- [Rust 编译器开发指南：HIR](https://rustc-dev-guide.rust-lang.org/hir.html)
- [Go 编译器源码说明](https://go.dev/src/cmd/compile/README)
