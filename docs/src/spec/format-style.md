# 格式化与代码风格

本章规定 Gugu 源码的规范化排版、命名、注释、公共 API 文档和风格诊断。语法与空白对程序的语义影响见[词法结构](lexical.md)；本章只规定在语法已经成立时，工具应如何产生稳定源码，以及代码应遵循的可读性约定。

本章的格式规则是工具链契约，不是可由项目配置覆盖的偏好。`gugu fmt` 是唯一的官方格式化器；不存在第二种官方格式风格、项目级格式配置或 style edition。代码风格规则中属于 lint 的部分仍可通过既有 lint 级别机制选择是否阻止构建。

## 规范层级

格式化和代码风格分为三层：

1. **语法约束**：由编译器强制，例如记号、属性适用位置、字符串闭合和换行是否结束语句。违反它们是编译错误。
2. **规范排版**：由 `gugu fmt` 产生的唯一稳定布局。`gugu fmt --check` 可以把源码是否符合该布局作为持续集成条件。
3. **风格 lint**：不改变类型或运行时语义的命名、文档和可读性诊断。默认级别见[风格 lint](#风格-lint)。

规范排版不得改变程序的 AST、字符串和字节常量内容、文档注释正文或属性参数值。格式化后 `std.src.file()`、`line()` 和 `column()` 仍按实际格式化源文件报告调用位置；排版改变物理行列是允许的，不得改变这些 API 的计数规则。格式化器不得借排版机会重排声明、改变表达式求值顺序、改写字面量值或删除显式丢值分号。

## `gugu fmt`

### 文件选择

无路径选择器时，`gugu fmt` 格式化当前 package 的源码文件和 package 根的 `build.gg`。它不读取或改写 `target/`、全局缓存、`vendor/`、VCS 数据和发布归档中的临时副本。`--all` 格式化当前 workspace 所有成员的相同文件集合；workspace 成员顺序按规范化相对路径排序。

`gugu fmt` 不格式化 `gugu.toml`、`gugu.lock` 或外部依赖源码。清单和锁文件的字段顺序、路径表示与发布约束由[包、依赖与构建模型](packages-builds.md)和[发布与生态](publishing-ecosystem.md)规定。编辑器可以在保存时调用 `gugu fmt`，但不能用自定义排版器宣称源码符合 Gugu 规范。

### 写入与检查

`gugu fmt` 默认写回格式化后的文件；每个文件只有在完整解析且格式化成功后才可以替换原文件。替换使用同目录临时文件和原子重命名，格式化失败不得截断或留下半份源文件。实现可以保留文件的所有者和访问权限，但不能把宿主换行、时间戳或绝对路径写入源码。

`gugu fmt --check` 只检查，不写入任何源文件。存在不符合规范的文件时，它报告文件路径和差异摘要并返回退出码 `1`；所有文件已经符合规范时返回 `0`。源文件无法解析、编码非法或 I/O 失败也返回非零，并按[工具链与命令行](toolchain-cli.md)输出诊断。

格式化输出具有以下稳定性保证：

- 对同一 formatter 构建身份、同一输入字节和同一格式规范，结果确定；文件系统枚举顺序不能影响结果。
- 结果再次运行 `gugu fmt` 不再变化，即 `fmt(fmt(source)) = fmt(source)`。
- 默认使用 LF 换行；文件末尾恰好有一个换行符。
- `--check` 不修改访问时间、权限、缓存、锁文件或 build.gg 生成物。
- 格式化器不执行 build task、不下载依赖、不运行用户程序，也不解析 FFI 指向的外部源码。

`--format json` 时，格式化事件使用全局 JSON 输出协议：每个不符合文件产生一个 `fmt-diff` 对象，成功结束产生一个 `fmt-result` 对象。对象至少包含规范化文件路径、是否发生差异和检查结果；实现不得把统一 diff 的控制字符嵌入 JSON 字符串而不转义。`--format json-diagnostic-short` 只输出格式解析错误，不输出进度事件。

## 文件与换行布局

### 缩进与宽度

- 缩进只使用空格，每级四个空格。
- 行首用于缩进的制表符必须被输出为相同宽度的空格；字符串、字符、字节串、raw 字符串和注释正文内部的制表符按原字节保留。
- 代码行的目标宽度为 100 个 Unicode 标量；格式化器使用块缩进，不使用为了对齐列而向右漂移的视觉缩进。
- 单个标识符、数字、字符串、字符、字节串、C 字符串和其它不可拆分记号超过宽度时不得拆分。此时保留完整记号，并由 `long_line` lint 报告可诊断的超长行。
- 独立注释行的目标宽度为 80 个 Unicode 标量；注释正文不因达到宽度而丢失或改写。

### 空行与结尾

连续模块项、语句和表达式元素之间可以有零或一个空行；格式化器不得产生两个以上连续空行。模块级声明按源文件原有顺序保留；不同声明不会因为类型相同而自动合并。

块的开括号与控制头、函数签名或声明放在同一行，除非行宽要求换行参数表。`else`、`else if` 和 `catch` 与前一个闭括号处于同一逻辑行。空块使用 `{}`；含内容的块在开括号后换行，在闭括号前换行。

逗号分隔的列表在多行布局时必须保留尾逗号，适用对象包括参数、实参、泛型实参、数组元素、元组元素、结构体字段、枚举变体和属性参数。单行列表不添加无语义需要的尾逗号。

分号不是普通语句终止符。格式化器必须保留用于丢弃表达式值的显式分号，也不得把换行结束的语句改写成依赖分号的形式。换行是否结束语句仍完全按[词法结构](lexical.md)的未完成记号规则判断。

### 记号间空白

二元运算符两侧各有一个空格；赋值和比较运算符遵循相同规则。逗号后有一个空格；冒号后有一个空格，类型标注中的冒号前无空格。成员访问的`.`、路径的`::`、范围的`..`和一元运算符不插入多余空格。

括号、方括号和花括号的内部空白按语法类别统一：调用和索引的左括号前不加空格，单行空集合写作 `[]`、`{}` 或 `()`；结构体字段块使用换行布局，不在花括号内添加填充空格。函数返回类型直接跟在 `)` 后，返回类型与 `{` 或 `=` 之间保留一个空格。

示例：

```text
fn clamp(value: int, low: int, high: int) int {
    if value < low {
        return low
    }
    if value > high {
        return high
    }
    value
}

let values = [
    1,
    2,
    3,
]
let result = transform(value).filter(predicate).collect()
```

### 短项与表达式体

只含简单标识符、字面量或单一调用的短结构可以保持单行；一旦单行会超过宽度或需要视觉对齐，使用块缩进。表达式体函数保留 `= expression` 形式，不为排版改成块体；块体最后的值不添加人为 `return`。

链式调用按语义分段换行，每个后续调用使用一个块缩进。条件表达式、`match`、`select` 和闭包的分支按块布局；分支体内部继续使用相同规则。格式化器不得通过改变括号数量来依赖不同的运算符结合顺序。

## 声明、导入与属性

### `use` 顺序

同一模块中的 `use` 声明不依赖源文件先后顺序；格式化器按以下键对连续的 `use` 区域稳定排序：

1. `std` 路径；
2. 当前 package 的绝对模块路径；
3. 外部 package 路径；
4. 同一路径下，未导出项、`pub use`、别名按规范化完整文本排序。

排序按 Unicode 标量的字节序比较路径和名称，大小写保持区分。被独立注释或空行分隔的区域分别排序；注释作为其后一个 `use` 项的附属文本移动，不跨越其它声明。两个 `use` 目标发生别名冲突时，格式化器不修复或隐藏编译错误。

```text
use std.io.{print, println}
use std.path as path

use acme.json
use acme.net.{Client, Request as HttpRequest}
```

`use` 的排序不能改变名称解析结果；若未来增加会使 `use` 顺序具有语义的声明形式，编译器必须先保持名称解析顺序无关，再纳入格式规范。

### 属性

每个属性独占一行，缩进与它附着的声明相同。外部文档注释位于所有属性之前；模块级内部文档注释仍位于文件开头。属性参数列表按普通函数实参的换行和尾逗号规则排版。

同一声明只输出一个 `derive` 属性，并保留其派生名称的源顺序。`repr`、`cfg`、lint 和链接属性各自保持独立行；格式化器不合并属性，也不把属性从声明移到表达式。

```text
/// 将两个坐标相加。
#[derive(Clone, Eq)]
#[repr(C)]
pub fn add(left: Point, right: Point) Point = Point {
    x: left.x + right.x,
    y: left.y + right.y,
}
```

### 注释与文档

普通注释优先使用 `//`，文档注释使用 `///`，模块文档使用 `//!`。注释 sigil 后保留一个空格；行尾注释与前一记号之间保留一个空格。块注释可以嵌套，格式化器不把块注释强制转换为行注释。

注释正文、代码示例、Markdown 标记、URL、诊断代码和 fenced code block 内的字节必须保留。格式化器可以整理注释 sigil、缩进和块注释边界，但不自动重排自然语言段落，不自动修正拼写，不把文档中的代码当作源代码重新格式化。

声明注释应是完整句子，并说明目的、前置条件、错误、panic、并发限制、资源寿命或 ABI 承诺中与调用者有关的部分。注释解释“为什么”而非复述下一行代码；临时决策应记录可移除条件和关联 issue，而不能用注释掩盖未实现行为。

公共模块、公共函数、公共类型、公共字段、公共 trait、公共 trait 方法和公共常量应有有意义的 `///` 文档。编译器通过 `missing_docs` lint 报告缺失文档；生成模块和明确不对外承诺的内部项可以在其模块范围调整 lint 级别。

## 命名与 API 风格

### 标识符命名

Gugu 标识符只能使用 ASCII 字母、数字和下划线；本章在此基础上规定默认风格：

| 对象 | 规范命名 | 示例 |
|------|----------|------|
| 类型、trait、枚举和枚举变体 | `UpperCamelCase` | `HttpClient`、`ParseError` |
| 函数、方法、参数、局部绑定、字段、模块 | `snake_case` | `parse_header`、`user_id` |
| `const` 和不可变 `static` | `SCREAMING_SNAKE_CASE` | `MAX_RETRIES` |
| 可变 `static` | `SCREAMING_SNAKE_CASE` | `GLOBAL_STATE` |
| package owner/name | 小写 ASCII 与 `-` | `acme/http-client` |
| feature | 小写 ASCII、数字、`_` 或 `-` | `default-tls` |

缩写和 initialism 按普通单词处理：写 `HttpServer`、`parse_http`、`user_id`，不要在 UpperCamelCase 中写 `HTTPServer` 或 `Httpserver`，也不要写 `user_ID`。专有外部名称可以通过 `#[export_name]` 或 FFI 字符串保留其 ABI 拼写；源标识符仍遵循本表，必要时由命名 lint 局部放宽。

模块名使用描述性 `snake_case`，避免无语义的 `util`、`common`、`misc`、`types` 和 `api`。package 名已经由清单规则限制；模块名和 package 名不要求相同。预导入的 lang item 名称按语言规范固定，不因本地风格改名。

### 转换与访问器

简单字段访问器直接使用字段名或描述性方法名，不为没有歧义的读取方法添加 `get_` 前缀。方法前缀按成本和所有权表达意图：

- `as_` 表示廉价的借用视图或类型解释，不转移所有权；
- `to_` 表示产生新值或执行可能有成本的转换；
- `into_` 表示消耗接收者并转移所有权。

这些命名是 API 风格 lint 的可诊断约定，不改变方法解析或类型检查。外部协议、兼容 API 和 `extern "C"` 的公开名称可以保留既有名称，但应在文档中说明原因。

### 错误、panic 与生命周期文档

返回 `Result` 或 `Option` 的公共 API 必须说明错误或空值出现的条件；可能 panic 的 API 必须说明触发前置条件。拥有文件、锁、channel、进程、订阅或其它资源的类型必须说明释放动作和句柄复制语义。异步函数和跨协程 API 必须说明取消、等待和线程迁移限制。

`panic` 只用于调用者无法通过正常结果处理的程序不变量或语言定义的失败；可预期的输入、权限、网络和文件错误使用 `Result`。这是一项代码审查约定，不会把所有 `panic` 自动变成编译错误。

## 风格 lint

风格 lint 使用[词法结构](lexical.md)已有的 `allow`、`warn`、`deny` 和 `forbid` 级别，也可以由工具链的 `--warn`、`--deny` 和 `--forbid` 参数设置。未写属性时采用下表默认级别：

| 名字 | 默认 | 诊断条件 |
|------|------|----------|
| `non_snake_case` | `warn` | 函数、方法、参数、局部绑定、字段或模块不符合 `snake_case` |
| `non_upper_camel_case` | `warn` | 类型、trait、枚举或枚举变体不符合 `UpperCamelCase` |
| `non_screaming_case` | `warn` | `const` 或 `static` 不符合 `SCREAMING_SNAKE_CASE` |
| `bad_initialism` | `warn` | initialism 在同一名称中大小写不一致 |
| `missing_docs` | `warn` | 对外可见项缺少有意义的 `///` 文档 |
| `long_line` | `warn` | 可拆分的代码行超过 100 列，或独立注释行超过 80 列 |
| `use_order` | `warn` | `use` 区域不符合本章稳定排序 |

风格 lint 不检查字符串、raw 字符串、代码块文档中的自然语言行宽，也不把 `extern "C"` 的导出字符串当作 Gugu 标识符。属性参数引用的名字仍按其实际声明种类检查。未知风格 lint 名称是编译错误；实现可以增加新的 lint，但必须使用不与现有名称冲突的稳定名字并在工具链帮助中列出。

`gugu fmt --check` 和风格 lint 是两项独立检查：格式差异不会通过 `#[allow(long_line)]` 消除，命名或文档 lint 也不会改变 formatter 的输出。持续集成若要求源码干净，应同时执行 `gugu fmt --check` 与目标 lint 检查。

## 生成源码与外部边界

`build.gg` 产生并通过 `std.build.emit_module` 注册的源码仍必须是合法 Gugu；生成器输出应直接生成规范格式。`gugu fmt` 不进入全局缓存或 `out_dir()` 中的旧生成物，不因格式化当前 package 而执行生成器。下一次构建若需要生成模块，生成器负责产生其格式；生成模块的内容哈希进入构建缓存 key。

`vendor/` 中的依赖源码属于外部 package，工具不因 `--all` 自动改写它们。源码归档中的 `.gg` 文件在发布前应已经符合规范格式，但 registry 不得通过格式化器改变已发布归档或其校验和。

`extern "C"` 声明、`asm`、`global_asm` 和 `#[naked]` 代码的内部空白仍按本章排版；汇编字符串、C 名称、链接节名和 ABI 参数是不透明字节，不得被 formatter 重排。相关安全前置条件见[unsafe 与 intrinsic](unsafe.md)，C 布局和调用约定见[平台与 ABI 参考](platform-abi.md)。

## 兼容性与审查

规范格式的变更必须先更新本章并说明 formatter 输出变化、迁移规则和是否会改变诊断位置。格式化器版本不能写入 `gugu.toml` 或 `gugu.lock`；编译器构建身份仍按[包、依赖与构建模型](packages-builds.md)进入编译缓存 key。格式变化不得改变语言 ABI、package ID、归档校验和或运行时语义。

代码审查至少检查：

- `gugu fmt --check` 无差异；
- 公共项有完整文档，错误、panic、资源和并发前置条件已写明；
- 命名符合本章，initialism 在整个 package 内一致；
- `use` 区域和属性顺序稳定；
- 注释解释非显然的设计原因，而不是复制实现；
- FFI、unsafe、生成源码和外部边界没有被排版工具改变其不透明字节。

## 设计参考

本章的机器格式化和风格边界参考以下官方资料；这些资料用于解释取舍，不覆盖 Gugu 已固定的语法和 lint 语义：

- [Rust Style Guide](https://doc.rust-lang.org/style-guide/)
- [rustfmt Book](https://rust-lang.github.io/rustfmt/)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- [Go Effective Go：Formatting](https://go.dev/doc/effective_go#formatting)
- [Go Code Review Comments](https://go.dev/wiki/CodeReviewComments)
