# 标准库

本章规定标准库的公开边界、模块组织和语言直接依赖的接口。标准库不是可替换 runtime：它与程序、GC、调度器一起参加闭世界编译，但普通接口仍以 Gugu 源码实现；只有 syscall、原子、栈切换、GC 屏障和同级底层能力使用 intrinsic。

## 公开边界

工具链只提供一个保留 package：`std`。不公开 `core` / `alloc` 分层，也不定义脱离 GC、调度器或 runtime 的第二套 `no_std` 程序模型。

公开模块至少包括：

```text
std.option        std.result        std.iter          std.ops
std.error         std.cmp           std.text          std.fmt           std.hash
std.collections   std.io            std.path          std.fs
std.net           std.process       std.env           std.time
std.random        std.math          std.sync          std.resource
std.mem           std.ptr           std.ffi           std.panic
std.runtime       std.signal        std.src           std.syntax
std.test
std.build         std.hint
```

`std.runtime` 与 `std.signal` 是公开的运行时控制和信号订阅 facade；runtime、collector、scheduler、platform 和 intrinsic 的实现模块仍是 `std` 的私有实现，不是 package API。用户不能在清单、依赖别名或源树中定义 `std`，见[包、依赖与构建模型](packages-builds.md)与[声明与模块](declarations.md)。

`std` 的私有实现模块由工具链随 compiler 内建源树注入，与用户源码走同一条解析、cfg 与检查路径。它们不是可导入的 package API：只有 `std` 内部的模块可以互相引用，非 `std` 模块导入 `std.runtime`、`std.platform` 等实现模块一律按保留名拒绝（`E0031`）。用户源码占用内建逻辑路径同样是 `E0031`。

## 平台范围与内存账本 {#platform-ranges-ledger}

`std.platform` 是 `std` 的私有实现模块，向 runtime、collector 与调度器提供平台范围原语。它们绑定 IR 原语，只能在 `unsafe` 块内调用（`E0041`），且不接受类型实参：

```text
reserve_aligned(bytes, alignment) RangeId
commit(range) uint            decommit(range) uint       release(range)
protect_guard(range)          unprotect(range)           zero(range)
wait(word, expected) bool     wake(word, count) uint     entropy(bytes) *byte
set_dump_policy(range, policy)                           low_memory_hint() bool
huge_page_hint(range)
```

固定操作集合、二次幂 extent 阶梯、range 状态迁移与失败分类由 compiler 持有的平台范围契约固定（当前 schema 2）；`commit` 与 `decommit` 返回无符号平台状态字，0 表示成功，非零交给 runtime 的平台错误处理。只有成功后才能更新提交标志和 committed 账本；decommit 失败保持原提交状态，成功仅归还提交页、保留虚拟 reservation，之后复用必须重新 commit。`std.platform` 的每个入口都对应契约目录中的一项，新增操作必须同时登记契约、Gugu 源码与 GIR/LIR 原语。

平台范围的生命周期是 `reserved → committed → decommitted → released`。`reserve_aligned` 只取得虚拟地址，不占物理页；`commit`/`decommit` 以平台页为粒度推进同一 range 的子区间；`release` 归还整个 range。地址只在 `reserved` 与 `committed` 期间有效，`decommitted` 与 `released` 的 range 不得解引用。

保护边界可在 reservation 尚未提交时建立，不隐式提交 payload。内部 stack arena 使用首尾各一页的保护范围，逐栈分配与回收不改变 arena 内部页权限；已受保护范围不能被普通子区间 commit 重新开放。未提交保护页只计入预留虚拟容量，不能作为已提交物理页重复记账。

每个 owner 在每个 memory domain 上持有自己的 extent arena，arena 按二次幂 class 阶梯切分。`decommit` 只在四类条件同时成立时发生：allocator、scanner、forwarder 三路 lease 全部归零，extent 上没有 live 或 queued slot，没有在途 return 消息，且 queue-page grace 已走完固定步数。任一条不成立时范围保持 committed，诊断必须点名未满足的具体条件；不得因为「看起来空闲」就撤销仍被使用的页。

平台失败统一映射为三个类别，Linux 与 Windows 的映射逐项一致：`OutOfMemory`、`ResourceExhausted`、`RuntimeInvariant`。同一失败在两个目标上必须落到同一类别，契约 verifier 拒绝任何 profile 之间的漂移。失败类别是诊断与恢复策略的依据，不是可捕获的异常。

内存压力统计不重复计数。`range_reserved_bytes` 只统计已预留但尚未提交的虚拟地址；`runtime_committed_bytes` 只统计已提交的物理页，两者严格互斥。`pending_return_bytes`、`owner_cache_bytes`、`reclaimable_bytes` 与 `live_bytes` 是 `runtime_committed_bytes` 的互斥分类，逐项相加恰好等于它；limit 判断不得把同一物理页计入多个分类。完整字段口径见[运行时](runtime.md#gc-stack-runtime-control-api)。

`std` 提供语言基座、集合、文本、格式化、I/O、文件、路径、transport 网络、进程、环境、时间、运行时控制、信号、同步、机器数值、随机、FFI、测试和构建接口。JSON、正则表达式、压缩、密码学、TLS、HTTP、WebSocket、QUIC、数据库、时区数据库、命令行框架和大文本 Rope 不属于 `std`；它们可以由官方 Registry package 提供，并独立于工具链发布。工具链自身的命令行接口见[工具链与命令行](toolchain-cli.md)。

## Prelude

除[声明与模块](declarations.md)已有的语言基座外，所有模块固定预导入下列名字：

```text
Error Default Hash StableHash StableOrd Print Debug Formatter Hasher
Read Write HashMap HashSet Path Duration
```

`Seek`、`BufRead`、`File`、socket、进程类型、其它集合和其它标准库项必须显式 `use`。Prelude 是封闭兼容面；标准库新增公开类型不会自动进入 Prelude。预导入名保留给 `std` 自己的定义：只有工具链随 compiler 注入的内建源单元可以声明它们（例如 `std.collections` 声明 `HashMap` 与 `HashSet`），用户源码声明同名项仍是 `E0031`。

## 错误模型

标准库使用具体、可穷举的领域错误。文件和通用 I/O 返回 `std.io.Error`，地址解析返回 `std.net.ParseError`，文本解析返回相应解析错误；不能把所有失败折叠进一个无边界的通用错误结构。

所有可作为通用错误处理的具体错误实现预导入的 `std.error.Error`：

```text
trait Error {
    fn message(self: &Self) string
    fn source(self: &Self) Option[&dyn Error]
}
```

`message()` 供人阅读，不是稳定机器协议；程序必须匹配具体错误的枚举变体或 kind，不能解析消息文本。`source()` 返回直接原因，没有原因时返回 `None`。错误链必须无环。具体错误另外实现 `Print` 与 `Debug`。

`?` 仍按[表达式与语句](expressions.md)要求传播相同错误类型；跨领域转换必须显式构造、`map` 或由调用者声明的转换完成，标准库不提供隐式错误装箱。

## comptime capability registry {#comptime-capability-registry}

官方编译器维护一份 compiler-owned、封闭的 comptime capability registry。凡是 lang item、
intrinsic 或标准库函数调用，只有按解析后的稳定身份登记且当前 comptime 执行域获准时才能
执行；编译器禁止按函数名称匹配，也禁止因为函数体当前看起来没有副作用就自动授予能力。
用户代码不能用属性扩展 registry。普通用户函数可以由 comptime 调用，但编译器必须传递
检查其全部静态 callee；最终遇到的每个语言/标准库操作仍须命中 registry。

registry 区分 `EarlyConst`、`SourceExpand` 与 `LateConst` 三个执行域，并为每个条目记录允许
的域、是否使用 evaluator heap、允许读取的显式输入、结果种类和 evaluator revision。未登记
调用或执行域不匹配产生 `comptime-capability` 编译错误，不能推迟到运行时，也不能用空结果
继续。标准库以 Gugu 实现的登记函数还必须由编译器构建验证器证明其 body 只调用同域允许
的能力；registry 声明与 body 不符是编译器构建错误。

标量运算、固定形状值的构造、`Option` / `Result` 构造与模式匹配、`?`、局部绑定和控制流是
evaluator 的语言核心，不作为可伪造的函数条目。当前规范要求 registry 恰好开放下列标准
能力组；未来增加或删除组必须改变 registry revision 和 compiler identity：

| 能力组 | comptime 执行域 | 允许的效果与输入 |
|---|---|---|
| `panic` | `EarlyConst`、`SourceExpand`、`LateConst` | 终止当前求值并产生编译诊断 |
| `size_of`、`align_of`、`offset_of`、符号化 `type_id`、`TypeId.name` | 三个域 | 只读已登记类型、布局与规范类型名；late 阶段不能发现新类型 |
| `type_id_count`、comptime `TypeId.as_int` 与依赖编号的序比较 | `LateConst` | 只读已冻结 type universe |
| `std.src.file`、`line`、`column` | `EarlyConst`、`SourceExpand` | 只读调用点的逻辑源范围 |
| `std.mem.embed_file` | `EarlyConst`、`SourceExpand` | 读取并登记受 package 输入根约束的文件 |
| `std.syntax.parse_source`、`parse_expr`、`parse_items`、`parse_type`、`parse_pattern` | `SourceExpand` | 只读 parser schema 与 source slot，返回 compiler-owned `ParsedSource` |
| `string` 的构造、容量/长度查询、修改、范围快照、字符/字节遍历与 `+` / `+=` | 三个域 | 只使用 evaluator heap；返回结果仍受各域的物化规则限制 |
| 静态 f-string 格式化、`Formatter` 写入，以及标量、string、`Option`、`Result`、固定数组和元组的内建格式 trait 实现 | 三个域 | 只写 evaluator 内的 string；用户格式 trait 实现继续做传递 capability 检查 |
| `SyntaxError` 的 `Error` / `Print` / `Debug` 实现 | `SourceExpand` | 只读确定的 parser 错误记录 |
| `Atomic::new`、`Mutex::new`、`RwLock::new`、`Condvar::new`、`OnceLock::new`、`Lazy::new` | `EarlyConst`、`SourceExpand` | 只构造可物化初始位状态，不执行同步、等待或发布 |

本表之外的标准库函数、方法和 intrinsic 默认不在任何 comptime 执行域中可调用；运行时
实现使用的纯函数也不会因此自动获得 comptime 能力。未来新增能力必须登记能力组、执行域、
输入和结果限制，并提升 registry revision 与 compiler identity。
`LateConst` 即使临时使用 evaluator string 或聚合值，最终也只能发布规范允许的标量叶值。
`SourceExpand` 返回 `ParsedSource` 是 compiler-owned 结果例外，不能物化为普通常量。registry
摘要、执行域和每个条目的 evaluator revision 都是编译输入；缓存命中不得绕过 capability
检查。具体表示与验证见[comptime 与抽象分析](../internals/comptime-analysis.md)。

## `std.syntax` 编译期源码解析

`std.syntax` 只在 `comptime` 脚本中可用，是语言级源码宏的解析入口。它使用当前
compiler identity 的 lexer/parser；它不是运行时字符串解析器，也不执行解析结果中的
代码。

```text
struct ParsedSource
struct SyntaxError

fn parse_source(text: string) Result[ParsedSource, SyntaxError]
fn parse_expr(text: string) Result[ParsedSource, SyntaxError]
fn parse_items(text: string) Result[ParsedSource, SyntaxError]
fn parse_type(text: string) Result[ParsedSource, SyntaxError]
fn parse_pattern(text: string) Result[ParsedSource, SyntaxError]
```
`SyntaxError` 实现 `std.error.Error`，其 `message()`、错误 kind、源文本范围和错误链均
是确定的；实现不得把 parser 内部地址、线程状态或随机恢复编号暴露给脚本。

`ParsedSource` 是 compiler-owned 的不透明片段值，只能由上述 `parse_*` 函数产生，且
只能在当前 comptime action 中保存、传递和返回给 `comptime source`。它不能构造、复制
到运行时对象、写入 `static`、发送到 channel、转成原始指针或物化进目标镜像。

`parse_source` 使用外围源码宏的 source slot；其它入口固定要求表达式、item 列表、
类型或模式。解析成功并不表示片段的名称、类型、trait、初始化、unsafe 或 ABI 约束
成立；宏展开后这些约束回到主编译前端检查。解析失败返回 `SyntaxError`，脚本可以
捕获并转换为自己的 `Error`；宏边界返回的最终 `Err` 才成为编译诊断。

## 可变 COW `string` {#mutable-cow-string}

`string` 是合法 UTF-8 的可变值，不是共享可变身份对象。赋值、参数传递、返回和模式绑定产生语义独立的 string 值；实现先共享只读 backing，直到任一值发生修改。

string 表示允许两种形态：

- 短 UTF-8 文本直接内联在 string 值表示中，不分配；具体内联上限是目标实现细节，不是 C ABI。
- 较长文本保存 backing、起始 byte offset 与 byte 长度。backing 处于 `Unique(owner_coroutine)` 或 `Sealed` 状态。

`Unique` 且覆盖完整 backing 的 string 可以原地修改。发生真实值复制、取得持久快照、写入可能逃逸的 GC 对象、发布到另一协程或作为集合键保存时，backing 单向变为 `Sealed`。修改 sealed、子区间或容量不足的值必须分离到新 unique backing；其它 string 值和旧快照不受影响。

编译器可以用闭世界逃逸摘要把只在调用期间读取的按值参数降为 borrow，把最后一次使用降为 transfer；这类优化不得 seal backing。分析失败时合法降级是 seal，不得产生 move、生命周期或 `Clone` 错误。跨协程发布必须在接收方可见该值之前完成 seal。

最低固有接口为：

```text
fn new() string
fn with_capacity(capacity: int) string
fn len(self: &Self) int
fn cap(self: &Self) int
fn is_empty(self: &Self) bool
fn clear(self: &Self)
fn reserve(self: &Self, additional: int)
fn shrink_to_fit(self: &Self)
fn push(self: &Self, value: char)
fn push_string(self: &Self, value: string)
fn insert(self: &Self, byte_offset: int, value: char)
fn remove(self: &Self, byte_offset: int) char
fn truncate(self: &Self, byte_len: int)
fn replace_range(self: &Self, range: Range, value: string)
fn byte_at(self: &Self, byte_offset: int) Option[byte]
fn char_at(self: &Self, scalar_index: int) Option[char]
fn chars(self: &Self) impl IntoIter
fn bytes_view(self: &Self) Bytes
fn normalize(self: &Self, form: NormalizationForm) string
```

`len`、capacity、range 与修改位置都按 byte 计。`char_at` 按 Unicode scalar 计并允许线性复杂度；需要全量处理时使用 `chars()`。负 capacity、负 additional 或负索引 panic。`insert`、`remove`、`truncate`、`replace_range` 和 string range 的端点必须在范围内且位于 UTF-8 scalar 边界，否则 panic。

string 不支持单整数 `s[i]`，避免把 byte、Unicode scalar 和用户可见 grapheme 混成同一种索引。`s[a..b]`、`s[a..]`、`s[..b]`、`s[..]` 返回 O(1) COW string 快照；创建快照会 seal backing。

`==`、`Eq`、`Ord` 与 `Hash` 按原始 UTF-8 byte 序列工作，不做隐式 Unicode normalization。规范等价文本必须先显式 `normalize`。Normalization 的形式为 `NFC`、`NFD`、`NFKC`、`NFKD`。

`+` 返回新 string；`+=` 修改左侧 string，并可以复用其 unique backing。string 字面量可以直接引用 sealed 只读镜像数据；第一次修改时分离。

COW 值语义的用法：

```gugu
fn demo() {
    let original = "gugu"
    let copy = original
    let modified = copy
    modified.push('!')
    // original 与 copy 仍是 "gugu"；修改 sealed backing 时分离
    _ = original
    _ = copy
    _ = modified
}
```

## 不可变 `Bytes`

`std.text.Bytes` 是不可变、可复制的 byte 快照值。它保存 backing、起始 offset 和长度；复制 Bytes 只共享不可变 backing。它不提供赋值下标、可写切片或转成可写 `&[byte]` 的安全接口。

```text
struct Bytes

fn new() Bytes
fn copy_from(source: &[byte]) Bytes
fn len(self: &Self) int
fn is_empty(self: &Self) bool
fn get(self: &Self, index: int) Option[byte]
fn slice(self: &Self, range: Range) Bytes
fn split_at(self: &Self, index: int) (Bytes, Bytes)
fn starts_with(self: &Self, prefix: Bytes) bool
fn ends_with(self: &Self, suffix: Bytes) bool
fn copy_to(self: &Self, destination: &[byte]) int
fn iter(self: &Self) impl IntoIter
fn thaw(self: &Self) ByteBuffer
```

`string.bytes_view()` 创建零拷贝 Bytes，并 seal string backing。之后修改 string 必须分离，Bytes 内容保持稳定。需要可变 byte 存储时使用 `Vec[byte]` 或 I/O 缓冲类型；从任意可写切片构造 Bytes 必须复制，不能让安全代码通过别名修改快照。

## 可变 `ByteBuffer`

`std.io.ByteBuffer` 是 string 的二进制同构：短载荷可以内联，堆 backing 处于 coroutine-local Unique 或 Sealed。赋值和按值传参保持独立值语义；真实复制、`freeze`、跨协程发布或写入共享 GC 图会 seal backing，之后修改任一副本时分离。

```text
struct ByteBuffer

fn new() ByteBuffer
fn with_capacity(capacity: int) ByteBuffer
fn len(self: &Self) int
fn cap(self: &Self) int
fn is_empty(self: &Self) bool
fn reserve(self: &Self, additional: int)
fn clear(self: &Self)
fn truncate(self: &Self, length: int)
fn get(self: &Self, index: int) Option[byte]
fn set(self: &Self, index: int, value: byte)
fn push(self: &Self, value: byte)
fn extend(self: &Self, source: Bytes)
fn freeze(self: &Self) Bytes
fn split_to(self: &Self, length: int) Bytes
```

`freeze` 创建覆盖当前内容的 O(1) Bytes 快照并 seal backing；ByteBuffer 本身保留内容，下一次修改时分离。`Bytes.thaw` 创建共享同一 sealed backing 的 ByteBuffer，第一次修改时分离。`split_to(n)` 返回前 n byte 的 Bytes，并把 buffer 换绑到剩余区间；越界或负数 panic。

ByteBuffer 是高吞吐 I/O 缓冲；`Vec[byte]` 仍是通用共享身份集合。从可写 `&[byte]` 或 Vec 构造 Bytes 必须复制，从 ByteBuffer freeze 则不复制。

## Unicode 文本

`std.text` 随工具链携带完整 Unicode 数据：UTF-8/UTF-16 编解码、Unicode scalar 属性、完整大小写、case folding、NFC/NFD/NFKC/NFKD normalization，以及 extended grapheme、word 与 line segmentation。正则表达式不因此进入 std。

```text
struct UnicodeVersion { major: int, minor: int, patch: int }
enum NormalizationForm { NFC, NFD, NFKC, NFKD }

fn unicode_version() UnicodeVersion
fn utf8_decode(source: Bytes) Result[string, EncodingError]
fn utf8_decode_lossy(source: Bytes) string
fn utf16_decode(source: &[u16]) Result[string, EncodingError]
fn utf16_decode_lossy(source: &[u16]) string
fn utf16_encode(source: string) Vec[u16]

fn string.to_lowercase(self: &Self) string
fn string.to_uppercase(self: &Self) string
fn string.case_fold(self: &Self) string
fn string.graphemes(self: &Self) impl IntoIter
fn string.words(self: &Self) impl IntoIter
fn string.lines(self: &Self) impl IntoIter
```

严格 decode 在第一个非法序列处返回包含 byte/code-unit offset 的 EncodingError；lossy decode 以 U+FFFD 替换每个 maximal invalid subsequence。graphemes 返回 extended grapheme cluster 的 COW string 快照，words 与 lines 按随附 Unicode 版本的默认边界算法返回快照。

工具链升级可以升级 Unicode 数据并改变属性、case mapping、normalization 或 segmentation 结果；`unicode_version` 使程序能够记录该版本。string 的默认 Eq、Ord 与 Hash 始终按原始 UTF-8 byte，不随 Unicode 表改变。

## 静态格式化 {#static-formatting}

f-string 的格式说明在编译期解析和类型检查。默认 `{value}` 要求 `Print`；其它格式能力由独立 trait 表达：

```text
trait Debug     { fn debug(self: &Self, out: &Formatter) }
trait Binary    { fn binary(self: &Self, out: &Formatter) }
trait Octal     { fn octal(self: &Self, out: &Formatter) }
trait LowerHex  { fn lower_hex(self: &Self, out: &Formatter) }
trait UpperHex  { fn upper_hex(self: &Self, out: &Formatter) }
trait LowerExp  { fn lower_exp(self: &Self, out: &Formatter) }
trait UpperExp  { fn upper_exp(self: &Self, out: &Formatter) }
```

格式码映射固定为：无格式码 → `Print`，`?` → `Debug`，`b` → `Binary`，`o` → `Octal`，`x` → `LowerHex`，`X` → `UpperHex`，`e` → `LowerExp`，`E` → `UpperExp`。整数、浮点、bool、char、string、Bytes、Option、Result、数组、元组和标准集合由语言或标准库提供适用实现。

格式说明采用 Rust 风格静态子集：fill/alignment、sign、alternate `#`、zero padding、width、precision 和 type code。width 与 precision 可以是编译期整数字面量，或用 `name$` 引用当前作用域中类型为 `int` 的绑定；负动态值是运行时 panic。格式能力不存在、标志与类型不兼容、未知格式码或格式说明未闭合都是编译错误。

标志与类型的兼容规则在推断收敛后按被格式化表达式的类型判定（`&T` 与 `T` 同类）：

- sign（`+`、`-`、空格）与 zero padding `0` 只适用于整数和浮点；
- precision 只适用于浮点（小数位数），以及 `Print` 格式下的 string（按 Unicode 标量截断）；
- `#` 只适用于 `Debug`（逐行缩进的结构化输出）与 `Binary` / `Octal` / `LowerHex` / `UpperHex`（`0b` / `0o` / `0x` 前缀）。

内建实现的输出固定为：整数 `Print` / `Debug` 是十进制并保留负号，进制格式按 64 位补码写出且不带负号；浮点 `Print` 不带 precision 时是最短往返表示，`Debug` 总是带小数部分，`e` / `E` 是尾数 1 位整数的科学计数，`NaN` / `inf` / `-inf` 不做 zero padding；bool 写 `true` / `false`；char 与 string 的 `Debug` 加引号并转义，string 的 `Debug` 不受 precision 截断。数值默认右对齐，文本默认左对齐；zero padding 在符号与 `#` 前缀之后补零。数组用 `[a, b]`、元组用 `(a, b)`、`Option` / `Result` 用 `Some(..)` / `Ok(..)` / `Err(..)` 的结构化 debug 形状。

`std.fmt.Formatter` 只写入当前构建中的 string，不执行 I/O。格式 trait 实现可以调用 Formatter 的文本（`write_str` / `write_char`）、padding（`pad` 用于文本、`pad_integral` 用于带符号与前缀的数字）和结构化 debug（`debug_tuple` / `debug_list` / `debug_map` / `debug_struct`）方法，但不能读取或改变已解析的格式说明；`#` 是否生效由这些方法自行体现。f-string 构建失败只可能是 panic（例如内存耗尽或负的动态计数），不返回领域错误。comptime 域中的 f-string 使用同一套解析、兼容规则与内建实现，见[comptime capability registry](#comptime-capability-registry)。

运行时 f-string 通过普通 trait 调用构建字符串；用户定义的 `Print` 实现与标准库实现使用相同的 `&Formatter` 目标。每个插值先求值一次，再读取该插值的动态 width / precision。`Formatter::new()` 建立空目标，`finish()` 返回当前字符串；每个插值的说明由编译器设置，不由用户格式 trait 修改。当前运行时标准库提供 string、char、bool 与全部整数类型的格式实现，以及上述文本和 padding 方法；浮点 `Print`（包括 `print` / `println` 等输出函数的浮点参数）、其它浮点格式与结构化 debug builder 尚未提供运行时实现，comptime 格式化不受此限制。

Formatter 追加文本不复制已有输出；累计追加 n 字节的存储量为 O(n)，`finish()` 线性物化当前内容的独立快照。

`pad` 与 `pad_integral` 的 width 均按 Unicode 标量数计算，不按 UTF-8 字节数计算；后者包括符号、启用的前缀和 digits 的标量数。多字节 fill 每次补一个标量，zero padding 仍在符号与前缀后补 ASCII `0`。

## 集合与 Hash {#collections-and-hash}

`std.collections` 首批稳定提供：

```text
Vec[T]
HashMap[K, V]        HashSet[T]
SecureHashMap[K, V]  SecureHashSet[T]
BTreeMap[K, V]       BTreeSet[T]
Deque[T]
BinaryHeap[T]
LinkedList[T]
BitSet
SmallVec[T, comptime N: int]
SmallMap[K, V, comptime N: int]
```

预导入的 `HashMap` / `HashSet` 与 `std.collections` 导出的同名类型相同；不需要显式 use 即可构造和使用。

MultiMap 不作为独立标准类型；使用 `HashMap[K, Vec[V]]` 或 `BTreeMap[K, Vec[V]]` 组合，重复值顺序由所选 Vec 明确表达。

Vec、Deque、BinaryHeap、LinkedList、BitSet、SmallVec 和全部 Map/Set 都是共享身份句柄：赋值只复制句柄，通过任一别名修改时，所有别名继续观察同一个逻辑集合。集合内部可以为快照迭代封存并替换 backing；这只改变物理表示，不把集合改成 COW 值语义。

`Vec.new()` 建立空集合，`Vec.from_slice(&[T])` 按顺序复制元素；`len()`、`is_empty()`、`get(index:int) -> Option[T]`、`set(index:int,value:T) -> bool` 和 `push(value:T)` 操作共享集合。负数或越界的 get 返回 None、set 返回 false；push 超过 int 可表示长度时 panic。`iter()` 和 IntoIter 保存创建时的元素快照；后来通过任一别名 push/set 不改变已有迭代器。Resolver 每次创建独立 Vec，返回后修改它不影响系统解析结果或其它调用。

Map 的普通读取返回 value 的语义副本，不公开会因 rehash、树旋转或紧凑化而失效的普通 `&K` / `&V`：

```text
impl[K: Eq + Hash + StableHash, V] HashMap[K, V] {
    fn new() Self
    fn len(self: &Self) int
    fn is_empty(self: &Self) bool
    fn get(self: &Self, key: &K) Option[V]
    fn contains_key(self: &Self, key: &K) bool
    fn with_ref[R, F: Fn(&V) R](self: &Self, key: &K, f: F) Option[R]
    fn for_each_ref[F: Fn(&K, &V) ()](self: &Self, f: F)
    fn insert(self: &Self, key: K, value: V) Option[V]
    fn remove(self: &Self, key: &K) Option[V]
    fn update[F: Fn(V) V](self: &Self, key: &K, f: F) bool
    fn entry(self: &Self, key: K) Entry[K, V]
    fn iter(self: &Self) HashMapIter[K, V]
}

impl[K: Ord + StableOrd, V] BTreeMap[K, V] {
    fn new() Self
    fn len(self: &Self) int
    fn is_empty(self: &Self) bool
    fn get(self: &Self, key: &K) Option[V]
    fn contains_key(self: &Self, key: &K) bool
    fn with_ref[R, F: Fn(&V) R](self: &Self, key: &K, f: F) Option[R]
    fn for_each_ref[F: Fn(&K, &V) ()](self: &Self, f: F)
    fn insert(self: &Self, key: K, value: V) Option[V]
    fn remove(self: &Self, key: &K) Option[V]
    fn update[F: Fn(V) V](self: &Self, key: &K, f: F) bool
    fn entry(self: &Self, key: K) BTreeEntry[K, V]
    fn iter(self: &Self) BTreeMapIter[K, V]
}

impl[K: Eq + Hash + StableHash, V] Entry[K, V] {
    fn or_insert(self: &Self, value: V) V
    fn or_insert_with[F: Fn() V](self: &Self, f: F) V
    fn and_modify[F: Fn(V) V](self: &Self, f: F) Self
}

impl[K: Ord + StableOrd, V] BTreeEntry[K, V] {
    fn or_insert(self: &Self, value: V) V
    fn or_insert_with[F: Fn() V](self: &Self, f: F) V
    fn and_modify[F: Fn(V) V](self: &Self, f: F) Self
}
```

`new` 建立空 Map；`len` 返回键数，`is_empty` 等价于长度为零。键按 Eq 相等或 Ord 比较等价视为同一键。`get` / `contains_key` 只查询；`insert` 在新键时返回 None，在已有键时替换 value 并返回旧值；`remove` 删除已有键并返回旧值，缺失时返回 None 且不改变长度。插入使长度超出 int 上界时 panic。BTreeMap 的查询、插入、删除使用 O(log len) 次键比较；其 `iter` 和 `for_each_ref` 都按键升序访问，每个键恰好一次。

SecureHashMap、SmallMap 保留与 HashMap 相同的 value 访问契约和约束，当前版本尚未实现其完整可执行 API。`with_ref` 在 key 存在时对 value 建立 `ScopedRead` view，调用 callback 并返回 `Some` 结果；key 不存在时不调用 callback 并返回 `None`。`for_each_ref` 在一次共享结构访问期间逐项调用 callback，每个键恰好一次，空 Map 不调用 callback，不创建 snapshot，也不复制 K/V。两者都不能在 callback 中修改同一 map：通过任一别名发出的 `insert`、`remove`、`update` 与 Entry 写入在 view 内都是 panic，包括缺失键或本不会改变内容的写操作。整个 `with_ref` / `for_each_ref` 调用位于接收者的 view 动态 extent 内；callback 的无 suspend、无 escape 和 safepoint relocation 规则见[函数与闭包](functions.md#scoped-borrowed-view-callback)。

`update` 只在键存在时调用一次 `f`，把当前 value 的语义副本交给它，再以返回值替换槽；存在时返回 true，缺失时不调用 f 并返回 false。`entry` 保存集合身份与键，不锁定当前是否存在；每次 Entry 操作都查询同一共享 Map 的当前状态。`and_modify` 等价于对该键 update 后返回 Entry；`or_insert` 在缺失时插入传入值，否则保留旧值；`or_insert_with` 只在缺失时调用一次 f 生成插入值。后两者返回最终 value 的语义副本。所有 Entry 操作只传入或返回语义副本，不产生集合内部引用。

Set 的元素约束与对应 Map 的键约束相同：

```text
impl[T: Eq + Hash + StableHash] HashSet[T] {
    fn new() Self
    fn len(self: &Self) int
    fn is_empty(self: &Self) bool
    fn contains(self: &Self, key: &T) bool
    fn insert(self: &Self, key: T) bool
    fn remove(self: &Self, key: &T) bool
    fn iter(self: &Self) HashSetIter[T]
    fn for_each_ref[F: Fn(&T) ()](self: &Self, f: F)
}

impl[T: Ord + StableOrd] BTreeSet[T] {
    fn new() Self
    fn len(self: &Self) int
    fn is_empty(self: &Self) bool
    fn contains(self: &Self, key: &T) bool
    fn insert(self: &Self, key: T) bool
    fn remove(self: &Self, key: &T) bool
    fn iter(self: &Self) BTreeSetIter[T]
    fn for_each_ref[F: Fn(&T) ()](self: &Self, f: F)
}
```

Set 的 `insert` 只在新增元素时返回 true，`remove` 只在实际删除时返回 true。`for_each_ref` 恰好访问每个现存元素一次，空 Set 不调用 callback；view 内通过任一别名写同一 Set 必须 panic。BTreeSet 的遍历为升序。四种 Map/Set 均实现 IntoIter；其关联迭代器实现 Iter，Map 的 Item 为 `(K, V)`，Set 为 T，耗尽后继续返回 None。

标准集合的 `iter()` 捕获创建时快照，产生元素的语义副本；Map 的 Item 是 `(K, V)`。Vec、HashMap、HashSet、BTreeMap、BTreeSet 创建迭代器的成本是 O(1)，之后通过任一集合别名插入、删除或替换值都不改变现有迭代器观察的键和值，每个快照元素恰好出现一次；这不递归复制元素内部的共享身份对象。内联 Small 容器和 LinkedList 创建迭代器时分别复制至多 N 个槽和当前全部节点值，以保持相同快照语义；因此它们的迭代器创建成本分别是 O(N) 上界和 O(len)。迭代器与集合可以同时存活，修改集合不会让 `next` panic，也不会让迭代结果弱一致。需要避免大值复制时使用 Map 的 `with_ref`、Map 的 `for_each_ref` 或 LinkedList 的 `for_each_ref`；这些 API 不改变 `iter()` 的 snapshot 语义。

SmallVec 与 SmallMap 的 N 必须是非负 comptime 整数。它们的 GC 对象内含 N 个连续内联槽，复制容器仍只复制共享身份句柄；元素超过 N 后溢出到堆 backing，别名、相等、修改和迭代语义不变。SmallMap 在内联阶段使用至多 N 项的紧凑线性查找，溢出后使用与 HashMap 相同的表表示；N 是调用方明确提供的小规模上界，不是运行时猜测阈值。

LinkedList 的节点是具有稳定 GC 身份且不复用代际的对象。`ListCursor[T]` 绑定 list 身份、node 身份与 node generation；`cursor_front`、`cursor_back`、`insert_before`、`insert_after`、`remove`、`splice_before`、`splice_after` 和 `value` / `update` 都返回 `Result[..., StaleCursor]`，不暴露节点引用。另提供 `for_each_ref[F: Fn(&T) ()](self: &Self, f: F)`：它在一次受保护遍历中直接访问节点值，不创建 snapshot或复制 T；callback 不能结构性修改该 list，且遵守 scoped view 的无 suspend、无 escape 和 relocation 规则。插入和同一 list 内的重排不使既有 cursor 失效；删除节点会使其全部 cursor 变为 stale。把另一 list 的节点 splice 进当前 list 时，目标 cursor 以及两边未移动节点的 cursor 仍有效，绑定到被移动节点旧 list 身份的 cursor 变为 stale。StaleCursor 是普通领域错误，不是 panic 或未定义行为。

HashMap/HashSet 的表示和内存布局不是稳定 ABI，可以随目标指令集和工具链改变。迭代顺序不保证，也可以在不同进程、不同集合和不同工具链间变化。

默认 HashMap/HashSet 使用 FoldHash-fast，seed 由实现选择，允许在不同进程和工具链间变化。它追求整数和 string 键的吞吐，不承诺抵抗能与进程交互并观察行为的 HashDoS 攻击。SecureHashMap/SecureHashSet 保留由 OS CSPRNG 初始化的 SipHash 1-3 契约，供不可信键使用；当前版本仅保留类型声明，其实现是后续工作。不能依赖具体 hash 值或迭代顺序。

`std.hash.Hasher` 是预导入的普通名义值类型，当前公开确定性 FoldHash-fast 接口：

```text
impl Hasher {
    pub fn with_seed(seed: uint) Hasher
    pub fn write_int(self: &Self, value: int)
    pub fn write_uint(self: &Self, value: uint)
    pub fn write_bool(self: &Self, value: bool)
    pub fn write_char(self: &Self, value: char)
    pub fn write_str(self: &Self, value: string)
    pub fn write_bytes(self: &Self, value: Bytes)
    pub fn finish(self: &Self) uint
}
```

`write_int` / `write_uint` 馈送 64 位小端补码 / 无符号整数；bool 馈送一个 0 或 1 字节，char 馈送 Unicode 标量的 32 位小端值。string 馈送 UTF-8，Bytes 馈送原始字节，两者均先馈送 64 位小端字节长度。所有写入拼成一个字节流，`finish` 等价于 Rust 参照模型对整个流执行一次 FoldHash-fast `FixedState::with_seed(seed)` 写入。重复 `finish` 不改变状态；之后仍可继续写入，下一次 `finish` 覆盖全部新旧输入。空输入也有定义。不承诺此输出跨工具链稳定。

内建 Hash 对不超过 64 位的有符号 / 无符号整数分别调用 `write_int` / `write_uint`（先扩展到 64 位），128 位整数按低、高两个 64 位字馈送；bool、char、string、Bytes 使用对应写入方法，unit 不写入。元组按字段顺序递归馈送；Option 用 64 位标签 0（None）/ 1（Some）后接载荷，Result 用 0（Ok）/ 1（Err）后接载荷。浮点不满足 Hash；数组与 TypeId 暂保留内建 trait 资格，但直接调用 Hash.hash 会明确编译报错。

当前不提供随机化 Hasher 构造器：原生平台尚未实现 entropy 执行片段。以上 seeded API 不替代集合的随机播种要求；集合实现及其默认播种路径单独提供。

`std.hash.Hash` 规定值向 hasher 馈送字段的语义顺序，并要求 `a == b` 蕴含 hash 输入相同。HashMap、SecureHashMap 及对应 Set 进一步要求键实现 `StableHash`：键的 Eq 与 Hash 可观察结果不能被任何外部别名改变。BTreeMap/BTreeSet 对称地要求 `Ord + StableOrd`，比较顺序不能被外部别名改变。string、Bytes、Path、标量以及字段全部满足同一稳定约束的值类型由编译器提供或安全派生这些 marker；插入 COW 键会封存其 backing。Vec、Map、resource 等可变身份句柄不能安全派生。用户只能用 `unsafe impl StableHash` / `unsafe impl StableOrd` 手写其余实现，并承担该类型的稳定性证明，见[接口](traits.md)与[unsafe](unsafe.md)。

默认 FoldHash 与 SipHash 输出都不是持久格式。`std.hash.XxHash3_64` 与 `XxHash3_128` 是算法命名的稳定非密码 hash；相同 byte 输入跨进程与工具链产生相同结果。它们只用于缓存键、测试、分片和非对抗内容摘要，不用于认证、签名或密码存储。

## Adaptive Resource Leasing {#adaptive-resource-leasing}

File、socket、Child、管道、锁守卫以及第三方 FFI 的外部资源都使用同一套自适应资源租约。资源值可以正常赋值、传参、返回和存入容器，不产生 move 错误；所有副本共享一个 `ResourceCell` 和 open/closed 状态。

ResourceCell 是 raw OS handle、open/closed 状态与租约的一份共享逻辑身份；物理 slab、计数器和 GC 交接只见 [GC 元数据](../internals/gc-metadata.md)。其发布语义从仅创建协程可达单向变为 Shared：

- 新资源最初只由创建协程可达；写入 global、channel、async 捕获或其它共享图时，发布操作先建立 happens-before，之后跨协程租约复制与 release 必须线程安全。
- 发布是单向语义状态；实现不能因对象后来只剩一个协程使用而撤销已经建立的共享同步。
- 参数传递、最后使用和相邻租约动作可以由实现合并，但不能改变 open/closed 状态、happens-before 或 release 次数；当前算法见 [GIR/LIR](../internals/gir-lir.md)。
- 最后一个可达租约结束时执行受限 release。显式 `close` 可以更早把共享状态原子地切到 closed。

所有公开资源的 `close()` 必须幂等：第一次成功关闭底层资源，之后返回成功且不重复执行 release。关闭后的其它操作返回具体错误的 `Closed` 变体。自动 release 不能报告错误；需要观察 `flush`、`commit`、`shutdown` 或 `Child.wait` 结果的程序必须显式调用相应方法。

`std.io` 提供组合式 I/O trait。可能阻塞的方法只挂起当前协程，runtime 通过有界 `BlockingBridge` admission 管理操作系统线程：没有 `BridgeCredit` 时不执行系统调用，调用方在等待队列中挂起；取得额度后才进入普通 `ForeignBridge`。`ForeignLeaf` 不得承载可能阻塞的 I/O，`ForeignBridge[DirtyCpu]` 不用于标准库文件操作。

含资源字段的受管值仍按 Adaptive Resource Leasing 保持一次性 release 语义；收集器物理移动不是语言复制，不能因此增加租约。资源只藏在不可达受管容器环中时，release 可以延迟到收集器发现该环；上一周期可达不构成本周期的根，残留扫描队列不得将已失去根的资源重新保活。栈和保存寄存器中的有效强根必须保留其可达资源；文件调用触发或跨越 GC 时，调用边界必须按栈图保存活跃根，并在恢复用户代码时使用收集器修正后的地址。未登记为根的字或已失效的初始化槽不能因残留地址而延长资源寿命。普通局部最后租约仍按[内存与对象模型](memory.md#adaptive-resource-leasing)结束。官方 descriptor 与 resource arena 实现见 [GC 元数据](../internals/gc-metadata.md)。

`MaybeUninit`、union、transmute、arena 批量释放和原始按位复制不能绕过资源租约语义，具体限制见[unsafe 与 intrinsic](unsafe.md)和[内存与对象模型](memory.md)。

## I/O

`std.io` 提供组合式 I/O trait。可能阻塞的方法只挂起当前协程，runtime 操作系统线程可以继续执行其它协程。

```text
trait Read {
    fn read(self: &Self, destination: &[byte]) Result[int, io.Error]
    fn read_cancel(self: &Self, destination: &[byte], cancel: &CancelToken)
        Result[int, io.Error]
    fn read_buffer(self: &Self, destination: &ByteBuffer, limit: int)
        Result[int, io.Error]
    fn read_buffer_cancel(self: &Self, destination: &ByteBuffer, limit: int,
                          cancel: &CancelToken) Result[int, io.Error]
}

trait Write {
    fn write(self: &Self, source: Bytes) Result[int, io.Error]
    fn write_cancel(self: &Self, source: Bytes, cancel: &CancelToken)
        Result[int, io.Error]
}

trait Seek {
    fn seek(self: &Self, from: SeekFrom) Result[uint, io.Error]
}

trait BufRead {
    fn fill(self: &Self) Result[Bytes, io.Error]
    fn consume(self: &Self, count: int)
}

enum SeekFrom {
    Start(uint)
    End(int)
    Current(int)
}
```

```text
enum Error {
    NotFound
    PermissionDenied
    AlreadyExists
    InvalidInput
    InvalidData
    UnexpectedEof
    WriteZero
    Interrupted
    TimedOut
    Cancelled
    Closed
    BrokenPipe
    NotConnected
    ConnectionReset
    ConnectionRefused
    ConnectionAborted
    AddrInUse
    AddrNotAvailable
    NotADirectory
    IsADirectory
    DirectoryNotEmpty
    CrossesDevices
    NotSeekable
    InvalidState
    MessageTooLarge
    Unsupported
    StorageFull
    ReadOnlyFilesystem
    FileTooLarge
    QuotaExceeded
    Other { code: int, source: Option[&Error] }
}
fn Error.message(self: &Self) string
fn Error.source(self: &Self) Option[&dyn Error]
fn Error.kind(self: &Self) Error
fn Error.with_source(self: &Self, cause: Error) Error
```

`Other` 是唯一带载荷的变体；`source` 以受管引用保存同一 `std.io.Error`，不是 `&dyn Error`，从而让递归错误链具有有限的值布局。`kind()` 去掉当前错误的来源载荷；`Other` 的 `kind()` 仍是 `Other { code, source: None }`。`message()` 只给人读，按变体名固定，不把 `code` 或来源文本拼进去。`source()` 只有 `Other` 且其 `source` 为 `Some` 时返回 `Some(&dyn Error)`，其余返回 `None`。`with_source` 不修改原值：结果是 `Other { code: self.kind 的稳定编号, source: Some(&cause) }`，引用使 cause 在返回后继续存活。稳定编号按上面枚举顺序从 0 开始，`Other` 是 30。错误链必须无环：`with_source` 若发现 `cause` 的链已经包含与 `self.kind()` 相同的编号，或链深超过 8，返回 `InvalidInput` 而不是构造环；这个失败本身不再包来源。

组合函数是 `std.io` 的自由函数，不是 trait 默认方法：

```text
fn read_exact[R: Read](reader: &R, destination: &[byte]) Result[(), Error]
fn write_all[W: Write](writer: &W, source: Bytes) Result[(), Error]
fn copy[R: Read, W: Write](reader: &R, writer: &W) Result[uint, Error]
fn read_to_end[R: Read](reader: &R) Result[Bytes, Error]
fn read_to_string[R: Read](reader: &R) Result[string, Error]
fn fill_buffer[R: Read](reader: &R, limit: int) Result[Bytes, Error]
fn write_buffer[W: Write](writer: &W, source: &ByteBuffer) Result[int, Error]
```

`read_exact` 循环 `read`，直到 destination 被填满。中途 `Ok(0)` 返回 `UnexpectedEof`；其它 `Err` 原样返回。已写入 destination 的前缀保留。`write_all` 循环 `write`，每次只把尚未写出的后缀再交给 `write`。`source` 非空且返回 `Ok(0)` 时返回 `WriteZero`；空 `source` 直接成功且不调用 `write`。`copy` 使用 8192 字节内部缓冲，返回成功写出的累计字节。读到 `Ok(0)` 正常结束；写到 `Ok(0)` 且仍有待写字节时返回 `WriteZero`。`read_to_end` 以 8192 为块调用 `read_buffer`，直到 `Ok(0)`，返回 `ByteBuffer.freeze()`。`read_to_string` 调用 `read_to_end` 后做严格 UTF-8 解码。非法序列返回 `InvalidData`，不使用 `with_source`；已读字节不回滚，但失败结果不返回部分 string。`fill_buffer` 把 `read_buffer` 的结果 `freeze` 成 `Bytes`。`limit < 0` panic，`limit == 0` 返回空 `Bytes` 且不调用 `read_buffer`。`write_buffer` 把 `ByteBuffer.freeze()` 交给一次 `write`，因此一次调用只写一个前缀。

`BufRead` 的默认适配也是自由函数，不改 trait 本体：

```text
fn buffered[R: Read](reader: &R, capacity: int) Result[Buffered[R], Error]
struct Buffered[R: Read]
fn Buffered.fill(self: &Self) Result[Bytes, Error]
fn Buffered.consume(self: &Self, count: int)
fn Buffered.read(self: &Self, destination: &[byte]) Result[int, Error]
```

`capacity < 0` 返回 `InvalidInput`；`capacity == 0` 合法，`fill` 每次直接向内部空缓冲读 0 字节并返回空 `Bytes`。`consume` 的 `count < 0` 或大于当前缓冲长度时 panic。`Buffered.read` 先消耗已有缓冲，缓冲空时 `fill`；底层 `Ok(0)` 才向调用方返回 0。

`Read.read` 成功返回写入 destination 的 byte 数；返回 0 表示 EOF，不能用 EOF error 同时返回部分数据。`read_buffer` 向 ByteBuffer 末尾直接追加至多 limit byte，负 limit panic，并允许实现写入 unique spare capacity；返回 0 同样表示 EOF。`Write.write` 可以成功写入短于 source 的前缀，返回 0 且 source 非空是 WriteZero。已经传输的 byte 不因后续错误回滚。取消发生在操作线性化完成前时返回 `Cancelled`；已经提交给对端或文件系统的 byte 仍然生效。

File、TcpStream、进程 pipe 和内存缓冲实现适用 trait。`print` / `println` 在格式化后写 stdout；它们不改变 `Print`/格式 trait 为纯文本构建能力的事实。

`print[Ts: Print...](...args: Ts)`、`println`、`eprint` 和 `eprintln` 均返回 `()`。它们按参数顺序用 `Print` 向一个 `Formatter` 追加文本，不插入分隔符；`println` / `eprintln` 最后追加一个 `\n`，所以空参数调用仍写出换行。`print` / `println` 写 fd 1（stdout），`eprint` / `eprintln` 写 fd 2（stderr）。空的 `print()` / `eprint()` 不发起写入。

这些便捷输出函数通过现有 ReportWrite 边界同步写出 UTF-8 字节，短写后继续写剩余部分；返回错误或零字节时静默停止，不重试错误、不 panic，也不返回 I/O 错误。已经写出的前缀保留，剩余文本丢弃。它们不承诺跨协程的整次调用原子性，也不采用上面通用 I/O trait 的挂起协议；需要错误处理或可取消写入的代码应使用 `Write` 等显式 I/O API。

Linux runtime 启动时将进程的 SIGPIPE disposition 设为 SIG_IGN。因此向读端已关闭的 pipe 输出不会因默认 SIGPIPE 动作终止进程；write 返回 EPIPE，以上便捷输出函数按写入错误静默停止。`Command.spawn` 的子进程在 fork 后、execve 前将 SIGPIPE 恢复为 SIG_DFL，不把 runtime 的忽略设置传给目标程序；恢复失败按子进程启动错误返回。Windows 行为不变。

## 取消

`std.sync.CancelSource` 创建共享的 `CancelToken`。取消是显式、幂等和协作式的：

```text
struct CancelSource
struct CancelToken
struct Cancelled

fn new() CancelSource
fn token(self: &Self) CancelToken
fn cancel(self: &Self)
fn is_cancelled(self: &CancelToken) bool
fn check(self: &CancelToken) Result[(), Cancelled]
```

阻塞 I/O、sleep、process wait 和 Join wait 提供接受 token 的变体。默认方法没有隐式 token；丢弃 Join 不取消子协程，`Child.wait_cancel` 也只取消当前等待者，不终止子进程。取消在 safepoint 和已登记的阻塞操作处被观察，返回 `Cancelled`，不是 panic。`cancel` 之前的普通写入与观察到取消的操作建立 happens-before。

尚未完成的 recv/Join 仍然是等待，不产生任何成功值；合法的位值 0 与未完成状态必须可区分。完成后必须保留通道关闭、Join 的位值、managed handle 与 descriptor、panic 等真实结果。挂起操作登记到取消源；正常完成与取消竞争同一个提交点，先提交的结果不能被后到的取消覆盖。完成、取消和错误退出都注销本轮等待，过期的等待节点不得影响复用后的新一轮等待。

## 路径

`std.path.OsString` 与 `Path` 是不可变、无损的跨平台值。Linux 保留任意非 NUL byte；Windows 保留合法 UTF-16。Path 的连接、替换文件名、父路径和扩展名操作返回新 Path，不修改原值。任何函数都只做字符串变换，不读取目录、不解析 `.` / `..`、不 canonicalize、不碰 symlink。

```text
fn OsString.from_string(value: string) Result[OsString, Error]
fn OsString.from_bytes(value: Bytes) OsString
fn OsString.from_units(value: &[u16]) Result[OsString, Error]
fn OsString.to_bytes(self: &Self) Bytes
fn OsString.to_units(self: &Self) Vec[u16]
fn OsString.to_string(self: &Self) Result[string, UnicodeError]
fn OsString.to_string_lossy(self: &Self) string
fn OsString.len(self: &Self) int
fn OsString.is_empty(self: &Self) bool

fn Path.from_string(value: string) Path
fn Path.from_os(value: OsString) Path
fn Path.as_os(self: &Self) OsString
fn Path.to_string(self: &Self) Result[string, UnicodeError]
fn Path.to_string_lossy(self: &Self) string
fn Path.join(self: &Self, child: Path) Path
fn Path.parent(self: &Self) Option[Path]
fn Path.file_name(self: &Self) Option[OsString]
fn Path.extension(self: &Self) Option[OsString]
fn Path.with_file_name(self: &Self, name: OsString) Path
fn Path.with_extension(self: &Self, extension: OsString) Path
fn Path.file_stem(self: &Self) Option[OsString]
fn Path.components(self: &Self) Vec[OsString]
fn Path.is_absolute(self: &Self) bool
fn Path.is_relative(self: &Self) bool

struct UnicodeError { offset: int }
fn UnicodeError.message(self: &Self) string
fn UnicodeError.source(self: &Self) Option[&dyn Error]
```

`UnicodeError` 属于 `std.path`，不是 `std.text`。`offset` 是原生 code unit 序号。`message()` 固定为 `"invalid unicode"`，`source()` 固定 `None`。`OsString.from_string` 与 `from_units` 的失败使用 `std.io.Error`。

Linux 原生序列是字节，分隔符是 `0x2F`。`from_bytes` 原样保存，包括非法 UTF-8；`from_string` 拒绝含 `0x00` 的 string，返回 `InvalidInput`，否则无损。`from_units` 在 Linux 不可用，返回 `Unsupported`。`to_string` 只接受合法 UTF-8，失败 `offset` 是字节序号。

Windows 原生序列是 UTF-16 code unit，分隔符是 `0x005C` 与 `0x002F`。`from_string` 按 UTF-16 编码且拒绝输入中的 U+0000。`from_units` 拒绝 lone surrogate 与 U+0000，失败 `offset` 是 code unit 序号。`from_bytes` 在 Windows 先按严格 UTF-8 解码，失败返回 `UnicodeError` 而不是 `io.Error`。`to_string` 拒绝 lone surrogate。

`is_absolute`：Linux 以 `/` 开头；Windows 以盘符加冒号加分隔符（`C:\`、`C:/`）或两个分隔符开头。其余都是 relative。`join`：`child` 为 absolute 时返回 `child` 的副本；否则父路径为空直接返回 child，父路径已以分隔符结尾时直接拼接，否则插入一个原生分隔符（Linux `/`，Windows `\`）。

`components` 按分隔符切分，保留空组件，不删除 `.` 或 `..`。`parent` 去掉最后一个组件及其前导分隔符；结果为空或原路径没有分隔符时返回 `None`。根路径（Linux `/`，Windows `C:\` 或 `\\server`）的 parent 是 `None`。`file_name` 是最后一个非空组件；没有时 `None`。`extension` 是最后一个组件里最后一个 `.` 之后的部分，且点不能是组件首字符；无点、点在首字符或点在末尾都返回 `None`。`file_stem` 是去掉该扩展名及点后的部分。

`with_file_name` 替换最后一个非空组件，没有组件时等价于 `join`。`with_extension` 替换最后一个组件的扩展名；空 `extension` 去掉点与扩展名；原组件没有扩展名时追加点与新扩展名。

相等、`Ord` 与 hash 使用原生 code unit 序列。Linux 区分大小写与斜杠；Windows 分隔符比较前都规范化为 `\`，盘符与路径体按 UTF-16 code unit 做不区分大小写比较，不调用文件系统。有效 string 在两个目标上都能无损构造 Path。非 Unicode 路径的 `to_string` 返回错误；日志和 UI 可以显式使用 `to_string_lossy`。

## 文件系统

`std.fs` 只提供以 Path 为入口的薄封装，不公开 Dir capability，也不提供 `atomic_write`、`rename_noreplace` 或其它跨平台组合事务。路径检查与随后使用之间可能发生 TOCTOU；`exists` / metadata 只能用于提示和普通控制流，不能作为授权边界。

最低接口为：

```text
struct File
struct OpenOptions
struct Metadata
struct Permissions
struct DirEntry
struct ReadDir

fn open(path: Path, options: OpenOptions) Result[File, io.Error]
fn read(path: Path) Result[Bytes, io.Error]
fn read_string(path: Path) Result[string, io.Error]
fn write(path: Path, content: Bytes) Result[(), io.Error]
fn metadata(path: Path, follow_symlinks: bool) Result[Metadata, io.Error]
fn exists(path: Path) Result[bool, io.Error]
fn canonicalize(path: Path) Result[Path, io.Error]
fn read_dir(path: Path) Result[ReadDir, io.Error]
fn create_dir(path: Path) Result[(), io.Error]
fn create_dir_all(path: Path) Result[(), io.Error]
fn remove_file(path: Path) Result[(), io.Error]
fn remove_dir(path: Path) Result[(), io.Error]
fn remove_dir_all(path: Path) Result[(), io.Error]
fn rename(from: Path, to: Path) Result[(), io.Error]
fn copy(from: Path, to: Path) Result[uint, io.Error]
fn hard_link(from: Path, to: Path) Result[(), io.Error]
fn symlink(target: Path, link: Path) Result[(), io.Error]
fn read_link(path: Path) Result[Path, io.Error]
```

OpenOptions 显式设置 read、write、append、truncate、create、create_new 与 follow_symlinks；非法组合在 open 时返回 InvalidInput。`OpenOptions.new()` 将前六项设为 false，`follow_symlinks` 设为 true；这些选项是公开 bool 字段。必须至少启用 read、write、append 之一；truncate、create、create_new 要求 write 或 append；append 与 truncate 不可同时启用。create_new 优先于 create，排他创建失败不得截断已有文件。`follow_symlinks = false` 拒绝最终路径分量为符号链接；若需先打开再检查，检查与后续截断必须作用于同一已打开句柄，拒绝链接之前不得执行截断。`create_new` 只保证单次 OS open 的排他创建语义。`write` 等价于 create + truncate + write_all，不承诺崩溃一致性或原子替换。rename、copy、remove_dir_all 的跨文件系统、部分完成、symlink 和平台差异通过具体 io.Error 变体报告，标准库不把多次 syscall 包装成伪原子操作。跨设备 rename 返回 `CrossesDevices`，不隐式退化为复制后删除。多步骤操作在取得临时句柄后失败时仍须执行清理；查询或检查的主错误不被后续清理错误覆盖，临时缓冲也须归还。

File 实现 Read、Write 与 Seek，并提供 metadata、set_len、sync_data、sync_all、try_clone 和幂等 close。close 的完成必须建立在平台关闭操作已经执行之上；运行时资源耗尽导致关闭操作无法提交时属于 runtime failure，不能伪造成功关闭。值复制共享同一关闭状态与文件位置；关闭一旦被请求，新操作返回 `Closed`，已开始的操作仍须归还操作租约才能执行平台关闭。后续 close 返回已记录的关闭结果（包括错误），不能重复关闭 OS handle；所有用户租约归还且平台关闭完成后，控制存储才可回收。GC 搬迁不得增加或重复释放资源租约；不可达资源值的同一租约最多释放一次，已转发的旧副本不再承担释放责任。`try_clone` 创建独立可关闭的 OS handle，但文件位置仍按宿主 duplicate 语义共享。`copy` 在截断目的文件前比较 device/inode，源与目的指向同一文件时返回 `InvalidInput`；其它错误可能发生在已经复制部分内容之后。目标打开失败仍关闭已打开的源；传输期间短写只重试未写后缀，零写返回 `WriteZero`，任一读写错误立即停止传输并保留已写内容，不回滚或删除目标。清理错误不得覆盖打开、元数据检查、截断或传输的主错误。传输成功后须检查目标关闭结果，关闭失败返回该错误；只有传输与目标关闭均成功才返回复制字节数。

`canonicalize` 在宿主接受路径但初始输出缓冲不足时扩容并返回完整路径，不截断或仅因缓冲不足报错。`read_link` 只从宿主实际返回的有效记录中读取目标；记录截断、名称区间越界或不满足宿主字符单元对齐要求时返回 `InvalidData`，不能把缓冲尾部解释为目标路径。

`Metadata` 提供 `len()`、`is_file()`、`is_dir()`、`is_symlink()`，并公开 `size:uint`、`permissions:Permissions`、`kind:uint`、`modified_seconds:int`、`modified_nanoseconds:uint`、`device:uint`、`inode:uint`。kind 为 0 其它、1 普通文件、2 目录、3 链接；修改时间以 Unix epoch 为基准。`Permissions` 公开 `readonly:bool` 与宿主原始 `native:uint`，不提供跨平台权限设置事务。

ReadDir 实现 `Iter[Item = Result[DirEntry, io.Error]]` 与 `IntoIter`；`next()` 返回 `Option[Result[DirEntry, io.Error]]`，跳过 `.` 和 `..`，顺序不保证。副本共享游标与 EOF，读取串行化；等待游标的读者恢复时必须重新检查关闭状态。关闭请求先禁止新读取，已经取得操作租约的读取结束后才执行底层关闭；尚未取得游标的等待者不能越过关闭状态开始新的原生读取。EOF 后保持 `None`，关闭后读取返回 `Some(Err(Closed))`，即使此前已经到达 EOF 也一样。`DirEntry.path()`、`file_name()` 返回路径与名称快照，后续目录读取或关闭不改变已返回的名称；内部缓冲容量不足不应悄悄跳过尚未交付的目录项。`metadata()` 重新查询且不跟随链接。File、ReadDir 和复制出来的 OS handle 都按 Adaptive Resource Leasing 管理。regular file 和无法由 poller 异步化的文件系统操作经过有界 `BlockingBridge`；worker、system stack、bridge frame、waiter 和排队 payload受 runtime tuning profile 与 memory pressure 约束，队列饱和时挂起调用协程，不创建额外无界 OS thread。

文件操作的原生缓冲、请求状态和资源控制存储必须计入 runtime 管理内存，并与堆、协程栈及 runtime 辅助映射采用相同的平台计费口径。预留地址空间不等于已提交内存；提交失败不增加 committed，复用已提交页不重复计费，文件分类小计不能与总量再次相加。尚未提交给 native worker 的文件请求在观察到取消后须退出 admission 等待并返回 `Cancelled`，不应为了取消等待无关 worker 完成；退出不消耗 worker 额度，剩余等待者的顺序保持不变。取消请求不能提前扣减仍被 native worker 使用的缓冲。确认线程退出的宿主等待失败、或 runtime 持有的线程句柄无法回收时属于 runtime failure；不得将失败当作普通 pending，也不得发布完成并复用请求存储。原生 worker 栈应随实际需求建立，空闲栈可以缓存复用；压力回收必须先确认相应线程已退出且该栈未被新请求取得，不能仅凭取消标志释放。自然退出排空后，已登记的文件原生映射须全部归还，不能仅依靠进程结束回收来掩盖账目残留。

## 网络

`std.net` 稳定提供 IPv4/IPv6 地址、SocketAddr、系统 DNS resolver、TcpListener、TcpStream、UdpSocket，以及目标支持时由 cfg 控制的 Unix domain socket。transport 操作接入 Read/Write、取消和资源租约。TLS、HTTP、WebSocket 与 QUIC 不在 std。

数字地址使用不可变值：`Ipv4Addr.new(a:byte, b:byte, c:byte, d:byte)`、`Ipv6Addr.new(segments:[u16;8])`、`IpAddr.V4/V6` 和 `SocketAddr.new(ip:IpAddr, port:u16)`。IPv4 提供 `octets()`，IPv6 提供 `segments()`；SocketAddr 提供 `ip()`、`port()`。四种类型均提供 `parse(string)` 与 `to_string()`，解析失败为 `ParseError.InvalidAddress` 或 `InvalidPort`。IPv4 必须恰好四段十进制，除单独的零外不接受前导零；IPv6 接受一次 `::` 压缩和末尾 IPv4，输出使用小写十六进制，压缩第一个最长且长度至少为二的零段。SocketAddr 的 IPv6 始终带方括号；数字解析不调用 resolver，不接受空白、符号、NUL、非 ASCII、zone name 或端口溢出。地址携带数值 `scope_id` 的 IPv6 可由 `SocketAddr.scoped(ip:Ipv6Addr, port:u16, scope_id:u32)` 构造，`scope_id()` 可查询；文本使用 `%` 后的十进制接口编号。

`ResolveError` 区分 `InvalidHost`、`InvalidPort`、`NotFound`、`TemporaryFailure`、`Cancelled`、`System(io.Error)` 与保留原始系统 resolver 码的 `Other(int)`。无地址的成功系统响应映射为 `NotFound`。Unix `AddressError` 区分 `Empty`、`Nul`、`TooLong`；Linux 路径保留原始字节，Windows AF_UNIX 路径使用 UTF-8；两目标的 `sun_path` 容量为 108 字节，末尾 NUL 占一字节，因而路径最多 107 字节。地址构造不访问文件系统。

系统库按实际可达的内建功能加入链接输入：Linux 的系统 Resolver 自动登记目标工具链的 libc、pthread 符号与动态解释器，数字地址解析及 bind 不引入这些依赖；Windows 的网络操作按需登记 Winsock 系统 DLL。不探测编译宿主的库或配置，导入身份、顺序与目标解释器均参与编译缓存身份。

```text
trait ToSocketAddrs {
    fn socket_addrs(self: &Self) Result[Vec[SocketAddr], net.ResolveError]
    fn socket_addrs_cancel(self: &Self, cancel: &CancelToken)
        Result[Vec[SocketAddr], net.ResolveError]
}

struct Resolver
fn Resolver.system() Resolver
fn Resolver.resolve(self: &Self, host: string, port: u16)
    Result[Vec[SocketAddr], net.ResolveError]
fn Resolver.resolve_cancel(self: &Self, host: string, port: u16,
                           cancel: &CancelToken)
    Result[Vec[SocketAddr], net.ResolveError]

struct ConnectOptions {
    pub timeout: Option[Duration]
    pub cancel: Option[CancelToken]
}

fn TcpStream.connect[T: ToSocketAddrs](target: T)
    Result[TcpStream, net.ConnectError]
fn TcpStream.connect_with[T: ToSocketAddrs](target: T, options: ConnectOptions)
    Result[TcpStream, net.ConnectError]
```

SocketAddr、`&[SocketAddr]` 与 string 实现 ToSocketAddrs。string 使用 `host:port`，IPv6 literal 必须写成 `[addr]:port`；host 只接受 ASCII DNS name 或数字地址，空 host、无效端口、NUL 和非 ASCII 名称返回 ResolveError。Unicode 域名必须由调用方先用生态包显式转换为 IDNA A-label。string 目标和 Resolver 都调用系统 resolver；std 不维护第二层 DNS cache，而尊重系统配置、hosts 文件、search domain 与系统缓存。Resolver 返回系统排序的地址快照；取消只停止等待并忽略迟到结果，不能保证宿主 resolver 已中止内部工作。

TcpStream.connect 对单个 SocketAddr 只发起一次连接；对多个地址和 string 解析结果执行固定的 Happy Eyeballs v2 双栈竞速。它保持每个地址族内部的 resolver 顺序并交错 IPv6/IPv4：首个候选立即开始，后续候选间隔 250 ms；当前尝试出现确定性硬失败时立即开始下一候选。第一个成功连接原子获胜并取消其它尝试。ConnectOptions.timeout 是包括 DNS 与全部连接尝试的总时限；None 表示不设 std 时限。全部失败时 ConnectError 保留按启动顺序排列的 `(SocketAddr, io.Error)`，DNS 失败和总超时是独立变体。

服务端不复用 ToSocketAddrs：`TcpListener.bind` 只接受一个已经确定的数字 SocketAddr，避免域名或地址列表只监听首个成功地址却被误认为覆盖全部地址。监听多个地址时，调用方必须逐个 bind，并在任一失败时自行关闭已经创建的 listener。

```text
enum Ipv6BindMode { V6Only, DualStack }

struct BindOptions {
    pub backlog: int
    pub reuse_address: bool
    pub ipv6_mode: Ipv6BindMode
}

fn BindOptions.new() BindOptions

struct DatagramBindOptions {
    pub reuse_address: bool
    pub ipv6_mode: Ipv6BindMode
}

fn DatagramBindOptions.new() DatagramBindOptions
fn UdpSocket.bind(address: SocketAddr, options: DatagramBindOptions)
    Result[UdpSocket, io.Error]
fn TcpListener.bind(address: SocketAddr, options: BindOptions)
    Result[TcpListener, io.Error]
fn TcpListener.accept(self: &Self) Result[(TcpStream, SocketAddr), io.Error]
fn TcpListener.accept_cancel(self: &Self, cancel: &CancelToken)
    Result[(TcpStream, SocketAddr), io.Error]
```

TcpListener 的 accept 等待者进入同一 FIFO 等待队列；允许任意数量的并发 accept，每个已建立连接只交付给队首的一个仍有效等待者。取消只移除该等待者，不关闭 listener，也不消耗其它等待者的名额；没有等待者时连接留在内核 backlog。已交付的 TcpStream 与 listener 独立，关闭 listener 不影响它们。listener `close()` 在 ResourceCell 建立关闭线性化点，停止新的 accept，唤醒全部等待者返回 Closed，并关闭内核 backlog 中尚未交付的连接；该操作幂等。

```text
fn TcpListener.local_addr(self: &Self) Result[SocketAddr, io.Error]
fn TcpListener.close(self: &Self) Result[(), io.Error]
fn TcpStream.local_addr(self: &Self) Result[SocketAddr, io.Error]
fn TcpStream.peer_addr(self: &Self) Result[SocketAddr, io.Error]
fn UdpSocket.local_addr(self: &Self) Result[SocketAddr, io.Error]
fn UdpSocket.peer_addr(self: &Self) Result[SocketAddr, io.Error]
```

local_addr 与 peer_addr 返回值类型的地址快照，不返回底层 sockaddr 的借用。listener、已连接 TCP stream 与已绑定 UDP socket 的 local_addr 成功；未连接 UDP socket 的 peer_addr 返回 NotConnected。TcpStream 的 peer_addr 在 connect 成功后固定；UDP 的 peer_addr 只在 connected 模式成功后存在。

UDP 同时提供无连接与 connected 两组明确不同的操作：

```text
fn UdpSocket.connect(self: &Self, peer: SocketAddr) Result[(), io.Error]
fn UdpSocket.disconnect(self: &Self) Result[(), io.Error]
fn UdpSocket.send(self: &Self, source: Bytes) Result[(), io.Error]
struct DatagramRecv { pub len: int, pub truncated: bool }
fn UdpSocket.recv(self: &Self, destination: &[byte])
    Result[DatagramRecv, io.Error]
fn UdpSocket.recv_cancel(self: &Self, destination: &[byte],
                         cancel: &CancelToken) Result[DatagramRecv, io.Error]
```

`connect(peer)` 是 UDP endpoint 的共享状态转换，不建立 TCP 连接；成功后 send 只发给 peer，recv 只接收 peer 的 datagram，来自其它来源的 datagram 按内核 connected-UDP 语义过滤。send/recv 与 send_to/recv_from 互斥：connected 期间调用无连接操作返回 InvalidState，disconnect 成功后才恢复；未 connected 时调用 send/recv 返回 NotConnected。connect、disconnect 与收发各有线性化点，已经在线性化点提交的操作使用旧状态，之后的操作使用新状态。connected UDP 的 peer_addr 返回当前 peer；disconnect 后再次返回 NotConnected。

UdpSocket 的 close 在收发与 connect/disconnect 控制队列中建立关闭线性化点，唤醒全部等待中的收发和控制操作返回 Closed；已经在线性化点提交的 datagram 不回滚，之后的调用返回 Closed。close 幂等，所有共享别名观察同一个状态。

Darwin 无法保留具体的本地 IP 绑定时，`disconnect` 返回 `Unsupported`，不改变已绑定的地址、端口或已连接的 peer。绑定通配地址的 UDP socket 断开后恢复通配地址，保留原本地端口。

Darwin 的原生 datagram connect/disconnect 失败也可能改变 peer；之后的收发模式以系统确认的 peer 状态为准。例如，重新 connect 失败且系统已移除原 peer 时，仍返回 connect 错误，但恢复无连接收发。

BindOptions.new 固定产生 backlog 1024、reuse_address true、Ipv6BindMode.V6Only。backlog 必须大于 0，宿主内核可以向下钳位；端口 0 请求系统分配端口，并可由 local_addr 查询。reuse_address 只抽象“前一个已关闭 listener 的地址可及时重绑”，不允许两个活 listener 共享同一地址；语义不同的 SO_REUSEPORT 不在 std。ipv6_mode 对 IPv4 地址无作用；DualStack 请求 IPv4-mapped IPv6，平台不支持时返回 Unsupported，不能静默退化为 IPv6-only。UdpSocket.bind 使用不含 backlog 的 DatagramBindOptions，并采用相同 reuse_address 与 ipv6_mode 语义。

TcpStream 允许一个进行中的读与一个进行中的写并行；同方向的多个操作按到达 socket 状态机的顺序排队，不能交错分配同一批 byte。`split()` 返回 TcpReadHalf 与 TcpWriteHalf，它们和原 TcpStream 共享同一 ResourceCell、TCP endpoint、timeout 状态与关闭状态，不复制 OS handle。任一别名 close 都关闭整个 endpoint；只丢弃某个 half 只释放该 lease，最后 lease 才自动关闭。

```text
enum Shutdown { Read, Write, Both }

fn TcpStream.shutdown(self: &Self, direction: Shutdown) Result[(), io.Error]
fn TcpReadHalf.shutdown(self: &Self) Result[(), io.Error]
fn TcpWriteHalf.shutdown(self: &Self) Result[(), io.Error]
```

方向关闭状态由同一 ResourceCell 的全部 stream/half 共享。Shutdown.Read 在单一线性化点标记读方向关闭：已经取得 byte 的 read 可以返回该批数据，仍在等待和未来的 read 被唤醒并成功返回 0（EOF）。Shutdown.Write 排在写方向队列中先前操作之后；它在这些操作已经提交的 byte 之后发送 FIN，排在其后的 write 返回 BrokenPipe。Both 组合两种效果。重复关闭同一方向必须成功，屏蔽 Linux/macOS 重复 shutdown 差异；整个 endpoint 已 close 后调用仍返回 Closed。对端 FIN 同样使 read 返回 0。丢弃 ReadHalf/WriteHalf 只释放 lease，不隐式半关闭；FIN 必须由显式 shutdown 或整个 endpoint 的 close 产生。

TcpStream 与 UdpSocket 提供 `set_read_timeout(Option[Duration])`、`read_timeout()`、`set_write_timeout(Option[Duration])` 和 `write_timeout()`；TcpListener 对应提供 accept timeout。timeout 是所有别名共享的 socket 状态。每个操作开始时捕获相应 timeout，之后修改只影响后来开始的操作；None 表示无限等待。到期返回 io.Error.TimedOut，不关闭 socket，也不取消已经在线性化点提交的 byte。read_cancel/write_cancel 仍使用逐操作 CancelToken，并同时服从捕获的 socket timeout；先被观察者决定结果。一次 primitive 在超时前已有进度就成功返回该次 byte 数，后续调用再观察剩余状态。

socket option 只公开类型化、跨目标有明确语义的子集：

```text
struct InterfaceIndex(pub u32)
struct KeepAliveConfig {
    pub idle: Option[Duration]
    pub interval: Option[Duration]
    pub count: Option[int]
}

fn TcpStream.set_nodelay(self: &Self, enabled: bool) Result[(), io.Error]
fn TcpStream.set_keepalive(self: &Self, value: Option[KeepAliveConfig])
    Result[(), io.Error]
fn TcpStream.set_ttl(self: &Self, hops: u32) Result[(), io.Error]
fn TcpStream.set_recv_buffer_size(self: &Self, bytes: int) Result[(), io.Error]
fn TcpStream.set_send_buffer_size(self: &Self, bytes: int) Result[(), io.Error]

fn UdpSocket.set_broadcast(self: &Self, enabled: bool) Result[(), io.Error]
fn UdpSocket.set_ttl(self: &Self, hops: u32) Result[(), io.Error]
fn UdpSocket.set_recv_buffer_size(self: &Self, bytes: int) Result[(), io.Error]
fn UdpSocket.set_send_buffer_size(self: &Self, bytes: int) Result[(), io.Error]
fn UdpSocket.set_multicast_loop(self: &Self, enabled: bool) Result[(), io.Error]
fn UdpSocket.set_multicast_ttl(self: &Self, hops: u32) Result[(), io.Error]
fn UdpSocket.join_multicast(self: &Self, group: IpAddr,
                            interface: Option[InterfaceIndex]) Result[(), io.Error]
fn UdpSocket.leave_multicast(self: &Self, group: IpAddr,
                             interface: Option[InterfaceIndex]) Result[(), io.Error]
```
每个 setter 都有同名去掉 set_ 前缀的 getter；buffer getter 返回内核实际采用的大小，而不是请求值。nodelay 默认 false，keepalive 默认 None；KeepAliveConfig 中 None 表示该字段使用系统默认，count 必须大于 0。普通 IP ttl/hop-limit 必须是 1..255，multicast ttl 允许 0..255，buffer size 必须大于 0。某目标不能表达所请求配置时返回 Unsupported，不能静默忽略字段。std 不公开 level/name/Bytes 形式的 raw setsockopt，也不公开会让自动 close 阻塞或丢弃未确认数据的 linger；目标特有选项通过 FFI 或生态 package 使用。

```text
struct DatagramRead {
    pub len: int
    pub source: SocketAddr
    pub truncated: bool
}

struct DatagramPacket {
    pub data: Bytes
    pub source: SocketAddr
}

fn UdpSocket.recv_from(self: &Self, destination: &[byte])
    Result[DatagramRead, io.Error]
fn UdpSocket.recv_from_cancel(self: &Self, destination: &[byte],
                              cancel: &CancelToken)
    Result[DatagramRead, io.Error]
fn UdpSocket.recv_packet(self: &Self, max_len: int)
    Result[DatagramPacket, io.Error]
fn UdpSocket.recv_packet_cancel(self: &Self, max_len: int,
                                cancel: &CancelToken)
    Result[DatagramPacket, io.Error]
fn UdpSocket.send_to(self: &Self, source: Bytes, target: SocketAddr)
    Result[(), io.Error]
```

recv_from 每次消费恰好一个 datagram。若 destination 太小，len 等于实际写入长度且 truncated 为 true；尾部被丢弃，绝不静默伪装成完整报文。零长度 datagram 返回 len 0、truncated false。recv_packet 分配恰好容纳完整 datagram 的 Bytes；max_len 为调用方给出的分配上界，负值 panic，报文超过上界时消费该报文并返回 MessageTooLarge。send_to 要么提交整个 datagram 并返回成功，要么返回错误，不报告短写。UdpSocket 与 TcpStream 一样允许一个接收方向操作和一个发送方向操作并行，同方向请求按状态机到达顺序排队。

`recv` 与 `recv_cancel` 返回 `DatagramRecv { len, truncated }`；len 是写入 destination 的 byte 数，每次恰好消费一个来自 connected peer 的 datagram，截断与零长度报文规则和 recv_from 相同。connected UDP 不提供来源地址，因为 peer 已固定。

`ConnectOptions.new()` 默认 timeout 与 cancel 都是 None。`ConnectError` 为 `Resolve(ResolveError)`、`TimedOut`、`Cancelled`、`Failed(Vec[(SocketAddr, io.Error)])`；空候选列表返回空的 Failed，单地址失败也以该列表保留具体地址与错误。

Unix domain socket 的稳定地址面只接受文件系统 Path：

```text
struct UnixAddr
struct UnixListener
struct UnixStream

fn UnixAddr.from_path(path: Path) Result[UnixAddr, net.AddressError]
fn UnixListener.bind(address: UnixAddr) Result[UnixListener, io.Error]
fn UnixStream.connect(address: UnixAddr) Result[UnixStream, io.Error]
```

这些类型只在目标提供文件系统 Unix domain socket 时存在；Linux 另提供 UnixDatagram，Windows 目标不承诺 datagram。

UnixStream 的 `connect_with(UnixAddr, ConnectOptions)` 使用单地址连接时限与取消，不产生 DNS 请求。UnixListener 的 `accept/accept_cancel` 返回 `(UnixStream, Option[UnixAddr])`；Unix stream/listener 的 `local_addr` 与 stream 的 `peer_addr` 返回 `Option[UnixAddr]` 地址快照，未命名 endpoint 为 None，不伪造空 Path。`split()` 返回共享同一 ResourceCell 的 UnixReadHalf/UnixWriteHalf，分别实现 Read/Write，具有与 TCP half 相同的 close、shutdown 和相应方向 timeout 方法。两个目标的 listener 提供 `set_accept_timeout` 与 `accept_timeout`，timeout getter 返回 `Result[Option[Duration], io.Error]`，已关闭资源返回 Closed。

Linux UnixDatagram 提供 bind、connect/disconnect、send/send_to、recv/recv_from、recv_packet、对应逐操作取消版本以及读写 timeout、local/peer addr 和 close。connected recv 返回 DatagramRecv；无连接 recv_from 返回 `UnixDatagramRead { len:int, source:Option[UnixAddr], truncated:bool }`，recv_packet 返回 `UnixDatagramPacket { data:Bytes, source:Option[UnixAddr] }`。来源未命名时为 None；报文消费、截断、连接模式与错误规则和 UDP 相同。UDP 与 UnixDatagram 的发送取消方法为 `send_cancel(source, cancel)` 与 `send_to_cancel(source, target, cancel)`。TCP/Unix half 的 read_buffer 也实现 Read trait 的缓冲追加与负上限 panic 规则。

地址含 NUL 或超过宿主 sockaddr 上限时返回 AddressError。bind 不自动删除已有路径，close 也不 unlink socket 文件；清理必须显式调用 std.fs。Linux abstract namespace、ancillary data、文件描述符传递与 peer credentials 不在首批稳定 std，需走 FFI 或生态 package。Unix stream/listener 的阻塞、取消、timeout、方向关闭和 Adaptive Resource Leasing 与 TCP 对应接口相同。

## 进程与 shell

`std.process.Command` 只接受 executable + argv，不经过 shell；Stdio 可设为 inherit、null、pipe 或已有 File。Child 提供 stdin/stdout/stderr、wait、wait_cancel、try_wait、kill、detach、id 与幂等 close。Command.spawn 在一个线性化点捕获全局 OS 环境和 runtime 虚拟 cwd，再应用 builder 上的 env/remove_env/cwd 覆盖。

`process.Error` 与 `env.Error` 均直接重导出 `std.io.Error`。Command 的副本共享 builder 身份；spawn/output 在入口复制配置，arg 保持追加顺序，同名 env/remove_env 以最后一次调用为准。Linux 名称按 byte 比较，Windows 使用系统 ordinal ignore-case 比较。

executable 是 Path，不搜索 PATH、不补扩展名、不解释 shell。相对 executable 基于本次子进程 cwd；相对 cwd 覆盖基于捕获的虚拟 cwd。空 executable、NUL、空环境名称和含 `=` 的名称均为 InvalidInput；setter 不返回 Result，故 spawn/output 在任何 OS 副作用前统一验证。空参数和空环境值合法，空值不是删除。

Windows 使用明确非空的 application name 与独立可写 UTF-16 命令行；参数按 Microsoft C argv 引号与反斜杠规则编码，保留空参数，不保证外部程序自定义解析规则。含终止符的命令行超过 32767 个 UTF-16 单元返回 InvalidInput，不截断。

```text
struct Command
struct ShellCommand
struct Child
struct Stdio
struct ExitStatus
struct ChildStdin
struct ChildStdout
struct ChildStderr

fn Command.new(executable: Path) Command
fn Command.arg(self: &Self, arg: OsString)
fn Command.args(self: &Self, args: &[OsString])
fn Command.env(self: &Self, name: OsString, value: OsString)
fn Command.remove_env(self: &Self, name: OsString)
fn Command.cwd(self: &Self, path: Path)
fn Command.stdin(self: &Self, value: Stdio)
fn Command.stdout(self: &Self, value: Stdio)
fn Command.stderr(self: &Self, value: Stdio)

fn Stdio.inherit() Stdio
fn Stdio.null() Stdio
fn Stdio.pipe() Stdio
fn Stdio.file(file: File) Stdio
fn Command.spawn(self: &Self) Result[Child, process.Error]
fn Command.output(self: &Self) Result[process.Output, process.Error]

fn ShellCommand.new(script: string) ShellCommand
fn ShellCommand.with(executable: Path, leading_args: &[OsString], script: string)
    ShellCommand
fn ShellCommand.spawn(self: &Self) Result[Child, process.Error]
fn ShellCommand.output(self: &Self) Result[process.Output, process.Error]
#[track_caller] fn exit(code: int) !

fn Child.wait(self: &Self) Result[ExitStatus, process.Error]
fn Child.wait_cancel(self: &Self, cancel: &CancelToken)
    Result[ExitStatus, process.Error]
fn Child.try_wait(self: &Self) Result[Option[ExitStatus], process.Error]
fn Child.kill(self: &Self) Result[(), process.Error]
fn Child.detach(self: &Self) Result[(), process.Error]
fn Child.close(self: &Self) Result[(), process.Error]
fn Child.id(self: &Self) u32
fn Child.stdin(self: &Self) Option[ChildStdin]
fn Child.stdout(self: &Self) Option[ChildStdout]
fn Child.stderr(self: &Self) Option[ChildStderr]
fn ChildStdin.close(self: &Self) Result[(), io.Error]
fn ChildStdout.close(self: &Self) Result[(), io.Error]
fn ChildStderr.close(self: &Self) Result[(), io.Error]

struct Output {
    pub status: ExitStatus
    pub stdout: Bytes
    pub stderr: Bytes
}

fn ExitStatus.success(self: &Self) bool
fn ExitStatus.code(self: &Self) Option[int]
fn ExitStatus.signal(self: &Self) Option[int]
```

`Command` 的 Stdio 默认是 inherit，显式设置后才改为 null、pipe 或已有 File；`output()` 强制把 stdout/stderr 接成独立 pipe，并把 stdin 设为 null。它并行排空 stdout 与 stderr，再等待子进程退出，返回完整 Bytes；不提供无界 pipe 的隐式聚合上限，调用方需要对不受信任的输出自行使用 Stdio.pipe 并流式读取。手动 spawn + pipe 后直接 wait 不会替调用方排空 stdout/stderr；调用方必须并发读取，否则子进程可能因 OS pipe 满而等待。wait 会先关闭仍持有的 stdin，再等待退出；try_wait 不阻塞，只在状态已经可收集时返回 Some。

三个 pipe getter 仅在对应 Stdio 为 pipe 且 Child 尚可使用时返回 Some；重复取得的是共享关闭状态的资源别名，不转移 Child 的所有权。ChildStdin 实现 Write，ChildStdout/ChildStderr 实现 Read；detach/close 关闭所有别名的底层父端。Stdio.file 保留原 File 的资源租约，创建时使用独立临时副本，不消费或关闭调用者的 File。output 的 stdio 覆盖不修改 builder；任一排空失败时终止并清理子进程、取消另一排空操作并等其清理完成，返回最先观察的错误，不返回部分 Output。

wait 成功后退出状态被缓存，重复 wait 与 try_wait 观察同一个状态，不重复等待。kill 向目标发送强制终止；目标已经退出时返回成功，但 kill 不代替 wait。wait_cancel 只取消当前等待者并返回 Cancelled，既不 kill 也不改变 Child；其它等待者继续有效。Child 的资源 lease 最后释放时等价于显式 detach：关闭父侧管道和观察句柄，子进程继续运行；runtime supervisor 负责回收已脱离进程的终止记录。detach 是同一行为的显式幂等请求；detach 后不能再次 wait、kill 或 close。close 才是终止语义：若尚未退出则强制 kill，再 wait/reap；若已经退出则直接收集状态。close 成功后所有别名观察 Closed，close 本身幂等。对进程退出码为零以外的状态，wait 仍成功返回 ExitStatus，成功与否由 `success()` 判断；signal 只在目标能提供信号原因时返回 Some。

预取消的 wait_cancel 在改变 Child 或 stdin 前返回 Cancelled；在途取消只摘除本等待者。退出完成与取消同轮到达时先消费已提交退出结果。close 发布关闭意图后拒绝新操作，已有 wait 返回同一退出状态；重复 close 返回首次关闭结果。detach 后 wait/kill/close 返回 Closed；close 完成后仅 close 幂等，其它操作返回 Closed。id 始终返回原始 pid。脱离进程仍在运行时，单独的 supervisor 观察记录不阻止 runtime 自然退出，shutdown 不终止脱离进程。

Linux 正常退出 code 为 Some(0..255)、signal 为 None；信号退出 code 为 None、signal 为 Some(sig)。Windows code 是完整 u32 无符号扩展到 int，signal 为 None；退出码 259 也是可收集的正常状态，不能仅凭 STILL_ACTIVE 判断仍运行。success 仅对正常退出码 0 为 true。exit 复用 runtime 的进程终止入口，不运行剩余 defer；Linux 观察低 8 位，Windows 观察低 32 位。
管道缓冲读取在 raw 请求暂存区等待完成，只按实际读到的字节数建立托管 Bytes；短读不得把未使用的请求容量长期保留在 ByteBuffer 输出链中。请求释放仍必须晚于 native 完成及唯一一次字节复制，EOF 不追加数据。

ShellCommand.new 在 Linux 固定执行 `/bin/sh -c <script>`，在 Windows 用 GetSystemDirectoryW 取得系统目录下的 `cmd.exe`，固定 `/S /C`；其尾部按 cmd 的原始脚本规则包裹，不套用普通 CRT argv encoder。不读取 `$SHELL`、COMSPEC、PATH 或 PowerShell 偏好。`with` 使用显式 shell executable，并把 leading_args 后的最后一个 argv 元素设为完整 script，复用结构化 argv 编码。

ShellCommand 只接受裸 string，不提供自动模板、参数转义类型或动态拼接 lint。script 的 quoting、命令替换、重定向、glob、编码与注入风险全部按所选 shell 解释；包含不可信数据时必须改用结构化 Command。ShellCommand 是普通程序接口；build.gg 只能使用 std.build.run，不能借它绕过 build action 记录和权限门。

## 环境与虚拟 cwd

`std.env.args()` 返回启动 argv 的不可变 OsString 快照。环境 API 读写真实 OS 进程环境：

```text
fn args() Vec[OsString]
fn get(name: OsString) Result[Option[OsString], env.Error]
fn vars() Result[Vec[(OsString, OsString)], env.Error]
fn set(name: OsString, value: OsString) Result[(), env.Error]
fn remove(name: OsString) Result[(), env.Error]
fn current_dir() Path
fn set_current_dir(path: Path) Result[(), io.Error]
```

`args` 每次返回启动 argv 的独立 Vec，修改结果不会改变进程参数；启动后修改宿主 argv 也不改变该快照。Windows 宿主 argv/cwd 若不能表示为严格 OsString，启动以 InvalidConfiguration 失败，不做有损替换。Gugu runtime 用一把启动时建立的专用进程锁线性化 get、vars、set、remove、cwd 替换和 Command.spawn 的环境/cwd 联合快照；任一 Gugu 协程只观察修改前或修改后的完整环境。外部 C/系统库不使用这把锁：程序若让 FFI 与 env.set/remove 并发访问宿主环境，行为由外部 ABI 决定，Gugu 不保证数据竞争安全。名称和值不得含 NUL；名称还不得为空或含 `=`，否则返回 InvalidInput。Windows 名称按系统 ordinal ignore-case 比较，Linux 按 byte 区分。set 空值后 get 返回 Some(空)，remove 后为 None，删除不存在的键成功。

Linux 环境 API 访问 libc 当前 environ/getenv/setenv/unsetenv，不把启动 envp 当作实时环境；返回值在锁内复制，不保留 libc 指针。Windows get/vars 遇到非法 UTF-16 返回 InvalidData，并仍释放系统环境块；空环境块返回空 Vec。宿主 `=C:` 等系统条目保留给 spawn，但不由 vars 作为普通可修改名称暴露。spawn 释放锁后仅在捕获的环境副本上应用 builder 覆盖，不重新读取全局状态；Windows 最终环境块按 ordinal ignore-case 排序并以双 NUL 终止。

cwd 不调用 OS chdir/SetCurrentDirectory。runtime 启动时读取一次宿主 cwd，之后维护进程内的不可变 Path 快照；set_current_dir 验证目标是目录后原子替换该快照。每个 fs 相对路径操作开始时捕获一次 cwd，并在 syscall 前合成绝对 Path；Command 默认把同一快照显式传给子进程。并发 set_current_dir 不会改变已经开始的 fs/spawn 操作。

set_current_dir 的相对路径基于调用时的旧 cwd；验证失败不改变状态，非目录返回 NotADirectory。空 Path 返回 InvalidInput，不能经 join 变成 cwd。单路径操作捕获一次 cwd；copy、rename、hard_link 在操作任何一端前用同一个 base 固定两端，递归 create_dir_all/remove_dir_all 在进入递归前固定根。symlink 只解析 link 位置，target 保持原始链接文本；read_link 返回宿主实际文本，不附加 cwd。ReadDir 保存解析后的绝对 root，后续 DirEntry.path/metadata 不受 cwd 更新影响。filesystem UnixAddr 在 bind/connect/send_to/datagram_connect 请求开始时解析一次，并重新检查最终 sockaddr 长度，超长返回 InvalidInput；收到的地址仍是原生快照，不改写来源。

FFI 与直接 syscall 仍看到进程启动时的宿主 cwd；需要确定互操作路径的代码必须传绝对 Path。虚拟 cwd 是进程级状态，不是 coroutine-local。

## 时间边界

`std.time` 提供不可序列化的单调 Instant、UTC SystemTime、Duration、sleep 与 timeout。Instant 使用目标提供的、计入系统挂起时间的单调时钟；它只能在同一进程中比较、相减或构造 deadline，不能转换为 Unix timestamp。SystemTime 使用 UTC 墙钟，可以因校时跳变，不能测量持续时间；日历、locale 和时区数据库不在 std。

```text
fn Instant.now() Instant
fn Instant.elapsed(self: &Self) Duration
fn Instant.checked_add(self: &Self, duration: Duration) Option[Instant]
fn Instant.checked_sub(self: &Self, duration: Duration) Option[Instant]
fn SystemTime.now() SystemTime
fn SystemTime.duration_since(self: &Self, earlier: SystemTime)
    Result[Duration, time.Error]
fn sleep(duration: Duration)
fn sleep_cancel(duration: Duration, cancel: &CancelToken)

struct TimedOut

fn timeout[T](duration: Duration, operation: fn(&CancelToken) T)
    Result[T, TimedOut]
```

Duration 提供 `new(seconds:uint, nanoseconds:u32)`（归一化十亿以上的纳秒，秒数溢出时 panic）、`from_secs(uint)`、`from_millis(uint)`、`from_nanos(uint)`，以及 `as_secs() uint`、`subsec_nanos() uint`、`as_nanos() u128`、`is_zero() bool`；秒与不足一秒的纳秒分开保存，转为总纳秒不会因 uint 乘法溢出而丢失时长。`RuntimeStats.gc_pause_total` 使用同一 Duration 类型。

Instant 的比较和相减只对同一进程产生的值定义；不同进程或反序列化值不能互相比较。SystemTime 的 duration_since 在 earlier 晚于当前值时返回 ClockWentBackward，不把墙钟回拨伪装成负 Duration。Duration 不含时区、日历或闰秒；Gregorian 日历和 leap second 不进入稳定语义。sleep(0) 立即返回，正 duration 至少等待该时长；sleep_cancel 被取消时返回 Cancelled，取消不会撤销已经完成的等待。timeout 是带 CancelToken 的操作组合器：`operation` 在新建协程上运行并取得一个新建的 `CancelToken`；deadline 到期时线性化为取消该 token 并返回 `Err(TimedOut)`，在同一时间窗内已经线性化完成的 operation 仍返回其 `Ok` 结果。operation panic 时该 panic 传播给 `timeout` 的调用方；在 `Panic` 字段结构接入前，传播使用固定 message。超时不强制终止外部资源：被取消的协程在自己的取消观察点结束，`timeout` 不等待它收尾，调用方必须使用操作自身的取消契约。

## 运行时控制与信号订阅

`std.runtime` 只公开进程级 runtime 控制和观测，不公开调度队列、GC 元数据、协程栈图或 runtime 私有 TLS。启动环境变量、状态转换、资源耗尽和报告格式见[运行时与运维语义](runtime.md)。

```text
enum RuntimeError {
    InvalidValue,
    Terminating,
}

enum GcTarget {
    Automatic(uint),
    Off,
}

struct TraceConfig {
    pub scheduler: bool,
    pub gc: bool,
    pub signal: bool,
    pub panic: bool,
}

struct RuntimeStats {
    pub live_coroutines: uint,
    pub live_os_threads: uint,
    pub parallelism: uint,
    pub heap_committed_bytes: uint,
    pub heap_live_bytes: uint,
    pub stack_reserved_bytes: uint,
    pub stack_committed_bytes: uint,
    pub stack_live_bytes: uint,
    pub dirty_cpu_active: uint,
    pub dirty_cpu_waiting: uint,
    pub blocking_bridge_active: uint,
    pub blocking_bridge_waiting: uint,
    pub blocking_bridge_workers: uint,
    pub blocking_bridge_queue_bytes: uint,
    pub timer_live: uint,
    pub timer_cancelled: uint,
    pub timer_overflow_bytes: uint,
    pub gc_cycles: uint,
    pub gc_pause_total: Duration,
    pub signal_events_dropped: uint,
    pub trace_events_dropped: uint,
    pub runtime_committed_bytes: uint,
    pub pending_return_bytes: uint,
    pub owner_cache_bytes: uint,
    pub reclaimable_bytes: uint,
    pub remote_return_batches: uint,
    pub remote_return_hops: uint,
    pub forwarded_messages: uint,
    pub mark_ticket_batches: uint,
    pub edge_delta_batches: uint,
    pub mark_credit_pending: uint,
    pub shared_handle_resolves: uint,
    pub shared_handle_forwards: uint,
    pub turn_region_resets: uint,
    pub region_promotions: uint,
    pub compressed_ref_decodes: uint,
    pub range_reserved_bytes: uint,
}

fn available_parallelism() uint
fn parallelism() uint
fn set_parallelism(value: uint) Result[uint, RuntimeError]
fn gc_target() GcTarget
fn set_gc_target(value: GcTarget) Result[GcTarget, RuntimeError]
fn memory_limit() Option[uint]
fn set_memory_limit(value: Option[uint]) Result[Option[uint], RuntimeError]
fn stack_limit() uint
fn safepoint_poll()
fn collect()
fn stats() RuntimeStats
fn trace_config() TraceConfig
fn set_trace(value: TraceConfig) Result[TraceConfig, RuntimeError]
```

setter 是进程级操作，按调用的线性化顺序采用最后发布的值；不会建立业务数据的 happens-before。`set_parallelism` 的参数必须大于 0，动态降低只回收空闲逻辑处理器；`set_gc_target` 的 `Automatic(p)` 使用非负百分数，`Off` 只关闭堆增长触发；`set_memory_limit(None)` 表示无软上限。所有 setter 在 `Terminating` 中返回 `RuntimeError::Terminating`。`collect()` 等待一个完整 GC 周期完成，但不保证空闲页返还 OS，也不运行 finalizer。

`safepoint_poll()` 是无参数 compiler intrinsic：fast path检查当前 `LogicalProcessor` 的抢占/GC poll word，slow path可以确认 stop、保存 roots、让出 coroutine并在恢复后继续。它不执行 I/O，不在 asm、`#[naked]` 或带函数体的 `#[ffi(dirty_cpu)]` 中可用。

`std.runtime` 还登记以下内部 compiler intrinsic；它们按规范路径识别，导入别名不改变原语身份，不是可取地址的普通函数：

```text
fn ownership_publish[T](place: T, value: T)
fn root_publish[T](place: T, value: T)
```

两者均返回 `()`，类型实参可以省略并由目标位置与值共同推断，也可用 `::[T]` 显式指定；只允许一个类型实参和两个值实参。`place` 必须是位置表达式，与 `std.ptr.addr_of` 一样只计算目标地址、不读取旧值；临时值等非位置实参是编译错误。目标地址先于 `value` 求值，二者均在发布区域外完成，随后就地存储句柄。

`T` 必须是受管句柄类型：`&T`、切片、`chan`、`Join`、函数值或 `dyn`。聚合值、`string`、原始指针和位类型不能用于这两个原语；它们不执行 COW、resource 或其它聚合语义复制。

整条存储及其 GC 混合写屏障位于 `NoSafepointRegion` 内，原因分别为 `OwnershipPublish` 与 `RootPublish`。需要写屏障时，区域外的 `BarrierReserve` 保证处理器屏障 buffer 有足够 shade 与 card-mark 额度，区域内只允许不带 refill slow edge 的 `GcWriteBarrierReserved`。这两个原语不建立 happens-before，不替代普通赋值或同步操作，也不得写进 asm 模板；调用方不能借此构造任意无 safepoint 区域。

`heap_committed_bytes` 是 managed heap 数据页的提交量，包含块内未使用页和大对象对齐开销；它与 `runtime_committed_bytes` 分开报告，但两者之和用于 runtime memory limit。`stack_reserved_bytes`、`stack_committed_bytes`与`stack_live_bytes` 分别观察地址 reservation、已提交宿主页和 live coroutine逻辑 stack容量，三者会因亚页共享、cache和decommit而不同；`dirty_cpu_active` 是当前执行 native work的数量，`dirty_cpu_waiting` 是已发布 bridge roots但等待 dirty 额度的调用数。`blocking_bridge_active` 是已取得 BridgeCredit 并执行普通 blocking native 的调用数，`blocking_bridge_waiting` 是 admission waiter 或已发布普通 bridge roots但尚未取得 credit 的调用数，`blocking_bridge_workers` 不超过 `max_blocking_workers`，`blocking_bridge_queue_bytes` 包含 waiter metadata 与排队 payload。timer字段分别统计 active wheel/heap entries、已取消但尚未 compact 的 entries与 overflow heap/runtime slab bytes。上述统计会随并发调度立即变化，不提供取消 native work、强杀线程、固定调度顺序或固定回收时刻的能力。

`std.signal` 把普通 OS 终止通知显式交给用户。没有订阅者时遵循目标 OS 默认动作；订阅不会自动取消根协程、触发 panic 或等待其它用户协程。fatal signal、`SIGKILL`、`SIGSTOP` 和 Windows 不可拦截的同步 fault 不在订阅集合中。

```text
enum Signal {
    Interrupt,
    Terminate,
    #[cfg(os = "linux")] Hangup,
    #[cfg(os = "linux")] User1,
    #[cfg(os = "linux")] User2,
    #[cfg(os = "windows")] Break,
}

enum Error {
    Closed,
    Unsupported,
    InvalidSignal,
    InvalidCapacity,
    Empty,
}

struct SignalEvent {
    pub signal: Signal,
    pub occurrences: uint,
}

struct SignalSubscription

fn subscribe(signals: &[Signal]) Result[SignalSubscription, signal.Error]
fn subscribe_with_capacity(signals: &[Signal], capacity: uint)
    Result[SignalSubscription, signal.Error]
fn SignalSubscription.recv(self: &Self) Result[SignalEvent, signal.Error]
fn SignalSubscription.try_recv(self: &Self)
    Result[SignalEvent, signal.Error]
fn SignalSubscription.close(self: &Self)
fn SignalSubscription.dropped(self: &Self) uint
```

`signals` 不能为空，重复项合并；默认队列容量为 16，显式容量必须大于 0。一个进程可以有多个订阅，每个订阅收到匹配信号的独立副本。相同信号在同一订阅尚未取走时合并，`occurrences` 饱和递增；计数饱和后的额外到达也计入 `dropped()`。不同信号各占一个队列项。队列满时不阻塞 OS 信号处理路径，新增事件计入 `dropped()`。`recv` 只挂起当前协程，`try_recv` 不阻塞；关闭且取尽队列后返回 `signal.Error::Closed`，未关闭但无事件时 `try_recv` 返回 `signal.Error::Empty`。最后一个 `SignalSubscription` 句柄 lease 释放等价于 `close()`。

`std.panic.panic(message)` 是 `!` 返回的 lang item，必须带 `#[track_caller]`。它与 `catch` 共享当前协程的展开边界；未处理时由[运行时与运维语义](runtime.md)产生 `unhandled-panic` 报告。标准库不提供 Go 式 `recover`，也不提供可以抑制 fatal、替换默认报告或改变退出类别的全局 panic hook。被 `catch` 或 `Join.wait()` 处理的 panic 不自动写 runtime 报告。

```text
#[track_caller]
fn panic(message: string) !
```

运行时 trace 通过 stderr 输出 `gugu-runtime-trace-v1` 逐行 JSON；报告和回溯的环境变量、字段及 fatal 分类见[运行时与运维语义](runtime.md)。

## 机器数值与数学

`std.math` 只稳定机器整数和 IEEE `f32` / `f64` 的完整数值层。BigInt、Decimal、Rational、Complex 与 SIMD 是官方 Registry package，不进入 std。

每个整数类型提供 checked、saturating 与 overflowing 的 add/sub/mul/div/rem/neg（无符号类型没有 neg），以及 rotate_left/right、leading/trailing_zeros、count_ones、swap_bytes、reverse_bits、min、max、clamp 和 radix parse/format。checked 运算失败返回 None；saturating 钳位到类型边界；overflowing 返回 `(wrapped_value, overflowed)`。运行时普通算术仍按[类型系统](types.md)的环绕规则。

浮点接口至少包括 abs、copysign、floor、ceil、trunc、round、round_ties_even、fract、sqrt、cbrt、hypot、pow、exp/exp2/expm1、ln/log2/log10/log1p、sin/cos/tan、asin/acos/atan/atan2、sinh/cosh/tanh、asinh/acosh/atanh、erf/erfc、gamma/lgamma，以及 e、pi、tau、sqrt2 和相应常量。基本算术遵循 IEEE 754；超越函数对有限结果的误差不超过 1 ulp，NaN、infinity、signed zero 与 domain error 由函数的 IEEE 规则决定，不返回 Result。

数值解析不接受前后空白，除非调用者先 trim。整数 radix 为 2..36，超出范围是 InvalidRadix；语法错误与目标类型溢出是不同 ParseIntError 变体。浮点解析接受规范十进制、inf、-inf 与 NaN 拼写，返回 ParseFloatError。格式化走静态格式 trait。

## 随机

`std.random` 不提供隐式进程全局或 coroutine-local RNG。安全随机、可替换快速随机与序列稳定算法是三个不同类型：

```text
struct SystemRandom
struct FastRandom
struct Xoshiro256PlusPlus

fn SystemRandom.fill(destination: &[byte]) Result[(), random.Error]
fn SystemRandom.bytes(count: int) Result[Bytes, random.Error]
fn SystemRandom.int(range: Range) Result[int, random.Error]

fn FastRandom.from_system() Result[FastRandom, random.Error]
fn FastRandom.from_seed(seed: u64) FastRandom
fn FastRandom.next_u64(self: &Self) u64

fn Xoshiro256PlusPlus.from_seed(seed: u64) Xoshiro256PlusPlus
fn Xoshiro256PlusPlus.next_u64(self: &Self) u64
```

SystemRandom 直接使用目标 OS 的 CSPRNG，失败返回具体 error。FastRandom 的算法是工具链实现细节；相同 seed 只在同一工具链构建身份内可复现，升级后可以变化。Xoshiro256PlusPlus 的 seed expansion 与输出序列属于稳定 API：相同 u64 seed 在所有目标和工具链产生相同 u64 序列，不能用于密码学。

FastRandom 与 Xoshiro256PlusPlus 共享标准采样方法：无偏 `int(a..b)`、`uint_below(bound)`、`float()`、`fill(&[byte])` 和 `shuffle(&[T])`。整数范围半开且必须 a < b，bound 必须大于 0，否则 panic；实现必须用 rejection sampling 或等价无偏算法，禁止直接 modulo。`float()` 在所有实现中均匀生成 `[0, 1)`，永不返回 1。shuffle 使用无偏 Fisher–Yates。SystemRandom 的相应采样返回 Result；已经成功构造的 PRNG 采样不失败。

## FFI 辅助类型

`std.ffi` 提供 C ABI 的字符串边界和目标相关的透明 C 类型别名；它不改变 `extern "C"` 可接受类型集合。别名的完整宽度与符号性表见[平台与 ABI 参考](platform-abi.md)。`CString` 拥有以单个 NUL 结尾的 byte 序列，`CStr` 是外部拥有、NUL 终止序列的非拥有视图：

```text
struct CString
struct CStr
struct NulError { offset: int }
struct InvalidCString

fn CString.from_string(value: string) Result[CString, ffi.NulError]
fn CString.from_bytes(value: Bytes) Result[CString, ffi.NulError]
fn CString.as_bytes(self: &Self) Bytes
fn CString.as_ptr(self: &Self) *byte
unsafe fn CStr.from_ptr(pointer: *byte) Result[CStr, ffi.InvalidCString]
fn CStr.to_bytes(self: &Self) Bytes
fn CStr.to_string(self: &Self) Result[string, text.EncodingError]
fn CStr.to_string_lossy(self: &Self) string
```

本模块还导出下列透明别名，用于直接对应 C 头文件中的标量类型：

```text
c_char c_schar c_uchar c_short c_ushort
c_int c_uint c_long c_ulong c_longlong c_ulonglong
c_size c_ssize c_intptr c_uintptr c_ptrdiff c_wchar
c_bool c_float c_double
```

这些别名不创建新的类型或 `TypeId`；`c_long`、`c_wchar` 等的目标差异必须按[平台与 ABI 参考](platform-abi.md)的表使用。

`CString.from_*` 拒绝输入中已有 NUL，并在末尾补一个 NUL；as_bytes 不包含终止 NUL，as_ptr 指向以 NUL 结尾的表示。向外部函数传递该指针时，调用方必须让 CString backing 在整个外部调用期间保持 pinned，或把内容复制到 `std.mem` 的非移动缓冲；指针不能跨越可能触发 GC 的 safepoint 保存。`CStr.from_ptr` 要求 pointer 非空、指向可读且最终存在 NUL 的内存；扫描越界、保存超过外部内存寿命或让 CStr 逃逸为长期 Gugu 值都是调用方违反 unsafe 契约。CStr 转换为 Bytes/string 会复制到 Gugu 自有存储，UTF-8 无效时只允许使用 to_string_lossy 或接收 EncodingError。std.ffi 不提供动态库加载器、C varargs、任意 ABI 转换、外部线程手工注册或跨边界异常封装。
