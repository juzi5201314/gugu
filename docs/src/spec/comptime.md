# 编译期执行

Gugu 有两种互相配合的编译期机制：**值 comptime** 和 **源码 comptime 宏**。

值 comptime 在编译期求出一个普通 Gugu 值，例如常量、数组长度或布局参数。源码
comptime 宏则运行一段编译期脚本，生成源码文本，再把文本解析成语法片段并自动放回
主编译流程。普通值 comptime 不会因为结果长得像代码就隐式执行文本；源码生成必须
显式写出 `comptime source` 和 `std.syntax.parse_source`。
前端阶段、`ParsedSource`、展开 query 和抽象分析的实现契约见[comptime 与抽象分析](../internals/comptime-analysis.md)。

## 能做什么

- 泛型实参、`comptime` 参数、`const`、关联常量、数组长度都在编译期已知。`const` 项与关联常量必须能在编译期求值。
- 可以根据 comptime 条件丢掉死分支、决定 `[T; n]` 的 `N`。按目标删除整项用 `#[cfg]`，不是 comptime：被裁侧不必类型检查。
- `size_of` / `align_of` / `offset_of`、`std.src.file` / `line` / `column` 必须在相应类型已知时由早期 comptime 求值。`type_id[T]()` 在 `T` 已知后先形成符号化 `TypeId`；`name()` 可早期求值，`as_int()` 与 `type_id_count()` 只能在闭世界具体类型集合冻结后的 late comptime 求值。
- 禁止把类型当成一等 comptime 值传递：没有 `fn Foo(comptime T: type) type`，对类型抽象只用 `[T]`。禁止在 comptime 里给 `struct` 动态加字段。
- 单态化、特化和其它需要编译期常量的语义选择都可以消费 comptime 结果；内联等纯优化只消费已经证明的事实。
- 编译期可以 panic，效果是编译错误。comptime 里 `panic(...)` 的类型仍是 `!`。
- 源码宏可以生成任意**在当前插入上下文中合法的 Gugu 源码片段**，包括声明、语句、表达式、类型和模式；生成的源码仍必须经过主编译流程的全部语义检查。

## 值 comptime 语法

```gugu
fn repeat[T](comptime n: int, x: T) [T; n] {
    let a: [T; n]
    for i in 0..n {
        a[i] = x
    }
    a
}

const N: int = 4
let xs = repeat(N, 1)
```

`repeat(N, 1)` 靠实参推断 `T`。要显式写类型实参时用 `repeat::[int](N, 1)`，禁止
`repeat[int](N, 1)`（那是下标）。也可以写 `[x; N]` 字面量，语义相同。

对 `[T; N]` 的元素赋值分析：若 `N` comptime，且控制流证明每个 `i in 0..N` 都执行了恰好一次 `a[i] = ...`（典型是 `for i in 0..N`），则 `a` 视为已初始化。证明不了仍是「读取未初始化」编译错误。运行时循环同样走这条分析，不是「解释一遍循环」才放行。

- 参数标 `comptime`：调用处必须传入编译期已知值。
- 块或表达式标 `comptime { ... }` / `comptime expr`：整段在编译期执行，并返回可物化的普通常量。
- 未标注的表达式若所有输入都是 comptime 已知，编译器必须仍能常量折叠；标 `comptime` 是强制「现在就求值，求不了就报错」。

## 早期与 late comptime

普通 `const`、`static`、类型形成、泛型实参和源码宏脚本的求值，先按其传递依赖归类：
不直接或传递依赖 late 值的 `const`/`static` 初始化、类型形成和泛型实参使用**早期
comptime**；源码宏脚本始终使用早期 comptime。直接或传递依赖 `type_id_count()`、或依赖
comptime `TypeId.as_int()` 数值结果的表达式属于 **late comptime**，这种依赖会沿 `const`、
`static`、函数返回和普通表达式传播，不能通过先保存到另一个绑定来变回早期值。依赖 late
值的 `const`/`static` 仍必须在早期固定其类型与存储形状，只能在 late 阶段填写允许的值；
需要早期结果的使用位置不能引用它。

late comptime 的可发布标量固定为 `bool`、`char`、全部内建整数、`f32`/`float` 和
`TypeId`；不包含 string、引用、raw pointer、函数值、句柄或资源。已经由早期 comptime
固定形状的元组、数组或结构体常量可以包含这些 late 标量叶值。显式 `comptime` 块或表达式
的执行域由其传递依赖决定；没有 late 依赖时属于早期 comptime，有 late 依赖时属于 late
comptime。late 值只能用于普通运行时值、运行时控制流和不改变形状的常量初始化，禁止用于
数组长度、类型或布局形成、comptime 泛型实参、`cfg`、源码宏脚本或生成结果、定义/impl
选择、可达性以及任何会新增具体类型或调用目标的求值。运行时控制流的所有分支仍在早期
完成名称解析和类型检查，late 值只决定最终镜像中的常量和分支。

编译器在冻结具体类型集合前，必须不执行 late 表达式地收集其完整求值闭包：所有静态解析
的 callee、类型、impl、布局和符号化 `TypeId` 依赖都进入闭世界输入。无法静态封闭的间接
调用不能用于 late comptime。具体类型集合与稠密 `TypeId` 分配冻结后，编译器执行受限的
late evaluator；它只能读取已冻结的 type universe，不能发现新定义、类型、impl 或 callee。
late 求值失败是编译错误，不得改成运行时求值或重新打开宏展开和单态化。

前端以稳定 late 常量键携带尚未物化的标量，冻结后通过不可变结果表提供数值，不回写已
冻结的 HIR/GIR。阶段、query 和缓存契约见[comptime 与抽象分析](../internals/comptime-analysis.md)。

## 源码 comptime 宏

### 源码宏块

`comptime source { ... }` 是源码生成位置，不是运行时表达式。它的块体是普通 Gugu
编译期脚本，脚本中的绑定、循环、条件、函数调用和编译期分配都由轻量解释器执行。
脚本最后必须产生以下两种结果之一：

- `ParsedSource`；
- `Result[ParsedSource, E]`，其中 `E` 必须实现 `std.error.Error`，且错误链与消息在同一 compiler identity 下确定。

脚本可以把任意编译期已知值格式化进字符串，也可以读取由 `embed_file` 明确登记的
编译输入。宏脚本中的普通值是编译期快照；把 `b = 2` 写入生成文本后，生成结果中
是字面量 `2`，不会形成对运行时绑定 `b` 的引用。

源码宏可以出现在模块项列表、块语句列表、表达式、类型和模式的 source slot。插入上下文
决定结果必须包含哪一类片段：模块位置接受 item 列表，块位置接受语句和可选尾表达式，
表达式位置接受一个表达式，类型位置接受一个类型，模式位置接受一个模式。`ParsedSource`
可以代表这些片段中的任意一种；不适合当前位置的结果是编译错误。

```gugu
const b: int = 2

fn generated_sum() int {
    let value = comptime source {
        let a = 1
        let text = f"{a} + {b}"
        std.syntax.parse_source(text)
    }
    value
}
```

这里的源码宏是 `let value = ...` 的初始化表达式，因此 `parse_source` 使用表达式 source
slot。它返回 `Result[ParsedSource, SyntaxError]`；成功后初始化器自动展开为：

```gugu
let value = 1 + 2
```

加法的名称/运算符解析、参数类型检查和最终运行时求值仍发生在主编译流程；源码宏只在
编译期生成表达式语法。

### 语法解析 API

`std.syntax.parse_source(text)` 只允许在 comptime 脚本中调用。它使用与主编译器相同的
lexer 和 parser，把 UTF-8 字符串解析成不透明的 `ParsedSource`：

```text
parse_source(text: string) -> Result[ParsedSource, SyntaxError]
parse_expr(text: string) -> Result[ParsedSource, SyntaxError]
parse_items(text: string) -> Result[ParsedSource, SyntaxError]
parse_type(text: string) -> Result[ParsedSource, SyntaxError]
parse_pattern(text: string) -> Result[ParsedSource, SyntaxError]
```

`parse_expr`、`parse_items`、`parse_type` 和 `parse_pattern` 是指定解析上下文的便利入口；
`parse_source` 按当前源码宏的插入上下文解析。若脚本在没有插入上下文的普通值
comptime 中调用它，必须使用带明确片段类别的便利入口。

`ParsedSource` 只能由这些解析函数产生，不能由用户构造、按位伪造或通过
`transmute` 获得。它只能在本次编译期求值期间保存和传递，不能写入运行时对象、
`static`、channel、资源句柄或目标镜像。解析函数本身不执行生成代码。

解析失败返回 `Err(SyntaxError)`，脚本可以用普通 `match`、`?` 或其它控制流捕获并
转换错误：

```gugu
comptime source {
    let text = make_source()
    let parsed = std.syntax.parse_source(text)
    match parsed {
        Ok(fragment) => Ok(fragment)
        Err(error) => Err(error)
    }
}
```

源码宏最终返回 `Err` 时，编译器不展开该宏，并把错误转换为编译诊断。宏脚本 panic、
调用禁止的编译期操作、fuel 耗尽或编译期内存耗尽属于宏执行错误；解析成功后生成
片段的名称解析、类型检查、初始化、trait、`unsafe` 和 ABI 错误属于展开源码的普通
编译错误。

### 展开与主编译流程

源码宏不是第二套语义编译器。主流程按下列顺序处理一个宏：

1. 解析并保留外层源码中的 `comptime source` 节点；
2. 对宏脚本及其编译期输入做名称解析和类型检查；
3. 用 comptime 解释器执行脚本；
4. 调用 `std.syntax.parse_*`，得到已解析的 `ParsedSource`；
5. 为生成节点附加展开记录并插入外层 AST；
6. 对生成结果重新执行适用的 `cfg`、定义收集、名称解析、类型检查和 HIR lowering；
7. 没有新的源码宏后，才冻结 HIR 并进入 GIR、单态化和代码生成。

生成的源码可以包含普通 `comptime`、`unsafe`、FFI、泛型和其它语言构造；它们在展开
后按普通源码规则检查。源码宏不能直接返回 HIR、GIR 或 LIR，也不能绕过主前端的
语义验证。

生成 item 可能引入新的定义、impl、vtable 或具体类型。因此源码宏展开必须在闭世界
可达性收集、`TypeId` 分配和 late comptime 之前完成。`type_id_count()` 和 comptime
`TypeId.as_int()` 不能参与宏脚本、生成文本或任何会改变宏展开结果的求值。

### 递归与资源预算

生成源码中的 `comptime source` 不因语法类别而被禁止。宏可以组合，也可以有限递归；
每次产生一个新的源码宏展开都消耗独立的展开预算：

- 当前展开深度；
- 本次 action 的展开次数；
- 生成源码字节数；
- 生成 AST 节点数；
- 宏脚本 comptime fuel；
- comptime heap 字节数。

编译器 profile 为每项预算提供默认值和不可突破的全局硬上限。模块属性或源码宏调用
点可以请求更高的局部展开深度，但不得超过全局硬上限；也可以主动降低其子树预算：

```gugu
#![comptime(expansion_limit = 64)]
```

`expansion_limit` 是当前展开树允许的最大源码宏深度，必须是正的 comptime `int`。
属性参数、继承后的有效预算和所有生成输入都进入编译 action key。达到任一预算时，
编译器必须报告资源上限错误，并显示从外层宏到当前宏的完整展开链；不能把它伪装成
类型错误，也不能静默截断生成结果。

宏递归预算与普通 comptime 的函数递归/循环预算分开计账。生成的普通 `comptime` 值
求值不额外增加源码宏深度；生成新的 `comptime source` 才增加展开深度和展开次数。

## 解释器与抽象分析器

comptime 解释器和运行时的抽象分析是两套机制，必须叠加：

**解释器**在精确值域中执行绑定、循环、`if`、函数调用和编译期堆分配。它负责常量、
宏脚本、数组长度和已知特化分支。它只回答“在这些已知输入下结果是什么”。

**抽象分析器**在范围、别名、内存版本、效果和控制流事实的抽象值域中分析运行时代码。
它负责类型无关的范围传播、越界检查消除、死分支、调用摘要、内联依据和 lint 输入；
它不能把一次具体执行当成对所有运行时输入的证明。

编译器必须对闭世界内所有可达的 Gugu 函数、单态化实例、标准库和 runtime 尽可能进行
跨函数、跨模块和跨 package 的抽象求解。递归函数、互递归函数和循环通过摘要固定点
求解；求解顺序、摘要合并和诊断顺序必须确定，并可纳入增量缓存。

分析结果必须健全：编译器声称已经证明的事实在所有符合语言语义的执行中都成立。
别名不明、未知调用、外部 FFI、并发写入、分析预算耗尽或固定点不收敛时，结果为
unknown；unknown 不能删除运行时检查，也不能被当成 lint 错误。

例如：

```gugu
let n = 20
let v = make_vec()
if v.len() > 10 {
    for i in 0..n {
        if i >= 2 {
            break
        }
        v[i]
    }
}
```

在 `v[i]` 的可达路径上，分析器可以合并以下事实：`v.len() >= 11`、`i >= 0`，以及
未执行 `break` 意味着 `i < 2`。因此得到 `0 <= i < 2 < 11 <= v.len()`，必须删除该
下标的运行时边界检查。若在检查和访问之间存在可能改变 `v` 长度的写入、未知调用、
未建模别名或并发修改，长度事实必须失效并保留检查。

抽象分析不负责改变类型。类型推断仍是约束求解，trait/impl 选择仍按类型规则进行；
分析器只能消费已确定的类型和效果。优化 pass 消费分析证明，lint 消费同一类事实但
不能改变程序合法性或运行时语义。函数是否“使用”必须按宏展开、入口、导出、`used`、
vtable、`type_id` 和 C 回调等全部根判断，不能靠文本搜索。

## 求值环境与阶段边界

值 comptime 和源码宏使用与运行时相同的表达式、类型、值传递、COW string、模式、
`defer` 和 panic 语义，但只允许确定性且可在编译宿主中安全模拟的子集。ResourceCell
不能在 comptime 构造或发布。`Atomic::new`、`Mutex::new`、`RwLock::new`、`Condvar::new`
和 `OnceLock::new` 只允许生成可物化的初始位状态，不执行同步动作、不取得 lease、
不建立运行时 happens-before；相应的 load/store/lock/wait/notify 操作仍禁止。
读取未初始化值、越界、除零、无效转换、显式 panic 或违反 unsafe 前置条件都转成带
源范围的编译错误；comptime 不产生可被目标程序捕获的 Panic 值。

允许的状态只存在于本次 comptime 求值：局部槽、comptime 堆、常量依赖和显式
`embed_file` 输入。禁止读取宿主时间、随机数、环境变量、网络、目标进程 I/O、操作系统
线程状态、FFI、内联汇编、原子、锁、channel、`async`、`yield`、`wait` 或 syscall。
lang item 与标准库函数只有登记在 compiler-owned comptime capability registry 中，且
当前执行域允许其 capability 时才能调用；不能仅因函数体看起来确定就自动放行。用户函数
可以在 comptime 调用，但其全部静态 callee 和操作必须传递地通过同一 capability 检查。
`std.syntax.parse_*` 是只对源码宏域开放的纯解析 intrinsic，不读取未登记输入，也不执行
生成源码。registry 的公开能力集合见[标准库](standard-library.md#comptime-capability-registry)。

`cfg` 先删除不存在的项；余下源码全部名称解析和类型检查。普通 `if comptime_condition`
的未选分支仍必须语法与类型正确，只是不执行。普通 `comptime expr` 强制立即求值并把
结果物化为常量；`comptime source` 的结果例外地物化为本次编译期的 `ParsedSource`，
随后必须回到主前端。任何上下文都不能物化原始宿主指针、运行时句柄、打开的外部
资源或指向 comptime 堆的悬空引用。

早期 `const`、普通 `static`、数组长度、判别值、repr 参数和 comptime 泛型实参组成有向
依赖图；依赖环是编译错误。late comptime 另形成只依赖早期结果与冻结 type universe 的
有向图，禁止任何边返回类型形成、源码宏或可达性。源码宏展开依赖图另行记录；宏脚本递归
和源码宏递归允许，但必须受上述预算限制并在不收敛时明确报告资源上限。

`embed_file` 的内容字节、规范化后的源相对路径和读取失败都属于编译输入。路径不得
逃逸调用源文件所在包允许的编译输入根；符号链接解析后的最终路径同样受限制。实现
必须记录该文件依赖，内容变化会使增量缓存失效。宏定义、宏输入、解析器版本、展开
属性和生成文本同样必须进入对应 query 和 action 的输入摘要。

## 编译期文件输入

`std.mem.embed_file` 是编译器提供的 lang item，只在 comptime 合法：

```text
fn embed_file(comptime path: string) [byte; N]
```

`path` 相对写出该调用的源文件目录，`N` 是文件字节数且在编译期已知；读不出文件或
路径逃出允许的 package 输入根是编译错误。文件内容不要求是 UTF-8，但必须作为显式
编译输入记录。源码宏调用 `std.syntax.parse_*` 时，`{value}` 插值应使用规范的字面量
编码；`$value` 不是当前普通字符串插值语法，不能依赖面向用户的 `Print` 文本隐式变成
代码。

## 展开位置、诊断与确定性

宏脚本自己的 `std.src.file` / `line` / `column` 指向脚本中的调用位置。生成源码的
每个节点保存生成文本中的字节范围、宏调用点、宏定义点和父 `ExpansionId`；生成源码
中的诊断以宏调用点为主位置，以生成文本偏移和宏定义位置为附注。生成代码中的
`std.src` 取宏调用点的逻辑源位置，不使用临时字符串的宿主地址或随机文件名。

宏脚本的局部绑定不自动泄漏到生成源码。生成文本中的普通路径按展开调用点的名称
作用域解析；生成的局部绑定只在生成片段的正常词法作用域内有效。生成 item 遵循
插入模块的可见性和命名冲突规则；同名冲突不能靠展开顺序消歧。字符串宏不提供隐式
运行时变量捕获；需要运行时值时，脚本必须生成合法的运行时表达式文本。

同一 compiler identity、target、feature、cfg、宏输入和显式文件输入必须产生相同的
ParsedSource、展开 AST、抽象分析摘要、诊断和最终语义结果。并行执行宏或分析器时，
完成顺序、线程编号、哈希表遍历和临时地址不能影响这些结果。
