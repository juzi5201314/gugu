# 性能与延迟门禁

本章登记发布 workload 的测量面：哪些用例算数、报告哪些指标、性能参数只允许由什么数据调整。它是实现说明；工作负载语义本身以 [spec/](../spec/) 为准。

## 门禁原则

- 性能参数只由真实 workload、确定性参照模型与 release 镜像数据调整；不针对单一 benchmark 走特殊路径。
- 默认 `cargo nextest run --workspace` 不跑任何 bench；全部 workload 由 `cargo bench -p gugu-compiler --bench <name>`（`harness = false`）手工验收，输出人读指标。
- 指标单位固定：吞吐 `item/us` 或 `op/us`，时间 `µs`，镜像 `B`；每次运行必须同时报告规模（items/producers/rounds），单独的数字不算记录。

## 默认正确性套件

编译器自身的默认正确性套件在进程内验证语言契约。资源协议用例直接解释构建期
artifact 中的生产 LIR，并为每个场景创建独立内存与平台钩子；用户源码到镜像、
栈图与目标 ABI 的集成检查仍单独执行完整编译。教程围栏逐块验证，较重章节按
围栏拆分，不能通过跳过示例或删减协议断言缩短默认套件。

### CI 执行与缓存

`.github/workflows/ci.yml` 保留 `Rust checks`、`Rust lint`、mdBook 与 Linux
可执行程序验收。`Rust checks` 直接由 nextest 构建并运行测试，不再先执行一次
`cargo build --workspace`；普通 CLI 镜像仍由可执行程序验收独立构建。
格式检查与 `cargo clippy --workspace --all-targets -- -D warnings` 在
`Rust lint` 中执行，与测试并行，不删减目标或放宽诊断。

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
| `pipeline_baseline` | 冷流水线地板、最小用户源、`std.collections`/`std.io` 实例化 | 每用例 elapsed µs、action 指纹 |
| `coroutine_context` | context switch 机器片段往返 | switch/restore 计数值 |
| `morestack_entry`、`stackmap_walk` | 栈增长入口、栈图遍历 | 片段正确性与代价 |
| `x64_encoding`、`x64_select`、`x64_frame` | 编码/选指/帧布局吞吐 | 每千条 µs |
| `linux_pie`、`resource_release` 等 | 镜像布局、release exactly-once | 校验不变量 + 吞吐 |

## routing profile 代价确认

`workload_gate` 用 `gugu-runtime-world` 的确定性 `RawWorld` 同一份实现跑 direct 与 radix 两种 routing profile：direct 不分配 bucket、remote hop 恒为 0；radix 固定 `2^k` bucket 分层、记录 `remote_return_hops` 与 staging flush 次数。两种模式都必须满足 exactly-once 消费（`consumed == published`）且 pending bytes 清零，吞吐差异即 profile 的实际代价，不允许用 profile 切换宣称优化。

GC pause、remark/evacuation、cache hit 与 RSS 的记录随各归口 bench 扩展；未登记的测量不进入调参依据。
