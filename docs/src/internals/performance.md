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
