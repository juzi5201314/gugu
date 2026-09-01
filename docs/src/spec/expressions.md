# 表达式与语句

块用 `{` `}`。条件不用括号。`if`、`match`、`try`、块都是**表达式**：有 `else` 时分支类型必须一致，或一臂是 `!`（见 [类型 · never](types.md)）；块的值是最后一条表达式。若最后一条是声明、赋值、`defer`、循环，块类型为 `()`。`return` / `break` / `continue` 是类型为 `!` 的表达式。`if` 的条件是 let 链，见下。

```
let x = if i <= 1 {
    print(f"i = {i}")
    i
} else {
    io2.eprint("err")
    -1
}
```

`else if` 是 `else` 后紧跟 `if`。

- 用作表达式（`let`、`return`、调用实参等要取值）：必须有 `else`，两分支类型相同，或一臂为 `!` 则结果是另一臂的类型。
- 用作语句（值丢弃）：可以没有 `else`；then 分支的值被丢弃，不强制是 `()`。
- 没有标号：`break` / `continue` 只作用于最内层循环。禁止 `break 'a`。

没有分号终止语句。块中要丢弃某个表达式的值、以免它成为块值时，写 `_ = expr` 或 `expr;`。赋值 `=` 不是表达式，禁止 `if x = 1`、禁止 `a = b = 0`。

`return expr` / `return`、`break expr` / `break`、`continue` 是表达式，类型 `!`。它们离开最内层具名函数/闭包或最内层循环（`break`/`continue` 不离开函数）。闭包里的 `return` 只离开该闭包。`?` 离开最内层 `try` 块，没有 `try` 才离开最内层函数/闭包。

## `match` / `if` 链 / `let-else`

模式、穷尽性、or / `@` / rest / 范围见 [模式](patterns.md)。`if` / `while` 的条件是 **let 链**，不只是 `bool`。

```
let n = match r {
    Ok(v) => v
    Err(_) => 0
}

let n = r.match {
    Ok(v) => v
    Err(_) => 0
}

if let Ok(x) = a && let Ok(y) = b && x > y {
    use(x, y)
}

if ready && let Some(v) = opt {
    use(v)
}

let Ok(v) = r else {
    return
}
```

- 前缀 `match expr { ... }` 与后缀 `expr.match { ... }` 语义相同。后缀挂在优先级第 1 档（与字段、调用、`?` 同级），便于 `foo().bar().match { ... }`。`match` 是关键字，不能当字段名，故 `.match {` 无歧义。
- let 链只允许用 `&&` 连接，禁止 `||`（否则绑定是否在作用域里取决于哪一臂）。每一段是 `let 模式 = 表达式` 或类型为 `bool` 的表达式。从左到右求值；`let` 失败或 `bool` 为 false 则整链失败。先成功的 `let` 引入的绑定在后续段以及 then 体里可见，在 `else` 里不可见。
- `if let Ok(v) = r { }` 就是一段式 let 链。`while` / `while let` 同样走 let 链；链失败则结束循环。
- `else if` 后面可以是新的 let 链。

## `try`

`try { ... }` 是表达式。块里的 `?` 把失败交给这个块，而不是 `return` 出函数。

```
let r = try {
    let f = open("a")?
    f.read_i64()?
}
```

- 类型必须实现 `Try`（通常由期望类型推成 `Result[T, E]` 或 `Option[T]`）。
- 块的最后一条表达式的类型是 `Try::Value`，整个 `try` 的值是 `from_value(那条)`。最后一条是声明/赋值/`defer`/循环则 `Value = ()`。
- `?` 失败时 `try` 的值是 `from_error(e)`，类型仍是该 `Try`。`Error` 必须一致，无隐式转换。
- `return` / `break` / `continue` 仍穿过 `try`，不把 `try` 当函数。
- 嵌套 `try`：`?` 只交给最内层。
- 没有 `try fn`。不要用 `try` 当标识符（它是关键字）。

## 循环

```
while cond { ... }

loop { ... }

for x in xs { ... }
for i in 0..n { ... }

while let Some(x) = it.next() { ... }
```

- `while` / `for` / `while let` 作为表达式时类型是 `()`（它们可以正常结束）。只允许无值 `break`。
- `loop`：没有任何 `break`（含经 `if` / `match` 到达的 `break`）则类型是 `!`。若存在不带表达式的 `break`，则类型是 `()`。有 `break expr` 则类型是该表达式类型；同一 `loop` 里所有 `break expr` 的类型必须相同，且不能混用无值 `break`。
- `break` / `continue` 作用于最内层循环。
- `for x in xs` 展开为：`let it = xs.into_iter()`，然后反复 `it.next()`，见 [接口 · IntoIter](traits.md)。
  - `[T; N]` 与 `&[T]`：`Item = T`，每次**拷贝**元素。
  - `Range`（`0..n`）：`Item = int`。`n` 是表达式，常用 `xs.len()`（方法，不是字段）。
  - `string`：不实现 `IntoIter`。用 `s.chars()` 或标准库字节迭代。
  - 其它类型必须实现 `IntoIter`。
  - 要避免大结构体拷贝：遍历下标，或让 `Item = &T`。

## `defer`

```
defer f.close()
defer {
    a()
    b()
}
defer ret f.close()
defer ret {
    a()
    b()
}
```

- `defer expr` / `defer { ... }`：当前**块**退出时 LIFO 执行（含 `return` / `?` / `break` 离开该块）。循环体里的 `defer` 每次迭代结束就跑。
- `defer ret expr` / `defer ret { ... }`：当前**函数**返回时才跑（含 `?` 从函数返回，含 panic 展开当前协程）。这是「在 `if` 里拿到资源，但要函数结束再收」的写法。
- `ret` 只在 `defer` 后面有特殊含义，不是全局关键字（别处仍可当标识符）。
- `defer ret` 不是闭包。闭包仍写 `fn() { ... }`。
- `defer` 里再发生 panic：若当前协程 **已经**在展开（正在处理另一个 panic），进程 abort；否则这条 panic 成为当前协程的 panic，继续展开。

`yield` 是语句，不是表达式：让出当前协程。禁止 `let x = yield`。

## 运算符

算术 `+ - * / %` 及 `+=` 等。比较 `== != < <= > >=`。逻辑 `&& || !` 只作用于 `bool`，短路。按位 `& | ^ << >>`、一元 `~`（只用于整数）及复合赋值。

赋值 `=` 不是表达式。禁止 `a = b = 0`。

内置整数（`int` / `uint` / `byte` / 精确宽度）的溢出、除零、移位、向零整除见 [类型系统](types.md)。下标默认检查；编译器证明安全则删除；`unsafe` 无检查。

用户类型通过 trait 重载对应运算符，见 [接口](traits.md)。

### `&` 与 `*`

- 类型 `&T`、前缀 `&x`：取引用，指向绑定或字段槽。
- 前缀 `*p`：解引用。`p: &T` 时 `*p` 的类型是 `T`（安全）。`p: *T` 时必须在 `unsafe` 里。
- 通过引用换绑或改位类型：`*p = v`。
- 二元 `a & b`：按位与（或 `BitAnd`）。
- 方法调用会自动解一层或多层 `&`：`p: &Vec[int]` 时 `p.push(1)` 合法。字段访问同样：`p: &Point` 时 `p.x` 合法。普通函数调用**不会**自动取地址或解引用，见 [传递](passing.md)。

## 区间与切片下标

`a..b` 是半开区间，类型 `Range`（`start` 含、`end` 不含）。`..` 是记号。没有 `..=`（要闭区间写 `0..(n + 1)`）。`a` 与 `b` 必须是 `int`。禁止单独的 `a..` / `..b` / `..` 当值。模式里允许 `char` 范围，那是模式语法，不产生 `Range` 值，见 [模式](patterns.md)。

`xs[i]` 单元素。`xs[a..b]`、`xs[a..]`、`xs[..b]`、`xs[..]` 得到切片 `&[T]`（数组/切片/`Vec`）或 `string`（对 `string`，边界必须在码点上）。不完整区间只允许写在 `[]` 里。

## 元组字段

`t.0`、`t.1` 可读可写（`t` 是绑定或通过引用可到达的槽，含自动解 `&`）。禁止 `5.` 浮点就是为了让 `.0` 只表示元组字段。命名字段 `p.x`。模块路径 `io2.eprint`。`a[i] = v` 走内置或 `Index`。标量转换见 [类型系统](types.md) 的 `int(x)` / `byte(x)`，不用 `as`。

## 调用与变参

`f(a, b)`。

齐次变参：

```
fn sum(...xs: &[int]) int {
    let s = 0
    for x in xs {
        s += x
    }
    s
}

sum(1, 2, 3)
```

实参被物化为切片（可能在栈上）。变参绑定的类型是 `&[T]`。

异构变参（`println("i + 1 = ", inc(i), bar())` 各参数类型不同）不能写成 `...args: &[T]`。它必须是**泛型参数包**，在调用点单态化：

```
fn println[Ts: Print...](...args: Ts)
```

`print` / `println` 是 `std.io` 里用参数包 + `Print` trait 写的普通函数，不是编译器魔法。每个调用点生成一份特化，不装箱。

## 字符串插值

只有 `f"..."` 插值。`{expr}` 必须实现 `Print`。插值把内容写进内部 `Vec[byte]` 再做成 `string`，见 [接口 · Print](traits.md)。普通 `"..."` 不含插值。没有 `{x:02}` 格式后缀。

## `?`

操作数必须实现 `Try`。出口是最内层 `try`，否则最内层函数/闭包，见 [接口 · Try](traits.md)。

```
fn load() Result[int, IoError] {
    let f = open("a")?
    Ok(f.read_i64()?)
}
```

## `async`（启动协程，不是染色）

`async` **不是** Rust/JS 那种把函数变成 Future、需要 `.await` 传染的关键字。任意普通 `fn` 里都可以 `recv` / `wait`。`async` 只表示：**把后面这块工作放到新协程上跑**。

```
async f(x)
let h = async { inc(i) }
let r = h.wait()
```

- `async` 的操作数只是**紧随其后的一次调用或一个块**。`async f(x).wait()` 解析为 `(async f(x)).wait()`，不是 `async (f(x).wait())`。
- `async 调用` 与 `async { 块 }` 都是表达式，类型是 `Join[T]`，`T` 是调用或块的值类型。
- `wait()`：当前协程阻塞到子协程结束，类型 `Result[T, Panic]`。正常结束是 `Ok(T)`；子协程 panic 并展开完毕后是 `Err(p)`，等待者**不会**跟着 panic。这就是恢复：没有 `recover()`。不要把 `wait()` 的结果当成 `T`。
- 丢掉 `Join`（不 `wait`）= 分离。分离协程 panic 时：主协程仍在跑则只打印、不杀进程；`main` 已正常返回、进程正在等待用户协程结束则记一次未处理 panic（最终退出码非 0）。见 [运行时](runtime.md)。
- 禁止 `async fn`。没有 `.await`。
- 同栈隔离用 `std.panic.catch`，不要为了 catch 去发明关键字。

## Channel 操作

`chan[T]` 的发送/接收是编译器认识的方法，不能由用户重载：

```
ch.send(x)
let r = ch.recv()
ch.close()
select {
    ch.send(x) => ...
    let r = ch.recv() => ...
    let r = h.wait() => ...
    _ => ...
}
```

- `send(T)`：阻塞直到送出。channel 已关闭则 panic。类型 `()`。对已关闭的 channel 再 `close()` 也 panic。
- `recv()`：阻塞直到收到或关闭。类型 `Result[T, ChanClosed]`。关闭且缓冲收尽后返回 `Err(ChanClosed)`。
- `try_send(T)`：`Result[(), TrySendErr]`。`Ok(())` 已送出；`Err(TrySendErr::Full)` 缓冲满（无缓冲则没有会合的接收者）；`Err(TrySendErr::Closed)` 已关闭。
- `try_recv()`：`Result[T, TryRecvErr]`。`Err(TryRecvErr::Empty)` 立即返回，不阻塞；`Err(TryRecvErr::Closed)` 表示 channel 已关闭且缓冲已收尽。
- 不存在 nil channel。未初始化的 `let c: chan[int]` 不能读：与其它绑定相同，读取未初始化是编译错误，见 [声明 · let](declarations.md)。
- 进入 `select` 时：先求值每个分支的 channel 表达式以及 `send` 的载荷，再等待。未选中的分支**不发送、不接收**；载荷表达式的副作用已经发生。
- `select` 是表达式。所有分支（含 `_`）的体类型必须一致（`!` 可与另一臂合流），该类型即 `select` 的类型。当语句用时体为 `()`。
- `select` 的分支只能是 channel 的 `send` / `recv`，以及 `Join` 的 `wait()`。`try_send` / `try_recv` 不进 `select`。就绪分支随机公平；`_` 是默认、不阻塞。无就绪且无默认则挂起当前协程。

## 优先级（从紧到松）

1. 路径、调用、索引、字段、后缀 `?`、后缀 `.match { ... }`
2. 前缀 `async`（操作数只是一次调用或 `{` 块；其后的 `.wait()` 挂在 `Join` 上）、一元 `!` `-` `~` `&` `*`（类型位置的 `!` 是 never，不是本层前缀）
3. `*` `/` `%`
4. `+` `-`
5. `<<` `>>`
6. 二元 `&`
7. `^`
8. 二元 `|`
9. `..`（两端都要有操作数）
10. 比较
11. `&&`
12. `||`
13. `=` `+=` `-=` `*=` `/=` `%=` `&=` `|=` `^=` `<<=` `>>=`（语句级，不结合）

## 求值、位置与完成顺序

表达式求值产生一个值；`place` 是可以被读取、赋值或取引用的槽表达式。绑定、字段、元组字段、解引用和内置下标在满足类型与安全约束时是 place；字面量、运算结果、函数返回值和临时聚合不是 place。需要 place 的上下文若得到临时值是编译错误，除非该语法明确创建一个新槽（例如 `let` 或 `LocalArena.alloc` / `SyncArena.alloc`）。

除短路逻辑外，求值顺序固定为从左到右：先求值调用目标，再依次求值实参；先求值二元运算符左操作数，再求值右操作数；先确定字段/下标的接收者，再求值下标；数组、元组、结构体和枚举的成员表达式按书写顺序求值。操作数求值完成后才执行运算、调用或构造。`&&` 在左侧为 `false` 时不求值右侧，`||` 在左侧为 `true` 时不求值右侧。

赋值先求值左侧 place 的定位信息，再求值右侧表达式，最后执行一次写入；定位信息只保存语义槽或句柄，不重复求值接收者和下标。复合赋值等价于读取左值一次、求值右值一次、执行对应运算并写回一次，因此 `a[i] += f()` 不会重复计算 `a` 或 `i`。把安全引用写入 managed 值必须保持其目标可达；具体屏障见[GC 元数据](../internals/gc-metadata.md)。

函数调用的所有实参在进入被调用函数前完成求值。调用期间发生的 panic 不会执行尚未开始的调用体，但已经完成的实参副作用保留。trait 运算符、方法和内置运算都遵守同一顺序；静态分发、内联和检查消除不能改变顺序或可观察副作用。

块按顺序执行其中的语句。最后一条没有丢弃标记的表达式是块值；带分号的表达式、声明、赋值、`defer` 和 `yield` 使该位置只产生 `()`。空块和没有值的控制流块产生 `()`。块值在离开块前已物化，随后执行该块的 `defer`。

## 返回、循环退出与 `defer`

`return expr` 先求值并保存返回值，再从最内层函数或闭包开始展开；无表达式的 `return` 保存 `()`。`break expr` 同样先保存循环结果，再执行离开循环体所需的 defer。`?` 在失败时构造出口类型的错误值，然后按同一展开流程离开最内层 `try` 或函数/闭包。保存的结果不会因后续 defer 修改原槽而改变。

`defer expr` 的 `expr` 必须是一次函数或方法调用；注册时立即求值并保存函数值、接收者和所有参数，但不调用函数，退出当前块时才调用已保存的动作并丢弃其返回值。`defer { ... }` 注册一个代码块，代码块的外层绑定按普通闭包规则引用槽，注册后对槽的修改在执行时可见。`defer ret` 使用相同的注册规则，但只在当前具名函数或闭包真正返回或因 panic 展开到边界时执行。

同一作用域中的 deferred action 按后注册先执行；块 defer 按嵌套块从内到外展开，函数返回 defer 在函数边界执行。一次退出只执行每个已注册 action 一次；循环体每轮都是新块，因此该轮注册的 defer 在该轮结束时执行。正常返回值在所有相关 defer 执行前已经保存。defer 中再次 panic 的处理见[运行时](runtime.md)。

`yield` 保存当前协程的完整可扫描状态并让出；恢复后从下一条指令继续。`yield` 不创建新值，也不改变局部槽、defer 注册表或 `Join` 状态。

## `try`、`async` 与 `Join`

`try { block }` 先建立一个新的错误出口，再执行 block。成功时把 block 的值传给 `Try::from_value`；`?` 失败时把错误传给 `Try::from_error`，执行该 try block 已注册的 block defer，然后产生整个 try 表达式的值。`return`、`break` 和 `continue` 不在 try 处截获，继续穿过它。嵌套 try 总是使用最近的出口。

`async` 的调用形式先在当前协程按普通调用顺序求值被调用函数和所有实参，然后创建可运行的子协程；块形式创建的闭包按[函数与闭包](functions.md)的槽捕获规则执行。子协程何时第一次获得执行机会没有顺序保证，父协程不能依赖它在 `async` 表达式返回前或之后启动；队列与提交协议见[调度器内部规范](../internals/scheduler.md)。

`Join[T]` 包含子协程的唯一完成记录，但句柄本身可以被复制传递为句柄值；语言不给它 `Clone`。`wait()` 阻塞当前协程直到记录完成，然后返回记录中的同一个 `Result[T, Panic]` 表示；重复 `wait()` 不重新执行子协程，也不重复展开 panic。丢弃尚未完成的 Join 是分离，不取消子协程；分离后的结果只能按[运行时](runtime.md)的未处理 panic 规则处理。

## `select` 的求值与分支

进入 `select` 时按分支书写顺序求值每个 channel/Join 接收者，以及发送分支的载荷；这些求值只发生一次。若某些分支已经就绪，则从全部就绪分支中随机选择一个并提交其操作，忽略 default；若没有就绪分支，有 default 就立即选择 default，否则挂起当前协程。选择和提交是一个不可分割的操作，未选分支不发生发送、接收或等待。

接收和等待分支的模式必须不可驳；分支被选中且操作完成后按该模式建立绑定，因此不存在运行时模式失败或“窥视但不取走”消息。选中分支的绑定只在其分支体内有效。所有分支体的类型先静态检查并按 `!` 合流；运行时选择不会改变 `select` 的静态类型。

优先级第 3 至第 8 档左结合；比较与区间不结合，`a < b < c` 和 `a..b..c` 是编译错误。前缀运算符从右向左嵌套；后缀运算符按书写顺序从左到右应用。赋值不产生值，也不能作为另一个赋值的右侧结构。
