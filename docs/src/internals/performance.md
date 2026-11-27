# 性能与延迟门禁

本章登记发布 workload 的测量面：哪些用例算数、报告哪些指标、性能参数只允许由什么数据调整。它是实现说明；工作负载语义本身以 [spec/](../spec/) 为准。

## 门禁原则

- 性能参数只由真实 workload、确定性参照模型与 release 镜像数据调整；不针对单一 benchmark 走特殊路径。
- 默认 `cargo nextest run --workspace` 不跑任何 bench；全部 workload 由 `cargo bench -p gugu-compiler --bench <name>`（`harness = false`）手工验收，输出人读指标。
- 指标单位固定：吞吐 `item/us` 或 `op/us`，时间 `µs`，镜像 `B`；每次运行必须同时报告规模（items/producers/rounds），单独的数字不算记录。

## 默认正确性套件

语言、标准库和协议用例按所检查的产物选择编译阶段。只观察诊断、GIR 或 LIR 的用例，
以及解释执行用户程序的用例，走 `Compiler::compile_to_lir`。该入口和完整编译共享目标
选择、内建制品、前端及 GIR/LIR 校验。资源协议用例也可直接解释构建期 artifact 中的
生产 LIR，并为每个场景创建独立内存与平台钩子。文档围栏按原登记逐块执行，普通围栏
编译到已验证 LIR，带预期输出的围栏继续逐字节比较解释器的输出。

检查 runtime 契约、物理栈图、机器码、目标 ABI、重定位或镜像的用例使用对应的后端产物。
概述示例检查多文件项目完整编译产出的 ELF，A64 镜像用例验证目标装配。CI 的格式化与集合用例还执行
实际生成的镜像，覆盖从 Gugu 源码到宿主运行的路径。

根 `clippy.toml` 登记了生产 `Compiler::compile`。CI 对 workspace 所有常规 target，以及
启用全部特性后的测试 target，运行 Clippy `disallowed_methods` 检查。测试直接调用生产
入口会报错。完整编译用例使用 `compile_full_for_test`，断言负责说明所需的后端契约。

### 本地全量测试

本地优化以完整正确性套件的真实编译和执行为测量对象，分别记录重新编译耗时、
已有二进制的执行耗时、用户态/内核态 CPU 时间与峰值内存。`time -v` 的最大 RSS
是单个进程的高水位，不能当作所有并发测试的总内存；堆分配次数与存活峰值另用
heaptrack 记录。权限不允许 `perf record` 时用 gprofng 的用户态时间采样和
Callgrind 指令计数定位调用链，不能把插桩后的耗时当作正常执行延迟。
Heaptrack 采集使用 `--record-only`，离线分析显式调用 `heaptrack_print -f <报告>`，
避免工具自动启动 GUI；采样报告与临时配置放在 `/tmp`。

重复工作在生产路径归属层消除，不给测试增加另一套编译实现：

- runtime glue 绑定表只构造一次 mangled 符号集合，点查不重新哈希、格式化全部绑定；
  内部摘要的十六进制字符直接编码，既有符号字节与绑定指纹不变。
- runtime artifact 的 opaque 表与函数层字节用 `Bytes` 切片共享输入缓冲；静态嵌入不复制，
  文件输入收编 `Vec` 所有权。对象缓存只解码一次，语义失败仍隔离坏对象；容器布局、
  schema、摘要、UTF-8、目标及安装端的内容校验不变。
- 内建前缀缓存只保存按目标、实际物化表内容身份与层 revision 验证过的不可变解码基底，
  最多登记八份输入缓冲身份；持有 `Bytes` 所有权并比对元数据、缓冲位置与长度后，热命中
  不重算大表摘要。新缓冲先计算实际内容身份，相同目标与内容可以共享解码 cell；不同制品
  的首次解码不占用登记表锁。命中复用语义/GIR 表与结构验证结果。每次安装仍按当前
  `SourceMap` 重建名称/导入记录对应的 `Span` overlay 并校验源、模块前缀，记录本身共享；
  用户派生数据留在会话侧追加，编号与诊断位置不跨会话复用。稠密表保持共享前缀，owner
  的输入身份单独重建，不能在追加时展平整表。
- 单态化上下文按稠密 `DefId` 索引首次 capture 的类型参数；runtime glue 源定义按
  登记表槽位惰性索引，缺失也只记忆化到当前不可变 world，不跨编译复用。
- GIR/LIR 的后继与边遍历借用原始存储，不在固定点、可达性或 verifier 循环里构造
  临时 `Vec`；保留顺序、重复边、unwind/cancelled 与 switch 默认分支。poll 环检测
  的每个 DFS 栈帧持有迭代器，不随游标推进重复扫描、过滤和分配后继集合；编辑态
  block 状态按 arena ID 索引，删除洞保持不可达，不为每条边线性扫描活跃 block 表。
  region 候选通过支配检查后才构造活跃性，不为无候选函数执行无用分析。
- LIR 活跃位图用共享不可变快照和写时复制，零集合与未变化的块状态不重复分配；
  本块生成的 use 集合只计算一次，实际位变化后才复制原始字长，不按扩容策略浪费大函数内存。
  操作 verifier 的短参数/结果列表和重写时的短边快照使用内联缓冲，较大列表仍扩容。
  更新不能污染已发布快照，不限制 arity、不截断位图，也不改变尾部 padding 的根判定。
- 规范编码的字段与元素短路径允许跨 crate 内联；编码布局、长度校验和 GBC1 revision
  不因此改变。

性能采样若需要完整遍历超时的基线，可在 `/tmp` 使用只延长截止时间的临时 nextest
配置，保持测试集合、线程数和调度权重一致。它不修改仓库默认门禁，也不代替默认
配置下的正确性验收；比较前后的执行时间必须使用同一份采样配置。

### 本地实测记录

本次在 WSL2、12 个逻辑 CPU、`rustc 1.101.0-nightly (8006091eb)` 下，对
`7a05c0a` 的隔离快照与当前实现分别构建，随后测量已有二进制的执行；两侧使用相同
O3 开发配置、测试选择、并发和权重。完整遍历使用上述临时截止时间配置，不计入
Rust 重新编译，也不将插桩耗时混进正常延迟。

| 指标 | 优化前 | 优化后 | 降低 |
| --- | ---: | ---: | ---: |
| 完整套件执行墙钟 | 125.86 s | 101.51 s | 19.3% |
| 完整套件用户态与内核态 CPU 时间合计 | 961.83 s | 753.01 s | 21.7% |
| 完整套件最大单进程 RSS | 518.42 MiB | 512.97 MiB | 1.1% |
| 双目标 aggregate ABI 用例独立执行 | 2.67 s | 2.17 s | 18.7% |
| 同一 ABI 用例最大 RSS | 455.51 MiB | 369.17 MiB | 19.0% |
| 同一 ABI 用例堆分配次数（heaptrack） | 15,404,673 | 7,474,541 | 51.5% |
| 同一 ABI 用例临时分配次数（heaptrack） | 6,288,632 | 1,728,591 | 72.5% |
| 同一 ABI 用例峰值堆（heaptrack） | 345.63 MB | 263.63 MB | 23.7% |

基线 1324 项、优化后 1330 项全部通过；两侧保留原有的 66 项跳过，新增 6 项覆盖
符号编码、CFG 边顺序与分支、位图快照隔离及制品错误隔离。收益来自实际计算与分配减少，
不是改变线程数、权重或覆盖面。RSS 是单进程高水位，不代表并发总内存同比下降。
这是同机单次 A/B，不是跨机器保证，也没有证明首次 Rust 编译耗时降低。

原默认配置的单项 3 秒截止时间在本机仍有超时；减少到 8 个测试槽也未消除该问题，
因此没有据此调整默认调度，也不把完整采样通过写成默认门禁通过。

### 内建共享基底实测

2026-11-27 以 `d6c18cb2ba54b1ac5793627054bb55605830bc0f` 的源码快照为优化前，
与共享基底实现使用逐字节相同的 `pipeline_baseline` harness；两侧均为 `--profile test`
的 O3 配置和 `x86_64-linux` 目标。每个用例单独启动进程、连续六轮，每轮新建 `Compiler`，
不沿用会话 query。计时覆盖完整编译，不包含 Rust 构建；首轮单列，热会话取后五轮中位数。

| 用例 | 首轮前 → 后（µs） | 热会话前 → 后（µs） | 热会话降低 | 最大 RSS 前 → 后（KiB） |
| --- | ---: | ---: | ---: | ---: |
| `minimal-main`（13 B） | 827030 → 812853 | 680979 → 602297 | 11.6% | 316224 → 302180 |
| `std-collections-io`（273 B） | 808268 → 759996 | 681156 → 592068 | 13.1% | 316076 → 302480 |

两侧每轮的完整 LIR 指纹在各自进程内保持相同，跨实现报告的指纹前缀也一致；镜像均为
1880064 B。标准库用例包含集合类型、泛型调用和导入检查，入口仍是空 `main`，不代表
大规模集合程序的后端成本。首轮只有一个样本，不据此声称稳定的冷启动收益；RSS 是每个
六轮进程的高水位。共享基底只在进程内复用，首次解码与会话绑定工作仍然存在。

复现时分别设置 `GUGU_PIPELINE_CASE=minimal-main` 或 `GUGU_PIPELINE_CASE=std-collections-io`、
`GUGU_PIPELINE_REPEATS=6`，运行 `cargo bench -p gugu-compiler --profile test --bench pipeline_baseline`；
内存测量用 `/usr/bin/time` 包住已构建的 bench 二进制，不把 Cargo/Rust 编译计入。

### CI 执行与缓存

`.github/workflows/ci.yml` 保留 `Rust checks`、`Rust lint`、mdBook 与 Linux
可执行程序验收。`Rust checks` 直接由 nextest 构建并运行测试，不再先执行一次
`cargo build --workspace`；普通 CLI 镜像仍由可执行程序验收独立构建。
格式检查、常规 target 的严格 Clippy 检查和全特性测试的编译入口检查在 `Rust lint` 中执行，
与测试并行。

变更分流只把 `docs/` 和 `book.toml` 视为纯文档输入。PR 按整个 PR 相对目标分支的
差异判断，`master` push 按本次 push 前后的差异判断；混合变更、工作流、Cargo
配置、源码、资源和 fixtures 变更都执行完整 Rust 测试、lint 与可执行程序验收。
纯文档变更仍执行 mdBook、`docs_links` 和 `filesystem_tests::docs_examples::`
下的全部检查，包括围栏登记完整性与代码示例编译、输出验证，不能只构建书籍。

Rust job 使用 `Swatinem/rust-cache` 缓存 Cargo 依赖，使用 sccache 的 GitHub Actions
后端按编译输入复用可缓存的编译结果，`CARGO_INCREMENTAL=0`；不依赖跨 checkout 的
源码 mtime 复用 workspace target，也不把缓存命中当作跳过当前检查的依据。
优化不降低 Rust 优化等级、不修改 nextest 的并发权重、超时或测试集合。

耗时比较必须区分冷缓存与热缓存，并分别记录测试编译、测试执行、lint 和 E2E
的耗时；并行 lint 会增加同时运行的 runner 数量，不能把关键路径缩短等同于
总计算量等比例下降。缓存失效与未命中仍执行同一套真实构建和检查。

## 已登记 workload

| bench | 覆盖面 | 指标 |
| --- | --- | --- |
| `workload_gate` | 双目标端到端编译（channel/select、I/O、FFI 混合源）、owner return 的 direct 与 radix routing profile、分配-归还 pacing | 编译 µs、镜像 B、action key、发布/消费 item/µs、remote hop、单次发布尾 µs |
| `pipeline_baseline` | 冷流水线地板、最小用户源、`std.collections`/`std.io` 类型与导入检查 | 每轮 elapsed µs、输入 B、LIR 指纹、镜像 B；区分首次与热进程的新会话 |
| `coroutine_context` | context switch 机器片段往返 | switch/restore 计数值 |
| `morestack_entry`、`stackmap_walk` | 栈增长入口、栈图遍历 | 片段正确性与代价 |
| `x64_encoding`、`x64_select`、`x64_frame` | 编码/选指/帧布局吞吐 | 每千条 µs |
| `linux_pie`、`resource_release` 等 | 镜像布局、release exactly-once | 校验不变量 + 吞吐 |

## routing profile 代价确认

`workload_gate` 用 `gugu-runtime-world` 的确定性 `RawWorld` 同一份实现跑 direct 与 radix 两种 routing profile：direct 不分配 bucket、remote hop 恒为 0；radix 固定 `2^k` bucket 分层、记录 `remote_return_hops` 与 staging flush 次数。两种模式都必须满足 exactly-once 消费（`consumed == published`）且 pending bytes 清零，吞吐差异即 profile 的实际代价，不允许用 profile 切换宣称优化。

GC pause、remark/evacuation、cache hit 与 RSS 的记录随各归口 bench 扩展；未登记的测量不进入调参依据。
