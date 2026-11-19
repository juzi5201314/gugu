# 类型与 trait 速查

权威定义见[类型系统](../spec/types.md)与[接口、实现与特化](../spec/traits.md)。

## 内置标量

`bool`、`int`/`uint`（指针宽度）、`i8`–`i64`、`u8`–`u64`、`f32`/`f64`、`char`、`byte`。`i128`/`u128` 有平台限制（Windows extern 签名禁止），见[平台与 ABI 参考](../spec/platform-abi.md)。

## 复合类型

- `string`：可变 COW 字符串，句柄类型；`Bytes`/`ByteBuffer` 是字节对应物。
- `(T, U)` 元组、`[T; n]` 数组、`&[T]` 切片、`&T` 引用。
- `struct`、`enum`（标签联合，三种变体：无载荷、元组式、结构体式）、newtype `struct Name(T)`。
- `union`（unsafe 域）、`*T` 原始指针、`MaybeUninit[T]`。

## trait

```gugu
trait Show {
    fn show(self: &Self) string
}

struct Small { v: int }

impl Show for Small {
    fn show(self: &Self) string = "small"
}

fn main() { _ = Small { v: 1 }.show() }
```

- `impl Trait for Type` 为类型实现接口；`impl Type` 写固有方法。
- 内建 trait：`Eq`、`Ord`、`Hash`、`Print`、`Try`（`?` 的入口）、`Iterator`、运算 trait。
- `impl Trait` 在参数或返回位置表示接口对象/不透明返回；`type Foo = impl Trait` 是不透明别名。
- 特化与单态化规则见[接口、实现与特化](../spec/traits.md)。

## `?` 与 Result

`?` 要求操作数实现 `Try`；标准库 `Result`/`Option` 已实现。没有错误类型隐式转换：出口的 `Try::Error` 必须与操作数相同。

## 布局属性

`#[repr(C)]` C 布局、`#[repr(packed)]` 去填充、`#[repr(transparent)]` 单字段透明、`#[repr(u8)]`（或其它整数 repr）枚举定判别值并关 niche 压缩。size/align 语义见[类型系统](../spec/types.md)。
