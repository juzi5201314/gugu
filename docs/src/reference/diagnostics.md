# 诊断码

稳定诊断码 `E####` 按签发域分段；`gugu explain E0056` 解释任意码或 lint 名。完整定义散在对应 spec 章节；本表是索引。

## 源码与词法（E0001–E0019）

| 码 | 含义 |
| --- | --- |
| `E0001`–`E0008` | 源文件不可读、缺入口、bootstrap 结构、非法 UTF-8/BOM、非法逻辑路径、span 越界、文件过大 |
| `E0009`–`E0019` | 词法：未闭合字面量/块注释/属性括号、非法转义/数字/记号、未知属性、非法 Unicode scalar、C 串内嵌 0、非法字节字符、非法 f-string 说明 |

定义见[词法结构](../spec/lexical.md)。

## 解析、名称与类型（E0020–E0047）

模块、名称解析、类型检查与接口诊断；保留名/私有 std 模块占用报 `E0031`，平台原语未在 unsafe 块调用报 `E0041`。见[声明与模块](../spec/declarations.md)、[类型系统](../spec/types.md)、[标准库](../spec/standard-library.md)。

## comptime 与单态化（E0048–E0055）

| 码 | 含义 |
| --- | --- |
| `E0048` | `expansion-cycle`：宏展开环 |
| `E0049` | `expansion-limit`：展开预算耗尽或非法预算值 |
| `E0050` | `expansion-fragment-mismatch`：片段与期望类别不符 |
| `E0051` | `macro-boundary-error`：宏显式报错 |
| `E0052`/`E0053` | 单态化不收敛 / 实例总数超限 |
| `E0054` | comptime 阶段越界使用 |
| `E0055` | late comptime 求值失败 |

见[编译期执行](../spec/comptime.md)与[程序与编译模型](../spec/program-model.md)。

## lint 与风格（E0056–）

| 码 | 含义 |
| --- | --- |
| `E0056` | lint `large_copy`：按值位结构体 >64B，默认 warn，可 `#[allow]` |
| `E0063` | lint `missing_docs`：公开项缺文档 |

lint 级别 `allow`/`warn`/`deny`/`forbid` 规则见[格式化与代码风格](../spec/format-style.md)与[测试](../spec/testing.md)。

## 运行时报告

fatal、`main` 返回 `Err` 与未处理 panic 的报告格式由 `GUGU_RUNTIME_DIAGNOSTICS` 控制（`text`/`json`/`both`），字段见[运行时与运维语义](../spec/runtime.md)。
