# 测试

测试是语言的一部分，不是外部脚本约定。生产镜像默认不含测试项。

## 测试构建

编译器有测试模式。该模式下：

- `#[cfg(test)]` 为真（见 [词法 · cfg](lexical.md)）。
- 带 `#[test]` 的函数被链进测试运行器，不进普通 `main` 镜像。
- 文档测试（见下）同样只在此模式编译。

非测试构建里，`#[test]` 项与 `#[cfg(test)]` 项不存在（与 `cfg` 相同）。

## `#[test]`

```
#[test]
fn adds() {
    std.test.assert(inc(1) == 2)
}

#[test]
fn io() Result[(), IoError] {
    let f = open("t")?
    Ok(())
}

#[test]
#[should_panic]
fn boom() {
    panic("expected")
}

#[test]
#[ignore]
fn slow() { }
```

- 必须是无参数的具名 `fn`。返回 `()` 或 `Result[(), E]`（失败时 `E` 实现 `Print`，测试记为失败而不展开整个进程）。
- 不能是 `unsafe fn`、不能是 `#[naked]`、不能是 `extern`。
- `#[should_panic]`：该测试必须 panic；正常返回则失败。可选 `#[should_panic(eq = "expected")]`：`Panic` 消息必须等于该 comptime 字符串。
- `#[ignore]`：默认跳过，运行器显式请求时才跑。
- 同一模块多个 `#[test]` 合法。运行器按模块路径 + 函数名排序，顺序确定。
- 每个测试在**新的用户协程**上跑。收集顺序按模块路径 + 函数名排序（列表与失败报告确定）。执行时这些协程**并行**调度。测试函数 panic 且无 `should_panic` → 该测试失败，其它测试继续。测试里的 `async` / `chan` 与普通程序相同；该测试协程结束时仍存活的分离协程按 [运行时](runtime.md) 等待。共享 `static` 是作者的事。

`std.test` 必须存在，且带 `#[track_caller]`：

```
fn assert(cond: bool)
fn assert_eq[T: Eq + Print](left: T, right: T)
fn assert_ne[T: Eq + Print](left: T, right: T)
```

条件失败则 `panic`，消息含源位置与（对 `assert_eq` / `assert_ne`）两边的 `Print` 输出。

## 文档测试

`///` 与 `//!` 里的围栏代码块在测试构建中编译为测试，除非标记为忽略。

````
/// 加一。
/// ```
/// std.test.assert(inc(1) == 2)
/// ```
fn inc(i: int) int = i + 1
````

规则：

- 信息串为空或 `gg`：当作 Gugu 源，包进一个 `#[test]` 函数体。当前模块的公开项在作用域内（与在该模块里写测试相同）。
- `ignore`：不编译为测试，仍是文档。
- `text`、`ignore` 以外的未知信息串：不编译为测试。
- 片段里若出现 `fn main()`，则改为编译成独立测试程序（仍链同一闭世界的 `std`），跑这个 `main`；`main` 返回或 `Result` 失败规则与 `#[test]` 相同。
- 文档测试失败必须报告**文档所在源位置**，不是生成的包装函数。

没有独立的宏展开层：围栏里就是 Gugu 源。
