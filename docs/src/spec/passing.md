# 值、句柄与传递

用户只需要四条：

1. **`f(x)` 永远合法。** 产生 `x` 的语义副本；没有 move 后再用错误，也不要求 `Clone`。
2. **`f(&x)` 是引用传递。** 引用指向绑定 `x` 的槽，callee 能改 caller 的那个槽。
3. **身份句柄复制后共享状态。** `Vec`、channel、Join、普通 GC 句柄的副本仍指向同一对象。
4. **COW 与资源是编译器管理的句柄。** string、ByteBuffer 副本共享密封 backing 但保持独立值语义；Bytes 只读共享；资源副本共享 ResourceCell，并由 Adaptive Resource Leasing 管理最后 release。

要两份互不影响的普通对象图时写 `clone`。string 与 ByteBuffer 已有 COW 值语义，不需要为了防止后续修改互相影响而 clone；资源通常不实现 Clone，复制资源值只增加同一底层资源的 lease。

编译器内部可以做逃逸分析、拷贝消除、borrow/transfer 摘要、COW seal 与资源 dup/drop 融合；这些只许改变机器码，不许变成用户可见的所有权门槛。

## 与 Rust 所有权的边界

| Rust | Gugu |
|------|------|
| 非 Copy 的 `f(x)` 是 move，再用 `x` 报错 | `f(x)` 始终合法；最后使用的 transfer 是优化 |
| 非 Copy 又想留下原值通常要 Clone | 参数传递不要求 Clone |
| `&` 带生命周期，逃逸时报错 | `&` 逃逸时槽升到 GC 堆 |
| `String` 唯一拥有 backing | Gugu string 用密封式 COW 保持可复制值语义 |
| Drop 在 owner 退出时运行 | 普通类型没有 Drop；外部资源由 Adaptive Resource Leasing 在最后 lease 结束时 release |

编译器管理动作不是任意用户回调。COW seal、resource dup/drop/publish 和 GC 写屏障一样由类型描述符与 intrinsic 固定，不能重载。

## string、身份句柄与资源句柄

三类按值传递的可观察差异为：

| 类型 | `f(x)` 的语义 | callee 修改后 caller 的值 | 管理动作 |
|------|---------------|---------------------------|----------|
| `string` | O(1) COW 值复制 | 不变；callee 写时分离 | 真实复制或快照 seal backing |
| `Vec[T]` / channel / Join | 复制身份句柄 | 观察到同一共享对象状态 | 普通 GC 句柄复制 |
| File / socket / Child 等资源 | 复制同一 ResourceCell 的 lease | 观察到同一 open/closed 状态 | borrow/transfer/dup/drop；跨协程时 publish |
| `&T` | 复制槽地址 | 双方访问同一槽 | 引用逃逸时装箱 |

string 示例：

```text
fn suffix(value: string) {
    value.push('!')
}

let text = "ok"
suffix(text)
// text 仍是 "ok"；参数 backing 可以先共享，push 时分离
```

身份句柄示例：

```text
fn grow(values: Vec[int]) {
    values.push(1)
}

let values = Vec::new()
grow(values)
// values.len() == 1；双方指向同一个 VecObj
```

资源示例：

```text
fn inspect(file: File) {
    // 不逃逸时编译器按 borrow ABI 调用，不必真的 dup lease
    read_header(file)
}

let file = File.open(path)?
inspect(file)
// file 仍合法；最后一个 lease 结束时自动 release
```

方法调用对 `&Self` 自动取槽地址。`text.push` 因而修改当前 text 槽，必要时把它换绑到新 COW backing；`vec.push` 修改共享 VecObj；`file.close` 原子关闭共享 ResourceCell。普通函数调用不自动给用户暴露 `&` / `*` 转换。

`clone()` 对普通身份句柄构造语义独立的对象图。string 的 clone 允许物理共享 sealed backing，只要后续修改保持值语义。资源若要复制底层 OS 资源，必须使用该类型明确返回 Result 的 `try_clone` 或等价领域方法，不能由 Clone 隐式执行 syscall。

## 表示类别

实现为每个具体类型生成值描述符，至少区分：

- **位值。** 标量、只含位的结构体、元组、数组、枚举。普通复制是 memcpy，两份值独立。
- **身份句柄。** `Vec`、channel、Join、胖 fn、dyn Trait 与普通 GC/arena 引用。复制指针或胖指针后共享载荷。
- **COW 值句柄。** string、ByteBuffer 与 Bytes。string/ByteBuffer 复制要保持独立值语义并在必要时 seal；Bytes backing 始终不可变。
- **资源句柄。** 包含 ResourceCell lease 的类型。复制可以由 borrow/transfer 消除，否则执行固定 dup；槽结束、覆盖或展开时执行固定 drop。

用户结构体按字段组合描述符。包含 string/ByteBuffer 的结构体复制后相应字段保持 COW 值语义；包含 Vec 的字段共享 VecObj；包含资源的字段参加 lease 管理。递归类型仍必须通过句柄或 `&T` 间接连接。

递归类型的字段必须是句柄或 `&T`（`next: Option[&Node]`），不能把无限大的 `Node` 嵌进 `Node`。

## `f(x)` 与 `f(&x)`

调用处按你写的做，函数参数类型必须对上：

| 参数 | 实参 | 含义 |
|------|------|------|
| `T` | `x: T` | 浅拷贝表示 |
| `&T` | `&x` | 引用传递，指向 `x` 这个绑定 |
| `T` | `&x` | 类型错误（不会偷偷解引用再拷一份，避免「我以为在共享」） |
| `&T` | `x: T` | 类型错误（不会偷偷取地址，避免「我以为在拷贝」） |

方法除外：`p.len()` 若方法接收者是 `&Self`，编译器可以插 `&p`。这是 UFCS 的糖，普通函数调用不自动取地址。这样 `f(x)` / `f(&x)` 对用户始终是可靠信号。

`&x` 的语义是 **C / Go 那种槽位地址**，不是 Rust 那种「这份值的临时租约」：

```
let x = 1
let r = &x
x = 2
// *r == 2，因为 r 指向 x 的槽
```

`&x` 被存起来、送进 `async`、放进结构体：槽必须活得足够久 → 编译器把该绑定**装箱到 GC 堆**（或一开始就在堆上），用户看不到生命周期错误。

## 大结构体

`f(big)` 对 1KB 的结构体仍然是浅拷贝，合法。编译器必须对超过 64 字节的按值位结构体发出 lint `large_copy`，提示改 `&T`。这不是类型错误。可用 `#[allow(large_copy)]` 关掉某一处。热路径靠诊断 + 逃逸分析，不靠 Clone 门槛。诊断通道见 [词法 · 诊断](lexical.md)。

## `clone`：只要深拷贝时才出现

```
trait Clone {
    fn clone(self: &Self) Self
}
```

- 位值的 clone 等于语义复制（可自动实现）。
- 普通身份句柄的 clone 复制载荷并构造独立对象。
- string 的 clone 产生语义独立值；实现可以继续共享 sealed backing，因为任一修改都会分离。
- channel 与 Join 有语言给出的 `impl !Clone`。资源类型通常也 `impl !Clone`；复制资源值只共享同一个 ResourceCell，复制底层 OS 资源必须使用领域 `try_clone`。
- 混合结构体可 `#[derive(Clone)]`：字段都 Clone 才行。
- **任何类型不实现 Clone 也能传入 `f(x)`。**

没有用户级 Copy trait。实现内部的 memcpy、seal、dup/drop 与 publish 由值描述符决定。

## `Vec` / 切片与再分配

`Vec` 的权威状态在堆对象上（见上）。容量不够时堆对象改指向**新** backing，旧 `&[T]` 仍指向旧块，由 GC 回收。不会出现 Rust 那种「再分配后引用悬空因此必须借用检查」。

同一 `Vec` 上的 `push` 所有持有该句柄的人都看得见。并发写入同一 `Vec` 是数据竞争，用 `chan` / `std.sync`。

## 什么时候写 `&x`

日常按值传 string、Vec、channel 或资源即可。`f(&x)` 表示 callee 需要直接访问或换掉 caller 的绑定槽，不是“让类型可以传递”的许可证。

用户结构体默认按字段产生语义副本。需要共享同一可变身份时，把对象放在 GC 句柄、标准身份容器或 `&T` 后面；需要外部资源寿命时，由标准库或 FFI 包用 `std.resource` 的受限 ResourceCell 封装，不能手写 GC finalizer。

## 编译器管理动作

编译器必须做逃逸分析、不逃逸栈分配、最后使用消除、大拷贝诊断、精确栈图、COW seal 和资源描述符。对于 COW 值与资源参数，闭世界摘要至少区分 borrow、transfer、真实复制和 publish：

- borrow 只在调用期间使用，不 seal COW backing，也不 dup resource lease；
- transfer 复用最后一次使用的值表示；
- 真实 string/ByteBuffer 复制 seal backing，真实资源复制 dup lease；
- 写入 global、共享 GC 图、channel 或 async 捕获先 publish resource，并 seal 可变 COW backing。

分析失败的合法降级是装箱、seal、保留 dup/drop 或写屏障；禁止把内部分析失败显示成生命周期、move 或“需要 Clone”错误。

普通位值与身份句柄的浅拷不调用用户代码。COW seal、resource dup/drop/publish 是编译器固定管理动作，也不调用可重载用户代码。赋值、参数传递、返回、模式绑定、聚合构造和 dyn Any 装箱都按字段描述符执行相同语义。

重新给绑定赋值时，先为将被覆盖的资源字段执行 drop，再把新值按描述符写入；此前指向该槽的 `&T` 看到新值。panic 展开与正常退出必须对活跃资源槽执行相同 drop glue。collector 搬迁对象只是物理 relocation，不是语义复制，禁止 seal 或 dup。
