# comptime 与抽象分析

本章规定官方编译器如何执行值 comptime、源码 comptime 宏以及面向未知运行时值的抽象分析。语言程序的合法性和可观察语义仍由 [`spec/`](../spec/overview.md) 规定；本章固定前端、query、HIR/GIR 优化器之间的内部边界。

## 权威边界

[编译期执行](../spec/comptime.md)规定 comptime 脚本、源码宏、解析结果、资源边界和展开预算；[程序与编译模型](../spec/program-model.md)规定闭世界输入与可达性；[AST 与 HIR](ast-hir.md)规定源码宏节点、展开轮次和 `Span`；[GIR 与 LIR](gir-lir.md)规定优化 pass 和 verifier。本章不允许实现用“更聪明的解释”改变这些语义。

## 四个执行域

编译器必须把四种不同的问题分开：

| 执行域 | 值域 | 主要结果 |
|---|---|---|
| `EarlyConst` | 精确的早期 comptime 值 | `ConstId`、布局参数、特化输入 |
| `SourceExpand` | 脚本状态与已解析源码片段 | `ParsedSource`、`ExpansionRecord` |
| `LateConst` | 冻结 type universe 上的受限精确值 | late 标量与 `TypeId` 重定位值 |
| `AbstractAnalysis` | 范围、关系、别名、内存版本、效果和路径事实 | 证明事实、函数摘要、lint 输入 |

`EarlyConst` 和 `LateConst` 只回答“这些输入的确切结果是什么”；它们不能通过执行一次具体
路径证明未知运行时输入的安全性。`LateConst` 只能读取已经冻结的 type universe，不能
形成类型、展开源码或改变可达图。`AbstractAnalysis` 不能伪造具体常量，也不能把
`unknown` 当成 `true`。类型推断仍是约束求解，trait/impl 选择仍由类型系统决定；分析
结果只在这些语义选择完成后被消费。

四个域可以共享 HIR/GIR 的节点语义、源范围和调用图，但不共享可变求值状态。一个域的
缓存结果不能被另一个域解释成不同的值。

## comptime evaluator

### 输入与表示

`EarlyConst` 接收已解析的 owner、规范化类型、已知实参、目标配置和显式编译输入；
`LateConst` 另接收冻结后的 `TypeUniverseKey`，且只求值前端已登记的 late 常量闭包。两者
执行 typed HIR 的语义，或者执行由 typed HIR 一次性降低出的受限 evaluator view；
generic GIR 和 LIR 不能成为 comptime 语义的第二个来源。

每次求值具有不可变输入和独立状态：

```text
ConstEvalState {
    domain: EarlyConst | SourceExpand | LateConst,
    call_key: StableDefinitionKey + canonical arguments,
    type_universe: None | TypeUniverseKey,
    locals: dense local slots,
    comptime_heap: isolated heap graph,
    dependencies: sorted input/query keys,
    fuel: remaining evaluation steps,
    memory: remaining comptime bytes,
}
```

局部槽和 comptime heap 只在本次求值中存在。值离开 evaluator 前必须归一化为规范
`ConstValue`；早期结果由 interner 得到 `ConstId`，late 结果写入以稳定 `LateConstKey` 索引的
不可变结果表。原始指针、宿主地址、运行时句柄、活动 resource lease 和指向 comptime heap
的引用不能进入结果。

所有 comptime 域必须使用与运行时相同的值传递、COW string、模式、`defer`、panic 和
溢出语义，但只允许规范规定的确定性子集。禁止的操作在执行点产生带展开链或调用链的
编译错误，不得伪造空值继续下游。

早期与源码宏 evaluator 的 revision 为 **3**。函数及常量初始化各自隔离词法帧，
复用统一字面量解码；控制流出口通过结构化结果逐层传播，不使用可被父表达式覆盖的
可变退出标志。确定性 heap 账本为聚合/装箱槽固定计 64 字节，并递归累计其动态负载；
复制、repeat、拼接与插值均在分配或增长前记账。

### capability registry

compiler-owned registry 以解析后的 `StableDefKey`、lang item 或 intrinsic ID 为键，每个条目
固定保存 `{ allowed_domains, effects, explicit_inputs, result_kind, evaluator_revision }`。
`allowed_domains` 是三个 comptime 域的位掩码；`effects` 至少区分纯计算、evaluator heap
分配、显式文件输入、源码解析和初始同步位构造。固定能力组以[标准库](../spec/standard-library.md#comptime-capability-registry)
为准，内部 registry 不能扩大公开集合。

类型检查在建立 comptime call graph 时验证每条边的执行域；evaluator 调用时再次以稳定 ID
检查同一条目，防止缓存或错误恢复绕过权限。用户函数没有 registry 条目，而是继承调用点
的执行域并传递检查全部静态 callee。无法静态封闭的间接调用不允许进入 late comptime；
其它域中的间接调用只有候选集合全部满足同域 capability 时才合法。

registry 的规范摘要、条目 evaluator revision 和验证器 revision 都进入
`CompilerIdentity`。标准库登记函数 body 与条目效果不符属于 compiler/std 构建错误；用户
程序命中未登记能力或错误执行域属于 `comptime-capability` 编译错误。

### late comptime 闭包

前端为每个直接或传递依赖 `type_id_count()`、comptime `TypeId.as_int()` 或编号序关系的
表达式建立 `LateConstKey`。该键包含稳定 owner、表达式节点稳定位置、规范类型、早期参数和
静态 callee 闭包摘要。闭包中的所有签名、impl、类型和调用目标必须在单态化前确定，并作为
普通可达性输入；late evaluator 不能新增图边。

单态化实例图和具体类型集合闭合后，`FreezeTypeUniverse` 按 `StableTypeKey` 排序分配
`TypeId`，得到 `TypeUniverseKey`。`EvaluateLateComptime` 只读取该 key、早期常量、已冻结
布局和已登记 capability，结果只能包含规范允许的标量叶值。结果通过 late 常量表供 GIR
重定位、运行时常量初始化和分支操作数消费；不得修改冻结 HIR/GIR，也不得触发新的前端或
单态化 query。

阶段 25 的 late 求值由 `frontend::late` 执行 `FreezeTypeUniverse`（25，schema 1）和
`EvaluateLateComptime`（26，schema 1）。阶段 26 起该步骤发生在 `LowerHir` 冻结之后、
`mono::close` 与全程序分析之间，不再嵌在 `LowerHir` compute 内。
`InstantiateGir` 和实例 world 的 schema 为 3，携带每实例具体类型记录与
HIR 类型到稳定类型键的绑定；名称、布局、递归字段依赖和 vtable payload 类型
在实例闭合时收集。opaque 先还原隐藏类型，透明别名复用原类型键。

类型记录按完整 32 字节 `StableTypeKey` 摘要排序，下标即 TypeId；同时保留
规范编码以检查摘要冲突。`!` 与 `MaybeUninit[T]` 不占编号，后者内部类型仍进入
依赖闭包。unsized 切片保留类型身份但没有固定布局，不能伪造固定大小 descriptor。
vtable 记录保存接口稳定键与已检查的具体类型编号；物理 GC section 在阶段 39 写出。

`LateKey` 保存实例摘要、owner 内表达式编号、结果类型键和完整静态求值闭包摘要。
显式 comptime 与依赖 late 的初始化器消费结果表；`type_id[T]` 和计数 intrinsic
形成可供后续 GIR 直接消费的重定位/常量条目。evaluator 只读 HIR、实例调用目标、
具体类型绑定和 universe，不持有 `Model` 或 query engine。调用闭包在执行前遍历
全部分支，拒绝未封闭的间接调用与外部函数；循环、调用和聚合分配受 fuel、深度与
heap 预算约束。结果 verifier 拒绝非固定形状标量聚合以及其它 universe 的类型引用。

早期 evaluator 保留符号 `TypeId` 的相等性和名称，数值编号与序关系只能晚求值。
阶段依赖沿 const/static、调用与表达式传播，早期使用点报 `E0054`。冻结类型表和
late 表的指纹共同进入全程序分析输入、action key 与镜像计划；缓存命中仍验证
schema、身份、排序、布局、引用与结果形状，不重新开启语义形成或可达性收集。

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

外层 `cfg` 为假的源码宏不执行，包括表达式、类型与模式 slot；复用脚本根表达式的 cfg 活动位判断，不建立第二份活动位图。生成片段中的 `cfg` 在拼接当轮删除。每一轮只把已经冻结的定义、签名、配置和显式输入提供给宏脚本。宏不能读取同一轮由后续宏产生的新定义，也不能读取自己的半初始化结果；需要的新定义在下一轮可见。

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
生成文件区域同时保存继承的子树深度上限；片段内层属性在注册 token 文件区域后校验，
其上限与祖先限制取交集，后续轮次读取该有效上限。

当前实现的 profile 默认值：深度上限 16（全局硬上限 256）、总展开次数 4096、生成字节 4 MiB、生成 AST 节点 1M、宏脚本 fuel 总池 10M、宏脚本 heap 总池 16 MiB。预算规范编码与全部生成文本摘要经 `ActionInputs` 的 `macro_budget`/`macro_inputs` 进入前端 action key。

完全相同的 `(宏定义稳定键、规范输入、source slot、配置)` 在当前展开栈再次出现时，报告确定性的 expansion cycle（`expansion-cycle`，`E0048`，以宏脚本文本与 source slot 为稳定键）；带有不同已知输入的递归可以继续执行，直到任一预算耗尽（`expansion-limit`，`E0049`）。达到预算时，诊断必须列出从外层调用到当前节点的完整展开链。普通 comptime 函数递归只消耗 evaluator fuel，不增加源码宏深度；生成新的 `comptime source` 才增加展开深度。

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

标准库、runtime 和跨 package Gugu 函数通过健全的 `FunctionSummary` 提供可消费事实：

```text
FunctionSummary {
    preconditions,
    return_ranges,
    return_relations,
    read_places,
    write_places,
    alias_effects,
    reads_hidden_state,
    writes_hidden_state,
    may_allocate,
    may_panic,
    may_suspend,
    may_call_unknown,
}
```

摘要不能声明比函数真实语义更强的前置条件或后置事实。调用点只有证明某组
`preconditions` 后才能消费对应返回事实；无法证明时仍应用无条件 effect，并把条件事实
降为 unknown。没有摘要的普通函数按其已知 GIR effect 分析；没有可证明上界的外部函数
按未知写入、可能 panic 和可能阻断处理。

### 跨 package 公共摘要

跨 package seam 使用 compiler-owned 的 `PublicFunctionSummaryV1`。它是稳定、内容寻址的
编译产物，不是 Gugu 用户 API、package 发布格式或跨 schema 的 ABI。payload 使用本编译器
缓存的规范编码，并固定包含：

```text
PublicFunctionSummaryV1 {
    schema_revision,
    analysis_semantics_revision,
    public_policy_revision,
    target_semantics,
    mono_key,
    signature_and_abi_fingerprint,
    unconditional_effects,
    conditional_facts,
    interface_places,
}
```

`interface_places` 只能引用参数序号、返回值、公开 static 的 `StableDefKey`，以及由稳定字段
键或 tuple/array 下标组成的投影。`conditional_facts` 也只能以这些 interface place 为
输入和输出，不能把 private type、局部槽或内部控制流标签暴露为可消费事实。局部槽、private
static、arena ID、裸地址和 session-local 编号不得跨 package；对私有状态的访问折叠为
`reads_hidden_state` / `writes_hidden_state`。序列按规范编码 byte 序排序，位集合必须拒绝未知
bit，范围与关系必须通过摘要 verifier。

公共对象只能由 `PublicSummaryPolicyV1` 产生；该策略随 compiler identity 固定抽象域、
widening、迭代次数和摘要大小上限，不使用 wall-clock deadline。生成 action 以 monomorphic
GIR 语义指纹、签名/ABI、选定 impl、目标与 cfg/feature、public policy、同一 SCC 的成员及
全部直接 callee 公共摘要键为输入。生产 provenance 保存在 action record，不写进公共
payload；公共对象 key 只由上面的可消费语义内容产生。因此 body、源码排版或 callee 实现
变化会重算生产 action，但若重新证明出的公共摘要完全相同，就得到同一对象 key，依赖
package 的分析保持 green。签名、ABI、effect 或可消费事实变化必然改变对象 key。

递归和互递归函数按 SCC 共同求固定点；`AnalysisSccSummary` 一次产生该 SCC 的全部内部
摘要，随后才逐 `MonoKey` 投影公共对象，禁止成员通过半初始化公共摘要互相求值。跨 package
SCC 使用相同规则，不按 package 人为切断调用环。

公共摘要只由兼容 `CompilerIdentity` 的本地 compiler action 产生。package 归档或 build.gg
不能提供权威摘要。对象缺失、损坏、schema/target/analysis revision 不匹配或 verifier 失败
时，编译器必须从当前闭世界可见的 GIR 重算；只有没有 Gugu body 的外部函数才能退化为
保守 unknown。缓存故障不能改变优化结果之外的程序语义。

## 全程序求解

闭世界的单态化实例图闭合后，编译器必须尽可能对所有可达 Gugu 函数、标准库和 runtime
body 计算摘要，允许跨模块和跨 package 复用。工作流程为：

1. 按稳定 `MonoKey` 排序建立可达实例图；
2. 将函数按调用图 SCC 分组；
3. 对每个 SCC 用确定的初始摘要迭代求解；
4. 在递归回边应用 widening，直到摘要不再变化或达到分析预算；
5. 验证 SCC 摘要并投影 `PublicFunctionSummaryV1`；
6. caller 只依赖 callee 的公共对象 key，局部证明另留在当前 world；
7. 对无法收敛的部分返回 `unknown`，不删除安全检查。

## 阶段 26 实现桥接

当前 compiler 在 **冻结后的 generic GIR body** 上运行分析：每个 callable owner 使用
`BuildGenericGir` 已验证的显式 CFG，在程序点传播 `AbstractState`（可达、区间、稀疏差约束、
初始化、别名类、memory version、效果）。抽象槽仍按 HIR local/表达式编号索引，GIR 临时值
经 `expression_locals` 与 `hir_local` 映射。循环 header 回边 widening，固定点后以重新合并
全部前驱的同步迭代执行 narrowing；每轮保持健全后固定点，精度迭代受同一块预算约束。

管线顺序为 `LowerHir`（schema **5**，只构造、校验并冻结）→ `BuildGenericGir` →
`mono::close` → `late::run` → `gir::attach_fragments` → `WholeProgramAnalysis` →
`EscapeAndPlacement` → `PublicFunctionSummary`。分析输入使用放置前的 generic GIR 指纹。
`WholeProgramAnalysis`（query schema **5**）输入指纹含冻结 HIR
指纹、generic GIR 指纹、late/mono 图指纹与策略字节；其内再嵌套 `AnalysisSccSummary`
（27，schema **4**）与 `FunctionAnalysisSummary`（23，schema **4**）。身份键为 `MonoKey`
（见[单态化与编译缓存](monomorphization-cache.md#阶段-24-实现桥接)）。
SCC 按实例图的凝聚顺序求解，每个实例具有独立固定点状态；解释器共享 owner 的 generic
GIR，但调用点消费该实例实际选中的 callee 摘要。`InstantiateGir` 仍从 HIR 收集调用边，
不从 GIR 重解析。运算符、迭代、try、格式化与局部 static 初始化器的调用效果均进入分析。
函数 query 只投影已完成 SCC，不通过 query 读取半初始化成员。共享 HIR 检查必须在所有
可达实例中都得到相同安全证明才可省略。证明只写入 `AnalysisWorldV1.proofs`，不回写
`RuntimeCheck`，不回看 `CheckedSemantics` 侧表，也不参与类型推断或 impl 选择。

类型检查期写在调用表达式上的 `PANIC`/`ALLOCATE`/`SUSPEND` 是上界；闭合后由选中实例
摘要取代，写回调用目的地不再重放该上界。未命中实例的普通调用才回退保守摘要。

`[T; N]` 与 `&[T]` 的固有 `len` 由类型检查在用户 impl 之前命中，HIR 降为
`Builtin::Len`；数组长度为类型中的 `N`，切片长度进入 `Len` 值槽，可被 `v.len() > 10`
一类比较收窄。入口按局部整数类型播种位宽边界（例如 `u8` 移位量下界 ≥ 0）；
调用 unwind 边只在当前状态已可能 panic 时传播，不把 CFG 上的 cleanup 边当成必然 panic。

固定 **`analysis_semantics_revision = 5`**、**公共摘要策略 revision = 2**（与公共摘要对象复用同一版本常量）
（默认 SCC 迭代 32、块迭代 256）。SCC 轮次或块迭代超预算 → 摘要取保守值、
`budget_exhausted = true`，检查保持 `Unknown`；**不是**用户 `Error`。

证明读检查表达式所在程序点、当前 memory version 上的状态：

- 数组 / 切片下标：`0 <= i < N` 或 `0 <= i < Len(base)` 才 `Proved`；字面量越界仍
  `Disproved`。切片 `Bounds { slice: true }` 要求 `0 <= start <= end <= Len`，允许长度处的空切片。
- 除法：除数区间不含 0 → `Proved`；字面量 0 → `Disproved`；否则 `Unknown`。
- 移位：移位量区间下界 ≥ 0 → `Proved`；负字面量 → `Disproved`。
- Unicode 标量：值区间完全落在合法 scalar 且不含 surrogate → `Proved`。
- 浮点转整数与 Utf8Boundary：可保持 `Unknown`。
- 只有支配该检查点的 `Proved` 写入 `AnalysisWorldV1.proofs`；**不删除** HIR 检查节点
  （物理消除仍是阶段 29）。

复合赋值按操作符更新原值，范围越过整数位宽时取该类型的保守范围，不能保留未回绕的
数学结果。循环再次执行表达式时清除该表达式上次求值的等式与范围。引用写入、未知调用、
挂起和隐藏状态写入使标量与长度事实一同失效。memory version 用入口版本与未知写入版本
表示，循环写入不能使版本号无限递增；narrowing 使用全部前驱重新构建输入而非对旧上界做 join。

未知调用、别名、并发、FFI、asm、spawn、COW seal、resource publish、长度可能被改写的
路径一律保留检查。跨函数 `FunctionSummary` 含规范字段（返回区间/关系、读写 place、
别名效果、hidden state、`may_allocate` / `may_panic` / `may_suspend` / `may_call_unknown`）
以及长度失效用的 `may_mutate_len`；首轮实际分析建立返回范围，后续迭代合并效果与返回上界。
所有可达返回出口的区间共同形成摘要；调用点消费已选实例的范围。未改写参数的直接 `len`
返回可形成 `EqLen`，纯调用将该关系映射到实参长度；写入或并发边界后不得恢复旧长度事实。
`Builtin::Len` 是纯函数。直接函数项调用按 `Resolved(Def)` 进入调用图与摘要查找，
不把已知 callee 当成 unknown。`PublicFunctionSummary`（28，schema 2）自阶段 24 起从已完成实例 SCC 投影
`PublicFunctionSummaryV1`：公共函数实例产生内容寻址摘要对象，经
`ActionInputs::add_public_summary` 进入前端 action key；私有函数不产生公共对象。
interface place 只引用参数序号、返回值与公开 static 稳定键投影，私有状态折叠为
hidden-state 标志；对象 key 只由可消费语义内容产生。磁盘 object 持久化与跨
package 消费由阶段 71 接入。

证明在冻结 HIR 与 generic GIR 就绪后写入 `AnalysisWorldV1.proofs`，不回写 HIR。
单态化闭合与公共摘要投影在冻结之后执行（`mono::close` → `late::run` →
`analysis::run_world` → `summary::project`）。world 的输入指纹取冻结 HIR、generic GIR
与实例图/late 指纹，不混合证明输出。后端 `ImagePlan.runtime_checks_elided_count`
统计 `Proved` 数量、`mono_instance_count`/`mono_root_count`/`mono_graph_fingerprint`
暴露闭世界实例图，供 smoke；**不改变**语言语义。`ActionInputs` 的 `macro_budget`、
`analysis_policy`、`analysis_world` 指纹与全部公共摘要对象键进入前端 action key。


局部证明按 `MonoKey`、闭世界、目标、feature/cfg、runtime/标准库版本和分析策略缓存；公共
摘要还按独立 schema、analysis semantics revision 和 `PublicSummaryPolicyV1` 版本化。
`ForeignLeaf`、`ForeignBridge`、内联汇编和未登记的 C 回调不能被假定为纯函数；除非有显式
且可验证的 ABI 契约，否则按 unknown 处理。

分析尽力覆盖全程序，但不承诺解决所有人类可读出的关系。健全性优先于完整性：求解超时、
内存不足、循环不收敛或 alias 不明都只能降低优化机会，不能改变合法程序的结果。

## 优化、类型与 lint 的边界

`AbstractAnalysis` 不替代类型检查：类型推断是静态约束求解，泛型参数和 impl 选择不能从一次运行时抽象执行中“观察”出来。它可以消费已确定的 `ConstId`，也可以为优化提供 `T` 已具体化后的布局事实。

GIR 优化器消费 `proved` 事实，执行边界检查消除、不可达块删除、常量分支折叠、无效检查消除、内联成本评估和逃逸/放置决策。任何删除都必须由 verifier 检查其前置证明仍支配该操作；写入、调用、suspend、panic cleanup 或 CFG 重写使证明失效时必须重新分析或恢复操作。

lint 只消费分析结果，不改变类型、控制流语义或运行时错误行为。可由该分析驱动的 lint 包括恒真/恒假条件、不可达代码、必然失败操作、永远无法满足的模式和冗余边界检查；具体 lint 名称只有在词法/诊断规范登记后才成为公开兼容面。函数是否被使用必须从宏展开后的闭世界根、导出、`used`、vtable、`type_id`、C 回调和测试入口计算，不能用文本搜索替代可达性图。

## Query 与 verifier

comptime、源码宏和抽象分析必须是独立 query，不能通过可变全局状态把结果塞进 HIR 或
GIR。官方 query 至少包含：

```text
ParseSource(source_fingerprint, source_slot)
ExpandSourceMacro(call_key, round, source_slot)
AnalysisSccSummary(scc_key, analysis_policy)
FunctionAnalysisSummary(mono_key, analysis_policy)
PublicFunctionSummary(mono_key, analysis_semantics_revision, public_policy_revision)
WholeProgramAnalysis(world_key, analysis_policy)
```

`FreezeTypeUniverse` 产生排序后的具体类型集合、稠密编号与 `TypeUniverseKey`；
`EvaluateLateComptime` 产生不可变 late 标量。`AnalysisSccSummary` 独占摘要固定点求解，
`FunctionAnalysisSummary` 只是从已完成 SCC 结果投影单函数内部摘要；
`PublicFunctionSummary` 验证并擦除非 interface place 后，产生内容寻址公共对象。
`WholeProgramAnalysis` 包含排序后的可达图、SCC 摘要、公共摘要键和 world-local 证明事实。
所有结果通过已有 query 状态机和 cycle/fixpoint 规则生成，不返回半初始化对象。

阶段 22 起两个源码宏 query 在前端注册：

- `ParseSource`（编号 21，schema 1）的 key 是 `(source slot 字节, 生成文本 BLAKE3)`；计算闭包
  用主 lexer/parser 对文本做一次性闸门解析，成功返回空结果，失败返回首个语法错误的
  消息与字节偏移。诊断只经 payload 传递（结构化 `SyntaxError`），不进入持久缓存。
- `ExpandSourceMacro`（编号 22，schema 2）的 key 包含 source slot、轮次、调用模块和逻辑位置、
  脚本文本、当轮所有源码内容摘要、名称指纹、cfg 规范串与 registry 摘要。
  计算闭包在 SourceExpand 域执行脚本，结果保存 `(source slot, 生成文本, fuel 用量, heap 用量)`；
  求值失败诊断经 `store_errors/restore_errors` 缓存并重绑定当前源码表。
  action 的总预算在 query 外扣除，冷热命中一致；总量超限不写入宏求值缓存。

拼接阶段（注册生成快照与展开记录、把片段解析进宿主 arena、执行片段 cfg 与列表
手术）发生在 query 之外，由轮次驱动器在宿主模块上完成；同一份生成文本在解析闸门
与拼接各解析一次，两次都使用主 lexer/parser，结果由确定性保证一致。

阶段 23 起分析 query 在前端注册；阶段 24 起身份键升级为 `MonoKey`：

- `AnalysisSccSummary`（编号 27，schema 4）的 key 是排序后的 `MonoKey` 集、
  analysis policy 与 world 输入指纹；计算闭包内对具体实例做摘要固定点，
  不经 query 读取本 SCC 的半初始化结果。
- `FunctionAnalysisSummary`（编号 23，schema 4）从已完成的 SCC 摘要投影单个实例。
- `WholeProgramAnalysis`（编号 24，schema 5）按实例图凝聚拓扑请求上述嵌套 query，
  合并为 world-local 共同证明与按实例排序的摘要。
- `PublicFunctionSummary`（编号 28，schema 2）以已完成 world 的结果指纹隔离生产者输入，
  投影不含私有定义的效果与参数序号；内容对象键独立于 body 和稠密定义编号，并进入前端 action key。

每个 GIR 改写 pass 必须在调试构建运行局部 verifier；跨阶段边界运行完整 verifier。verifier
至少检查：

- late comptime 闭包只读取冻结 type universe，且没有形成新类型、调用边或源码；
- `proved` 下标事实支配被删除的边界检查；
- 事实引用的 memory version 在中间没有被可能写入的操作失效；
- 摘要的调用效果覆盖 callee 的真实操作；
- 公共摘要只含可表示的 interface place，且条件事实不能在未证明前置条件时使用；
- 生成 AST 没有遗留 `SourceMacro`、错误节点或未解析定义；
- 成功 GIR/LIR 没有未物化的早期 comptime 值、悬空 arena ID 或未知语义占位；GIR 中合法的
  `LateConstRef` 必须指向当前冻结 type universe 的成功结果，进入 LIR 前必须物化。

分析失败不产生成功的“猜测摘要”。失败 query 只能缓存本 session 的错误或明确的 unknown
结果；unknown 的优化结果必须与保留全部运行时检查的结果语义等价。

## 确定性与资源

宏执行、摘要求解和局部分析可以并行，但输入遍历、SCC 顺序、事实合并、诊断排序和缓存编码必须确定。线程编号、完成顺序、hash table 遍历顺序、宿主地址和临时文件名不得进入结果。

源码宏的生成预算与抽象分析预算分别计账。分析器可以使用时间、内存、迭代和摘要大小预算；达到上限时返回 `unknown` 并记录可诊断的分析备注，不把资源不足伪装成用户程序错误。语言语义只依赖健全性，不依赖某台机器恰好完成了更多分析。
