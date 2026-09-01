# 单态化与编译缓存

本章规定编译器内部的 demand-driven query 图、单态化实例身份和内容寻址编译缓存。外部可观察的缓存输入集合、目录覆盖和清理语义见[包、依赖与构建模型](../spec/packages-builds.md#缓存与-target-视图)；本章固定当前编译器如何实现这些要求，但不把磁盘格式承诺给第三方工具。

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

域标签固定包括 `gugu-input-v1`、`gugu-query-key-v1`、`gugu-query-result-v1`、`gugu-mono-v1`、`gugu-object-v1` 和 `gugu-action-v1`。不同域的相同 payload 不能得到可互换身份。

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

session-local 的 `DefId`、`TyId`、arena ID、指针、线程编号和绝对 workspace 路径禁止进入规范编码。IR 在编码前按稳定定义键、block/value 的确定性构造顺序和规范化类型结构重编号。

## 编译器构建身份

`CompilerIdentity` 是下列字段的规范编码摘要：

- 编译器源码 revision 和工作树状态标志；
- AST、HIR、GIR、LIR、cache、stack-map、GC metadata 与后端 schema 版本；
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
| 10 | `EvaluateComptime` | stable definition + args | comptime 值 |
| 11 | `BuildGenericGir` | stable owner key | generic GIR |
| 12 | `CollectMonoRoots` | target/harness | 根 `MonoKey` 集合 |
| 13 | `InstantiateGir` | `MonoKey` | monomorphic GIR |
| 14 | `LayoutOf` | stable concrete type key | 目标布局 |
| 15 | `BuildLir` | `MonoKey` | 优化后 LIR + `PollSummary` |
| 16 | `CodegenFragment` | `MonoKey` + LIR fingerprint | 机器码片段 |
| 17 | `TypeMetadata` | stable concrete type key | 未重定位类型 metadata |
| 18 | `RuntimeMetadata` | closed-world type/instance set | metadata sections |
| 19 | `PlanImage` | target + roots + fragments | 镜像布局与重定位计划 |
| 20 | `EmitImage` | image plan fingerprint | 最终镜像 |

新增、删除或改变 kind 的 key/result 必须改变注册表 schema；不能复用旧编号表达不同含义。

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
- 互递归函数 body 各自 type-check，但只依赖已冻结签名；
- trait/impl obligation 使用独立 obligation stack 报循环或求规范允许的固定点；
- 相同 `MonoKey` 的递归调用复用正在收集的实例节点，不再次实例化。

并行任务之间的等待边也加入 session wait graph。形成环时由稳定 key 最大的等待者中止等待并把完整环交给普通 cycle 诊断，避免线程死锁。

## 指纹与 red/green 复用

input fingerprint 为 `gugu-input-v1` 域下的规范输入摘要。query result fingerprint 为：

```text
kind number
query schema version
canonical key
sorted direct dependency result fingerprints
canonical successful result bytes
sorted diagnostics bytes
```

经 `gugu-query-result-v1` 域哈希得到的 32 字节值。

加载上一 session 的 query record 时，engine 先递归验证直接 input/query 的当前 fingerprint：

- 全部相同则标为 green，直接复用结果；
- 任一不同、缺失或 schema 不匹配则标为 red，重新执行；
- 重新执行后若 result fingerprint 与旧值相同，下游仍可保持 green。

query 不允许读取未通过 query/input API 登记的文件、环境、时钟、随机源或全局可变状态。build.gg 输出、`embed_file`、target 配置、feature、锁图和 native link metadata 都作为显式 input query 注入。

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

`HH` 是 key 的前两个十六进制字符，`HASH` 是完整 key。目录布局只供同一 `CompilerIdentity` 的 Gugu 工具使用。

`ActionKey` 由 compiler identity、query kind/key、目标、harness/插桩、feature 域、完整锁图及所有直接输入摘要组成。action record 也是普通 object，其 payload 固定包含：

- action key 的完整 32 字节值；
- 成功结果 kind 与 object key；
- 排序后的辅助 object key，如诊断、stack map、unwind 和 relocation；
- 创建该记录的 compiler identity；
- payload 总长度，用于 LRU accounting。

记录不保存创建时间；LRU 访问时间属于独立、可丢失的 cache 索引，不参与 action 内容。

writer 在 `tmp/` 中以随机不可猜名称创建同文件系统临时文件，完整写入、刷新文件内容、重新读取并验证摘要后，以 create-if-absent 原子发布到目标路径。目标已存在时验证既有对象并丢弃临时文件；同 key 不同内容是 compiler internal error。Windows 和 Linux 都不得先删除已存在目标再重命名。损坏文件原子移入 `quarantine/` 后重新构建；隔离失败时也必须绕过该 entry，不能继续反序列化。

reader 的已打开文件句柄就是 lease：Linux 即使被 unlink 仍从原 inode 完整读取；Windows 以 `FILE_SHARE_READ` 打开且不授予 delete sharing，使删除非阻塞失败。LRU 回收对候选执行一次非阻塞删除，Linux unlink 后由最后句柄回收，Windows 遇 sharing violation 直接跳过；不得等待活动 reader。writer 只持有 `tmp/` 文件并以 create-if-absent 发布。进程崩溃留下的 `tmp/` 文件不被索引，下一次 cache maintenance 可以清理。

## 单态化实例

### 稳定类型与实例键

`StableTypeKey` 是具体类型的规范结构编码经 `gugu-mono-v1` 域哈希的 32 字节值。编码必须包含：

- 名义定义的 `StableDefKey`，或结构类型的 kind；
- 规范化后的全部类型和 comptime 实参；
- 引用/指针种类、函数签名、ABI、数组长度和 repr；
- 闭包/opaque 类型的 owner 稳定键与匿名节点消歧信息。

它不包含运行时稠密 `TypeId`。相同结构元组得到同一 key；不同名义定义即使布局相同也得到不同 key。

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

### 具体类型集合与 `TypeId`

实例图闭合后，从签名、local、global、vtable、descriptor、常量和 runtime 根递归收集所有具体类型。类型集合按完整 `StableTypeKey` 字节序排序并分配连续运行时 `TypeId` `0..N`；`N` 必须小于等于 `u32::MAX`，与[类型规范](../spec/types.md#typeid-与-dyn-any)一致。

单类型 metadata object 可以独立缓存，但其中对其他类型、glue 和 vtable 的引用必须保存稳定 relocation key。最终 `RuntimeMetadata` query 在本次闭世界集合上把这些 key 重定位为稠密 `TypeId`/RVA。含最终数字 `TypeId` 的完整 section 不能跨不同闭世界集合直接复用。

## 每实例代码缓存

`CodegenFragment` 以单个 `MonoKey` 为粒度，payload 固定包含：

- 机器码 bytes 与对齐；
- 本地只读常量 bytes；
- 按 offset 排序的逻辑 relocation；函数/数据引用使用 stable symbol，运行时类型常量/记录使用完整 `StableTypeKey`，源码索引使用 `{ MonoKey, fragment_source_ordinal }`，禁止烘焙本次闭世界的数字 `TypeId`/source record index；
- 未重定位 stack map、unwind 和 source location 记录；每条保存逻辑路径、byte span、synthetic kind和 fragment source ordinal；
- 定义符号、引用符号及可见性；
- LIR result fingerprint、直接计费的 `PollFreeLeaf` summary fingerprint和所有内联 callee fingerprint。

单实例粒度避免 package 内一个无关函数变化使整个目标代码失效。最终 image layout统一放置 fragment并解析 relocation。跨实例内联使 caller key依赖被内联 callee的 GIR/result fingerprint；未内联且保留 entry `StackCheck` 的普通调用只依赖 callee ABI/signature fingerprint，不因 callee body改变而使 caller失效。只有 direct `PollFreeLeaf` 调用额外依赖其固定尺寸 `PollSummary`；leaf分类或 `poll_free_cost` 改变时 caller `BuildLir`必须失效。递归/间接调用始终使用 checked entry，不形成 `BuildLir` query环。

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
