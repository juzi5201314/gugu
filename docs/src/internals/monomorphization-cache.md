# 单态化与编译缓存

本章规定编译器内部的 demand-driven query 图、单态化实例身份和内容寻址编译缓存。外部可观察的缓存输入集合、目录覆盖和清理语义见[包、依赖与构建模型](../spec/packages-builds.md#cache-and-target-view)；本章固定当前编译器如何实现这些要求，但不把磁盘格式承诺给第三方工具。

## 权威边界

[程序模型](../spec/program-model.md)、[包与构建](../spec/packages-builds.md)和[工具链](../spec/toolchain-cli.md)唯一规定闭世界集合、构建输入、离线/锁定行为、缓存目录覆盖与命令结果。本章只规定官方编译器的 query依赖、实例身份和私有磁盘编码；第三方工具不能解析这些文件取得新的 package、ABI或语言契约。

## 总体模型

编译采用两个互补层次：

1. 前端和语义分析使用 Rust 风格的 query DAG，按依赖指纹判断既有结果能否复用；
2. 单态化 GIR、LIR、机器码片段、runtime metadata 和最终镜像使用 Go build cache 风格的内容寻址 object/action cache。

所有缓存命中都必须等价于重新执行对应 query。缓存不是正确性的来源；丢失、损坏、版本不兼容或并发发布冲突只能导致重新计算，不能改变语义或选择另一份输入。

## 哈希与规范编码

内部缓存统一使用 BLAKE3-256。package 发布 checksum 的 SHA-256 不复用为编译缓存摘要；两者具有不同域和生命周期。

每次哈希都先写入 ASCII 域标签和一个零字节，再写规范编码：

```text
BLAKE3-256(domain || 0x00 || canonical_bytes)
```

域标签固定包括 `gugu-input-v1`、`gugu-query-key-v1`、`gugu-query-result-v1`、`gugu-mono-v1`、`gugu-analysis-summary-v1`、`gugu-object-v1` 和 `gugu-action-v1`。不同域的相同 payload 不能得到可互换身份。

## GUGURT01 runtime artifact

runtime/std 源在 compiler build-time 由 `gugu-runtime-builder` 固化为 target 专属 `GUGURT01` 容器。容器包含 28 字节头、GBC1 manifest、GBC1 payload 与 `BLAKE3-256("gugu-runtime-artifact-v1" || 0x00 || payload)`；头部保存 schema 3 与 manifest/payload 的 `u64` 长度，schema 1 的 JSON 容器直接拒绝。manifest 的 target descriptor、source content digest、compiler identity、revision、analysis policy 和 target feature digest 均参与身份。payload 在 source blob 之后带可选的 `prebuilt` 段（schema 1）：内建块的名称解析、身份、早期常量、类型检查、冻结 HIR、泛型 GIR 与 record 类型目录各是一段前端 canonical 编码字节，随表带 `(标识域摘要, 内容摘要)`。内部 query key 同时纳入 GBC1 revision，旧 JSON payload 不作为新版本的命中。

`gugu-runtime-builder` 只提供 source manifest 与接口摘要，不执行语义类型求解；`prebuilt` 段由 façade 的 build script 调用 `gugu_frontend::materialize` 派生并写入，contract/template/function 表仍为空。物化只覆盖与用户无关的层：编号前缀按 source manifest 顺序固定，跨块引用只走稳定键，因此内建块下标的含义在用户编译里仍然有效。物化在构建期拒绝非 target 的 cfg 谓词：内建块必须对用户 cfg 保持不变。compiler façade 核对请求目标，并显式拒绝源码 artifact 中未物化的 executable function，不能选出后丢弃，也不能把它冒充用户 HIR owner。用户编译从固定 source blob 形成独立的 `RecordTypes` 目录，复用正常类型布局求解器；它保存冻结参数类型和 size/align/field-offset 证据，不创建 executable instance、GIR/LIR body 或机器片段。七个 runtime layout verifier 在 query 构造与恢复时完整逐字段验证该目录，目录完整二进制身份进入 `RuntimeRawModel` key。fresh query 保留刚构造的类型表、late 值、GIR/LIR 与机器片段对象，不立即解码自身 payload；缓存命中仍解码并验证身份与结构。

加载端由 `gugu_frontend::prefix::BuiltinPrefix` 安装 `prebuilt`，它按层拆成两段：`BuiltinPrefix` 先核对容器 schema 与各派生层 revision，再交给 sema 层的 `SemaPrefix` 解码 `names`/`identities`/`hir.module`/`early.constants`/`checked.semantics`，最后自己解码 GIR 层的 `record.types` 与 `gir.bodies`。名称解析、宏展开、类型检查、comptime 与 HIR 降级只接收 `SemaPrefix`，不感知 GIR 层表；`semantics::check` 只产出 sema 层结果（`SemaWorld`），冻结之后的 `gir → mono → late → analysis → placement → summary` 编排由 `frontend::pipeline::derive` 承接，GIR 层表从 `BuiltinPrefix` 注入。每张表先重算表内容摘要与表内摘要比对，然后校验表结构（定义编号稠密、稳定键唯一、位置落在内建前缀内），最后把表里的源码位置按当前 `SourceMap` 重建为会话局部 `Span`——持久形式只保存文件下标、字节区间与展开下标；源码表身份按内容派生并在重建时绑定当前 `SourceMap`，会话局部编号不会进入持久摘要。物化模块与物化源单元占据文件 ID 与模块表前缀（artifact 顺序），未物化的内建源（探针源）与用户模块依次追加；安装后 `verify_installed` 钉住这一位次，位置错位即报 compiler invariant。定义编号的排序键是「物化块优先、模块编号、模块内候选登记顺序」，`Definition.module` 因此单调不减（`Module::verify` 校验），定义表按模块连续排列——这是分块安装与裁剪能够按区间切段的位置前提。身份表按同一前缀安装：定义候选先按「内建模块的定义在前、用户模块在后」稳定重排（判定只看模块归属，不看是否被物化覆盖，因此构建期派生与用户编译给出同一顺序），块内保持登记顺序，装载前逐项核对现算身份与物化前缀，不符即失败；物化前缀的内容身份进入 `LowerHir` 查询键，而世界身份（`input_fingerprint` 及其派生）仍只由源码与依赖决定，两条加载路径必须给出同一种编号无关指纹，该不变量由 `numbering_stability` 用真实语料逐项比对。HIR 片段按同一前缀接管：解码后立即跑完整结构 verifier，`types`/`definitions`/`aggregates`/`interfaces`/`implementations`/`opaques`/`owners`/`initialization`/`linkage` 整段装入，类型驻留表用片段内容预置，用户侧复用同一批 `TypeId` 只追加新类型；各侧表只对边界之后的用户定义补算，物化路径跳过内建块的降级，现算路径按「内建块先完成、用户定义整体追加」的两段顺序补齐。owner 的 `input_fingerprint` 是 owner 级缓存的输入身份，装载时按当前世界重算，不搬运构建期世界的值。契约记录的类型目录（`record.types`）在内建 `linkage` 未被裁剪时同样直取物化表，七个 runtime layout verifier 仍在构造 raw model 时逐字段校验它。generic GIR body 表（`gir.bodies`）按 owner 顺序整段安装：装载时逐条核对 `owner` 与 `owner_key` 并跑 `gir::verify`，用户 owner 仍走 `BuildGenericGir` 查询并按 owner 指纹校验缓存结果。物化块不参与派生这一点是可观测事实，不只是输出等价：内建 owner 从不注册 `BuildGenericGir` cell，由 `materialized_block_skips_builtin_derivation` 用 cell 统计钉住。早期常量表（`early.constants`）同样整段接管：comptime 只对未被物化覆盖的模块求值，用户定义的结果按定义序追加；comptime 的 query key 折叠物化前缀身份，两条加载路径不会互相复用缓存。类型检查表（`checked.semantics`）同样整段安装：内建 body 的推断结果取自前缀，用户模块照旧现算后按定义序追加；内建隐藏类型在检查用户 body **之前**就要就位（用户推断会读到它们），初始化计划的内建条目同样取自前缀、用户条目由本次计划按定义序追加。类型检查的查询键只由世界身份决定，不折叠前缀身份——两条加载路径的输出完全相同，把前缀折进键会让依赖指纹带着模式差异流进下游内容指纹。原先设想的「把 `Model` 内建条目改由 `prebuilt.hir` 构造」这条路已核实不成立：`model::Nominal` 携带 `params`（泛型参数名）与 `model::Representation`，而 `hir::Aggregate` 只有 `variants`/`representation`，逆向构造会丢参数名。缺少 `prebuilt`、revision 或摘要不符、结构校验失败都不会回退到源码派生：内建块只有这一条加载路径。

版本策略：物化表与缓存 payload 的编码形态变更时，`canonical::REVISION` 与 `QUERY_REGISTRY_REVISION` 一起递增，旧 artifact 与旧 action/query record 全部失效；只在语义等价、payload 形状不变的前提下才允许共用记录。本轮把 `Span` 的会话局部表身份移出编码后，二者分别升到 2 与 4；`LowerHir` 在键内容变化时单独升到 8。各层 revision（`checked.semantics`、`early.constants`、`gir.body`、`gir.build`）只在对应派生规则改变时递增。

按 import 闭包裁剪内建模块在本实现中不成立，已按计划 5.4 的收缩规则移除裁剪逻辑并保留分块 schema。实测依据：裁掉未引用模块的 `hir.owners`、`checked.bodies`、`early.constants`、`gir_bodies` 后，`identity::collect` 的边界断言（现算内建定义数必须等于物化前缀定义数）报出 1386 对 1388——`checked.bodies` 与 `hir.owners` 是身份表与模型派生的输入，不是纯派生成分，删条目会改变编号空间；`gir_bodies` 与 owner 下标一一对齐，单独剪会让装载期 `gir_body(index)` 错位；`early.constants` 可按 `ConstKey.module` 独立过滤，但只此一项收益不足以抵消重映射风险。因此安装路径仍整块安装内建块，分块清单（`blocks` 的定义/owner 区间）在装载期照常校验，作为后续裁剪的位置前提保留。

规范编码 `GBC1` 使用：

- 小端无符号固定宽度整数；
- `bool` 为单字节 0 或 1；
- 枚举使用本章/对应 IR schema 固定的 `u16` tag；
- byte/string/sequence 先写 `u64` 字节数或元素数，再写内容；
- UTF-8 string 不带 NUL 结尾；
- map/set 必须先把键编码为 byte string，再按该 byte string 的无符号字典序写入；
- optional 值先写 0/1 tag；
- package/source 路径只使用 package-relative、`/` 分隔的规范 UTF-8；toolchain 文件使用 toolchain-relative逻辑路径加内容 digest；ELF `PT_INTERP` 等目标运行时路径使用目标 namespace 中以 `/` 开头的规范 byte path，不能写构建宿主 sysroot绝对位置；
- 浮点写 IEEE bit pattern，不做文本转换或 NaN 规范化。

跨 session 的定义与类型身份使用稳定键；指针、线程编号和绝对 workspace 路径禁止进入规范编码。冻结 IR payload 保留现有确定性构造顺序下的稠密 arena ID，完整 HIR/world 输入指纹将其绑定到当前定义与类型表；不能把这些局部编号当成跨 world 的定义身份。

## 编译器构建身份

`CompilerIdentity` 是下列字段的规范编码摘要：

- 编译器源码 revision、工作树状态标志、目标 Unicode 数据版本（`unicode`，当前 18.0.0）与依赖提供的文本表版本（规范化、分段，当前 17.0.0）；
- AST、HIR、GIR、LIR、cache、stack-map、GC metadata 与后端 schema 版本；
- comptime capability registry 摘要、evaluator/验证器 revision、late comptime schema；
- abstract analysis semantics revision、`PublicSummaryPolicyV1` revision 与公共摘要 schema；
- runtime 和标准库源码摘要；
- 编译 compiler 使用的 Rust target、panic 模式及会改变生成物的 Cargo feature；
- 启用的目标注册表和内建 lang item 表摘要。

正式发布构建的工作树状态必须是 clean。开发构建可以缓存，但 dirty 摘要必须覆盖实际修改内容，不能只使用布尔 `dirty`。`CompilerIdentity` 进入所有跨进程 action key；内部格式改变必须提高对应 schema 版本或改变源码摘要。

## query 注册表

query kind 使用固定 `u16` 编号和独立 schema 版本。当前注册表为：

| 编号 | query | key | 可持久化结果 |
|------|-------|-----|----------------|
| 1 | `SourceSnapshot` | package + logical path | 源码快照 |
| 2 | `Lex` | source fingerprint | token/trivia buffer |
| 3 | `Parse` | source fingerprint | AST file |
| 4 | `Configure` | AST + target cfg | 启用项集合 |
| 5 | `CollectDefinitions` | package | 定义表 |
| 6 | `ResolveImports` | package | 模块绑定表 |
| 7 | `LowerHir` | stable owner key | HIR owner |
| 8 | `TypeCheck` | stable owner key | typeck 侧表 |
| 9 | `TraitSelection` | canonical obligation | impl selection |
| 10 | `EvaluateEarlyComptime` | stable definition + args + comptime domain | 早期 comptime 值 |
| 11 | `BuildGenericGir` | 冻结 HIR 指纹 + owner 稳定键 + owner 下标（schema 7，域 `gugu-build-generic-gir-v1`） | generic GIR body（含 `large_copies`） |
| 12 | `CollectMonoRoots` | 冻结 HIR + target/harness + runtime 执行根策略（schema 4） | executable 根与独立 record 根 |
| 13 | `InstantiateGir` | 冻结 HIR + `MonoKey`（schema 6） | 单态化实例记录 |
| 14 | `LayoutOf` | stable concrete type key | 目标布局 |
| 15 | `BuildLir` | `MonoKey` + consumed late-result fingerprint | 优化后 LIR + `PollSummary` |
| 16 | `CodegenFragment` | `MonoKey` + LIR fingerprint | 机器码片段 |
| 17 | `TypeMetadata` | stable concrete type key | 未重定位类型 metadata |
| 18 | `RuntimeMetadata` | closed-world type/instance set + late table | metadata sections |
| 19 | `PlanImage` | target + roots + fragments + late table | 镜像布局与重定位计划 |
| 20 | `EmitImage` | image plan fingerprint | 最终镜像 |
| 21 | `ParseSource` | generated source fingerprint + source slot | `ParsedSource` |
| 22 | `ExpandSourceMacro` | stable macro call + round + source slot + script inputs | generated source/fragment + expansion record |
| 23 | `FunctionAnalysisSummary` | `MonoKey` + analysis policy + world 输入（schema 5） | completed SCC 中的函数摘要投影 |
| 24 | `WholeProgramAnalysis` | closed-world instance graph + analysis policy + late table（schema 6，输入含冻结 HIR 指纹、generic GIR 指纹与实例图/late 指纹） | 排序摘要与 world-local 证明事实 |
| 25 | `FreezeTypeUniverse` | closed-world instance graph（schema 1；实例 world schema 6） | `TypeUniverseKey`、类型序列与稠密编号 |
| 26 | `EvaluateLateComptime` | `LateConstKey` + `TypeUniverseKey` + registry（schema 1） | 固定形状 late 标量聚合与类型重定位 |
| 27 | `AnalysisSccSummary` | 排序 `MonoKey` 集 + analysis policy + world 输入（schema 5） | 完整 SCC 摘要固定点 |
| 28 | `PublicFunctionSummary` | `MonoKey` + analysis semantics revision + public policy revision + 已完成 world 的结果指纹（schema 2） | 内容寻址跨 package 摘要 |
| 29 | `EscapeAndPlacement` | 放置前 GIR 指纹 + 分析输入指纹 + 冻结 HIR 指纹（schema 2，域 `gugu-escape-placement-v1`） | `PlacementWorldV1` |
| 30 | `RuntimeRawModel` | target + tuning/platform profile + LIR/placement 指纹 + 资源需求 + 独立契约类型目录（schema 29，域 `gugu-runtime-raw-v1`） | `RuntimeRawContractV1`：完整 raw/resource/rt0/scheduler/wait/sync/GC、屏障、LocalHeap/SharedHeap、mark/edge、region/block-return、compression/routing/provenance/combining 契约与真实 type/meta sections |

新增 query kind 或改变 payload 编码必须更新 query registry revision；当前 revision 为 3，并把 GBC1 revision 纳入 `QueryKey`。旧 revision 的 action/query record 不得复用。编号 21--30 不得重用或改变既有编号含义。`registry_fingerprint()` 按编号升序编码 `(kind, name)`，经 `ActionInputs::set_query_registry` 进入 action key；`set_runtime_raw` 记录本次 action 消费的 raw 契约指纹。query 23 与 27 的 callable 身份是 `MonoKey`，不使用 owner 下标作为跨实例身份。

`BuildGenericGir` 为每个冻结 HIR owner 构造一份 generic body，依赖 `LowerHir` schema 7 的模块指纹，缓存恢复时核对 owner 定义及其稳定键。`GirWorldV1` schema 3 在 fragment 之外保存 placement，指纹域为 `gugu-gir-world-v3`。`InstantiateGir` 仍从 HIR 收集调用边；可达 GIR 的隐含 runtime glue 需求在同一闭包固定点内补齐。generic GIR 指纹（含 placement）进入 action identity 与镜像计划，全程序分析仍消费放置前的 GIR 指纹。冻结之后按 `gir → mono → late → analysis → placement → summary` 执行。

## query 状态机

每个 `(kind, canonical key)` 在一个 session 内恰有一个 query cell，状态只沿下列方向转换：

```text
Vacant -> Computing -> Complete
                    -> Failed
                    -> Cancelled
```

首个请求者以原子 compare-exchange 获得 `Computing`；其他请求者等待同一 cell，不重复计算。`Complete` 保存不可变结果、结果 fingerprint、直接依赖列表和排序后的诊断。`Failed` 只在本 session 内 memoize 同一错误；失败结果不写持久缓存。上游取消导致 `Cancelled`，后续新顶层 action 可以新建 session 重新计算。

query 执行期间，线程局部 query stack 记录读取的每个 input/query。读取完成后把依赖稳定 key 加入当前 query 的直接依赖集合；集合去重并按 `(kind, key bytes)` 排序，不能记录调用时序。

若当前 stack 再次请求同一 cell，query engine 产生确定性的 cycle 报告。允许递归的语言结构必须由相应 query 显式求 SCC 或 fixpoint，不能靠 query engine 返回半初始化对象：

- 函数签名和模块定义由 `CollectDefinitions` 一次收集；
- `ExpandSourceMacro` 按 `(stable macro call, expansion round, source slot)` 显式推进有限展开；再次遇到相同展开栈 key 报 expansion cycle，预算耗尽返回失败，不能读取半初始化 AST；
- `EvaluateLateComptime` 只读取已经完成的 `FreezeTypeUniverse`；其依赖图必须是有向无环图，返回类型形成、源码宏或单态化 query 的边是内部错误；
- `WholeProgramAnalysis` 先建立调用图 SCC，`AnalysisSccSummary` 再按稳定成员顺序显式求摘要固定点；`FunctionAnalysisSummary` 和 `PublicFunctionSummary` 只能投影完成的 SCC 结果，不读取半初始化成员；
- 互递归函数 body 各自 type-check，但只依赖已冻结签名；
- trait/impl obligation 使用独立 obligation stack 报循环或求规范允许的固定点；
- 相同 `MonoKey` 的递归调用复用正在收集的实例节点，不再次实例化。

并行任务之间的等待边也加入 session wait graph。形成环时由稳定 key 最大的等待者中止等待并把完整环交给普通 cycle 诊断，避免线程死锁。

## 指纹与 red/green 复用

input fingerprint 为 `gugu-input-v1` 域下的规范输入摘要。query record 分开保存**依赖
fingerprint**和**结果 fingerprint**；前者判断是否需要重算，后者判断变化是否应继续传播。
result fingerprint 的规范输入为：

```text
kind number
query schema version
canonical key
canonical successful result bytes
sorted diagnostics bytes
```

经 `gugu-query-result-v1` 域哈希得到 32 字节值。直接依赖列表及当时的 input/query result
fingerprint 另存于 record，不写进结果 fingerprint；否则任一实现依赖变化都会强制向所有
caller 传播，即使可消费结果逐字节未变，red/green 就失去结果等价的意义。

加载上一 session 的 query record 时，engine 先递归验证直接 input/query 的当前 fingerprint：

- 全部相同则标为 green，直接复用结果；
- 任一不同、缺失或 schema 不匹配则标为 red，重新执行；
- 重新执行后若 result fingerprint 与旧值相同，只更新本 query 的依赖记录，下游保持 green；
- 结果 fingerprint 不同才把 red 继续传播给依赖该结果的 query。

依赖验证不能只比较 result fingerprint：input 内容或 producer provenance 变化必须先使当前
query 重算；只有重算后确认结果等价，才允许停止传播。`PublicFunctionSummary` 因而可以在
callee body 改变但公共语义摘要不变时保持同一对象 key 和结果 fingerprint，同时仍保证生产
action 已针对新 body 重新验证。

query 不允许读取未通过 query/input API 登记的文件、环境、时钟、随机源或全局可变状态。build.gg 输出、`embed_file`、target 配置、feature、锁图、源码宏生成文本、宏展开属性、comptime capability registry、parser/evaluator/analysis schema、late type universe、公共摘要对象键和 native link metadata 都作为显式输入或 `CompilerIdentity` 成分注入。

## 持久 object 格式

每个内容对象是一个不压缩的文件，header 固定 56 字节：

```text
magic:          [u8; 8] = "GUGUCV01"
kind:           u16
flags:          u16 = 0
schema_version: u32
payload_len:    u64
payload_hash:   [u8; 32] = BLAKE3-256("gugu-object-v1" || 0x00 || payload)
payload:        [u8; payload_len]
```

不压缩是固定选择：前端/IR payload 主要在同机频繁读写，避免压缩器版本影响、额外分配和解压 CPU；容量由现有 LRU 策略控制。`payload_len` 不得超过 `u32::MAX`，也必须恰好等于文件长度减 56；所有保留字段必须为 0。reader遇到未知 flag、尾随字节、长度溢出或摘要不符都把对象视为损坏。

对象 key 为完整 header 中 `kind`、`schema_version` 和未压缩 payload 的规范编码经 `gugu-object-v1` 域哈希的结果。文件名使用 64 个小写十六进制字符。

## action record 与目录

当前内部目录位于全局 cache 根的：

```text
compile/v1/objects/HH/HASH
compile/v1/actions/HH/HASH
compile/v1/tmp/
compile/v1/quarantine/
```

`HH` 是 key 的前两个十六进制字符，`HASH` 是完整 key。目录布局只供同一
`CompilerIdentity` 的 Gugu 工具使用。

`ActionKey` 由 compiler identity、query kind/key、目标、harness/插桩、feature 域、完整锁图、宏展开闭包摘要、分析策略和所有直接输入摘要组成。action record 也是普通 object，其 payload 固定包含：

- action key 的完整 32 字节值；
- 成功结果 kind 与 object key；
- 排序后的辅助 object key，如诊断、stack map、unwind 和 relocation；
- 创建该记录的 compiler identity；
- payload 总长度，用于 LRU accounting。

记录不保存创建时间；LRU 访问时间属于独立、可丢失的 cache 索引，不参与 action 内容。

writer 在 `tmp/` 中以随机不可猜名称创建同文件系统临时文件，完整写入、刷新文件内容、重新读取并验证摘要后，以 create-if-absent 原子发布到目标路径。目标已存在时验证既有对象并丢弃临时文件；同 key 不同内容是 compiler internal error。Windows 和 Linux 都不得先删除已存在目标再重命名。损坏文件原子移入 `quarantine/` 后重新构建；隔离失败时也必须绕过该 entry，不能继续反序列化。

reader 的已打开文件句柄就是 lease：Linux 即使被 unlink 仍从原 inode 完整读取；Windows 以 `FILE_SHARE_READ` 打开且不授予 delete sharing，使删除非阻塞失败。LRU 回收对候选执行一次非阻塞删除，Linux unlink 后由最后句柄回收，Windows 遇 sharing violation 直接跳过；不得等待活动 reader。writer 只持有 `tmp/` 文件并以 create-if-absent 发布。进程崩溃留下的 `tmp/` 文件不被索引，下一次 cache maintenance 可以清理。

## 依赖缓存与 ActionInputs

`project::cache::ActionInputs` 已实现 action key 的输入收集和规范排序。session 内 query engine 与编译对象存储见下一节；持久 action record reader、LRU 索引和 CLI cache 子命令尚未接入。`DependencyCache` 只负责外部 package 源码输入，不把未验证源码、编译中间对象或 target 视图混入依赖缓存。

## Query engine 与对象存储

`gugu-base::query` 落地 session 内 query engine 与编译对象存储（façade 再导出）：
`QueryKind` 固定登记 1--30，`QueryKey` 使用 schema 与规范 key 形成域隔离摘要；每个
query cell 按 `Uncomputed -> Computing -> Complete/Failed/Cancelled` 转换，并以条件变量
让并发请求共享唯一计算结果。成功结果保存不可变 payload、结果 fingerprint 和按稳定 key
排序去重的直接依赖；失败与取消不会作为成功对象写入持久存储。

`ObjectCache` 使用本章的 `compile/v1/objects/HH/HASH` 布局和 `GUGUCV01` 固定 header，
写入先在 `tmp/` 完整同步、重新校验，再以 create-if-absent 硬链接发布。读取先检查 magic、
kind、flags、schema、长度与 BLAKE3 payload 摘要，再把 payload 交给 verifier；损坏或 schema
不匹配的对象会被移入 `quarantine/`，不可信字节不会交给 verifier 之外的 consumer。

当前只提供可复用的 compiler 基础设施；action record、LRU 索引、清理命令和 target 物化尚未完整接入 CLI。

## 单态化闭合与公共摘要 {#mono-closure-public-digest}

`CollectMonoRoots`（12，schema 4）与 `InstantiateGir`（13，schema 6）已在
`gugu-frontend::frontend::mono` 落地。闭合发生在 `LowerHir` 冻结之后，
与 generic GIR、late 和全程序分析同一管线：

- `StableTypeKey`：语义类型经 GBC1 编码（`u16` kind tag、小端定宽、长度前缀）
  后按 `gugu-mono-v1` 域哈希；名义定义携带 `Definition.key` 稳定键、repr 标志、
  对齐与 tag 形态；闭包/opaque 携带 owner 稳定键。泛型参数、投影与未收敛变量
  出现在具体类型中即 `E0052`。`MonoInterner` 同时保留 digest 与规范字节，
  相同 digest 不同字节是 `DefinitionHashCollision` 内部错误。
- `MonoKey`：definition 稳定键、`StableTypeKey` 序列、规范 comptime 值序列、
  selected_impls、`u16` call_abi、target、harness 与插桩位。C ABI 导入只登记
  外部符号，有体 C ABI 导出使用 C 调用 ABI。comptime 实参复用早期求值器的
  值树并按 GBC1 编码；相同值共享实例，不同值分别闭合。
- executable 根：入口、普通用户 `#[used]`、导出、static/const 初始化器、global asm 与 harness 函数，以及启动和登记机器入口必需的 runtime 定义。runtime 普通 `#[used]` 不再自动成为全局执行根；实际调用和可达 GIR 操作的隐含 glue 需求进入同一个固定点。record 根单独按稳定定义 key 排序、去重并进入图指纹，只生成签名布局目录。缓存恢复时核对 executable 根类别和排序、record 根完整集合，不接受缺失或额外定义。late comptime 依赖随可达具体实例收集，未使用的泛型声明不作为无实参根；被 cfg 裁掉的定义不入根。
- 闭合 driver：pending 为按 key 摘要排序的 `BTreeSet`，依次取出首项实例化
  并把未见 callee 入队；递归调用复用相同实例节点只形成图边。ancestry 预算按
  链上不同 key 256 / 同一 definition 严格增长重复 128 报 `E0052`；实例总数
  达 `u32::MAX` 报 `E0053`。`MonoId` 在闭合后按 key 摘要排序分配。
  具体 GIR 构造预先计算一次实例摘要列，再二分定位 fragment；late 结果验证同样使用
  摘要顺序查找，未知实例仍拒绝。新鲜 HIR 由 `Validated::freeze` 统一执行完整 verifier，
  不在调用方重复验证同一对象。具体 GIR 构造及每轮 pass 完整验证结构并计算一次指纹；
  已有对象和缓存对象的 `verify` 仍重新计算指纹并比对，不信任序列化中的指纹字段。
- 实例边以 HIR owner 为边界，覆盖全部 dispatch 位点（普通/协程调用、运算符与
  `for`/`try`/下标协议派发）、擦除前的函数值、闭包、协程 body、局部 static
  初始化器、类型反射及 dyn 擦除的 vtable 根。不同具体类型实现同一接口时，
  以 `(interface, self_type)` 分别保留 vtable。闭包和初始化器的调用不计入外层 body。
  泛型体在绑定具体类型后重新选择 impl，再统一方法自身的泛型实参；trait
  obligation 及所属 impl 参与实例键，body 选择的 impl 也记入实例依赖。
- `InstantiateGir` 的 session key 同时包含冻结 HIR 输入指纹与 `MonoKey`。
  同一 session 的源码变化不会重用旧实例；缓存命中仍重建 interner 的类型和实例记录。
- `InstanceRecordV1` 保留实例键、具体签名/ABI 指纹、body 输入指纹、调用位点与
  callee 实例映射、selected_impls、vtable/metadata 根、外部符号及 late 依赖。
  fragment 输入指纹不依赖定义表编号，且实现变化会更新；它与公共签名指纹分离。
  机器码、relocation 与 stack map 由后端代码生成写出。
- `PublicFunctionSummary`（28，schema 2）从已完成实例 SCC 投影公共摘要：效果位、
  严格递增的 read/write 参数序号、参数数量与条件事实。参数集合没有 64 个的截断边界；
  verifier 检查版本、未知效果位、排序及参数引用范围。对象 key 对规范 GBC1 内容取摘要，
  不包含私有定义或 session-local 编号。生产者输入更新会重算 query，但可消费内容
  未变时对象 key 不变。对象 key 经 `ActionInputs::add_public_summary` 进入前端 action key；
  磁盘持久化与跨 package 消费尚未接入 CLI。
- 分析 query 23/27 的 schema 为 5，world schema 为 6。固定点在 generic GIR body 上求解，
  成员与调用位点按具体实例确定；不把同一泛型定义的首个摘要复制给其它实例。
  SCC 按凝聚图顺序完成，函数 query 仅投影完成结果。共享 HIR 上的检查只有在全部
  可达实例均证明安全时才省略；未收敛 SCC 置预算耗尽并保留检查。证明只存在于
  `AnalysisWorldV1.proofs`。

## 单态化实例 {#mono-instance}

### 稳定类型与实例键

`StableTypeKey` 是具体类型的规范结构编码经 `gugu-mono-v1` 域哈希的 32 字节值。编码必须包含：

- 名义定义的 `StableDefKey`，或结构类型的 kind；
- 规范化后的全部类型和 comptime 实参；
- 引用/指针种类、函数签名、ABI、数组长度和 repr；
- 闭包/opaque 类型的 owner 稳定键与匿名节点消歧信息。

它不包含运行时稠密 `TypeId`。相同结构元组得到同一 key；不同名义定义即使布局相同也得到不同 key。

函数项类型（`fn` 声明与闭包形成的有名/匿名函数值类型）同样只由上面的结构编码决定：
它的编码包含被引用函数的 `StableDefKey`、调用 ABI、类型实参与签名。它不使用该函数的
`MonoKey` 或 `Definition.key` 本身，因此 `Ty::Callable` 与任何其它 `Ty` 共用同一条
`type_key` 编码路径；冻结类型表、具体 GIR 类型表与 LIR 符号地址必须引用同一个
`StableTypeKey`，不允许任何阶段对同一 `Ty` 走第二条成键规则。owner 内合成的 panic 消息使用 HIR 固定基础类型表中的 `string`；对含 panic 检查、panic 效果或 suspend 操作的 owner，冻结类型宇宙也登记相同 `StableTypeKey`，使具体 GIR 可为合成消息物化布局而不另造键。

`MonoKey` 的规范字段为：

```text
MonoKey {
    definition: StableDefKey,
    type_arguments: [StableTypeKey],
    const_arguments: [canonical ConstValue],
    selected_impls: [StableDefKey],
    call_abi: Gugu | C,
    target: canonical target name,
    harness_mode,
    instrumentation_mode,
}
```

字段经 `gugu-mono-v1` 域哈希得到实例摘要。具名函数、闭包 body、static initializer、vtable shim、copy/drop/publish glue 和 runtime adapter 都使用该结构；compiler-generated item 通过 definition 域中的固定 synthetic kind 区分。

compiler 在 interner 中同时保留 key对应的规范 bytes。相同 `StableTypeKey`/`MonoKey` 再次出现时必须逐字节比较；摘要相同而规范 bytes不同是 digest collision，立即以 compiler internal error停止，不能合并实例、重盐或依赖发现顺序。

### 根与可达性

单态化根固定包括：

- 可执行入口、选中的 lib 导出和 `#[export_name]`；
- 当前 test/bench harness 收集的函数；
- 可达 static 初始化器与退出 glue；
- runtime/GC/调度器所需 lang item；
- 可达 `dyn Trait` 的 vtable、方法 shim 和 downcast metadata；
- `type_id[T]`、`type_name[T]` 和 metadata 查询显式要求的类型；
- global asm、native 导入和 C 回调所引用的内部符号。
collector 使用按 `MonoKey` 摘要字节序排列的 `BTreeSet` 作为 pending 集合；集合无可证明小上界且需要稳定有序 pop，因此不使用稠密位图。已发现实例用 hash table 点查以避免重复，但最终 `MonoId(u32)` 只在收集闭合后按 `MonoKey` 排序分配。hash table 的迭代顺序不得影响工作顺序。

处理一个实例时，collector 从 monomorphic GIR 收集直接调用、函数值、vtable、类型 descriptor、global 和 glue 边，并把未见 key 插入 pending。调用自身或 SCC 中已标记 `Visiting` 的相同 key 只增加图边。沿一条实例化 ancestry 出现超过 256 个不同 `MonoKey`，或同一 generic definition 在 ancestry 中以严格增长的类型结构重复 128 次，报无法收敛的递归单态化；总实例达到 `u32::MAX` 报 `implementation-limit`。

“类型结构大小”固定为规范类型树节点数：每个 primitive/名义定义引用/泛型参数叶计 1，每个 tuple/array/reference/pointer/function/dyn/opaque/closure 构造器计 1并递归加全部类型实参，comptime scalar实参各计 1，结构化 comptime值按其规范值树计数；计数以 `u64` 饱和。同一 definition最近 128 次出现的总大小严格逐次增加才命中增长规则，大小不增或相同 key 的普通递归只形成图环。

首版总是完整单态化，不做跨类型的 polymorphization 或 dictionary sharing。布局相同的不同 `MonoKey` 只有在最终机器码、relocation、stack map、unwind、source record 和可见性全部相同时才由 image planner 做 identical code folding；其符号、诊断和 metadata 身份仍独立。

### 具体类型集合与 `TypeId` {#concrete-type-set-typeid}

实例图闭合后，`FreezeTypeUniverse` 从签名、local、global、vtable、descriptor、早期常量、
late comptime 闭包中的类型依赖和 runtime 根递归收集所有具体类型。类型集合按完整
`StableTypeKey` 字节序排序并分配连续运行时 `TypeId` `0..N`；`N` 必须小于等于 `u32::MAX`。
结果的规范编码经独立 query fingerprint 形成 `TypeUniverseKey`，并在本次 world 内不可变。

`EvaluateLateComptime` 随后只读取 `TypeUniverseKey`、冻结布局、早期常量与 capability
registry，按稳定 `LateConstKey` 顺序生成 late 结果表。任何新类型、实例、impl、宏或定义
依赖都是内部错误。`WholeProgramAnalysis`、`BuildLir`、`RuntimeMetadata` 和 `PlanImage` 只消费
该表，不能重新执行 evaluator。

单类型 metadata object 可以独立缓存，但其中对其他类型、glue 和 vtable 的引用必须保存
稳定 relocation key。最终 `RuntimeMetadata` query 在本次闭世界集合上把这些 key 重定位为
稠密 `TypeId`/RVA，并填充 late metadata 值。含最终数字 `TypeId` 的完整 section 不能跨不同
`TypeUniverseKey` 直接复用。

## 每实例代码缓存

`CodegenFragment` 以单个 `MonoKey` 为粒度，payload 固定包含：

- 机器码 bytes 与对齐；
- 本地只读常量 bytes；
- 按 offset 排序的逻辑 relocation；函数/数据引用使用 stable symbol，直接运行时类型常量/记录使用完整 `StableTypeKey`，源码索引使用 `{ MonoKey, fragment_source_ordinal }`；
- 未重定位 stack map、unwind 和 source location 记录；每条保存逻辑路径、byte span、synthetic kind和 fragment source ordinal；
- 定义符号、引用符号及可见性；
- LIR result fingerprint、直接计费的 `PollFreeLeaf` summary fingerprint、所有内联 callee fingerprint，以及该 LIR 实际消费的 late 结果 fingerprint。

单实例粒度避免 package 内一个无关函数变化使整个目标代码失效。直接 `type_id[T]()` 仍以 `StableTypeKey` relocation 表示，不把本次 world 的稠密编号烘焙进可跨 world 复用的片段。若 GIR 优化或 LIR 物化实际消费了 `type_id_count()`、comptime `TypeId.as_int()` 或其它 late 标量，`BuildLir` key 和 fragment payload 必须包含精确 late 结果 fingerprint；这类片段只能在相同 late 结果下复用。跨实例内联使 caller key依赖被内联 callee的 GIR/result fingerprint；未内联且保留 entry `StackCheck` 的普通调用只依赖 callee ABI/signature fingerprint，不因 callee body改变而使 caller失效。只有 direct `PollFreeLeaf` 调用额外依赖其固定尺寸 `PollSummary`；leaf分类或 `poll_free_cost` 改变时 caller `BuildLir`必须失效。递归/间接调用始终使用 checked entry，不形成 `BuildLir` query环。

机器码 object key还必须覆盖 target、CPU baseline、内部 ABI revision、panic/unwind模式、GC barrier revision、poll/`NoSafepointRegion` policy revision、stack-map schema和 instrumentation。绝对代码地址和本次闭世界的稠密 `TypeId`不进入 fragment；只以逻辑 symbol/type relocation表示。

## JIT 兼容发布边界

当前发布 profile只生成 AOT，不启用 JIT。未来内部 profile若加入分层编译，只能为闭世界收集阶段已经存在的 `MonoKey` 生成新的 `CodeVersion`；不得在 `Running` 后追加 `MonoKey`、具体类型、`TypeId`、vtable槽或导出。

每个可替换实例使用 compiler生成的 `CodeSlot`。发布新 version前，runtime先验证并登记 code range、relocation、stack map、unwind和 source records，再以 release store更新 slot；caller acquire load后才能跳转。旧 code range在所有 worker的执行 epoch都越过该 version且没有 return PC/函数值引用前不能回收。生成或验证失败不得修改 slot或继续隐藏错误，必须走该内部 profile的显式失败路径。

是否经 CodeSlot调用、tier阈值和 version内容都进入 compiler profile与 cache key。AOT direct-call profile不含可补丁跳板；JIT不能靠改写任意指令绕过 metadata原子发布。

## 并行执行与确定性

query scheduler 可以并行执行无依赖 owner 和 `MonoKey`。任务队列优先级按 `(query kind, stable key bytes)` 排序仅用于可重现调试；运行时完成顺序不参与任何结果。并行产物在以下边界统一排序后分配稠密 ID 或写出：

- 定义表和匿名定义；
- 单态化实例和具体类型；
- 诊断；
- 符号、常量、relocation 和 metadata record；
- 测试/bench 注册项。

同一 compiler identity、目标和输入的两次 clean build 必须得到相同 query/result fingerprints、相同 fragment bytes 和相同最终镜像 bytes，允许的非语义 build-id 节必须在可复现模式中关闭或由完整内容摘要派生。

## 安全与验证

反序列化缓存对象时先验证 header、`payload_len <= u32::MAX`、文件精确长度，并以固定缓冲流式验证 payload hash，再按已验证长度分配 arena。sequence长度乘元素大小必须 checked；arena ID、range、enum tag、UTF-8、IR verifier和 target/schema必须全部验证。cache文件即使位于用户自己的 cache目录也视为不可信输入，禁止用 unchecked索引或把字节直接转成含 Rust引用/枚举的内存布局。

缓存命中后仍必须在阶段边界运行对应的结构 verifier。`CodegenFragment` 还要验证 relocation 位于片段内、符号 key 存在、stack map safepoint 对应指令边界及 unwind 范围不重叠。

## 参考实现资料

- [Rust 编译器开发指南：Query system](https://rustc-dev-guide.rust-lang.org/query.html)
- [Rust 编译器开发指南：增量编译](https://rustc-dev-guide.rust-lang.org/queries/incremental-compilation.html)
- [Rust 编译器开发指南：单态化](https://rustc-dev-guide.rust-lang.org/backend/monomorph.html)
- [Go build cache 源码](https://go.dev/src/cmd/go/internal/cache/cache.go)
