# 接口、实现与特化

没有类、没有继承、没有可覆盖的槽。多态是：单态化泛型、枚举、闭包、以及显式的 `dyn Trait`。

## `trait`

```
trait Add[Rhs] {
    type Output
    fn add(self, rhs: Rhs) Self::Output
}

trait Dim {
    const N: int
}
```

- 方法签名是契约。可以有默认方法体。
- 可以有关联类型。
- 可以有关联常量：`const N: int`。必须 comptime 可求值。在 impl 里写 `const N = 4`（类型可省略）。使用处写 `Self::N` / `Trait::N` / `Type::N`。出现在数组长度、`comptime` 参数、`match` 范围端点里。
- 可以有无 `self` 的关联函数；调用写 `Trait::name(...)` 或能唯一确定时的 `Type::name(...)`。
- 泛型参数写在 `[]` 里。
- 禁止 `trait A: B` 继承。要组合约束写 `T: A + B`。
- 禁止默认类型实参。

运算符重载通过指定 trait 完成，不能随意给任意符号写全局函数：

| 运算符 | 标准库 trait |
|--------|-----------------|
| `+ - * / %` | `Add` `Sub` `Mul` `Div` `Rem` |
| `+=` 等 | 对应 `AddAssign` 等 |
| `== !=` | `Eq` |
| `< <= > >=` | `Ord` |
| `[]` | `Index` |
| 二元 `& \| ^ << >>` | `BitAnd` 等 |

禁止重载：`&&` `||`（短路）、`.`、`=`（赋值）、`async`、`select`、channel 的 `send`/`recv`、`?`（除非该类型实现 `Try`）。一元 `&` 是取引用，不能重载。

内置整数、浮点、`string` 的运算符与 `==` 不走 trait 分发，由编译器直接降成指令；溢出与 IEEE 规则见 [类型](types.md)。`Clone` 见 [传递](passing.md)，只表示深拷贝。

## 语言认识的 trait

这些在预导入里，编译器按名字挂钩。用户给自己的类型 `impl`，不能重新声明这些 trait。

### `Print`

插值与 `std.io.print` / `println` 把值写成 UTF-8 字节：

```
trait Print {
    fn print(self, buf: &Vec[byte])
}
```

`f"i = {i}"` 等价于往一个 `Vec[byte]` 里依次 `print` 各段，再把合法 UTF-8 做成 `string`。内置标量、`string`、`bool` 实现 `Print`。不要把 `print` 理解成「直接写 stdout」——那是 `std.io` 在缓冲之后的 syscall。

### `IntoIter` / `Iter`

```
trait IntoIter {
    type Item
    type Iter
    fn into_iter(self) Iter
}

trait Iter {
    type Item
    fn next(self: &Self) Option[Item]
}
```

`IntoIter::Iter` 必须实现 `Iter`，且 `Iter::Item` 与 `IntoIter::Item` 相同，否则该 impl 非法。`for x in xs` 是 `let it = xs.into_iter()` 再循环 `it.next()`。`xs` 按值传入 `into_iter`（句柄则共享载荷）。`[T; N]` 与 `&[T]` 的语言 impl **不**先 memcpy 整个数组，游标按索引逐个浅拷元素。`Range` 由语言提供 `IntoIter`。

### `Try`

```
trait Try {
    type Value
    type Error
    fn branch(self) Result[Value, Error]
    fn from_value(v: Value) Self
    fn from_error(e: Error) Self
}
```

`expr?` = `match expr.branch() { Ok(v) => v, Err(e) => 把 from_error(e) 交给最内层出口 }`。出口是：最内层 `try` 块（该块表达式的结果），否则最内层具名函数或闭包的 `return`。没有 `From` 式转换。出口处的 `Try::Error` 必须与操作数的 `Try::Error` 相同（对 `Option` 两边都是 `()`）。

`Result[T, E]`：`Value = T`、`Error = E`，`from_value` 是 `Ok`，`from_error` 是 `Err`。`Option[T]`：`Value = T`、`Error = ()`，`from_value` 是 `Some`，`from_error(())` 是 `None`。

`try` 块见 [表达式](expressions.md)。

### `Index`

```
trait Index {
    type Output
    fn index(self: &Self, i: int) Output
    fn index_set(self: &Self, i: int, v: Output)
}
```

仅用户类型。`a[i]` 调 `index`；`a[i] = v` 调 `index_set`。`[T; N]`、`&[T]`、`Vec[T]`、`string` 不走此 trait。

### `Fn` / `Eq` / `Ord` / `Clone`

`Fn(T) U` 见 [函数](functions.md)，不能用户 `impl`。`Any` 见下，同样不能用户 `impl`。

```
trait Eq {
    fn eq(self: &Self, other: &Self) bool
}

trait Ord {
    fn cmp(self: &Self, other: &Self) int  // <0 / 0 / >0
}
```

`==` `!=` 对用户类型走 `Eq`；`<` 等走 `Ord`。`#[derive(Eq)]` / `#[derive(Ord)]` 要求所有字段都实现对应 trait。`Ord` 必须是全序。不要给 `float` 写 `impl Ord`（浮点比较是内置 IEEE）。含浮点字段的 `#[derive(Eq)]` 仍用 IEEE `==`。`TypeId` 的比较由编译器按编号直接做，并提供 `Eq` / `Ord`。`Clone` 见 [传递](passing.md)。

### `Any`

```
trait Any {
    fn type_of(self: &Self) TypeId
}
```

编译器认识的 lang item，用户不能重新声明、不能手写肯定或否定 impl。编译器给所有拥有 `TypeId` 的类型生成 impl；语言对 `!` 与 `MaybeUninit[T]` 写 `impl !Any`。方法不能是泛型的，否则不能 `dyn Any`。`is` / `downcast` / `downcast_copy` 是 `dyn Any` 的固有方法，见 [类型 · TypeId](types.md)。

## `impl`

```
impl Point {
    fn origin() Point = Point { x: 0, y: 0 }
    fn len(self) float = ...
}

impl Vec[T] {
    fn new() Vec[T] = ...
    fn push(self, x: T) { ... }
}
```

路径规则：

- **值后面用 `.`：** 字段、方法。`p.x`、`p.len()`、`v.push(x)`、模块项 `std.io.print`、`green.bar`。没有 `()` 的 `.len` 是字段，不是方法。
- **类型后面用 `::`：** 关联函数（无 `self`）以及从类型出发的 UFCS。`Vec::new()`、`Vec[int]::new()`、`Point::origin()`、`Point::len(p)`。禁止 `Vec::[int]::new()`；显式类型实参贴在类型名上。
- **没有把模块改成 `::`。** `use std.io` 仍是点分路径。类型上的 `::` 只接在类型名（可带 `[实参]`）后面。

这样 `Vec::new` 和 `v.new` 不会混：前者是静态构造，后者会去找值 `v` 上名叫 `new` 的方法（通常没有）。

- `impl Type { ... }` 给类型挂固有方法与关联函数。同一类型可以有多块固有 `impl`，合并看待。
- 类型参数：出现在 Self 或 trait 实参里的 `T` 由此引入，写作 `impl Vec[T]`、`impl Print for Foo[T]`。blanket `impl Trait for T` 必须写成 `impl[T] Trait for T`（可带约束 `impl[T: Print] Trait for T`）。
- 带 `self` 的是方法：`p.len()` 与 `Point::len(p)` 等价（UFCS）。没有隐式 `this`；`self` 必须是第一个参数。
- 不带 `self` 的是关联函数：`Vec::new()` / `Vec[int]::new()`，没有接收者。能从上下文推断时 `let v: Vec[int] = Vec::new()` 即可。
- `impl Trait for Type` 给类型实现接口。
- 固有方法默认模块私有，跨模块要 `pub fn`。`pub trait` 的方法全部公开；`impl Trait for Type` 不能改方法可见性。
- 方法调用默认静态分发、单态化、可内联。
- 只有写成 `dyn Print` 才走 vtable。哪些 trait 能 `dyn` 见 [类型](types.md)。
- 语言可以对 `dyn Trait` 写固有 impl（`impl dyn Any`）。用户不能给 `dyn Trait` 写固有方法。
- 关联类型与关联常量都走 `::`：`Iter::Item`、`Dim::N`。在 impl 里写 `type Output = Point`、`const N = 4`；在签名里用 `Self::Output` / `Self::N`，不要靠裸名。

`self` 的类型可以写 `self`、`self: Point`、`self: &Point`。按值 `self` 拷贝接收者；`&Point` 按引用。句柄类型（`Vec`）的方法接 `self` 就是拷贝句柄，能 `push` 到同一块载荷。大位结构体应接 `&Self`。

```
impl Dim for [int; 4] {
    const N = 4
}

impl Add[Point] for Point {
    type Output = Point
    fn add(self, rhs: Point) Point = ...
}

impl Print for Foo[T] {
    fn print(self, buf: &Vec[byte]) { ... }
}

impl Print for Foo[string] {
    fn print(self, buf: &Vec[byte]) { ... }
}
```

## 约束

```
fn dump[T: Print](x: T) {
    let buf = Vec::new()
    x.print(&buf)
}
```

多个约束用 `+`：`T: Print + Clone`。可调用约束写成 `F: Fn(int) int`：与函数类型同形，但 `Fn` 是内建约束（单态化），`fn(int) int` 是擦除句柄类型，二者不要混用。没有 `where` 子句。

没有约束的泛型参数在用到不存在的方法时必须在定义处或实例化处报错，禁止静默生成错误代码。

## 特化

允许重叠 impl，用**最具体者获胜**：

```
impl Bar for Foo[T] { ... }
impl Bar for Foo[string] { ... }
```

`Foo[string]` 比 `Foo[T]` 更具体，`string` 走特化，其它 `T` 走通用 impl。

具体规则：

1. 闭世界：编译器看见全部 impl。不需要 Rust 孤儿规则来保证连贯性；用户可以给 `int` 实现自己的 trait，也可以给自己的类型实现 `std` 的 trait。
2. 两个 impl 都能匹配时，必须能比较具体性：替换次数更少、有更多具体类型构造器的更具体（C++ 部分序那种直觉）。
3. 无法比较（交叉重叠，例如 `impl Trait for Foo[T: A]` 与 `impl Trait for Foo[T: B]`，而某类型同时是 A 和 B）必须**编译错误**，不能靠声明顺序。没有单独的 `where` 子句；约束写在 `[T: Bound]` 里。
4. 特化可以改方法体，不能改方法签名、关联类型与关联常量（不一致是错误）。
5. `dyn Trait` 的 vtable 按**该具体类型选中的最具体 impl** 生成。
6. 否定 impl 见下。

特化是类型系统的一部分，不是优化提示。

## 否定 impl

```
impl !Clone for chan[T] {}
impl !Clone for Join[T] {}
```

- 体必须为空。表示 `Self` **不得**实现该 trait。
- 与指向同一 `Self`（经替换后）的肯定 impl 并存是编译错误。
- 比它更泛的肯定 blanket 被它**挖掉**：`impl[T] Clone for Foo[T]` 与 `impl !Clone for Foo[chan]` 时，`Foo[chan]` 不实现 `Clone`，其它 `Foo[T]` 走肯定 impl。具体性规则与特化相同；挖不干净（交叉重叠）仍是硬错误。
- `T: Clone` 在否定 impl 匹配时不成立，错误必须指出否定 impl 的位置。
- 语言义务：`chan[T]`、`Join[T]` 必须有 `impl !Clone`。用户不能再给它们写肯定 `Clone`。语言对 `!`、`MaybeUninit[T]` 写 `impl !Any`；用户不能给其它类型写 `impl !Any`，也不能给 `!` / `MaybeUninit` 写肯定 `Any`。
- 否定 impl 不进 `dyn` vtable。不能写 `dyn !Clone`。

「不写 impl」仍然表示「没有肯定实现」；否定 impl 用来对抗 blanket 和把「禁止实现」写成闭世界事实。
