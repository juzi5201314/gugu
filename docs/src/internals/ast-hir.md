# AST 与 HIR

本章规定 Gugu 编译器前端从源码快照到已解析、已解析名称且已完成类型检查的 HIR 的内部表示。这里的“必须”约束同一编译器构建中的前端、查询缓存、诊断器和后续 lowering；它不是跨编译器版本的公开 ABI。语言可观察规则仍以 [`spec/`](../spec/overview.md) 为准。

## 权威边界

[语法](../spec/syntax.md)、[词法](../spec/lexical.md)、[声明](../spec/declarations.md)、[表达式](../spec/expressions.md)、[模式](../spec/patterns.md)和[类型](../spec/types.md)唯一规定程序是否合法及其含义。本章只规定合法源码如何存入 AST/HIR和如何携带已经确定的语义选择；节点列表不是第二份语法，lowering表不得改变上游求值顺序、错误出口或绑定规则。两侧不一致时必须修订 internals，不能用前端实现反向解释 spec。

官方 compiler使用 Rust 2024实现；本章的数据结构、query和 verifier按 Rust实现约束书写，但不能把 Rust类型、借用或 ABI暴露给 Gugu程序。

## 阶段边界

前端具有下列固定阶段脊柱；类型检查、comptime、源码宏展开和抽象分析是显式 query
依赖，不靠可变的全局 phase 回跳：

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
11. `evaluate_type_comptime` 固定数组长度、判别值、repr 和泛型实参等类型形成输入；
12. `type_check` 生成普通 body 的表达式类型、调整、trait/impl 选择和效果表；
13. `evaluate_body_comptime` 固定剩余影响布局和控制流的编译期值；
14. `validate_hir` 验证穷尽性、确定初始化、控制流出口和安全边界；
15. `build_generic_gir` 生成已完成语义选择的 generic GIR；
16. `collect_mono_roots`、`instantiate_gir` 闭合可达单态化实例图并固定具体布局输入；
17. `abstract_analysis` 对闭世界可达的 monomorphic GIR body 求范围、别名、内存版本、效果、可达性和调用摘要固定点，结果供 GIR 优化、lint 和代码生成查询；
18. LIR lowering 和代码生成只消费上述稳定结果，不能重新执行源码宏或 comptime。

源码宏 query 只能读取当前展开轮次已经冻结的源快照、配置、定义、签名和编译期输入。
生成的定义必须在下一轮重新 configure、收集和解析；同一轮不允许宏读取自己或同轮
后续生成的定义。这样宏依赖始终沿展开轮次单向前进，间接递归由 expansion budget
或显式 cycle 诊断终止。

每个 query 只读取不可变产物。`TypeCheck(owner)` 可以依赖它引用的
`EvaluateComptime(def,args)`；`ExpandSourceMacro(call,round)` 可以依赖宏脚本的
`TypeCheck`、编译期值和 `ParseSource`；`WholeProgramAnalysis(world)` 可以依赖已验证的
HIR、generic/monomorphic GIR 及其调用摘要。相同稳定 key 再次出现在普通依赖栈即按
cycle 诊断，允许的递归只由对应 query 显式求 SCC 或 fixpoint，不能读取半初始化结果。
诊断收集器可以并发接收消息，但不得回写 AST/HIR。某个 query 失败时，下游只可以处理
显式错误占位以继续产生同一根因附近的诊断；错误占位不得进入 GIR、单态化或持久成功
产物。

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

`AstNodeId.local` 按语法节点起始 token 的先后顺序分配；同一起始 token 的父节点先于子节点。`DefId`、`TyId` 和 `LocalHirId` 只用于内存内的稠密访问，不能直接写进持久缓存或最终镜像。持久身份由[单态化与编译缓存](monomorphization-cache.md)定义的稳定键承担。

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

lexer 输出一个连续 `TokenBuffer`。每个 token 保存 `kind`、`Span` 和可选 `Symbol`/规范化字面量 ID；空白、换行、行注释和块注释作为 trivia 连续保存，并由 token 的前后范围引用。AST 不复制注释正文。格式化器读取同一 `TokenBuffer` 与 AST，因此注释、raw 字符串和字面量原始拼写不会在解析阶段丢失。

非法 UTF-8、未闭合字面量和无法形成 token 的字节生成词法错误 token；parser 必须消费该 token 并建立只覆盖当前恢复范围的错误节点，保证恢复过程单调前进。

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
- 闭包、`async` 块或调用、普通调用、方法调用、字段、元组字段和下标；
- 一元、二元、比较、逻辑短路、赋值、复合赋值和半开区间；
- `unsafe` 块、`comptime`、`SourceMacro`、`intrinsic` 和 `asm`；
- `return`、`break`、`continue`、后缀 `?` 和字符串插值；
- parser 恢复用的 `Error`。

`StmtKind` 固定为 `Let`、`LetElse`、`Assign`、`Defer`、`Expr` 和 `SourceMacro`。块尾是否产生值由最后一个表达式语句的 terminator 状态记录，不通过查看源码末字节重新推断。

`PatternKind` 固定为通配、绑定、`&P` 引用模式、字面量、范围、元组、数组/切片、结构体、构造器、or、`@`、rest 和 `SourceMacro`；不存在 `ref`/`ref mut` 节点。每个绑定节点只保存名字、可变性和独立 span；绑定动作由 HIR 按[模式规范](../spec/patterns.md)生成。

`TypeKind` 固定为路径、元组、数组、切片、引用、原始指针、函数、`dyn Trait`、`impl Trait`、never、`SourceMacro`、推断占位和错误类型。泛型实参保留类型实参、comptime 实参和参数包展开的语法差异。

`SourceMacro` 节点保存脚本 body、插入上下文的 `SourceSlot`、展开预算句柄和调用点 span；它不是普通运行时表达式。解析成功并插入后，成功 HIR 不得残留 `SourceMacro` 节点。

操作符在 AST 中使用封闭枚举，不保存运算符文本。数值字面量同时保存原始 token span 和不带目标类型的任意精度整数/十进制浮点解析结果；类型相关的范围和舍入只在类型检查或 comptime 中完成。

### 解析与恢复不变量

parser 必须满足：

- 每个非 trivia token 恰好属于一个最内层 AST 节点或一个错误节点；
- 子节点 span 位于父节点 span 内，按源码顺序引用的 range 单调递增；
- 恢复只能在匹配的闭合分隔符、换行 terminator、分号或模块项起始 token 处同步；
- 一个缺失 token 只产生一个零宽合成 token，不得被多个节点重复认领；
- 解析结果不受目录枚举顺序、线程完成顺序或 hash 随机种子影响。

## `cfg` 与定义收集

`configure` 在完整解析后运行。未启用项从后续定义表中删除，但其 AST 和词法诊断仍属于解析结果；未启用项不做名称解析、类型检查、comptime 求值和代码生成。`cfg` 自身的语法、未知键和值类型仍必须诊断。

定义收集先按 package 规范路径、模块规范路径、源码起始偏移和定义种类排序，再分配 `DefId`。同一命名空间的冲突在分配后统一诊断。局部变量不进入全局定义表；它们按词法作用域分配 owner-local `LocalBindingId(u32)`。

每个定义同时得到：

- `DefPath`：package identity、模块路径、父定义路径、名字、定义种类和同名消歧序号；
- `StableDefKey`：`DefPath` 的规范编码经 BLAKE3-256 得到的 32 字节值；
- `DefId`：按 `StableDefKey` 字节序排序后的稠密编号。

若两个不同的规范 `DefPath` 得到相同 `StableDefKey`，编译器必须报告 digest collision 并停止，不能合并定义或靠源码顺序消歧。

匿名闭包、匿名 `impl Trait` 和编译器生成定义的路径分量使用其 owner 的稳定键、节点起始偏移和封闭的 lowering kind；不能使用内存地址或并行任务编号。

## HIR

### owner 与节点表示

每个具名函数、闭包、常量求值体、static 初始化体和带默认实现的 trait 项是一个 `HirOwner`。owner 保存连续的表达式、语句、模式、局部绑定和作用域 arena：

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

HIR 节点按确定性的前序遍历分配 `LocalHirId`。父子关系、词法作用域和控制流目标都使用 owner-local 稠密 ID。跨 owner 引用只使用 `DefId`；禁止把另一个 owner 的 `LocalHirId` 单独保存。

模块级 HIR 另存定义签名、泛型参数、where 约束、字段/变体、trait 项和 impl 头。函数体不会内嵌到调用者 HIR，内联只在单态化 GIR 上发生。

### 名称解析结果

每个 HIR 路径必须解析为封闭的 `Res` 枚举：

- `Def(DefId)`；
- `Local(LocalBindingId)`；
- `Primitive(PrimitiveId)`；
- `Builtin(BuiltinId)`；
- `Error`。

方法、操作符、下标、调用和关联项的最终选择不以字符串保存。类型检查后，它们分别记录选中的 `DefId`、内建操作编号或 `dyn` vtable 槽。trait 选择记录具体 impl、规范化泛型实参和选择所依赖的约束；后续阶段不得重新按名字搜索一次。

### 语法归一化

HIR 保留对诊断有价值的 `if`、`match`、循环、`try`、`async`、`select`、模式和 `defer` 结构，但消除以下纯语法差异：

| AST 形式 | HIR 表示 |
|----------|----------|
| 表达式体函数 | 与块体相同的单表达式 body |
| 复合赋值 | 单次求值 place 加显式二元操作和写回 |
| 方法调用 | 已记录接收者调整和候选集合的 `HirCall` |
| 用户类型下标 | 已选择 `Index::index` 或 `index_set` 的调用 |
| `for pattern in value` | 一次 `IntoIter::into_iter`、循环调用 `Iter::next` 和 `Option` 匹配 |
| `expr?` | `HirTryExit { operand, branch_slot, from_error_slot, target }`；槽位与结果规则只引用 [`Try` 规范](../spec/traits.md#try) |
| 字符串插值 | 按片段顺序写入 builder 的字面量片段和已选择 `Print` 调用 |
| `if let`、`while let`、let 链 | 共享被匹配临时槽的条件/模式节点 |
| 参数位置 `impl Trait` | 独立隐式类型参数 |
| 返回位置 `impl Trait` | owner 下的独立 opaque 定义 |

`async`、`select` 和 `defer` 在 HIR中保留专用节点，并各自携带由[表达式规范](../spec/expressions.md)生成的一次求值、出口和提交/cleanup计划；GIR只能消费该计划，不能按节点名重新解释随机、公平、取消或展开语义。

### 类型与调整侧表

HIR 节点本体不复制完整类型。类型检查为每个 owner 生成等长稠密侧表：

```text
TypeckResults {
    expr_types: IndexVec<ExprId, TyId>,
    pattern_types: IndexVec<PatId, TyId>,
    adjustments: IndexVec<ExprId, AdjustmentRange>,
    call_resolutions: IndexVec<ExprId, CallResolution>,
    binding_modes: IndexVec<PatId, BindingMode>,
    effects: IndexVec<ExprId, EffectSet>,
    closure_captures: IndexVec<ExprId, CaptureRange>,
    adjustment_pool: Vec<Adjustment>,
    capture_pool: Vec<Capture>,
}
```

自动解引用层数由源码类型决定，没有固定小上界。每个 `AdjustmentRange { start: u32, len: u32 }` 指向 owner 级连续 `adjustment_pool`，避免给每个表达式单独分配小 vector，同时支持任意合法的多层 `&`；range 必须 checked 位于 pool 内。调整枚举只允许自动解引用、方法接收者自动取引用、函数项/闭包擦除、`dyn` 擦除、never 到目标类型和规范允许的数值扩宽。

`EffectSet` 只有 8 个 compiler 布尔标志，使用零分配 `u32` 位掩码，位固定表示 `MAY_PANIC`、`MAY_ALLOCATE`、`MAY_SAFEPOINT`、`MAY_SUSPEND`、`READS_MEMORY`、`WRITES_MEMORY`、`FOREIGN_CALL` 和 `UNSAFE_OPERATION`。构造/合并后以 `debug_assert!(bits & !KNOWN_EFFECT_BITS == 0)` 检查上界。`FOREIGN_CALL` 同时覆盖普通 `ForeignBridge`、`ForeignBridge[DirtyCpu]` 与 `ForeignLeaf`；`call_resolutions` 对已解析的 C 导入或 native definition 额外记录 compiler-only 的 `ForeignCallMode`（`ForeignBridge`、`ForeignBridge[DirtyCpu]` 或 `ForeignLeaf`）和 leaf stack budget（字节）。普通 bridge 与 dirty bridge 设置由交接引入的 `MAY_SAFEPOINT` 与 `MAY_SUSPEND`；`ForeignLeaf` 不因外调本身设置这两个标志；无法证明模式的间接调用选择普通 `ForeignBridge`。带函数体的 `ffi(dirty_cpu)` 只允许 native-only operation，并固定记录为 dirty bridge。该集合供 GIR 构造和优化验证使用，不是用户可观察的效果类型系统。

### 捕获计划

每个闭包/async节点的 `CaptureRange` 指向 owner级连续 `capture_pool`。`Capture { root: LocalBindingId, projections: ProjectionRange, access: u8, environment_field: u32 }` 的 access位固定为 `READ`、`WRITE`、`ADDRESS_TAKEN`、`CROSSES_SUSPEND` 和 `RECURSIVE`；其它位为 0并以 `debug_assert!` 检查。

捕获分析从自由 place use收集路径。互不重叠且只读的字段/tuple投影保持独立 capture；一个写入、取地址、动态下标或相互重叠投影把共同前缀合并为同一共享槽。递归闭包另加入自引用环境槽，async把跨 suspend活跃 capture标记 `CROSSES_SUSPEND`。capture按 root绑定的 `HirId`、投影编码排序后分配 environment field，不受 hash或遍历线程影响。

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

一个 owner 只有同时满足以下条件才可以标为 `Validated` 并交给 GIR：

- 不含 `Res::Error`、错误类型或未求解类型变量；
- 所有路径、调用、操作符、关联项和 impl 已唯一选择；
- 所有 comptime 实参、数组长度、判别值和布局属性已求值；
- 每个控制流出口对应确定的作用域清理链；
- 每个读取 place 在该点确定初始化，所有 unsafe 操作位于合法边界；
- `async` 捕获、跨 suspend 活跃值和 `select` 分支载荷已经固定；
- owner 的稳定输入摘要已经计算，且不含 session-local 数字 ID。

冻结后的 HIR 与侧表不可修改。优化、资源动作展开、协程 lowering、逃逸分析和布局选择都属于后续 GIR/LIR 阶段。

## 诊断与确定性

阶段可以并行处理 owner，但最终诊断按规范路径、起始字节、结束字节、诊断代码和稳定定义键排序。相同位置的主诊断先于附注和建议。类型变量编号、hash 表遍历、线程完成顺序不能出现在用户诊断、HIR dump 或持久摘要中。

调试 dump 使用 owner 的 `DefPath`、`LocalHirId` 和规范化类型文本；默认不打印裸 `DefId`、`TyId` 或地址。dump 只用于实现检查，不进入编译 action 的可观察输出摘要。

## 与后续阶段的契约

HIR 向后续阶段只提供：冻结的定义签名、owner body、类型/调整/impl 选择、comptime 值、控制流作用域和稳定输入摘要。GIR 构造器不能读取 token 文本重新推断语义，也不能绕过 HIR 重新执行名称解析。具体控制流、资源管理、GC 与调度 intrinsic 见 [GIR 与 LIR](gir-lir.md)。

## 参考实现资料

本章借鉴 Rust 编译器的 AST/HIR owner 分层、稠密索引和 query 边界，以及 Go 编译器按 package 固定输入、在 SSA 前完成类型检查的做法；Gugu 的节点种类和 lowering 仍由自己的语言规范决定：

- [Rust 编译器开发指南：编译器总览](https://rustc-dev-guide.rust-lang.org/overview.html)
- [Rust 编译器开发指南：HIR](https://rustc-dev-guide.rust-lang.org/hir.html)
- [Go 编译器源码说明](https://go.dev/src/cmd/compile/README)
