# ADR-0005：impl Trait、try、let 链、否定 impl、测试与协程本地

- 状态：已接受
- 日期：2026-08-30

## 背景

类型系统骨架就位后，还差一批「写库会立刻碰到」的表达力缺口：返回闭包与匿名约束、错误传播的统一出口、`&&` 条件链、对 blanket impl 的否定，以及测试与本地存储的语言级约定。本 ADR 一次钉死，避免各项各自引入平行机制。

## 决策

- `impl Trait` 是单态化的存在类型（APIT / RPIT / TAIT），不是 `dyn`。透明 `type` 别名与 `type Foo = impl Trait` 靠右侧是否 `impl` 区分。
- `try { }` 接住 `?`；`Try` 增加 `from_value` / `from_error`。没有错误类型隐式转换。
- `if` / `while` 条件是 `&&` let 链。后缀 `expr.match { }` 与前缀 `match` 同语义。
- `impl !Trait for Type` 挖掉 blanket，并把 `chan` / `Join` 的「禁止 Clone」写成闭世界事实。
- lint 四级：allow / warn / deny / forbid。`#[test]`、doctest、`cfg(test)`。
- 用户要的「线程本地」其实是 `#[coroutine_local] static`（跟协程走，协程换操作系统线程仍是同一槽）。操作系统线程本地是 `#[os_thread_local]`，只给 FFI。进程一次性初始化是 `OnceLock` / `Lazy`。

## 替代方案

本节为 2026-09 文档整理时补记：列出决策时已隐含拒绝的方向，便于后续对照；非决策当时的原始记录。

- **RPIT/APIT 一律擦成 `dyn`**：丢失单态化与内联，违背「泛型默认单态化」公理。
- **`?` 依赖 `From` 式隐式错误转换**：出口处错误类型必须一致，保持显式；需要转换时在代码里写。
- **let 链允许 `||`**：绑定是否在作用域取决于走哪一臂，语义不清；只允许 `&&`。
- **协程本地复用操作系统 TLS**：协程在 safepoint 后迁移线程会读错槽；`#[coroutine_local]` 跟协程走，`#[os_thread_local]` 只留给 FFI。
- **doctest/测试用外部脚本或框架**：与「测试是语言的一部分」冲突；`#[test]`、`cfg(test)` 与 doc-test 进语言与运行器。

## 后果

- 返回闭包不必擦成 `fn(T) U`，也不必手写结构体。
- 协程迁移到另一条操作系统线程后，`#[coroutine_local]` 仍是同一槽；`#[os_thread_local]` 会变成当前操作系统线程的那一份。
- `forbid` 不能被内层 `allow` 打穿，适合测试与库根模块。

## 实施状态

APIT/RPIT/TAIT/RPITIT、`try` 与 `Try::from_value`/`from_error`、`&&` let 链与后缀 `.match`、否定 impl、lint 四级与 `#[coroutine_local]` / `#[os_thread_local]` 已在编译器实现；`#[test]`、doc-test 与测试运行器的执行链路（`gugu run` / `test` 子命令接入 bootstrap、bench harness）尚未实现，语言侧契约已定稿于 [测试](../src/spec/testing.md)。
