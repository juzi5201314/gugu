# 测试

Gugu 的测试与基准在 test/bench 域编译：`#[test]`、`#[bench]` 由工具链收集成确定排序的运行器。权威语义见[测试](../spec/testing.md)。

## 写测试

```gugu-test
use std.test.{assert, assert_eq}

fn inc(i: int) int = i + 1

#[test]
fn adds() {
    assert_eq(inc(1), 2)
}

#[test]
#[should_panic]
fn boom() {
    panic("expected")
}

#[test]
#[ignore]
fn slow() { }

#[test]
fn fallsible() Result[(), int] {
    assert(inc(0) == 1)
    Ok(())
}
```

- 测试是无参具名 `fn`，返回 `()` 或 `Result[(), E]`（`E` 实现 `Print` 时按失败记录而不是炸进程）。
- `#[should_panic]` 要求 panic，可选 `#[should_panic(eq = "...")]` 精确匹配；`#[ignore]` 默认跳过。
- 每个测试在新的用户协程上并行调度；`async`/`chan` 用法与普通程序相同。
- `std.test.assert`/`assert_eq`/`assert_ne` 带 `#[track_caller]`，失败消息含源位置。

## 跑测试

```text
gugu test                 # 整个 workspace 的 test target
gugu test --lib           # 只跑 lib 内部用例（编译 test 域的 lib）
gugu test -p owner/name   # 单包
gugu bench                # bench 域
```

`test`/`bench` 只对宿主目标可执行，测试镜像的退出码透传；收集式选择（`--lib`、全 workspace）跳过没有对应 target 的成员。

## 文档测试

`///` 与 `//!` 里的 ```` ```gugu ```` 围栏块在测试构建中编译为测试并运行；标 ignore 的块不跑。 doctest 的失败锚定回文档块在源文件中的行号。

本仓库自身用 `cargo nextest run --workspace` 跑 Rust 侧回归；bench 均为 `harness = false` 的手工验收，不进默认套件，见[性能与延迟门禁](../internals/performance.md)。
