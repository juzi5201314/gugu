# ADR 0007：规范审查补洞与命名

- 状态：已接受
- 日期：2026-08-30

## 上下文

通读现行 `docs/src/spec` 之后，发现计数类型自相矛盾、数组无法按长度写 impl、`static` 初始化与协程本地冲突，以及调度模型用单字母点名。一并钉死。

## 决策

### 命名

- 禁止用单字母指称调度对象。规范与属性名写全称：**协程**、**操作系统线程**、**逻辑处理器**。禁止 `G` / `M` / `P` / `GMP` / `g_local`，禁止把 coroutine 缩成 `c` / `g`。
- 协程本地存储属性是 `#[coroutine_local]`（不是 `g_local`，也不是 global）。进程级可变存储仍是普通 `static`。操作系统线程本地仍是 `#[os_thread_local]`。

### 语言条款

- 计数一律 `int`：`len` / `cap` / 下标 / `Range` / `size_of` / `align_of` / `offset_of` / `type_id_count` / `TypeId.as_int`。`0..xs.len()` 合法。
- `[]` 参数表可混写类型参数与 `comptime` 参数，适用于 `fn` / `struct` / `enum` / `union` / `trait` / `impl` / `type`。语言为数组与元组生成 `Clone` / `Eq` / `Ord` / `Print`（元素满足约束时）。
- `#[coroutine_local]` / `#[os_thread_local]` 的初始化在**第一次访问时运行时求值**（可分配）。同一槽重入初始化则 panic。普通 `static` / `const` 仍必须 comptime。
- 固有 `impl` 只能写在类型的定义模块（语言类型由标准库 / 编译器）。其它模块加方法只能写 trait。
- 测试收集顺序确定，执行时各测试在新的用户协程上**并行**。
- 编译入口由 CLI 指定源文件；文件系统即模块树；`std` 保留；同名 `foo.gg` 与 `foo/mod.gg` 并存是错误。
- 导入的外部函数视为可能阻塞：调用前让出逻辑处理器。外部操作系统线程调入导出函数时，临时登记为工作线程并配逻辑处理器，返回后拆掉。
- `string` 的 `+` / `+=` 走语言提供的 `Add[string]` / `AddAssign[string]`，与用户类型同一套重载。整数加减仍由编译器直接降指令，同时提供对应 trait impl 供泛型约束使用。
- `derive` 允许 `Print`。`Print` 接收者是 `&Self`。`Option` / `Result` / `Vec` / 数组 / 元组必须有 `Print` / `Eq` / `Clone`（`Ord` 在元素都 `Ord` 时）。
- 字段访问与 `match` 自动解 `&`，和方法一致。
- `main` 返回 `()` 或 `Result[(), E]`（`E: Print`）。整数变窄：debug panic，release 按目标位宽截断。
- `panic` 只接 `string`。`std.mem.Arena` 与 `std.mem.pin` 是必须存在的 lang item。

## 后果

- 调度章节不再依赖读者认识 Go 的字母表。
- 数组字段上的 `#[derive(Clone)]` 与 `f"{xs}"` 有定义。
- 协程本地的 `Vec` 每份协程真正独立，不会变成共享句柄。
