# 编译期执行

Gugu 有 Zig 式的 **comptime**：语言的一个子集可以在编译期跑，结果写进目标程序。这不是宏文本替换。

## 能做什么

- 泛型实参、`comptime` 参数、`const`、关联常量、数组长度都在编译期已知。`const` 项与关联常量必须能在编译期求值。
- 可以根据 comptime 条件丢掉死分支、决定 `[T; n]` 的 `N`。按目标删除整项用 `#[cfg]`，不是 comptime：被裁侧不必类型检查。
- `size_of` / `align_of` / `offset_of` / `type_id` / `type_id_count`、`std.src.file` / `line` / `column` 必须 comptime 可求值。`type_id[T]().name()` 在 `T` 已知时也是。
- 禁止把类型当成一等 comptime 值传递：没有 `fn Foo(comptime T: type) type`，对类型抽象只用 `[T]`。禁止在 comptime 里给 `struct` 动态加字段。
- 单态化、特化选择、内联候选，都可以消费 comptime 已知信息。
- 编译期可以 panic，效果是编译错误。comptime 里 `panic(...)` 的类型仍是 `!`。

## 语法

```
fn repeat[T](comptime n: int, x: T) [T; n] {
    let a: [T; n]
    for i in 0..n {
        a[i] = x
    }
    a
}

const N: int = 4
let xs = repeat(N, 1)
```

`repeat(N, 1)` 靠实参推断 `T`。要显式写类型实参时用 `repeat::[int](N, 1)`，禁止 `repeat[int](N, 1)`（那是下标）。也可以写 `[x; N]` 字面量，语义相同。

对 `[T; N]` 的元素赋值分析：若 `N` comptime，且控制流证明每个 `i in 0..N` 都执行了恰好一次 `a[i] = ...`（典型是 `for i in 0..N`），则 `a` 视为已初始化。证明不了仍是「读取未初始化」编译错误。运行时循环同样走这条分析，不是「解释一遍循环」才放行。

- 参数标 `comptime`：调用处必须传入编译期已知值。
- 块或表达式标 `comptime { ... }` / `comptime expr`：整段在编译期执行。
- 未标注的表达式若所有输入都是 comptime 已知，编译器必须仍能常量折叠；标 `comptime` 是强制「现在就求值，求不了就报错」。

## 解释器与优化器

comptime **解释器**执行语言语义（绑定、循环、`if`、函数调用、分配到编译期堆）。它负责：用户写的生成代码、常量表、特化分支里被证明为死的那一侧。

它**不是**给运行时变量做抽象解释的唯一引擎。下标不越界、溢出不发生、空指针不出现，对**运行时**值要靠：

1. comptime 已知的长度与下标 → 编译期直接删检查或直接报错。
2. 控制流事实：`if i >= 0 && i < xs.len() { xs[i] }`、`for i in 0..xs.len()`、循环归纳变量。
3. 专门的范围 / 约束传播（数据流），与解释器共享 IR，但不把用户程序「对所有输入跑一遍」。

因此：Zig comptime 能让「智能」在**你写得出编译期证明**时发生；默认的下标检查消除是编译器分析，必须做，而且必须利用 comptime 事实。二者叠加，不是二选一。

## 边界

- comptime 代码不能启动协程（禁止 `async`）、不能 `recv`/`wait`、不能做目标进程的 syscall。读编译期文件、嵌入字节用 intrinsic `embed_file`（参数必须是 comptime 字符串路径，相对**写该调用的源文件**所在目录）。
- 编译期堆与运行时堆断开：comptime 分配的值要进目标程序，必须是可物化的常量。
- 无限循环在 comptime 必须被燃料限制打断并报错（即使该 `loop` 的类型是 `!`）。
- 整数在 comptime **溢出是编译错误**，不环绕。
- `std.src.file` / `line` / `column` 取该调用点在编译中的源位置，必须 comptime 已知。
