# ADR 0002：表面语法与并发内存模型

- 状态：已接受
- 日期：2026-08-30

## 决策

- 类型：`名字: 类型`；返回类型在 `()` 后只用空白：`fn inc(i: int) int`。
- 结构体按值；共享写 `&T`（永不 null）。绑定默认可变。
- 错误：`Result[T, E]` + `match` + `?`。
- `char` + 精确宽度（`byte`/`u8` 等同名同类型）。字段默认私有。
- UFCS + `trait`/`impl` + 最具体特化；`dyn Trait` 才虚。值用 `.`，类型关联用 `::`（`Vec::new()`、`Point::len(p)`）。泛型 `[T]`，数组 `[T; N]`。
- 有栈协程、M:N、抢占、`async` 启动（不是 `async fn` 染色）、`chan[T]` 的 `send`/`recv`、`select`、`yield`。
- 闭包：用户无捕获列表；语义共享+GC 延命；能证伪逃逸则栈/拷贝。
- 溢出：debug 检查 / release 环绕；下标默认检查，可证明则删。
- Windows：PE 导入薄 kernel32/ntdll，不扫 syscall 号。默认不链 CRT。
- 模块顶层有 `static`（进程寿命、GC 根）；没有模块级 `let`。
- 类型别名 `type Name = T`。`extern "C"` 导入/导出。
- 数组 `[T; N]` + 切片 `&[T]`；`Vec` 在 std。
- `while` / `loop` / `for x in xs`；块级 `defer` 与函数级 `defer ret`。
- 插值只有 `f"..."`；齐次变参 `...xs: &[T]`；异构参数包 `fn println[Ts: Print...](...args: Ts)`。
- Zig 式 comptime 解释器 + 独立的范围分析（BCE）。
- 词法：块注释、`raw"..."`、`+=`、按位运算、禁止 `.5`/`5.`、运算符经 trait 重载。
- GC：精确、分代、Immix 老年代、并发标记与回收、TLAB、写屏障、不在热路径加读屏障。

## 后果

- `&` 既是引用又是按位与，必须靠一元/二元上下文区分。
- 抢占与 GC 必须共享 safepoint，否则无法同时做到百万协程与并发回收。
- 特化在闭世界做全局部分序；交叉重叠是硬错误。
- `println` 走异构参数包单态化，不能写成单一 `&[T]`。
- panic 只展开当前 G；不做 Go `recover`。恢复靠 `Join.wait() Result[T, Panic]` 与 `std.panic.catch`。
- `main` 正常返回则等待其余用户 G；`main` panic 或 `process.exit` 则立刻终止进程。
