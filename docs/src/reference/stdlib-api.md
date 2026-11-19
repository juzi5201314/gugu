# 标准库 API

`std` 是唯一保留 package，没有 `core`/`alloc` 分层。模块清单与逐条签名见[标准库](../spec/standard-library.md)。

## 模块地图

| 模块 | 内容 |
| --- | --- |
| `std.option`/`std.result`/`std.iter`/`std.ops` | `Option`、`Result`、迭代器、运算 trait |
| `std.error`/`std.cmp`/`std.fmt`/`std.hash` | 错误模型、比较、格式化、散列 |
| `std.text` | Unicode 文本 |
| `std.collections` | `Vec`、`HashMap`、`HashSet` 等 |
| `std.io`/`std.path`/`std.fs` | `print`/`println`、路径、文件系统 |
| `std.net`/`std.process`/`std.env`/`std.time` | socket、进程、环境、时间 |
| `std.random`/`std.math` | 随机数、数学 |
| `std.sync`/`std.resource` | 锁、`OnceLock`/`Lazy`、`CancelSource`、资源租约 |
| `std.mem`/`std.ptr` | `pin`、内存原语（`std.ptr` 仅 unsafe） |
| `std.ffi`/`std.panic` | FFI 支持、panic 拦截（`catch`） |
| `std.runtime`/`std.signal` | 运行时控制 facade、信号订阅 |
| `std.src`/`std.syntax` | comptime 源码解析 |
| `std.test`/`std.build`/`std.hint` | 断言、build 脚本接口、编译器提示 |

## Prelude

`Option`、`Result`、`Vec`、`HashMap`、`HashSet` 等在 Prelude 中免导入；其它标准库项必须显式 `use`。Prelude 是封闭兼容面，新增公开类型不会自动进入。

## 常用条目

```text
std.io.print / println / eprint
std.test.assert / assert_eq / assert_ne
std.sync.CancelSource / CancelToken / OnceLock / Lazy
std.fmt.Formatter            // write_str / pad / debug_*
std.runtime.set_parallelism(n)
std.mem.pin
std.panic.catch
std.fs.open -> Result[File, std.fs.Error]
```

错误模型（领域错误 vs panic）、可变 COW `string`、`Bytes`/`ByteBuffer` 与集合的完整契约见[标准库](../spec/standard-library.md)各节。
