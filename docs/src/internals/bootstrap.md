# Compiler bootstrap 与 action graph

本章固定官方 compiler 从工程入口逐步接入完整编译管线时的 bootstrap 边界。它是实现说明，不增加 Gugu 语言语义，也不构成跨 compiler identity 的 ABI。公开行为以 [`spec/`](../spec/overview.md) 为准；后续阶段必须在本章登记的归属层继续扩展，不能另起一套语义等价实现。

## 阶段 1 交付边界

阶段 1 建立 Rust compiler bootstrap、单一 `gugu` 入口、目标描述、确定性诊断、bootstrap 前端、稠密 IR、后端 image plan 和 Gugu runtime 源资源登记。此阶段可检查空 package、内存中的单文件入口和文件系统单文件入口，并为合法的 `fn main() { ... }` 生成端到端 action graph。

阶段 1 的 `ImagePlan` 是 compiler 内存中的验证结果，不是 ELF、PE、静态库或共享库。`emit-image` action 在本阶段保持 `skipped`，因此成功检查不会写出伪造的目标镜像；任一前置 action 失败时，所有后续 action 都会被跳过，结果中不会留下镜像计划。真正的 machine encoder、镜像 writer 和 rt0 写出分别属于阶段 52、56、57。

## 工程边界

当前实现的模块树如下：

```text
crates/
├── gugu-cli/
│   └── src/main.rs                 单一 gugu 进程入口与 bootstrap 命令
└── gugu-compiler/
    ├── src/lib.rs                  CompileRequest 与 action 编排
    ├── src/action.rs               稠密 action graph 与状态迁移
    ├── src/diagnostics.rs          稳定代码、源码范围与排序
    ├── src/target.rs               目标注册表与 TargetDescriptor
    ├── src/frontend.rs             阶段 1 的入口结构检查
    ├── src/ir.rs                   main -> ReturnUnit 的 bootstrap IR
    ├── src/backend.rs              目标相关的内存 image plan 输入
    ├── src/runtime.rs              Gugu 源树、rt0 和 intrinsic 登记
    └── resources/
        ├── std/prelude.gg          标准库 Gugu 源单元
        └── runtime/core.gg         runtime Gugu 源单元
```

模块职责是单向的：CLI 只构造请求和渲染结果；compiler 负责阶段编排；frontend 不创建机器码；IR 不读取源码文本；backend 只消费 IR 与目标描述；runtime 模块只提供 compiler 携带的 Gugu 源资源和边界登记。阶段 1 不在 Rust 中实现 Gugu runtime 的调度、GC、资源释放或标准库语义。

`gugu-compiler` 使用 `#![forbid(unsafe_code)]`。平台入口、系统调用、原子、换栈、safepoint、GC 写屏障和外部函数交接在这里仅以 `IntrinsicBoundary` 登记，实际 machine intrinsic 必须在后续 backend/runtime 阶段按相应内部契约接入。

## 目标描述与 rt0 边界

`TargetName` 只接受规范登记的 `x86_64-linux` 和 `x86_64-windows`。每个目标由不可变 `TargetDescriptor` 提供架构、操作系统、对象格式、指针宽度和 rt0 类型：

| 目标 | 对象格式 | 指针宽度 | rt0 边界 |
|---|---|---:|---|
| `x86_64-linux` | ELF64 | 64 | Linux syscall |
| `x86_64-windows` | PE32+ | 64 | Windows 薄 IAT |

rt0 不是普通 Gugu 函数。Linux 入口和 Windows 薄导入路径由后端与平台 runtime 负责；`RuntimeResources` 只把这项边界附加到 image plan，不实现宿主启动、分配、调度或报告逻辑。这样可以使目标描述进入编译结果，同时保持公开的 rt0 启动契约由 [`运行时规范`](../spec/runtime.md#rt0-与启动) 和 [`平台 ABI`](../spec/platform-abi.md#入口重定位与-tls)唯一规定。

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

节点状态只有 `pending`、`complete`、`skipped` 和 `failed`。成功路径的 `validate-image` 只验证内存计划；`emit-image` 在阶段 1 为 `skipped`。失败路径从第一个失败节点开始把下游节点标为 `skipped`，编排器不执行降级编译、不调用外部 assembler/linker，也不写出部分产物。

输入有三种明确形态：

- `empty_package`：没有用户源文件，前端和 IR 成功完成，但没有 executable entry，后端之后的节点跳过，不产生 image plan；
- `single_file`：调用者提供逻辑路径和内存源码，适合确定性测试与编辑器；
- `single_file_path`：compiler 在 `load-sources` action 内读取指定 `.gg` 文件，读取失败形成 `E0001` 并停止后续 action。

阶段 1 前端只验证 NUL、`u32` 源长度、`fn main()` 入口和函数体括号，并把合法入口降低为单个 `ReturnUnit`。这不是完整 lexer/parser/type checker；阶段 7、8、12–20 必须替换该实现并保持 action graph 的错误传播契约。因而本阶段的成功只表示 bootstrap 计划合法，不表示已经满足完整语言规范或可以运行目标程序。

## runtime 源资源与实现归属

`RuntimeResources::builtin()` 返回 compiler 构建时嵌入的 Gugu 源文件登记：

| 逻辑路径 | 角色 | 阶段 1 的责任 |
|---|---|---|
| `std/prelude.gg` | 标准库源单元 | 证明标准库输入进入 action graph |
| `runtime/core.gg` | runtime 源单元 | 证明 runtime 输入进入 action graph |

这两个源单元是源树登记输入，不是 Rust runtime 的替代实现。Rust compiler 可以拥有读取、验证和编排逻辑；Gugu runtime 的可观察语义必须最终来自镜像内的 Gugu runtime、rt0 和登记的 machine intrinsic。任何新增 runtime 能力都必须同时说明其 Gugu 源实现、必要 intrinsic 和 compiler lowering，不能在 Rust 中复制一份正常执行路径。

## 公开规范归属表

下表给出每条公开规范的实现归属。阶段 1 只交付表中标为“已建立”的工程边界；“后续阶段”表示对应模块尚未实现，不能把当前 bootstrap 检查误认为该章节已经完成。

| 公开规范 | 主要实现归属 | 阶段 1 状态 | 完整实现阶段 |
|---|---|---|---:|
| `overview` | `action`、`target`、`runtime` | 已建立闭世界/目标边界 | 01–79 |
| `lexical` | `frontend` lexer | 已登记前端入口 | 07 |
| `format-style` | `gugu-cli` fmt 与 formatter | CLI 未接入 | 09 |
| `syntax` | `frontend` parser | 已登记前端入口 | 08 |
| `types` | type arena、type checker | 未实现 | 12–20 |
| `declarations` | module tree、definition collector | 未实现 | 04、10、13 |
| `program-model` | action、backend、runtime | 已建立 plan/不写部分镜像契约 | 01、24、52–57 |
| `packages-builds` | project/workspace resolver | 未实现 | 04–06、72–73 |
| `publishing-ecosystem` | registry、archive、signature | 未实现 | 74 |
| `toolchain-cli` | `gugu-cli` 与 action orchestrator | 已建立单一入口 | 01；完整为 02、73 |
| `expressions` | frontend、HIR、GIR | 未实现 | 14、20、26 |
| `patterns` | pattern checker、HIR matrix | 未实现 | 15、20 |
| `functions` | capture、async、HIR/GIR | 未实现 | 16、20、26 |
| `traits` | trait solver、impl selection | 未实现 | 17、18、20 |
| `passing` | value/resource lowering | 未实现 | 27 |
| `memory` | placement、resource runtime、GC | 仅登记 runtime 边界 | 27、30–51 |
| `concurrency` | scheduler、channel、sync runtime | 仅登记 intrinsic 边界 | 35–37 |
| `comptime` | evaluator、source expansion、analysis | 未实现 | 21–23 |
| `unsafe` | safety checker、FFI/asm backend | 仅登记 intrinsic 边界 | 19、58 |
| `platform-abi` | `target`、x86 backend、image writer | 已建立两个目标 descriptor | 52–58 |
| `runtime` | Gugu runtime、rt0、报告路径 | 已建立资源与 rt0 边界 | 33–51、56–58 |
| `standard-library` | `runtime` Gugu 源树与 std modules | 已建立源树登记 | 59–68 |
| `testing` | test collector、harness、CLI | 未实现 | 69–70 |

内部契约也沿同一边界扩展：[`AST/HIR`](ast-hir.md) 消费阶段 1 frontend 的后继实现，[`GIR/LIR`](gir-lir.md) 消费后续 HIR，[`后端`](backend.md) 负责从合法 LIR 到 machine code，[`调度器`](scheduler.md) 和 [`GC 元数据`](gc-metadata.md) 负责 runtime 语义。当前阶段不会为这些后续模块建立平行的占位语义路径。

## 验收契约

阶段 1 的确定性测试覆盖以下可观察结果：

- 空 package 的所有 graph 节点都离开 `pending`，没有 image plan；
- `fn main() {}` 完成 frontend、IR、backend、runtime 和 image validation，并留下带目标、入口、runtime 源单元数量和 rt0 的内存计划；
- malformed source 产生稳定诊断，frontend 为 `failed`，下游为 `skipped`，没有 image plan；
- 两个登记目标使用不同 rt0/object format 边界，未登记目标不能解析；
- runtime 源资源的逻辑路径、角色和 intrinsic 登记来自同一 `RuntimeResources`，不依赖目录枚举或线程完成顺序。

最终镜像写出、Gugu 源 runtime 自举、完整 parser/type checker、GC、scheduler 和双目标 machine code 都不属于本阶段验收；它们必须在路线图后续阶段以各自规范和测试完成。
