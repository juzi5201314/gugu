# 表达式与语句

块用 `{` `}`。条件不用括号。`if`、`match`、块都是**表达式**：有 `else` 时分支类型必须一致；块的值是最后一条表达式。若最后一条是声明、赋值、`defer`、循环，块类型为 `()`。

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

- 用作表达式（`let`、`return`、调用实参等要取值）：必须有 `else`，两分支类型相同。
- 用作语句（值丢弃）：可以没有 `else`；then 分支的值被丢弃，不强制是 `()`。
- 没有标号：`break` / `continue` 只作用于最内层循环。禁止 `break 'a`。

没有分号终止语句。块中要丢弃某个表达式的值、以免它成为块值时，写 `_ = expr` 或 `expr;`。赋值 `=` 不是表达式，禁止 `if x = 1`、禁止 `a = b = 0`。

## `match`

穷尽匹配，是表达式：

```
let n = match r {
    Ok(v) => v
    Err(_) => 0
}
```

也允许类型限定：`Result::Ok(v)`。`Ok` / `Err` / `Some` / `None` 在预导入里。

- 未覆盖变体且无 `_` 必须编译错误。
- 可以解构元组、结构体、数组、引用。`&Point { x, y }` 把字段**浅拷**到 `x`、`y`（与读 `p.x` 相同），不是 `&int`。
- 守卫用 `if`：`Some(x) if x > 0 => ...`

## 循环

```
while cond { ... }

loop { ... }

for x in xs { ... }
for i in 0..n { ... }
```

- `while` / `loop` / `for` 作为表达式时默认类型是 `()`。
- `break` / `continue` 作用于最内层循环。`loop` 允许 `break expr`，此时 `loop` 的类型是该表达式类型。`while` / `for` 只允许无值 `break`（否则与 `()` 出口不一致）。
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
- `defer ret expr` / `defer ret { ... }`：当前**函数**返回时才跑（含 `?` 从函数返回，含 panic 展开当前 G）。这是「在 `if` 里拿到资源，但要函数结束再收」的写法。
- `ret` 只在 `defer` 后面有特殊含义，不是全局关键字（别处仍可当标识符）。
- `defer ret` 不是闭包。闭包仍写 `fn() { ... }`。
- `defer` 里再发生 panic：若当前 G **已经**在展开（正在处理另一个 panic），进程 abort；否则这条 panic 成为当前 G 的 panic，继续展开。

`yield` 是语句，不是表达式：让出当前 G。禁止 `let x = yield`。

## 运算符

算术 `+ - * / %` 及 `+=` 等。比较 `== != < <= > >=`。逻辑 `&& || !` 只作用于 `bool`，短路。按位 `& | ^ << >>` 及复合赋值。

赋值 `=` 不是表达式。禁止 `a = b = 0`。

内置整数（`int` / `uint` / `byte` / 精确宽度）的溢出、除零、移位、向零整除见 [类型系统](types.md)。下标默认检查；编译器证明安全则删除；`unsafe` 无检查。

用户类型通过 trait 重载对应运算符，见 [接口](traits.md)。

### `&` 与 `*`

- 类型 `&T`、前缀 `&x`：取引用，指向绑定或字段槽。
- 前缀 `*p`：解引用。`p: &T` 时 `*p` 的类型是 `T`（安全）。`p: *T` 时必须在 `unsafe` 里。
- 通过引用换绑或改位类型：`*p = v`。
- 二元 `a & b`：按位与（或 `BitAnd`）。
- 方法调用会自动解一层或多层 `&`：`p: &Vec[int]` 时 `p.push(1)` 合法。普通函数调用**不会**自动取地址或解引用，见 [传递](passing.md)。

## 区间与切片下标

`a..b` 是半开区间，类型 `Range`（`start` 含、`end` 不含）。`..` 是记号。没有 `..=`（要闭区间写 `0..(n + 1)`）。`a` 与 `b` 必须是 `int`。禁止单独的 `a..` / `..b` / `..` 当值。

`xs[i]` 单元素。`xs[a..b]`、`xs[a..]`、`xs[..b]`、`xs[..]` 得到切片 `&[T]`（数组/切片/`Vec`）或 `string`（对 `string`，边界必须在码点上）。不完整区间只允许写在 `[]` 里。

## 元组字段

`t.0`、`t.1` 可读可写（`t` 是绑定或通过引用可到达的槽）。禁止 `5.` 浮点就是为了让 `.0` 只表示元组字段。命名字段 `p.x`。模块路径 `io2.eprint`。`a[i] = v` 走内置或 `Index`。标量转换见 [类型系统](types.md) 的 `int(x)` / `byte(x)`，不用 `as`。

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

操作数必须实现 `Try`，规则见 [接口](traits.md) 与 [类型](types.md)。

```
fn load() Result[int, IoError] {
    let f = open("a")?
    Ok(f.read_i64()?)
}
```

## `async`（启动协程，不是染色）

`async` **不是** Rust/JS 那种把函数变成 Future、需要 `.await` 传染的关键字。任意普通 `fn` 里都可以 `recv` / `wait`。`async` 只表示：**把后面这块工作放到新 G 上跑**。

```
async f(x)
let h = async { inc(i) }
let r = h.wait()
```

- `async` 的操作数只是**紧随其后的一次调用或一个块**。`async f(x).wait()` 解析为 `(async f(x)).wait()`，不是 `async (f(x).wait())`。
- `async 调用` 与 `async { 块 }` 都是表达式，类型是 `Join[T]`，`T` 是调用或块的值类型。
- `wait()`：当前 G 阻塞到子 G 结束，类型 `Result[T, Panic]`。正常结束是 `Ok(T)`；子 G panic 并展开完毕后是 `Err(p)`，等待者**不会**跟着 panic。这就是恢复：没有 `recover()`。不要把 `wait()` 的结果当成 `T`。
- 丢掉 `Join`（不 `wait`）= 分离。分离 G panic 时：主 G 仍在跑则只打印、不杀进程；`main` 已正常返回、进程正在等待用户 G 结束则记一次未处理 panic（最终退出码非 0）。见 [运行时](runtime.md)。
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
- `try_recv()`：`Result[T, TryRecvErr]`。`Err(TryRecvErr::Empty)` 会阻塞；`Err(TryRecvErr::Closed)` 已关闭且收尽。
- 不存在 nil channel。未初始化的 `let c: chan[int]` 不能读。
- 进入 `select` 时：先求值每个分支的 channel 表达式以及 `send` 的载荷，再等待。未选中的分支**不发送、不接收**；载荷表达式的副作用已经发生。
- `select` 是表达式。所有分支（含 `_`）的体类型必须一致，该类型即 `select` 的类型。当语句用时体为 `()`。
- `select` 的分支只能是 channel 的 `send` / `recv`，以及 `Join` 的 `wait()`。`try_send` / `try_recv` 不进 `select`。就绪分支随机公平；`_` 是默认、不阻塞。无就绪且无默认则挂起当前 G。

## 优先级（从紧到松）

1. 路径、调用、索引、字段、后缀 `?`
2. 前缀 `async`（操作数只是一次调用或 `{` 块；其后的 `.wait()` 挂在 `Join` 上）、一元 `!` `-` `&` `*`
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
13. `=` `+=` …（语句级）
