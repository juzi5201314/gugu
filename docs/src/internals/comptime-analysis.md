# comptime 与抽象分析

本章规定官方编译器如何执行值 comptime、源码 comptime 宏以及面向未知运行时值的抽象分析。语言程序的合法性和可观察语义仍由 [`spec/`](../spec/overview.md) 规定；本章固定前端、query、HIR/GIR 优化器之间的内部边界。

## 权威边界

[编译期执行](../spec/comptime.md)规定 comptime 脚本、源码宏、解析结果、资源边界和展开预算；[程序与编译模型](../spec/program-model.md)规定闭世界输入与可达性；[AST 与 HIR](ast-hir.md)规定源码宏节点、展开轮次和 `Span`；[GIR 与 LIR](gir-lir.md)规定优化 pass 和 verifier。本章不允许实现用“更聪明的解释”改变这些语义。

## 三个执行域

编译器必须把三种不同的问题分开：

| 执行域 | 值域 | 主要结果 |
|---|---|---|
| `ConstEval` | 精确的 comptime 值 | `ConstId`、布局参数、特化输入 |
| `SourceExpand` | 脚本状态与已解析源码片段 | `ParsedSource`、`ExpansionRecord` |
| `AbstractAnalysis` | 范围、关系、别名、内存版本、效果和路径事实 | 证明事实、函数摘要、lint 输入 |

`ConstEval` 只回答“这些输入的确切结果是什么”；它不能通过执行一次具体路径证明未知运行时输入的安全性。`AbstractAnalysis` 不能伪造一个具体常量，也不能把 `unknown` 当成 `true`。类型推断仍是约束求解，trait/impl 选择仍由类型系统决定；分析结果只在这些语义选择完成后被消费。

三个域可以共享 HIR/GIR 的节点语义、源范围和调用图，但不共享可变求值状态。一个域的缓存结果不能被另一个域解释成不同的值。

## 值 comptime evaluator

### 输入与表示

`ConstEval` 接收已解析的 owner、规范化类型、已知实参、目标配置和显式编译输入。它执行 typed HIR 的语义，或者执行由 typed HIR 一次性降低出的受限 evaluator view；generic GIR 和 LIR 不能成为 comptime 语义的第二个来源。

每次求值具有不可变输入和独立状态：

```text
ConstEvalState {
    call_key: StableDefinitionKey + canonical arguments,
    locals: dense local slots,
    comptime_heap: isolated heap graph,
    dependencies: sorted input/query keys,
    fuel: remaining evaluation steps,
    memory: remaining comptime bytes,
}
```

局部槽和 comptime heap 只在本次求值中存在。值离开 evaluator 前必须归一化为规范 `ConstValue`，再由 interner 得到 `ConstId`；原始指针、宿主地址、运行时句柄、活动 resource lease 和指向 comptime heap 的引用不能进入结果。

`ConstEval` 必须使用与运行时相同的值传递、COW string、模式、`defer`、panic 和溢出语义，但只允许规范规定的确定性子集。禁止的操作在执行点产生带展开链或调用链的编译错误，不得伪造一个空值继续下游。

### 源码宏返回值

源码宏脚本的成功结果是编译器拥有的 `ParsedSource`。它只能由 `std.syntax.parse_*` 产生，不能由 Gugu 代码直接构造。`ParsedSource` 保存解析后的 fragment kind、生成文本摘要、解析上下文和本次 action 的 session-local 句柄；它不是运行时值，也不能写入常量镜像。

`SourceExpand` 接收以下返回形式：

```text
ParsedSource
Result[ParsedSource, E]
```

其中 `E` 必须实现 `std.error.Error`，且错误链和 `message()` 在同一 compiler identity 下确定。`Err` 返回到宏边界时转为编译诊断；脚本在边界内可以捕获 `SyntaxError`，把它转换成自己的 `E`，或继续尝试其它候选文本。

`parse_source` 根据当前 source slot 解析；`parse_expr`、`parse_items`、`parse_type` 和 `parse_pattern` 指定片段类别。解析 intrinsic 只产生语法树，不执行其中的函数、I/O、`unsafe` 或另一个宏。

## 源码宏展开

### 展开轮次

一个 package 的宏闭包按轮次求解：

```text
parse / lex
  → configure(cfg)
  → collect current definitions
  → resolve and type-check macro scripts
  → evaluate scripts
  → parse generated text
  → merge generated fragments
  → repeat until no SourceMacro node remains
  → final definition collection and HIR freeze
```

外层 `cfg` 为假的源码宏不执行；生成片段中的 `cfg` 在下一轮删除。每一轮只把已经冻结的定义、签名、配置和显式输入提供给宏脚本。宏不能读取同一轮由后续宏产生的新定义，也不能读取自己的半初始化结果；需要的新定义在下一轮可见。

最终 `DefId` 只能在宏闭包稳定后分配。生成定义的持久身份使用父 `ExpansionKey`、生成片段中的稳定字节偏移、名称和定义种类形成 `StableDefKey`，不能用展开顺序、线程编号或 session-local arena ID。生成 item 可以引入新的函数、类型、impl、vtable 和可达根，因此闭世界收集、`TypeId` 分配和 `type_id_count()` 冻结都必须晚于宏闭包。

宏脚本不能查询“最终是否被使用”或“最终优化后是否内联”来决定当前展开结果；这会让生成结果反过来改变查询答案。可达性和优化分析在宏闭包完成后运行。

### source slot 与名称解析

`ParsedSource` 的 fragment kind 必须适配插入位置：模块位置接受 item 列表，块位置接受语句和可选尾表达式，表达式位置接受一个表达式，类型和模式位置分别接受相应片段。片段类别不符是展开错误，不是普通类型错误。

生成文本中的自由路径按宏调用点的名称作用域解析；宏脚本自己的局部绑定不泄漏到生成源码。生成的绑定遵循生成片段的普通词法作用域，可以按正常语言规则遮蔽名称；不同展开之间的绑定身份必须不同。生成 item 遵循插入模块的可见性和冲突规则，不能靠宏展开顺序解决同名冲突。

每个生成节点保留生成文本的半开字节范围、宏调用点、宏定义点和父 `ExpansionId`。生成代码中的 `std.src.file`、`line`、`column` 使用调用点的逻辑源位置；诊断以调用点为主位置，并附加生成偏移和宏定义位置。

### 递归与预算

生成源码可以再次包含 `comptime source`。每一次新的源码宏展开都消耗以下独立预算：

- 展开树当前深度；
- 一个 action 的总展开次数；
- 生成源码总字节数；
- 生成 AST 总节点数；
- 宏脚本的 comptime fuel；
- comptime heap 字节数。

`#![comptime(expansion_limit = N)]` 为模块设置该模块展开树的深度上限；附着在源码宏位置的同名属性设置该子树上限。属性只能提出不超过 compiler profile 全局硬上限的请求；超过硬上限必须报错，不能静默截断或自动取整。其它总量预算由 compiler profile 固定，并进入 `CompilerIdentity` 或 action key。

完全相同的 `(宏定义稳定键、规范输入、source slot、配置)` 在当前展开栈再次出现时，报告确定性的 expansion cycle；带有不同已知输入的递归可以继续执行，直到任一预算耗尽。达到预算时，诊断必须列出从外层调用到当前节点的完整展开链。普通 comptime 函数递归只消耗 evaluator fuel，不增加源码宏深度；生成新的 `comptime source` 才增加展开深度。

## 抽象分析值域

### 基本事实

`AbstractAnalysis` 在显式 CFG 上为每个程序点传播抽象状态。状态至少包含：

```text
AbstractState {
    path_condition,
    integer_ranges,
    symbolic_relations,
    initialization_state,
    place_aliases,
    memory_versions,
    effect_facts,
    reachable,
}
```

整数范围至少表示有符号/无符号边界、空集合和未知；关系可以表达 `i < n`、`i + c <= n`、相等和长度关系。事实绑定到程序点、`Place` 和内存版本；不能把一个已经被写入失效的旧事实复制到新版本。

数组下标、切片、移位、容量、除法、枚举判别和 `unsafe` 前置条件都可以消费证明事实。分析器必须区分：

- `proved`：在所有符合语言语义的路径上成立；
- `disproved`：当前路径不可达或操作必然失败；
- `unknown`：信息不足、预算耗尽或分析没有收敛。

只有 `proved` 能删除运行时检查。`unknown` 必须保留原检查；它既不是错误，也不能被 lint 当成错误。

### 控制流与循环

条件分支对路径状态做分裂和收窄，合流点按确定顺序合并。`break`、`continue`、panic、return、suspend 和 unwind 只把真实可达边带到对应 successor。循环按自然循环和回边求固定点；为保证终止，抽象域使用 widening，收敛后可以用一次 narrowing 恢复精度。

最低必须支持以下事实传播：

- 常量和布尔条件；
- 数组的静态长度与切片范围；
- `Range` 的上下界；
- 循环归纳变量的初值、步长和出口条件；
- `Vec`/string/Bytes 的 `len` 与不变性；
- 初始化状态和支配关系；
- 已知纯函数或标准库函数的返回范围。

例如：

```gugu
let n = 20
let v = make_vec()
if v.len() > 10 {
    for i in 0..n {
        if i >= 2 {
            break
        }
        v[i]
    }
}
```

访问点的状态包含 `v.len() >= 11`、`i >= 0` 和 `i < 2`，于是可以证明 `i < v.len()`，必须删除该访问的边界检查。若检查与访问之间存在可能改变 `v.len()` 的写入、未知调用、未建模别名或并发修改，相关 memory version 改变，证明失效，检查必须保留。

### 别名与效果

每个可变身份对象和可写 place 都有逻辑别名类及 memory version。已知写入只使受影响字段和派生事实失效；未知调用、FFI、并发边界或逃逸引用使保守的对象集合失效。COW string 的 backing seal、resource publish、集合迭代快照和 GC 写入也必须作为 effect 进入状态，而不能仅按语法名字判断。

标准库、runtime 和跨 package Gugu 函数通过稳定的 `FunctionSummary` 提供可消费的契约：

```text
FunctionSummary {
    preconditions,
    return_ranges,
    return_relations,
    read_places,
    write_places,
    alias_effects,
    may_allocate,
    may_panic,
    may_suspend,
    may_call_unknown,
}
```

摘要不能声明比函数真实语义更强的前置条件或后置事实。没有摘要的普通函数按其已知 GIR effect 分析；没有可证明上界的外部函数按未知写入、可能 panic 和可能阻断处理。

## 全程序求解

闭世界的单态化实例图闭合后，编译器必须尽可能对所有可达 Gugu 函数、标准库和 runtime body 计算摘要，允许跨模块和跨 package 复用。工作流程为：

1. 按稳定 `MonoKey` 排序建立可达实例图；
2. 将函数按调用图 SCC 分组；
3. 对每个 SCC 用确定的初始摘要迭代求解；
4. 在递归回边应用 widening，直到摘要不再变化或达到分析预算；
5. 把稳定摘要写入 query cache，并让 caller 的局部分析消费它；
6. 对无法收敛的部分返回 `unknown`，不删除安全检查。

摘要和证明事实按 `MonoKey`、目标、feature/cfg、runtime/标准库版本和分析策略版本缓存。跨 package 的摘要只在导出定义、ABI、effect 和相关源码输入摘要未变化时复用。`ForeignLeaf`、`ForeignBridge`、内联汇编和未登记的 C 回调不能被假定为纯函数；除非有显式且可验证的 ABI 契约，否则按未知处理。

分析尽力覆盖全程序，但不承诺解决所有人类可读出的关系。健全性优先于完整性：求解超时、内存不足、循环不收敛或 alias 不明都只能降低优化机会，不能改变合法程序的结果。

## 优化、类型与 lint 的边界

`AbstractAnalysis` 不替代类型检查：类型推断是静态约束求解，泛型参数和 impl 选择不能从一次运行时抽象执行中“观察”出来。它可以消费已确定的 `ConstId`，也可以为优化提供 `T` 已具体化后的布局事实。

GIR 优化器消费 `proved` 事实，执行边界检查消除、不可达块删除、常量分支折叠、无效检查消除、内联成本评估和逃逸/放置决策。任何删除都必须由 verifier 检查其前置证明仍支配该操作；写入、调用、suspend、panic cleanup 或 CFG 重写使证明失效时必须重新分析或恢复操作。

lint 只消费分析结果，不改变类型、控制流语义或运行时错误行为。可由该分析驱动的 lint 包括恒真/恒假条件、不可达代码、必然失败操作、永远无法满足的模式和冗余边界检查；具体 lint 名称只有在词法/诊断规范登记后才成为公开兼容面。函数是否被使用必须从宏展开后的闭世界根、导出、`used`、vtable、`type_id`、C 回调和测试入口计算，不能用文本搜索替代可达性图。

## Query 与 verifier

源码宏和抽象分析必须是独立 query，不能通过可变全局状态把结果塞进 HIR 或 GIR。官方 query 至少包含：

```text
ParseSource(source_fingerprint, source_slot)
ExpandSourceMacro(call_key, round, source_slot)
FunctionAnalysisSummary(mono_key, analysis_policy)
WholeProgramAnalysis(world_key, analysis_policy)
```

`ParseSource` 的结果是已验证的 parser fragment 及其 source hash；`ExpandSourceMacro` 的结果包括生成文本/解析结果摘要、`ExpansionRecord`、直接输入和诊断；`FunctionAnalysisSummary` 的结果包括稳定摘要和证明版本；`WholeProgramAnalysis` 的结果包括排序后的可达图、SCC 摘要和可消费事实。所有结果通过已有 query 状态机和 cycle/fixpoint 规则生成，不返回半初始化对象。

每个 GIR 改写 pass 必须在调试构建运行局部 verifier；跨阶段边界运行完整 verifier。verifier 至少检查：

- `proved` 下标事实支配被删除的边界检查；
- 事实引用的 memory version 在中间没有被可能写入的操作失效；
- 摘要的调用效果覆盖 callee 的真实操作；
- 生成 AST 没有遗留 `SourceMacro`、错误节点或未解析定义；
- 成功 GIR/LIR 没有未求值 comptime 值、悬空 arena ID 或未知语义占位。

分析失败不产生成功的“猜测摘要”。失败 query 只能缓存本 session 的错误或明确的 unknown 结果；unknown 的优化结果必须与保留全部运行时检查的结果语义等价。

## 确定性与资源

宏执行、摘要求解和局部分析可以并行，但输入遍历、SCC 顺序、事实合并、诊断排序和缓存编码必须确定。线程编号、完成顺序、hash table 遍历顺序、宿主地址和临时文件名不得进入结果。

源码宏的生成预算与抽象分析预算分别计账。分析器可以使用时间、内存、迭代和摘要大小预算；达到上限时返回 `unknown` 并记录可诊断的分析备注，不把资源不足伪装成用户程序错误。语言语义只依赖健全性，不依赖某台机器恰好完成了更多分析。
