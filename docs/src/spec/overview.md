# 概述与目标

## 一句话

Gugu 是 AOT 编译的、健全静态类型的语言：语法刻意简单，语义刻意静态。省略类型注解只影响编译期；跑起来之后不能再出现「这个字段变成了函数」这类动态对象模型。

## 设计公理

后续语法糖不得削弱这些条款。

1. **健全静态类型。** 推断可以很激进，但不是 TypeScript 那种可擦除、可 `any` 的渐进类型。
2. **静态布局。** 数据是 `struct` / `enum` / 元组 / 数组。不是 JavaScript 对象，也不是 Lua table。
3. **闭世界。** 全程序编译结束前必须能枚举全部可达函数和类型。禁止 `eval`、禁止运行时改布局、禁止加载未知本语言代码。插件只走窄 C ABI。
4. **单态化、少装箱。** 泛型按使用处展开。擦除必须显式（`dyn Trait`、胖 `fn`）。
5. **分配可见。** 逃逸分析、栈分配、arena。GC 只处理真的堆对象。
6. **推断 ≠ 开世界。** 闭世界是代码全集可见；省略类型是编译器补全注解。
7. **运行时是语言的一部分。** 镜像 = 用户码 + Gugu runtime + rt0。runtime 禁止用 Rust 写两遍。
8. **性能写进编译器。** 分配、写屏障、换栈、safepoint 是 IR 原语。
9. **必须有 `unsafe` 与 intrinsic。** 否则 GC、调度、channel 无法用本语言写。
10. **没有对象系统。** 禁止类、继承、原型链、隐式 `this`、运行时加字段。方法是 UFCS / `impl`，默认静态分发。
11. **闭包是一等公民。** 捕获不增加用户心智负担；逃逸则 GC 延命。
12. **高并发。** 有栈绿色协程 + M:N + 抢占 + channel。`async` 只启动协程，不是函数染色。
13. **系统接口：** Linux 直接 syscall；Windows 薄 IAT（kernel32/ntdll）。默认不链 libc。

## 目标

- 轻量：编译器短小；静态镜像；无重型宿主。
- 高性能：机器码锚点是 Rust / C++。
- 低占用：值类型、定布局、可见分配、精确可移动 GC。
- 语法简单：表面接近 Go / JS·TS / Lua。
- 使用方便：推断、短函数、插值、comptime、特化。

## 非目标

- 不用 LLVM，不用 Cranelift。自研 x86_64 codegen。
- 不用系统 `ld` / `link.exe` 作为发布模型。
- 不是渐进类型，不是字节码 VM，不是 WASM guest。
- 编译器用 Rust 写，不赋予程序 Rust ABI。

## 示例

绑定与参数：`名字: 类型`。返回类型在 `()` 后，只用空白。

`main.gg`：

```
use green.{bar}
use std.io.{print, println}
use std.io as io2

fn main() {
    let i = 0
    if i <= 1 {
        print(f"i = {i}")
    } else {
        io2.eprint("err")
    }
    println("i + 1 = ", inc(i), bar())
}

fn inc(i: int) int = i + 1
```

`green.gg`：

```
pub fn bar() string = "bar"
```

`use green` 只引入模块名；要打散写 `use green.{bar}`。插值必须写 `f"..."`。`println` 的多参数是异构参数包，不是模块值。

## 源文件

UTF-8，扩展名 `.gg`。文件是模块，目录是包，见 [声明与模块](declarations.md)。
