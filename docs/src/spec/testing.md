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
- 同一模块多个 `#[test]` 合法。运行器按模块路径 + 函数名排序收集（列表与失败报告确定）。每个测试在**新的用户协程**上跑；执行时这些协程**并行**调度。测试函数 panic 且无 `should_panic` → 该测试失败，其它测试继续。测试里的 `async` / `chan` 与普通程序相同；该测试协程结束时仍存活的分离协程按 [运行时](runtime.md) 等待。共享 `static` 是作者的事。

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

## 收集、执行与结果

测试收集在 `cfg`、名称解析和单态化之后进行。测试身份是规范化模块路径加函数名；重复身份、无效签名和测试属性组合冲突是编译错误。文档测试使用源文件路径、围栏起始行和同文件序号形成稳定身份。

运行器必须提供至少三种操作：按稳定顺序列出测试；按名称子串或完整身份筛选；执行默认测试或显式包含 `#[ignore]`。筛选不改变测试的静态编译结果，未被执行的测试仍必须编译。没有匹配项不是语言错误，运行器应以成功且零测试报告结束。

执行可以并行，但报告按稳定测试身份排序；不能用完成顺序改变最终摘要。每个测试有独立的顶层用户协程和 panic 捕获边界。测试返回 `Err(e)`、未预期 panic、`should_panic` 未发生、panic 消息不等于 `eq`、断言 panic 或该测试创建且未被处理的分离协程 panic，都会使该测试失败，不终止其它测试。

测试顶层返回后，运行器等待该测试创建的所有仍存活后代协程；这些后代只归属于创建它们的测试，即使 Join 已丢弃。后代全部结束后才确定该测试结果。共享全局/static 仍可使不同测试发生数据竞争，运行器不通过串行化掩盖该程序错误。

`#[should_panic]` 只匹配测试顶层协程逃出的 panic；被 `std.panic.catch` 或 `Join.wait()` 处理的 panic不算。`eq` 只比较 `Panic.message` 的 UTF-8 内容，不比较位置。`#[ignore]` 与 `#[should_panic]` 可以同时出现，显式执行 ignored 测试时仍检查 panic 条件。

失败报告至少包含测试身份、失败类别、主源位置和可用的 `Panic`/`Print` 内容。测试进程最终退出码：所有实际执行的测试成功为 0，只要一个失败或运行器自身初始化失败即非 0；忽略和零匹配不算失败。

## Benchmark Harness

默认 bench target 在 `cfg(bench)` 为真时收集 `#[bench]` 函数；其它 target 中这些项不存在。函数签名必须恰好为：

```text
#[bench]
fn name(b: &std.test.Bencher)
```

benchmark 不能同时标记 `test`、`should_panic` 或 `ignore`，不能是 generic、unsafe、extern 或闭包。每个 benchmark 有独立 panic 边界；panic、未处理的子协程 panic、未执行测量或执行多个顶层测量都会使该 benchmark 失败。

最低 API 为：

```text
struct Bencher

fn iter[F: Fn()](self: &Self, f: F)
fn iter_batched[T, S: Fn() T, F: Fn(T)](self: &Self, setup: S, f: F)
fn bytes(self: &Self, n: int)
fn black_box[T](value: T) T
```

`iter` 在预热后自适应选择每个样本的迭代次数；只统计 `f` 的执行。`iter_batched` 在每次测量外调用 setup，并只统计使用新值的 `f`，避免准备过程污染结果。`bytes(n)` 设置每次迭代处理的非负字节数，用于报告吞吐；重复设置覆盖前值。`black_box` 阻止编译器基于值内容删除或常量折叠穿过该点，但不是内存 fence 或同步原语。

运行器至少报告 benchmark 身份、样本数、每样本迭代数、中位数纳秒/迭代以及最小/最大样本。预热时长、样本数和统计扩展可以由工具选择并通过命令行覆盖；这些测量值不是语言确定性的一部分。Benchmark 默认串行执行，只有显式并行选项才能并发，以避免普通命令引入竞争噪声。

`gugu bench` 只缓存编译结果，不缓存或复用历史测量。`harness = false` 的 bench 按普通 main 可执行程序构建和运行，仍处于 `cfg(bench)` 并可以使用 test-dependencies；其中出现 `#[bench]` 是编译错误。
