# Compiler bootstrap 与 action graph

本章固定官方 compiler 从工程入口逐步接入完整编译管线时的 bootstrap 边界。它是实现说明，不增加 Gugu 语言语义，也不构成跨 compiler identity 的 ABI。公开行为以 [`spec/`](../spec/overview.md) 为准；后续阶段必须在本章登记的归属层继续扩展，不能另起一套语义等价实现。

## 阶段 1 交付边界

阶段 1 建立 Rust compiler bootstrap、单一 `gugu` 入口、目标描述、确定性诊断、bootstrap 前端、稠密 IR、后端 image plan 和 Gugu runtime 源资源登记。此阶段可检查空 package、内存中的单文件入口和文件系统单文件入口，并为合法的 `fn main() { ... }` 生成端到端 action graph。

阶段 1 的 `ImagePlan` 是 compiler 内存中的验证结果，不是 ELF、PE、静态库或共享库。`emit-image` action 在本阶段保持 `skipped`，因此成功检查不会写出伪造的目标镜像；任一前置 action 失败时，所有后续 action 都会被跳过，结果中不会留下镜像计划。真正的 machine encoder、镜像 writer 和 rt0 写出分别属于阶段 52、56、57。

## 阶段 2 交付边界

阶段 2 将 `gugu` 作为唯一 CLI 入口：根级全局参数可在子命令前后解析，配置按内置默认、用户配置、当前 workspace 的 `.gugu/config.toml`、`--config`、环境变量、命令行的顺序合并，后层覆盖前层。`--frozen` 在解析结果中同时设置 `offline` 与 `locked`。

规范表中的 `new`、`init`、`build`、`check`、`run`、`test`、`bench`、`fmt`、`doc`、`clean`、`add`、`remove`、`update`、`tree`、`vendor`、`package`、`publish`、`yank`、`login`、`cache`、`explain`、`version` 和 `help` 均已登记。阶段 2 只有 `build`、`check`、`version` 和 `help` 接入真实 action；其它已登记命令返回统一 `cli-error`，不会调用 compiler。

`text` 保留人读的 action/诊断/最终结果；`json` 为 NDJSON 事件信封，bootstrap 的构建事件顺序固定为 `build-start`、诊断、`build-finish`；`json-diagnostic-short` 只发布诊断事件。NDJSON 对源码路径使用逻辑相对路径，对工作区外路径使用 `<external>/文件名`，并清理凭据键值。

## 阶段 3 交付边界

阶段 3 在 `source` 模块落地源码快照与 Span 系统：`load-sources` 现在把每个输入固化为不可变 `SourceSnapshot`——UTF-8 校验、BOM 拒绝、`u32` 长度上限、BLAKE3-256 内容摘要与规范行首表。逻辑路径按词法归一：解析 `.` 与 `..`、拒绝绝对路径、反斜杠与越界回溯，保证相同输入在不同工作目录与换行环境下产生相同 span。

`Span` 携带源码文件 ID、逻辑路径、半开字节范围、行/列与 `ExpansionId`；行列从行首表二分推导，按 UTF-8 字节计数。`SourceMap` 按逻辑路径排序分配稠密文件 ID、拒绝重复路径，并为阶段 22 的源码宏预留确定性展开注册：`ExpansionRecord` 记录父展开、宏调用与定义位置、生成源码哈希与片段类别，注册顺序按调用位置、轮次与片段顺序稳定。诊断新增 `E0004`~`E0007`（非法 UTF-8、BOM、非法逻辑路径、span 越界），全部诊断按路径、偏移、级别、代码稳定排序。

## 阶段 4 交付边界

阶段 4 在 `project` 模块落地清单、workspace 与 target 发现，并把 CLI 的 `build`/`check` 无文件参数路径切换为项目模式。清单发现从当前目录向父目录查找最近 `gugu.toml`；解析使用 serde 严格 schema，未知核心字段、缺失 `[package].name`、保留 package 名 `std` 与保留依赖别名 `std` 都是编译前错误。

workspace 成员按 `members` glob 展开并扣除 `exclude`，glob 展开按规范相对路径排序、同一路径只算一个成员；根清单可同时是根 package。package 选择遵循规范：显式 `-p` 按规范名或短名唯一匹配，`--workspace` 覆盖默认选择，成员目录启动时构建当前 package，workspace 根启动时依次使用 `default-members`、根 package 或全部成员。

target 自动发现覆盖 `src/lib.gg`、`src/main.gg`、`src/bin/`、`tests/`、`benches/`、`examples/` 与 package 根 `build.gg` 的文件与目录形式，`auto-*` 开关与显式 target 表按清单规则生效；`foo.gg` 与 `foo/mod.gg` 同时存在、同种类重名 target、入口越过 package 根均在编译前失败。lib 的默认名是 package 短名把 `-` 换成 `_`。项目模式下每个选中 target 以 `project_entry` 进入同一 action graph：bin 类入口要求合法 main，lib/test/harness 类入口只做源码快照检查。单文件模式（`gugu build <file.gg>`）拒绝 `-p`、`--workspace`、`--features`、`--lib`、`--bin`、`--test`、`--bench`、`--example`、`--all-targets`，以退出码 2 失败。依赖解析、锁图与 SemVer 仍属于阶段 5、6。

## 工程边界

当前实现的模块树如下：

```text
crates/
├── gugu-cli/
│   └── src/main.rs                 单一 gugu 入口、参数解析、输出、项目发现与 bootstrap 命令
└── gugu-compiler/
    ├── src/lib.rs                  CompileRequest 与 action 编排
    ├── src/action.rs               稠密 action graph 与状态迁移
    ├── src/diagnostics.rs          稳定代码、源码范围与排序
    ├── src/source.rs               源码快照、Span、行首表与展开记录
    ├── src/project.rs              清单、workspace 与 target 发现
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

输入形态与阶段扩展如下：

- `empty_package`：没有用户源文件，前端和 IR 成功完成，但没有 executable entry，后端之后的节点跳过，不产生 image plan；
- `single_file`：调用者提供逻辑路径和内存源码，适合确定性测试与编辑器；
- `single_file_path`：compiler 在 `load-sources` action 内读取指定 `.gg` 文件，逻辑路径按输入路径推导；读取或快照失败形成 `E0001`~`E0007` 并停止后续 action；
- `project_entry`（阶段 4）：CLI 从清单发现的 target 入口，逻辑路径由 package root 推导，与工作目录无关；bin/example 与 `harness = false` 的 bench 要求合法 main，lib/test 与默认 bench 走库检查，不产生 executable entry。

阶段 1 前端只验证 NUL、`u32` 源长度、`fn main()` 入口和函数体括号，并把合法入口降低为单个 `ReturnUnit`。这不是完整 lexer/parser/type checker；阶段 7、8、12–20 必须替换该实现并保持 action graph 的错误传播契约。因而本阶段的成功只表示 bootstrap 计划合法，不表示已经满足完整语言规范或可以运行目标程序。

## runtime 源资源与实现归属

`RuntimeResources::builtin()` 返回 compiler 构建时嵌入的 Gugu 源文件登记：

| 逻辑路径 | 角色 | 阶段 1 的责任 |
|---|---|---|
| `std/prelude.gg` | 标准库源单元 | 证明标准库输入进入 action graph |
| `runtime/core.gg` | runtime 源单元 | 证明 runtime 输入进入 action graph |

这两个源单元是源树登记输入，不是 Rust runtime 的替代实现。Rust compiler 可以拥有读取、验证和编排逻辑；Gugu runtime 的可观察语义必须最终来自镜像内的 Gugu runtime、rt0 和登记的 machine intrinsic。任何新增 runtime 能力都必须同时说明其 Gugu 源实现、必要 intrinsic 和 compiler lowering，不能在 Rust 中复制一份正常执行路径。

## 公开规范归属表

下表给出每条公开规范的实现归属。状态列描述截至阶段 4 已交付的边界；未落地部分不能把当前 bootstrap 检查误认为该章节已经完成。

| 公开规范 | 主要实现归属 | 当前状态 | 完整实现阶段 |
|---|---|---|---:|
| `overview` | `action`、`target`、`runtime` | 已建立闭世界/目标边界 | 01–79 |
| `lexical` | `source` 快照、`frontend` lexer | 快照/BOM/UTF-8/逻辑路径已落地；lexer 未实现 | 03、07 |
| `format-style` | `gugu-cli` fmt 与 formatter | CLI 未接入 | 09 |
| `syntax` | `frontend` parser | 已登记前端入口 | 08 |
| `types` | type arena、type checker | 未实现 | 12–20 |
| `declarations` | module tree、definition collector | 清单层模块布局已落地；模块树未实现 | 04、10、13 |
| `program-model` | action、backend、runtime | 已建立 plan/不写部分镜像契约 | 01、24、52–57 |
| `packages-builds` | `project` 清单/workspace/target 发现 | 清单发现、workspace、target 自动发现已落地；依赖解析未实现 | 04–06、72–73 |
| `publishing-ecosystem` | registry、archive、signature | 未实现 | 74 |
| `toolchain-cli` | `gugu-cli` 与 action orchestrator | 已建立单一入口与项目/单文件模式 | 01–04；完整为 73 |
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

阶段 1–4 的确定性测试覆盖以下可观察结果：

- 空 package 的所有 graph 节点都离开 `pending`，没有 image plan；
- `fn main() {}` 完成 frontend、IR、backend、runtime 和 image validation，并留下带目标、入口、runtime 源单元数量和 rt0 的内存计划；
- malformed source 产生稳定诊断，frontend 为 `failed`，下游为 `skipped`，没有 image plan；
- 两个登记目标使用不同 rt0/object format 边界，未登记目标不能解析；

阶段 3、4 的确定性测试补充覆盖：

- 快照拒绝 BOM、非法 UTF-8（带精确字节偏移）与超长输入；行首表对 LF/CRLF/CR 混合输入给出确定行列映射；逻辑路径词法归一并拒绝绝对路径、反斜杠与越界回溯；
- `SourceMap` 按逻辑路径排序分配稠密 ID，重复路径拒绝；span 半开范围、未知文件/展开 ID 均有稳定错误；宏展开记录按调用位置、轮次、片段顺序稳定注册并维护父链；
- BOM 输入使 `load-sources` 失败且诊断携带 `E0005`，下游 action 全部跳过；
- 单 package、虚拟 workspace、根 package workspace、成员目录启动的 package/target 选择与规范一致；glob 展开排除 `exclude`，`default-members` 只在根启动时生效；
- 保留名 `std`（package 名与依赖别名）、未知核心字段、`foo.gg` 与 `foo/mod.gg` 冲突、target 重名、入口越过 package 根均在编译前失败；
- 单文件模式拒绝全部项目选择参数并以退出码 2 失败。

最终镜像写出、Gugu 源 runtime 自举、完整 parser/type checker、GC、scheduler 和双目标 machine code 都不属于本阶段验收；它们必须在路线图后续阶段以各自规范和测试完成。
