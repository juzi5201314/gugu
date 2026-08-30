# ADR 0005：impl Trait、try、let 链、否定 impl、测试与 G 本地

- 状态：已接受
- 日期：2026-08-30

## 决策

- `impl Trait` 是单态化的存在类型（APIT / RPIT / TAIT），不是 `dyn`。透明 `type` 别名与 `type Foo = impl Trait` 靠右侧是否 `impl` 区分。
- `try { }` 接住 `?`；`Try` 增加 `from_value` / `from_error`。没有错误类型隐式转换。
- `if` / `while` 条件是 `&&` let 链。后缀 `expr.match { }` 与前缀 `match` 同语义。
- `impl !Trait for Type` 挖掉 blanket，并把 `chan` / `Join` 的「禁止 Clone」写成闭世界事实。
- lint 四级：allow / warn / deny / forbid。`#[test]`、doctest、`cfg(test)`。
- 用户「线程本地」是 `#[g_local] static`（跟 G 走）。OS TLS 是 `#[os_thread_local]`，只给 FFI。进程一次性初始化是 `OnceLock` / `Lazy`。

## 后果

- 返回闭包不必擦成 `fn(T) U`，也不必手写结构体。
- G 迁移后 `#[g_local]` 仍是同一槽；`#[os_thread_local]` 会变成另一份 M 的槽。
- `forbid` 不能被内层 `allow` 打穿，适合测试与库根模块。
