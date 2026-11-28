# Compiler bootstrap 与 action graph

本章固定官方 compiler 从工程入口接入完整编译管线时的 bootstrap 边界。它是实现说明，不增加 Gugu 语言语义，也不构成跨 compiler identity 的 ABI。公开行为以 [`spec/`](../spec/overview.md) 为准；后续能力必须在本章登记的归属层继续扩展，不能另起一套语义等价实现。

## 工程入口与镜像计划

官方 compiler 以 Rust 实现 bootstrap：单一 `gugu` 入口、目标描述、确定性诊断、前端、稠密 IR、后端 image plan 和 Gugu runtime 源资源登记。它可以检查空 package、内存中的单文件入口和文件系统单文件入口，并为合法的 `fn main() { ... }` 生成端到端 action graph。

`ImagePlan` 持有链接完成的镜像字节（ELF/PE）。`emit-image` 在镜像生成时 `complete`；落盘物化由 `gugu build`/`gugu test` 等宿主工具写入 `target/` 布局（见[构建产物布局](../spec/packages-builds.md#cache-and-target-view)）。任一前置 action 失败时，所有后续 action 都会被跳过，结果中不会留下镜像计划。

## CLI 入口与输出

`gugu` 是唯一 CLI 入口：根级全局参数可在子命令前后解析，配置按内置默认、用户配置、当前 workspace 的 `.gugu/config.toml`、`--config`、环境变量、命令行的顺序合并，后层覆盖前层。`--frozen` 在解析结果中同时设置 `offline` 与 `locked`。

规范表中的 `new`、`init`、`build`、`check`、`run`、`test`、`bench`、`fmt`、`doc`、`clean`、`add`、`remove`、`update`、`tree`、`vendor`、`package`、`publish`、`yank`、`login`、`cache`、`explain`、`version` 和 `help` 均已登记。`build`、`check`、`fmt`、`doc`、`version` 和 `help` 接入真实 action；其它已登记命令返回统一 `cli-error`，不会调用 compiler。
现状基线是 [`gugu-cli`](../../../crates/gugu-cli/src/main.rs)：compiler 已接入源码、清单、依赖、缓存、词法/AST、类型/语义检查、冻结 HIR 与 generic GIR；runtime 调度/GC、LIR、后端机器码与标准库语义仍按对应内部契约推进。

### Build-time runtime artifact

compiler 构建阶段由 `gugu-runtime-builder` 读取内建 `std/` 与 `runtime/` 源，按目标生成 `GUGURT01` artifact；`gugu-compiler`（façade）嵌入并校验 Linux/Windows bytes，`gugu-frontend` 不持有字节，只按目标接收注入的 `RuntimeArtifactProvider`。artifact 固化 source blob、manifest 与已物化的内建派生表。frontend 按目标、物化表实际内容身份及层 revision 复用通过结构 verifier 的不可变解码基底；缓存身份覆盖表字节、指纹、模块顺序与 revision，不能只凭声明摘要命中。每次会话仍校验源码前缀与模块归属；名称与导入记录共享解码表，`Span` / `SourceTableId` 在独立会话 overlay 中重建；用户定义按稳定前缀之后的编号追加。独立布局证据从 record 签名形成，不依赖 executable 闭包。

runtime 的契约入口为 record-only 根：用于原有 size/align/offset/managed 校验，不生成 executable body。runtime `#[used]` 不再隐式形成全局执行根；rt0 启动、登记机器片段的源目标与可达 GIR 操作产生的 glue 需求进入单态化固定点。用户的 `#[used]`、导出、入口和初始化语义不变。未引用的源 glue 缺少实例时不绑定，实际引用缺少目标仍是 invariant error。

placement 在实例闭合后才形成，源引用/取址可能在后续落 LocalHeap/TurnRegion；因此这些真实地址操作会保守要求两种分配域和屏障入口，不能只扫描显式 `AllocObject`。这是闭包形成前缺少最终 placement 证据的固定选择，不恢复全部 runtime `#[used]` 根。

最终 image assembly 保留原有 target、encoder/raw/LIR fingerprint、符号、relocation、frame、stackmap、unwind 与 source metadata 验证。artifact manifest digest、当前需求及记录目录共同参与 runtime contract 身份。

`text` 保留人读的 action/诊断/最终结果，诊断按 `级别[代码]: 消息`、`--> 文件:行:列` 与带行号源码片段块渲染；`json` 为 NDJSON 事件信封，bootstrap 的构建事件顺序固定为 `build-start`、诊断、`build-finish`；`json-diagnostic-short` 只发布诊断事件。NDJSON 对源码路径使用逻辑相对路径，对工作区外路径使用 `<external>/文件名`，并清理凭据键值。

## 源码快照与 Span

项目输入由 frontend 的 `project_logical_path` 在 package 根下形成逻辑路径：先拒绝不能无损转换为 UTF-8 的相对路径，再将宿主目录分隔符转换为 `/`，最后交给 `normalize_logical_path` 校验。源码快照直接复用该逻辑路径，入口与源码根也经过同一边界；转换不使用编译目标平台，也不放宽快照对非法逻辑路径的拒绝规则。

`source` 模块落地源码快照与 Span 系统：`load-sources` 把每个输入固化为不可变 `SourceSnapshot`——UTF-8 校验、BOM 拒绝、`u32` 长度上限、BLAKE3-256 内容摘要与规范行首表。逻辑路径按词法归一：解析 `.` 与 `..`、统一为正斜杠分隔符、拒绝绝对路径、反斜杠与越界回溯，保证相同输入在不同操作系统、工作目录与换行环境下产生相同 span。Span 端点必须落在 UTF-8 字符边界，行首表的列号按 Unicode 标量而非字节数计算；Span 携带所属源码表的 `SourceTableId`，跨表的 span 在宏展开注册时被拒绝。`SourceTableId` 由源码表内容派生（快照逻辑路径与内容摘要），同一份源码表在任何会话、任何编译顺序下都得到同一编号，身份在建表时定死，不随后续追加的生成快照或展开记录改变——否则同一个编译里更早形成的 span 会被判成外来 span。这个编号只是会话内的跨表标记：它不进规范编码，物化表、query 缓存与诊断因此都能被另一会话逐字节重建；反序列化得到的 span 没有表身份，跨表校验按「未知」放过，其余位置校验照旧。

`SourceMap` 拒绝重复路径，并把内建源按 artifact source manifest 顺序装到稠密文件 ID 前缀、用户快照按逻辑路径排序追加在后（`prepend_builtin_prefix`）。模块列表按「内建块在前、块内按模块路径」定序，因此物化表的内建下标在用户编译里仍指向同一个内建单元。源码宏展开注册保持确定性：`ExpansionRecord` 记录父展开、宏调用与定义位置、生成源码哈希与片段类别，注册顺序按调用位置、轮次与片段顺序稳定。诊断的源码校验码为 `E0004`~`E0008`（非法 UTF-8、BOM、非法逻辑路径、span 越界、源文件过大），全部诊断按路径、偏移、级别、代码稳定排序。源码宏生成文本的快照字节校验失败按快照自身的代码报告（BOM 为 `E0005`、非法 UTF-8 为 `E0004`、超限为 `E0008`），不再一律归为非法逻辑路径；解析闸门在此之前已把生成文本送回主 lexer/parser，脚本可以像处理语法错误一样捕获这类失败。

## 清单、workspace 与 target

`project` 模块落地清单、workspace 与 target 发现，并把 CLI 的 `build`/`check` 无文件参数路径切换为项目模式。清单发现从当前目录向父目录查找最近 `gugu.toml`；解析使用 serde 严格 schema，未知核心字段、缺失 `[package].name`、保留 package 名 `std` 与保留依赖别名 `std` 都是编译前错误。workspace 本地配置从 workspace 根目录读取（从当前目录向上找最近的带 `[workspace]` 或 `[package]` 的清单目录），从成员目录启动也能继承根配置。

workspace 成员按 `members` glob 展开并扣除 `exclude`，glob 展开按规范相对路径排序、同一路径只算一个成员；`exclude` 允许指定非存在目录。根清单可同时是根 package，此时无论从根还是成员目录启动，根 package 都纳入模型。package 选择遵循规范：显式 `-p` 按规范名或短名唯一匹配，`--workspace` 覆盖默认选择并包含根 package，成员目录启动时默认只构建当前 package，workspace 根启动时依次使用 `default-members`、根 package 或全部成员。

target 自动发现覆盖 `src/lib.gg`、`src/main.gg`、`src/bin/`、`tests/`、`benches/`、`examples/` 与 package 根 `build.gg` 的文件与目录形式，`auto-*` 开关与显式 target 表按清单规则生效；`foo.gg` 与 `foo/mod.gg` 同时存在、同种类重名 target、入口越过 package 根均在编译前失败；target 自动发现与冲突检查会忽略构建输出目录 `target` 及隐藏目录。显式 `path` 可直接位于 package 根（如 `main.gg`），其源码根为 package 根本身，不再误当作目录。lib 的默认名是 package 短名把 `-` 换成 `_`。项目模式下每个选中 target 以 `project_entry` 进入同一 action graph：bin 类入口要求合法 main，lib/test/harness 类入口只做源码快照检查。`required-features` 按当前启用 feature 集合在编译前过滤：未满足的 target 不选择，`--features`/`--all-features`/`--no-default-features` 决定启用集合，未知 feature 名退出码 2。单文件模式（`gugu build <file.gg>`）拒绝 `-p`、`--workspace`、`--features`、`--no-default-features`、`--all-features`、`--lib`、`--bin`、`--test`、`--bench`、`--example`、`--all-targets`，以退出码 2 失败。项目 `build`/`check` 已接入依赖解析：锁图写入 workspace 根 `gugu.lock`，下载、缓存、checksum 与 vendor 由下一节规定。

## 依赖解析

`project::dependencies` 实现依赖清单到锁图的单向解析。`Version` 严格解析 SemVer 2.0.0 的三段数字、预发布标识和 build metadata；`VersionReq` 支持 caret、tilde、关系运算、逗号交集和 wildcard，并按 SemVer precedence 比较，build metadata 不参与兼容判断。预发布候选只有约束显式覆盖同一 `major.minor.patch` 时才进入匹配。

每个依赖的身份由规范包名、精确版本和 `path`/`git`/registry source 共同决定。path source 只从已发现 package 和可递归发现的 `gugu.toml` 读取；Git 与 registry source 由 `ResolveOptions` 提供确定性候选索引，候选按版本降序和 package ID 排序，撤回版本不参与新解析。别名只影响边上的名称；同一 package ID 在同一 normal/test/build 上下文只建立一个节点，不兼容版本可以并存。

解析上下文固定分为 target 普通图、target 测试图和 host build 图。普通依赖可由测试图和 build 图读取，test 依赖不传播给消费者，build 依赖只在 host 图激活；target 条件在 target 名上求值，build 条件在 host 名上求值。根 package 的 feature 请求、依赖 feature、`dep:` optional 引用和默认 feature 在各域内做并集，未声明 feature、source 候选缺失、版本无解和图循环都在进入 frontend 前报错。

`LockGraph` 只写规范字段：版本、package ID、checksum、已解析依赖边、边的域与条件，以及 normal/test/build feature 集。package、依赖边和 feature 均在编码前稳定排序并去重；path source 只能写规范相对路径，Git source 必须写完整 commit/tree。锁图读取会验证版本、source、SemVer、域、重复 package ID 和悬空边。CLI 普通 `build`/`check` 自动写根锁文件，`--locked` 只读取并要求规范编码与重新解析结果完全一致。

本节不负责 registry 协议、发布签名或 patch 远程输入；这些能力由[发布与生态](../spec/publishing-ecosystem.md)规定。下载、缓存、checksum 与 vendor 见下一节。

## 缓存与输入

`project::cache` 实现外部依赖输入的闭环。`PackageFiles` 只接受规范相对路径，按 UTF-8 字节序和长度前缀编码文件内容，并以 `gugu-package-v1` 内容流计算 SHA-256。gzip tar 归档先解包并拒绝绝对路径、父目录、符号链接和特殊文件，再校验 checksum；校验成功后才进入缓存。

`DependencyCache` 使用 `dependencies/v1/packages/<package-key>/`、`tmp/` 和 `quarantine/` 布局。条目记录 package identity、registry checksum、文件长度和 BLAKE3 文件摘要；读取时验证目录结构、记录、每个文件和整体 checksum。损坏条目会原子移入 quarantine，读取不会继续消费不可信字节；同一 package 的并发写入使用临时目录和 create-if-absent 发布。

`prepare_dependency_inputs` 以 `LockGraph` 为唯一 package 集合：path package 从本地规范目录读取，registry package 必须匹配锁定 checksum，Git package 必须匹配锁定 source 并验证缓存内容。启用 vendor 时，`vendor/.gugu-vendor.toml` 必须与锁图 package identity、目录映射和内容摘要完全一致，并且不会回退到缓存或网络；path package 仍从本地读取。CLI 在进入 codegen 前执行这一门禁。

`ActionInputs` 将 compiler identity、host/target、target kind、harness/插桩、feature、锁图、源码、嵌入文件、宏/comptime/type universe/late constant、公共摘要、build 输入输出、cfg 和 native link metadata 编码后计算域隔离的 `ActionKey`。`TargetView` 只把已验证的相对产物原子写入 `<target>/<target-name>/`，不会把全局缓存当作用户可见目录。

`--frozen` 在 CLI 中等价于 `--locked` 与 `--offline`；`--vendor` 只改变外部 package 的输入源。锁图重放使用锁定版本重建 resolver 候选，因此清单、target 或 feature 变化在锁门禁处失败，不会到达目标代码生成。

## 工程边界

当前实现的模块树如下：

```text
crates/
├── gugu-base/                      最底层：不依赖任何编译阶段
│   ├── src/action.rs               稠密 action graph 与状态迁移
│   ├── src/canonical.rs            GBC1 规范编码
│   ├── src/diagnostics.rs          稳定代码、源码范围与排序
│   ├── src/diagnostics/            人读诊断渲染
│   │   ├── human.rs                片段、插入符与 ANSI 颜色的文本布局
│   │   └── human_tests.rs          渲染契约测试
│   ├── src/hash.rs                 域隔离摘要 `hash_domain`（全仓唯一实现）
│   ├── src/query.rs                session 内 query engine 与编译对象存储
│   ├── src/source.rs               源码快照、Span、行首表与展开记录
│   ├── src/target.rs               目标描述符、sysroot 与成本 profile
│   └── src/tuning.rs               调度调优 profile（`RUNTIME_TUNING_PROFILE`）
├── gugu-cli/
│   ├── src/main.rs                 单一 gugu 入口、参数解析与 bootstrap 执行编排
│   ├── src/config.rs               CLI 配置合并、环境变量与本地配置发现
│   └── src/output.rs               Text/JSON 格式化、事件流、诊断渲染与敏感信息清理
├── gugu-compiler/
│   ├── build.rs                    build-time 物化：builder source 表 → 前端派生 → GUGURT01 bytes
│   ├── src/lib.rs                  façade：嵌入 artifact、注入 provider、再导出前端 API
│   ├── src/artifact.rs             目标专属 artifact 解码与 RuntimeArtifactProvider 实现
│   ├── tests/integration/          合并集成回归二进制（ABI、网络、进程、文件系统、物化保真等）
│   ├── tests/docs_links.rs         文档链接与章节锚点契约
│   └── benches/                    手工验收二进制（编码、栈图、片段、协程上下文）
├── gugu-project/                   项目模型：只依赖 gugu-base
│   ├── src/lib.rs                  项目聚合、选择与缓存输入导出
│   ├── src/model.rs                package、target 与 workspace 模型
│   ├── src/error.rs                项目发现、选择、依赖与锁文件错误
│   ├── src/manifest.rs             清单 schema 与 package 构建
│   ├── src/targets.rs              target 自动发现与源码布局校验
│   ├── src/dependency_model.rs     package identity、source、域与 feature 模型
│   ├── src/dependency_manifest.rs  依赖清单与 workspace 继承解析
│   ├── src/semver.rs               SemVer 版本和版本约束
│   ├── src/lock.rs                 确定性 gugu.lock 编解码与校验
│   ├── src/resolver.rs             候选求解、域传播与循环检查
│   ├── src/dependencies.rs         依赖子模块聚合与公共导出
│   ├── src/dependencies_tests.rs   依赖解析、feature 和锁图确定性测试
│   ├── src/cache.rs                依赖缓存、vendor、锁图输入和缓存记录
│   ├── src/package_files.rs        归档解包、规范路径和 package checksum
│   ├── src/action_key.rs           完整编译输入编码与 action key
│   ├── src/target_view.rs          target 用户产物视图与原子物化
│   ├── src/cache_tests.rs          缓存输入、损坏隔离、vendor 和 key 确定性测试
│   └── src/workspace.rs            workspace 成员与 glob 解析
├── gugu-runtime-abi/               runtime 契约层：只依赖 gugu-base
│   ├── src/lib.rs                  源树/rt0/intrinsic 登记、raw 平面常量与 glue 定义表
│   ├── src/model.rs                `RuntimeRawContractV1` 与消息字段 schema
│   ├── src/startup_kinds.rs        rt0 启动/终止/报告的共享枚举目录与常量
│   ├── src/startup_schema.rs       rt0 启动序列、生命周期、启动变量、fatal/退出/报告契约段
│   ├── src/provider.rs             平台 range 操作、二次幂 extent 阶梯与确定性替身
│   ├── src/platform.rs             Linux/Windows profile 常量与 fake platform
│   ├── src/platform_schema.rs      平台操作目录、range 状态迁移与失败映射契约
│   ├── src/extent.rs               extent 阶梯、三路 lease 与 queue-page grace 门禁
│   ├── src/ledger.rs               内存账本互斥分类 schema
│   ├── src/resource.rs             ResourceCell slab、lease 状态机与统一 release 入口
│   ├── src/resource_schema.rs      ResourceCell header/状态位/release 描述符/资源种类契约 schema
│   ├── src/size_class.rs           dense size class 表与 stride 除法常量
│   ├── src/coroutine.rs            hot/cold/slot 固定布局、context 与地址稳定控制页
│   ├── src/coroutine_schema.rs     版本化协程布局、栈策略、LIR需求与片段契约
│   ├── src/coroutine_pack.rs       子协程参数包机器布局常量
│   ├── src/fragment_code.rs        协程契约携带的 morestack/main 返回片段数据形状
│   ├── src/context.rs              x86_64 switch/restore-only 直接编码
│   ├── src/scheduler_schema.rs     版本化调度容量、分片、batch 与 service 节奏契约段
│   ├── src/scheduler_tests.rs      调度 deque 双变体、唤醒、retire 与契约闭环的确定性回归
│   ├── src/stack.rs                栈尺寸、迟滞收缩与精确StackInterior复制
│   ├── src/slab.rs                 owner 目录、slab 描述符与 slot 状态机
│   ├── src/message.rs              return message、link 编码、producer staging 与 source slab 聚合
│   ├── src/inbox.rs                8 shard batch queue、bounded snapshot、epoch gate
│   ├── src/wait_schema.rs          WaitRuntimeContract schema 1 与 WaitDemand
│   └── src/harness.rs              真实并发可运行切片（bench façade）
├── gugu-sema/                      语义层：只依赖 gugu-base 与 gugu-runtime-abi，产物止于冻结 HIR
│   ├── src/lib.rs                  `ParsedModule`、探针源判定与内建包身份
│   ├── src/modules.rs              源码表 → 模块表：模块路径、逐模块词法/语法/cfg
│   ├── src/lex.rs                  TokenBuffer、trivia 与字面量
│   ├── src/parse/                  项、类型、表达式、模式
│   ├── src/ast.rs                  u32 arena 与节点种类
│   ├── src/expand.rs               源码宏展开与预算
│   ├── src/names.rs                名称解析与导入
│   ├── src/semantics/              TypeCheck、早期 comptime、布局交接与 AST 到 HIR 形成
│   ├── src/sema_prefix.rs          内建块 sema 层物化表（names/identities/hir/early/checked）安装
│   ├── src/string/fmt.rs           `std.fmt` 的 `Formatter` 参照模型，comptime 格式化共用
│   └── src/hir/                    类型化 owner、侧表与冻结 verifier
├── gugu-gir/                       GIR 层：只依赖 gugu-base、gugu-runtime-abi 与 gugu-sema，产物止于 GIR world
│   ├── src/lib.rs                  Frontend action 入口 `bootstrap_with`：内建源安装、展开、derive_world、内建块派生
│   ├── src/prefix.rs               内建块安装：核对 revision、委托 SemaPrefix、解码 GIR 层表
│   ├── src/prebuild.rs             build-time 内建块物化与 cfg 独立性检查
│   ├── src/pipeline.rs             冻结后 gir → mono → late → analysis → placement → summary
│   ├── src/gir/                    generic/concrete GIR、pass 管线、placement 与 verifier
│   ├── src/mono/                   单态化根、实例闭合、稳定类型键与公共摘要
│   ├── src/late/                   冻结类型表与 late comptime
│   ├── src/analysis/               全程序抽象分析与 SCC 摘要
│   ├── src/gc.rs                   GC 元数据推导与两个镜像 section
│   ├── src/contract/               HIR/GIR 与 runtime 契约的交叉视图：源记录、image 根需求与控制块布局校验
│   └── src/testing.rs              测试用 build-time artifact provider（`test-support` 对上层开放）
├── gugu-lir/                       LIR 层：只依赖 gugu-base、gugu-runtime-abi 与 gugu-gir，产物止于已验证 LIR world
│   ├── src/lib.rs                  BuildLir query、`Validated` world 与 runtime 需求推导
│   ├── src/target.rs               `TargetOracle`：LIR 向 backend 索取的目标知识边界
│   ├── src/build/                  GIR → SSA LIR lowering
│   ├── src/pass/                   LIR 优化管线、poll placement 与 rewrite editor
│   ├── src/verify/                 结构、provenance、region 与资源隔离 verifier
│   ├── src/stackmap.rs             逻辑栈图推导
│   └── src/root_init.rs            逐根初始化 must 证明
├── gugu-backend/                   x86_64 后端：只依赖 gugu-base、gugu-runtime-abi、gugu-gir 与 gugu-lir
│   ├── src/lib.rs                  目标相关的内存 image plan 输入（`BackendPlan`）
│   ├── src/x64/lower/              LIR → x64 指令序列 lowering 与 `poll_cost`
│   ├── src/x64/alloc/              寄存器分配、frame 布局与逐根初始化字节
│   ├── src/x64/codegen.rs          CodegenFragment query 与 `X64World`
│   ├── src/x64/{table,encode,verify}.rs 描述符表、编码器与 instruction verifier
│   ├── src/x64/fragments.rs        登记机器片段编码与协程片段逐字节比对
│   ├── src/x64/glue.rs             runtime glue 绑定、闭世界 glue 目录与 op 隐含 glue
│   ├── src/x64/oracle.rs           `X64Oracle`：`TargetOracle` 的 x86_64 实现
│   └── src/x64/{elf,pe,metadata,unwind}.rs 镜像写出与元数据/unwind section
├── gugu-runtime-world/             runtime world 参照实现：只依赖 gugu-runtime-abi，只被上层确定性测试消费
│   ├── src/startup.rs              环境快照与启动配置解析的参照实现
│   ├── src/lifecycle.rs            生命周期状态机与 rt0 启动序列参照实现
│   ├── src/report.rs               emergency buffer 与 text/NDJSON 报告渲染参照实现
│   ├── src/termination.rs          `TerminationPlan` 构造与退出码解析参照实现
│   ├── src/stack_arena.rs          双端guard arena、span bitmap与有界cache
│   ├── src/owner.rs                本地 free path、本地/远程 return 分叉
│   └── src/world/                  owner 上下文 service、转发、grace 与 retire
│       ├── mod.rs                  raw owner 世界与 ResourceCell 世界骨架
│       ├── extent_impl.rs          arena 开立、extent 取用与 trim 门禁接入
│       ├── resource_impl.rs        资源分配、lease、close、release 与整页 mapping 接入
│       ├── provenance_impl.rs      raw link provenance 平面接入与 release 拒绝分类
│       ├── routing_impl.rs         direct/radix routing 平面配置、转发与 drain
│       ├── coroutine_impl.rs       main/子协程创建、栈复制、cold完成发布与owner return
│       ├── contract_tests.rs       世界测试共用的 raw 契约构造：直接 `RuntimeRawContractV1::build`，不经 Compiler
│       └── termination_impl.rs     rt0 进程模型：boot、终止路径与设施关闭接入
└── gugu-frontend/
    ├── src/lib.rs                  CompileRequest 与 action 编排
    ├── src/raw_model.rs            runtime raw 契约 query driver：注入 backend 片段并做跨层校验
    ├── src/frontend/               sema 与 GIR 层模块的再导出，以及需要完整 Compiler 的前端回归
    │   └── semantics_tests/        依赖整条管线的语义层回归
    ├── src/lir_tests/              依赖整条管线与 `X64Oracle` 的 LIR 回归及其 fixtures
    ├── src/backend/x64/            `gugu_backend::x64` 再导出、x64 验收 harness 与需要完整 Compiler 的后端回归
    ├── src/runtime/                需要整条管线的 runtime 回归；`mod.rs` 以 `pub use gugu_runtime_abi::*` 再导出契约层，world 参照实现经 dev-dependency 取自 gugu-runtime-world
    └── resources/
        ├── std/prelude.gg          标准库 Gugu 源单元
        ├── runtime/core.gg         runtime Gugu 源单元
        ├── runtime/platform.gg     std.platform 平台范围适配的 Gugu 源单元
        ├── runtime/coroutine.gg    固定控制块、栈策略与record访问的 Gugu 源单元
        └── runtime/channel.gg      等待控制块布局交叉校验的 Gugu 源单元
```

模块职责是单向的：CLI 只构造请求和渲染结果；compiler 负责管线编排；frontend 不创建机器码；IR 不读取源码文本；backend 只消费 IR 与目标描述；runtime 模块只提供 compiler 携带的 Gugu 源资源和边界登记。Rust compiler 不实现 Gugu runtime 的镜像执行路径：调度、GC、资源释放与标准库语义必须在镜像内的 Gugu runtime、rt0 与登记的 machine intrinsic 中落地；compiler 只持有确定性的契约与尚未迁走的参照模型（raw 平面、资源租约、owner-directed return、rt0 启动/终止/报告）。channel、Join、select 与 `std.sync` 的执行在镜像内 Gugu 源；Rust 侧只留 schema、布局和非法序拒绝。

所有库 crate（`gugu-base`、`gugu-project`、`gugu-runtime-abi`、`gugu-runtime-world`、`gugu-sema`、`gugu-gir`、`gugu-lir`、`gugu-backend`、`gugu-frontend`、`gugu-compiler`）都使用 `#![forbid(unsafe_code)]`。平台入口、系统调用、原子、换栈、safepoint、GC 写屏障和外部函数交接以 `IntrinsicBoundary` 登记；context switch 已在 runtime 侧直接编码为 x86_64 machine 片段，通过 backend 的已验证契约交接。真实宿主 VM 与裸汇编仅位于 `coroutine_context` 手工验收二进制中，不进入 compiler 或默认测试套件。

### 编译器测试构建

`cargo nextest run --workspace` 使用当前 workspace 的测试构建配置：`opt-level = 3`、
`debug = 0`，保留 `debug-assertions` 与 `overflow-checks`；`[profile.dev.build-override]`
同步使用 `opt-level = 3` 与 `debug = 0`，使 build-dependencies 与目标产物共享依赖编译，
且不为验收切换新的 Cargo 构建 profile。`gugu-compiler` 的常规集成回归合并为单一
`tests/integration/` 二进制（与纯文档检查 `tests/docs_links.rs` 分离），避免每个集成测试
文件重复链接 180+ MiB 嵌入制品。构建时间与测试执行时间分别记录。`.config/nextest.toml`
使用 `test-threads = "num-cpus"`，调度槽数随可用 CPU 数伸缩，保持零重试与失败即停。重编译与
协议用例按既有的 2/3/4/6 槽权重限制并发，不在多核机器上固定独占；六槽用例在 12 槽时最多两个
并发，在六槽及以下独占。默认每个测试限时 3 秒，终止宽限期为 1 秒；整轮测试执行限时 120 秒。
超时终止测试进程组并判失败，不仅打印 `SLOW`。
CI 通过 `ci/nextest.toml` 的 `profile.ci` 继承调度槽数与权重，单项与整轮时限分别为 10 秒与 600 秒；整轮上限在套件规模已接近 300 秒后为容量余量放宽，单项时限不变。
该时限同样适用于显式运行 `slow-tests`；不得用连续分片、提高超时、黑名单或削弱 fixture 掩盖不合格的耗时。
发现超时后停止本轮，修复对应的编译或测试路径，再做定向验证；不继续串行堆积全量验收。

## 目标描述与 rt0 边界

`TargetName` 只接受规范登记的 `x86_64-linux` 和 `x86_64-windows`。每个目标由不可变 `TargetDescriptor` 提供架构、操作系统、对象格式、指针宽度、rt0 类型、页大小与 CPU 基线：

| 目标 | 对象格式 | 指针宽度 | rt0 边界 | 页大小 | CPU 基线 |
|---|---|---:|---|---:|---|
| `x86_64-linux` | ELF64 | 64 | Linux syscall | 4096 | `x86_64-v1` |
| `x86_64-windows` | PE32+ | 64 | Windows 薄 IAT | 4096 | `x86_64-v1` |

descriptor 还固定 `linux_interpreter`（Linux 为 `/lib64/ld-linux-x86-64.so.2`，Windows 为 `None`）、`sysroot_digest`（域 `gugu-target-sysroot-v1`，绑定目标名、解释器、登记导入库顺序与 `import_policy_revision`）、`import_policy_revision`、`runtime_tuning_profile_digest`（取 `RUNTIME_TUNING_PROFILE.digest()`）与 `backend_cost_profile_digest`（取 `BackendCostProfile::digest()`）。`TargetDescriptor::digest()` 以域 `gugu-target-descriptor-v1` 覆盖全部字段（含 pointer compression 能力），descriptor 整体进入 compiler identity 与 action key。

rt0 不是普通 Gugu 函数。Linux 入口和 Windows 薄导入路径由后端与平台 runtime 负责；`RuntimeResources` 只把这项边界附加到 image plan，不实现宿主启动、分配、调度或报告逻辑。这样可以使目标描述进入编译结果，同时保持公开的 rt0 启动契约由 [`运行时规范`](../spec/runtime.md#rt0-and-startup) 和 [`平台 ABI`](../spec/platform-abi.md#entry-relocation-tls)唯一规定。

`RuntimeRawModel` schema 5 把协程契约并入同一缓存对象（该 schema 已随后续阶段扩到 20，最新并入 `CompressionRuntimeContract`）：优化后 LIR 的创建点、入口检查与 suspend 需求，以及固定布局、栈策略和换栈字节都进入 fingerprint。内建 Gugu 协程源通过正常 LoadSources/前端/单态化形成 record，query 的构造与恢复均校验源布局。CLI `image-plan` 暴露 `coroutine-runtime` 和 `coroutine-contract-fingerprint`，`-Zdump-runtime` 输出逐字段 offset、arena/cache 策略和需求计数；任何 verifier 失败不形成 backend/image plan。

rt0 的确定性进程模型不再单独模拟协程数量：main 与获准创建的子协程均取得同一控制表和 stack arena allocation，首次进入时 commit，完成先向 cold 发布结果与 barrier，再在 system-stack 交接点清除旧 context/root 并归还 stack。既有终止计划继续决定是否等待存活子协程；关闭设施按原顺序 drain raw inbox、回收 stack/cache、释放范围。默认测试只运行确定性传输模型，`cargo bench -p gugu-compiler --bench coroutine_context` 执行实际 context 片段，验证独立栈往返、寄存器恢复、processor 重建和 finish 后结果存活。

## Action graph

每个 bootstrap action 预先登记为一个稠密节点，节点按以下固定顺序执行：

```text
resolve-target
      ↓
load-sources
      ↓
frontend
      ↓
build-ir
      ↓
codegen
      ↓
plan-backend
      ↓
attach-runtime
      ↓
validate-image
      ↓
emit-image
```

节点状态只有 `pending`、`complete`、`skipped` 和 `failed`。成功路径的 `validate-image` 验证镜像计划，`emit-image` 在镜像字节生成后为 `complete`（落盘由宿主工具完成）。失败路径从第一个失败节点开始把下游节点标为 `skipped`，编排器不执行降级编译、不调用外部 assembler/linker，也不写出部分产物。

回归测试使用 `Compiler::compile_sema_for_test`、`Compiler::compile_frontend_for_test`、
`Compiler::compile_to_lir` 或 `Compiler::compile_full_for_test` 显式选择阶段。
Clippy `disallowed_methods` 门禁禁止测试直接调用生产 `Compiler::compile`；需要完整后端的回归
使用测试专用入口声明阶段需求。

`compile_sema_for_test` 在语义检查、冻结 HIR 与文档/lint 诊断后结束，跳过 GIR、单态化与全程序分析；
`compile_frontend_for_test` 在完成前端 GIR/单态化/分析/placement 后结束，跳过 LIR 与后端；
`Compiler::compile_to_lir` 与 `Compiler::compile` 共用前置流水线，
在 LIR 通过结构校验后结束。成功结果保留诊断、源码表、HIR、GIR 和 LIR，将 `build-ir`
记为 `complete`、`codegen` 及下游记为 `skipped`。完整编译继续解析 runtime glue、派生
GC metadata 和 runtime raw 契约，然后进入机器码生成与镜像规划。

输入形态如下：

- `empty_package`：没有用户源文件，前端和 IR 成功完成，但没有 executable entry，后端之后的节点跳过，不产生 image plan；
- `single_file`：调用者提供逻辑路径和内存源码，适合确定性测试与编辑器；
- `single_file_path`：compiler 在 `load-sources` action 内读取指定 `.gg` 文件，逻辑路径按输入路径推导；读取或快照失败形成 `E0001`~`E0008` 并停止后续 action；
- `project_entry`：CLI 从清单发现的 target 入口，逻辑路径由 package root 推导，与工作目录无关；bin/example 与 `harness = false` 的 bench 要求合法 main，lib/test 与默认 bench 走库检查，不产生 executable entry。target 源码树的结构违规（例如符号链接）在输入校验中报告 `E0003`；CLI 路径通常在项目发现阶段就先拒绝符号链接。

空 package 没有内建 runtime 源图，不执行依赖源图的跨语言 record 校验，也不形成可执行入口；固定 machine layout 与契约 verifier 仍照常校验。非空输入一旦装入 runtime 源，就必须形成完整 record 集合，不能把缺失字段当作空 package 处理。

Frontend action 对每个源码快照运行词法分析：生成带精确 span 的 `TokenBuffer` 与 trivia，校验字面量、最长匹配、闭集属性与 cfg 记号形状。词法诊断 `E0009`–`E0019` 或 `Error` token 会使 Frontend action 失败，并跳过 IR 与 image plan。早期括号扫描入口检查已删除。

同一 Frontend action 内消费 `TokenBuffer`，用递归下降构造稠密 `u32` AST arena（声明、泛型、类型、块、表达式、模式、`async`/`select`/`try`/`defer`、`comptime source`、FFI 与 asm）。`()`/`[]` 增加分隔符深度，内部换行只作空白；`{` 单独跟踪花括号深度，块内换行可以结束语句、字段或臂。比较与 `..` 不结合，主诊断带 `Note` 次诊断。解析诊断 `E0020`–`E0026` 使 Frontend 失败，不得把错误占位交给 IR 或 image plan。可执行入口是 AST 中名为 `main`、无参数且带块体或 `=` 体的 `fn`。节点身份不是指针；结构 dump 按 arena 下标，不受线程完成顺序影响。

同一 Frontend action 继续做声明/表达式/模式/trait/unsafe 检查、布局校验和 `LowerHir` query。`BuildIr` 登记真实定义与冻结 owner；`Compilation::succeeded` 必须拥有 `Validated`，后端计划只接受此凭据。冷计算和缓存恢复都经过冻结 verifier，失败没有 image plan。旧 `ReturnUnit` IR 已移除；`BuildX64` 已对每个 LIR body 做 instruction selection、block layout 与直接编码，产出带 mangled 符号的函数级片段；`emit-image` 生成 ELF/PE 镜像字节，`gugu build` 将其物化到 `target/`。完整交接表见 [AST 与 HIR](ast-hir.md)。

`BuildIr` 同时报告 generic GIR：body / block / 语句数量。`ImagePlan` 含 `gir-body-count`、`gir-block-count`、`gir-statement-count` 与 `gir-fingerprint`。这些字段只说明已验证的 generic 操作树，不代表 monomorphic GIR 或机器码已经写出。

`BuildIr` 之后、附加 runtime 资源之前，compiler 通过 `RuntimeRawModel`（query 30，当时 schema 8、当前 20）构建并校验 runtime raw 平面契约：dense size class（raw 记录与 64-byte header 的 ResourceCell slab class 阶梯）、消息字段 schema、ResourceCell 状态位与迁移表、release 描述符 schema、File/socket/process/lock/FFI 资源种类目录与唯一 release 入口、batch 上限、shard 数量、queue-page grace 步骤、账本互斥分类、调度契约、等待契约、同步契约、压缩契约（cage 位布局、cage 上限与粒度、FFI 交接规则、目标能力与压缩需求）与需求视图。契约失败诊断为 `E0058`（退出码 101），`attach-runtime` 之后的 action 全部跳过且没有镜像计划。`ImagePlan` 因此增加 `raw-size-class-count`、`raw-shard-count`、`raw-batch-max-items`、`raw-batch-soft-bytes`、`raw-message-node-capacity` 与 `raw-model-fingerprint`，以及资源租约字段 `raw-resource-class-count`、`raw-resource-cell-header-bytes`、`raw-resource-kind-count`、`raw-release-descriptor-count`、`raw-resource-sites` 与 `raw-release-sites`；契约指纹同时进入 action key。

schema 6 再并入 `SchedulerRuntimeContract`（schema 1）：本地队列容量 256、remote 分片 8、batch 上限 128、service 间隔 61、service 批量 128，以及由优化后 LIR 推导的创建点、`RuntimeCall::Yield` 计数与 suspend 需求。`SchedulerDemand` 与协程需求对齐创建点与挂起点，`yield_sites` 只做上界一致性（`yield_sites <= suspend_points`）。`ImagePlan` 增加 `scheduler-local-capacity`、`scheduler-remote-shard-count`、`scheduler-batch-max-items`、`scheduler-service-interval`、`scheduler-service-batch`、`scheduler-contract-fingerprint` 与 `scheduler-runtime`；契约指纹以派生键 `gugu-scheduler-runtime-v1` 固定并进入 action key。镜像内调度是单 OS worker 的 Classic64 本地队列，入口在 `std/runtime/scheduler.gg`。`take_next` 从本地头取最老项，`yield` 放到本地尾。steal、多 worker remote carry 与 `scheduler_runqueue` bench 已随 Rust 执行体删除。契约常量与 dirty 配额仍由 `scheduler_schema` 固定。

schema 7 再并入 `WaitRuntimeContract`（schema 1）：等待源种类 channel/join/never、wait-node 字段目录（禁止裸栈指针）、FIFO wait 队列、`INLINE_SELECT_CASES = 8`、scratch class `1,2,4,…,1024` words、wait-node raw class 64/128 B、`SelectTxn` 相位 Building/Armed 与 winner UNSET/DEFAULT/case。`WaitDemand` 由优化后 LIR 统计 `ChannelNew/Close/Send/Receive/TrySend/TryRecv`、`JoinWait`、`SelectCommit`、never select 与 `SafepointKind::Select`。`ImagePlan` 增加 `wait-inline-select-cases`、`wait-scratch-class-count`、`wait-node-class-count`、`wait-contract-fingerprint`、`wait-demand` 与 `wait-runtime`；契约指纹以派生键 `gugu-wait-runtime-v1` 固定。内建 `std/runtime/channel.gg` 的 `ChannelControl`/`WaitNode`/`SelectTxn`/`SelectScratchCache` 经冻结 HIR/具体 GIR 与 machine 布局交叉校验。镜像内 channel、Join 与 select 由 `std/runtime/channel.gg` 执行，`RuntimeCall` 绑定到这些符号。Rust 执行体已删除。`ChannelControl`、`WaitNode`、`SelectTxn` 与 `SelectScratchCache` 的布局仍由 `wait_schema` 交叉校验。

schema 8 再并入 `SyncRuntimeContract`（schema 1）：内存序名称 Relaxed/Acquire/Release/AcqRel/SeqCst、合法原子标量类型目录、Once 状态枚举（Uninit/Initializing/Ready/Failed）、Cancel 状态枚举（Active/Cancelled）、Mutex 状态枚举（Unlocked/Locked/Contended），以及 5 类 64-byte/64-byte 对齐控制结构（`MutexControl`、`RwLockControl`、`CondvarControl`、`OnceControl`、`CancelControl`）。`SyncDemand` 由优化后 LIR 统计原子操作、锁操作、条件变量操作、OnceLock/Lazy 操作与取消操作。`ImagePlan` 增加 `sync-contract-fingerprint`、`sync-demand`、`sync-primitive-count` 与 `sync-runtime`；契约指纹以派生键 `gugu-sync-runtime-v1` 固定并进入 action key。内建 `std/runtime/sync.gg` 的 5 类控制块经冻结 HIR/具体 GIR 与 machine 布局交叉校验。镜像内 Mutex、RwLock、Condvar、Once 与 Cancel 由 `std/runtime/sync.gg` 和用户可见的 `std/sync.gg` 执行。锁争用只挂起当前协程。`sync_lock` bench 已删除。原子非法序仍由 `sync_schema` 的 `AtomicStateMachine` 在测试里拒绝。

schema 4 再并入 `Rt0SchemaV1`：rt0 五步启动序列、四个进程生命周期状态与单向迁移表、环境快照字段、7 个启动变量的文法与默认值、7 类 fatal 目录、退出类别与码规则、`gugu-runtime-report-v1` 报告 schema（固定字段序与 reason 目录）、`TerminationPlan` 字段与主线程关闭设施顺序、emergency buffer 策略（4096 字节定容、诊断配置非法时回退固定纯文本、先截断 message 再丢 backtrace 帧）。需求视图 `Rt0Demand` 由编译产物推导：`main` 是否存在、`main` 是否返回 `Result[(), E]`（决定 `main-error` 报告路径是否可达），二者进入 query key。`ImagePlan` 增加 `rt0-step-count`、`rt0-lifecycle-count`、`startup-config-var-count`、`startup-fatal-count`、`report-reason-count`、`rt0-emergency-buffer-bytes`、`rt0-contract-fingerprint` 与 `rt0-demand`；契约指纹以派生键 `gugu-rt0-startup-v1` 固定并进入 action key。`startup`/`lifecycle`/`report`/`termination` 参照实现消费同一组枚举：`RawWorld` 的 rt0 进程模型覆盖五步启动、`InvalidConfiguration` fatal、生命周期单向迁移、各终止路径的计划生成、`Terminating` 中的用户代码闸门、`PanicDuringUnwind` 升级与设施关闭的 exactly-once；报告只经定容 emergency buffer 渲染，不调用用户代码。镜像内真正的 rt0 与报告执行路径随 rt0 写出与调度阶段落地，复用同一 schema。

`ImagePlan` 再增加 `placement-count`、`turn-region-count`、`local-heap-count`、`shared-heap-count` 与 `placement-fingerprint`。这些字段记录逃逸与存储选择，不代表已经改写 CFG 做堆装箱或写出机器码。`large_copy` 警告进入 `Compilation` 诊断且不阻止镜像计划；升为错误时 Frontend 失败且没有镜像。

## runtime 源资源与实现归属

`RuntimeResources::builtin()` 返回 compiler 构建时嵌入的 Gugu 源文件登记：

| 逻辑路径 | 角色 | bootstrap 责任 |
|---|---|---|
| `std/prelude.gg` | 标准库源单元 | 证明标准库输入进入 action graph |
| `runtime/core.gg` | runtime 源单元 | 证明 runtime 输入进入 action graph |
| `std/runtime/platform.gg` | 平台范围适配 | 固定 `std.platform` 的 Gugu 入口与 profile 常量 |
| `std/runtime/coroutine.gg` | runtime 源单元 | 固定协程控制块布局交叉校验 |
| `std/runtime/channel.gg` | runtime 源单元 | 固定 `ChannelControl`/`WaitNode`/`SelectTxn`/`SelectScratchCache` 布局交叉校验 |
| `std/runtime/sync.gg` | runtime 源单元 | 固定 `MutexControl`/`RwLockControl`/`CondvarControl`/`OnceControl`/`CancelControl` 布局交叉校验 |
| `std/build.gg` | 标准库源单元 | `std.build` 门面：输入校验、保留名拒绝、调用转交 `std.runtime.build_task` |
| `std/runtime/build_task.gg` | runtime 源单元 | 构建 action 记录：头 + 追加条目原生 bump arena，非活动上下文一律 panic |

这些源单元在 `LoadSources` 内注入源码表，与用户源码走同一条解析、cfg 与检查路径。它们是 `std` 的私有实现：只有 `std` 内部模块可以互相引用，非 `std` 模块导入实现模块按保留名拒绝（`E0031`），用户源码占用内建逻辑路径同样被拒绝。`std/runtime/platform.gg`、`std/runtime/coroutine.gg`、`std/runtime/channel.gg` 与 `std/runtime/sync.gg` 的 `#[used]` 入口使对应布局记录成为闭世界镜像的根，不依赖某个用户调用点是否出现。

这些源单元是源树登记输入，不是 Rust runtime 的替代实现。Rust compiler 可以拥有读取、验证和编排逻辑；Gugu runtime 的可观察语义必须最终来自镜像内的 Gugu runtime、rt0 和登记的 machine intrinsic。任何新增 runtime 能力都必须同时说明其 Gugu 源实现、必要 intrinsic 和 compiler lowering，不能在 Rust 中复制一份正常执行路径。

## 公开规范归属表

下表给出每条公开规范的实现归属。状态列描述当前已交付的边界；未落地部分不能把当前 bootstrap 检查误认为该章节已经完成。

| 公开规范 | 主要实现归属 | 当前状态 |
|---|---|---|
| `overview` | `action`、`target`、`runtime` | 已建立闭世界/目标边界 |
| `lexical` | `source` 快照、`frontend` lexer | 快照与 lexer/trivia/闭集属性词法已落地 |
| `format-style` | `gugu-cli` fmt 与 formatter | `gugu fmt` 已接入 |
| `syntax` | `frontend` parser | 递归下降 AST、错误恢复与稳定 dump 已落地 |
| `types` | type arena、type checker | 类型形成、布局、TypeCheck 与冻结 TypeId 已落地 |
| `declarations` | module tree、definition collector | 模块树、定义收集、绑定与初始化检查已落地 |
| `program-model` | action、backend、runtime | 已建立 plan/不写部分镜像契约 |
| `packages-builds` | `project` 清单、依赖解析、workspace、target、锁图与缓存 | 清单发现、workspace、target、SemVer、锁图、依赖缓存与 vendor 已落地；`std.build` 门面与 action 记录已落地；build.gg 宿主执行闭环未落地 |
| `publishing-ecosystem` | registry、archive、signature | 未实现 |
| `toolchain-cli` | `gugu-cli` 与 action orchestrator | 已建立单一入口、项目/单文件模式与 `fmt`/`build`/`check` |
| `expressions` | frontend、HIR、GIR | 类型检查、HIR 与 generic GIR 已落地；LIR 与机器码未落地 |
| `patterns` | frontend parser、pattern checker | 穷尽性检查已落地 |
| `functions` | frontend parser、capture、async、HIR/GIR | 捕获、async 与 HIR 已落地 |
| `traits` | trait solver、impl selection | 选择、特化与 dyn/Any 前端已落地 |
| `passing` | value/resource lowering | GIR 已按类别展开浅拷、COW seal 与 resource lease；`large_copy` 已接入诊断 |
| `memory` | placement、resource runtime、GC、region registry | 已记录 TurnRegion/LocalHeap/SharedHeap 选择与 region 门禁、promote、transfer 路径；真实 GC 搬迁归属镜像内 Gugu runtime |
| `concurrency` | scheduler、channel、sync runtime | 单 OS worker 的队列、yield、park、channel/Join/select 与 `std.sync` 已在镜像内 Gugu runtime 执行；Rust 侧保留契约、布局与非法序拒绝 |
| `comptime` | evaluator、source expansion、analysis | EarlyConst、源码宏与 generic GIR 上的抽象分析已接入前端管线 |
| `unsafe` | safety checker、FFI/asm backend | 前端安全检查已落地；外部桥接执行与机器编码未落地 |
| `platform-abi` | `target`、x86 backend、image writer | 已建立两个目标 descriptor |
| `runtime` | Gugu runtime、rt0、报告路径 | main 协程、栈增长、单 worker 调度、channel/Join/select 与 `std.sync` 已进入镜像内 runtime；GC、共享堆与多 worker steal 仍待迁移 |
| `standard-library` | `runtime` Gugu 源树与 std modules | 已建立源树登记 |
| `testing` | test collector、harness、CLI | 未实现 |

`RuntimeRawModel` 把各自阶段的 runtime 契约并入同一缓存对象（该 schema 已从 13 扩到 22，最近加入 `BarrierRuntimeContract`、`LocalHeapRuntimeContract`、`MarkRuntimeContract`、`EdgeRuntimeContract`、`BlockReturnRuntimeContract`、`CompressionRuntimeContract`、`RoutingRuntimeContract` 与 `ProvenanceRuntimeContract`）：TurnRegion 契约部分包含容量 class 阶梯（64 B 起的十档，上界与单个 managed block 同源）、单 region 对象上界、单 owner 活跃 region 上界、transfer lease 上界、export summary 位目录、region 状态目录，以及 `RegionTransfer` 的字段目录与需求视图。`RegionTransfer` 与 return/card 共用同一条传输通道、同一个 node pool、同一套 producer staging 与 queue-page grace，消息只携带 region 序号、generation、type summary 编号、bytes、export state、来源 owner 与目标 owner 身份，不携带地址；receiving owner 采纳后两侧账本一起移动。`ImagePlan` 暴露 `turn-region-*` 与 `turn-region-contract-fingerprint`，`-Zdump-runtime` 输出容量阶梯、位目录、状态目录、字段目录与需求计数。runtime 侧的参照实现（`runtime/region.rs` 与 `world/region_impl.rs`）把同一份契约接到真实 owner 事实：region 容量在 `open` 时 commit 给 owner，`reset` 时 release，`promote` 时保留；resource lease 与 FFI 地址写入观察位后 reset 被拒绝并转为保留；在途 transfer 字节计入 credit 快照的 pending 项。`RuntimeRawModel` schema 14 在同一缓存对象上并入 `LocalHeapRuntimeContract`（当前 `LOCAL_HEAP_SCHEMA = 4`，为候选回收与 block return 补上 `HeapBlockRecord`、lease 门禁、归还入队位与全局 block 身份）：arena 2 MiB、block 32 KiB、line 128 byte、granule 16 byte、8 block TLAB span、object-start/mark/page-cover/card 尺寸、`ObjectHeader`/`HeapArenaMetadata`/`HeapPinEntry`/`HeapBlockRecord` 记录布局、arena/block/generation/representation 目录与 `HeapTriggerProfile`；`local-heap-demand` 由优化后 LIR 与冻结类型表推导并与 gc metadata、barrier 站点口径交叉校验，`local-heap-*` 四组键进入 `ImagePlan` 与 `-Zdump-runtime`。`RuntimeRawModel` schema 15 在同一缓存对象上并入 `MarkRuntimeContract`（当前 `MARK_SCHEMA = 3`）：每 owner 单 consumer MarkMailbox、`owner(8) | counter(24)` 的 credit id、cycle/snapshot/credit 目录、七项收敛条件与「条件 → credit 来源」绑定（来源并集必须恰好覆盖九个 credit 来源）、`MarkTicket` 的 15 个字段（`source_block` 为全局块身份，消费端据此解析来源 owner 并确认块仍存在），以及 `MarkMailboxHead`/`MarkCreditHead`/`MarkTerminationRecord` 三条 record 布局（与 `resources/runtime/mark.gg` 交叉校验）；`mark-demand` 由 gc metadata、barrier 与 LocalHeap 需求推导，`mark-*` 键进入 `ImagePlan` 与 `-Zdump-runtime`。`RuntimeRawModel` schema 17 再并入 `EdgeRuntimeContract`（`EDGE_SCHEMA = 1`）：候选 job 的十个相位、七个 block 候选状态、`EDGE_CANDIDATE_QUANTUM`、每 processor edge scratch 与 18 个 `EdgeDelta` 字段集合；`edge-demand` 由 barrier 的 `edge_summary_sites` 派生并与 mark 的 `edge_delta_sites` 交叉校验，`edge-*` 键进入 `ImagePlan` 与 `-Zdump-runtime`。

`RuntimeRawModel` schema 20 在同一缓存对象上并入 `CompressionRuntimeContract`（`COMPRESSION_SCHEMA = 1`，profile `mosaic-compression` revision 1）：cage id/generation/offset 位布局、cage 上限与 2 MiB 粒度、FFI 交接规则与六项统计名、目标能力核对；`compression-demand` 由优化后 LIR 推导（`DecodeCompressedRef` 解码点与按安全点聚合的压缩根槽），`compression-*` 键进入 `ImagePlan` 与 `-Zdump-runtime`。默认策略关闭，即 full-pointer 语义；cage 只在显式 profile 下预留。真实编译同样默认关闭、需求为零；`CompileRequest::with_compression_policy` 显式开启后，LIR 输入身份编入开关与 cage 字节数（LIR schema 随之升到 4），lowering 产出真实 `DecodeCompressedRef` 解码点与压缩环境分配，demand 由优化后 LIR 推导并进入契约，策略不允许在 lowering 之后事后改契约。

`RuntimeRawModel` schema 21 在同一缓存对象上并入 `RoutingRuntimeContract`（`ROUTING_SCHEMA = 1`，profile `mosaic-routing` revision 1），并把 raw policy revision 升到 3：路由模式目录（`direct`/`radix`）、固定 `2^k` bucket（`k = 6`，64 bucket）、有限 levels（`MAX_RADIX_LEVELS = 2`）、转发 hop 上限（`RADIX_HOP_LIMIT = 4`）、radix 拦截的消息族目录（return 族）、maintenance 相位目录与两条切换序列（direct → radix 为 `freeze-target-cache`/`flush-staging`/`publish-mode`，radix → direct 为 `drain-buckets`/`drain-forwarding`/`publish-mode`/`restore-target-cache`），以及五项统计口径；`routing-demand` 由 raw 平面需求的 owner 数与 raw/resource 站点之和推导。默认 direct，即 owner inbox 直达语义；`RoutingPlane` 在 radix 模式下按原始 target 的 route key bits 逐层 staging，终层经 owner directory 解析交付、转发或注入 domain，direct 模式不分配任何 bucket 表。`routing-*` 键进入 `ImagePlan` 与 `-Zdump-runtime`，`CompileRequest::with_routing_policy` 显式选择模式并随 raw policy 进入契约与 action key。

`RuntimeRawModel` schema 22 在同一缓存对象上并入 `ProvenanceRuntimeContract`（`PROVENANCE_SCHEMA = 1`，profile `mosaic-provenance` revision 1），并把 raw policy revision 升到 4：释放 provenance 检查目录（`canonical`/`alignment`/`range`/`class`/`owner`/`generation`/`link`/`state` 加上 debug/security 追加的 `poison`/`double-return-marker`/`full-chain`/`random-reuse`/`guard-region`/`checked-copy`）、per-domain secret 登记目录与管理规则、release 基线检查集合与 profile 激活序列（release 只保留 owner/generation/range/alignment 基线，debug 追加 poison、双重释放标记与全链 verifier，security 追加随机复用、guard region 与 checked copy）、七类释放拒绝分类与六项统计口径；`provenance-demand` 由 raw 平面需求的 owner 数与两份 class ladder 推导。默认 release，即基线 provenance 检查语义；`ProvenancePlane` 的 per-domain secret 由 world secret 经派生键推导、每个 domain 持有独立 `LinkCodec`，释放拒绝在真实释放路径上按类记账，debug/security 的额外检查只改变记账与复用顺序、不改变 managed trace 语义。`provenance-*` 键进入 `ImagePlan` 与 `-Zdump-runtime`，`CompileRequest::with_provenance_policy` 显式选择 profile 并随 raw policy 进入契约与 action key。

`RuntimeRawModel` schema 23 在同一缓存对象上并入 `CombiningRuntimeContract`（`COMBINING_SCHEMA = 1`，profile `mosaic-combining` revision 1），并把 raw policy revision 升到 5：冷操作 tag 目录（`global-range-refill`/`extent-coalesce`/`topology-rebuild`/`platform-trim`）、与 tag 目录逐项相同的允许用途、十项禁止用途（含 TLAB allocation、raw local pop、普通 remote return、channel/select/park-wake 线性化与任意 closure/drop glue）、operation record 的十个标量字段、状态目录与七条迁移、单条 fast path 与两条争用路径、九项统计口径，以及合并上限、轮次条目/字节预算、超时轮数、记录规范槽、chunk 槽位数（尾部 1 槽预留给 refill）与 chunk 上界；`combining-demand` 由 raw 平面需求的 owner 数与分配站点数推导。默认 direct，即冷操作在请求者上下文逐条执行同一份 handler；combined 模式把请求记录进非移动池，无争用走单原子 claim 字、争用挂到 owner 的 MCS 链尾，由固定上界的轮次按 FIFO 认领并按同类合并执行。`combining-*` 键进入 `ImagePlan` 与 `-Zdump-runtime`，`CompileRequest::with_combining_policy` 显式开启 combined。

`RuntimeRawModel` schema 24 把压缩契约升到 `COMPRESSION_SCHEMA = 2`、profile `mosaic-compression` revision 2：契约新增 cage 控制记录的字段表（`generation`/`cage_id`/`base`/`len`/`canonical_headroom`/`decodes`/`rejections` 的名字、字节偏移、宽度、记录字节数、对齐）与 `CAGE_CANONICAL_LIMIT`，两者与 runtime 侧 `CageControlRecord` 同源，机器解码序列按同一张表读取记录，`DecodeCompressedRef` 夹具与 `CompressionPlane` 的 checked 解码逐字对照；同一 schema 还让调度契约段与 `RUNTIME_TUNING_PROFILE` 逐字段核对（本地容量、远端 shard 数、batch 上限、service interval/batch、queue padding、cache line），段内自洽但 profile 对不上同样是非法状态。

`RuntimeRawModel` schema 26 并入外调交接契约：普通 blocking worker 上限、waiter 字节、队列上限、service 预算、worker stack 与 20µs 逻辑宽限只来自 `foreign_schema`。schema 25 把 `SchedulerRuntimeContract` 升到 schema 2，并入 `LogicalProcessorPrefix` 的 poll/ownership/TLAB/TurnRegion 偏移：`poll_flags_offset = 0`、`tlab_cursor_offset = 3520`、`turn_region_cursor_offset = 3536`。偏移只来自 `processor.rs` 的 `offset_of!`，backend 的 TLAB/TurnRegion bump、SafepointPoll 与 StackCheck 热路只消费契约字段；`-Zdump-runtime` 增加 `scheduler-layout poll-flags=… tlab-cursor=… turn-region-cursor=…` 行。

内部契约也沿同一边界扩展：[`AST/HIR`](ast-hir.md) 消费 frontend 产物，[`GIR/LIR`](gir-lir.md) 消费冻结 HIR，[`后端`](backend.md) 负责从合法 LIR 到 machine code，[`调度器`](scheduler.md) 和 [`GC 元数据`](gc-metadata.md) 负责 runtime 语义。不得为这些后续模块建立平行的占位语义路径。

## 验收契约

确定性测试覆盖以下可观察结果：

- 空 package 的所有 graph 节点都离开 `pending`，没有 image plan；
- `fn main() {}` 完成 frontend、IR、backend、runtime 和 image validation，并留下带目标、入口、runtime 源单元数量和 rt0 的内存计划；
- malformed source 产生稳定诊断，frontend 为 `failed`，下游为 `skipped`，没有 image plan；
- 两个登记目标使用不同 rt0/object format 边界，未登记目标不能解析；

源码与项目发现补充覆盖：

- 快照拒绝 BOM、非法 UTF-8（带精确字节偏移）与超长输入；行首表对 LF/CRLF/CR 混合输入给出确定行列映射，列号按 Unicode 标量计算，非字符边界偏移在行号计算与 Span 构造时被拒绝；
- `SourceMap` 按逻辑路径排序分配稠密 ID，重复路径拒绝；Span 绑定按内容派生的 `SourceTableId` 并拒绝跨表 span；span 半开范围、未知文件/展开 ID 均有稳定错误；宏展开记录按调用位置、轮次、片段顺序稳定注册并维护父链；
- BOM 输入使 `load-sources` 失败且诊断携带 `E0005`，下游 action 全部跳过；
- 单 package、虚拟 workspace、根 package workspace、成员目录启动的 package/target 选择与规范一致；glob 展开排除 `exclude` 并允许非存在目录，`default-members` 只在根启动时生效；
- 保留名 `std`（package 名与依赖别名）、未知核心字段、`foo.gg` 与 `foo/mod.gg` 冲突、target 重名、入口越过 package 根均在编译前失败；显式 `path` 直接位于 package 根时正确解析源码根为 package 本身；冲突检查忽略 `target` 目录与隐藏目录；
- target 的 `required-features` 在编译前依据启用 feature 过滤，未知 feature 名以退出码 2 失败；
- 单文件模式拒绝全部项目选择参数并以退出码 2 失败。

依赖解析补充覆盖：

- SemVer 的规范数字、预发布 precedence、caret/tilde/关系/逗号交集/wildcard 与 build metadata 规则；无效约束在候选选择前失败；
- `cfg(all/any/not(...))` target 条件按 target/host 正确激活，path、Git、registry source 按 package identity 区分，别名不改变 package ID；
- normal、test、build 三个解析域的传播边界、optional/依赖 feature 并集、默认 feature 与 root `--no-default-features` 状态可重现；
- 候选版本选择不受输入顺序影响，yanked/无解版本、source candidate 缺失和依赖循环产生稳定错误；
- 锁图拒绝绝对路径、未知 source、重复 package ID 和悬空边，规范编码排序稳定且重复读写不改变内容；CLI `--locked` 在锁图与清单不一致时于 frontend 前失败。

最终镜像写出、Gugu 源 runtime 自举、GC、scheduler 和双目标 machine code 必须在各自规范和测试中完成，不能把当前内存计划误认为已经写出可执行镜像。
