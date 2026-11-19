# comptime 与宏

Gugu 的编译期执行分两层：**值 comptime**（编译期求出普通常量）和**源码宏**（编译期生成源码片段）。权威定义见[编译期执行](../spec/comptime.md)。

## 值 comptime

`comptime { ... }` 把整段表达式强制在编译期求值，返回可物化的常量；未标注的纯常量表达式仍会常量折叠，`comptime` 是“求不了就报错”的强制声明：

```gugu
fn main() {
    let x = comptime {
        let a = 2
        a * 3
    }
    if x != 6 { panic("comptime") }
}
```

形参标 `comptime`（如 `fn repeat[T](comptime n: int, x: T)`）要求调用处给编译期已知值；参数与返回的完整规则见[编译期执行](../spec/comptime.md)。

## 早期与 late comptime

类型检查前的展开（如 `comptime n` 形参、注解参数）是早期 comptime；等到单态化或值依赖时才求值的是 late comptime。阶段越界使用报 `E0054`；两阶段的求值规则与缓存语义见[早期与 late comptime](../spec/comptime.md#early-and-late-comptime)。

## 源码宏

`comptime source { ... }` 在编译期执行并用 `std.syntax`/`std.src` 解析源码片段注入调用点——可以出现在声明、表达式、类型和模式位置：

```gugu
comptime source {
    std.syntax.parse_items("fn helper() int { 7 }")
}

fn main() {
    let value = comptime source {
        std.syntax.parse_source("1 + 2")
    }
    if helper() + value != 10 { panic("expand") }
}
```

宏有递归与资源预算（`#![comptime(expansion_limit = N)]`），展开环报 `E0048`，预算耗尽报 `E0049`：

```gugu
#![comptime(expansion_limit = 64)]

fn main() {}
```

宏展开的输入、展开记录与诊断回锚定（错误指回宏调用处的源位置）由编译器保证，不需要宏作者手工管理。展开 API 的完整签名见[标准库 · std.syntax](../spec/standard-library.md)。

## 常见坑

- comptime 函数用定义处的词法作用域，读不到调用者的局部绑定。
- `return`/`break`/`?` 立即离开当前求值上下文；未求值的兄弟操作数不执行。
- 缓存命中必须得到与求值相同的值——comptime 结果依赖的外部输入要在编译配置里登记，否则指纹失效规则见[包、依赖与构建模型](../spec/packages-builds.md)。
