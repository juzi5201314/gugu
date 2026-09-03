# 函数与闭包

## 一等函数

函数与闭包是值：可传、可返回、可进字段。可调用的是：具名函数、闭包、类型 `fn(T) U`。没有「可调用对象」体系。

## 具名函数

无捕获。表达式体与块体见 [声明](declarations.md)。允许递归与相互递归。禁止按签名重载；多态走泛型、`impl Trait`、特化、枚举。参数与返回里的 `impl Trait` 见 [类型](types.md)。

## 参数传递

见[值、句柄与传递](passing.md)。`f(x)` 按值传递规则产生语义副本，`f(&x)` 引用槽。普通函数不自动取地址；方法接收者是 `&Self` 时 `p.m()` 可以插 `&p`。关联函数写 `Type::m()`，见[接口](traits.md)。

## 变参

见 [表达式](expressions.md)：齐次 `...xs: &[T]`，异构 `fn println[Ts: Print...](...args: Ts)`。

## 闭包

闭包是没有名字的函数字面量，语法与具名函数同一套，只是 `fn` 后面直接跟参数表。不用 `|x|`（和按位 `|` 打架，也多一套语法）。

```
let add = fn(x: int, y: int) int = x + y
let add = fn(x: int, y: int) int { x + y }

let inc = fn(x: int) int = x + n     // 捕获外层 n，不用写捕获列表
let tick = fn() { i += 1 }           // 省略返回类型 = ()

let f = fn(x) = x + 1                // 参数/返回可从上下文推断
```

解析：`fn` 后若是标识符，是具名声明；若是 `(`，是闭包表达式。具名 `fn` 不能出现在函数体里。

参数名在字面量里必有（不用的写 `_`）。参数可以是不可驳模式：`fn((x, y): (int, int)) int { x + y }`。类型与返回类型可省略，规则与 `let` 相同：推不出就报错，不会变成动态类型。返回 `!` 时必须写出 `!`（不能靠省略，省略是 `()`）。

`return` 只离开该闭包本身。`?` 先交给闭包体内最内层 `try`，没有则离开该闭包（该闭包返回类型必须实现 `Try`）。见 [声明](declarations.md)。

调用就是 `add(1, 2)`、`inc(0)`，和具名函数一样。

## 闭包 / 函数的类型

有两层，对应「单态化」和「当值存起来」：

| 你写的 | 是什么 | 何时用 |
|--------|--------|--------|
| 不写类型，`let f = fn(x: int) int { ... }` | 这个字面量独有的匿名类型 | 传给泛型、要内联 |
| `fn(int) int` | 擦除后的句柄（代码指针 + 环境指针） | 结构体字段、数组、类型不同的闭包要放一起 |

类型位置**没有参数名**，返回类型仍在 `()` 后面用空格：

```
fn()                            // fn() ()
fn() !
fn() int
fn(int) int
fn(int, string) bool
```

```
struct Handler {
    on_click: fn()
    map: fn(int) int
}

fn apply(f: fn(int) int, x: int) int = f(x)

let h = Handler {
    on_click: fn() { }
    map: fn(x: int) int = x + 1
}
```

`fn(T) U` 是句柄：`f(g)` 拷贝句柄、共享同一份环境（和 `Vec` 一样）。要换掉调用者手上那个函数才写 `&f`。`clone` 对函数类型不默认提供（环境图不一定可深拷）。

具名函数可以强制成 `fn(T) U`（环境为空）。闭包字面量在需要 `fn(T) U` 的上下文里同样强制：生成胖指针。

## 泛型 callable

`fn(T) U` 擦除具体 callable 类型；使用 `F: Fn(T) U` 则让泛型参数保留闭包或具名函数的具体类型。两种写法的调用结果相同，本规范不承诺某个调用一定内联：

```
fn map[T, U, F: Fn(T) U](xs: &[T], f: F) Vec[U] {
    let out = Vec::new()
    for x in xs {
        out.push(f(x))
    }
    out
}

map(xs, fn(x: int) int = x + 1)   // F 是这个字面量的匿名类型
map(xs, inc)                      // F 是具名函数的具体函数项类型
```

`Fn(T) U` 是编译器内建的可调用约束，不是用户自己 impl 的普通 trait。每个闭包匿名类型、每个匹配签名的具名函数、以及类型 `fn(T) U` 都满足对应的 `Fn`。没有 Rust 那种 `Fn` / `FnMut` / `FnOnce` 三分——捕获语义已经是共享可变 + GC，一种 `Fn` 就够。

用户不要写 `|x|`、不要写捕获列表、不要写 `move`。

## 捕获语义

用户不写捕获列表，也不处理生命周期错误。闭包可以读写外层绑定，效果像共享同一个可变位置（绑定默认可变）。闭包活多久，被它实际使用的外层状态就活多久；互相引用、送进 `async { }`、存进结构体都合法。实现不能把捕获分析失败变成 move、borrow、Clone或生命周期错误。

环境是否拆字段、拷入只读值、留在 stack、提升 managed storage、消除原子/屏障或共享槽，都属于 [AST/HIR](../internals/ast-hir.md)与 [GIR/LIR](../internals/gir-lir.md)；这些优化必须保持上述共享位置语义。

## UFCS

`p.len()` 查找 `len` 对 `p` 的类型：固有 `impl` 优先，然后 trait。与 `Point::len(p)` 等价。没有虚槽、不能在值上改方法。`dyn Trait` 才是虚调用。`dyn Any` 的 `is` / `downcast` / `downcast_copy` 是挂在 `dyn Any` 上的固有方法，显式类型实参写 `a.downcast::[T]()`。无 `self` 的关联函数只能 `Type::name(...)`，不能 `value.name()`。

## `async` 与闭包

`async { ... }` / `async f(x)`：闭包按上面的捕获规则延命，然后在新协程上跑。禁止因为捕获栈变量而悬空——必须升到堆或拷贝。

## Scoped borrowed view callback

标准库的 `with_ref`、`with_read_ref` 与 `for_each_ref` 使用 compiler-owned 的 scoped view callback。它们的 callback 参数在源码层可以写作 `&T`，但 HIR 额外标记为 `ScopedRead` view，不等同于普通可写引用；实现必须在 GIR 中保留该访问模式。`ScopedRead` 只允许在 callback 动态 extent 内读取或向已登记的无逃逸 helper 传递，不能写入、存入任何外部槽、作为返回值、跨协程发布或转换为 raw pointer。需要修改值的 API必须显式声明 `ScopedWrite`，不能借用只读 view 的类型检查空缺。

scoped callback 必须是同步调用：其 body、可达的静态 callee 和 compiler intrinsic 不能 `suspend`、`await`、`yield`、进入 `select`、执行可能挂起的锁/I/O/foreign bridge 或把 view 传给无法证明 no-escape 的动态调用。callback 可以触发普通 safepoint；runtime 会通过 view token 注册临时 root/epoch并在 backing relocation 后修正 view。callback 正常返回或 panic 展开时 token 必须闭合，未闭合或逃逸是 compiler internal error，不能降级成普通 `&T`。

## 调用边界与捕获身份

具名函数项各有唯一的函数项类型；只有在需要 `fn(...) ...` 时才擦除成代码指针加空环境。闭包表达式每次求值创建一个新的闭包值，同一个词法闭包表达式的静态匿名类型不变，但每次执行可以产生不同环境实例。

闭包捕获的是名称解析后实际使用的外层槽，不捕获同名但已被遮蔽的槽。多个闭包捕获同一个绑定时共享同一槽；任一闭包写入后其它闭包和外层代码都能观察。捕获槽若逃逸当前栈帧，编译器必须在逃逸前把槽和需要的环境放入 GC 可追踪存储。

构造闭包不会执行闭包体。闭包可以捕获正在初始化的自身槽以实现递归，但在槽完成赋值前调用或读取该闭包仍是未初始化读取；无法由期望签名推断递归闭包类型时必须写出 `fn(...) ...` 类型注解。互相递归闭包同样要求每个被提前引用的槽先有完整类型。

调用时，实参按[表达式与语句](expressions.md)从左到右求值，再按值传递规则产生语义副本或按引用写进参数槽；不可驳参数模式随后在 callee 入口建立绑定。参数模式理论上失败即程序不是良构程序，因此没有运行时“参数匹配失败”分支。返回表达式按声明的返回类型检查并在 defer 展开前物化；返回值的资源租约建立后才结束退出槽的租约。

齐次变参在调用点把尾部实参按同一元素类型物化为临时连续存储，绑定为 `&[T]`；该临时存储至少活到调用返回，若引用逃逸则按普通引用规则升到 GC。异构类型参数包只在单态化期间展开，包内每个类型分别检查约束，不构造运行时类型数组或 `dyn` 盒子。

## 外部函数调用效应

无函数体的 `extern "C"` 导入函数项和带函数体的 `unsafe extern "C" fn` 可以携带 compiler-only 的 `ForeignEffect`：未标注导入为普通 `ForeignBridge`，`#[ffi(leaf(stack = N))]` 为 `ForeignLeaf`，`#[ffi(dirty_cpu)]` 为 `ForeignBridge[DirtyCpu]`。函数项直接调用或单态化后仍能证明其 effect 时，调用可按该 mode lowering；普通 `fn(...) ...` 擦除、无法解析的函数值和动态分派不保留可证明的 leaf/dirty effect，调用统一 lowering 为普通 `ForeignBridge`。带函数体的 dirty function 不能被调用点改成 leaf；调用点的 `#[ffi(bridge)]` 或 `#[ffi(dirty_cpu)]` 只覆盖当前直接 C 调用。具体声明契约见[unsafe 与 intrinsic](unsafe.md#外部调用效应与桥接)。这些 effect 是 compiler 优化和 runtime 交接信息，不是用户可写的返回类型或可捕获的异常类型。
