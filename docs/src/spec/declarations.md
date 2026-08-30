# 声明与模块

## 模块

一个 `.gg` 文件是一个模块，模块名是去掉 `.gg` 的文件名。

点分路径对应目录：`std.io` → `std/io.gg`，若存在目录 `std/io/` 则入口文件是 `std/io/mod.gg`。文件系统就是模块树，不另写 `mod foo;`。

一个目录是一个**包**（分发与可见性组）。`pub` 跨模块可见；包级私有不另设第三档，直到有明确的 `pub(package)` 需求。

模块顶层只允许：`use`、`fn`、`struct`、`enum`、`trait`、`impl`、`const`、`type`、`static`、`extern` 声明。禁止模块级 `let`。具名 `fn` 不能写在函数体里（嵌套可调用物用闭包）。

## 可见性

默认模块私有。`pub` 可被其他模块 `use`。结构体字段默认私有，公开字段写 `pub x: int`。

## `use`

```
use green
use green.{bar}
use std.io.{print, println}
use std.io as io2
```

| 写法 | 效果 |
|------|------|
| `use green` | 引入模块名，访问 `green.bar` |
| `use green.{bar}` | 引入 `bar` |
| `use std.io as io2` | 模块别名 |
| `use std.io.{print as p}` | 项别名 |
| `pub use green.{bar}` | 再导出：本模块的调用者可以 `use this_mod.{bar}` |

禁止 `use green.*` 全打散。循环 `use` 报错。`use` 不执行代码。

## `let`

```
let i = 0
let i: int = 0
let i: int
i = 1
```

绑定**默认可变**。没有 `mut` / `var`。

类型可省略，必须仍能推到唯一类型。允许先声明后赋值，读取未初始化必须是编译错误（所有路径赋值分析）。

同一块里后一个 `let x` 遮蔽前一个 `x`，引入**新槽**。已经捕获旧槽的闭包继续指向旧槽，不受遮蔽影响。

## 函数

```
fn main() {
    ...
}

fn inc(i: int) int = i + 1

pub fn bar() string = "bar"

fn sum(...xs: &[int]) int { ... }

fn println[Ts: Print...](...args: Ts) { ... }
```

类型位置规则：

- 绑定、参数、字段：`名字: 类型`
- 返回类型在 `()` 后，只用空白，不写 `:` / `->`
- 函数类型：`fn(int, string) bool`
- 省略返回类型 = `()`
- `pub` 必须写全参数类型；返回非 unit 必须写出

`)` 后是 `{` 或 `=` 则无显式返回类型，是类型则有。

表达式体 `= expr`，块体 `{ ... }`。块是表达式，最后一条可以是返回值。

`return expr` / `return` 从**最内层具名函数或闭包**返回，不会穿过外层。闭包里的 `?` 同样只提前返回该闭包（该闭包的返回类型必须实现 `Try`）。

`main` 无参数、返回 `()`，不必 `pub`。runtime 初始化之后调用。

禁止函数按签名重载。方法上的额外类型参数写在名字后面：`fn convert[U](self) U`。

## 结构体、枚举、`const`、`type`、`static`

```
struct Point {
    pub x: int
    pub y: int
}

struct Empty {}

enum Option[T] {
    Some(T)
    None
}

enum Shape {
    Dot
    Line(int, int)
    Rect { w: int, h: int }
}

const N: int = 4
const M = 4

type Ids = Vec[int]
type Table[K] = Vec[K]

static COUNTER: int = 0
```

`const` 必须 comptime 可求值。类型可省略，规则与 `let` 相同。`const` 没有稳定地址，编译器可以内联副本。

`type Name = T` / `type Name[T] = ...` 是透明别名：`Ids` 与 `Vec[int]` 是同一类型，不能给别名单独写一份 `impl`。

`static NAME: T = expr`：进程寿命、有稳定地址。`expr` 必须 comptime。若 `T` 含堆引用，该 static 是 GC 根。读写规则与普通绑定一样（默认可变）。多个 G 无同步地写同一 `static` 是数据竞争。禁止模块级 `let`；进程级可变状态用 `static` 或 `std.sync`。

泛型参数写 `[T]`；数组类型写 `[T; N]`。表达式里的下标与泛型见 [类型系统 · 泛型写法](types.md)。`[]` 里只有类型参数（及类型包 `Ts...`），没有 const 泛型；编译期整数用参数上的 `comptime n: int`。

结构体字面量只能写在能看见所有被赋值字段的模块里（私有字段 = 同模块，或走关联函数构造）。

`trait` / `impl` 见 [接口](traits.md)。`comptime` 见 [编译期执行](comptime.md)。`extern` 见 [unsafe](unsafe.md)。

## 预导入

下列名字在每个模块里直接可用，不必 `use`：

`Option` `Result` `Some` `None` `Ok` `Err` `Vec` `Range` `Join` `ChanClosed` `TrySendErr` `TryRecvErr` `Panic` `Print` `Clone` `Eq` `Ord` `Iter` `IntoIter` `Index` `Try` `Fn`

`print` / `println` 仍在 `std.io`。其它标准库类型按模块路径导入。编译器把上表里的 trait 当 lang item：插值、`for`、`?`、`==`、`[]`（用户类型）必须解析到这些定义，用户不能在自己的模块里再声明同名预导入项。

## 结构体与枚举的值

```
Point { x: 1, y: 2 }
Point { x, y }              // 字段名与当前绑定同名的简写
Result::Ok(1)
Ok(1)                       // 预导入
Some(p)
None
Shape::Rect { w: 1, h: 2 }
```

结构体字面量必须写字段名（禁止只按位置的 `Point(1, 2)`，以免和函数调用、元组式枚举变体混淆）。元组式变体用 `名字(值)`。结构体式变体用 `名字 { 字段: 值 }`。无字段结构体的值写成 `Empty {}`，也可以只写类型名 `Empty`（与 `None` 那种零元构造同一观感）。禁止 `Point { x: 1, ..p }` 这种结构体更新；要改副本就逐字段写，或 `clone` 再赋值。

