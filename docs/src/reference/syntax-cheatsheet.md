# 语法速查

一页看全日常写法；形式语法见[形式语法](../spec/syntax.md)，词法细节见[词法结构](../spec/lexical.md)。

## 声明

```gugu
use std.fs as fs
use std.io.{Error, Read}

const LIMIT: int = 100
static counter: int = 0

type Meters = int          // 透明别名
struct Id(pub uint)        // newtype
struct Point { pub x: int, pub y: int }

enum Option2[T] {
    Some(T)
    None
}

fn add(a: int, b: int) int = a + b

fn main() { _ = add(1, 2) }
```

## 绑定与控制流

绑定默认可变，没有 `mut`；同块内后一个 `let` 遮蔽前一个：

```gugu
fn main() {
    let x = 1
    let y = 2
    y += x

    if y > 2 { _ = 1 } else { _ = 0 }

    for i in 0..8 { _ = i }
    for item in [1, 2, 3] { _ = item }
    loop { break }
    while y > 0 { y -= 1 }

    match y {
        0 => { _ = 0 }
        _ => { _ = 1 }
    }
    _ = x
}
```

## 类型写法速查

| 写法 | 含义 |
| --- | --- |
| `Vec[int]`、`Result[T, E]` | 泛型用 `[]`，不用 `<>` |
| `f::[int](x)` | 显式类型实参（`f[int](x)` 是下标） |
| `&T`、`&[T]` | 引用、切片 |
| `[T; n]`、`&[T]` | 定长数组、切片类型 |
| `(int, string)` | 元组 |
| `*T` | 原始指针（unsafe） |
| `!` | never |
| `chan[T]`、`Join[T]` | channel、协程句柄 |

## 并发

```gugu
fn work(c: chan[int]) {
    c.send(1)
}

fn main() {
    let c = chan[int](4)
    let job = async work(c)
    _ = job.wait()
    select {
        let v = c.recv() => { _ = v }
        _ => { _ = 0 }
    }
}
```

## 常用属性

| 属性 | 用途 |
| --- | --- |
| `#[test]`、`#[bench]`、`#[ignore]`、`#[should_panic]` | 测试与基准 |
| `#[repr(C)]`、`#[repr(packed)]`、`#[repr(transparent)]`、`#[repr(u8)]` | 布局 |
| `#[cfg(...)]` | 条件编译 |
| `#[allow(lint)]`、`#[deny(lint)]`、`#[forbid(lint)]` | lint 级别 |
| `#[export_name]`、`#[link_name]`、`#[ffi(...)]` | FFI |
| `#[os_thread_local]`、`#[coroutine_local]` | 线程/协程局部静态 |
| `#![comptime(expansion_limit = N)]` | 宏展开预算 |

完整属性表见各 spec 章节。
