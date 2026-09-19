# Compiler bootstrap 与 action graph

本章固定官方 compiler 从工程入口接入完整编译管线时的 bootstrap 边界。它是实现说明，不增加 Gugu 语言语义，也不构成跨 compiler identity 的 ABI。公开行为以 [`spec/`](../spec/overview.md) 为准；后续能力必须在本章登记的归属层继续扩展，不能另起一套语义等价实现。

## 工程入口与镜像计划

官方 compiler 以 Rust 实现 bootstrap：单一 `gugu` 入口、目标描述、确定性诊断、前端、稠密 IR、后端 image plan 和 Gugu runtime 源资源登记。它可以检查空 package、内存中的单文件入口和文件系统单文件入口，并为合法的 `fn main() { ... }` 生成端到端 action graph。

`ImagePlan` 是 compiler 内存中的验证结果，不是 ELF、PE、静态库或共享库。`emit-image` action 保持 `skipped`，因此成功检查不会写出伪造的目标镜像；任一前置 action 失败时，所有后续 action 都会被跳过，结果中不会留下镜像计划。machine encoder、镜像 writer 和 rt0 写出分别由[后端](backend.md)与[运行时](../spec/runtime.md#rt0-and-startup)契约规定，当前尚未物化。

## CLI 入口与输出

`gugu` 是唯一 CLI 入口：根级全局参数可在子命令前后解析，配置按内置默认、用户配置、当前 workspace 的 `.gugu/config.toml`、`--config`、环境变量、命令行的顺序合并，后层覆盖前层。`--frozen` 在解析结果中同时设置 `offline` 与 `locked`。

规范表中的 `new`、`init`、`build`、`check`、`run`、`test`、`bench`、`fmt`、`doc`、`clean`、`add`、`remove`、`update`、`tree`、`vendor`、`package`、`publish`、`yank`、`login`、`cache`、`explain`、`version` 和 `help` 均已登记。`build`、`check`、`fmt`、`version` 和 `help` 接入真实 action；其它已登记命令返回统一 `cli-error`，不会调用 compiler。
现状基线是 [`gugu-cli`](../../../crates/gugu-cli/src/main.rs)：compiler 已接入源码、清单、依赖、缓存、词法/AST、类型/语义检查、冻结 HIR 与 generic GIR；runtime 调度/GC、LIR、后端机器码与标准库语义仍按对应内部契约推进。
`text` 保留人读的 action/诊断/最终结果，诊断按 `级别[代码]: 消息`、`--> 文件:行:列` 与带行号源码片段块渲染；`json` 为 NDJSON 事件信封，bootstrap 的构建事件顺序固定为 `build-start`、诊断、`build-finish`；`json-diagnostic-short` 只发布诊断事件。NDJSON 对源码路径使用逻辑相对路径，对工作区外路径使用 `<external>/文件名`，并清理凭据键值。

## 源码快照与 Span

`source` 模块落地源码快照与 Span 系统：`load-sources` 把每个输入固化为不可变 `SourceSnapshot`——UTF-8 校验、BOM 拒绝、`u32` 长度上限、BLAKE3-256 内容摘要与规范行首表。逻辑路径按词法归一：解析 `.` 与 `..`、统一为正斜杠分隔符、拒绝绝对路径、反斜杠与越界回溯，保证相同输入在不同操作系统、工作目录与换行环境下产生相同 span。Span 端点必须落在 UTF-8 字符边界，行首表的列号按 Unicode 标量而非字节数计算；Span 携带所属源码表的 `SourceTableId`，跨表的 span 在宏展开注册时被拒绝。

`SourceMap` 按逻辑路径排序分配稠密文件 ID、拒绝重复路径，并为源码宏提供确定性展开注册：`ExpansionRecord` 记录父展开、宏调用与定义位置、生成源码哈希与片段类别，注册顺序按调用位置、轮次与片段顺序稳定。诊断的源码校验码为 `E0004`~`E0008`（非法 UTF-8、BOM、非法逻辑路径、span 越界、源文件过大），全部诊断按路径、偏移、级别、代码稳定排序。源码宏生成文本的快照字节校验失败按快照自身的代码报告（BOM 为 `E0005`、非法 UTF-8 为 `E0004`、超限为 `E0008`），不再一律归为非法逻辑路径；解析闸门在此之前已把生成文本送回主 lexer/parser，脚本可以像处理语法错误一样捕获这类失败。

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
├── gugu-cli/
│   ├── src/main.rs                 单一 gugu 入口、参数解析与 bootstrap 执行编排
│   ├── src/config.rs               CLI 配置合并、环境变量与本地配置发现
│   └── src/output.rs               Text/JSON 格式化、事件流、诊断渲染与敏感信息清理
└── gugu-compiler/
    ├── src/lib.rs                  CompileRequest 与 action 编排
    ├── src/action.rs               稠密 action graph 与状态迁移
    ├── src/diagnostics.rs          稳定代码、源码范围与排序
    ├── src/diagnostics/            人读诊断渲染
    │   ├── human.rs                片段、插入符与 ANSI 颜色的文本布局
    │   └── human_tests.rs          渲染契约测试
    ├── src/source.rs               源码快照、Span、行首表与展开记录
    ├── src/project/                清单、workspace、target、依赖、锁图与缓存输入
    │   ├── mod.rs                  项目聚合、选择与缓存输入导出
    │   ├── model.rs                package、target 与 workspace 模型
    │   ├── error.rs                项目发现、选择、依赖与锁文件错误
    │   ├── manifest.rs             清单 schema 与 package 构建
    │   ├── targets.rs              target 自动发现与源码布局校验
    │   ├── dependency_model.rs     package identity、source、域与 feature 模型
    │   ├── dependency_manifest.rs  依赖清单与 workspace 继承解析
    │   ├── semver.rs               SemVer 版本和版本约束
    │   ├── lock.rs                 确定性 gugu.lock 编解码与校验
    │   ├── resolver.rs             候选求解、域传播与循环检查
    │   ├── dependencies.rs         依赖子模块聚合与公共导出
    │   ├── dependencies_tests.rs   依赖解析、feature 和锁图确定性测试
    │   ├── cache.rs                依赖缓存、vendor、锁图输入和缓存记录
    │   ├── package_files.rs        归档解包、规范路径和 package checksum
    │   ├── action_key.rs           完整编译输入编码与 action key
    │   ├── target_view.rs          target 用户产物视图与原子物化
    │   ├── cache_tests.rs          缓存输入、损坏隔离、vendor 和 key 确定性测试
    │   └── workspace.rs             workspace 成员与 glob 解析
    ├── src/frontend/               词法、AST、语义检查与冻结 HIR
    │   ├── mod.rs                  Frontend action 入口
    │   ├── lex.rs                  TokenBuffer、trivia 与字面量
    │   ├── parse/                  项、类型、表达式、模式
    │   ├── ast.rs                  u32 arena 与节点种类
    │   ├── semantics/              TypeCheck、布局交接与 AST 到 HIR 形成
    │   └── hir/                    类型化 owner、侧表与冻结 verifier
    ├── src/backend.rs              目标相关的内存 image plan 输入
    ├── src/runtime/                Gugu 源树登记、raw 平面契约、rt0 契约与 owner-directed return 参照实现
    │   ├── mod.rs                  源树/rt0/intrinsic 登记与 raw 平面常量
    │   ├── model.rs                `RuntimeRawContractV1`、消息字段 schema 与 query driver
    │   ├── startup_kinds.rs        rt0 启动/终止/报告的共享枚举目录与常量
    │   ├── startup_schema.rs       rt0 启动序列、生命周期、启动变量、fatal/退出/报告契约段
    │   ├── startup.rs              环境快照与启动配置解析的参照实现
    │   ├── lifecycle.rs            生命周期状态机与 rt0 启动序列参照实现
    │   ├── report.rs               emergency buffer 与 text/NDJSON 报告渲染参照实现
    │   ├── termination.rs          `TerminationPlan` 构造与退出码解析参照实现
    │   ├── provider.rs             平台 range 操作、二次幂 extent 阶梯与确定性替身
    │   ├── platform.rs             Linux/Windows profile 常量与 fake platform
    │   ├── platform_schema.rs      平台操作目录、range 状态迁移与失败映射契约
    │   ├── extent.rs               extent 阶梯、三路 lease 与 queue-page grace 门禁
    │   ├── ledger.rs               内存账本互斥分类 schema
    │   ├── resource.rs             ResourceCell slab、lease 状态机与统一 release 入口
    │   ├── resource_schema.rs      ResourceCell header/状态位/release 描述符/资源种类契约 schema
    │   ├── size_class.rs           dense size class 表与 stride 除法常量
    │   ├── coroutine.rs            hot/cold/slot 固定布局、context 与地址稳定控制页
    │   ├── coroutine_schema.rs     版本化协程布局、栈策略、LIR需求与片段契约
    │   ├── scheduler_schema.rs     版本化调度容量、分片、batch 与 service 节奏契约段
    │   ├── scheduler.rs            runnable 队列、park、steal、topology 与终止的确定性参照模型
    │   ├── scheduler_tests.rs      调度 deque 双变体、唤醒、retire 与契约闭环的确定性回归
    │   ├── coroutine_layout.rs     冻结HIR/具体GIR与machine布局交叉验证
    │   ├── context.rs              x86_64 switch/restore-only 直接编码
    │   ├── stack.rs                栈尺寸、迟滞收缩与精确StackInterior复制
    │   ├── stack_arena.rs          双端guard arena、span bitmap与有界cache
    │   ├── slab.rs                 owner 目录、slab 描述符与 slot 状态机
    │   ├── owner.rs                本地 free path、本地/远程 return 分叉
    │   ├── message.rs              return message、link 编码、producer staging 与 source slab 聚合
    │   ├── inbox.rs                8 shard batch queue、bounded snapshot、epoch gate
    │   ├── world/                  owner 上下文 service、转发、grace 与 retire
    │   │   ├── mod.rs              raw owner 世界与 ResourceCell 世界骨架
    │   │   ├── extent_impl.rs      arena 开立、extent 取用与 trim 门禁接入
    │   │   ├── resource_impl.rs    资源分配、lease、close、release 与整页 mapping 接入
    │   │   ├── coroutine_impl.rs   main/子协程创建、栈复制、cold完成发布与owner return
    │   │   ├── wait_impl.rs        channel / Join wait / select 接入 ready_publish
    │   │   └── termination_impl.rs rt0 进程模型：boot、终止路径与设施关闭接入
    │   ├── wait.rs                 WaitSourceId、wait-node slab、FIFO 与 park
    │   ├── wait_schema.rs          WaitRuntimeContract schema 1 与 WaitDemand
    │   ├── channel.rs              有/无缓冲环、会合、close 与 try_* 线性化
    │   ├── channel_layout.rs       channel.gg 与 machine 布局交叉校验
    │   ├── select.rs               xoshiro256++ 与两条 select 提交路径
    │   ├── harness.rs              真实并发可运行切片（bench façade）
    │   └── tests.rs                确定性参照实现的验证套件
    └── resources/
        ├── std/prelude.gg          标准库 Gugu 源单元
        ├── runtime/core.gg         runtime Gugu 源单元
        ├── runtime/platform.gg     std.platform 平台范围适配的 Gugu 源单元
        ├── runtime/coroutine.gg    固定控制块、栈策略与record访问的 Gugu 源单元
        └── runtime/channel.gg      等待控制块布局交叉校验的 Gugu 源单元
```

模块职责是单向的：CLI 只构造请求和渲染结果；compiler 负责管线编排；frontend 不创建机器码；IR 不读取源码文本；backend 只消费 IR 与目标描述；runtime 模块只提供 compiler 携带的 Gugu 源资源和边界登记。Rust compiler 不实现 Gugu runtime 的镜像执行路径：调度、GC、资源释放与标准库语义必须在镜像内的 Gugu runtime、rt0 与登记的 machine intrinsic 中落地；compiler 只持有确定性的契约与参照模型（raw 平面、资源租约、owner-directed return、rt0 启动/终止/报告与 channel/Join/select 等待协议），用于固定 schema、verifier 与参照行为。

`gugu-compiler` 使用 `#![forbid(unsafe_code)]`。平台入口、系统调用、原子、换栈、safepoint、GC 写屏障和外部函数交接以 `IntrinsicBoundary` 登记；context switch 已在 runtime 侧直接编码为 x86_64 machine 片段，通过 backend 的已验证契约交接。真实宿主 VM 与裸汇编仅位于 `coroutine_context` 手工验收二进制中，不进入 compiler 或默认测试套件。

## 目标描述与 rt0 边界

`TargetName` 只接受规范登记的 `x86_64-linux` 和 `x86_64-windows`。每个目标由不可变 `TargetDescriptor` 提供架构、操作系统、对象格式、指针宽度和 rt0 类型：

| 目标 | 对象格式 | 指针宽度 | rt0 边界 |
|---|---|---:|---|
| `x86_64-linux` | ELF64 | 64 | Linux syscall |
| `x86_64-windows` | PE32+ | 64 | Windows 薄 IAT |

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
plan-backend
      ↓
attach-runtime
      ↓
validate-image
      ↓
emit-image
```

节点状态只有 `pending`、`complete`、`skipped` 和 `failed`。成功路径的 `validate-image` 只验证内存计划；`emit-image` 为 `skipped`。失败路径从第一个失败节点开始把下游节点标为 `skipped`，编排器不执行降级编译、不调用外部 assembler/linker，也不写出部分产物。

输入形态如下：

- `empty_package`：没有用户源文件，前端和 IR 成功完成，但没有 executable entry，后端之后的节点跳过，不产生 image plan；
- `single_file`：调用者提供逻辑路径和内存源码，适合确定性测试与编辑器；
- `single_file_path`：compiler 在 `load-sources` action 内读取指定 `.gg` 文件，逻辑路径按输入路径推导；读取或快照失败形成 `E0001`~`E0008` 并停止后续 action；
- `project_entry`：CLI 从清单发现的 target 入口，逻辑路径由 package root 推导，与工作目录无关；bin/example 与 `harness = false` 的 bench 要求合法 main，lib/test 与默认 bench 走库检查，不产生 executable entry。target 源码树的结构违规（例如符号链接）在输入校验中报告 `E0003`；CLI 路径通常在项目发现阶段就先拒绝符号链接。

空 package 没有内建 runtime 源图，不执行依赖源图的跨语言 record 校验，也不形成可执行入口；固定 machine layout 与契约 verifier 仍照常校验。非空输入一旦装入 runtime 源，就必须形成完整 record 集合，不能把缺失字段当作空 package 处理。

Frontend action 对每个源码快照运行词法分析：生成带精确 span 的 `TokenBuffer` 与 trivia，校验字面量、最长匹配、闭集属性与 cfg 记号形状。词法诊断 `E0009`–`E0019` 或 `Error` token 会使 Frontend action 失败，并跳过 IR 与 image plan。早期括号扫描入口检查已删除。

同一 Frontend action 内消费 `TokenBuffer`，用递归下降构造稠密 `u32` AST arena（声明、泛型、类型、块、表达式、模式、`async`/`select`/`try`/`defer`、`comptime source`、FFI 与 asm）。`()`/`[]` 增加分隔符深度，内部换行只作空白；`{` 单独跟踪花括号深度，块内换行可以结束语句、字段或臂。比较与 `..` 不结合，主诊断带 `Note` 次诊断。解析诊断 `E0020`–`E0026` 使 Frontend 失败，不得把错误占位交给 IR 或 image plan。可执行入口是 AST 中名为 `main`、无参数且带块体或 `=` 体的 `fn`。节点身份不是指针；结构 dump 按 arena 下标，不受线程完成顺序影响。

同一 Frontend action 继续做声明/表达式/模式/trait/unsafe 检查、布局校验和 `LowerHir` query。`BuildIr` 登记真实定义与冻结 owner；`Compilation::succeeded` 必须拥有 `Validated`，后端计划只接受此凭据。冷计算和缓存恢复都经过冻结 verifier，失败没有 image plan。旧 `ReturnUnit` IR 已移除；当前没有生成目标机器码，`emit-image` 仍跳过。完整交接表见 [AST 与 HIR](ast-hir.md)。

`BuildIr` 同时报告 generic GIR：body / block / 语句数量。`ImagePlan` 含 `gir-body-count`、`gir-block-count`、`gir-statement-count` 与 `gir-fingerprint`。这些字段只说明已验证的 generic 操作树，不代表 monomorphic GIR 或机器码已经写出。

`BuildIr` 之后、附加 runtime 资源之前，compiler 通过 `RuntimeRawModel`（query 30，当时 schema 8、当前 20）构建并校验 runtime raw 平面契约：dense size class（raw 记录与 64-byte header 的 ResourceCell slab class 阶梯）、消息字段 schema、ResourceCell 状态位与迁移表、release 描述符 schema、File/socket/process/lock/FFI 资源种类目录与唯一 release 入口、batch 上限、shard 数量、queue-page grace 步骤、账本互斥分类、调度契约、等待契约、同步契约、压缩契约（cage 位布局、cage 上限与粒度、FFI 交接规则、目标能力与压缩需求）与需求视图。契约失败诊断为 `E0058`（退出码 101），`attach-runtime` 之后的 action 全部跳过且没有镜像计划。`ImagePlan` 因此增加 `raw-size-class-count`、`raw-shard-count`、`raw-batch-max-items`、`raw-batch-soft-bytes`、`raw-message-node-capacity` 与 `raw-model-fingerprint`，以及资源租约字段 `raw-resource-class-count`、`raw-resource-cell-header-bytes`、`raw-resource-kind-count`、`raw-release-descriptor-count`、`raw-resource-sites` 与 `raw-release-sites`；契约指纹同时进入 action key。

schema 6 再并入 `SchedulerRuntimeContract`（schema 1）：本地队列容量 256、remote 分片 8、batch 上限 128、service 间隔 61、service 批量 128，以及由优化后 LIR 推导的创建点、`RuntimeCall::Yield` 计数与 suspend 需求。`SchedulerDemand` 与协程需求对齐创建点与挂起点，`yield_sites` 只做上界一致性（`yield_sites <= suspend_points`）。`ImagePlan` 增加 `scheduler-local-capacity`、`scheduler-remote-shard-count`、`scheduler-batch-max-items`、`scheduler-service-interval`、`scheduler-service-batch`、`scheduler-contract-fingerprint` 与 `scheduler-runtime`；契约指纹以派生键 `gugu-scheduler-runtime-v1` 固定并进入 action key。调度参照模型覆盖双变体 deque、`run_next` 限幅、overflow、分片 carry、park 唤醒、steal、topology 与终止执行，真实并发只在 `cargo bench -p gugu-compiler --bench scheduler_runqueue` 中 smoke，确定性正确性由单测承担。

schema 7 再并入 `WaitRuntimeContract`（schema 1）：等待源种类 channel/join/never、wait-node 字段目录（禁止裸栈指针）、FIFO wait 队列、`INLINE_SELECT_CASES = 8`、scratch class `1,2,4,…,1024` words、wait-node raw class 64/128 B、`SelectTxn` 相位 Building/Armed 与 winner UNSET/DEFAULT/case。`WaitDemand` 由优化后 LIR 统计 `ChannelNew/Close/Send/Receive/TrySend/TryRecv`、`JoinWait`、`SelectCommit`、never select 与 `SafepointKind::Select`。`ImagePlan` 增加 `wait-inline-select-cases`、`wait-scratch-class-count`、`wait-node-class-count`、`wait-contract-fingerprint`、`wait-demand` 与 `wait-runtime`；契约指纹以派生键 `gugu-wait-runtime-v1` 固定。内建 `std/runtime/channel.gg` 的 `ChannelControl`/`WaitNode`/`SelectTxn`/`SelectScratchCache` 经冻结 HIR/具体 GIR 与 machine 布局交叉校验。参照模型覆盖有/无缓冲线性化、会合、close 不丢缓冲、Join wait 队列、`select` 两条提交路径、never wait，以及跨 owner `ReturnKind::WaitNode`；唤醒只经 `ready_publish`。镜像内真正的 channel/select 执行路径仍待后续写出，复用同一 schema。

schema 8 再并入 `SyncRuntimeContract`（schema 1）：内存序名称 Relaxed/Acquire/Release/AcqRel/SeqCst、合法原子标量类型目录、Once 状态枚举（Uninit/Initializing/Ready/Failed）、Cancel 状态枚举（Active/Cancelled）、Mutex 状态枚举（Unlocked/Locked/Contended），以及 5 类 64-byte/64-byte 对齐控制结构（`MutexControl`、`RwLockControl`、`CondvarControl`、`OnceControl`、`CancelControl`）。`SyncDemand` 由优化后 LIR 统计原子操作、锁操作、条件变量操作、OnceLock/Lazy 操作与取消操作。`ImagePlan` 增加 `sync-contract-fingerprint`、`sync-demand`、`sync-primitive-count` 与 `sync-runtime`；契约指纹以派生键 `gugu-sync-runtime-v1` 固定并进入 action key。内建 `std/runtime/sync.gg` 的 5 类控制块经冻结 HIR/具体 GIR 与 machine 布局交叉校验。参照模型覆盖原子序步调一致与 SeqCst 全局序、锁争用只挂起协程、non-poisoning 守卫、租约与完成自动解锁、OnceLock 异常永久 Failed、取消幂等与协作、以及 channel/Join 等待取消接缝（不丢已线性化消息、绝不杀死子协程）。真实多线程争用由 `cargo bench -p gugu-compiler --bench sync_lock` 验证，确定性正确性由单测承担。镜像内真正的同步原语执行仍待后续阶段交付，复用同一 schema。

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
| `packages-builds` | `project` 清单、依赖解析、workspace、target、锁图与缓存 | 清单发现、workspace、target、SemVer、锁图、依赖缓存与 vendor 已落地；build.gg 闭环未落地 |
| `publishing-ecosystem` | registry、archive、signature | 未实现 |
| `toolchain-cli` | `gugu-cli` 与 action orchestrator | 已建立单一入口、项目/单文件模式与 `fmt`/`build`/`check` |
| `expressions` | frontend、HIR、GIR | 类型检查、HIR 与 generic GIR 已落地；LIR 与机器码未落地 |
| `patterns` | frontend parser、pattern checker | 穷尽性检查已落地 |
| `functions` | frontend parser、capture、async、HIR/GIR | 捕获、async 与 HIR 已落地 |
| `traits` | trait solver、impl selection | 选择、特化与 dyn/Any 前端已落地 |
| `passing` | value/resource lowering | GIR 已按类别展开浅拷、COW seal 与 resource lease；`large_copy` 已接入诊断 |
| `memory` | placement、resource runtime、GC、region registry | 已记录 TurnRegion/LocalHeap/SharedHeap 选择与 region 门禁、promote、transfer 路径；真实 GC 搬迁仍未物化 |
| `concurrency` | scheduler、channel、sync runtime | 调度基础路径与 channel/Join/select 等待协议参照模型已接入；镜像内执行仍待后续写出 |
| `comptime` | evaluator、source expansion、analysis | EarlyConst、源码宏与 generic GIR 上的抽象分析已接入前端管线 |
| `unsafe` | safety checker、FFI/asm backend | 前端安全检查已落地；外部桥接执行与机器编码未落地 |
| `platform-abi` | `target`、x86 backend、image writer | 已建立两个目标 descriptor |
| `runtime` | Gugu runtime、rt0、报告路径 | 已建立资源与 rt0 边界；raw 平面契约、资源租约、owner-directed return 与 rt0 启动/终止/报告契约及参照实现已落地 |
| `standard-library` | `runtime` Gugu 源树与 std modules | 已建立源树登记 |
| `testing` | test collector、harness、CLI | 未实现 |

`RuntimeRawModel` 把各自阶段的 runtime 契约并入同一缓存对象（该 schema 已从 13 扩到 20，最近加入 `BarrierRuntimeContract`、`LocalHeapRuntimeContract`、`MarkRuntimeContract`、`EdgeRuntimeContract`、`BlockReturnRuntimeContract` 与 `CompressionRuntimeContract`）：TurnRegion 契约部分包含容量 class 阶梯（64 B 起的十档，上界与单个 managed block 同源）、单 region 对象上界、单 owner 活跃 region 上界、transfer lease 上界、export summary 位目录、region 状态目录，以及 `RegionTransfer` 的字段目录与需求视图。`RegionTransfer` 与 return/card 共用同一条传输通道、同一个 node pool、同一套 producer staging 与 queue-page grace，消息只携带 region 序号、generation、type summary 编号、bytes、export state、来源 owner 与目标 owner 身份，不携带地址；receiving owner 采纳后两侧账本一起移动。`ImagePlan` 暴露 `turn-region-*` 与 `turn-region-contract-fingerprint`，`-Zdump-runtime` 输出容量阶梯、位目录、状态目录、字段目录与需求计数。runtime 侧的参照实现（`runtime/region.rs` 与 `world/region_impl.rs`）把同一份契约接到真实 owner 事实：region 容量在 `open` 时 commit 给 owner，`reset` 时 release，`promote` 时保留；resource lease 与 FFI 地址写入观察位后 reset 被拒绝并转为保留；在途 transfer 字节计入 credit 快照的 pending 项。`RuntimeRawModel` schema 14 在同一缓存对象上并入 `LocalHeapRuntimeContract`（当前 `LOCAL_HEAP_SCHEMA = 4`，为候选回收与 block return 补上 `HeapBlockRecord`、lease 门禁、归还入队位与全局 block 身份）：arena 2 MiB、block 32 KiB、line 128 byte、granule 16 byte、8 block TLAB span、object-start/mark/page-cover/card 尺寸、`ObjectHeader`/`HeapArenaMetadata`/`HeapPinEntry`/`HeapBlockRecord` 记录布局、arena/block/generation/representation 目录与 `HeapTriggerProfile`；`local-heap-demand` 由优化后 LIR 与冻结类型表推导并与 gc metadata、barrier 站点口径交叉校验，`local-heap-*` 四组键进入 `ImagePlan` 与 `-Zdump-runtime`。`RuntimeRawModel` schema 15 在同一缓存对象上并入 `MarkRuntimeContract`（当前 `MARK_SCHEMA = 3`）：每 owner 单 consumer MarkMailbox、`owner(8) | counter(24)` 的 credit id、cycle/snapshot/credit 目录、七项收敛条件与「条件 → credit 来源」绑定（来源并集必须恰好覆盖九个 credit 来源）、`MarkTicket` 的 15 个字段（`source_block` 为全局块身份，消费端据此解析来源 owner 并确认块仍存在），以及 `MarkMailboxHead`/`MarkCreditHead`/`MarkTerminationRecord` 三条 record 布局（与 `resources/runtime/mark.gg` 交叉校验）；`mark-demand` 由 gc metadata、barrier 与 LocalHeap 需求推导，`mark-*` 键进入 `ImagePlan` 与 `-Zdump-runtime`。`RuntimeRawModel` schema 17 再并入 `EdgeRuntimeContract`（`EDGE_SCHEMA = 1`）：候选 job 的十个相位、七个 block 候选状态、`EDGE_CANDIDATE_QUANTUM`、每 processor edge scratch 与 18 个 `EdgeDelta` 字段集合；`edge-demand` 由 barrier 的 `edge_summary_sites` 派生并与 mark 的 `edge_delta_sites` 交叉校验，`edge-*` 键进入 `ImagePlan` 与 `-Zdump-runtime`。

`RuntimeRawModel` schema 20 在同一缓存对象上并入 `CompressionRuntimeContract`（`COMPRESSION_SCHEMA = 1`，profile `mosaic-compression` revision 1）：cage id/generation/offset 位布局、cage 上限与 2 MiB 粒度、FFI 交接规则与六项统计名、目标能力核对；`compression-demand` 由优化后 LIR 推导（`DecodeCompressedRef` 解码点与按安全点聚合的压缩根槽），`compression-*` 键进入 `ImagePlan` 与 `-Zdump-runtime`。默认策略关闭，即 full-pointer 语义；cage 只在显式 profile 下预留。真实编译同样默认关闭、需求为零；`CompileRequest::with_compression_policy` 显式开启后，LIR 输入身份编入开关与 cage 字节数（LIR schema 随之升到 4），lowering 产出真实 `DecodeCompressedRef` 解码点与压缩环境分配，demand 由优化后 LIR 推导并进入契约，策略不允许在 lowering 之后事后改契约。

内部契约也沿同一边界扩展：[`AST/HIR`](ast-hir.md) 消费 frontend 产物，[`GIR/LIR`](gir-lir.md) 消费冻结 HIR，[`后端`](backend.md) 负责从合法 LIR 到 machine code，[`调度器`](scheduler.md) 和 [`GC 元数据`](gc-metadata.md) 负责 runtime 语义。不得为这些后续模块建立平行的占位语义路径。

## 验收契约

确定性测试覆盖以下可观察结果：

- 空 package 的所有 graph 节点都离开 `pending`，没有 image plan；
- `fn main() {}` 完成 frontend、IR、backend、runtime 和 image validation，并留下带目标、入口、runtime 源单元数量和 rt0 的内存计划；
- malformed source 产生稳定诊断，frontend 为 `failed`，下游为 `skipped`，没有 image plan；
- 两个登记目标使用不同 rt0/object format 边界，未登记目标不能解析；

源码与项目发现补充覆盖：

- 快照拒绝 BOM、非法 UTF-8（带精确字节偏移）与超长输入；行首表对 LF/CRLF/CR 混合输入给出确定行列映射，列号按 Unicode 标量计算，非字符边界偏移在行号计算与 Span 构造时被拒绝；
- `SourceMap` 按逻辑路径排序分配稠密 ID，重复路径拒绝；Span 绑定 `SourceTableId` 并拒绝跨表 span；span 半开范围、未知文件/展开 ID 均有稳定错误；宏展开记录按调用位置、轮次、片段顺序稳定注册并维护父链；
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
