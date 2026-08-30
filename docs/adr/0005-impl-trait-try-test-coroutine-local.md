# ADR 0005：impl Trait、try、let 链、否定 impl、测试与协程本地

- 状态：已接受
- 日期：2026-08-30

## 决策

- `impl Trait` 是单态化的存在类型（APIT / RPIT / TAIT），不是 `dyn`。透明 `type` 别名与 `type Foo = impl Trait` 靠右侧是否 `impl` 区分。
- `try { }` 接住 `?`；`Try` 增加 `from_value` / `from_error`。没有错误类型隐式转换。
- `if` / `while` 条件是 `&&` let 链。后缀 `expr.match { }` 与前缀 `match` 同语义。
- `impl !Trait for Type` 挖掉 blanket，并把 `chan` / `Join` 的「禁止 Clone」写成闭世界事实。
- lint 四级：allow / warn / deny / forbid。`#[test]`、doctest、`cfg(test)`。
- 用户要的「线程本地」其实是 `#[coroutine_local] static`（跟协程走，协程换操作系统线程仍是同一槽）。操作系统线程本地是 `#[os_thread_local]`，只给 FFI。进程一次性初始化是 `OnceLock` / `Lazy`。

## 后果

- 返回闭包不必擦成 `fn(T) U`，也不必手写结构体。
- 协程迁移到另一条操作系统线程后，`#[coroutine_local]` 仍是同一槽；`#[os_thread_local]` 会变成当前操作系统线程的那一份。
- `forbid` 不能被内层 `allow` 打穿，适合测试与库根模块。
