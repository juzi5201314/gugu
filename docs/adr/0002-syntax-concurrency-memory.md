# ADR-0002：表面语法与并发内存模型

- 状态：已接受
- 日期：2026-08-30

## 背景

ADR-0001 确定了「静态语义、闭世界、runtime 属于语言」之后，需要一次性钉死用户每天接触的表面语法与并发内存模型，避免后续规范在表达式、传递和调度上各自漂移。

## 决策

- 类型：`名字: 类型`；返回类型在 `()` 后只用空白：`fn inc(i: int) int`。
- 结构体按值；共享写 `&T`（永不 null）。绑定默认可变。
- 错误：`Result[T, E]` + `match` + `?`。
- `char` + 精确宽度（`byte`/`u8` 等同名同类型）。字段默认私有。
- UFCS + `trait`/`impl` + 最具体特化；`dyn Trait` 才虚。值用 `.`，类型关联用 `::`（`Vec::new()`、`Point::len(p)`）。泛型 `[T]`，数组 `[T; N]`。
- 有栈协程、多对多调度、抢占、`async` 启动（不是 `async fn` 染色）、`chan[T]` 的 `send`/`recv`、`select`、`yield`。
- 闭包：用户无捕获列表；语义共享+GC 延命；能证伪逃逸则栈/拷贝。
- 溢出：debug 检查 / release 环绕；下标默认检查，可证明则删。
- Windows：PE 导入薄 kernel32/ntdll，不扫 syscall 号。默认不链 CRT。
- 模块顶层有 `static`（进程寿命、GC 根）；没有模块级 `let`。
- 类型别名 `type Name = T`。`extern "C"` 导入/导出。
- 数组 `[T; N]` + 切片 `&[T]`；`Vec` 在 std。
- `while` / `loop` / `for x in xs`；块级 `defer` 与函数级 `defer ret`。
- 插值只有 `f"..."`；齐次变参 `...xs: &[T]`；异构参数包 `fn println[Ts: Print...](...args: Ts)`。
- Zig 式 comptime 解释器 + 独立的范围分析（越界检查消除；与规范 comptime 章的「范围 / 约束传播」是同一件事）。
- 词法：块注释、`raw"..."`、`+=`、按位运算、禁止 `.5`/`5.`、运算符经 trait 重载。
- GC：保持精确、分代和移动语义；managed storage 采用 `TurnRegion`、owner-local Immix `LocalHeap` 与 stable-handle `SharedHeap`，使用 MarkTicket/EdgeDelta 消息、owner credit、TLAB、写屏障、局部 forwarding 和 checked pointer compression。LocalHeap direct-pointer 热路径不加 read barrier；SharedHeap resolve/access guard 只在共享对象路径承担额外访问成本。

## 替代方案

本节为 2026-09 文档整理时补记：列出决策时已隐含拒绝的方向，便于后续对照；非决策当时的原始记录。

- **`async fn` / `.await` 函数染色（Rust/JS 式）**：违背「高并发不要求函数染色」公理；Gugu 的 `async` 只启动协程。
- **尖括号 `<T>` 泛型**：表达式解析歧义与 `>>` 问题，是 C++/Rust 需要 turbofish 的根源；泛型一律 `[T]`。
- **Rust 式所有权/引用计数内存模型**：与闭包共享可变语义、精确移动 GC 冲突；共享语义由 GC 延命承担（传递语义见 ADR-0003，回收平面见 ADR-0009）。
- **默认有缓冲 channel 或所有权转移 channel**：会合语义已经覆盖同步需求，缓冲由 `chan[T](n)` 显式表达；channel 值是身份句柄，不需要转移语义。
- **分号 + 自动分号插入（JavaScript 式 ASI）**：换行续行规则更确定，禁止 ASI。

## 后果

- `&` 既是引用又是按位与，必须靠一元/二元上下文区分。
- 抢占与 GC 必须共享 safepoint，否则无法同时做到百万协程与并发回收。
- 特化在闭世界做全局部分序；交叉重叠是硬错误。
- `println` 走异构参数包单态化，不能写成单一 `&[T]`。
- panic 只展开当前协程；不做 Go `recover`。恢复靠 `Join.wait() Result[T, Panic]` 与 `std.panic.catch`。
- `main` 正常返回则等待其余用户协程；`main` panic 或 `process.exit` 则立刻终止进程。

## 实施状态

表面语法与类型系统（含特化、模式、插值、参数包、`defer`）已在编译器前端完整实现；并发与调度以语言契约加确定性参考模型落地；内存模型中 `TurnRegion`、owner-local `LocalHeap`、`SharedHeap` 稳定句柄、MarkTicket/EdgeDelta 消息与写屏障平面已实现到共享字段存储路由与转发闭环（提交 `cb1e703`…`a043620`）。checked pointer compression、credit termination 与 radix/security profile 按 ADR-0009 的实现顺序尚未完成。
