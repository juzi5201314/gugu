# unsafe 与 intrinsic

没有 `unsafe`，GC、调度、channel 就必须用另一种语言写。`unsafe` 是语言的一部分。

## 安全子集

不进入 `unsafe` 的代码必须：

- 不把整数当指针解引用
- 不绕过写屏障
- 不越界（无检查下标只在 `unsafe`）
- 不破坏 `string` 的 UTF-8
- 不把未初始化内存当已初始化值读（ZST 除外）
- 不制造数据竞争

## `unsafe fn` 与 `unsafe` 块

- `unsafe fn`：调用方必须在 `unsafe` 里调。
- `unsafe { }`：此处由程序员维持不变量。
- 安全函数可以内含 `unsafe` 块，用来封装原语。

`unsafe` 不关闭 GC，也不关闭类型检查。

## 原始指针 `*T`

绑定默认可变，因此不拆 `*const` / `*mut`。`*T`：

- 可以为 0，可以不对齐，可以悬空
- 拷贝地址
- 解引用必须在 `unsafe` 中

`&T` → `*T`、`uint` → `*T` 必须写成类型构造：`(*T)(&x)`、`(*T)(addr)`。`*T` 转回 `&T` 必须由程序员保证非空、存活、对齐。

## intrinsic

绑定到 IR 原语，不是「内联汇编包装函数」所能替代：

| 职责 | 说明 |
|------|------|
| 裸分配 / arena | GC 堆或区域上的未初始化内存；OS `mmap` / `VirtualAlloc` |
| 写屏障 | 手写 GC 字段赋值 |
| 栈切换 | 保存 callee-saved 与栈指针 |
| 栈边界 / SP | GC 与溢出探测 |
| 栈图 / 类型元数据 | 根遍历 |
| 原子 | `xchg`、`cas`、acquire/release/seqcst；channel 与调度握手 |
| 系统调用 | Linux `syscall`；Windows 对导入符号的调用 |
| 无检查索引 / 转换 | 误用即未定义行为 |
| pin | 禁止移动，供 FFI |
| comptime 嵌入文件 | `embed_file`，只在 comptime 合法 |

未定义行为包括：野指针、数据竞争、破坏 UTF-8 或对象头、漏写屏障、在非 safepoint 认为栈图有效。调试器可以抓一部分；没炸不是定义。

## 内联汇编

允许。必须声明破坏哪些寄存器，以便分配寄存器与生成栈图。runtime 仍应优先 intrinsic。

## FFI

`extern` 声明导入或导出 C ABI 函数：

```
extern "C" fn puts(s: *byte) int

extern "C" {
    fn puts(s: *byte) int
    fn abort()
}

pub extern "C" fn gugu_on_load() {
    ...
}
```

- ABI 字符串目前必须是 `"C"`。其它字符串是编译错误。
- 无函数体的 `extern` 是导入：库名与符号必须在编译配置里显式登记。编译器自己把导入写进镜像（Windows IAT；Linux 动态导入表或内建桩）。禁止靠系统 `ld` 事后扫一堆 `.o` 来解析。
- 有函数体的 `pub extern "C" fn` 是导出。
- Linux System V AMD64，Windows x64。C 字符串 `*byte` 与 `string` 显式转换。交给 C 的 GC 对象必须 pin 或先拷到非移动缓冲。
- 导出给 C 的函数若发生 panic：必须在导出边界用 `std.panic.catch`，否则 runtime **abort 进程**，禁止把 Gugu 展开推进 C 帧。
