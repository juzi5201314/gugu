# 模式

模式出现在 `match`、`if let`、`while let`、`let` / `let-else`、函数与闭包参数里。匹配时字段与元素按 [传递](passing.md) **浅拷**到绑定（与读 `p.x` 相同），不是自动变成 `&T`。没有 `ref` / `ref mut`。

## 可驳与不可驳

- **不可驳：** 对目标类型的任何值都成功。例如 `let (a, b) = t`、`let Point { x, y } = p`、`let Meters(n) = m`。
- **可驳：** 可能失败。例如 `Ok(v)`、`Some(x)`、字面量、范围、带长度约束的切片模式、or 中含可驳臂。

`let` 与函数/闭包参数只接受不可驳模式。可驳模式必须写 `if let`、`while let`、`let-else` 或 `match`。

## 模式语法

| 形式 | 含义 |
|------|------|
| `_` | 丢弃。同一作用域可出现多次。 |
| `x` | 绑定。默认可变，引入新槽。 |
| `x @ 子模式` | 先按子模式匹配，成功则把**整个**被匹配值浅拷到 `x`。 |
| 字面量 | `0`、`true`、`'A'`、`b'A'`。整数/字符/布尔/字节。`string` 与字节串字面量不进模式。 |
| `a..b` | 范围模式。半开，与 `Range` 相同：含 `a`、不含 `b`。只允许出现在 `match` / `if let` / `while let` / `let-else` 的模式里，不能当独立值。两端必须是 comptime 的 `int` 或 `char`（两端同一类型）。`char` 范围只存在于模式里，不产生 `Range` 值；`a..b` 作为表达式时两端必须是 `int`，类型是 `Range`，见 [表达式](expressions.md)。 |
| `P \| Q` | or-模式。`P` 与 `Q` 必须绑定同一组名字，各名字类型相同。 |
| `(P, Q)` | 元组。单元素仍须 `(P,)`。 |
| `[P, Q, R]` | 数组 `[T; N]`，长度必须恰好 `N`。 |
| `[P, ..]`、`[.., Q]`、`[P, .., Q]`、`[..]` | 数组或切片。`..` 吃掉剩余元素。同一模式里至多一个 `..`。 |
| `[P, xs @ .., Q]` | `..` 绑定剩余：对 `[T; N]`，`xs` 类型 `[T; 剩余长度]`（剩余长度 comptime）；对 `&[T]`，`xs` 类型 `&[T]`。 |
| `Point { x, y }` | 结构体。`x` 是字段名简写。`Point { x: P, y }` 给字段子模式。 |
| `Point { x, .. }` | 结构体 rest：其余字段丢弃。这是**模式**，不是值更新；禁止 `Point { x: 1, ..p }`。 |
| `Meters(P)` | 单字段元组结构体（newtype）。 |
| `Ok(P)` / `Result::Ok(P)` | 枚举变体。元组式、结构体式、无载荷与构造写法相同。 |
| `&P` | 匹配 `&T`：按 `P` 匹配所指的 `T`（浅拷出绑定）。 |

or-模式的优先级低于 `@`、构造器与字面量：`A | B` 是两臂；`x @ 0..10 | x @ 20..30` 合法。禁止在 or 的一臂绑定、另一臂不绑定同一名字。

范围模式没有 `..=`。要含上界写 `0..(n + 1)`，`n + 1` 必须仍是 comptime。

## `match`

穷尽匹配，是表达式。臂写成 `模式 => 表达式` 或 `模式 if 守卫 => 表达式`。

```
let n = match r {
    Ok(v) => v
    Err(_) => 0
}

match x {
    0..10 => "small"
    10 => "ten"
    n @ 11..100 => f"{n}"
    _ => "big"
}
```

也允许类型限定：`Result::Ok(v)`。`Ok` / `Err` / `Some` / `None` 在预导入里。

- 未覆盖且无 `_` 必须编译错误。or-模式、范围、守卫都计入覆盖分析；带运行时守卫的臂**不**单独构成穷尽。
- 多臂匹配同一值时，按书写顺序；先匹配者获胜。
- 守卫用 `if`：`Some(x) if x > 0 => ...`。守卫是 `bool` 表达式，可读模式引入的绑定。
- 各臂体类型必须一致，或其中一些是 `!`（见 [类型 · never](types.md)）。该类型即 `match` 的类型。
- `match` 与 `if let` 会自动解一层或多层 `&`：`opt: &Option[T]` 时 `match opt { Some(x) => ... }` 合法。绑定仍按 [传递](passing.md) 浅拷出 `T`。模式里的 `&P` 仍可显式写。

## `if let` / let 链 / `let-else`

```
if let Ok(v) = r {
    use(v)
} else {
    fallback()
}

if let Ok(x) = a && let Ok(y) = b && x > y {
    use(x, y)
}

while let Some(x) = it.next() {
    use(x)
}

let Ok(v) = r else {
    return
}
```

- `if` / `while` 的条件是 let 链（`let 模式 = 表达式` 与 `bool` 用 `&&` 连接），见 [表达式](expressions.md)。`if let Ok(v) = r { }` 是一段式。
- 用作表达式时必须有 `else`，then 与 else 的类型相同（`!` 可与另一臂合流）。用作语句时可以没有 `else`；then 的值丢弃。
- `while` 的 let 链失败则结束循环。作为表达式类型是 `()`。无值 `break` / `continue` 作用于它。
- `let 模式 = 表达式 else { ... }`：模式必须可驳（不可驳写普通 `let`）。失败则跑 `else` 块；`else` 块的类型必须是 `!`。成功则绑定进入当前块后续语句。`let-else` **不是** let 链，不能写成 `let Ok(x) = a && let Ok(y) = b else { ... }`；要链式先 `if`。

禁止不可驳模式当 `if`/`while` 里的 `let` 段（应写成普通 `bool` 或 `let`）。

## `let` 与参数

```
let (a, b) = pair
let Point { x, y } = p
let Meters(n) = m

fn add((x, y): (int, int)) int = x + y
```

先声明后赋值仍只允许**简单标识符**（`let i: int` 然后 `i = 1`）。模式 `let` 必须带初始化器。

函数与闭包参数是不可驳模式。不用的子位置写 `_`。`self` 仍必须是第一个参数，不能是模式的一部分。
