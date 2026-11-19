# 语言入门

本章用 15 分钟跑通 Gugu 的最小闭环：建包、写代码、编译、运行、测试。语言语义的权威定义在[语言规范](../spec/overview.md)；本章只教使用顺序。

## 新建包

```text
gugu new hello
cd hello
```

`gugu new` 生成 `gugu.toml` 清单和 `src/main.gg` 入口。`gugu init` 在已有目录里补同样的骨架，不会覆盖已存在的入口文件。

## 第一个程序

最小可执行入口是 `fn main()`；返回值是 `()`：

```gugu
use std.io.{println}

fn main() {
    println("hello, gugu")
}
```

绑定写作 `let 名字 = 值`，类型标注可选（`let i: int = 0`）；返回类型写在参数列表之后、只用空白分隔。绑定默认可变，没有 `mut` 关键字：

```gugu
fn inc(i: int) int = i + 1

fn main() {
    let i = 0
    i = i + 1
    if inc(i) != 2 {
        panic("unexpected")
    }
}
```

要点：

- `if`/`else`、`for i in 0..n`、`loop`、`match` 都是表达式或语句形式，见[表达式与语句](../spec/expressions.md)。
- 字符串插值写 `f"value={name}"`；输出与断言走标准库 `std.io`/`std.test`，见[标准库](../spec/standard-library.md)。
- 当前运行时尚未提供浮点 `Print` 实现，不能直接把浮点值传给 `print` / `println` 或放进运行时 f-string；整数、bool、char 和 string 已支持。
- `use std.fs as fs` 引入模块名，`use std.io.{Error, Read}` 打散导入。规则见[声明与模块](../spec/declarations.md)。

## 编译与运行

```text
gugu build          # 物化到 target/<目标>/bin/
gugu run            # 编译并执行 bin target
gugu run -- args    # -- 之后是程序实参
gugu check          # 只检查，不物化镜像
```

`--target x86_64-windows` 交叉出 Windows 镜像；目标名见[平台与 ABI 参考](../spec/platform-abi.md)。

## 跑测试

在源文件里写 `#[test]`，`gugu test` 收集并执行：

```gugu-test
use std.test.{assert_eq}

#[test]
fn adds() {
    assert_eq(1 + 1, 2)
}
```

完整的测试语义（`#[should_panic]`、`#[ignore]`、并行调度）见[测试](../spec/testing.md)与[测试教程](testing.md)。

## 单文件模式

不建包也能编译单个 `.gg` 文件：`gugu build path/to/main.gg`，产物写到当前目录。清单与 package 模型见[包、依赖与构建模型](../spec/packages-builds.md)。

## 下一步

- 值、句柄与所有权：[值、句柄与资源](values-handles.md)
- 协程与 channel：[并发与取消](concurrency.md)
- 依赖与工作区：[包与构建](package-build.md)
