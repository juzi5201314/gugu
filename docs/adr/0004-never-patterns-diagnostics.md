# ADR 0004：never、精确宽度 128、模式与诊断通道

- 状态：已接受
- 日期：2026-08-30

## 上下文

规范补齐 runtime / FFI / 双目标立刻会碰到的洞：未初始化内存、C 布局、条件编译、字节字面量、发散、名义包装、穷尽匹配的表达力、lint。

## 决策

- never 类型写成 `!`，与 unit `()` 分开。`!` 零值，可隐式变成任何类型。无 `break` 的 `loop` 类型是 `!`。
- 单字段元组结构体做 newtype；多字段仍禁止位置构造。
- 关联常量与 `const` 同一套 comptime。
- `i128` / `u128` 进精确宽度标量表（16 字节、对齐 16）。日常代码仍用 `int`。`extern "C"`：Linux 按 `__int128`，Windows 禁止。
- `#[cfg]` 按目标裁项。`union`、`MaybeUninit`、`transmute`、volatile、指针读写、`asm` / `global_asm` / 链接属性进入语言。
- `b"..."` / `b'x'` / `c"..."`。`size_of` / `align_of` / `offset_of` 预导入且 comptime。
- 模式：`if let` / `while let` / `let-else`、or、`@`、rest、范围、`let` 与参数解构。值更新 `{ ..p }` 仍禁止。
- 诊断：错误 vs lint。`#[must_use]` 标在 `Result` / `Option`。`large_copy`。`#[track_caller]` + `std.src`。丢掉 `Join` 仍是分离，不 `must_use`。

## 后果

- `fn() !` 与 `fn() ()` 不是同一类型；前者可强制成 `fn() T`。
- 写 runtime 不必再等宏系统：位置、布局、未初始化、汇编都是 intrinsic / 属性。
- Windows 上 128 位整数过 C 边界必须拆字，不能假装有 `__int128`。
