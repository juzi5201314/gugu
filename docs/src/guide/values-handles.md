# 值、句柄与资源

Gugu 区分**值**（按位拷贝的位类型）、**句柄**（GC 管理的对象引用）与**资源**（带终结语义的句柄）。本章教怎么选、怎么传；权威规则在[值、句柄与传递](../spec/passing.md)与[内存与对象模型](../spec/memory.md)。

## 位类型与拷贝

`int`、`bool`、数组、不含句柄字段的 struct 是位类型，赋值与传参按位拷贝：

```gugu
struct Point {
    x: int
    y: int
}

fn move_it(p: Point) Point {
    Point { x: p.x + 1, y: p.y }
}

fn main() {
    let a = Point { x: 1, y: 2 }
    let b = move_it(a)
    if b.x != 2 { panic("copy") }
}
```

超过 64 字节的按值位结构体触发 `large_copy` lint（`E0056`，默认 warn）：不是错误，但提示改传 `&T`。

## 引用 `&T`

`&T` 借用一个值或对象，只读共享。逃逸到堆的 `&T` 由 GC 保活，不需要手动管理：

```gugu
fn sum(xs: &[int]) int {
    let total = 0
    for x in xs {
        total += x
    }
    total
}

fn main() {
    let xs = [1, 2, 3]
    if sum(&xs) != 6 { panic("sum") }
}
```

## 句柄类型

`string`、`Vec[T]`、`chan[T]` 等是句柄：变量里存的是指向 GC 对象的引用，赋值拷贝句柄而非对象。递归类型的字段必须是句柄或 `&T`，不能把无限大的 `Node` 嵌进 `Node`：

```gugu
enum List[T] {
    Nil
    Cons(T, &List[T])
}
```

## 资源与租约

文件、socket、进程这类资源句柄带显式终结：`close()` 或 `defer` 作用域清理。资源不是“用完即弃”的值；把它当普通值复制传递前，先读[内存与对象模型](../spec/memory.md)的租约规则。

```gugu
use std.fs
use std.io.{Error}
use std.path.{Path}
use std.text.{Bytes}

fn write_greeting(path: Path) Result[(), Error] {
    let options = fs.OpenOptions::new()
    options.write = true
    options.create_new = true
    let file = fs.open(path, options)?
    defer { _ = file.close() }
    _ = file.write(Bytes::from_string("hi"))?
    Ok(())
}
```

## pin：交给外部代码前的固定

把 GC 对象交给 FFI 或异步原生代码时必须 `std.mem.pin` 固定，或先拷到非移动缓冲；pin 不递归固定 referent。细则见[unsafe 与 intrinsic](../spec/unsafe.md)与[FFI 教程](ffi-unsafe.md)。

## 选择速查

| 场景 | 写法 |
| --- | --- |
| 小位结构体 | 直接按值传 |
| 大位结构体 | `&T` |
| 可变集合 | 句柄类型本身，如 `Vec[T]` |
| 递归结构 | `&T` 或句柄字段 |
| 外部代码可见的缓冲 | `std.mem.pin` 或非移动拷贝 |
