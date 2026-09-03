# Gugu 生产级完整实现路线图

## 使用说明

这份路线图覆盖当前仓库 `docs/src/spec/` 中的公开语言、工具链、标准库、运行时与 ABI 规范，以及 `docs/src/internals/` 中的编译器和 runtime 内部契约。路线图按可交付垂直切片组织，同时显式区分实现阶段与跨阶段里程碑。

复杂度采用 1–5 级，并表示实现难度与所需 AI 推理能力：`1` 是局部、机械、低耦合任务；`2` 是边界清晰的单组件任务；`3` 是一次会话可完成的标准中型子系统；`4` 涉及多个契约、并发不变量或安全边界，需要更强模型与更完整验证；`5` 是跨层集成、GC/调度/后端/ABI/发布门禁等高风险任务，需要最强模型、分阶段验证，通常还应继续拆成 `2/3` 级子阶段。复杂度不是完成状态，所有条目仍使用 `[ ]`。

路线图的权威依据：

- 公开语言契约：[规范总览](src/spec/overview.md)、[词法](src/spec/lexical.md)、[形式语法](src/spec/syntax.md)、[类型](src/spec/types.md)、[声明](src/spec/declarations.md)、[表达式](src/spec/expressions.md)、[模式](src/spec/patterns.md)、[函数](src/spec/functions.md)、[trait](src/spec/traits.md)、[传递](src/spec/passing.md)、[内存](src/spec/memory.md)。
- 工具与平台契约：[程序模型](src/spec/program-model.md)、[包与构建](src/spec/packages-builds.md)、[发布生态](src/spec/publishing-ecosystem.md)、[工具链 CLI](src/spec/toolchain-cli.md)、[平台 ABI](src/spec/platform-abi.md)、[运行时](src/spec/runtime.md)、[标准库](src/spec/standard-library.md)、[测试](src/spec/testing.md)、[unsafe](src/spec/unsafe.md)、[格式化](src/spec/format-style.md)。
- 编译器与 runtime 内部契约：[AST/HIR](src/internals/ast-hir.md)、[comptime 分析](src/internals/comptime-analysis.md)、[GIR/LIR](src/internals/gir-lir.md)、[单态化与缓存](src/internals/monomorphization-cache.md)、[栈图](src/internals/stack-maps.md)、[GC 元数据](src/internals/gc-metadata.md)、[内存消息](src/internals/memory-messaging.md)、[调度器](src/internals/scheduler.md)、[x86_64 后端](src/internals/backend.md)。
- 关键架构约束：[ADR-0001](adr/0001-static-closed-world-runtime.md)、[ADR-0002](adr/0002-syntax-concurrency-memory.md)、[ADR-0003](adr/0003-passing-semantics.md)、[ADR-0004](adr/0004-never-patterns-diagnostics.md)、[ADR-0005](adr/0005-impl-trait-try-test-coroutine-local.md)、[ADR-0006](adr/0006-closed-world-type-id.md)、[ADR-0008](adr/0008-platform-abi-reference.md)、[ADR-0009](adr/0009-owner-directed-memory-messaging.md)、[ADR-0010](adr/0010-comptime-source-macros.md)。

现状基线是 [`gugu-cli`](../crates/gugu-cli/src/main.rs)：当前入口只解析 clap 参数并打印示例文本；`docs/src/guide/` 与 `docs/src/reference/` 也尚未形成完整内容。因此路线图从编译器 bootstrap、runtime 引导和规范测试基础开始，最终以双目标、闭世界、无系统 linker 依赖、可复现和可发布为生产级门槛。

## 每阶段完成定义

1. 实现只进入该阶段所属的真实归属层；不得通过弱化 fixture、跳过 snapshot 或增加平行兼容接口掩盖缺口。
2. 公开行为与对应 `docs/src/spec/` 章节一致，内部表示与对应 `docs/src/internals/` 章节一致；若发现规范缺口，先在同一阶段修订规范并同步 `docs/src/SUMMARY.md` 导航。
3. 测试使用固定输入、确定性顺序、进程内替身和明确的失败边界；网络、真实子进程、压力负载和随机性能测量放到专门的 bench/手工验证。
4. 阶段验证通过后才能将该阶段从 `[ ]` 改为 `[x]`。工作区级验证使用 `cargo fmt --all --check`、`cargo build --workspace` 和 `cargo nextest run --workspace`；文档构建使用 `mdbook build -d target/book`。
5. 代码或规范提交遵守仓库的 `docs/.commit` 跟踪规则；路线图本身不代表任何阶段已实现。

## 一、工程与前端基础

- [ ] **阶段 01：建立 compiler/runtime 工程骨架**（复杂度：4）
  - 依赖：无。
  - 建立 Rust compiler bootstrap、`gugu` CLI、目标描述、诊断、前端、IR、后端、runtime 资源的清晰模块边界；登记用 Gugu 编写的标准库/runtime 源树，明确 rt0、必要 intrinsic 与 Gugu runtime 的边界，禁止维护第二套语义等价 Rust runtime。
  - 验收：空 package、单文件入口和一个最简单 `main` 拥有端到端 action graph；无效阶段不会写出镜像；架构文档与模块清单能够定位每条公开规范的实现归属。

- [ ] **阶段 02：实现 CLI 全局参数与输出骨架**（复杂度：2）
  - 依赖：阶段 01。
  - 实现单一 `gugu` 可执行入口、全局参数优先级、子命令注册、`text`/`json`/`json-diagnostic-short` 输出信封和退出码 `0/1/2/101`。
  - 验收：无子命令等价于 help，`version` 与 `--version` 一致，非法参数不进入编译，NDJSON 不泄漏绝对路径和凭据；覆盖 CLI 规范中的命令解析表。

- [ ] **阶段 03：实现源码快照、规范路径与 Span 系统**（复杂度：2）
  - 依赖：阶段 01、02。
  - 实现 UTF-8 源码读取、BOM 拒绝、规范化逻辑路径、`SourceSnapshot`、字节/行列映射、宏展开 source context 和稳定文件 ID。
  - 验收：相同输入在不同工作目录、目录枚举顺序和换行环境下产生相同 span 与诊断位置；非法 UTF-8、BOM 和越界 span 有稳定错误。

- [ ] **阶段 04：实现清单、workspace 与 target 发现**（复杂度：3）
  - 依赖：阶段 02、03。
  - 实现 `gugu.toml` 向父目录查找、package/workspace 层级、默认 source root、lib/bin/test/bench/example target 自动发现、host/target 分离和 `foo.gg` 与 `foo/mod.gg` 冲突检查。
  - 验收：package、虚拟 workspace、单文件模式和 target 选择规则与规范一致；未知核心字段、target 重名、入口越界和保留 package `std` 均在编译前失败。

- [ ] **阶段 05：实现依赖解析、SemVer 与锁图**（复杂度：4）
  - 依赖：阶段 04。
  - 实现 path/git/registry source、package identity、SemVer 求解、依赖别名、target 条件、normal/test/build 三域、feature 并集和确定性 `gugu.lock` 编码。
  - 验收：循环依赖、无解版本、source identity 冲突、锁图不一致和 feature 缺失得到稳定错误；锁文件不含绝对路径、token、缓存位置或宿主信息。

- [ ] **阶段 06：实现离线、vendor、checksum 与缓存输入**（复杂度：4）
  - 依赖：阶段 05。
  - 实现依赖源码缓存、归档 checksum 验证、`--offline/--locked/--frozen/--vendor` 组合、vendor mapping、损坏缓存隔离、编译 action key 的完整输入集合和 target 视图目录。
  - 验收：无网络替身下可重放已验证锁图；缺包、checksum 污染、vendor 不一致和 frozen 修改均在目标代码生成前失败；缓存命中与否不改变语义结果。

- [ ] **阶段 07：实现词法分析器与字面量/属性**（复杂度：3）
  - 依赖：阶段 03。
  - 覆盖最长记号、换行续行、嵌套块注释、raw string、整数/浮点/字符/字节/C 字符串、数组/元组记号、属性参数与 `cfg` 词法。
  - 验收：词法 token 带精确 span；禁止 `.5`、`5.`、非法 Unicode scalar、错误转义、未知属性参数和非法记号组合；错误恢复不会把占位节点交给 codegen。

- [ ] **阶段 08：实现递归下降 parser 与 AST arena**（复杂度：4）
  - 依赖：阶段 07。
  - 按形式语法构造稠密 AST，覆盖声明、泛型、类型、块、表达式、模式、`async`、`select`、`try`、`defer`、`comptime source`、FFI 和 asm 节点；实现错误恢复与节点稳定排序。
  - 验收：规范语法示例全部可解析，非法嵌套和优先级得到主/次诊断；解析结果不使用指针作为节点身份，结构 dump 不受线程完成顺序影响。

- [ ] **阶段 09：实现格式化器与 `gugu fmt`**（复杂度：2）
  - 依赖：阶段 03、07、08。
  - 实现规范缩进、换行、尾逗号、use 排序、属性/注释布局、字符串内部字节保留、`--check`、原子写回和 workspace `--all`。
  - 验收：formatter 满足幂等性；解析失败不截断源文件；不会执行 build task、读取缓存或改写 vendor；格式化不会改变 AST、诊断语义、ABI 或 package checksum。

- [ ] **阶段 10：实现 cfg、模块树与定义收集**（复杂度：3）
  - 依赖：阶段 04、08。
  - 实现 host/target cfg 求值、配置裁项、模块声明表、可见性、`use/pub use`、保留名称、稳定 `DefId` 分配和命名空间冲突诊断。
  - 验收：被 cfg 裁掉的项不进入名称解析、类型检查和 codegen；导入循环、私有跨模块导入、大小写路径不一致和重复声明均可确定重现。

- [ ] **阶段 11：实现 query 状态机与内容寻址缓存**（复杂度：4）
  - 依赖：阶段 03、05、06、10。
  - 实现 query 注册表、`Uncomputed/Computing/Complete/Failed/Cancelled` 状态、依赖 fingerprint、结果 fingerprint、BLAKE3 对象、原子发布、损坏校验和确定性依赖排序。
  - 验收：并发请求同一 query 只计算一次；失败不写持久缓存；缓存对象按长度/哈希/版本/IR verifier 校验后才反序列化；上游结果未变时不会传播无意义失效。

## 二、类型与语言语义

- [ ] **阶段 12：实现类型 arena、类型形成与布局基础**（复杂度：4）
  - 依赖：阶段 08、10、11。
  - 实现标量、引用、原始指针、函数、元组、数组、切片、struct/enum/union/newtype、never、透明别名、`repr` 属性、大小/对齐/字段偏移和递归大小检查。
  - 验收：类型变量必须唯一收敛；数组长度、对齐、判别值和 offset 在正确阶段确定；`!` 与 `()`、别名与 newtype、句柄与值类型的区分符合规范。

- [ ] **阶段 13：实现声明、绑定与初始化数据流**（复杂度：3）
  - 依赖：阶段 10、12。
  - 实现 `let`、模式绑定、遮蔽、新槽、函数/结构体/枚举/const/type/static 声明、普通 static 无环初始化、局部 static 延迟初始化标记和所有路径初始化分析。
  - 验收：未初始化读取、模块级 let、static 初始化循环、非法 main 签名和私有字段构造均被拒绝；普通 static 与 coroutine-local/OS-thread-local 的初始化阶段严格分离。

- [ ] **阶段 14：实现表达式、运算符与控制流类型检查**（复杂度：4）
  - 依赖：阶段 12、13。
  - 覆盖 place/value、字段/索引/切片、短路逻辑、整数/浮点规则、显式转换、循环、`if`/`match`/`try` 表达式、返回/分支和 `defer` 注册语义。
  - 验收：左到右求值、块值、never 合流、除零/移位/边界规则和 `?` 出口与规范一致；用户 trait 运算符不会通过隐式转换获得额外候选。

- [ ] **阶段 15：实现模式匹配与穷尽性分析**（复杂度：3）
  - 依赖：阶段 12、14。
  - 实现通配/绑定/引用/字面量/范围/元组/数组切片/结构体/构造器/or/`@`/rest 模式、可驳性、let 链、let-else、守卫与有限域覆盖计算。
  - 验收：被匹配表达式只求值一次；重复绑定、or 绑定集合不一致、空范围、不可驳 let 段、非穷尽 match 和错误类型守卫都有稳定诊断；模式不调用用户 Eq/Ord。

- [ ] **阶段 16：实现函数、闭包与 async 捕获**（复杂度：4）
  - 依赖：阶段 13、14、15。
  - 实现具名函数、闭包函数字面量、一等函数、函数项擦除、参数包、捕获槽、递归捕获、`async` 新协程语义和 `Join[T]` 类型形成。
  - 验收：闭包捕获共享正确槽并延长寿命；遮蔽不改变旧捕获；普通函数无 await 染色；捕获、返回、存储和跨 suspend 不制造悬空引用或借用错误。

- [ ] **阶段 17：实现 trait、impl、UFCS 与特化选择**（复杂度：5）
  - 依赖：阶段 12、14、16。
  - 实现 trait 方法/关联类型/关联常量、固有 impl 归属、trait impl 完整性、方法自动解引用、UFCS、操作符 trait、否定 impl 和闭世界最具体特化。
  - 验收：固有方法优先、trait 候选唯一、重叠特化部分序无歧义；交叉重叠、缺项、错误关联类型、外模块固有 impl 和 `forbid` 相关约束均正确诊断。

- [ ] **阶段 18：实现 impl Trait、dyn Trait 与 Any 前端**（复杂度：4）
  - 依赖：阶段 17、12。
  - 实现 APIT/RPIT/TAIT 隐藏类型、对象安全判断、胖函数/胖 trait 表示、`dyn Any` 擦除以及 `is/downcast/downcast_copy` 的静态类型检查。
  - 验收：`impl Trait` 保持单态化；不安全对象 trait 不能形成 dyn；Any 只能恢复放入容器的具体类型；不得出现名为 `any` 的渐进类型或跨 dyn Trait 猜测。

- [ ] **阶段 19：实现 unsafe、原始指针、union 与 intrinsic 检查**（复杂度：4）
  - 依赖：阶段 12、14、17。
  - 实现 unsafe 边界、`MaybeUninit`、`transmute`、volatile/unaligned 访问、`unreachable`、原始指针有效位模式、`asm/global_asm` 语法约束、链接属性和 C ABI 签名可表示性检查。
  - 验收：安全代码不能越过 unsafe 前置条件；资源/COW 类型不能被位操作绕过；union 只接受位类型；Windows `i128/u128` C 签名、naked、dirty/leaf/bridge 属性按规范拒绝或接受。

- [ ] **阶段 20：构造 AST/HIR 结构与冻结校验**（复杂度：5）
  - 依赖：阶段 10、12–19。
  - 实现 `SourceSnapshot -> AST -> configure -> collect -> resolve -> type_check -> HIR` 的固定阶段、owner arena、Res、类型/调整表、捕获计划、cleanup plan、诊断排序和 Validated 冻结接口；为源码宏生成节点保留 expansion source context。
  - 验收：基础 HIR 结构与冻结校验可独立运行；源码宏在阶段 22 生成的片段重新进入同一前端并最终满足相同 Validated 条件；GIR 只能消费冻结 HIR，不能重新解析 token 或名称。


## 三、comptime、闭世界与 IR

- [ ] **阶段 21：实现 EarlyConst 与 capability registry**（复杂度：5）
  - 依赖：阶段 11、12、20。
  - 实现早期常量、数组长度、布局参数、泛型参数和 comptime 脚本解释器；登记允许的 lang item/intrinsic/std 能力、效果、显式输入和 evaluator revision。
  - 验收：不在 registry 或执行域未授权的调用在求值前失败；comptime 使用确定性堆、fuel、panic 和资源边界；运行时副作用、未登记文件/网络/进程访问不会被 evaluator 偷渡。

- [ ] **阶段 22：实现 `comptime source` 与源码宏展开**（复杂度：5）
  - 依赖：阶段 08、10、20、21。
  - 实现 `ParsedSource` 不透明值、`std.syntax.parse_*`、item/statement/expression/type/pattern source slot、轮次闭包、ExpansionId/source map、递归与展开预算。
  - 验收：生成源码必须重新经过 cfg、收集、解析、名称、类型、trait、unsafe、ABI 和 HIR；片段类别不匹配、cycle、fuel/字节/节点/深度超限均保留完整展开链诊断。

- [ ] **阶段 23：实现 AbstractAnalysis、范围证明与效果传播**（复杂度：5）
  - 依赖：阶段 14、20、21、22。
  - 实现 CFG 固定点、范围/符号关系、初始化、别名类、memory version、COW seal、resource publish、并发/FFI unknown、widening/narrowing 和跨函数摘要。
  - 验收：只有 `proved` 才能删除边界检查或生成更强 placement；未知调用、别名、并发和预算耗尽保留原检查；分析不会替代类型推断或 impl 选择。

- [ ] **阶段 24：实现闭世界可达性与单态化实例图**（复杂度：5）
  - 依赖：阶段 17、18、20、22、23。
  - 实现入口、runtime/std、测试/export/used、comptime 和 late closure 根；实现 `StableTypeKey`、`MonoKey`、SCC 实例闭合、公共函数摘要和每实例 code fragment。
  - 验收：删除项不进入实例图；递归泛型和 impl 选择在闭合后稳定；并行单态化结果按稳定 key 排序，跨 package 摘要不暴露 private state 或 session-local ID。

- [ ] **阶段 25：实现 LateConst、FreezeTypeUniverse 与 TypeId**（复杂度：4）
  - 依赖：阶段 18、24。
  - 实现具体类型集合冻结、按 `StableTypeKey` 分配稠密 TypeId、`type_id_count()`、late 结果表、类型名、descriptor/vtable 引用和禁止反向影响类型形成的阶段约束。
  - 验收：所有拥有 TypeId 的具体类型恰有一个编号；`!`/MaybeUninit 无编号；late 值不能改变可达性、布局、impl、宏或调用图；编号不作为跨镜像或 C ABI 稳定密钥。

- [ ] **阶段 26：实现 GIR 语义 lowering 与 cleanup CFG**（复杂度：5）
  - 依赖：阶段 20、24、25。
  - 实现 place/operand/rvalue、显式 CFG、值描述符动作、COW/resource 动作、suspend、panic、`ScopedViewBegin/End`、`NoSafepointRegion` 和 HIR CleanupPlan 到 cleanup block 的 lowering。
  - 验收：return、break、continue、`?`、panic 和正常出口的动作序列与 HIR 一致；scoped view 无逃逸、无 suspend、每条出口恰有一次 end；NoSafepointRegion 只能由登记 intrinsic 产生。

- [ ] **阶段 27：实现值传递、COW 与 placement 分析 lowering**（复杂度：4）
  - 依赖：阶段 23、26。
  - 实现位值浅拷贝、身份句柄共享、string/ByteBuffer seal、resource lease、`f(x)`/`f(&x)`、大拷贝 lint、escape 与 TurnRegion/LocalHeap/SharedHeap placement 选择。
  - 验收：传递不产生 move/borrow 门槛；句柄身份、COW 独立值和 ResourceCell 一次性 release 语义在赋值/返回/聚合/Any 中一致；分析未知时保留安全通用路径。

- [ ] **阶段 28：实现 LIR SSA、memory SSA 与 verifier**（复杂度：5）
  - 依赖：阶段 26、27。
  - 实现 block parameter SSA、Mem token、封闭 LIR 指令集、provenance、调用/原子/volatile/屏障/safepoint effect、source scope 和结构 verifier。
  - 验收：每个可能内存操作都有正确 Mem 链；合流参数顺序确定；禁止悬空引用、越权 pointer provenance、非法 memory order、未闭合 region、缺失 barrier 和不合法 terminator。

- [ ] **阶段 29：实现固定优化管线与 poll budget**（复杂度：5）
  - 依赖：阶段 23、28。
  - 按 GIR/LIR 规范实现常量/复制传播、CFG、边界消除、循环、逃逸、placement、COW、defer、vectorizer、poll placement、NoSafepoint 和 barrier reserve pass。
  - 验收：pass 顺序不可任意交换；无限 managed 路径满足 safepoint 与 cost budget；优化不得移动用户可观察求值、cleanup、同步、GC barrier 或 FFI effect；所有 verifier 在每个 pass 后运行。

## 四、runtime 内存、调度与 GC

- [ ] **阶段 30：实现 raw slab/span 与 owner-directed return**（复杂度：4）
  - 依赖：阶段 01、11（使用阶段 01 提供的可替换 fake range provider）。
  - 实现稳定 `OwnerRecord/OwnerToken`、`SlabDescriptor`、dense size class、本地 free path、producer staging、ReturnMessage、8 shard owner inbox、MPSC batch 发布和 exactly-once return。
  - 验收：跨 owner 只发送 descriptor/index/generation/epoch/bytes/integrity，不发送 managed 裸地址；MPSC 交错、远程批量、generation mismatch、owner retire 和链完整性均有确定性测试。

- [ ] **阶段 31：实现 ResourceCell 与自适应资源租约**（复杂度：4）
  - 依赖：阶段 27、30。
  - 实现地址稳定 ResourceCell slab、open/closed 状态、lease 复制、受限 cleanup、发布前后状态迁移、File/socket/process/lock/FFI resource 的统一 release 入口。
  - 验收：复制资源值不会重复关闭；最后 lease release 不执行用户代码；异常、panic、detach、close、进程终止和共享发布保持规范的一次性语义；资源值禁止进入错误的 arena reset。

- [ ] **阶段 32：实现 PlatformRange、extent 与内存账本**（复杂度：3）
  - 依赖：阶段 30。
  - 实现 reserve/commit/decommit/release/guard/wait/wake、2 的幂 extent、range metadata、huge-page hint、zero/entropy/dump policy 和 committed/reserved/pending/cache/reclaimable 分类。
  - 验收：decommit 只在 allocator/scanner/forwarder lease 和 grace 结束后发生；Linux fake platform 与 Windows fake platform 的错误映射一致；内存压力统计不重复计数。

- [ ] **阶段 33：实现 rt0、启动配置与 fatal/报告路径**（复杂度：4）
  - 依赖：阶段 32；bridge/ABI 在阶段 58 接入。
  - 实现 Booting/Running/Waiting/Terminating 状态、环境快照、runtime 配置解析、main/Err/panic/fatal 终止计划、emergency buffer、text/NDJSON report、backtrace 收集与报告和退出类别。
  - 验收：非法配置、OOM、StackOverflow、RuntimeInvariant、ForeignUnwind、PanicDuringUnwind、HardwareFault 的边界不可被 catch；报告不调用用户代码、普通分配器或异步 I/O；输出字段与 schema 稳定。

- [ ] **阶段 34：实现协程控制块、stack arena 与 context switch**（复杂度：5）
  - 依赖：阶段 30、32、33。
  - 实现 `CoroutineHot/Cold/Slot` 固定布局、可复制 stack arena、size class、上下界 guard、x86_64 context 保存/恢复、stack growth/retire 和完成记录。
  - 验收：`CoroutineSlot` 及 cache-line layout 满足 backend 断言；栈增长不超过逻辑上限；完成 coroutine 的 result/panic 先转移到 cold control，再安全归还旧 stack。

- [ ] **阶段 35：实现 M:N scheduler 基础路径**（复杂度：5）
  - 依赖：阶段 34、30。
  - 实现 LogicalProcessor/WorkerThread、固定 256 LocalDeque、remote batch/injection、run_next、park/unpark、work stealing、weak fairness、producer gate 与动态 processor topology 骨架。
  - 验收：runnable 无额外 FIFO 保证但不被持续新工作永久饿死；无 `P × P` mailbox；park 的 queue recheck/work sequence 不丢唤醒；processor retire 能转移全部本地状态。

- [ ] **阶段 36：实现 channel、Join、select 与等待源**（复杂度：5）
  - 依赖：阶段 35、阶段 16、31。
  - 实现有/无缓冲 channel、send/recv/try/close 线性化、Join 完成记录、WaitSourceId、select scratch、默认分支、随机公平和等待队列。
  - 验收：close 竞态、收尽缓冲、重复 wait、分离子协程、无缓冲会合和 select 单次求值/单次提交严格符合规范；等待只挂起协程，不占用 OS thread。

- [ ] **阶段 37：实现 std.sync 原子、锁、OnceLock、Lazy 与取消**（复杂度：4）
  - 依赖：阶段 35、36。
  - 实现 Ordering、Atomic 合法类型、Mutex/RwLock/Condvar、非 poisoning guard、OnceLock/Lazy 的 Ready/Failed、CancelSource/CancelToken、取消与阻塞操作的接缝。
  - 验收：Acquire/Release/SeqCst 语义由状态机测试验证；初始化 panic 永久 Failed；锁 guard 释放不依赖用户 drop；取消幂等、协作、不会隐式 kill 子进程或取消 Join 子协程。

- [ ] **阶段 38：实现 stack map、unwind 与可复制栈扫描**（复杂度：5）
  - 依赖：阶段 28、34、35。
  - 实现 root kind、safepoint kind、寄存器/slot map、固定 section 编码、frame walk、StackInterior 修正、panic landing、stack copy 和 map verifier。
  - 验收：CallReturn/suspend/bridge/stop 点的 managed roots 可精确枚举并更新；旧栈 opaque pointer 不会重用；缺 map、版本错误、范围越界和寄存器冲突在镜像写出前失败。

- [ ] **阶段 39：实现 Mosaic GC 元数据与精确 trace**（复杂度：5）
  - 依赖：阶段 25、32、38。
  - 实现 TypeRecord、heap header、2 MiB arena/32 KiB block/128-byte line 元数据、trace descriptor/program、value program、vtable/glue/root/source metadata section 和 boot verifier。
  - 验收：canonical ULEB128、唯一 END、size/align/TypeId、section offset、root range、vtable 和 glue 关系全部校验；collector 不对 runtime raw memory 做保守扫描。

- [ ] **阶段 40：实现 hybrid write barrier 与 remembered set**（复杂度：5）
  - 依赖：阶段 27、29、39。
  - 实现 Yuasa deletion + Dijkstra insertion、direct field barrier、跨 block edge summary、256 项 CardMarkBuffer、dedup stamp、CardMarkBatch 与 owner 消费。
  - 验收：实际 field store 先于 barrier 账本发布；buffer 满、processor handoff、foreign、pressure、minor stop 和 producer gate 会 flush；barrier reserve 不跨 NoSafepointRegion 分配或补容量。

- [ ] **阶段 41：实现 GC debt、credit、pacing 与 pressure drain**（复杂度：5）
  - 依赖：阶段 30、32、39、40。
  - 实现 allocation/mark debt、owner credit、gc CPU fraction、assist quantum、remark/evacuation pause budget、pressure hysteresis、forced full cycle 和 pending bytes backpressure。
  - 验收：一次 pressure episode 至多强制一次 full cycle；预算超限发布 continuation 或延后 block；无法取得 headroom 才 OOM；统计覆盖 message/cache/credit/grace 的真实物理占用。

- [ ] **阶段 42：实现 TurnRegion 私有图与 transfer/reset**（复杂度：4）
  - 依赖：阶段 23、27、39、41。
  - 实现 owner-local bump/reset、export summary、RegionTransfer、promotion、ResourceCell/FFI alias 检查和 region registry。
  - 验收：只有无外部 alias、无 resource lease、无 FFI 地址且 export summary 闭合时才 reset；sender 仍需使用语义时先 promote/copy；普通 channel 不获得 transfer 语义。

- [ ] **阶段 43：实现 LocalHeap Immix、TLAB 与分代 cycle**（复杂度：5）
  - 依赖：阶段 39–42。
  - 实现 nursery/aging/old/pinned/large/resource arena、TLAB refill、object-start/mark/card bitmap、minor copying、major mark/sweep/evacuation、pin side table 和 owner-local free。
  - 验收：对象不跨 block；age、pin、large/resource 规则正确；minor 只移动允许的 LocalHeap 对象；所有移动都更新 exact roots/interior pointers；无 read barrier 的 direct pointer 热路径保持成立。

- [ ] **阶段 44：实现 MarkTicket、MarkMailbox 与终止信用**（复杂度：5）
  - 依赖：阶段 40、41、43。
  - 实现每 owner 单 consumer MarkMailbox、cycle/topology/generation 标识、跨 owner mark ticket、credit acquire/consume/return、root snapshot gate 和并发终止检测。
  - 验收：mailbox 为空不等于全局完成；所有 owner credit、worklist、barrier、producer epoch 和 forwarding work 收敛后才能 remark；过期 ticket、重复 ticket 和错误 owner 得到 invariant failure。

- [ ] **阶段 45：实现 EdgeDelta、block candidate 与 SCC fallback**（复杂度：5）
  - 依赖：阶段 40、44。
  - 实现跨 block EdgeAdd/EdgeDrop 聚合、generation/epoch 排序、candidate block lease、exact local trace、受限 trial deletion/SCC fallback。
  - 验收：EdgeDelta 只作 candidate，不当对象级引用计数；add/drop 顺序不丢边；循环垃圾在 block candidate 失败时通过 SCC 规则处理，不错误释放仍可达对象。

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
  - 验收：无 warning、无未实现路径、无占位内容、无未解释规范差异；生产镜像不依赖系统 LLVM/assembler/linker；所有公开章节、内部章节、ADR、测试、教程和参考手册之间可追踪；只有在此阶段通过后才允许把路线图阶段全部改为 `[x]`。

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

阶段之间可以在不违反契约的前提下并行实现，例如前端类型系统与 raw range fake、标准库纯值模块与后端 encoder；但任何并行工作都必须共享稳定 schema，不能各自建立第二套类型、资源、诊断、ABI、root 或 runtime 状态表示。喵~
