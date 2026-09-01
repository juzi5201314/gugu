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
- `type_id_count()` 只在具体类型集合冻结后求值，禁止参与数组长度、泛型实参、布局与可达性，避免类型集合对自身计数产生循环。
- `[]` 参数表可混写类型参数与 `comptime` 参数，适用于 `fn` / `struct` / `enum` / `union` / `trait` / `impl` / `type`。语言为数组与元组生成 `Clone` / `Eq` / `Ord` / `Print`（元素满足约束时）。
- `#[coroutine_local]` / `#[os_thread_local]` 的初始化在**第一次访问时运行时求值**（可分配）。同一槽重入初始化则 panic。普通 `static` / `const` 仍必须 comptime。
- 固有 `impl` 只能写在类型的定义模块（语言类型由标准库 / 编译器）。其它模块加方法只能写 trait。
- 测试收集顺序确定，执行时各测试在新的用户协程上**并行**。
- 编译入口由 CLI 指定源文件；文件系统即模块树；`std` 保留；同名 `foo.gg` 与 `foo/mod.gg` 并存是错误。命令行契约已独立成章，见[工具链与命令行](../src/spec/toolchain-cli.md)。
- 未标注导入视为可能阻塞或回调，使用普通 `ForeignBridge`：先切 system stack并发布精确 roots；runtime可为短调用暂留可被 GC、回调、退役或 runnable压力取回的逻辑处理器 lease，快速返回时直接恢复。`ForeignBridge[DirtyCpu]`立即释放处理器，只有承担有界时间、stack和无回调契约的 `ForeignLeaf`始终保留处理器。外部操作系统线程调入导出函数时，临时登记为工作线程并取得逻辑处理器，返回后拆掉。
- `string` 的 `+` / `+=` 走语言提供的 `Add[string]` / `AddAssign[string]`，与用户类型同一套重载。整数加减仍由编译器直接降指令，同时提供对应 trait impl 供泛型约束使用。
- `derive` 允许 `Print`。`Print` 接收者是 `&Self`。`Option` / `Result` / `Vec` / 数组 / 元组必须有 `Print` / `Eq` / `Clone`（`Ord` 在元素都 `Ord` 时）。
- 字段访问与 `match` 自动解 `&`，和方法一致。
- `main` 返回 `()` 或 `Result[(), E]`（`E: Print`）。整数变窄：debug panic，release 按目标位宽截断。
- `panic` 只接 `string`。`std.mem.LocalArena`、`std.mem.SyncArena` 与 `std.mem.pin` 是必须存在的 lang item；不保留未区分并发模型的 `std.mem.Arena`。

## 后果

- 调度章节不再依赖读者认识 Go 的字母表。
- 数组字段上的 `#[derive(Clone)]` 与 `f"{xs}"` 有定义。
- 协程本地的 `Vec` 每份协程真正独立，不会变成共享句柄。
