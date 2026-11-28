# 性能与延迟门禁

本章登记发布 workload 的测量面：哪些用例算数、报告哪些指标、性能参数只允许由什么数据调整。它是实现说明；工作负载语义本身以 [spec/](../spec/) 为准。

## 门禁原则

- 性能参数只由真实 workload、确定性参照模型与 release 镜像数据调整；不针对单一 benchmark 走特殊路径。
- 默认 `cargo nextest run --workspace` 不跑任何 bench；全部 workload 由 `cargo bench -p gugu-compiler --bench <name>`（`harness = false`）手工验收，输出人读指标。
- 指标单位固定：吞吐 `item/us` 或 `op/us`，时间 `µs`，镜像 `B`；每次运行必须同时报告规模（items/producers/rounds），单独的数字不算记录。

## 默认正确性套件

语言、标准库和协议用例按所检查的产物选择编译阶段：只校验词法/语法、类型诊断、冻结 HIR
或 `gugu doc` 输出的用例走 `Compiler::compile_sema_for_test`；只观察单态化闭包、GIR 或
前端分析结果的用例走 `Compiler::compile_frontend_for_test`；检查 LIR 或解释执行用户程序的
用例走 `Compiler::compile_to_lir`。这些入口与完整编译共享目标选择、内建制品与前置校验。
资源协议用例可直接解释构建期 artifact 中的生产 LIR（按目标进程内缓存一次），同组测试复用
`Harness` 与线性内存并在场景间重置状态；需要访问私有 `std.runtime.*` 的探针模块以
`std/runtime/fixtures/` 前缀追加到已物化 artifact 之后，避免为了单份探针重编全部标准库。
文档围栏按原登记逐块执行，普通围栏编译到已验证 LIR，带预期输出的围栏继续逐字节比较解释器的输出。

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
  不因此改变。`canonical::hash_domain` / `hash_derive_key` 复用线程局部编码缓冲，
  `QueryResult::new` 改用流式 BLAKE3 摘要，消除大型 payload 的二次分配与拷贝。
- `SourceSnapshot` 以 `Arc` 共享只读文本、行首表与摘要，内建源码快照克隆降为 O(1)；
  `TypeCheck` query 改用规范二进制编码并跳过未命中路径的二次反序列化。
- `GirBody` 与 `LIR::Body` 指纹按 world 缓存并复用物化解码阶段的结构校验，避免
  `placement`、`verify_world` 与 `x64::codegen` 重复序列化整表函数体；
  `gir::pass::cow` 对无 `CowSnapshot` 函数早退并用稠密 `Vec` 替代 `BTreeMap`；
  `gir::placement::region_decisions` 对无受管分配的函数跳过 `RegionFacts` 支配与边界分析。
- 过程内抽象分析固定点在块与轮次间复用 `AbstractState` 与分支暂存缓冲，采用回调式
  边展开消除每块/每边克隆与 `Vec` 分配；`sync_resolved` 仅在赋值与读取点按需触发；
  `narrow` 仅在 `widen` 实际将区间端点外推至 `±∞` 时执行，无外推循环的精确最小不动点
  直接跳过多余轮次。
- `lir::verify::provenance` 增加直接栈来源（`Origin::Stack` 与短 `Derived` 链）零分配
  快路径，对无栈槽函数跳过寿命固定点并按块分桶排序寿命事件；`lir::pass::poll` 成本
  传播改用稠密 `Vec`。
- `x64::codegen` 以借用视图零克隆计算 `FragmentPayload` 指纹，并对本次现算片段跳过重复
  自哈希；`x64::alloc::rewrite` 直接接管 `SelectedBlock` 所有权且不逐条克隆 `Inst`；
  `x64::layout::relax` 改为单次指令长度计算加纯整数前缀和松弛，不再在松弛循环中反复
  执行整函数汇编；`x64::metadata` 建立稠密站点索引、二分值查找、有序数组边界与惰性
  `LiveValues`，消除安全点与落地表构造中的二次方扫描。
- 构建与链接布局：`[profile.dev]` 与 `[profile.dev.build-override]` 保持 `opt-level = 3`、
  `debug-assertions = true`、`overflow-checks = true` 并关闭 DWARF 调试信息（`debug = 0`），
  workspace 统一 `sha2 = "0.11"` 消除隐式双份编译；`gugu-compiler/build.rs` 以受限线程池
  并行物化五目标并通过 `hard_link` 复用构建产物；14 个 `gugu-compiler` 集成回归合并为单一
  `tests/integration/` 二进制并关闭无单测库目标的 `[lib] test`。

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

### 构建、热路径与测试聚合实测记录

2026-11-27 在同一台 WSL2 宿主机（AMD Ryzen 5 5600 6C/12T、19 GiB RAM、`rustc 1.101.0-nightly`、
`mold 3.0.0`、`cargo-nextest 0.9.143`）上，对比本轮优化前后全量测试的构建产物、增量编译与默认门禁执行。
优化前默认 3 秒门禁因 330 项超时并触发 120 秒全局超时而失败，运行基线采用临时延长超时配置采集完整遍历；
优化后直接运行仓库默认 `.config/nextest.toml`（单项 3 秒、整轮 120 秒、零重试、`fail-fast = true`）全量通过，
所有断言、多目标覆盖、文档围栏与物化等价校验保持不变。

| 指标 | 优化前 | 优化后 | 变化 |
| --- | ---: | ---: | ---: |
| 测试二进制数量 | 29 个 | 14 个 | -51.7% |
| 测试二进制磁盘总占用 | 4,704.9 MiB | 260.8 MiB | -94.5%（18.0×） |
| 空增量构建（`--workspace --no-run`） | 0.62 s | 0.31 s | -50.0% |
| 单集成测试增量编译 / 进程树峰值 RSS | 1.54 s / 1.06 GiB | 0.58 s / 0.49 GiB | -62.3% / -53.8% |
| 叶 crate 增量（`gugu-compiler/src/lib.rs`） | 7.38 s / 7.13 GiB | 1.52 s / 1.90 GiB | -79.4% / -73.4% |
| 核心 crate 级联触碰（`gugu-sema/src/lib.rs`） | 149.08 s（965.21 s CPU） | 14.44 s（38.97 s CPU，增量命中）；全量重编 `gir`→`cli` 约 26–55 s | 显著缩短 |
| 默认 3 秒 / 120 秒门禁状态 | 失败（330 超时 / 120 s 截断） | 全部通过（1073/1073，最慢单项 2.38 s） | 门禁全绿 |
| 全量测试执行墙钟 | 154.56 s（延长超时采样） | 52.37 s（严格默认门禁） | -66.1%（2.95×） |
| 全量测试总 CPU（用户态 + 内核态） | 1,101.34 s（865.82 + 235.52 s） | 208.51 s（171.64 + 36.87 s） | -81.1%（5.28×） |
| 全量测试进程树峰值 RSS | 3,217.1 MiB（3.14 GiB） | 1,847.1 MiB（1.80 GiB） | -42.6% |
| 全量测试次缺页（minor page faults） | 27,414,187 | 11,999,073 | -56.2% |

基线活动测试数为 1,343 项、优化后为 1,073 项（两侧均保留原有 66 项跳过）：用例数减少来自将
多组同域单场景聚合到同进程多入口程序以消除跨进程重复解码与编译，未删减任何契约断言或目标覆盖。
上述数据为同机单次实测，不作为跨机器保证。

### CI 执行与缓存

`.github/workflows/ci.yml` 保留 `Rust checks`、`Rust lint`、mdBook 与
`Gugu executables`。`Rust checks` 与可执行程序验收均使用四平台原生矩阵：
Linux x86_64（`ubuntu-latest`）、Windows x86_64（`windows-2025`）、
Linux aarch64（`ubuntu-24.04-arm`）与 macOS aarch64（`macos-15`）。
矩阵不因某个平台失败而取消其它平台。`Rust checks` 直接由 nextest 构建并运行测试，不再先执行一次
`cargo build --workspace`；普通 CLI 镜像仍由可执行程序验收独立构建。
格式检查、常规 target 的严格 Clippy 检查和全特性测试的编译入口检查在 `Rust lint` 中执行，
与测试并行；`Rust lint` 和 mdBook 只在 Linux x86_64 执行。

`ci/e2e/run.py` 通过 Python 标准库调用编译器和生成程序，显式传入矩阵目标，
处理 Windows 的 `.exe` 后缀及 Unix 执行权限。四平台复用全部既有 fixtures，
标准输出按原始字节比较，预期输出文件固定以 LF 检出；Unicode 规范化数据仍按
分片启动独立进程并合并输出。单次编译限时 180 秒，单次执行限时 30 秒，
临时镜像和缓存随验收结束清理。执行器自身的判定测试使用进程替身，不运行真实子进程。

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
