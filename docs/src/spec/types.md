# 类型系统

## 原则

1. 类型在编译期健全。没有 `any`，没有未检查的安全强制转换。
2. 每个值在给定程序点有唯一静态类型。推断只省略书写。
3. 复合类型的大小、对齐、字段偏移在给定目标上编译期已知。
4. 泛型默认单态化。类型擦除必须显式（胖 `fn`、接口对象）。
5. `pub` 项必须有完全写明的参数类型；非 `()` 的返回类型必须写出（含 `!`）。函数体内部激进推断。
6. 禁止隐式数值拓宽 / 变窄。仅允许 `!` 合流、数组引用到切片以及函数项/闭包到擦除函数句柄这三类规范强制，见本章“类型形成、推断与转换”。
7. ZST 合法。ZST（零大小类型）指 `size_of` 为 0 的类型：`()`、空结构体、`[T; 0]`、字段递归全是 ZST 的结构体。**纯 ZST** 就是这种没有堆对象身份的零大小值，不在 GC 堆上分配对象头。句柄类型（`Vec`、`chan`、`dyn Trait`）即使载荷为空也不是 ZST。
8. 没有 `null`。缺值用 `Option[T]`。`&T` 永不为空。

## 泛型写法：`[T]`，不用 `<T>`

泛型参数和实参一律方括号：`Vec[T]`、`fn id[T](x: T) T`、`impl Foo[T]`、`Foo[string]`。不用尖括号。

不用 `<T>` 的原因：表达式里 `f<T>(x)` 会先被看成小于号；`Map<K, Vec<V>>` 还有 `>>`。那是 C++ / Rust 才需要 turbofish `::<>` 的根源。方括号没有这个问题。

和数组 / 下标的区分：

| 写法 | 含义 |
|------|------|
| `Vec[int]` | 泛型实参。出现在类型位置，或类型路径里：`Vec[int]::new()` |
| `Block[int, 4] { ... }` | 泛型结构体字面量。实参贴在类型名上，与 `Vec[int]::new()` 同一规则 |
| `[int; 4]` | 数组类型，**必须有分号和长度** |
| `&[int]` | 切片类型，以 `&[` 开头 |
| `[1, 2, 3]` | 数组值 |
| `x[i]` | 下标。`x` 是值 |
| `id::[int](x)` | 表达式里给泛型**函数**或**方法**显式传类型实参（`::` + `[...]`）。普通 `id(x)` 靠推断 |

禁止把数组类型写成 `[int]`（没有长度）。因此 `Foo[int]` 不会和数组类型撞车。

剩下的歧义是 `foo[bar]()`：既可能是「下标再调用」，也可能是「泛型函数」。规则：**值后面的 `[]` 永远是下标**；要显式实例化函数，必须写 `foo::[Bar]()`。类型名后面的 `[]` 永远是泛型实参（`Vec[int]::new()`）。

关键字构造器例外：`chan[T](n)`、`size_of[T]()`、`align_of[T]()`、`offset_of[T](field)`、`type_id[T]()` 仍允许，因为这些名字是关键字，不可能是下标目标。`offset_of` 的 `field` 是字段名（具名标识符或 `0`），不按值表达式求值。`type_id_count()` 无类型实参。

类型参数包用于异构变参：`fn println[Ts: Print...](...args: Ts)`。`Ts...` 只出现在泛型参数列表里；`Ts: Print...` 表示每个展开后的类型都实现 `Print`。在调用点按实参列表展开并单态化。

`[]` 参数表可以混写类型参数与 `comptime` 参数，适用于 `fn`、`struct`、`enum`、`union`、`trait`、`impl`、`type`：

```
struct Block[T, comptime N: int] {
    data: [T; N]
}

impl[T: Clone, comptime N: int] Clone for [T; N] {
    fn clone(self: &Self) [T; N] { ... }
}
```

`comptime` 参数必须是编译期已知的值（日常是 `int`）。出现在数组长度、`repr(align(N))`、`match` 范围端点等处。类型实参与 comptime 实参都写在同一对 `[]` 里，贴在类型名上：`Block[int, 4] { data: [0; 4] }`。禁止 `Block::[int, 4] { ... }`：`::[...]` 只给泛型函数与方法，见上表。函数的 comptime 形参仍可写在参数表里：`fn repeat[T](comptime n: int, x: T)`。禁止另搞一套 const 泛型尖括号。

## 内置标量

| 名字 | 含义 | x86_64 大小 |
|------|------|-------------|
| `byte` / `u8` | 同一类型，无符号 8 位 | 1 |
| `i8` `i16` `i32` | 有符号精确宽度 | 1 / 2 / 4 |
| `int` / `i64` / `isize` | 同一类型，有符号 64 位（tier-1 指针宽） | 8 |
| `i128` | 有符号 128 位 | 16 |
| `u16` `u32` | 无符号精确宽度 | 2 / 4 |
| `uint` / `u64` / `usize` | 同一类型，无符号 64 位 | 8 |
| `u128` | 无符号 128 位 | 16 |
| `bool` | 不是整数 | 1 |
| `char` | Unicode 标量（不是 UTF-16，不是 `byte`） | 4 |
| `f32` | IEEE 754 binary32 | 4 |
| `float` / `f64` | 同一类型，binary64 | 8 |

日常代码用 `byte`、`int`、`uint`、`float`、`bool`、`char`、`string`。精确宽度（含 `i128` / `u128`）与 `f32` 用于布局、SIMD、FFI，是独立的一等类型（可作泛型实参、可写约束），不是 C 那种随时可替换的 typedef。

下列各组是**同一个类型的多个名字**（不是弱别名，也不能分成两个类型分别 `impl`）：

| 同一类型 |
|----------|
| `byte` ≡ `u8` |
| `int` ≡ `i64` ≡ `isize` |
| `uint` ≡ `u64` ≡ `usize` |
| `float` ≡ `f64` |

`isize` / `usize` 在 tier-1（本规范钉死的两个支持目标，见 [程序与编译模型](program-model.md)）上与 `int` / `uint` 同宽，因此并进上表。`i8`、`i16`、`i32`、`i128`、`u16`、`u32`、`u128`、`f32` 各是不同类型。`i128` / `u128` 在 x86_64 上对齐 16。

标量之间禁止隐式转换。显式转换写成类型构造：`int(b)`、`byte(n)`、`float(i)`、`char(u)`、`i128(x)`、`u128(x)`。整数变宽按符号或零扩展；整数变窄保留目标位宽的低位，按二进制补码解释结果。整数转浮点按 IEEE 舍入到最近、偶数优先；浮点转整数先向零截断，NaN、无穷或截断后超出目标范围时 panic。`char(u)` 的 `u` 必须是 Unicode 标量值，否则 panic。`bool` 不能与整数互转。`string` 与字节的转换走标准库并校验 UTF-8。

`i128` / `u128` 可以出现在 Gugu 函数、结构体、数组里。它们**不是**日常类型。`extern "C"` 的签名里：`x86_64-linux` 按 SysV `__int128`（`rdx:rax`）；`x86_64-windows` 的 C ABI 没有 `__int128`，把 `i128`/`u128` 写进 `extern "C"` 是编译错误。要过 Windows C 边界，拆成两个 `u64` 或 `#[repr(C)]` 结构体。不要抄 Rust 自己的 `i128` 传参。

整除 `/` 向零截断，`%` 的符号与被除数相同。除零一律 panic；最小有符号整数除以 `-1` 得到原最小值，余数为 0。负移位量 panic；非负移位量先按操作数位宽取模。左移丢弃移出的高位并按位宽环绕；有符号右移复制符号位，无符号右移补零。

运行时整数加、减、乘和整数负号都按目标位宽二进制补码环绕，不存在构建模式相关的溢出检查。comptime 求值时溢出仍是编译错误，不环绕。浮点溢出产生 IEEE inf / NaN，不 panic。

一元 `-` 只用于有符号整数与浮点。对无符号类型写 `-x` 是编译错误。按位取反是前缀 `~`，只用于整数。

`==` `!=` `<` `<=` `>` `>=` 对内置标量、`string`、`TypeId`、由它们组成的元组与数组，由编译器直接实现，不走 trait。浮点按 IEEE 754（`NaN != NaN`，涉及 NaN 的序比较为 false）。用户 `struct` / `enum` 没有默认同等；要写 `==` 必须 `#[derive(Eq)]` 或手写 `impl Eq`。内置整数与浮点的算术同样由编译器直接降指令；语言同时给出对应的 `Add` 等 impl，使 `T: Add` 能用在 `int` 上。`string` 的 `+` / `+=` 走 `Add` / `AddAssign`，见下。

## 引用 `&T`

共享写成显式 `&T`。传递规则（`f(x)` 产生语义副本、`f(&x)` 引用槽、身份/COW/resource 句柄）见[值、句柄与传递](passing.md)。

- `&T` 是指向某个绑定或字段槽的引用，永不为空。拷贝 `&T` 只拷地址。
- 绑定默认可变，通过 `&T` 可以改槽里的 `T`（没有单独的 `&mut T`）。
- 引用逃逸则装箱到 GC 堆，不报生命周期错误。
- 可空：`Option[&T]`。
- 原始指针 `*T` 见 [unsafe](unsafe.md)。

## `string`

`string` 是合法 UTF-8 的可变 COW 值。短文本可以内联；较长文本共享 backing。赋值、按值传参、返回和模式绑定得到语义独立的 string：真实复制或持久快照把 backing 单向密封，之后修改任一值会分离，不影响其它副本。

`len()`、capacity、range 与修改位置都按 byte 计。string 不支持单整数 `s[i]`；读取使用 `byte_at` / `char_at` 或迭代器。`s[a..b]` 等 range 返回 O(1) COW 快照，端点必须位于 UTF-8 scalar 边界，否则 panic。`==`、顺序与 Hash 按原始 UTF-8 byte 序列工作，不隐式 normalization。

`+` 返回新 string；`+=` 修改左侧并可以复用未密封 backing。完整固有接口、`Bytes` 快照、密封状态与编译器 borrow/transfer 优化见[标准库 · 可变 COW string](standard-library.md#可变-cow-string)。

## 元组、数组、切片

- 元组：匿名积类型。`(T, U)` 结构等价即同一类型。`()` 是 unit（恰好一个值，ZST）。
- 数组 `[T; N]`：长度属于类型，内联，不经堆。按值拷贝。`N` 必须是编译期整数。值：`[1, 2, 3]` 类型 `[int; 3]`；`[x; N]` 把 `x` 重复 `N` 次（`N` comptime）。
- 切片 `&[T]`：胖指针（指针 + 长度），不拥有存储。`&[T; N]` 必须能强制成 `&[T]`。
- 下标：`xs[i]`、`xs[a..b]`、`xs[a..]`、`xs[..b]`、`xs[..]`。不完整区间 `a..` / `..b` / `..` **只**允许出现在 `[]` 里，不能当独立值（独立值必须是 `a..b`，类型 `Range`）。
- `Vec[T]` 不是内置类型，在 `std`（预导入）：可增长、存储在 GC 堆或显式传入的 `LocalArena` / `SyncArena`。至少提供 `new`、`push`、`len`、`cap`（`len` / `cap` 返回 `int`），以及 `IntoIter`（`Item = T`，拷贝元素）。必须实现 `Clone`（`T: Clone`）、`Eq`（`T: Eq`）、`Print`。

下标类型是 `int`。语言层默认插入越界检查；发布构建也检查。编译器在**证明** `0 <= i < len` 时必须删掉该检查（循环归纳、`if` 守卫、`for i in 0..n`、comptime 已知长度与下标）。证明不了就留检查。`unsafe` 提供无检查下标。

`[T; N]`、`&[T]`、`Vec[T]` 的下标是语言内置，不走 `Index` trait。`xs[i]` 作为右值拷贝出 `T`；作为左值 `xs[i] = v` 写入该槽。string 不支持单整数下标，只支持规范规定的 range 快照。用户类型的 `[]` 见 [接口](traits.md) 的 `Index`。

## never：`!`

`!` 是 never 类型：零个值、ZST。与 unit `()` 不同——`()` 有恰好一个值；`!` 表示「没有下一条指令」。

只出现在**类型位置**（返回类型、`: !`、泛型实参 `Foo[!]`、`fn() !`）。表达式里的前缀 `!` 仍是逻辑非；`!=` 仍是一个记号。

没有值可以合法地构造出来。下列表达式的类型是 `!`：

- `return` / `return expr`
- `break` / `break expr` / `continue`
- 无 `break` 的 `loop { ... }`
- `panic(...)`、`std.process.exit(...)`，以及规范标明返回 `!` 的 intrinsic（如 `unreachable`）

`!` **可以合流到任何类型**，运行时不会产生 `!` 的值。数组引用到切片、函数项/闭包到擦除函数句柄是另外两类不执行用户代码的表示强制；除此之外没有隐式类型转换。合流规则：

- 两臂都是 `T` → `T`
- 一臂 `!`、一臂 `T` → `T`
- 两臂都是 `!` → `!`

因此 `fn parse() int { panic("bug") }` 合法。`if` / `match` / `select` / `if let` 用作表达式时按上面合流。

函数类型 `fn() !` 与 `fn() ()` 不是同一类型。`fn() !` 可以强制成 `fn() T`（调用侧永远看不到返回值）。反过来不行。

## 结构体、枚举、newtype

名义类型。字段默认私有，`pub` 单独打开。允许空结构体（ZST）。禁止运行时加字段。

除具名字段结构体外，允许**恰好一个字段**的元组结构体（newtype）。多字段禁止位置构造（避免和函数调用、元组变体混淆）。

```
struct Point {
    pub x: int
    pub y: int
}

struct Meters(int)
struct Id(pub uint)

enum Result[T, E] {
    Ok(T)
    Err(E)
}

enum Option[T] {
    Some(T)
    None
}
```

`Meters(3)` 构造；`m.0` 读写内部值（可见性与字段相同：默认私有，`pub` 打开）。`type Meters = int` 仍是透明别名，不能单独 `impl`；`type Foo = impl Trait` 是不透明别名。名义包装用 newtype。`#[repr(transparent)]` 见本章「布局与 size_of」。

枚举是标签联合。`match` 必须穷尽。编译器可以做 niche 压缩；`#[repr(C)]` / `#[repr(u8)]` 关掉压缩供 FFI。

`Result` 与 `Option` 在预导入中。`?` 的操作数必须实现内建 `Try`（见 [接口](traits.md)）。标准库的 `Result` 与 `Option` 实现 `Try`；用户类型实现了 `Try` 就能用 `?`，没有其它重载入口。`?` **没有**错误类型的隐式转换：出口处（`try` 块或函数返回类型）的 `Try::Error` 必须与操作数相同。

枚举变体三种：无载荷 `A`、元组式 `B(int, string)`、结构体式 `C { x: int, y: int }`。变体与枚举同可见性，不能给单个变体单独标 `pub`。`#[repr(u8)]`（或其它整数 repr）时允许 `A = 1` 写判别值；无 `#[repr]` 时禁止显式判别值（编译器可做 niche）。

递归类型的字段必须是句柄或 `&T`，不能把无限大的 `Node` 嵌进 `Node`。典型写法：`enum List[T] { Nil, Cons(T, &List[T]) }`，或用 `Vec[Node]` 这种句柄字段。构造逃逸的 `&T` 会升到 GC 堆，见 [传递](passing.md)。

空枚举（零个变体）合法，与 `!` 一样没有值。`match` 穷尽性：不可达的变体（载荷是 `!` 或空枚举）不必写臂。`Result[T, !]` 只写 `Ok(v)` 即穷尽。

## 函数类型

类型位置写成 `fn(参数类型列表) 返回类型`，没有参数名，返回类型在 `()` 后用空格，与具名函数同一套：

```
fn(int) int
fn(int, string) bool
fn()                 // 即 fn() ()
fn() !
```

闭包字面量（表达式）是 `fn(x: int) int { ... }`，见 [函数与闭包](functions.md)。

- 每个闭包字面量有**独特匿名类型**，实现对应的 `Fn(T) U`，供单态化与内联。
- 写成类型 `fn(T) U` 是擦除句柄（代码 + 环境），间接调用。这是句柄：拷贝即共享环境。
- 具名函数与闭包都可强制成 `fn(T) U`。
- 热路径泛型用约束 `F: Fn(T) U`，不要把参数写成 `fn(T) U` 除非你就是要擦除。
- 不想写出类型参数名时用 `impl Trait`，见下。

## `impl Trait`

`impl Trait`（可写 `impl Print + Clone`）出现在类型位置时表示「某个实现了这些约束的具体类型」，**单态化，不是 `dyn`**。

| 位置 | 含义 |
|------|------|
| 参数（APIT） | `fn dump(x: impl Print)` 等价于 `fn dump[T: Print](x: T)`，该 `T` 在函数体内不能当类型名写。每个 `impl Trait` 出现是独立类型参数。`&impl Print` 等价于 `&T`（`T: Print`）。 |
| 返回（RPIT） | `fn adder(n: int) impl Fn(int) int { fn(x) = x + n }`。返回的是**隐藏的具体类型**（此处即该闭包的匿名类型），调用方可单态化、可内联，不能写出该类型的名字。 |
| `type` 别名（TAIT） | `type Adder = impl Fn(int) int`。这是**不透明**别名：与透明的 `type Ids = Vec[int]` 不同。同一 `type Adder` 在整个模块里指同一个隐藏类型。可出现在字段、参数、返回里。 |

规则：

- 同一签名里两个 `impl Print` 是两个类型，不必相同。
- 函数体必须能推到**唯一**隐藏类型；推不出或有两个候选则编译错误。
- 隐藏类型必须满足写出的全部约束。关联类型通过约束里的绑定不另设语法（需要关联类型时写具名泛型或 TAIT）。
- `pub` 函数可以返回 `impl Trait`；对外仍不能点名隐藏类型。
- trait 方法可以返回 `impl Trait`（RPITIT）。该 impl 对所有实现者可以是不同的具体类型；调用方仍只能当不透明类型用。
- 禁止：`extern "C"` 签名、`union` 字段、`dyn impl Print`、`impl Trait` 当 `chan`/`Vec` 的类型实参（必须先 TAIT 起名）。
- 与 `dyn Trait` 对照：`impl` 单态、零虚表；`dyn` 擦除、胖指针。不要混。

```
fn twice(f: impl Fn(int) int, x: int) int = f(f(x))

type Cmp = impl Fn(int, int) bool
fn make_cmp() Cmp = fn(a, b) = a < b
```

## 接口对象

`dyn Trait` 是胖指针：数据 + vtable。这是**显式擦除**。`dyn` 是关键字。泛型默认仍然单态化，不经过 `dyn`。

只有同时满足下列条件的 trait 才能写成 `dyn Trait`：没有关联类型、没有关联常量、没有泛型方法、没有返回 `impl Trait` 的方法、方法接收者是 `self` 或 `self: &Self`。`Print` 可以 `dyn`；`Add`（有 `Output`）不可以。`Any` 可以 `dyn`（见下）。

`T` 实现 `Trait` 时，`T` 的值可以强制成 `dyn Trait`（分配或复用堆对象，胖指针）。这是显式擦除。

## `TypeId` 与 `dyn Any`

闭世界一次编译能枚举全部单态化后的具体类型，因此类型身份是**稠密编号**，不是哈希。预导入类型 `TypeId` 布局与 `u32` 相同（大小 4、对齐 4），取值范围 `0 .. type_id_count()`。同一镜像内比较是一次整数运算，`as_int()` 后可作数组下标。没有碰撞。重新编译可以重排编号；禁止跨镜像、跨进程拿 `TypeId` 当稳定密钥。插件只走 C ABI，对岸没有 Gugu `TypeId`。`TypeId` 不能出现在 `extern "C"` 签名里；过边界传 `as_int()` 的值或 `u32`。

这不是开世界反射，也不是公理禁止的渐进类型 `any`。运行时擦除仍然只有显式的 `dyn Trait`。

关键字构造器（与 `size_of` 相同，不是下标）：

```
type_id[T]()          // TypeId，comptime（`T` 在该单态里已知）
type_id_count()       // int；具体类型集合冻结后的 comptime 常量
```

`type_id_count()` 不能参与数组长度、类型/布局形成、comptime 泛型实参、`cfg`、可达性或任何可能新增具体类型的求值；这些位置使用它是编译错误。它可以初始化不改变类型形状的标量常量、控制运行时循环，或填充长度已由其它常量确定的元数据。编号个数放不进 `int` 或内部 `u32` 是编译错误。

### 谁有编号

本次编译中每一个单态化后的具体类型（含只出现在 `type_id[T]()` 里的类型）恰好一个编号，包括：标量、元组、数组、切片、结构体、枚举、union、newtype、每个闭包匿名类型、每个函数项类型、胖 `fn` 签名、`dyn Trait`、句柄类型的每个单态（`Vec[int]` 与 `Vec[string]` 不同）。

- 透明 `type` 别名与原类型同一 `TypeId`。
- newtype（即使 `#[repr(transparent)]`）与内部类型编号不同。
- TAIT / RPIT / APIT：`type_id[Adder]()`（`Adder` 是 `type Adder = impl Trait`）等于**隐藏的具体类型**的编号。两个不透明别名若隐藏同一具体类型，编号相同。
- `!` 与 `MaybeUninit[T]` **没有** `TypeId`。`type_id[!]`、`type_id[MaybeUninit[int]]` 是编译错误。

没有从整数构造 `TypeId` 的安全 API。`transmute` 出越界编号再拿去索引类型表是未定义行为。

`TypeId` 的 `==` / `!=` / 序比较由编译器按编号直接实现，并实现 `Eq`、`Ord`、`Print`（打印规范类型名）。固有方法：

```
fn as_int(self) int          // 稠密下标，`0 <= i < type_id_count()`
fn name(self) string         // 镜像 rodata 里的 intern 字符串；`type_id[T]().name()` 仍是 comptime
```

`name` 供诊断与调试，不能用来改布局、不能当求值入口。规范名优先日常名字：`int` 不是 `i64`，`uint` 不是 `u64`，`byte` 不是 `u8`，`float` 不是 `f64`。

### `Any` 与 downcast

`Any` 是 lang trait（编译器按名字挂钩的 trait，见 [概述 · 术语](overview.md#术语)），**不能有泛型方法**（否则不能 `dyn`）。用户不能声明、不能手写 `impl Any`，也不能 `impl !Any` 挖掉语言生成的肯定 impl。语言自己写：

```
trait Any {
    fn type_of(self: &Self) TypeId
}

impl !Any for ! {}
impl !Any for MaybeUninit[T] {}
```

其余拥有 `TypeId` 的类型由编译器生成 `impl Any`：`type_of` 就是 `type_id[Self]()`。方法名不能叫 `type_id`，那是关键字。

`dyn Any` 合法。把 `x: T`（`T: Any`）强制成 `dyn Any`：分配一个 GC 对象，并按 T 的值描述符把 x 写入载荷；vtable 带载荷 TypeId，类型表带 GC、COW 与 resource descriptor。身份句柄进盒子时共享对象，string backing 先密封，resource 字段建立盒子持有的 lease。因此 `dyn Print` 再进 `dyn Any` 后，downcast 只能回到 `dyn Print`，不能穿过接口对象猜到原来的 Point。

若 x 已经是 `dyn Any`，强制只复制胖指针，不再套一层盒子。ZST 的盒子没有载荷 byte；实现可以共用一个不移动的永生对象。

泛型方法不能放进 `Any` trait。语言给类型 `dyn Any` 写固有 impl（不能用户重载）：

```
impl dyn Any {
    fn is[T: Any](self: &Self) bool
    fn downcast[T: Any](self: &Self) Option[&T]
    fn downcast_copy[T: Any](self: &Self) Option[T]
}
```

三者只比较载荷 `TypeId` 与 `type_id[T]()`。相等则 `downcast` 的 `&T` 指向载荷槽，`downcast_copy` 按 T 的值描述符产生语义副本；不等则 `None`，不 panic。`&T` 指向盒子内部：只要该引用或 dyn Any 句柄还是 GC 根，盒子活着。

没有「downcast 成另一个 `dyn Trait`」。要接口对象，直接用 `dyn Print` 擦除。

```
let a: dyn Any = Point { x: 1, y: 2 }
if let Some(p) = a.downcast::[Point]() {
    p.x += 1
}
let q: Option[&Point] = a.downcast()   // 靠期望类型推断 T
```

方法上的显式类型实参必须写 `::[T]`（值后面的 `[]` 是下标）。能推出来就不写。

镜像 rodata 有一张以编号为下标的类型表（名字、`size_of`、`align_of`、GC 扫描描述符）。`downcast` 是一次整数比较，不是哈希探表。不能在运行时登记新类型。

## 语言类型

这些由编译器认识（是 lang item：编译器按名字挂钩，用户不能自己再定义同名类型）。除 `MaybeUninit` 与 `!` 外预导入（见 [声明](declarations.md)）。`MaybeUninit` 在 `std.mem`。`Panic` 的字段由标准库给出，见 [运行时](runtime.md)。

| 类型 | 含义 |
|------|------|
| `chan[T]` | 通道。关键字 `chan`。句柄，权威状态在堆对象上。构造 `chan[T](n)`，`n: int`。 |
| `Join[T]` | `async` 启动的协程的句柄。`wait()` 得到 `Result[T, Panic]`。 |
| `Range` | `a..b` 作为**值**时的类型。`pub start: int`、`pub end: int`，半开。只表示 `int` 区间。模式里的 `char` 范围不产生 `Range` 值，见 [模式](patterns.md)。 |
| `ChanClosed` | 空结构体（无字段；值可写 `ChanClosed` 或 `ChanClosed {}`，与 [声明](declarations.md) 的 `Empty` 相同）。`recv` 在关闭且收尽后返回 `Err(ChanClosed)`。 |
| `TrySendErr` | `enum TrySendErr { Full, Closed }`。`try_send` 的 `Err`。 |
| `TryRecvErr` | `enum TryRecvErr { Empty, Closed }`。`try_recv` 的 `Err`。 |
| `Panic` | 标准库结构体（预导入 lang item）：`message: string`、`location: Location`（`Location` 在 `std.src`）。字段定义见 [运行时](runtime.md)。`#[must_use]` 不适用：它是数据，不是「未处理的结果」。 |
| `TypeId` | 闭世界稠密类型编号。见上。预导入。 |
| `MaybeUninit[T]` | 可能未初始化的 `T`。布局与 `T` 相同，GC **不**把其中的引用当活根，直到 `assume_init`。见 [unsafe](unsafe.md)。 |
| `!` | never，见上。不是预导入名字，是记号。 |

`for i in 0..n` 走语言提供的 `Range` 的 `IntoIter`，不是用户写的 impl。

`Result[T, E]` 与 `Option[T]` 带 `#[must_use]`：丢掉未使用的值是 lint `unused_must_use`。`Join[T]` **不**带：丢掉即分离。`Option` / `Result` 在元素满足约束时必须实现 `Clone`、`Eq`、`Print`；元素都 `Ord` 时实现 `Ord`。

## 布局与 `size_of`

无 `#[repr]` 时编译器可重排字段与压缩枚举，但在「同一目标 + 同一编译器版本」上确定。跨语言用 `#[repr(C)]`。

额外 repr：

- `#[repr(u8)]` 等整数 repr：枚举判别值，见上。
- `#[repr(packed)]`：去掉字段间填充；所有字段必须是位类型，禁止句柄、引用、`string`、切片、胖函数和 `dyn Trait`。若字段的自然对齐大于 1，不能形成或解引用它的 `&T`；必须在 `unsafe` 中用 `std.ptr.addr_of(field)` 取得原始地址，再调用 `read_unaligned` / `write_unaligned` 或按字节复制，见[unsafe](unsafe.md)。
- `#[repr(transparent)]`：结构体或 newtype 恰好一个非 ZST 字段（其余必须是 ZST）。与那一字段同一 ABI 与布局。供 FFI newtype。
- `#[repr(align(N))]`：`N` 是 comptime 二的幂。类型对齐至少为 `N`。可与 `C` / `transparent` / `packed` 组合（`packed` 与 `align` 同时出现时，对齐取 `N`，字段仍紧排）。

GC 堆对象可以有用户不可见的头；`#[repr(C)]` 的 FFI 结构体默认不当作可移动 GC 对象传递给 C，除非显式 pin，见 [内存](memory.md)。

下列为语言关键字构造，必须在 comptime 可求值（`T` 与字段名编译期已知）：

```
size_of[T]()         // int，字节
align_of[T]()        // int
offset_of[T](field)  // int。具名字段写标识符；newtype / 元组写 `0`
type_id[T]()         // TypeId，见上；`!` / `MaybeUninit[_]` 非法
type_id_count()      // int；类型集合冻结后求值，不能参与类型形成
```

`union` 的 `offset_of` 对每个字段都是 0。`!` 与 `()` 的 `size_of` / `align_of` 都是 0 与 1。`size_of[TypeId]()` 是 4，`align_of[TypeId]()` 是 4。

## 类型形成、推断与转换

类型检查先验证类型本身是良构的，再在每个表达式处建立类型约束。名义类型只与自身和透明别名相等；元组按成员序列、数组按元素类型与长度、函数类型按参数和返回类型、引用/指针按被指类型进行结构相等判断。`dyn Trait`、newtype、枚举、结构体和不同的泛型实例都不是彼此的结构别名。

局部推断采用双向规则：期望类型从绑定注解、参数类型、返回类型、字段类型、分支合流和调用约束向表达式传入；表达式的已知类型向外产生约束。一个 `let`、数组、元组、结构体或调用完成检查时，所有类型变量必须被唯一确定；无法确定或有多个候选是编译错误，不会生成动态类型。函数、trait、impl 的公共签名不能依赖调用点才能确定的匿名局部类型。

整数和浮点字面量先保持为对应的未定型字面量。若期望类型是整数或浮点，就按该类型检查范围；没有期望类型时整数默认为 `int`，浮点默认为 `float`。默认化发生在表达式检查结束时，不能把两个不同整数类型通过默认化合并。数组元素、元组成员、结构体字段和枚举载荷必须分别满足声明的类型。

语言只有以下隐式转换或强制：`!` 到任意类型；`&[T; N]` 到 `&[T]`；函数项和闭包到兼容的擦除函数句柄 `fn(...) ...`。这些转换不执行用户代码、不改变载荷语义。所有数值转换、`string`/字节转换、newtype 构造和 `dyn Trait` 擦除必须写出明确的构造或上下文允许的接口擦除；数值之间绝不隐式拓宽或变窄。

期望类型相同且两个分支分别为 `T` 和 `!` 时结果为 `T`；两个分支都是 `!` 时结果为 `!`；除此之外分支、数组元素、返回值和调用实参不做共同超类型推断。用户类型的运算符候选由 trait 解析，不能通过自动转换把一个候选变成另一个候选。

未初始化状态不是一种可用于推断的类型。声明后赋值的槽必须在每个到达读取点的路径上已写入，且写入类型与声明类型相等；编译器不能以运行时分支概率或 panic 作为初始化证明。

## `impl Trait` 的隐藏类型

参数位置的 `impl Trait` 为该次出现引入独立的隐式类型参数和约束；同一签名中的两个 `impl Print` 不要求相同。该类型参数只在函数体的类型检查中作为未知具体类型使用，不能用名字构造，也不能在运行时查询其隐藏身份之外的布局。

返回位置的 `impl Trait` 为该函数（以及其泛型单态）确定一个唯一的隐藏具体类型。所有正常返回表达式必须是同一具体类型，`!` 可以合流；多个不相同的候选即编译错误。隐藏类型必须满足列出的全部 trait 约束，并继承该函数捕获的泛型参数。调用方可以继续把返回值传给满足约束的泛型或擦除成 `dyn Trait`，但不能书写隐藏类型名称。

`type Name = impl Trait` 是 TAIT。每个 TAIT 在声明模块内只有一个确定隐藏类型的定义点；定义点可以是返回 `Name` 的函数体或初始化 `Name` 的常量，所有其它使用只消费已经确定的类型。没有定义点、存在两个不一致定义点、定义点不能满足约束或形成未允许的递归大小，都是编译错误。TAIT 可以出现在字段、参数和返回类型中，但不能出现在 `extern "C"` 签名、union 字段或另一个未命名 `impl Trait` 的类型实参中。

trait 方法的返回 `impl Trait` 是 RPITIT；每个实现可以选择不同的具体返回类型，但每个实现内必须唯一并满足方法声明的约束。trait 对象不能调用依赖未暴露具体返回类型的 RPITIT 方法，因此含有此类方法的 trait 不能形成 `dyn Trait`。

## `dyn Trait` 与方法解析

`dyn Trait` 只能由满足该 trait 的具体值显式擦除得到。擦除产生数据指针和 vtable 句柄；vtable 固定该具体类型的最具体肯定 impl。`dyn Trait` 的复制只复制胖指针，不复制载荷；若 trait 没有适用 impl 或存在无法比较具体性的 impl，程序在编译期失败。

可对象化 trait 除已有规则外，还必须满足：除接收者外的方法参数和返回类型不能出现 `Self`；方法不能要求按值返回未知大小的 `Self`；不能有泛型方法、关联类型、关联常量或 RPITIT。`self` 和 `self: &Self` 是允许的接收者；`self: &Self` 的调用不移动对象。违反任一条件的 trait 仍可用于泛型约束，但不能写 `dyn Trait`。

方法调用按以下顺序解析：先确定接收者静态类型，检查该类型定义模块内的固有方法；若唯一匹配则使用它；否则收集当前可见且约束满足的 trait 方法。只有一个候选时调用成功，零个或多个候选都是编译错误。自动解引用只用于接收者和字段访问，不用于普通函数参数；方法重载不能通过参数类型区分，同名候选必须靠固有优先、trait 路径或显式 UFCS 消歧。

## 类型良构的边界

数组长度、`repr(align(N))`、枚举判别值、`offset_of` 和所有 `comptime` 类型参数必须在类型检查结束前确定且为合法 `int` 值。递归类型沿字段展开时必须最终经过句柄、引用或其它有界间接层；直接递归形成无限大小是编译错误。

`pub` 签名中使用的类型、trait 约束、关联类型和 TAIT 必须从该项的可见接口可解析；私有模块中的未命名闭包类型只能出现在内部单态化上下文。类型检查不依赖优化、目标机器当前寄存器状态或运行时输入。
