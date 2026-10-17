# ADR-0011：内建块构建期物化与加载期重建

- 状态：已接受
- 日期：2026-10-17

## 背景

每次用户编译都要把整份内建源（`std/*`、`std/runtime/*`）重新走一遍前端：名称解析、comptime、类型检查、HIR 降级、GIR 构造、契约类型目录。真实程序的语义工作很小，绝大多数时间花在与用户源码无关的内建块上，固定成本约一秒量级，且与用户代码无关。

`GUGURT01` artifact 原本只携带 `source manifest` 与接口摘要，用户编译据此重新派生全部内建块。要让内建块的派生只做一次，必须回答三个问题：派生结果能不能跨进程复用；跨会话复用后编号还对不对；用户编译还能不能保住「内建块编号占前缀」这条不变式。

## 决策

内建块的派生结果在**构建期**物化进 `GUGURT01` artifact 的 `prebuilt` 段，用户编译在**加载期**按规范校验后重建内建块。跨单元只保存稳定键（`StableDefKey`、`DefPath`、源码逻辑路径 + 字节区间），会话局部编号一律在加载时重建——与 rustc 的 `rmeta`、Go 的 export data 采取同一路线。

物化的层与顺序：

1. `names`（名称解析）、`identities`（`DefRef` ↔ HIR `DefId`）、`early.constants`（comptime）、`checked.semantics`（类型检查）、`hir.module`（HIR 全表）、`gir.bodies`（generic GIR）、`record.types`（契约类型目录）；
2. 每层带表指纹：`(标识域摘要, 内容摘要)`；加载端先核对容器 schema 与层 revision，再重算摘要比对，最后跑结构 verifier；
3. 失败一律报 compiler invariant，**没有源码回退路径**——回退会让「物化表与源码树是否同源」这件事失去判据。

装载重建的规则：

- 文件表与模块表：物化源占前缀（artifact manifest 顺序），未物化的内建源（探针源）与用户模块依次追加；
- 定义编号：内建块的定义整段占前缀，用户定义按收集顺序追加；判定只看模块归属，不看是否被物化覆盖，构建期派生与用户编译因此同序；
- 类型与各侧表：降级分两段——内建块先完成签名、侧表与 body，用户定义整体追加；类型驻留表用片段预置，用户侧复用同一批 `TypeId`；
- owner 的 `input_fingerprint` 与诊断位置在当前会话重算/重建，会话局部编号不进入内容指纹；
- `Span` 的 `SourceTableId` 是纯内存标记，不进规范编码，否则物化字节与现算字节永远不会相等。

## 后果

- 用户编译跳过内建块的 comptime、类型检查、HIR 降级与 GIR 构造；`materialized_block_skips_builtin_derivation` 用 query cell 统计钉住「内建 owner 从不注册 `BuildGenericGir`」。
- 两条加载路径必须给出同一份世界：`numbering_stability` 以真实语料逐项比对编号无关指纹与诊断集合，任何不忠实的安装都会立刻暴露。
- 物化表是编译器内部格式，随 `canonical` revision 与层 revision 一起失效；派生失败只影响构建期，不影响用户源码语义。
- 物化边界必须与用户无关：内建块不能依赖用户 cfg、用户入口或用户定义。构建期派生时 `entry` 为空，任何把用户诱导类型插进内建块中段的写法都会破坏编号前缀，必须按两段式降级消除。
