# Gugu 生产级完整实现路线图

## 使用说明

本文件是临时任务文件，不是文档。它把当前仓库 `docs/src/spec/` 中的公开语言、工具链、标准库、运行时与 ABI 规范，以及 `docs/src/internals/` 中的编译器和 runtime 内部契约拆成可交付的实现阶段，用于分阶段实现整个规范。它不属于 mdBook 书籍源，不进入 `docs/src/SUMMARY.md` 导航，也不是规范、教程、参考或 internals 章节，仓库的永久约定（如 `AGENTS.md`）不引用它；整个规范实现完成并通过最终发布门禁后，删除本文件。

「阶段 N」是本文件内部的临时任务标识，只允许出现在本文件和聊天对话中，不得出现在仓库的任何其它位置。其它位置需要指代某项能力时，直接引用对应的规范或 internals 章节。

条目以 `[ ]`/`[x]` 表示完成状态：阶段验证通过后把条目标为 `[x]` 并保留，不删除，以便追踪进度与阶段间依赖。路线图按可交付垂直切片组织，同时显式区分实现阶段与跨阶段里程碑。

复杂度采用 1–5 级，表示实现难度与所需 AI 推理能力，不表示完成状态：`1` 是局部、机械、低耦合任务；`2` 是边界清晰的单组件任务；`3` 是一次会话可完成的标准中型子系统；`4` 涉及多个契约、并发不变量或安全边界，需要更强模型与更完整验证；`5` 是跨层集成、GC/调度/后端/ABI/发布门禁等高风险任务，需要最强模型、分阶段验证，通常还应继续拆成 `2/3` 级子阶段。

路线图的权威依据：

- 公开语言契约：[规范总览](src/spec/overview.md)、[词法](src/spec/lexical.md)、[形式语法](src/spec/syntax.md)、[类型](src/spec/types.md)、[声明](src/spec/declarations.md)、[表达式](src/spec/expressions.md)、[模式](src/spec/patterns.md)、[函数](src/spec/functions.md)、[trait](src/spec/traits.md)、[传递](src/spec/passing.md)、[内存](src/spec/memory.md)。
- 工具与平台契约：[程序模型](src/spec/program-model.md)、[包与构建](src/spec/packages-builds.md)、[发布生态](src/spec/publishing-ecosystem.md)、[工具链 CLI](src/spec/toolchain-cli.md)、[平台 ABI](src/spec/platform-abi.md)、[运行时](src/spec/runtime.md)、[标准库](src/spec/standard-library.md)、[测试](src/spec/testing.md)、[unsafe](src/spec/unsafe.md)、[格式化](src/spec/format-style.md)。
- 编译器与 runtime 内部契约：[AST/HIR](src/internals/ast-hir.md)、[comptime 分析](src/internals/comptime-analysis.md)、[GIR/LIR](src/internals/gir-lir.md)、[单态化与缓存](src/internals/monomorphization-cache.md)、[栈图](src/internals/stack-maps.md)、[GC 元数据](src/internals/gc-metadata.md)、[内存消息](src/internals/memory-messaging.md)、[调度器](src/internals/scheduler.md)、[x86_64 后端](src/internals/backend.md)。
- 关键架构约束：[ADR-0001](adr/0001-static-closed-world-runtime.md)、[ADR-0002](adr/0002-syntax-concurrency-memory.md)、[ADR-0003](adr/0003-passing-semantics.md)、[ADR-0004](adr/0004-never-patterns-diagnostics.md)、[ADR-0005](adr/0005-impl-trait-try-test-coroutine-local.md)、[ADR-0006](adr/0006-closed-world-type-id.md)、[ADR-0008](adr/0008-platform-abi-reference.md)、[ADR-0009](adr/0009-owner-directed-memory-messaging.md)、[ADR-0010](adr/0010-comptime-source-macros.md)。

路线图从编译器 bootstrap、runtime 引导和规范测试基础开始，最终以双目标、闭世界、无系统 linker 依赖、可复现和可发布为生产级门槛。当前进度以本文件中的 `[x]` 标记为准，不在此另行维护会过期的现状描述。

## 每阶段完成定义

1. 实现只进入该阶段所属的真实归属层；不得通过弱化 fixture、跳过 snapshot 或增加平行兼容接口掩盖缺口。
2. 公开行为与对应 `docs/src/spec/` 章节一致，内部表示与对应 `docs/src/internals/` 章节一致；若发现规范缺口，先在同一阶段修订规范并同步 `docs/src/SUMMARY.md` 导航。
3. 测试使用固定输入、确定性顺序、进程内替身和明确的失败边界；网络、真实子进程、压力负载和随机性能测量放到专门的 bench/手工验证。
4. 阶段验证通过后在本文件把该条目标为 `[x]` 并保留，不删除。工作区级验证使用 `cargo fmt --all --check`、`cargo build --workspace` 和 `cargo nextest run --workspace`；文档构建使用 `mdbook build -d target/book`。
5. 代码或规范提交遵守仓库的 `docs/.commit` 跟踪规则；路线图本身不代表任何阶段已实现，`[x]` 标记也不替代规范、internals 与测试之间的一致性。
6. 「阶段 N」是临时任务标识，只允许出现在本路线图和聊天对话中。禁止把阶段号写入 `docs/src/spec/`、`docs/src/internals/`、ADR、教程与参考文档、代码与注释、测试与 fixture 名称、提交信息或用户可见输出。实现对照现行规范与 internals 契约；规范有缺口时当场修订规范，不把临时任务编号写进规范。

## 路线图使用约束：实现必须形成可运行闭环

单个“实现 X”阶段不得仅新增数据结构、解析器或局部算法；阶段完成前必须把能力接入已有 action graph、query、诊断、缓存 fingerprint、CLI 入口及下游消费者。以下接入规则适用于所有阶段：

1. **归属层接入**：新增模块必须由唯一上游入口调用，禁止只保留未被调用的孤立 API；调用链、错误传播和取消语义必须明确。
2. **数据流闭环**：输入必须从 `SourceSnapshot`/清单进入，经过对应 query，产出可被下游消费的稳定对象；对象必须进入 action key、缓存校验和确定性排序。
3. **失败闭环**：错误必须进入统一 `Diagnostic`/退出码/事件输出路径；失败、取消和脏缓存不得写出后续产物。
4. **阶段桥接**：跨阶段接口必须在本阶段声明 schema、版本和 verifier；下阶段只能消费已验证对象，不能重新解析或猜测上游语义。
5. **可运行切片**：每个阶段至少交付一个真实输入到真实输出的 smoke slice；仅有单元测试、dump 或“未来接入”不算完成。

路线图中的“基础”表示可被真实调用的基础，不表示仅搭建占位类型。若实现先于消费者完成，应把消费者接入列为同一阶段的验收项，或新增明确的集成阶段，不能提前勾选实现阶段。

## 一、工程与前端基础

- [x] **阶段 01：建立 compiler/runtime 工程骨架**（复杂度：4）
  - 依赖：无。
  - 建立 Rust compiler bootstrap、`gugu` CLI、目标描述、诊断、前端、IR、后端、runtime 资源的清晰模块边界；登记用 Gugu 编写的标准库/runtime 源树，明确 rt0、必要 intrinsic 与 Gugu runtime 的边界，禁止维护第二套语义等价 Rust runtime。
  - 验收：空 package、单文件入口和一个最简单 `main` 拥有端到端 action graph；无效阶段不会写出镜像；架构文档与模块清单能够定位每条公开规范的实现归属。

- [x] **阶段 02：实现 CLI 全局参数与输出骨架**（复杂度：2）
  - 依赖：阶段 01。
  - 实现单一 `gugu` 可执行入口、全局参数优先级、子命令注册、`text`/`json`/`json-diagnostic-short` 输出信封和退出码 `0/1/2/101`。
  - 验收：无子命令等价于 help，`version` 与 `--version` 一致，非法参数不进入编译，NDJSON 不泄漏绝对路径和凭据；覆盖 CLI 规范中的命令解析表。

- [x] **阶段 03：实现源码快照、规范路径与 Span 系统**（复杂度：2）
  - 依赖：阶段 01、02。
  - 实现 UTF-8 源码读取、BOM 拒绝、规范化逻辑路径、`SourceSnapshot`、字节/行列映射、宏展开 source context 和稳定文件 ID。
  - 验收：相同输入在不同工作目录、目录枚举顺序和换行环境下产生相同 span 与诊断位置；非法 UTF-8、BOM 和越界 span 有稳定错误。

- [x] **阶段 04：实现清单、workspace 与 target 发现**（复杂度：3）
  - 依赖：阶段 02、03。
  - 实现 `gugu.toml` 向父目录查找、package/workspace 层级、默认 source root、lib/bin/test/bench/example target 自动发现、host/target 分离和 `foo.gg` 与 `foo/mod.gg` 冲突检查。
  - 验收：package、虚拟 workspace、单文件模式和 target 选择规则与规范一致；未知核心字段、target 重名、入口越界和保留 package `std` 均在编译前失败。

- [x] **阶段 05：实现依赖解析、SemVer 与锁图**（复杂度：4）
  - 依赖：阶段 04。
  - 实现 path/git/registry source、package identity、SemVer 求解、依赖别名、target 条件、normal/test/build 三域、feature 并集和确定性 `gugu.lock` 编码。
  - 验收：循环依赖、无解版本、source identity 冲突、锁图不一致和 feature 缺失得到稳定错误；锁文件不含绝对路径、token、缓存位置或宿主信息。

- [x] **阶段 06：实现离线、vendor、checksum 与缓存输入**（复杂度：4）
  - 依赖：阶段 05。
  - 实现依赖源码缓存、归档 checksum 验证、`--offline/--locked/--frozen/--vendor` 组合、vendor mapping、损坏缓存隔离、编译 action key 的完整输入集合和 target 视图目录。
  - 验收：无网络替身下可重放已验证锁图；缺包、checksum 污染、vendor 不一致和 frozen 修改均在目标代码生成前失败；缓存命中与否不改变语义结果。

- [x] **阶段 07：实现词法分析器与字面量/属性**（复杂度：3）
  - 依赖：阶段 03。
  - 覆盖最长记号、换行续行、嵌套块注释、raw string、整数/浮点/字符/字节/C 字符串、数组/元组记号、属性参数与 `cfg` 词法。
  - 验收：词法 token 带精确 span；禁止 `.5`、`5.`、非法 Unicode scalar、错误转义、未知属性参数和非法记号组合；错误恢复不会把占位节点交给 codegen。

- [x] **阶段 08：实现递归下降 parser 与 AST arena**（复杂度：4）
  - 依赖：阶段 07。
  - 按形式语法构造稠密 AST，覆盖声明、泛型、类型、块、表达式、模式、`async`、`select`、`try`、`defer`、`comptime source`、FFI 和 asm 节点；实现错误恢复与节点稳定排序。
  - 验收：规范语法示例全部可解析，非法嵌套和优先级得到主/次诊断；解析结果不使用指针作为节点身份，结构 dump 不受线程完成顺序影响。

- [x] **阶段 09：实现格式化器与 `gugu fmt`**（复杂度：2）
  - 依赖：阶段 03、07、08。
  - 实现规范缩进、换行、尾逗号、use 排序、属性/注释布局、字符串内部字节保留、`--check`、原子写回和 workspace `--all`。
  - 验收：formatter 满足幂等性；解析失败不截断源文件；不会执行 build task、读取缓存或改写 vendor；格式化不会改变 AST、诊断语义、ABI 或 package checksum。

- [x] **阶段 10：实现 cfg、模块树与定义收集**（复杂度：3）
  - 依赖：阶段 04、08。
  - 实现 host/target cfg 求值、配置裁项、模块声明表、可见性、`use/pub use`、保留名称、稳定 `DefId` 分配和命名空间冲突诊断。
  - 验收：被 cfg 裁掉的项不进入名称解析、类型检查和 codegen；导入循环、私有跨模块导入、大小写路径不一致和重复声明均可确定重现。

- [x] **阶段 11：实现 query 状态机与内容寻址缓存**（复杂度：4）
  - 依赖：阶段 03、05、06、10。
  - 实现 query 注册表、`Uncomputed/Computing/Complete/Failed/Cancelled` 状态、依赖 fingerprint、结果 fingerprint、BLAKE3 对象、原子发布、损坏校验和确定性依赖排序。
  - 验收：并发请求同一 query 只计算一次；失败不写持久缓存；缓存对象按长度/哈希/版本/IR verifier 校验后才反序列化；上游结果未变时不会传播无意义失效。

## 二、类型与语言语义

- [x] **阶段 12：实现类型 arena、类型形成与布局基础**（复杂度：4）
  - 依赖：阶段 08、10、11。
  - 实现标量、引用、原始指针、函数、元组、数组、切片、struct/enum/union/newtype、never、透明别名、`repr` 属性、大小/对齐/字段偏移和递归大小检查。
  - 验收：类型变量必须唯一收敛；数组长度、对齐、判别值和 offset 在正确阶段确定；`!` 与 `()`、别名与 newtype、句柄与值类型的区分符合规范；类型基础由阶段 12a 接入后才算完成。

- [x] **阶段 12a：接入类型形成与布局 query**（复杂度：3）
  - 依赖：阶段 11、12。
  - 将类型 arena、形成器和布局计算接入 `configure -> collect -> resolve -> type_check` 查询链，建立版本化 `TypeRef/Layout`、fingerprint、缓存恢复、统一诊断传播及 GIR/comptime 的消费接口。
  - 验收：`check/build` 对真实 package 执行类型 query；失败不写 target；缓存命中与冷编译输出一致；下游只消费已验证结果，不重新解析 token 或名称。

- [x] **阶段 12b：建立前端语义链集成门禁**（复杂度：4）
  - 依赖：阶段 12a、13–20。
  - 将声明、表达式、模式、trait、unsafe 检查和 HIR 冻结统一接入 query/action graph，定义 `Validated` 输出、失败传播、缓存边界及 GIR 消费契约。
  - 验收：真实 `check/build` 从源码运行到冻结 HIR；任何前端失败均无下游产物；冷编译与缓存命中结果、诊断排序和 fingerprint 一致；GIR 不接受未验证 AST。
  - 接入证据：`frontend::bootstrap -> TypeCheck(schema 6) -> 布局检查 -> LowerHir(v1) -> Validated::freeze -> BuildIr/ImagePlan`；backend 只接受冻结凭据，Compilation 成功必须持有 HIR。冷/热 query 与输入文件顺序回归验证相同 HIR、指纹和稳定诊断；真实 CLI 的 check/build 对未被调用的非法函数同样拒绝，退出码 1、镜像计划为空。

- [x] **阶段 13：实现声明、绑定与初始化数据流**（复杂度：3）
  - 依赖：阶段 10、12。
  - 实现 `let`、模式绑定、遮蔽、新槽、函数/结构体/枚举/const/type/static 声明、普通 static 无环初始化、局部 static 延迟初始化标记和所有路径初始化分析。
  - 验收：未初始化读取、模块级 let、static 初始化循环、非法 main 签名和私有字段构造均被拒绝；普通 static 与 coroutine-local/OS-thread-local 的初始化阶段严格分离。
- [x] **阶段 14：实现表达式、运算符与控制流类型检查**（复杂度：4）
  - 依赖：阶段 12、12a、13。
  - 覆盖 place/value、字段/索引/切片、短路逻辑、整数/浮点规则、显式转换、循环、`if`/`match`/`try` 表达式、返回/分支和 `defer` 注册语义。
  - 验收：左到右求值、块值、never 合流、除零/移位/边界规则和 `?` 出口与规范一致；结果接入类型 query、统一诊断和 action graph；用户 trait 运算符不会通过隐式转换获得额外候选。
- [x] **阶段 15：实现模式匹配与穷尽性分析**（复杂度：3）
  - 依赖：阶段 12、12a、14。
  - 实现通配/绑定/引用/字面量/范围/元组/数组切片/结构体/构造器/or/`@`/rest 模式、可驳性、let 链、let-else、守卫与有限域覆盖计算。
  - 验收：被匹配表达式只求值一次；重复绑定、or 绑定集合不一致、空范围、不可驳 let 段、非穷尽 match 和错误类型守卫都有稳定诊断；模式不调用用户 Eq/Ord。
  - 接入证据：声明/表达式/模式结果由 `TypeCheck query` 与 `CheckedSemantics verifier` 交接；源码、cfg 和解析结果进入语义指纹，诊断在缓存命中时重绑定当前源码表。`frontend::semantics::tests` 覆盖声明/初始化、短路与退出、检查计划、模式覆盖及冷/热 query；真实 CLI `check/build` 已验证成功检查、结构化错误和失败无产物。完整 HIR 冻结现已由阶段 12b、20 接入同一链。

- [x] **阶段 16：实现函数、闭包与 async 捕获**（复杂度：4）
  - 依赖：阶段 13、14、15。
  - 实现具名函数、闭包函数字面量、一等函数、函数项擦除、参数包、捕获槽、递归捕获、`async` 新协程语义和 `Join[T]` 类型形成。
  - 验收：闭包捕获共享正确槽并延长寿命；遮蔽不改变旧捕获；普通函数无 await 染色；捕获、返回、存储和跨 suspend 不制造悬空引用或借用错误。
  - 接入证据：`CheckedSemantics` schema 2 携带函数项泛型实例身份、捕获槽及存储需求、调用前置条件和参数包计划，经 verifier 进入布局、IR 与镜像计划指纹。`callable_tests` 验证递归/互递归、共享与遮蔽、返回/defer/async 边界、Fn 约束和参数包；128 项工作区测试通过，Linux/Windows CLI `check` 均接受真实函数切片，错误 `build` 输出稳定诊断且无产物。物理环境分配、协程执行和机器码仍由后续 GIR/runtime/backend 阶段实现。

- [x] **阶段 17：实现 trait、impl、UFCS 与特化选择**（复杂度：5）
  - 依赖：阶段 12、14、16。
  - 实现 trait 方法/关联类型/关联常量、固有 impl 归属、trait impl 完整性、方法自动解引用、UFCS、操作符 trait、否定 impl 和闭世界最具体特化。
  - 验收：固有方法优先、trait 候选唯一、重叠特化部分序无歧义；交叉重叠、缺项、错误关联类型、外模块固有 impl 和 `forbid` 相关约束均正确诊断。
  - 接入证据：`CheckedSemantics` schema 3 保存静态方法/操作符派发、impl 与 trait 身份、接收者调整和关联投影；完整性、模块归属、泛型证据、负实现和特化部分序均由同一接口表检查。150 项工作区测试通过，覆盖冷/热 query 中实际选中的方法、跨模块泛型路径、关联项循环、用户 `Try`、`Index` 与泛型迭代协议；Linux/Windows CLI 接受真实接口切片，缺少必需方法的 `build` 返回 E0040 并停止在镜像计划形成之前。完整 EarlyConst 与单态化实例闭合仍由阶段 21、24 验收。

- [x] **阶段 18：实现 impl Trait、dyn Trait 与 Any 前端**（复杂度：4）
  - 依赖：阶段 17、12。
  - 实现 APIT/RPIT/TAIT 隐藏类型、对象安全判断、胖函数/胖 trait 表示、`dyn Any` 擦除以及 `is/downcast/downcast_copy` 的静态类型检查。
  - 验收：`impl Trait` 保持单态化；不安全对象 trait 不能形成 dyn；Any 只能恢复放入容器的具体类型；不得出现名为 `any` 的渐进类型或跨 dyn Trait 猜测。
  - 接入证据：`CheckedSemantics` schema 4 保存匿名泛型、不透明声明与唯一隐藏类型、擦除边界、动态派发和精确恢复目标；同一类型模型进入布局与后续计划。165 项工作区测试通过，覆盖独立 APIT、跨模块 RPIT、关联 TAIT、Fn 与 IntoIter 约束传递、嵌套 Self 对象安全、Any payload 身份和缓存 verifier；Linux/Windows CLI 接受真实接口与函数切片，非法 dyn 的 `build` 返回 E0038 且没有镜像计划。TypeId 在本阶段保留符号类型，稠密编号、物理容器及 vtable 由阶段 25 及后续 lowering 物化。

- [x] **阶段 19：实现 unsafe、原始指针、union 与 intrinsic 检查**（复杂度：4）
  - 依赖：阶段 12、14、17。
  - 实现 unsafe 边界、`MaybeUninit`、`transmute`、volatile/unaligned 访问、`unreachable`、原始指针有效位模式、`asm/global_asm` 语法约束、链接属性和 C ABI 签名可表示性检查。
  - 验收：安全代码不能越过 unsafe 前置条件；资源/COW 类型不能被位操作绕过；union 只接受位类型；Windows `i128/u128` C 签名、naked、dirty/leaf/bridge 属性按规范拒绝或接受。
  - 接入证据：`CheckedSemantics` schema 5 保存内存原语、引用投影、外部调用效应、链接属性和汇编计划；类型形成后的布局校验统一检查按位管理边界、重解释大小、packed 自然对齐、寄存器宽度和双目标 C ABI。188 项工作区测试通过，包含 unsafe 函数项与动态方法、MaybeUninit、union、转换、leaf/dirty/bridge 优先级、native-only 限制、managed asm 有限 CFG 及条件清理捕获回归。Linux/Windows 真实 CLI 均接受综合输入；非法 packed 引用的 build 返回 E0038，镜像计划为空。实际外部桥接与机器编码仍分别由阶段 58、52 完成。

- [x] **阶段 20：构造 AST/HIR 结构与冻结校验**（复杂度：5）
  - 依赖：阶段 10、12–19、12a、12b。
  - 实现 `SourceSnapshot -> AST -> configure -> collect -> resolve -> type_check -> HIR` 的固定阶段、owner arena、Res、类型/调整表、捕获计划、cleanup plan、诊断排序和 Validated 冻结接口；为源码宏生成节点保留 expansion source context。
  - 验收：`check/build` action graph 真实执行完整前端链并缓存结果；基础 HIR 结构与冻结校验可独立运行；源码宏在阶段 22 生成的片段重新进入同一前端并最终满足相同 Validated 条件；GIR 只能消费冻结 HIR，不能重新解析 token 或名称。
  - 接入证据：独立 HIR 包含真实函数/闭包/async/static/全局汇编 owner、声明与类型表、调整前后类型、派发、捕获、清理链、格式计数和展开上下文；旧 `main -> ReturnUnit` 路径已删除。verifier 拒绝越界类型/字段、错误捕获归属和缺失清理链。195 项工作区测试通过，构建零 warning；Linux build 与 Windows check 接受同时包含 Any、闭包/async、局部 static、切片、intrinsic、asm、FFI 和动态格式捕获的真实输入，报告 6 个实际 callable owner。源码宏执行、GIR 和机器码仍在各自后续阶段，本阶段只冻结其唯一前端输入。

- [x] **阶段 21：实现 EarlyConst 与 capability registry**（复杂度：5）
  - 依赖：阶段 11、12、20。
  - 实现早期常量、数组长度、布局参数、泛型参数和 comptime 脚本解释器；登记允许的 lang item/intrinsic/std 能力、效果、显式输入和 evaluator revision。
  - 验收：不在 registry 或执行域未授权的调用在求值前失败；comptime 使用确定性堆、fuel、panic 和资源边界；运行时副作用、未登记文件/网络/进程访问不会被 evaluator 偷渡。
  - 接入证据：`EvaluateEarlyComptime`（schema 1）先于类型检查运行于 `Model -> evaluate -> TypeCheck(schema 7) -> LowerHir` 链，输入指纹与依赖记录包含封闭 registry 摘要；产出按 `(module,item,expr)` 规范排序的 `EarlyConstTable`，checker 的数组长度/范围端点与 HIR 的 Repeat/范围降级优先消费表内结果，缺失位置回退到共享 fuel 的惰性求值。registry 以解析后规范路径登记 `spec/standard-library.md` 能力组（域位掩码、效果、显式输入、结果种类、evaluator revision），`std.io.*` 等未登记路径与 `std.syntax.parse_*` 错域调用在求值前返回 `E0045`；`ConstEvalState` 携带 fuel（默认 100 万步）、4 MiB 确定性 heap 账本与深度上限（`E0046`），`panic` 产生 `E0047`。受限解释器覆盖标量运算、固定形状聚合、局部绑定、控制流、match 与用户函数调用，并沿调用链传递 capability 检查；`comptime` 块强制立即求值，comptime 值参数在调用点要求早期常量。registry 摘要经 `ActionInputs::set_comptime_registry` 进入前端 action key（`Compilation::action_key`，源码/cfg/registry 敏感且确定）。205 项工作区测试通过（新增 10 项），fmt/build 零警告；Linux CLI 真实 `check`/`build` 接受含常量数组、comptime 块、comptime 参数与用户函数求值的 package，未登记能力、错域、panic 与运行时实参均退出码 1 且无镜像产物。`SourceExpand`/`LateConst` 域条目已登记但调用方由阶段 22/25 引入；符号化数组长度与实例物化由阶段 24/25 消费 EarlyConstTable 验收。
  - 审查修复：evaluator revision 3 隔离函数与常量词法帧、共享字面量模式解码、以结构化出口传播控制流，并在聚合深复制与字符串增长前扣除完整 heap 负载。

- [x] **阶段 22：实现 `comptime source` 与源码宏展开**（复杂度：5）
  - 依赖：阶段 08、10、20、21。
  - 实现 `ParsedSource` 不透明值、`std.syntax.parse_*`、item/statement/expression/type/pattern source slot、轮次闭包、ExpansionId/source map、递归与展开预算。
  - 验收：生成源码必须重新经过 cfg、收集、解析、名称、类型、trait、unsafe、ABI 和 HIR；片段类别不匹配、cycle、fuel/字节/节点/深度超限均保留完整展开链诊断。
  - 接入证据：`frontend::expand::run` 在 `parse_modules -> 展开 -> entry/names/semantics::check` 链上驱动轮次闭包：每轮以冻结的名称视图在 SourceExpand 域求值脚本，经 `ParseSource`(21)/`ExpandSourceMacro`(22) query（schema 1）执行解析闸门与脚本求值，随后注册生成快照与展开记录、以非零 `ExpansionId` 解析进宿主 arena、片段 cfg 裁项、原位拼接并修正列表范围；全部宏展开后 `names::analyze`/`TypeCheck`/`LowerHir` 在合并 AST 上重新运行。`Ok`/`Err`/`?` 与结果模式进入受限解释器（仅 SourceExpand 域），`Err` 到边界转 `E0051`；类别不匹配为 `E0050`、自再生循环为 `E0048`（脚本文本+slot 稳定键）、六项预算超限为 `E0049`（含 `expansion_limit` 属性校验，默认深度 16/硬上限 256）；生成代码诊断按规范重锚定为宏调用点主位置+展开链附注。宏预算编码与全部生成文本摘要经 `ActionInputs::macro_budget/macro_inputs` 进入 action key。230 项工作区测试通过（新增 24 项，覆盖五个 slot、嵌套两轮父链、生成 cfg 裁项、边界 Err、`?` 传播、类别不匹配、cycle、深度/属性超限、冷热 query 一致、action key 敏感性与双目标 smoke）；Linux CLI 真实 `check`/`build` 接受含宏 package，非法宏退出码 1 且无镜像产物。
  - 审查修复：ExpandSourceMacro schema 2 纳入当轮完整源码与调用位置，缓存资源用量并在 query 外扣除 action 总量；所有 slot 尊重 cfg 活动位，生成子树校验并继承深度预算。

- [x] **阶段 23：实现 AbstractAnalysis、范围证明与效果传播**（复杂度：5）
  - 依赖：阶段 14、20、21、22。
  - 实现 CFG 固定点、范围/符号关系、初始化、别名类、memory version、COW seal、resource publish、并发/FFI unknown、widening/narrowing 和跨函数摘要。
  - 验收：只有 `proved` 才能删除边界检查或生成更强 placement；未知调用、别名、并发和预算耗尽保留原检查；分析不会替代类型推断或 impl 选择。
  - 接入证据：`LowerHir` 冻结前从 HIR 构造显式 CFG，传播完整 `AbstractState`（区间、差约束、初始化、别名、memory version、效果），循环 header widening 后一轮 narrowing；只有支配检查点的 `Proved` 写入 `RuntimeCheck.proof`，不删除 HIR 检查节点。`[T; N]`/`&[T]` 固有 `len` 降为 `Builtin::Len`。`WholeProgramAnalysis` schema 2，嵌套 `AnalysisSccSummary`(27)/`FunctionAnalysisSummary`(23)（身份为 `AnalysisOwnerKey`，SCC 内不经 query 读半初始化摘要）；`PublicFunctionSummary` 仍为空 map。规范切片 `v.len() > 10` + `for i in 0..n` + `break` 下标 `Proved`；写入/FFI/spawn/预算耗尽保留 `Unknown`。`analysis_semantics_revision = 2`，块迭代预算进入 policy 字节与 action key。249 项工作区测试通过（含字面量回归、归纳变量、len 收窄、失效、冷热一致与嵌套 query 投影）。
  - 审查修复：analysis semantics revision 4 修正引用写入后标量失效、复合赋值、整数回绕及空切片证明；统一状态重放，有限版本格支持真正 narrowing，真实返回出口范围与纯函数长度关系贯通实例摘要和调用点。
  - 审查验收：新增 17 项确定性回归，316 项工作区测试全部通过；fmt、build 零 warning，mdBook 构建成功。CLI 在 Linux build 与 Windows check 接受覆盖三个阶段的真实输入；嵌套 `?` 失败返回 E0051，未执行后续 panic，镜像计划为空。

- [x] **阶段 24：实现闭世界可达性与单态化实例图**（复杂度：5）
  - 依赖：阶段 17、18、20、22、23。
  - 实现入口、runtime/std、测试/export/used、comptime 和 late closure 根；实现 `StableTypeKey`、`MonoKey`、SCC 实例闭合、公共函数摘要和每实例 code fragment。
  - 验收：删除项不进入实例图；递归泛型和 impl 选择在闭合后稳定；并行单态化结果按稳定 key 排序，跨 package 摘要不暴露 private state 或 session-local ID。
  - 接入证据：`frontend::mono` 在 `LowerHir` compute 内、冻结前执行 `CollectMonoRoots`(12, schema 2) -> `InstantiateGir`(13, schema 2) -> 闭合 driver -> `WholeProgramAnalysis`(24, schema 4) -> `PublicFunctionSummary`(28, schema 2) -> proof 写回 -> freeze。`StableTypeKey`/`MonoKey` 按 GBC1 编码，实例键包含类型摘要、规范 comptime 实参、选中 impl、真实调用 ABI 与 target/harness；interner 验证摘要对应的规范字节。根覆盖入口/used/export/static/global asm/harness；late 依赖从可达具体实例收集。实例边按 HIR owner 隔离函数值、闭包、协程和局部 static，并覆盖运算符及协议调用；每个具体接收者保留独立 vtable。泛型绑定复用语义检查结果，在具体接收者上重新选择 impl，保留方法泛型、APIT、参数 pack 与 callable 身份。driver 按 key 摘要排序闭合；同 key 递归形成图环，严格增长或 ancestry 超限报 E0052，总数超限报 E0053。分析 query 23/27（schema 3）按实例 SCC 求解，调用点只消费选中实例摘要；共享 HIR 的检查对全部实例取共同证明。公共摘要分离签名/ABI 与 body 指纹，保留参数序号和隐藏状态效果，生产者变化重算但内容相同时对象 key 不变；经 `ActionInputs::add_public_summary` 进入 action key。实例、元数据、vtable、外部符号和片段输入全部进入闭世界指纹；`ImagePlan` 报告实例数、根数和图指纹。当前交付每实例 fragment 输入与依赖，机器码 payload 在阶段 52、GIR 操作树在阶段 26、摘要磁盘持久化在阶段 71 接入；runtime/std 源树仍为 bootstrap 空单元，lang item 根为空。审查回归覆盖缓存失效、泛型特化、嵌套 owner、函数值、comptime 实参、双 vtable、公共效果、共享证明及严格增长拒绝；真实 CLI `check`/`build` 验证闭合计划，不把尚未实现的镜像写出标为成功。

- [x] **阶段 25：实现 LateConst、FreezeTypeUniverse 与 TypeId**（复杂度：4）
  - 依赖：阶段 18、24。
  - 实现具体类型集合冻结、按 `StableTypeKey` 分配稠密 TypeId、`type_id_count()`、late 结果表、类型名、descriptor/vtable 引用和禁止反向影响类型形成的阶段约束。
  - 验收：所有拥有 TypeId 的具体类型恰有一个编号；`!`/MaybeUninit 无编号；late 值不能改变可达性、布局、impl、宏或调用图；编号不作为跨镜像或 C ABI 稳定密钥。
  - 接入证据：`LowerHir` schema 2 在 HIR verifier 与实例图闭合后执行 `FreezeTypeUniverse`（25，schema 1）-> `EvaluateLateComptime`（26，schema 1）-> 全程序分析 -> HIR freeze。实例 schema 3 递归保存签名、局部、泛型、字段与反射类型，按完整 StableTypeKey 摘要排序分配稠密编号；透明与不透明别名归一到具体身份，`!`/MaybeUninit 不占编号，vtable payload 引用由同一类型表校验。早期 TypeId 相等性/名称可用于宏；late HIR evaluator 使用冻结实例调用目标和具体类型绑定，支持固定形状聚合、局部更新、循环、静态调用和标量计算，结果绑定 universe/闭包指纹。E0054 阻断传递 late 值进入数组长度、泛型参数与宏；panic、预算和非法发布结果停止编译。类型表与结果表接入 action key、分析输入及 CLI 镜像计划。299 项工作区测试全部通过（阶段 25 新增 12 项），fmt/build 零 warning，mdBook 构建通过；Linux build 的真实输入报告 8 个 TypeId/3 条后期结果，Windows check 的泛型与隐藏类型切片报告 10 个 TypeId/6 条结果，非法数组长度 build 返回 E0054、退出码 1、image-plan 为 null。GIR 操作树和物理 GC 类型 section 分别仍由阶段 26、39 交付，未声称已写出机器镜像。

- [x] **阶段 26：实现 GIR 语义 lowering 与 cleanup CFG**（复杂度：5）
  - 依赖：阶段 20、24、25。
  - 实现 place/operand/rvalue、显式 CFG、值描述符动作、COW/resource 动作、suspend、panic、`ScopedViewBegin/End`、`NoSafepointRegion` 和 HIR CleanupPlan 到 cleanup block 的 lowering。
  - 验收：return、break、continue、`?`、panic 和正常出口的动作序列与 HIR 一致；scoped view 无逃逸、无 suspend、每条出口恰有一次 end；NoSafepointRegion 只能由登记 intrinsic 产生。
  - 接入证据：`BuildGenericGir`（11，schema 1）为每个冻结 HIR owner 构造 generic body，经结构/前驱/storage/cleanup/cancelled/scoped view/`NoSafepoint` verifier 后写入 `GirWorldV1`；`FrontendOutput`/`BuildIr`/`ImagePlan`/`ActionInputs` 消费 body/block/语句计数与 GIR 指纹，`-Zdump-gir` 输出稳定 dump，差异为 E0055。cleanup 序列与 HIR `CleanupPlan` 对齐（`return_defer`/`loop_break`/`try_question` fixtures）；`cancelled` 仅 `ChanSend` 与含 send 的 `SelectCommit`。`LowerHir` schema 5 只构造、校验并冻结；管线为 `gir → mono → late → attach_fragments → WholeProgramAnalysis`（24，schema 5，`analysis_semantics_revision = 5`）→ `PublicFunctionSummary`。分析在 generic GIR 上求固定点，证明只写入 `AnalysisWorldV1.proofs`，调用效果以上界不再覆盖选中实例摘要，unwind 边只在可能 panic 时传播。`InstantiateGir` 仍从 HIR 收集调用边。331 项工作区测试全部通过，fmt/build 零 warning，mdBook 构建通过。单态化 GIR 替换、LIR 与机器码分别仍由阶段 27/28/52 交付。

- [x] **阶段 27：实现值传递、COW 与 placement 分析 lowering**（复杂度：4）
  - 依赖：阶段 23、26。
  - 实现位值浅拷贝、身份句柄共享、string/ByteBuffer seal、resource lease、`f(x)`/`f(&x)`、大拷贝 lint、escape 与 TurnRegion/LocalHeap/SharedHeap placement 选择。
  - 验收：传递不产生 move/borrow 门槛；句柄身份、COW 独立值和 ResourceCell 一次性 release 语义在赋值/返回/聚合/Any 中一致；分析未知时保留安全通用路径。
  - 接入证据：`BuildGenericGir` schema 2 在构造期按传递类别展开 `ValueAction`/`CowSnapshot`/`ResourceAction`；调用实参先语义拷再 `MoveInternal`，`dyn Any` 擦除先 seal。`large_copy` 为 `E0056`（默认 warn，`deny`/`forbid` 使 Frontend 失败且无镜像）。管线为 `gir → mono → late → attach_fragments → WholeProgramAnalysis`（24，schema 5，`analysis_semantics_revision = 5`）→ `EscapeAndPlacement`（29，schema 1）→ `PublicFunctionSummary`。`GirWorldV1` schema 2 携带 `PlacementWorldV1`；分析用放置前指纹。未知/逃逸/发布不选 `TurnRegion`。纯位 `ValueAction` 不破坏范围证明。`ImagePlan`/`ActionInputs`/`-Zdump-gir` 消费 placement 计数与指纹。338 项工作区测试全部通过（阶段 27 新增 7 项），fmt/build 零 warning，mdBook 构建通过。Linux CLI `check`/`build` 接受含 `f(x)` 后再用、string COW 与 placement 的真实输入，JSON `image-plan` 含 placement 字段；`#![deny(large_copy)]` 对 `[uint; 9]` 退出码 1 且 `image-plan` 为 null。单态化 GIR 替换、LIR 与堆装箱改写分别仍由阶段 28/29 交付。

- [x] **阶段 28：实现 LIR SSA、memory SSA 与 verifier**（复杂度：5）
  - 依赖：阶段 26、27。
  - 实现 block parameter SSA、Mem token、封闭 LIR 指令集、provenance、调用/原子/volatile/屏障/safepoint effect、source scope 和结构 verifier。
  - 验收：每个可能内存操作都有正确 Mem 链；合流参数顺序确定；禁止悬空引用、越权 pointer provenance、非法 memory order、未闭合 region、缺失 barrier 和不合法 terminator。
  - 接入证据：`BuildConcreteGir`（schema 1）把每个实例的操作树绑定到具体布局表、闭合实例键/动态槽与协议展开；`BuildLir`（query 15，schema 2）按实例键、GIR/布局、late、placement 与目标指纹缓存，构造结果与缓存恢复都经同一 `verify`，失败为 `E0057`。`Compilation` 暴露 `lir`/`dump_lir`/`lir_fingerprint`，`ActionInputs` 收录 `lir` 指纹，backend 与 `image-plan` JSON 消费 body/block/指令/内存操作/safepoint 计数。修正了 LIR 暴露的归属层缺口：变参尾物化、内建 `Clone` 与算术 impl 展开、union 聚合、`?` 载荷提取、static/const 初始化返回类型、接收者自动借用/解引用、string 拼接与 `TypeId.name()` 运行期名称。348 项工作区测试全部通过（阶段 28 新增 10 项，含 9 项非法 LIR 拒绝用例），fmt/build 零 warning，mdBook 构建通过；Linux CLI `check`/`build` 接受含变参、迭代器、用户 `Try`/`Index`、union、裸指针构造引用与泛型算术的真实输入，`-Zdump-lir` 打印 Mem 链、provenance、scope 与 source。

- [x] **阶段 29：实现固定优化管线与 poll budget**（复杂度：5）
  - 依赖：阶段 23、28。
  - 按 GIR/LIR 规范实现常量/复制传播、CFG、边界消除、循环、逃逸、placement、COW、defer、vectorizer、poll placement、NoSafepoint 和 barrier reserve pass。
  - 验收：pass 顺序不可任意交换；无限 managed 路径满足 safepoint 与 cost budget；优化不得移动用户可观察求值、cleanup、同步、GC barrier 或 FFI effect；所有 verifier 在每个 pass 后运行。
  - 接入证据：`gir::pass` 与 `lir::pass` 各驱动一条固定顺序管线，枚举顺序即执行顺序、禁止运行时重排，每跑一个 pass 立即运行结构 verifier（GIR `E0055`、LIR `E0057`）。GIR 侧实现 `Inline`、`SimplifyCfg`、`SparseConditionalConstants`、`CopyPropagationAndGvn`、`BoundsCheckElimination`、`CowAndResourceElision`；LIR 侧实现 CFG 规范化、常量与代数化简、GVN、死存储/死值消除、循环规范化与 LICM、强度削减、版本化与反开关、展开与向量化、屏障预留、ABI 校验、poll 分类与预算化插点、寄存器分配准备。`PollSummary`、`BackendCostProfile`、`OptimizationPolicyV1` 与各 revision 进入前端 action key 与 `image-plan`；`-Zdump-gir` 打印 `gir-passes`/`inline-count`/`checks-elided`，`-Zdump-lir` 打印 `poll-summary` 与逐指令 `poll-cost`。计数循环按 strip mining 拆成「外层每次 poll、内层 poll-free」，无限与不可数循环按一次最大 cycle cost 计算 interval，总成本不超预算的计数循环保持 poll-free；可证明有界的 counted 环由 verifier 复核。测试覆盖固定 pass 顺序、可观察效果不变、代数化简保持环绕与除法、无限循环插点、strip mining 内层 poll-free、超预算 poll-free 环被拒、策略指纹敏感与内联展开；工作区测试全部通过，fmt/build 零 warning。

## 四、runtime 内存、调度与 GC

- [x] **阶段 30：实现 raw slab/span 与 owner-directed return**（复杂度：4）
  - 依赖：阶段 01、11（使用阶段 01 提供的可替换 fake range provider）。
  - 实现稳定 `OwnerRecord/OwnerToken`、`SlabDescriptor`、dense size class、本地 free path、producer staging、ReturnMessage、8 shard owner inbox、MPSC batch 发布和 exactly-once return。
  - 验收：跨 owner 只发送 descriptor/index/generation/epoch/bytes/integrity，不发送 managed 裸地址；MPSC 交错、远程批量、generation mismatch、owner retire 和链完整性均有确定性测试。
  - 接入证据：`RuntimeRawModel`（query 30，schema 1）在 `BuildIr` 内、runtime 资源附加前构建 `RuntimeRawContractV1`：目标语义、tuning profile、7 项 dense class 阶梯（64…4096，64 KiB span）、12 字段消息 schema（地址种类在 verifier 中被拒）、queue-page grace 步骤、四类互斥账本与由 GIR 协程创建点、placement 记录推导的需求视图；query registry 登记 revision 提升到 2，`registry_fingerprint` 与契约指纹经 `ActionInputs` 进入 action key，`ImagePlan` 报告 dense class 数、shard 数、batch item/byte 上限、常驻 node 容量与契约指纹，`-Zdump-runtime` 与 `runtime-dump` 事件输出稳定 dump。参照实现覆盖本地四步分配（free list → span bump → domain cache → typed range request，前两步零平台调用并由 provider 统计断言）、`ReturnQueued` 唯一状态迁移与 double return 拒绝、encoded link（空链/校验位/对齐/过期 generation/外来 slab 分类）、producer staging 的 item/byte 双上限与六类触发、8 shard 四步发布与 phantom-null 语义、source slab 聚合的 victim/close、旧 token 转发、queue-page grace 与 owner retire、账本互斥分类；LIR 层 publish 区域契约 verifier 先于通用指令校验运行，违规诊断为 `E0058`。工作区 395 项测试通过（本阶段新增 27 项：raw 平面参照实现 22 项、publish 区域契约 3 项、镜像计划端到端 2 项），构建零 warning，mdBook 构建通过；`cargo bench -p gugu-compiler --bench owner_return` 以真实多 producer 压 MPSC 批量发布，exactly-once 与账本不变量保持；Linux CLI 真实输入的 `image-plan` JSON 含 raw 字段，目标变化使契约指纹变化。PlatformRange 的 guard/wait/wake/entropy 与 debt/pacing、`ResourceCell` release、`CoroutineSlot`/stack class 尺寸分别由阶段 31/32/34/41 交付，Gugu runtime 源实现随 rt0 与调度阶段落地并复用同一 schema。
  - 审查修复：带 tag 的 node free stack 与独立 `free_next` 车道消除 ABA；head 读取与 CAS 之间的窗口改为重试而非不变量失败；message node 只在 queue-page grace 之后复用，避免 consumer 的 front/last 记账指向被重新写入的 node。

- [x] **阶段 31：实现 ResourceCell 与自适应资源租约**（复杂度：4）
  - 依赖：阶段 27、30。
  - 实现地址稳定 ResourceCell slab、open/closed 状态、lease 复制、受限 cleanup、发布前后状态迁移、File/socket/process/lock/FFI resource 的统一 release 入口。
  - 验收：复制资源值不会重复关闭；最后 lease release 不执行用户代码；异常、panic、detach、close、进程终止和共享发布保持规范的一次性语义；资源值禁止进入错误的 arena reset。
  - 接入证据：`RuntimeRawModel`（query 30）升到 schema 2，在同一 `RuntimeRawContractV1` 中并入 Resource domain 的 64-byte header class 阶梯、`SHARED`/`CLOSED`/`RELEASE_QUEUED`/`RELEASE_DONE`/`RECLAIMING` 状态位与迁移表、受限 release 描述符 schema（禁止 managed payload、capture、分配、panic、取锁、等待 channel 与 spawn）、File/socket/process/lock/FFI 种类目录与唯一 release 入口，以及 `RawResourceDemand`。`runtime/resource.rs` 提供确定性参照实现：lease 复制只增计数，close 与最后 lease 竞争唯一入队点，受限 cleanup 只记录 ticket 与计数，`leases == 0 && RELEASE_DONE` 的一方才能推进 generation 并归还 slot；超过 4096 byte payload 或 64 对齐时走独立 non-moving 整页 mapping。`world/resource_impl.rs` 把资源分配、发布、close、lease 结束与 panic/detach/shutdown 接入 owner 上下文，跨 owner 回收只发布携带 descriptor/unit/generation/bytes 的 `ReturnKind::ResourceRelease` 消息并经 owner service 与 queue-page grace 归还。LIR 世界 verifier 新增资源隔离闸门，resource 描述符进入 `RegionAlloc` 即 `E0059`（退出码 101、无镜像计划）；参照模型同时拒绝 resource class descriptor 进入整区 reset。`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `raw-resource-cell-header-bytes`、`raw-resource-class-count`、`raw-resource-kind-count`、`raw-release-descriptor-count`、`raw-resource-sites` 与 `raw-release-sites`，契约指纹进入 action key。当前工作区 423 项测试全部通过（阶段 31 原有 416 项，本轮审查新增 7 项：GIR 资源覆盖 1 项、ResourceSchema 负向 1 项、runtime 生命周期与回滚 5 项），fmt/build 零 warning，mdBook 构建通过；`cargo bench -p gugu-compiler --bench resource_release` 以真实多 producer 发布、单 owner 消费验证已构造 release 的 exactly-once cleanup、slot return 与账本守恒；完整 release request 交错由 runtime 确定性测试覆盖。Linux CLI 对真实 `struct ResourceCell` 源切片的 `build --format json -Zdump-runtime` 报告 schema 2 契约、resource class/种类/入口与镜像字段，退出码 0。Gugu runtime 侧的等价实现随 rt0 与调度阶段复用同一 schema。

- [x] **阶段 32：实现 PlatformRange、extent 与内存账本**（复杂度：3）
  - 依赖：阶段 30。
  - 实现 reserve/commit/decommit/release/guard/wait/wake、2 的幂 extent、range metadata、huge-page hint、zero/entropy/dump policy 和 committed/reserved/pending/cache/reclaimable 分类。
  - 验收：decommit 只在 allocator/scanner/forwarder lease 和 grace 结束后发生；Linux fake platform 与 Windows fake platform 的错误映射一致；内存压力统计不重复计数。
  - 接入证据：`RuntimeRawModel`（query 30）升到 schema 3，在同一 `RuntimeRawContractV1` 中并入 `PlatformRangeSchemaV1`（13 项固定操作目录与 mutating/blocking 分类、`Reserved`/`Committed`/`Decommitted`/`Released` 状态迁移与按 commit 拆分的成本规则、10 级二次幂 extent class 阶梯、`PlatformRangeDemand`）与 `LedgerSchemaV1`（physical/virtual 两个 plane 的互斥分类与 residual 位置），失败映射按 `(profile, error)` 排序进入契约编码，verifier 拒绝任何 profile 之间的漂移。`runtime/provider.rs` 提供确定性参照实现：range 描述符、页粒度 `CommitBitmap`、guard、wait/wake 字、entropy 与 dump policy，`ProviderStats` 的 `reserved_bytes` 是当前未提交虚拟字节，累计量拆为 `reserved_total`/`committed_total`/`decommitted_total`；`runtime/platform.rs` 固定 Linux/Windows 的 profile 常量、huge-page 尺寸与单一 `fault_class` 比较点。`runtime/extent.rs` 实现 buddy 阶梯：每个 owner 在 raw 与 Resource 两个 domain 上各持有一个 2 MiB arena，只做一次 `reserve_aligned`，块按二次幂 class 切分、按 buddy 编号低位翻转合并，描述符槽位经 free list 复用并推进 generation。`decommit` 门禁固定为四条同时成立：allocator/scanner/forwarder 三路 lease 归零、无 live/queued slot、无在途 return、queue-page grace 走完 4 步（按 epoch 累积）；`world/extent_impl.rs` 把 arena 开立、extent 取用、trim 与 dump policy 接进 owner 上下文，跨 owner 归还只发布携带 extent 编号的 `ReturnKind::Extent` 消息，线性化点在 producer 侧（lease 未归零时拒绝且不改变状态），grace 未走完时消息被消费而 extent 留在 `ReturnQueued` 并由 owner service 继续推进，同一 extent 不可能归还两次。`std.platform` 的 13 个原语经 checker（`unsafe` 门禁、无类型实参、实参形状）、HIR（按操作推导 `UNSAFE`/`FOREIGN`/`SUSPEND`/`READ`/`WRITE` 效果）、GIR（`NoSafepointRegion` 拒绝）与 LIR（arity、类型、provenance 与结果 lane）四层 verifier 接入，`wait` 是挂起点、其余为 `CallReturn`；`std/runtime/platform.gg` 由 `LoadSources` 注入源码表，非 `std` 模块导入私有实现模块按 `E0031` 拒绝，其 `#[used]` 入口使平台 adapter 成为闭世界镜像的根。`ImagePlan`/CLI JSON 报告 `platform-profile`、`platform-op-count`、`platform-range-class-count`、`platform-contract-fingerprint`、`platform-range-demand` 与 `ledger-category-count`，契约指纹进入 action key。当前工作区 450 项测试全部通过（阶段 31 原有 423 项，本轮新增 27 项：平台契约与失败映射、extent 分裂合并与槽位复用、lease/grace 门禁、跨 owner 归还 exactly-once 与二次归还拒绝、页 commit 拆分、越界 span、entropy/dump policy、契约目录与原语枚举一一对应、账本分类互斥与预留/提交不重叠，以及 5 项 IR 路径测试），fmt/build 零 warning，mdBook 构建通过；Linux CLI 对真实 `std.platform` 源切片的 `build --format json` 报告 schema 3 契约与全部镜像字段，退出码 0。Gugu runtime 侧的等价实现随 rt0 与调度阶段复用同一 schema。

- [x] **阶段 33：实现 rt0、启动配置与 fatal/报告路径**（复杂度：4）
  - 依赖：阶段 32；bridge/ABI 在阶段 58 接入。
  - 实现 Booting/Running/Waiting/Terminating 状态、环境快照、runtime 配置解析、main/Err/panic/fatal 终止计划、emergency buffer、text/NDJSON report、backtrace 收集与报告和退出类别。
  - 验收：非法配置、OOM、StackOverflow、RuntimeInvariant、ForeignUnwind、PanicDuringUnwind、HardwareFault 的边界不可被 catch；报告不调用用户代码、普通分配器或异步 I/O；输出字段与 schema 稳定。
  - 接入证据：`RuntimeRawModel`（query 30）升到 schema 4，在同一 `RuntimeRawContractV1` 中并入 `Rt0SchemaV1`：rt0 五步启动序列、四个生命周期状态与单向迁移表（`Booting→Running` on runtime-established、`Running→Waiting` on main-returned、`Running→Terminating` on natural-exit、三态→`Terminating` on termination-started）、环境快照字段（argv/env/cwd 固定一次）、7 个启动变量的文法与默认值（字节量语法含溢出/零值/未知后缀拒绝与 `STACK_MAX` 64 KiB 下界）、7 类 fatal 目录、退出类别与码规则（Linux `128+信号号`、Windows 登记非零 status）、`gugu-runtime-report-v1` 报告 schema（8 字段固定序、`panic`/`termination` 事件、5 类 class、15 个 reason）、`TerminationPlan`（mode `natural`/`immediate`/`explicit-exit`/`fatal`/`signal`，`report_epoch` 为报告冲刷数量下界）、6 项关闭设施固定顺序与 emergency buffer 策略（4096 字节定容、容量下限 256、诊断配置非法时回退固定纯文本）。`Rt0Demand`（`main` 存在性、`main` 是否返回 `Result[(), E]`，由 GIR entry body 签名推导）进入 query key 与契约编码。`startup`/`lifecycle`/`report`/`termination` 参照实现消费同一组枚举：快照固定一次、canonical 顺序收集全部配置错误并报首个错误、报告只经定容 emergency buffer 渲染（先截断 message、再从尾部丢帧、location 与 exit_code 结构预留完整、JSON 始终合法）、回溯收集为 best-effort 空列表回退（栈图在阶段 38 接入）。`world/termination_impl.rs` 把 rt0 进程模型接进 `RawWorld`：boot 完成前四步并在配置非法时以 `InvalidConfiguration` fatal 从 `Booting` 直接进入 `Terminating`（main 未调用、退出码 2、`GUGU_RUNTIME_DIAGNOSTICS`/`GUGU_BACKTRACE` 非法时使用 emergency 纯文本）；`Terminating` 中拒绝 defer 与新协程接纳、后续 fatal 被抑制计数；主协程 panic 展开窗口内的 fatal 升级为 `PanicDuringUnwind`（类别转 `runtime-failure`）；终止执行恰好一次，按「producer flush → 越过最新 epoch → 按 `wait_foreign` 等待外部工作 → poller→processor→GC→stack arena→cold slab→`CoroutineSlot` slab 固定顺序关闭设施 → 报告冲刷到 `report_epoch`」收尾并产出退出类别与码；`main` Err 先发布终止事件仍按自然规则等待其余协程，`Waiting` 中的分离 panic 把自然退出改为 `UnhandledPanic`。契约指纹以派生键 `gugu-rt0-startup-v1` 进入 action key，`ImagePlan`/CLI JSON 新增 `rt0-step-count`、`rt0-lifecycle-count`、`startup-config-var-count`、`startup-fatal-count`、`report-reason-count`、`rt0-emergency-buffer-bytes`、`rt0-contract-fingerprint` 与 `rt0-demand`，`-Zdump-runtime` 输出稳定 rt0 段。工作区 503 项测试全部通过（本阶段新增 53 项：配置解析正/负矩阵与 canonical 首错顺序、emergency 回退判定、JSON 字段序/转义/截断后仍合法、报告序号单调、回溯模式、生命周期迁移与接纳闸门、七类 fatal 计划与信号退出码、主协程 panic/Err/显式退出/分离 panic 各终止路径、升级与抑制、终止 exactly-once 与设施顺序、producer flush 账本守恒，及 3 项真实 CLI 端到端），fmt/build 零 warning，mdBook 构建通过；Linux CLI 真实 `build --format json -Zdump-runtime` 报告 schema 4 契约与全部 rt0 字段，`Result` main 改变 `rt0-demand` 与契约指纹，退出码 0。镜像内真正的 rt0 启动、报告 I/O 与宿主退出分别随阶段 56/57/58 验收，Gugu runtime 侧等价实现复用同一 schema。

- [x] **阶段 34：实现协程控制块、stack arena 与 context switch**（复杂度：5）
  - 依赖：阶段 30、32、33。
  - 实现 `CoroutineHot/Cold/Slot` 固定布局、可复制 stack arena、size class、上下界 guard、x86_64 context 保存/恢复、stack growth/retire 和完成记录。
  - 验收：`CoroutineSlot` 及 cache-line layout 满足 backend 断言；栈增长不超过逻辑上限；完成 coroutine 的 result/panic 先转移到 cold control，再安全归还旧 stack。
  - 接入证据：`RuntimeRawModel`（query 30，schema 5）内嵌 `CoroutineRuntimeContract` schema 1，固定 `CoroutineHot`/`StackDescriptor` 各 64 B、`CoroutineSlot` 128 B、`CoroutineCold` 512 B、context 48 B、morestack scratch 208 B；Rust 构建时断言与内建 `std/runtime/coroutine.gg` 的冻结 HIR/具体 GIR 布局逐字段交叉校验。栈采用 256 MiB payload arena、128 个 2 MiB span、首尾各 4 KiB guard、512 B 起的稠密二次幂 class、连续 span extent 与超过半 arena 的独立 reservation；亚页 slot 共用宿主页，首次切入才 commit，七类 owner cache 执行 64/32 KiB 迟滞，空页合并回收且不改变内部 protection。rt0 的 main/子协程使用同一控制表与 arena；增长执行逻辑上限与精确 StackInterior 重定位，收缩使用四个独立 GC 观察窗；完成先向 cold 发布 result/panic 与 barrier，再在 system-stack 交接点摘除旧 context/root，归还 stack 后 release 发布 Dead。跨 owner 完成沿既有 `StackSpan` inbox/integrity/forwarding 路径归还，Join 不保留 stack，最后 Join 在完成前消失也能回收控制块。
  - 验收证据：布局、栈策略、62-byte x86_64 switch/restore-only 片段及优化后 LIR 需求进入契约 fingerprint、action key、CLI `image-plan` 与 `-Zdump-runtime`；source fixture 的 Linux `build`、Windows `check` 均报告 2 个创建点、10 个入口检查、2 个 suspend，缺失或损坏的布局/片段被 verifier 拒绝。`cargo bench -p gugu-compiler --bench coroutine_context --profile dev` 在真实宿主 VM 上执行编译计划中的实际片段，验证独立栈往返、保存寄存器、processor 重建、单向 finish 及释放旧栈后结果 42 仍存活。519 项工作区测试全部通过，fmt/build 零 warning，mdBook 构建通过。接入同时修复裸属性被误读为 lint 参数，以及 GIR 字段/借用投影提前复制聚合的问题（`BuildGenericGir` schema 4）；全部原有 fixture 保留，证明回归按被测输入的源位置收集，移除固定内建函数数量的偶然断言。M:N 调度、完整机器栈图与最终镜像写出仍由对应后续阶段交付。

- [x] **阶段 35：实现 M:N scheduler 基础路径**（复杂度：5）
  - 依赖：阶段 34、30。
  - 实现 LogicalProcessor/WorkerThread、固定 256 LocalDeque、remote batch/injection、run_next、park/unpark、work stealing、weak fairness、producer gate 与动态 processor topology 骨架。
  - 验收：runnable 无额外 FIFO 保证但不被持续新工作永久饿死；无 `P × P` mailbox；park 的 queue recheck/work sequence 不丢唤醒；processor retire 能转移全部本地状态。
  - 接入证据：`RuntimeRawModel`（query 30，schema 6）内嵌 `SchedulerRuntimeContract` schema 1：本地队列 256、remote 分片 8、batch 上限 128、service 间隔 61、service 批量 128；`SchedulerDemand` 由优化后 LIR 统计创建点、`RuntimeCall::Yield` 与 suspend，并与协程需求对齐（`yield_sites <= suspend_points`）。契约指纹以派生键 `gugu-scheduler-runtime-v1` 进入 action key。`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `scheduler-local-capacity`、`scheduler-remote-shard-count`、`scheduler-batch-max-items`、`scheduler-service-interval`、`scheduler-service-batch`、`scheduler-contract-fingerprint` 与 `scheduler-runtime`。参照模型覆盖 Classic64/Packed55 双变体 deque、`run_next` 限幅 1、overflow 认领最旧 batch、全通道唯一 `ready_publish`、park 的 queue recheck、steal 取半上取整、processor retire 有序转移本地状态、dirty CPU 配额与 managed bound；禁止 `P × P` mailbox 与稀疏 processor `HashMap`。
  - 验收证据：`scheduler_tests` 覆盖常量与契约漂移、双 deque 语义、Packed55 `RESETTING`、run_next、overflow、Waiting/Parking 唤醒、yield 入本地尾、park 不丢唤醒、steal、retire、dirty 配额与 lifecycle generation。`cargo bench -p gugu-compiler --bench scheduler_runqueue` 以真实多 worker smoke 队列与账本，确定性正确性由单测承担。timer/poller/GC-stop 完整状态机与镜像内调度执行仍由后续阶段写出，复用同一 schema。

- [x] **阶段 36：实现 channel、Join、select 与等待源**（复杂度：5）
  - 依赖：阶段 35、阶段 16、31。
  - 实现有/无缓冲 channel、send/recv/try/close 线性化、Join 完成记录、WaitSourceId、select scratch、默认分支、随机公平和等待队列。
  - 验收：close 竞态、收尽缓冲、重复 wait、分离子协程、无缓冲会合和 select 单次求值/单次提交严格符合规范；等待只挂起协程，不占用 OS thread。
  - 接入证据：前端将 `recv()`/`wait()` 定为 `Result[T, ChanClosed]`/`Result[T, Panic]`，并形成编译器已知的 `ChanClosed`/`TrySendErr`/`TryRecvErr`/`Panic` 名义类型；`try_send`/`try_recv` 不可重载、不带 `SUSPEND`，经 HIR builtin → GIR `IntrinsicOp`（不是 Suspend）→ LIR `RuntimeCall::ChannelTrySend/ChannelTryRecv`。`chan[T](n)` 的 comptime 负容量与 `select` 内 `try_*` 在 Frontend 失败且没有镜像计划。`RuntimeRawModel` 升到 schema 7，并入 `WaitRuntimeContract` schema 1：等待源种类 channel/join/never、wait-node 字段（协程句柄、generation、case、stack-high-relative 偏移，禁止裸栈指针）、FIFO 队列、`INLINE_SELECT_CASES = 8`、scratch class `1,2,4,…,1024` words、wait-node raw class 64/128 B、`SelectTxn` 相位 Building/Armed 与 winner UNSET/DEFAULT/case。`WaitDemand` 由优化后 LIR 统计 channel/Join/select/never 与 `SafepointKind::Select`；指纹键 `gugu-wait-runtime-v1` 进入 action key。内建 `std/runtime/channel.gg` 的 `ChannelControl`/`WaitNode`/`SelectTxn`/`SelectScratchCache` 与 machine 布局交叉校验；`CoroutineCold.select_scratch` 的 4 个 `u64` 解释为 `SelectTxn` 描述符，8-word 内联区是 processor-local scratch cache 的 0 号 class。跨 owner 归还 `ReturnKind::WaitNode`。参照模型挂在 `RawWorld`：`WaitSourceId` 单调不复用，缓冲环与无缓冲会合，close 不丢已线性化缓冲，Join 完成 Release 发布后再 Dead，select ≤8 同时 `try_lock` 与失败/>8 的逐源扫描，never `select {}` 挂 never wait，大 payload 两阶段 reservation，唤醒只经 `ready_publish`。`ImagePlan`/CLI JSON/`-Zdump-runtime` 报告 `wait-inline-select-cases`、`wait-scratch-class-count`、`wait-node-class-count`、`wait-contract-fingerprint`、`wait-demand` 与 `wait-runtime`。
  - 验收证据：`wait_tests` 覆盖 close 与 send 两种线性化顺序、收尽后 Closed、重复 wait、分离 Join 不取消、无缓冲会合、`try_*` Full/Empty/Closed、ready 时 default 不获选、try_lock 回退、>8 扫描、Building 期 winner CAS、loser 注销、never select、wait generation 只 ready 一次、WaitNode 跨 owner exactly-once 归还，以及含 `chan`/`select`/`wait` 的源切片改变 demand/指纹、comptime 负容量与非法 `try_send` 进 select 使 Frontend 失败且 `image-plan` 为 null；Linux 与 Windows 目标冷/热编译 action key 与 dump 一致。`cargo bench -p gugu-compiler --bench channel_wait` 以多 producer ping-pong 与 select 提交验证 exactly-once 与账本守恒。工作区 547 项测试全部通过，fmt/build 零 warning，mdBook 构建通过。镜像内真正的 channel/select 执行路径仍待后续写出，复用同一 schema。

- [x] **阶段 37：实现 std.sync 原子、锁、OnceLock、Lazy 与取消**（复杂度：4）
  - 依赖：阶段 35、36。
  - 实现 Ordering、Atomic 合法类型、Mutex/RwLock/Condvar、非 poisoning guard、OnceLock/Lazy 的 Ready/Failed、CancelSource/CancelToken、取消与阻塞操作的接缝。
  - 验收：Acquire/Release/SeqCst 语义由状态机测试验证；初始化 panic 永久 Failed；锁 guard 释放不依赖用户 drop；取消幂等、协作、不会隐式 kill 子进程或取消 Join 子协程。
  - 接入证据：`RuntimeRawModel` 升至 schema 8，并入 `SyncRuntimeContract` schema 1：包含 5 种内存序名称（Relaxed/Acquire/Release/AcqRel/SeqCst）、12 种原子合法标量类型目录（`bool`, `int8`, `int16`, `int32`, `int64`, `int`, `uint8`, `uint16`, `uint32`, `uint64`, `uint`, `ptr` 与 raw 指针）、Once 状态枚举（Uninit/Initializing/Ready/Failed）、Cancel 状态枚举（Active/Cancelled）、Mutex 状态枚举（Unlocked/Locked/Contended），以及 5 类 64-byte/64-byte 对齐控制结构（`MutexControl`、`RwLockControl`、`CondvarControl`、`OnceControl`、`CancelControl`）。`SyncDemand` 由优化后 LIR 统计原子操作、互斥锁、读写锁、条件变量、OnceLock、Lazy 与取消操作；指纹派生键 `gugu-sync-runtime-v1` 进入 action key。内建 `std/runtime/sync.gg` 经冻结 HIR/具体 GIR 与 machine 布局交叉校验。参照模型挂在 `RawWorld`：`AtomicStateMachine` 验证 Relaxed/Release/Acquire/SeqCst 与 CAS 序降级校验；`Mutex` 与 `RwLock` 锁争用只挂起协程，guard 经由 Adaptive Resource Leasing 在协程完成或 panic 展开时自动解锁（non-poisoning）；`Condvar` 原子释放锁挂起并在唤醒后重获锁；`OnceLock`/`Lazy` 初始化异常永久 Failed 且不可 reset；`CancelSource`/`CancelToken` 幂等协作，在 `channel_recv_cancel` 中安全注销不丢消息，在 `join_wait_cancel` 中仅取消当前等待者且绝不 kill 子协程。`ImagePlan`/CLI JSON/`-Zdump-runtime` 输出 `sync-contract-fingerprint`、`sync-demand`、`sync-primitive-count` 与 `sync-runtime`。
  - 验收证据：`sync_tests` 覆盖内存序编码与 CAS 失败序非法拒绝、合法与非法原子标量类型、Release-Acquire 视图同步与 SeqCst 全局序、Mutex 争用与持锁协程完成/panic 时租约自动解锁不 poisoning、RwLock 多读单写与展开自动释放、Condvar 原子 wait 与 notify_one/notify_all 唤醒、OnceLock 正常 Ready 与闭包 panic 永久 Failed 拒绝重试、`CancelSource` 幂等取消与 token 检查、channel 取消接缝与 Join 取消接缝验证（子协程绝未被 kill 且正常完成）、以及契约 demand 变更敏感度与 CLI 报告。`cargo bench -p gugu-compiler --bench sync_lock` 以真实多工作线程争用验证原子状态机、互斥锁与账本守恒。工作区 555 项测试全部通过，fmt/build 零 warning，mdBook 构建通过。镜像内真正的同步原语执行由后续阶段交付，复用同一 schema。

- [x] **阶段 38：实现 stack map、unwind 与可复制栈扫描**（复杂度：5）
  - 依赖：阶段 28、34、35。
  - 实现 root kind、safepoint kind、寄存器/slot map、固定 section 编码、frame walk、StackInterior 修正、panic landing、stack copy 和 map verifier。
  - 验收：CallReturn/suspend/bridge/stop 点的 managed roots 可精确枚举并更新；旧栈 opaque pointer 不会重用；缺 map、版本错误、范围越界和寄存器冲突在镜像写出前失败。
  - 接入证据：`RuntimeRawModel`（query 30）升到 schema 9，在同一 `RuntimeRawContractV1` 中并入 `StackMapRuntimeContract`：五类根种类目录（heap-direct/heap-interior/shared-handle/compressed-ref/stack-interior，判别值 0..4）、safepoint kind 数值（0=CallReturn/1=PollResume/2=SuspendResume/3=ForeignBridge/4=MorestackEntry）、寄存器位号（bit0..14，bit15 保留零，r14/r15 普通函数禁用）、flags 位定义与 section 常量（魔数 `GUGUSM01`、version 2 拒收 v1、指针宽度 8、小端）。`StackMapDemand`（函数/安全点/去重 map 数、五类 kind 分类计数、alloc/barrier 站点数、根字数、落地函数数）进入 query key 与契约编码。`lir/stackmap.rs` 从优化后 LIR 推导逻辑世界：入口 `StackCheck` 走 `MorestackEntry`（零槽加 ABI 参数根），Managed 调用点（含 `Allocation` 种类）走 `CallReturn`（传出参数与按值副本展开，sret 排除），`poll_free_leaf` 与 `ForeignLeaf` 无记录，`Suspend` 走 `SuspendResume`（全零掩码），有 default select 走 `CallReturn`，bridge 走 `ForeignBridge`（dirty 置 bit3），`SafepointPoll` 走 `PollResume`，纯分配与屏障只计数；`Provenance` 新增 `SharedHandle`/`CompressedRef` 显式变体（handle 与压缩引用不参与偏移、cast 与 SSA 降级合流）。`runtime/stackmap_codec.rs` 实现 header/function/safepoint/map 四表编解码与 map 字典序去重（生产契约只收录逻辑指纹与计数，合成布局字节仅用于单测）；`runtime/stackmap.rs` 实现函数与安全点二分查找、五类根扫描（direct 空值容忍、interior 增量透传、handle 代际校验、压缩引用 cage 与代际 checked 解码、stack 收集）、最内层落地选择、复制输入组装与 bridge 帧校验，字级复制委托 `StackImage::relocate`。契约指纹以派生键 `gugu-stackmap-runtime-v1` 进入 action key，`ImagePlan`/CLI JSON 新增 `stackmap-function-count`、`stackmap-safepoint-count`、`stackmap-map-count`、`stackmap-root-words`、`stackmap-contract-fingerprint` 与 `stackmap-demand`，`-Zdump-runtime` 输出稳定栈图段。工作区 583 项测试全部通过（本轮新增 10 项：栈图推导覆盖与重复根拒绝、契约需求跟踪与分类失配拒绝、编解码往返与版本/保留位/互斥/越界拒绝、walker 五类扫描与落地选择、镜像计划栈图断言），fmt/build 零 warning，mdBook 构建通过；真实 fixture 的 Linux `build` 与 Windows `check` 报告 17 函数、19 安全点、9 去重 map、18 根字，冷热一致，退出码 0。真实机器布局填充、GC 侧精确追踪消费与 bridge 生命周期分别随阶段 55/39/58 验收。

- [x] **阶段 39：实现 Mosaic GC 元数据与精确 trace**（复杂度：5）
  - 依赖：阶段 25、32、38。
  - 实现 TypeRecord、heap header、2 MiB arena/32 KiB block/128-byte line 元数据、trace descriptor/program、value program、vtable/glue/root/source metadata section 和 boot verifier。
  - 验收：canonical ULEB128、唯一 END、size/align/TypeId、section offset、root range、vtable 和 glue 关系全部校验；collector 不对 runtime raw memory 做保守扫描。
  - 接入证据：`RuntimeRawModel`（query 30）升到 schema 10，在同一 `RuntimeRawContractV1` 中并入 `GcMetadataRuntimeContract` schema 1：section 魔数 `GUGUGC01`、主版本 1、arena 2 MiB / block 32 KiB / line 128 byte（与 slab/extent 参数同源）、8 类 type flag 名（`has-heap-direct`/`has-heap-interior`/`has-value-actions`/`has-resource`/`has-deferred-release`/`unsized-view`/`variable-size`/`pin-sensitive`）、trace op 名（`end`/`direct`/`interior`/`repeat`/`switch`）与 value op 名（`end`/`aggregate`/`repeat-value`/`switch-value`/`cow-publish`/`acquire-resource`）。`GcMetadataDemand`（类型数、trace/value program 字节数、vtable/glue/root/source/alloc 计数、arena/block/line 参数）由冻结类型表（`TypeUniverse.records` 与 `vtables`）推导，进入 query key 与契约指纹；指纹以派生键 `gugu-gc-metadata-contract-v1` 进入 action key。`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `gc-metadata-type-count`、`gc-metadata-trace-bytes`、`gc-metadata-value-bytes`、`gc-metadata-vtable-count`、`gc-metadata-root-count`、`gc-metadata-arena-bytes`、`gc-metadata-block-bytes`、`gc-metadata-line-bytes`、`gc-metadata-contract-fingerprint` 与 `gc-metadata-demand`；dump 三行 `gc-metadata schema=...`、`gc-metadata-types ...` 与 `gc-metadata-fingerprint ...`，冷/热编译一致。`frontend/gc.rs::derive` 从 `TypeUniverse` 镜像 `GcMetadataWorldV1`，boot_verify 校验 END 唯一、child/vtable key 可解析、layout 与 flags 自洽、root 范围不重叠。`gc_metadata_tests` 覆盖最小 world 自洽、缺 trace/value END 拒绝、child/vtable key 不可解析拒绝、demand 指纹稳定且随字段变化、arena 漂移拒绝、RawModelError 文本展示，allocation 慢路径真实启动自动 cycle 并记录 live record 残差、软上限下分配被真实拒绝、候选超出 relocation 预算时整块延后而预算内前缀仍真实 decommit，以及契约在 `RuntimeRawContractV1::build` 内的端到端集成；`tests::image_plan_reports_gc_metadata_contract` 验证镜像计划含完整 `gc-metadata-*` 字段、dump 行存在、类型表扩张时 GC metadata 指纹变化。阶段 39 同期修复 `Ty::Callable` 在 mono 与具体 GIR 布局之间的稳定键分叉（见 `internals/monomorphization-cache.md` 与提交 `0813ad6`），并新增 `function_item_type_key_matches_frozen_universe` 回归保证 `type_id[函数项]()` 不再报 `E0054`。当前阶段 39 的 trace/value program 对每条 entry 只发单字节 `End`，REPEAT_FIELD/ARENA_SLOTS 与 `String`/COW/`ResourceCell` 资源字段由阶段 40–47 在 `placement` 与 `LocalHeap` 接入后补齐。

- [x] **阶段 40：实现 hybrid write barrier 与 remembered set**（复杂度：5）
  - 依赖：阶段 27、29、39。
  - 实现 Yuasa deletion + Dijkstra insertion、direct field barrier、跨 block edge summary、256 项 CardMarkBuffer、dedup stamp、CardMarkBatch 与 owner 消费。
  - 验收：实际 field store 先于 barrier 账本发布；buffer 满、processor handoff、foreign、pressure、minor stop 和 producer gate 会 flush；barrier reserve 不跨 NoSafepointRegion 分配或补容量。
  - 接入证据：`RuntimeRawModel`（query 30）升到 schema 11 并在同一 `RuntimeRawContractV1` 中并入 `BarrierRuntimeContract`（`BARRIER_SCHEMA = 1`）：六步 hybrid 序列 `read-old`→`shade-old-deleted`→`shade-new-inserted`→`store`→`card-mark`→`edge-summary`，`store` 早于 `card-mark` 由 verifier 强制；512-byte card 粒度与 `GC_ARENA_CARDS` 同源，256 项 `CardMarkBuffer` + 256 项直接映射 dedup stamp，前者 8256 byte、后者 8192 byte，per-arena card table 4096 byte，四个 `#[repr(C)]` record（`CardMarkEntry`/`CardMarkStamp`/`CardMarkBufferHead`/`CardTableDescriptor`）的 size/align/offset 由 `offset_of!` 逐字段登记并与 `resources/runtime/barrier.gg` 的 Gugu 布局交叉校验；六个 flush 原因 `buffer-full`/`processor-handoff`/`foreign-bridge`/`memory-pressure`/`minor-stop`/`producer-stop-gate` 与五种 barrier kind `yuasa-deletion`/`dijkstra-insertion`/`direct-field`/`edge-add`/`edge-drop` 全部登记。`BarrierDemand`（reserved/bare 屏障数、region/permit 数、permit 的 `max_shades`/`max_card_marks` 上界与总额度、edge summary 与 card-mark 站点）由 `lir::Validated::barrier_demand()` 从优化后 LIR 推导，`BarrierPermitData { region, max_shades, max_card_marks }` 的额度由 `lir/pass/barriers.rs` 计算为 `2 × region 内 barrier 数` 与 distinct 写入地址数，`lir/verify/regions.rs` 按 region 窗口重算同一上界并要求 permit 额度精确一致，先按 permit 自身字段拒绝 card-mark 额度超过 `CARD_MARK_BUFFER_ENTRIES` 的预留（超额的 permit 必然需要在 region 内补容量），再要求额度与 region 静态复算精确一致，区域内超额消费报 “hybrid barrier 超过预留 shade 或 card-mark 额度”。runtime 侧 `message.rs` 在同一 non-moving node pool 上引入消息族判别（`state_kind` 的族字节 + `payload_low`/`payload_high` 车道），`CardMarkBatch` 携带 arena descriptor/generation、card 区间、cycle epoch、bytes 与独立 integrity 派生键 `gugu-card-mark-integrity-v1`，`ProducerStaging::stage` 改为载荷无关的 `(target, bytes, shard)`，两个族共用 producer gate、queue-page grace 与 node 复用；`runtime/barrier.rs` 给出确定性参照实现（`CardMarkBuffer` 的 dedup 命中不丢键、256 上界触发 `BufferFull`、`advance_epoch` 先把旧 cycle 的键交成草稿再切换、buffer 非空时拒绝切换且 epoch 只能前进、`CardTable` 的 owner 写与幂等置位、`EdgeSummary` 的 add/drop epoch 提升与同 epoch 净零抵消、跨 owner 或跨 block 的写入才进入 owner-local edge summary、owner 经 `take_edge_deltas` 取走聚合结果并累计 `edge_deltas`/`edge_pending` 统计，`CardMarkHarness` 在生产路径上驱动该取出口），`world/barrier_impl.rs` 把六个触发点接到 arena 登记、owner 本地合并写与 `CardMarkBatch` 发布/消费上，并接入真实事件路径：owner `retire`、`drain_all`、source-slab cache 的 `GcHandoff`/`PressureDrain` 关闭以及 `enter_foreign` 都在各自交接点冲刷本 owner 的全部 processor 账本，minor stop 门禁要求全部 buffer 已 flush 且 pending batch 已消费；错配 generation、错误 owner、越界 card 区间与 integrity 失败都进入 `RuntimeInvariant`，flush 次数与六个原因的分类计数由平面累计。`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `barrier-card-granularity-bytes`、`barrier-card-mark-buffer-entries`、`barrier-card-mark-stamp-entries`、`barrier-flush-reason-count`、`barrier-card-mark-batch-fields`、`barrier-record-count`、`barrier-contract-fingerprint` 与 `barrier-demand`，dump 输出 `barrier schema=1 card=512 buffer=256 stamps=256`、`barrier-steps`、`barrier-flush-reasons`、`barrier-kinds`、`barrier-record`/`barrier-field`、`barrier-demand`、`barrier-pressure`、`barrier-fingerprint` 与 `runtime-message return-fields=12 card-mark-fields=13 card-mark-family=card-mark`，冷/热编译逐字节一致，指纹以派生键 `gugu-barrier-runtime-v1` 并入 `RuntimeRawContractV1` 后进入 action key。`runtime/barrier_layout.rs` 把 `resources/runtime/barrier.gg` 的四个 `#[repr(C)]` record 与 machine 契约逐字段交叉校验，空 package 与任何非登记外部 record 都被拒绝。`runtime/barrier_tests.rs` 覆盖契约漂移拒绝（六步换序、粒度/容量/原因目录/地址字段）、store 先于账本、dedup 冲突不丢键、连续 card 合并与幂等、256 上界、epoch 前进交出旧键且拒绝回退、六个 flush 原因与分类计数、owner `retire`/`drain_all`/cache 关闭/`enter_foreign` 四条真实交接路径的冲刷、跨 owner 消费与 generation/owner 错配、minor 门禁、edge delta 聚合与取走；`lir::tests::permit_quota_beyond_buffer_capacity_is_rejected` 与 `permit_quota_at_buffer_capacity_passes_the_capacity_check` 用错误文本分别锁定容量检查与额度一致性检查，`tests::image_plan_reports_barrier_contract_for_managed_writes` 验证冷/热 dump 一致、站点增多时指纹变化且协议不随目标漂移，`benches/barrier_card_mark.rs` 提供记账/dedup/flush 吞吐 smoke（不进 `nextest`）。阶段 43 的 Immix/TLAB、阶段 44 的 MarkMailbox、阶段 45 的 `EdgeDelta` 传输与阶段 41 的 pressure episode 不在本阶段范围内。

- [x] **阶段 41：实现 GC debt、credit、pacing 与 pressure drain**（复杂度：5）
  - 依赖：阶段 30、32、39、40。
  - 实现 allocation/mark debt、owner credit、gc CPU fraction、assist quantum、remark/evacuation pause budget、pressure hysteresis、forced full cycle 和 pending bytes backpressure。
  - 验收：一次 pressure episode 至多强制一次 full cycle；预算超限发布 continuation 或延后 block；无法取得 headroom 才 OOM；统计覆盖 message/cache/credit/grace 的真实物理占用。
  - 接入证据：`RuntimeRawModel`（query 30）升到 schema 12，在同一 `RuntimeRawContractV1` 中并入 `GcPacingRuntimeContract`（`PACING_SCHEMA = 2`）：唯一 profile `mosaic-default`（revision 2）固定 16 个参数——`min_growth_budget` 8 MiB、`assist_threshold` 1 MiB、`assist_quantum` 64 Ki、`mark_cost_per_byte` 1、`gc_cpu_fraction` 25、`gc_cpu_window_cost` 16 Mi、`remark_cost_budget` 4 Mi、`evacuation_pause_bytes` 2 MiB（必须是 32 KiB block 的整数倍，且不得小于 extent 阶梯顶层）、`evacuation_pause_roots` 4096、`evacuation_pause_fields` 65536、`pressure_enter_ratio` 85、`pressure_clear_ratio` 70、`pressure_poll_bytes` 1 MiB、`owner_drain_items` 64、`owner_drain_bytes` 64 KiB（不得小于一个 128-byte return node）与 `owner_drain_interval_bytes` 1 MiB，并登记 cost unit 名 `mark-cost-unit`。契约还固定三个 pressure 状态 `steady`/`drain`/`emergency`、三个必须各自完成 owner drain 的账本分类 `owner-cache-bytes`/`pending-return-bytes`/`reclaimable-bytes`（与 `LedgerSchemaV1` 的 `runtime-committed-bytes` 分区独立计数器逐项同源）、四种 assist 结局 `none`/`within-quantum`/`quantum-truncated`/`no-work`、两种 remark 结局 `complete`/`continuation`、两种 evacuation 结局 `admit`/`defer` 与五个 credit 来源 `barrier-buffer`/`card-mark-batch`/`edge-delta`/`pending-return`/`producer-staging`。verifier 拒绝参数漂移、违反 `0 < clear < enter < 100`、非 block 整数倍或小于 extent 阶梯顶层的 payload 上界、窗口预算 `window × fraction / 100` 容不下一次 assist quantum、`assist_threshold`/`remark_cost_budget` 小于 quantum、四个 drain 节奏参数为零、`owner_drain_bytes` 小于一个 return node、与账本不一致的 drain 分类，以及内容与登记指纹不一致。`GcPacingDemand`（分配站点、屏障站点、assist slow edge、受管类型数）由 `lir::Validated::pacing_demand` 从优化后 LIR 推导（分配站点覆盖 `GcAlloc`/`RegionAlloc`/`PromoteManaged`，slow edge 等于分配站点加显式 `SafepointPoll`），进入 query key 与契约指纹，指纹以派生键 `gugu-gc-pacing-runtime-v1` 并入 `RuntimeRawContractV1` 后进入 action key。`runtime/pacing.rs` 的 `PacingPlane` 实现 `growth_budget = max(min_growth_budget, last_live × target / 100)`、`allocation_debt`、`mark_debt`、`pressure_debt`（未配置 soft limit 时恒为 0）与 `return_pressure`，assist 只在窗口额度内偿还真实可消费 work（无 work 报 `no-work` 且不记账，超出 quantum 报 `quantum-truncated`），`worker_work` 把超窗工作转为 debt，emergency 可越过吞吐预算；remark 要求 barrier 已开启并在超预算时发布 continuation；relocation 的 bytes/root/field 任一超出即整 block `defer`；pressure episode 由平面线性化（enter 水位开启、达 limit 进 `emergency`、低于 clear 且三类分类各自 drain 一次后结束、一个 episode 至多一次 forced full cycle、用尽 drain 与 forced cycle 仍无 headroom 才 `OutOfMemory`），`CreditPlane` 用真实结构观测五个来源并要求全部归零才能推进 cycle epoch。`world/pacing_impl.rs` 把决定接到真实路径：分配成功后累加 allocation debt；分配上的唯一慢路径 `pacing_slow_edge(owner, bytes)` 由 `slow_edge_due` 把关（assist 阈值、cost window、`pressure_poll_bytes` 的 poll 节奏与 `min_growth_budget` 的 cycle 尝试节奏；都不成立时普通分配不付任何全局查询），先借一次真实 assist、再按节奏推进 hysteresis、在 episode 内按 `owner_drain_interval_bytes` 执行一次有界 drain、按 allocation debt 启动自动 cycle，最后用「committed 估计 + 本次请求的真实 slot 字节」判断是否需要推进 headroom：只有可能越过 limit 时才刷新快照并走 drain → forced cycle → `OutOfMemory` 链。`run_gc_cycle`/`bounded_owner_drain` 共用一条 drain 实现：先交出未发布的 return staging、再关闭 owner 真实的 source-slab cache（关闭顺带以 memory-pressure 原因冲刷该 owner 的 processor 账本，同一次 drain 不重复冲刷也不虚增空 flush 统计）、排空 owner inbox、推进 queue-page grace epoch（否则空载 extent 永远停在 `GracePending`）、取出 owner-local edge delta，把本 cycle 相对上一次基线真实完成的 card 工作计入滑动窗口（emergency 越过吞吐预算，普通 cycle 把超出部分转为 mark debt，工作量取累计计数器差值，deferred mark 工作跨 cycle 存活由 assist 归还）。只有 full cycle 过 remark 终止门禁：通过后推进 barrier epoch 并立即排空新 epoch 发布的批次、取走新 edge delta，再要求五个 credit 来源收敛——收敛时 credit epoch 同步前进，未收敛只是本轮 cycle 未完成而不是错误（credit epoch 保持落后、下个完成 cycle 单调追平）。drain 随后对全部空载 extent 重跑 allocator/scanner/forwarder lease、live/queued slot 与在途 return 四条门禁并真实 `decommit`，pause footprint 取候选自身撤销的 committed 字节与 descriptor 数；通过门禁的 extent 在 decommit 的同一步让名下空载 descriptor 离开 committed 口径，未过门禁的保持 committed 并计入 `blocked_extents`，超出一轮 relocation pause 预算的候选整块计入 `deferred_extents` 并由下个 cycle 继续（单个 extent 至多等于预算上界，因此每轮至少推进一个候选）。`complete_cycle` 记录真实 live record 残差（`committed − pending − reclaimable − cache`，由 `ledger_invariant` 运行时强制守恒）作为下一次增长预算输入；软上限口径取 provider committed 总量（stack arena 与 raw plane 共用同一 provider，再加一次 stack committed 会把同一物理页计入两次），headroom 被拒时走 rt0 的 `OutOfMemory` fatal 并让本次分配失败。`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `pacing-contract-fingerprint`、`pacing-profile`、`pacing-profile-revision`、`pacing-min-growth-budget`、`pacing-assist-threshold`、`pacing-assist-quantum`、`pacing-mark-cost-per-byte`、`pacing-gc-cpu-fraction`、`pacing-gc-cpu-window-cost`、`pacing-remark-cost-budget`、`pacing-evacuation-pause-bytes`、`pacing-evacuation-pause-roots`、`pacing-evacuation-pause-fields`、`pacing-pressure-enter-ratio`、`pacing-pressure-clear-ratio`、`pacing-credit-source-count`、`pacing-pressure-poll-bytes`、`pacing-owner-drain-items`、`pacing-owner-drain-bytes`、`pacing-owner-drain-interval-bytes` 与 `pacing-demand`，dump 输出 `pacing`/`pacing-budget`/`pacing-cpu`/`pacing-remark`/`pacing-evacuation`/`pacing-pressure`/`pacing-drain-classes`/`pacing-drain`/`pacing-assist-outcomes`/`pacing-remark-outcomes`/`pacing-evacuation-outcomes`/`pacing-credit-sources`/`pacing-demand`/`pacing-fingerprint` 十四个段，冷/热编译逐字节一致。`pacing_tests` 覆盖：契约自洽与参数/目录漂移拒绝、指纹随需求变化且与整体契约身份同步、debt 公式与 `GcTarget::Off` 只关闭 debt 触发、assist 四种结局与不虚构进度、assist 只按真实交接的键数偿还、无交接时 `no-work` 不记账、per-cycle 工作量取累计计数器差值、deferred mark 工作跨 cycle 存活、cost window 拒绝越过预算并把超出部分转为 debt、remark continuation 与未开启 barrier 拒绝、evacuation 三项上界、credit 收敛与 epoch 只能前进、hysteresis 开启/结束条件（分类为空不算 drain 证据）、一个 episode 至多一次 forced cycle 后才是 OOM、请求字节与 committed 之和才决定 headroom、episode drain 节奏门禁、world 侧真实 credit 观测与 drain、drain 冲刷真实 return staging、稳态分配不读全局快照、memory-pressure 每 owner 只冲刷一遍、连续 cycle 持续推进 epoch、未收敛的 cycle 既不报错也不推进 credit epoch、空载 extent 过 grace 后 committed 真实回落、allocation 慢路径真实启动自动 cycle 并记录 live record 残差、软上限下分配被真实拒绝、候选超出 relocation 预算时整块延后而预算内前缀仍真实 decommit，以及契约在 `RuntimeRawContractV1::build` 内的端到端集成；`tests::image_plan_reports_barrier_contract_for_managed_writes` 与 `build_json_reports_barrier_contract_keys` 验证镜像计划与 CLI JSON 含完整 `pacing-*` 字段、dump 段存在、`pacing-demand` 与 barrier 需求覆盖同一屏障站点集合、站点数变化同时改变两处指纹。grep-based 扫描确认无残留阶段号写入规范、internals 与代码注释；本阶段不实现 Immix/TLAB、`MarkMailbox`/`MarkTicket` 与 `EdgeDelta` 传输（分别由后续阶段的 LocalHeap、mark mailbox 与 edge delta 传输接手），credit 平面已为它们预留同一入口。

- [x] **阶段 42：实现 TurnRegion 私有图与 transfer/reset**（复杂度：4）
  - 依赖：阶段 23、27、39、41。
  - 实现 owner-local bump/reset、export summary、RegionTransfer、promotion、ResourceCell/FFI alias 检查和 region registry。
  - 验收：只有无外部 alias、无 resource lease、无 FFI 地址且 export summary 闭合时才 reset；sender 仍需使用语义时先 promote/copy；普通 channel 不获得 transfer 语义。
  - 接入证据：`EscapeAndPlacement`（query 29）升到 schema 2，在同一 `PlacementWorldV1` 中新增 region 计划：CFG 去掉直接挂起（`Suspend`、`SelectCommit { suspend }`）与出口（`Panic`/`Return`/`ResumePanic`/`Abort`/`Unreachable`）后按连通段分段（边界 block 自成一段，使「分配与 send 同块」也能成立），每段至多一个 `RegionPlan { body, region, allocations, export, exits }`，`RegionExit { block, transfer }` 记录出口与是否整体移交所有权；分配点记录新增 `region` 字段，`ExportFlags` 新增 `CHANNEL_SEND`（上界 8 位）与 `TRANSFER`。可调用边界不是 region 出口：callee 只在调用者下一次直接挂起或出口前返回，期间调用者不会 reset，因此 `may_suspend` 调用不作为 reset 点；挂起安全由直接挂起出口的活跃性承担。region 判定要求分配点支配段的每个出口、出口终结符读取的可达局部全部是 send 值、且没有任何可达局部在该出口之后仍 live，否则整段退回 stable storage；`IsAlloc` 只收录对象/数组装箱与「真正带捕获环境」的闭包（无捕获的函数项引用在 lowering 里直接返回空指针，不留分配点），`Intrinsic{ChanNew|Spawn}` 从分配点表移除并在 `seed_assign` 标记 `PUBLISH|ESCAPE`，因此普通 channel 自身永不获得 region。channel send 读取的值带 `CHANNEL_SEND` 而不带 `PUBLISH|ESCAPE`：只有「sender 之后不再使用」才留下 region 并标记移交，sender 仍需使用语义时该站点退回 `SharedHeap`（等价于在分配点上 promote）。`liveness.rs` 新增 block-local bitset 活跃性与 turn 边界识别（直接挂起 + 出口）。`LIR` revision 升到 5：`RegionAlloc { region, descriptor, align }`、`RegionPublish { region, export }`、`RegionReset { region }`、`PromoteManaged { region }`、`RegionTransfer { region }` 全部改为无参数、无结果的生命周期指令（`PromoteManaged` 仍要求 `Allocation` safepoint，其余四个为 `None`），lowering 按静态 region 计划在每个边界 block 的终结符之前发出「一次 publish + 恰好一个 reset/promote/transfer」，闭包环境按分配点记录走 `RegionAlloc`，普通 managed 环境仍走 `GcAlloc`；`lir/verify/regions.rs::verify_lifecycle` 用 must/may 双数据流强制「每条路径恰好分配一次、恰好发布一次、恰好结束一次」，并拒绝未闭合 summary 上的 reset、结束后再分配与重复发布；`verify_channel_transfer` 强制 `RegionTransfer` 只能出现在同 block 更晚的 `ChannelSend`/`SelectCommit` 之前，且任何可追溯到分配点的 send 实参都必须先被显式移交，使「普通 channel 不获得 transfer 语义」成为结构不变量。`lir::Validated::turn_region_demand()` 从优化后 LIR 推导 region 数、分配/发布/重置/保留/移交站点数、容量 class 需求、payload 总量与单区上界。`RuntimeRawModel`（query 30）升到 schema 13，在同一 `RuntimeRawContractV1` 中并入 `TurnRegionRuntimeContract`（`REGION_SCHEMA = 1`）：十档容量 class（64 B…32 KiB，上界与 `GC_BLOCK_BYTES` 同源，verifier 拒绝越界与稀疏阶梯）、单 region 对象上界 64、单 owner 活跃 region 上界 64、transfer lease 上界 1、五位 export summary 目录（`external-alias`/`resource-lease`/`ffi-address`/`pending-transfer`/`live-root`，掩码由位目录自身推导）、七个状态名（`private`/`publishing`/`reset-pending`/`reset`/`local-promote`/`region-transfer`/`received`）与 `RegionTransfer` 的 13 个字段（经 `MessageSchemaV1::verify` 复用「禁带地址 + 必含身份字段」检查），`TurnRegionDemand` 同时进入 query key 与契约指纹（派生键 `gugu-turn-region-runtime-v1`），verifier 拒绝需求越界、目录/字段漂移与指纹不一致。`message.rs` 在既有 non-moving node pool 上新增第三个消息族 `RegionTransfer`（族判别 2），`RegionTransferBatch` 携带 region 序号、region generation、type summary 编号、bytes、容量 class、声明与观察到的 export state、来源 owner 与目标 owner 身份、cycle epoch，独立 integrity 派生键 `gugu-region-transfer-integrity-v1`，`payload_low`/`payload_high`/`descriptor_unit`/`bytes_epoch` 车道承载全部字段，`stage_region_transfer` 复用同一 producer gate、staging chain 与 queue-page grace。`runtime/region.rs` 给出参照实现：`RegionRegistry` 用稠密 `Vec<Option<RegionDescriptor>>` + free list + 单调 generation 管理 descriptor，`open`/`bump`（容量与对象双上界、返回 bump 偏移）`/publish`/`observe`/`clear_observation`/`reset`（五条判据：状态为 `publishing`、无 transfer lease、summary 闭合）`/promote`（`LocalPromote`，地址稳定、字节继续计入）/`transfer`（占用 lease 并发出消息）/`receive`（校验目标 owner、integrity、容量 class 并重新稠密分配编号）/`confirm`/`receive_reset`，`RegionPlane` 管理多 owner registry 与在途消息（拒绝同一 region 两条在途 transfer），并按拒绝原因分三类计数。`world/region_impl.rs` 把判据接到真实 owner 事实：`configure_regions` 用同一份契约建立 plane，`region_open` 把整个容量 class commit 给 owner 并从 cache 扣除，`region_end` 在声明 summary 非零时直接 `PromoteManaged`、否则走门禁，拒绝时转 promote 并记录原因，`reset` 成功才 `release` 容量；`region_confine`/`region_release_confine` 把外部 alias、resource lease、pin/foreign 地址、在途 transfer 与 live root 写成观察位，`region_transfer` 经 `stage_region_transfer` 发布消息并在 plane 登记，`service_region_transfer` 在 `RawWorld::service` 的消息族分派里被调用，采纳后两侧 owner 账本一起移动（来源 `release`、目标 `commit` + `take_from_cache`），在途字节经 `RegionPlane::pending_bytes` 计入 credit 快照的 `pending_return_bytes`，harness 的 world 构造同时配置 region plane。`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `turn-region-sites`、`turn-region-publish-sites`、`turn-region-reset-sites`、`turn-region-promote-sites`、`turn-region-transfer-sites`、`turn-region-capacity-class-count`、`turn-region-object-limit`、`turn-region-max-bytes`、`turn-region-total-bytes`、`turn-region-contract-fingerprint` 与 `turn-region-demand`，dump 输出 `region schema=`/`region-capacity-classes`/`region-export-bits`/`region-states`/`region-transfer-fields`/`region-demand`/`region-fingerprint` 七行。`runtime/region_tests.rs` 覆盖契约自洽与需求越界拒绝、字段目录无地址且排序稳定、闭合 summary 才能 reset、五个观察位逐一拒绝 reset 并转 promote、未发布与在途 lease 的拒绝分类、transfer 往返（generation/容量 class/integrity/lease/接收方重编号/已采纳 region 可再次发布）、容量与对象上界、未登记 summary 位与重复发布拒绝、plane 拒绝重复在途消息、node 车道往返保真、world 侧账本 commit/release 与跨 owner 移交、未配置契约即失败；`lir::tests` 覆盖真实闭包程序的 alloc/publish/reset 序列与生命周期自洽、sender 已死时的一条 transfer 且无 reset、sender 仍使用时退回 `SharedHeap` 且零 region、未闭合 summary 上的 reset 拒绝、缺少结束动作拒绝、普通出口上出现移交拒绝与生命周期指令不得带参数；`tests::image_plan_reports_turn_region_contract` 验证两条真实程序的镜像计划与 dump，`build_json_reports_barrier_contract_keys` 验证 CLI JSON 键，`benches/region_transfer.rs` 提供「建立/bump/发布/回收/保留/移交往返」吞吐 smoke（不进 `nextest`）。真实 Immix/TLAB backing 与分代 cycle 在阶段 43，promoted region 的 trace 驱动搬迁与 SharedHeap handle 在阶段 43/46，移交回执消息由 scheduler 层承担，本阶段只在 owner 账本与 credit 观测上落实两阶段移交。

- [x] **阶段 43：实现 LocalHeap Immix、TLAB 与分代 cycle**（复杂度：5）
  - 依赖：阶段 39–42。
  - 实现 nursery/aging/old/pinned/large/resource arena、TLAB refill、object-start/mark/card bitmap、minor copying、major mark/sweep/evacuation、pin side table 和 owner-local free。
  - 验收：对象不跨 block；age、pin、large/resource 规则正确；minor 只移动允许的 LocalHeap 对象；所有移动都更新 exact roots/interior pointers；无 read barrier 的 direct pointer 热路径保持成立。
  - 接入证据：新增 `LOCAL_HEAP_SCHEMA = 1` 契约段（`runtime/local_heap_schema.rs`），把 Immix/TLAB 尺寸固定成单一事实来源——arena 2 MiB（64 block × 32 KiB、每 block 256 line、granule 16 byte）、8 block TLAB span（256 KiB）、object-start/mark 各 131072 bit（16384 byte）、`page_covering_object` 512 项、card 表 4096 byte，以及 `ObjectHeader`（16 byte，`control`/`payload_size_or_forward`）、`HeapArenaMetadata`（55576 byte，逐字段偏移由 `local_heap_layout::verify_source` 与 `resources/runtime/heap.gg` 的 Gugu 布局交叉校验）、`HeapPinEntry`（16 byte）三条记录。目录固定 arena state（`free`/`nursery`/`aging`/`old`/`resource`/`pinned`/`evacuating`）、block state、四代 generation、representation 与 object flag，并登记 minor/major 阶段名；`HeapTriggerProfile`（revision 1）固定 `minor_trigger_bytes` 256 KiB、`tenure_age` 2、`max_age` 15 与 non-moving 阈值，verifier 拒绝参数漂移、非 block 整数倍尺寸、违规代龄与目录/记录篡改。`LocalHeapDemand`（分配/pin/resource/shared/promote/pin/unpin/barrier 站点、managed/large 类型数、单对象上界）由 `lir::Validated::local_heap_demand` 从优化后 LIR 与冻结类型表推导，与 `gc_metadata.demand.type_count`、`barrier.demand.card_mark_sites` 在同一 `RuntimeRawContractV1` 内交叉校验；`RuntimeRawModel`（query 30）升到 schema 14，`ImagePlan`/CLI JSON/`-Zdump-runtime` 增加 `local-heap-contract-fingerprint`、`local-heap-runtime`、`local-heap-trigger`、`local-heap-demand` 四组键（`local-heap schema=1 arena=2097152 block=32768 line=128 granule=16 tlab-span=262144 …`、record/directory/trigger/demand/fingerprint 行）。运行时参照实现 `runtime/gc_trace.rs` 与 `runtime/local_heap.rs` 消费同一份契约：trace descriptor 解释器按 `Bitmap`/`Program` 两种表示扫描 managed pointer word（`TRACE_MAX_DEPTH = 32`，wrapper 体保持 class 同质），`HeapArena` 用连续数组承载 line 表、object-start 位图与 block live 计数而不用 hash 容器，line run 分配记录块内碎片并保留 Immix 的 whole-block 大对象慢路径，nursery 从 owner 自己的 arena 取 8 block 局部 span；`world/heap_impl.rs` 把同一份契约接到真实 owner 事实：每个 owner 的 nursery/old/resource/pinned/large arena 按需建立，32 KiB block 经 extent 层按页提交（`MANAGED_LOCAL`，不进入 `OwnerAccounting`，`runtime_committed_bytes` 口径不变），`allocate_managed`/`store_managed_field`/`load_managed_field`/`pin_managed`/`unpin_managed`/`collect_minor`/`collect_major` 构成 mutator 与 collector 的完整接口。minor cycle 顺序为 flush remembered set → 根与 card 键保守扫描 → 搬运 nursery 存活对象到 old 并原地改写 exact/interior 引用 → 只对搬运前既已 aging 的对象加龄 → 重置 nursery；major cycle 先 flush 再标记、回收未标记对象的 line 并把空 block 交回 owner-local 空闲表；pin 是 safepoint：nursery 对象先提升进 pinned arena 再按 `HeapPinEntry` 计数，重复 pin/unpin 与跨 owner 地址均以精确 invariant 拒绝。`runtime/world/heap_tests.rs` 的 23 个确定性测试覆盖契约/触发参数/记录布局、nursery 与 TLAB 分配、placement 路由与大对象慢路径、字段读写与 card 记账、minor 搬运与引用改写、major 回收、pin 计数、owner-local 地址校验、interior 回表，以及用真实编译的 `local_heap_runtime()` 与 GC metadata section 驱动 arena 的端到端切片。本阶段边界：major 的 block 选择式 evacuation 与跨 owner `SharedHeap` handle 由阶段 45–46 接手，空 block/arena 交还 provider 与 decommit 属阶段 47，4 KiB page 级 radix map 的镜像落地与 heap 公共统计（`heap_committed_bytes`/`heap_live_bytes`）分别属阶段 55 与 67。

- [x] **阶段 44：实现 MarkTicket、MarkMailbox 与终止信用**（复杂度：5）
  - 依赖：阶段 40、41、43。
  - 实现每 owner 单 consumer MarkMailbox、cycle/topology/generation 标识、跨 owner mark ticket、credit acquire/consume/return、root snapshot gate 和并发终止检测。
  - 验收：mailbox 为空不等于全局完成；所有 owner credit、worklist、barrier、producer epoch 和 forwarding work 收敛后才能 remark；过期 ticket、重复 ticket 和错误 owner 得到 invariant failure。
  - 接入证据：`RuntimeRawModel`（query 30）升到 schema 15，在同一 `RuntimeRawContractV1` 中并入 `MarkRuntimeContract`（`MARK_SCHEMA = 1`，profile `mosaic-mark` revision 1，派生键 `gugu-mark-runtime-v1`）：每 owner 单 consumer（`mailbox_consumers = 1`）、8 个 shard（与 `OWNER_INBOX_SHARDS` 同源）、`owner(8) | counter(24)` 的 credit id、credit 池上界「常驻 message node 容量 + 根槽数」（在飞 ticket 占一个 non-moving node，根 seed 不占 node 但每根槽每 cycle 至多一次，因此池耗尽即契约违约），并登记六个 cycle 状态 `idle`/`snapshot`/`marking`/`converging`/`remark`/`complete`、三个 credit 转移 `acquire`/`consume`/`return`、六类 root snapshot 参与者 `producer-stop-epoch`/`remote-consumer`/`root-slice`/`region-registry`/`handle-access-guard`/`local-worklist`、七项收敛条件 `local-worklist`/`published-batch`/`mailbox`/`barrier-buffer`/`producer-epoch`/`forwarding-work`/`pending-credit` 与「条件 → credit 来源」绑定；verifier 要求绑定来源的并集**恰好**覆盖 credit 来源目录（既不遗漏也不重复绑定）、`owner_bits + counter_bits == 32`、池为正，并逐字段校验 `MarkMailboxHead`（64 byte / align 64）、`MarkCreditHead`（64 / 64）与 `MarkTerminationRecord`（72 / 8）三条 record 的 size/align/offset 与 `resources/runtime/mark.gg` 的 Gugu 布局（`runtime/mark_layout.rs::verify_source` 复用同一 entry 走查，空 package 只保留 machine 布局校验）。`message.rs` 在既有 non-moving node pool 上新增第四个消息族 `MarkTicket`（族判别 3），`MarkTicket` 的 14 个字段只允许目标 arena descriptor、arena 内 header 偏移、source block、cycle/topology epoch、credit 与 bytes，车道为 `descriptor_unit = target_arena(32) | target_offset(32)`、`bytes_epoch = bytes(32) | topology_epoch(32)`、`payload_low = cycle_epoch(64)`、`payload_high = credit(32) | source_block(32)`，独立 integrity 派生键 `gugu-mark-ticket-integrity-v1` 取摘要字节 8..12（card 族取 0..4、region 族取 4..8，三族不共用前缀），`stage_mark_ticket` 复用同一 producer gate、staging chain 与 queue-page grace；`model.rs` 新增 `Credit`/`SourceBlock` 字段种类与 `MessageSchemaV1::mark_ticket()`，`required_fields` 对 mark 族要求 `UnitIndex`/`Credit`/`SourceBlock`，`verify_family` 沿用「禁带地址」检查。`pacing_schema.rs` 的 credit 目录从五个扩到九个（新增 `mark-credit`/`mark-mailbox`/`mark-worklist`/`forwarding-work`）并升到 `PACING_SCHEMA = 3` / revision 3，`GcWorkCounters` 增加 `marks`，因此「本 cycle 真实完成的工作」把整堆标记量计入同一滑动窗口。`MarkDemand`（根站点、屏障站点、shared 站点、edge delta 站点）完全由 `GcMetadataDemand.root_range_count`、`BarrierDemand.card_mark_sites`/`edge_summary_sites` 与 `LocalHeapDemand.shared_sites` 推导，不新增 LIR 遍历，跨段相等性由 `RuntimeRawContractV1::verify` 强制。`runtime/mark.rs` 的 `MarkPlane` 是契约的确定性对偶：`MarkCredit` 以稠密 `states` 承载 InFlight/Done/Returned，编号在一次 cycle 内不复用，因此重复 ticket 落在 `CreditNotInFlight`、未 consume 就归还落在 `CreditNotDone`、超池落在 `PoolExhausted`；`MarkMailbox` 单 consumer、`consume`/`forward` 在空时失败；`RootSnapshotGate` 记录每个 owner 的六位掩码，`ready`/`missing` 决定能否进入 mark，直接重复登记同一项即 `DuplicateConfirm`；`MarkTermination` 由真实观测与 credit 账本推导七个条件并给出 `converged`/`blocking`；`MarkError` 目录覆盖 `UnknownOwner`/`PoolExhausted`/`CreditNotIssued`/`CreditNotInFlight`/`CreditNotDone`/`CycleOutstanding`/`StaleCycle`/`StaleTopology`/`SelfTicket`/`MailboxEmpty`/`DuplicateConfirm`/`SnapshotNotOpen`/`SnapshotIncomplete`/`CycleState`。`runtime/local_heap.rs` 修掉 mark 位图的陈旧位缺陷（位图是一 granule 一位，epoch 切换只能清本 block 的 granule 区间；按字节累加会让 block ≥ 1 清掉 block 0 的位并保留自己的陈旧位，使下一个 cycle 把同一对象误判为已标记），并新增 world mark pass 所需的四个原语：`begin_mark_cycle`（每个 arena 的 mark epoch 与 cycle 一起前进）、`mark_object`（test-and-mark，重复标记返回 `None`）、`trace_pointers`（对象图 → `(payload 偏移, 值)`）、`ticket_identity`（地址 → arena descriptor / header 偏移 / block）与 `object_at_ticket`（ticket 身份 → 目标对象，descriptor 不属于本 heap、偏移越过容量或 granule 无 object-start 都以 invariant 拒绝），`sweep_unmarked` 公开为 sweep 阶段入口，`LocalHeap::collect_major` 被 world 级 driver 取代。`world/mark_impl.rs` 把整条链接到真实 arena：`configure_gc(&RuntimeRawContractV1)` 一次配置 LocalHeap 与 mark 平面（credit 池上界同时依赖 `policy` 与需求，无法从单独的 LocalHeap 契约得到）；`begin_mark_cycle` 固定 cycle/topology、授权 credit、打开 grace、推进全部 owner 的 mark epoch，再由 `confirm_snapshot_all` 做六类**真实动作**（交出未发布 staging、在 snapshot 边界彻底排空 inbox、按 owner 分片登记根、检查 region registry 无在途移交、要求 access guard 为 0、清空并登记本地 worklist）；确认动作幂等，重入同一 cycle 只补缺项，直接重复登记才是失败。`seed_mark_roots` 从根槽与 remembered-set dirty card 入队；`drain_mark_worklist` 在同一 owner 上下文内标记并扫描，跨 owner 引用不写对方 arena，而是发布一条只带稳定身份的 `MarkTicket`，目标 owner 经 `RawWorld::service` 的消息族分派进入 `service_mark_ticket`，校验目标身份、integrity、`resolve` 结果（`Forward` 时复用同一个 credit 转发、`Retired`/`Unknown` 失败）、cycle/topology 身份与目标对象有效性，再 `consume` credit 并把对象入队；`settle_owner` 归还 Done credit，一轮无任何进展即退出，轮数上限把真正的不收敛变成 invariant failure 而不是死循环。`mark_termination` 的观测全部来自真实结构：worklist 深度、未落地 staging、barrier buffer 键数、在途 region 移交与转发中的 ticket、producer epoch 确认数；`pressure_drain` 的 cycle 路径先跑 mark pass，再以 `mark_converged` 作为 `remark` 的第二道门禁——mark 未收敛时只发布 continuation、保持 barrier 开启、不推进 epoch，收敛且 credit 收敛后才 `finish_mark_cycle` 并对全部 owner 执行 `sweep_owner`。`ImagePlan`/`-Zdump-runtime`/CLI JSON 报告 `mark-contract-fingerprint`、`mark-runtime`、`mark-cycle-states`、`mark-conditions`、`mark-snapshot-participants`、`mark-credit-pool`、`mark-mailbox-consumers`、`mark-ticket-fields`、`mark-records` 与 `mark-demand`，dump 输出 `mark schema=1 profile=mosaic-mark revision=1`/`mark-cycle-states`/`mark-conditions`/`mark-condition-sources`/`mark-credit-sources`/`mark-credit-transitions`/`mark-snapshot-participants`/`mark-mailbox`/`mark-record`/`mark-field`/`mark-ticket-fields`/`mark-demand`/`mark-fingerprint`，`runtime-message` 行增加 `mark-ticket-fields=14`，CLI 侧新增 `mark-*` 键并断言 `mark-demand` 与三份既有需求覆盖同一站点集合。`runtime/mark_tests.rs` 覆盖 owner 位宽越界拒绝、credit 生命周期（InFlight 不可归还、重复 ticket 报 `CreditNotInFlight`、编号不复用、池耗尽报 `PoolExhausted`）、snapshot gate（未收齐拒绝 release、重复确认、越界 owner、关闭后拒绝）、七项条件与 `blocking` 顺序、收敛后可完成；`world/mark_tests.rs` 用真实 arena 覆盖跨 owner ticket 的 credit 轨迹（`acquire`→`consume`→`return`、mailbox 清空、四个 mark credit 来源归零）与 owner 内环不产生 ticket 且重复 cycle 收敛；`heap_tests::mark_bitmap_clears_only_the_marked_block` 用跨两个 block 的对象锁定位图回归（修复前第二个 cycle 会因陈旧位漏标记）；`pacing_tests::pressure_cycle_runs_mark_pass_and_sweeps_when_gc_is_configured` 验证 cycle 真正跑 mark、按收敛结果 remark、并在收敛后回收未标记的 old 对象。本阶段边界：跨 block `EdgeDelta` 传输与 block candidate/SCC（阶段 45）、`SharedHeap` stable handle 与 forwarding grace（阶段 46）、block/arena 交还 provider 与 decommit（阶段 47）不在范围内；`MarkTicket` 已进入同一 credit 平面，因此 mailbox 为空只说明没有待消费 ticket，credit 必须实际归还才算收敛。

- [x] **阶段 45：实现 EdgeDelta、block candidate 与 SCC fallback**（复杂度：5）
  - 依赖：阶段 40、44。
  - 实现跨 block EdgeAdd/EdgeDrop 聚合、generation/epoch 排序、candidate block lease、exact local trace、受限 trial deletion/SCC fallback。
  - 验收：EdgeDelta 只作 candidate，不当对象级引用计数；add/drop 顺序不丢边；循环垃圾在 block candidate 失败时通过 SCC 规则处理，不错误释放仍可达对象。
  - 接入证据：`RuntimeRawModel`（query 30）升到 schema 17，在同一 `RuntimeRawContractV1` 中并入 `EdgeRuntimeContract`（`EDGE_SCHEMA = 1`，profile `mosaic-edge` revision 1）：十个候选相位（`discover`/`trace`/`trial`/`scc`/`validate`/`commit`/`sweep`/`release`/`complete`/`invalidate`）、四个 block 候选状态（与 `HeapBlockRecord.state` 判别值同源）、`EDGE_CANDIDATE_QUANTUM = 4096`、每 processor 512 项 edge scratch 与每次写入至多 2 项边变更（与 `SHADE_SLOTS_PER_WRITE` 同源），并逐字段校验 18 个 `EdgeDelta` 字段；`EdgeDemand` 由 `BarrierDemand.edge_summary_sites` 派生并与 `MarkDemand.edge_delta_sites` 交叉校验，`reserve_slots` 取 barrier 的 `shade_slots`。edge summary 是**多重计数**：同一 block 对的重复 add 累加、drop 只扣减，扣到零且无保留记录才清退 block 对（`edge_pairs` 的已应用计数与 `EdgeStats.retired_pairs` 提供真实账本）。真实源码回归：`runtime/fixtures/edge_nodes.gg`（`EdgeNode { value: uint, next: &EdgeNodeTail }`）编译出 6 个边站点，测试用镜像计划的 LocalHeap/GC metadata/edge 契约配置 `RawWorld`、按真实类型表在 old generation 建立跨 owner 引用，断言一次写入发布一条差量、目标 owner 消费后 `incoming_applied == 1`、credit 结算为零；`world/edge_tests.rs::source_edge_node_fixture_drives_old_placement_edge_deltas` 在 Linux 与 Windows 两个目标断言夹具派生需求与契约指纹。双 owner 双轮情景（`candidate_tests.rs::two_owner_double_cycle_with_local_invalidation_converges`）在两条跨 owner 环上验证：按 block 对聚合的两条独立边计数各自为 2、mutator 局部失效让在飞 job 失效且不触发释放、两轮 cycle 后边计数守恒、活环对象与根槽保持可解析。性能情景由 `benches/edge_candidates.rs`（`EdgeCandidateHarness`，输入是真实 `Compilation`）承担：8 轮 × 200 节点的跨 owner 写入、发布、标记与候选判定在 release 下不破坏守恒（`applied == 跨 owner 写入数`、ticket 全部结清、活块零释放）。同一 harness 暴露的 `release_block` granule 换算错误已修复：block 起始 granule 必须由 line 起点乘每 line 字节数换算，回归见 `world/heap_tests.rs::releasing_a_block_preserves_earlier_block_object_start_bits`。

- [ ] **阶段 46：实现 SharedHeap stable handle 与 forwarding grace**（复杂度：5）
  - 依赖：阶段 39、44、45。
  - 实现 handle slot、resolve/access guard、forwarding lease、payload copy、slot 线性化切换、pin/mark/access grace 和共享字段 barrier。
  - 验收：guard 结束前旧 payload 保持有效；direct pointer 不逃出 guard；重复 resolve、过期 handle、forward generation 和 slot 状态均被 verifier 检查；共享访问额外成本不扩散到 LocalHeap。

- [ ] **阶段 47：实现 GC block return 与全链路 owner 回收**（复杂度：5）
  - 依赖：阶段 30–32、41–46。
  - 将 sweeping/evacuation 完成的 block、line-run、arena、large mapping 和 resource block 接入 owner-directed return；完成 lease、pending message、handle grace、queue-page grace 后才能 OwnedFree。
  - 验收：空 block 不因低 live ratio提前释放；block/arena/large/resource 各自经过 metadata verifier；return、forward、consume、decommit 计入 exactly-once 和 pressure 账本。

- [ ] **阶段 48：实现 checked pointer compression**（复杂度：4）
  - 依赖：阶段 46、47、38。
  - 实现 compressed cage reservation、offset/generation decode、compressed root map、FFI pin/copy 交接和 target capability 检查。
  - 验收：cage 外地址、越界 offset、错误 generation、非 canonical pointer 和非法 FFI 保存全部失败；未启用 cage 时保留等价 full-pointer 语义；统计记录 decode 次数。

- [ ] **阶段 49：实现 radix routing profile**（复杂度：4）
  - 依赖：阶段 30、35、47。
  - 在 direct owner routing 之外实现有限 levels、固定 `2^k` bucket、forwarding record、hop 上限、maintenance epoch 切换和旧 topology drain。
  - 验收：默认 direct mode 不分配 producer×owner 队列矩阵；radix 模式只在 profile 开启，旧 epoch 最终进入新 token/domain，超 hop 或错误 forward 得到稳定 fatal/diagnostic。

- [ ] **阶段 50：实现 raw link provenance 与 release 安全 profile**（复杂度：4）
  - 依赖：阶段 30、47、48、49。
  - 实现 per-domain secret + slot address 编码、canonical/alignment/range/class/owner/generation 校验、debug poison/double-return/full-chain 检查和 release provenance 检查。
  - 验收：链损坏、伪造 link、重复返还、跨 owner/class 释放和旧 generation 全部被拒绝；random reuse、guard page 和 checked copy 不改变 managed trace 语义。

- [ ] **阶段 51：实现 typed combining 与 topology/range 慢路径**（复杂度：4）
  - 依赖：阶段 32、35、49、50。
  - 实现稳定 operation record、固定 tag、标量参数、response slot、有限同类合并、无争用 atomic fast path、owner inbox/MCS wait path。
  - 验收：combiner 不执行用户 closure、drop glue、await 或跨 safepoint 持锁；GlobalRange、extent coalescing、topology 和 trim 操作结果可重复；分配/mark/resolve 热路径不经过全局 combining lock。

## 五、x86_64 后端、镜像与外部边界

- [ ] **阶段 52：实现 target descriptor、数值 lowering 与 x86 encoder**（复杂度：5）
  - 依赖：阶段 28、29、32。
  - 实现 `x86_64-linux`/`x86_64-windows` target descriptor、x86-64-v1/SSE2 指令编码、整数/浮点/NaN/shift/conversion、V128、原子/fence 和指令 verifier。
  - 验收：未登记目标或超出 baseline 的指令不能写出；机器结果与类型规范一致；编码器不调用系统 assembler；relocation、immediate、address mode 和原子约束均有 byte-level fixture。

- [ ] **阶段 53：实现 instruction selection、内部 ABI 与 block layout**（复杂度：5）
  - 依赖：阶段 52、28。
  - 实现封闭 `X64Inst`、GIR/LIR 到机器指令选择、内部保留寄存器、参数/返回约定、hot/cold block、branch relaxation 和 mangling。
  - 验收：`r14/r15`、栈对齐、panic/slow path 冷区、runtime fast path、owner inbox atomic 序列符合 backend 契约；内部 ABI 只在同一 CompilerIdentity 镜像内使用。

- [ ] **阶段 54：实现线性扫描寄存器分配与固定 frame**（复杂度：5）
  - 依赖：阶段 53、29。
  - 实现 GPR/XMM interval、liveness fixed point、spill/split、parallel copy、stack slot、outgoing args、Windows shadow space 和 prologue/epilogue。
  - 验收：跨 call 值使用正确保留寄存器；copy cycle 使用规定 scratch；普通 managed frame 不动态 alloca；所有 frame size、alignment、saved register 与 stack map 输入一致。

- [ ] **阶段 55：接入 stack map、panic landing 与 unwind table**（复杂度：5）
  - 依赖：阶段 38、54。
  - 在寄存器/栈布局完成后生成 `.gugu.stackmap/.gugustk`、unwind function/landing record、safepoint/root map、source record，并执行机器码与 metadata 联合验证。
  - 验收：map 去重按完整 bytes 字典序；PC range 不重叠；strip 不移除运行时必需 metadata；异常、suspend、bridge、poll、stack copy 的根与 landing chain 可被 runtime 消费。

- [ ] **阶段 56：实现 ELF64 static PIE 与 Linux rt0 写出**（复杂度：5）
  - 依赖：阶段 52–55、阶段 33。
  - 实现逻辑节到 ELF segment、无 libc static PIE、`AT_PHDR` load bias、自重定位、RELRO、显式动态 FFI 的 PT_INTERP/GOT/PLT、归档抽取和 Linux syscall stub。
  - 验收：无动态导入的镜像不依赖系统 linker/loader 语义；未知/越界/重复 relocation、非法节权限和入口错误在写出前失败；hello、panic、GC、channel 程序能在 Linux 启动并退出。

- [ ] **阶段 57：实现 PE32+、Windows rt0 与导入导出**（复杂度：5）
  - 依赖：阶段 52–55、阶段 33。
  - 实现 PE/COFF section、IAT、export、base relocation、ASLR/NX、高熵 ASLR、Windows unwind、staticlib/cdylib/exe writer、ntdll/kernel32 薄导入和 Windows syscall/错误映射。
  - 验收：不链接 CRT、不扫描 syscall 号、不搜索宿主 DLL；shadow space、callee-saved 寄存器、SEH/console handler、导出 panic abort 和 i128 C ABI 错误符合规范。

- [ ] **阶段 58：实现 FFI bridge、foreign effect、asm 与 OS 适配**（复杂度：5）
  - 依赖：阶段 19、33、35、38、52–57。
  - 实现普通 `ForeignBridge`、`DirtyCpu`、`ForeignLeaf`、外部线程接入、errno/last-error 捕获、CStr/CString、managed asm/global asm/naked/dirty native 和 Linux/Windows poller syscall 接缝。
  - 验收：bridge root、processor lease、BlockingBridge/dirty credit、pin、回调和 unwind 边界可验证；opaque native 不伪造 stack map；外部线程不能直接操作 Gugu 协程或 GC metadata。

## 六、标准库与公开运行时 API

- [ ] **阶段 59：实现核心 prelude、Option/Result 与错误模型**（复杂度：3）
  - 依赖：阶段 12、17、27。
  - 实现 `std.option`、`std.result`、`std.error`、`std.cmp`、`std.ops`、`std.iter`、`Print/Debug/Clone/Eq/Ord/Hash/Default` 基础实现和 `must_use`。
  - 验收：数组/元组/Option/Result 的派生与条件 trait 实现符合规范；`?`、Try、错误链、Print 与格式化可用于 compiler/runtime 自举；丢弃 Result/Option 触发 lint 而非类型错误。

- [ ] **阶段 60：实现 string、Bytes、ByteBuffer 与 Unicode**（复杂度：4）
  - 依赖：阶段 27、59。
  - 实现 UTF-8 COW string、Bytes 快照、ByteBuffer seal/thaw/split、Unicode decode/encode、大小写/case fold、NFC/NFD/NFKC/NFKD、grapheme/word/line segmentation。
  - 验收：所有修改 API 的 byte/scalar 边界、负值、非法 UTF-8、lossy replacement 和 COW 分离行为可确定测试；Unicode 数据版本进入 compiler/toolchain identity。

- [ ] **阶段 61：实现 fmt、Print 与哈希集合**（复杂度：4）
  - 依赖：阶段 59、60。
  - 实现 `std.fmt` Formatter、静态 f-string format code、Debug/Binary/Octal/Hex/Exp trait、稳定 hash、`HashMap/HashSet/SecureHashMap/SecureHashSet/BTreeMap/BTreeSet/SmallMap`。
  - 验收：格式码在编译期解析；集合的 `with_ref/for_each_ref` 生成 scoped view，callback 不能结构性修改或逃逸；哈希选择和迭代输出按规范区分稳定序与实现顺序。

- [ ] **阶段 62：实现 std.io 基础、读写 trait 与 Path**（复杂度：4）
  - 依赖：阶段 31、37、58–60。
  - 实现 Read/Write/Seek/BufRead、read_exact/write_all/copy/read_to_end/read_to_string、I/O Error、ByteBuffer 接口、OsString/Path、取消和 timeout 接缝。
  - 验收：短读/短写/EOF/WriteZero、取消和错误链语义稳定；Path 不隐式访问文件系统、不丢失非 Unicode 字节；阻塞操作只挂起协程。

- [ ] **阶段 63：实现 std.fs 与目录迭代**（复杂度：4）
  - 依赖：阶段 31、32、58、62。
  - 实现 File/OpenOptions/Metadata/Permissions/DirEntry/ReadDir、open/read/write/metadata/exists/canonicalize/read_dir/create/remove/rename/copy/link/symlink/read_link。
  - 验收：OpenOptions 非法组合、symlink、TOCTOU、跨文件系统部分完成和 permission 错误映射为具体 `io.Error`；不提供伪原子组合事务；文件资源 release exactly once。

- [ ] **阶段 64：实现 std.net 地址、resolver 与监听器**（复杂度：3）
  - 依赖：阶段 31、37、58、62、63。
  - 实现 IPv4/IPv6/SocketAddr、ToSocketAddrs、system Resolver、DNS 错误、TcpListener/UdpSocket bind/accept 基础和 cfg 控制的 Unix domain address。
  - 验收：IPv6 `[addr]:port`、ASCII DNS、NUL/非 ASCII/非法端口、IPv6 bind mode、backlog 和 Unix path 边界均有固定测试；不创建第二层 DNS cache。

- [ ] **阶段 65：实现 std.net transport、TCP/UDP 与 Unix socket**（复杂度：4）
  - 依赖：阶段 64、35–37、62。
  - 实现 TcpStream、UDP datagram、recv_from/recv_packet、方向并发、截断规则、timeout/cancel、local/peer addr、UnixStream/UnixListener/UnixDatagram 的目标条件。
  - 验收：每个 datagram 恰消费一次，destination 不足明确 truncated，短写/MessageTooLarge/Closed/NotConnected 不被伪装；poller 与 BlockingBridge 选择遵守 runtime 规则。

- [ ] **阶段 66：实现 std.process 与 std.env**（复杂度：4）
  - 依赖：阶段 31、37、58、62、63。
  - 实现 Command/ShellCommand/Child/Stdio/Output/ExitStatus、spawn/wait/try_wait/kill/detach/close、argv、环境锁、env set/remove、虚拟 cwd。
  - 验收：Command 不经过 shell，ShellCommand 显式按目标 shell；stdout/stderr 并行排空；wait/close/detach 语义、环境快照、NUL/大小写规则和子进程资源回收符合规范。

- [ ] **阶段 67：实现 std.time、std.runtime 与 std.signal facade**（复杂度：4）
  - 依赖：阶段 33、35–37、41、58、66。
  - 实现 Instant/SystemTime/Duration/sleep/timeout、runtime stats/GC target/parallelism/trace facade、Linux/Windows signal subscription、合并计数、dropped 和关闭语义。
  - 验收：单调时钟与墙钟边界、超时只取消当前操作、signal handler 不运行用户代码；所有 setter 在 Terminating 返回正确错误；RuntimeStats 字段与内部账本一致。

- [ ] **阶段 68：实现 std.mem、std.ptr、std.ffi、std.src、std.syntax/build/hint**（复杂度：5）
  - 依赖：阶段 19、21、22、31、32、58、59。
  - 实现 LocalArena/SyncArena/pin/MaybeUninit、pointer intrinsic、CStr/CString、source location、syntax parser facade、build task API、embed_file 和 unreachable 等 lang item。
  - 验收：arena 不接受 resource 类型；pin 生命周期、MaybeUninit 初始化、source location、build emit/rerun/link/define API 与 capability registry 统一；这些 API 不泄漏 compiler private representation。

## 七、测试、构建任务、生态与工具闭环

- [ ] **阶段 69：实现测试/文档测试/benchmark harness**（复杂度：5）
  - 依赖：阶段 20、25、36、59、67。
  - 实现 `cfg(test/bench)`、`#[test]`、`should_panic(eq)`, `ignore`、稳定测试身份、并行用户协程、panic 捕获、doctest 围栏、`#[bench]`/Bencher/black_box。
  - 验收：测试收集顺序确定、执行可并行、报告按身份排序；测试后代协程归属正确；失败不终止其它测试；ignored/零匹配成功；bench 不缓存历史测量。

- [ ] **阶段 70：实现 `gugu doc`、API 文档与 doctest 生成**（复杂度：4）
  - 依赖：阶段 09、20、59、69。
  - 生成公共 module/function/type/field/trait 文档、缺失文档 lint、源位置链接、doctest source context、依赖文档和 `--open/--no-deps` 视图。
  - 验收：文档测试失败指向原始围栏位置和宏展开链；生成内容不读取 target/cache 临时副本；公共 API 的错误、panic、资源和并发限制可追踪到规范章节。

- [ ] **阶段 71：完成 action cache、target 物化与 cache 子命令**（复杂度：4）
  - 依赖：阶段 06、11、24、56、57、69。
  - 实现 action cache LRU、clean/cache clean/cache gc/cache verify/cache dir、target/bin/lib/tests/benches/examples/generated/build-logs 物化、strip 末端 action。
  - 验收：正在读写的 entry 不被回收；损坏 entry 隔离重算；strip 不删除 C export、GC metadata、stack map、source/unwind；clean 默认不误删全局依赖缓存。

- [ ] **阶段 72：实现 build.gg、权限门与生成模块闭环**（复杂度：5）
  - 依赖：阶段 05、06、58、66、68、71。
  - 实现 build.gg host 编译、std.build.run、out_dir、rerun 记录、emit_module、define_cfg、link metadata、permission advisory gate、TTY/非交互授权和授权失效。
  - 验收：build task 在 target 源码前运行；输出协议与 rerun 输入完整；权限门不被描述为 sandbox；`--permission` 缺授权的非交互构建失败；生成模块进入 cfg/name/type/HIR/cache 闭包。

- [ ] **阶段 73：完成 CLI 命令端到端编排**（复杂度：5）
  - 依赖：阶段 04–06、09、56、57、69–72。
  - 将 `new/init/build/check/run/test/bench/fmt/doc/clean/add/remove/update/tree/vendor/package/publish/yank/login/cache/explain/version/help` 接入同一 action graph、配置覆盖、日志、目标选择和进程传参。
  - 验收：每条命令的成功/代码错误/用法错误/内部错误退出码符合规范；单文件模式禁用 package 参数；命令不会绕过锁、缓存、权限、诊断或镜像验证。

- [ ] **阶段 74：实现 package 归档、registry Protocol v1 与发布签名**（复杂度：5）
  - 依赖：阶段 05、06、71–73。
  - 实现 config/index/download/publish/yank API、HTTPS/TLS、package ID、规范归档内容流、SHA-256 checksum、Ed25519 可选签名、认证脱敏、镜像、allowlist 和稳定 registry 诊断。
  - 验收：归档排除凭据/lock/target/vendor/VCS/未声明生成物；已存在 package ID checksum 不可覆盖；签名严格策略、yank、offline/vendor、redirect/source identity 和损坏缓存行为符合发布规范。

## 八、生产级验证与文档交付

- [ ] **阶段 75：完成双目标 ABI 与镜像一致性套件**（复杂度：5）
  - 依赖：阶段 19、52–58、63–68、74。
  - 建立 Linux/Windows 的 C 对照程序、repr(C)/transparent/packed/union/enum、SysV/MS x64 参数返回、sret、i128、TLS、导入导出、ELF/PE section/relocation fixture。
  - 验收：覆盖 0/1/2/16/17-byte aggregate 边界、shadow space、栈对齐、panic abort、外部线程接入、pin/bridge/leaf/dirty 和 strip 后 metadata；所有 fixture 使用确定性替身，不修改输入规避失败。

- [ ] **阶段 76：完成可复现性、供应链与安全审计**（复杂度：5）
  - 依赖：阶段 06、11、25、39、50、71、74、75。
  - 对源码/锁图/宏/late const/摘要/cache/镜像/归档执行重复构建、并发构建、损坏输入、路径变化、凭据泄漏、恶意 metadata、伪造 pointer/link、ABI 越界和 fuzz corpus 审计。
  - 验收：同一 CompilerIdentity 与输入产生语义等价且可重现的结果；任何不可信 cache/registry/metadata 输入都先验证再分配/执行；诊断、trace、package 和镜像不含 token、私钥、宿主绝对路径或 managed 裸地址。

- [ ] **阶段 77：完成发布 workload 性能与延迟门禁**（复杂度：5）
  - 依赖：阶段 35、40–51、56–68、75、76。
  - 建立 release generated-code、百万 coroutine、channel/select、I/O、FFI、COW/collection、GC pressure、symmetric/asymmetric owner return 和双目标端到端 workload；记录吞吐、RSS、GC pause、remark/evacuation、remote hop、cache hit 与 tail latency。
  - 验收：性能参数只由真实 workload、确定性 model 和 release 镜像数据调整；确认 direct mode、MosaicThroughput、MosaicLowLatency、radix/security profile 的代价；不得用针对单一 benchmark 的特殊路径宣称生产级性能。

- [ ] **阶段 78：完成用户教程与规范参考手册**（复杂度：4）
  - 依赖：阶段 59–74 的公开 API 基本稳定。
  - 补齐 `docs/src/guide/`：语言入门、值/句柄/资源、并发/取消、comptime/宏、包构建、FFI/unsafe、测试和部署；补齐 `docs/src/reference/`：语法速查、类型/trait、标准库 API、CLI、诊断码、运行时配置、ABI 与兼容性矩阵。
  - 验收：每个示例可由 test/doc build 编译；教程不把 runtime 内部细节写成语言语义；参考手册链接到唯一规范章节；更新 `docs/src/SUMMARY.md` 后 mdBook 无断链、无未决占位文本。

- [ ] **阶段 79：执行最终生产发布门禁**（复杂度：5）
  - 依赖：阶段 01–78 全部完成。
  - 执行全 workspace fmt/build/nextest、mdBook、双目标 smoke、CLI 全命令、离线/冻结/签名/损坏缓存、panic/fatal/OOM、GC/stack/scheduler、FFI/ABI、package publish/yank 和文档示例矩阵；整理版本、CompilerIdentity、runtime tuning profile、迁移说明与发布产物清单。
  - 验收：无 warning、无未实现路径、无占位内容、无未解释规范差异；生产镜像不依赖系统 LLVM/assembler/linker；所有公开章节、内部章节、ADR、测试、教程和参考手册之间可追踪；此阶段通过后删除 `docs/roadmap.md`，路线图不作为文档或发布产物保留。

## 依赖主线

```text
工程/CLI/源码
    -> 词法/parser/cfg/HIR
    -> 类型/trait/comptime/宏/单态化/TypeId
    -> GIR/LIR/优化
    -> runtime raw/stack/scheduler
    -> GC/barrier/owner messaging
    -> x86_64 backend/ELF/PE/FFI
    -> std/test/build/publish/CLI
    -> 双目标验证/安全/性能/教程参考/发布门禁
```

阶段之间可以在不违反契约的前提下并行实现，例如前端类型系统与 raw range fake、标准库纯值模块与后端 encoder；但任何并行工作都必须共享稳定 schema，不能各自建立第二套类型、资源、诊断、ABI、root 或 runtime 状态表示。

## 跨阶段接入矩阵

| 能力层 | 上游入口 | 下游消费者 | 完成证据 |
|---|---|---|---|
| 源码/项目 | CLI、SourceMap、manifest | query、诊断、ActionKey | 真实 package 可重放 |
| AST/名称/类型 | parser、cfg、定义表 | HIR、comptime、GIR | `Validated` 严格冻结 |
| comptime/宏 | HIR、capability registry | 前端重入、可达性、缓存 | 展开后重新走前端 |
| GIR/LIR | Validated HIR、布局 | verifier、backend、stack map | 每阶段 verifier 通过 |
| runtime/GC | rt0、range、scheduler | lowering、镜像 metadata | 启动与生命周期 smoke |
| 标准库 | intrinsic、ABI | compiler 自举、用户 API | compiler/runtime 自举 |
| backend/image | LIR、target descriptor | CLI emit/run、cache | 双目标镜像 smoke |
| CLI/生态 | action graph、cache、权限 | 命令、registry、报告 | 命令矩阵与失败路径 |

“实现”阶段必须同时写明输入、输出、调用入口、失败传播和下游消费者；仅新增模块、dump、fixture 或孤立单元测试不得标记完成。
