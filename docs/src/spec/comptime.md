# 编译期执行

Gugu 有 Zig 式的 **comptime**：语言的一个子集可以在编译期跑，结果写进目标程序。这不是宏文本替换。

## 能做什么

- 泛型实参、`comptime` 参数、`const`、关联常量、数组长度都在编译期已知。`const` 项与关联常量必须能在编译期求值。
- 可以根据 comptime 条件丢掉死分支、决定 `[T; n]` 的 `N`。按目标删除整项用 `#[cfg]`，不是 comptime：被裁侧不必类型检查。
- `size_of` / `align_of` / `offset_of` / `type_id`、`std.src.file` / `line` / `column` 必须在相应类型已知时 comptime 可求值。`type_id_count()` 只在闭世界具体类型集合冻结后可求值，且不能参与类型形成。`type_id[T]().name()` 在 `T` 已知时也是 comptime。
- 禁止把类型当成一等 comptime 值传递：没有 `fn Foo(comptime T: type) type`，对类型抽象只用 `[T]`。禁止在 comptime 里给 `struct` 动态加字段。
- 单态化、特化和其它需要编译期常量的语义选择都可以消费 comptime结果；内联等纯优化如何使用这些信息只见 [GIR/LIR](../internals/gir-lir.md)。
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

comptime 解释器与运行时的检查消除是两套机制，必须叠加，不是二选一。

**解释器**执行语言语义（绑定、循环、`if`、函数调用、分配到编译期堆）。它负责：用户写的生成代码、常量表、特化分支里被证明为死的那一侧。编译期求得出的长度与下标，在这里直接删检查或直接报错。

**运行时**值上的「下标不越界、溢出不发生」不能指望解释器对所有输入跑一遍。那是优化器的范围 / 约束传播（数据流，即越界检查消除）：`if i >= 0 && i < xs.len() { xs[i] }`、`for i in 0..xs.len()`、循环归纳。它与解释器共享 IR，消费 comptime 已知事实，但不把用户程序当解释对象。

Zig 的 comptime 能让「智能」在你写得出编译期证明时发生；默认的下标检查消除是编译器分析，必须做。两套一起用。

## 边界

- comptime 代码不能启动协程（禁止 `async`）、不能 `recv`/`wait`、不能做目标进程的 syscall。读编译期文件、嵌入字节用 `std.mem.embed_file`（lang item，只在 comptime 合法）：

```
fn embed_file(comptime path: string) [byte; N]
```

`path` 相对**写该调用的源文件**所在目录。`N` 是文件字节数，comptime 已知。读不出或路径非法是编译错误。不校验 UTF-8。
- 编译期堆与运行时堆断开：comptime 分配的值要进目标程序，必须是可物化的常量。
- 无限循环在 comptime 必须被燃料限制打断并报错（即使该 `loop` 的类型是 `!`）。
- 整数在 comptime **溢出是编译错误**，不环绕。
- `std.src.file` / `line` / `column` 取该调用点在编译中的源位置，必须 comptime 已知。

## 求值环境与阶段边界

comptime 使用与运行时相同的表达式、类型、值传递、COW string、模式、defer 和 panic 语义，但只允许确定性且可在编译宿主中安全模拟的子集。ResourceCell 不能在 comptime 构造或发布。读取未初始化值、越界、除零、无效转换、显式 panic 或违反 unsafe 前置条件都转成带源范围的编译错误；comptime 不产生可被目标程序捕获的 Panic 值。

允许的状态只存在于本次 comptime 求值：局部槽、comptime 堆、常量依赖和显式 `embed_file` 输入。禁止读取宿主时间、随机数、环境变量、网络、目标进程 I/O、操作系统线程状态、FFI、内联汇编、原子、锁、channel、`async`、`yield`、`wait` 或 syscall。标准库函数只有其实现本身完全落在该子集内时才能在 comptime 调用。

`cfg` 先删除不存在的项；余下源码全部名称解析和类型检查。普通 `if comptime_condition` 的未选分支仍必须语法与类型正确，只是不执行。`comptime expr` 强制立即求值并把结果物化为常量；不能物化原始宿主指针、运行时句柄、打开的外部资源或指向 comptime 堆的悬空引用。

`const`、普通 `static`、数组长度、判别值、repr 参数和 comptime 泛型实参组成有向依赖图；依赖环是编译错误。函数递归和循环允许，但实现必须以可配置的步骤/内存上限中止不收敛求值，并明确报告资源上限而不是伪装成类型错误。

`embed_file` 的内容字节、规范化后的源相对路径和读取失败都属于编译输入。路径不得逃逸调用源文件所在包允许的编译输入根；符号链接解析后的最终路径同样受限制。实现必须记录该文件依赖，内容变化会使增量缓存失效。
