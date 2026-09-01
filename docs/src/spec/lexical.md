# 词法结构

源文件是 UTF-8。BOM 必须被拒绝。

## 空白与换行

空格、制表符、换行都是空白。块用 `{` `}`，不靠缩进。规范排版、缩进、行宽和换行输出见[格式化与代码风格](format-style.md)。

语句之间不写分号。换行结束一条语句，除非该行在词法上未完成。行末记号属于下列集合则必须续行：二元运算符、`(`、`[`、`{`、`,`、`.`、`::`、`..`、`:`、`=`、`=>`、`?`。该规则适用于一切「换行否则结束语句」的上下文，包括 `match` 臂、`let` 右值、调用实参、数组字面量。因此下列各是一条语句：

```
let xs = [
    1, 2,
]
let y =
    xs[0]
Ok(v) =>
    v
foo()?
    .bar()
```

行末是 `]`、`)`、`}` 时该行已完成。函数返回类型必须与 `)` 写在同一行（`fn f(x: int) int {`；参数表用 `(` 续行时，`) int {` 仍在 `)` 那一行）。实现必须按「未完成则续行」识别，禁止 JavaScript 式自动分号插入。

可选的 `;` 只用于**丢弃表达式的值**（块里不想把某表达式当成块值时）。它不是语句终止符。

## 注释

注释的语法由本节规定，注释正文、文档用途和规范排版见[格式化与代码风格](format-style.md)。

- `//` 到行末：普通注释。
- `///` 到行末：文档注释，附着在其后第一个声明上。
- `//!` 到行末：模块级文档，附着在当前文件模块上。
- `/* ... */`：块注释，可嵌套。文档块注释不另设语法。

文档注释必须进入 AST。

## 属性

```
#[inline]
#[repr(C)]
#[derive(Clone, Eq)]
#[cfg(os = "linux")]
#[must_use]
pub fn bar() string = "bar"
```

- `#[]` 附着在其后的声明或表达式上。记号是 `#` 后接 `[...]`。
- `#![...]` 附着在当前模块上。
- 未知属性必须报错，禁止静默忽略。
- 语言内建属性：
  - 优化：`inline`、`cold`
  - 布局：`repr(C)`、`repr(u8)`、`repr(u16)`、`repr(u32)`、`repr(u64)`、`repr(packed)`、`repr(transparent)`、`repr(align(N))`（`N` comptime 二的幂）
  - `derive(...)`：只允许 `Clone`、`Eq`、`Ord`、`Hash`、`StableHash`、`StableOrd`、`Print`。其它名字按未知属性报错。不是插件机制。
  - 条件编译：`cfg(...)`，见下
  - 诊断：`must_use`、`allow(lint)`、`warn(lint)`、`deny(lint)`、`forbid(lint)`
  - 测试：`test`、`should_panic`、`ignore`（见 [测试](testing.md)）
  - 存储：`coroutine_local`、`os_thread_local`
  - 调用点：`track_caller`、`ffi(bridge)`、`ffi(dirty_cpu)`
  - FFI：`ffi(leaf)`、`ffi(dirty_cpu)`
  - 链接：`export_name = "..."`、`link_name = "..."`、`link_section = "..."`、`used`、`naked`

### `cfg`

`#[cfg(谓词)]` 使该项或表达式在谓词为假时**不存在**（不参与类型检查、不 codegen）。与 `comptime if` 不同：被裁掉的一侧不必在当前目标上成立。

谓词：

| 谓词 | 为真当 |
|------|--------|
| `os = "linux"` / `os = "windows"` | 目标 OS；合法目标组合见[平台与 ABI 参考](platform-abi.md) |
| `arch = "x86_64"` | 目标架构；合法目标组合见[平台与 ABI 参考](platform-abi.md) |
| `feature = "name"` | 当前 package 在本解析域启用了 feature，见[包、依赖与构建模型](packages-builds.md) |
| `test` | test target/harness，见[测试](testing.md) |
| `bench` | bench target/harness |
| build.gg 注册的 `name` / `name = "value"` | 当前 package 的 build 输出包含该自定义 cfg |
| `not(P)` | P 为假 |
| `all(P, Q, ...)` | 全部为真 |
| `any(P, Q, ...)` | 至少一个为真 |

未知谓词、未定义 feature 或未由 build.gg 注册的自定义 cfg 是编译错误。`os` 与 `arch` 是本版本仅有的内建平台键；模块级 `#![cfg(...)]` 使整个文件在该 target 上不存在。语言没有 `cfg(debug)`、`cfg(release)`、`cfg(strip)`、`cfg(race)` 或 `cfg(coverage)`。

### 诊断

诊断分：**错误**（必须修）与 **lint**。lint 四级，从松到紧：

| 级 | 属性 | 效果 |
|----|------|------|
| allow | `#[allow(name)]` | 不报 |
| warn | `#[warn(name)]` | 警告，仍产出镜像 |
| deny | `#[deny(name)]` | 升为错误，阻止镜像 |
| forbid | `#[forbid(name)]` | 同 deny，且内层不得再 `allow` / `warn` 把它降回去 |

未写属性时用该 lint 的默认级。外层 `forbid` 覆盖内层任何降级，内层再写 `allow` 是编译错误。`deny` 可被内层 `allow`。未知 lint 名是编译错误。

语言与风格 lint：

| 名字 | 默认 | 何时 |
|------|------|------|
| `large_copy` | warn | 按值传递超过 64 字节的位结构体，见 [传递](passing.md) |
| `unused_must_use` | warn | 丢掉带 `#[must_use]` 的值。`expr;` 仍告。只有 `_ = expr`，或绑定到之后确实被读取的具名变量，才算处理过 |
| `unused` | warn | 绑定、参数、`use` 引入的名字从未被读。`_` 与 `_` 开头的名字不告 |
| `dead_code` | warn | 模块私有的 `fn` / `static` / 类型从未被引用（测试构建里 `#[test]` 项不算 dead） |
| `non_snake_case` | warn | 函数、方法、参数、局部绑定、字段或模块不符合 `snake_case` |
| `non_upper_camel_case` | warn | 类型、trait、枚举或枚举变体不符合 `UpperCamelCase` |
| `non_screaming_case` | warn | `const` 或 `static` 不符合 `SCREAMING_SNAKE_CASE` |
| `bad_initialism` | warn | initialism 在同一名称中大小写不一致 |
| `missing_docs` | warn | 对外可见项缺少有意义的 `///` 文档 |
| `long_line` | warn | 可拆分的代码行超过 100 列，或独立注释行超过 80 列 |
| `use_order` | warn | `use` 区域不符合规范排序 |

`#[must_use]` 可标在函数、方法、结构体、枚举、newtype 上。标在类型上：该类型的值被丢掉就告。标在函数上：返回值被丢掉就告。`Result` 与 `Option` 必须带。

风格 lint 的完整命名规则、注释要求和 `gugu fmt --check` 行为见[格式化与代码风格](format-style.md)。

### `track_caller`

标在 `fn` 上。`std.src.caller()` 返回调用链上**最近一个带 `#[track_caller]` 的函数的调用者**的源位置。

例：`panic` 必须带此属性。用户写 `panic("x")` 时，`panic` 体内的 `caller()` 得到 `panic("x")` 这一行，而不是 `panic` 定义里的行。若 `foo` 也带 `#[track_caller]` 且体内直接调用 `panic`，则 `caller()` 再往外走到 `foo` 的调用者。`std.process.exit` 同样必须带此属性。

`std.src.file()` / `line()` / `column()` 始终是**该调用表达式在源文件里的物理位置**（1 基；`column` 按该行的 Unicode 标量计），不受 `track_caller` 影响。

## 标识符

从 ASCII 字母或 `_` 开始，后接 ASCII 字母、数字、`_`。区分大小写。

禁止 Unicode 字母当标识符（避免同形字与规范化问题）。字符串与注释可含任意 Unicode。

## 关键字

`as` `align_of` `asm` `async` `break` `chan` `comptime` `const` `continue` `defer` `dyn` `else` `enum` `extern` `false` `fn` `for` `global_asm` `if` `impl` `in` `let` `loop` `match` `offset_of` `pub` `return` `select` `size_of` `static` `struct` `trait` `true` `try` `type` `type_id` `type_id_count` `union` `unsafe` `use` `while` `yield`

`self` 在 `impl` / trait 方法里按关键字处理。`Self` 只在 `impl` / `trait` 里表示当前类型，别处可当普通标识符。

`type_id` / `type_id_count` / `size_of` / `align_of` / `offset_of` / `chan` 用作关键字构造器，见 [类型](types.md)。

`mut` / `var` 不是关键字（绑定默认可变）。`ret` 只在 `defer ret` 里是修饰符。

`as` 只用于 `use` 别名（`use std.io as io2`、`use std.io.{print as p}`）。禁止 `x as T` 这种转换写法；标量转换见 [类型系统](types.md)。

`_` 不是关键字，但不能当值读。它只出现在绑定、参数、模式里，表示丢弃；同一作用域可以出现多次 `_`。

## 字面量

### 整数

- 十进制 `42`，十六进制 `0xFF`，二进制 `0b1010`，八进制 `0o755`
- `_` 分隔：`1_000_000`
- 无后缀。无约束时默认 `int`。超出目标类型范围必须报错。
- 禁止前导 `0` 表示八进制。

### 浮点

- `0.0`、`3.14`、`1e-9`
- 无约束时默认 `float`（binary64）
- **禁止** `.5` 与 `5.`，必须写 `0.5` / `5.0`（否则与元组字段 `.0` 冲突）

### 布尔

`true`、`false`。与整数之间无隐式转换。

### 字符

`'A'`、`'\n'`、`'\u{1F600}'`。必须恰好一个 Unicode 标量，类型 `char`。

### 字符串与字节

| 形式 | 含义 |
|------|------|
| `"..."` | `string`，有转义，**无**插值 |
| `f"..."` | `string`，有转义，有插值 `{expr}`；字面花括号写成 `{{` `}}` |
| `raw"..."` | `string`，无插值；只认 `\\` 与 `\"`，其余字符原样 |
| `raw"""..."""` | 同 `raw`，可跨行，可含单个 `"` |
| `b"..."` | `[byte; N]`（`N` 为字节数），有转义，无插值。可强制成 `&[byte]` |
| `b'x'` | `byte`。必须恰好一个字节（转义后） |
| `c"..."` | 静态、不可变、以 `0` 结尾的字节序列；类型 `*byte`。禁止内含 `0` 字节。不是 `string`，不校验 UTF-8 |

普通字符串转义：`\\` `\"` `\n` `\r` `\t` `\0` `\u{HEX}`。

`b"..."` / `b'x'` / `c"..."` 额外允许 `\xHH`（恰好两位数的十六进制，一个字节）。`b"..."` 与 `c"..."` 里 `\u{HEX}` 按 UTF-8 编码进字节；编码后若 `c"..."` 出现内含 `0` 则编译错误。`b'x'` 不允许 `\u{HEX}` 解码成多字节。

`raw"..."` 不得含未转义换行；跨行必须用 `raw"""..."""`。没有 `br"..."` / `cr"..."`。

`f"..."` 的 `{` `}` 里是完整表达式；表达式后可以写一个冒号和 Rust 风格静态格式说明，例如 `f"{id:08x}"`、`f"{value:?}"`、`f"{ratio:.digits$e}"`。格式码在编译期选择 `Print` / `Debug` / 进制或指数格式 trait；width 与 precision 的 `name$` 必须引用当前作用域的 `int` 绑定。表达式里的括号、方括号、花括号按嵌套配对；未配对且不属于格式说明的 `}` 结束插值。禁止在插值表达式里再写 `f"..."`。完整格式规则见[标准库 · 静态格式化](standard-library.md#静态格式化)。

### 数组与元组

- 数组：`[1, 2, 3]`；重复 `[x; N]`（`N` 必须 comptime）
- 元组：`(1, "a")`；`()` 是 unit；单元素必须 `(x,)`

## 记号

`( ) { } [ ]` `.` `,` `:` `;` `=` `==` `!=` `<` `>` `<=` `>=`

`+` `-` `*` `/` `%` `+=` `-=` `*=` `/=` `%=`

`&&` `||` `!` `~`

`&` `|` `^` `<<` `>>` `&=` `|=` `^=` `<<=` `>>=`

`#` `...` `=>` `?` `::` `..`

列表、参数、泛型实参、枚举变体实参允许尾逗号。

记号按最长匹配：从当前字符起，在上列记号里取能匹配的最长者。空白隔开记号，本身不是记号。因此 `&&`、`||`、`..`、`::`、`!=`、`+=` 等由多个运算符字符组成的记号，中间不能有空白。这是空白影响分词的地方：不是「缩进有意义」，而是最长匹配需要空白才能把相邻运算符字符拆开。

`&` 一元（类型或表达式里取引用）与二元（按位与）靠上下文区分。`&&` 是一个记号，只表示短路逻辑与，**禁止**把 `&&x` 解析成双重引用。引用的引用写成 `&(&T)`；取双重引用写成 `&(&x)` 或 `& &x`（两个一元 `&` 之间有空白，否则最长匹配得到 `&&`）。`a && b` 是逻辑与；`a & &b` 是按位与，右操作数是 `&b`。`||` 与两个 `|` 同理。

`!` 在**类型位置**是 never 类型；在表达式里是一元逻辑非。`!=` 是一个记号，不会拆成 `!` 与 `=`。类型位置是一切书写类型的语法位置：`:` 之后（绑定、参数、字段）、函数 `()` 之后的返回类型、`fn(...)` 的参数与返回、泛型实参、`&` / `*` / `dyn` 的操作数、`impl ... for` 的类型、数组与元组类型。不包括 `use ... as` 别名（`as` 只引入名字，从不引入类型）。`fn abort() ! { ... }` 的 `!` 是返回类型。

`~` 只在表达式里出现，是整数的按位取反，不是逻辑非。

## 分词确定性与错误

词法分析从左到右进行。当前位置若能匹配多个记号，取最长记号；长度相同则只有一个合法记号，否则是词法错误。字符串、字符和注释内部使用各自的转义规则，不重新应用普通记号的最长匹配。

数字记号中不能出现两个基数前缀、两个小数点或两个指数标记；下划线只能出现在两个数字之间，不能出现在记号首尾或基数前缀之后。整数和浮点记号后直接出现 ASCII 字母或数字是词法错误，不会拆成数字加标识符。负号、正号都不是数字字面量的一部分。

普通字符串、插值字符串、raw 字符串、字节字符串和 C 字符串必须在同一文件中闭合；未知转义、非法 Unicode 标量、非法 UTF-8、C 字符串中的内嵌零字节以及字节字符编码超过一个字节都是编译错误。插值中的表达式必须完整闭合，插值结束后恢复外层字符串扫描。

圆括号和方括号内部的换行只作为空白；花括号内部若当前块语句、字段、枚举变体、match/select 臂已经完整，换行可以按[形式语法](syntax.md)产生 `terminator` 或相应 separator，否则仍是空白。块内上一项已经完成且下一记号能开始新语句时，换行产生 `terminator`。一行不能同时以两个独立语句结束；分号只执行形式语法规定的丢值作用。

关键字在词法阶段与标识符区分。`self` 只有在方法参数和方法体对应的特殊位置使用；`Self` 只有在 trait/impl 类型上下文代表当前类型。其它位置出现保留关键字、孤立 `_` 值或不允许的记号均为编译错误。

## 属性适用位置与冲突

属性只能用在下列位置；其它附着位置是编译错误：

| 属性 | 允许附着于 |
|------|------------|
| `inline`、`cold`、`track_caller` | 有函数体的函数或方法 |
| `repr(...)` | 结构体、newtype、枚举、union；具体组合按[类型系统](types.md) |
| `derive(...)` | 结构体、newtype、枚举 |
| `cfg(...)` | 模块、声明、字段、枚举变体、match/select 臂、块内语句或列表元素 |
| `must_use` | 函数、方法、结构体、newtype、枚举 |
| `allow`、`warn`、`deny`、`forbid` | 模块、声明、表达式 |
| `test`、`should_panic`、`ignore` | 具名函数；`should_panic`/`ignore` 必须同时有 `test` |
| `bench` | 具名函数；只由内建 benchmark harness 收集 |
| `coroutine_local`、`os_thread_local` | `static`，且二者互斥 |
| `export_name`、`link_name`、`link_section`、`used`、`naked` | [unsafe 与 intrinsic](unsafe.md)规定的函数、static 或汇编项 |
| `ffi(leaf[, stack = N])` | 无函数体的 `extern "C"` 导入项或 `#[naked] unsafe extern "C" fn` |
| `ffi(bridge)` | 直接调用导入 `extern "C"` 函数的表达式 |
| `ffi(dirty_cpu)` | 无函数体的 `extern "C"` 导入项、带函数体的 `unsafe extern "C" fn` 或直接 C 调用表达式 |

同一属性重复出现必须语义一致；重复但参数不同、互斥 repr、两个存储属性、`inline` 与 `cold` 同时出现、`test` 与 `bench` 同时出现，或 test/bench 与 `extern`/`naked`/`unsafe fn` 冲突，都是编译错误。`ffi(leaf)` 附着在非 extern 导入且非 naked C 函数、`stack` 不是非负整数常量或重复指定不一致、`ffi(bridge)`/`ffi(dirty_cpu)` 附着在非许可位置、两者出现在同一调用点、带函数体的 `ffi(dirty_cpu)` 缺少 `unsafe extern "C"` 或包含 managed operation，都是编译错误。`naked` 与 `ffi(dirty_cpu)` 同时出现是冗余属性并报错；`cfg` 可以重复，效果是所有谓词的逻辑与；lint 属性按从外到内的作用域覆盖规则合并。

`cfg(false)` 的节点在解析后、名称解析前删除。它只允许附着于删除后语法仍完整的序列成员；不能删除调用目标、赋值右侧、函数唯一返回类型或其它单一必需表达式。被删除节点的名称、类型和属性参数不再检查，但其外层记号必须已经能成功解析。

## 诊断输出契约

编译错误和升为错误的 lint 必须使编译器返回非零状态且不得留下可执行镜像。警告不改变返回值或程序语义。诊断至少包含级别、稳定的诊断名字、主消息、主源范围；涉及重名、impl 重叠、类型来源或属性覆盖时还必须标出相关的次级范围。

同一输入的诊断顺序必须确定：先按规范化文件路径、主范围起始位置、错误优先于警告、诊断名字排序。并行解析、单态化或测试收集不能改变顺序。UTF-8 文本输出写到 stderr；实现可以另提供机器可读格式，但不能以机器格式替代规范文本中的物理源位置。

解析恢复可以继续报告后续错误，但不得把一个已知语法错误产生的占位节点当作有效程序进入 codegen。实现可以限制单次报告数量，达到上限时必须明确说明仍有诊断被省略。
