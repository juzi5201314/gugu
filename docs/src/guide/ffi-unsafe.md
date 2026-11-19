# FFI 与 unsafe

Gugu 的 `unsafe` 边界是显式的：原始指针、union、`extern`、intrinsic、`asm` 只能在 `unsafe` 上下文出现。权威规则见[unsafe 与 intrinsic](../spec/unsafe.md)与[平台与 ABI 参考](../spec/platform-abi.md)。

## unsafe 块与 unsafe fn

```gugu
union Bits {
    a: uint
    b: [u8; 8]
}

fn main() {
    let bits = Bits { a: 0x1122334455667788 }
    unsafe {
        if bits.b[0] != 0x88 { panic("endian") }
    }
}
```

读 union 的非活动字段是 unsafe 操作，必须包在 `unsafe {}` 或 `unsafe fn` 里。`unsafe fn` 把责任转移给调用者；安全接口尽量把 unsafe 包在内部。

## 导入 C 函数

`extern "C"` 声明导入；库名与符号必须在编译配置里登记：

```text
extern "C" {
    #[ffi(leaf(stack = 256))]
    fn strlen(s: *byte) uint
    fn read(fd: int, buf: *byte, len: uint) int
}
```

调用效应决定调度代价：`#[ffi(leaf)]` 短调用不释放 processor、`#[ffi(bridge)]` 普通桥接、`#[ffi(dirty_cpu)]` 占受限 dirty CPU 额度。调用点可用 `#[ffi(bridge)] f(...)` 覆盖声明效应。完整约束见[外部调用效应与桥接](../spec/unsafe.md#foreign-call-effects-bridging)。

## 导出 C 函数

有函数体的 `pub unsafe extern "C" fn` 是导出，可用 `#[export_name]` 改符号：

```gugu
#[export_name = "gugu_probe"]
pub unsafe extern "C" fn gugu_probe(x: int) int = x + 1
```

导出边界若 panic，runtime abort 进程；要拦截用 `std.panic.catch`，不能把 Gugu 展开推进外部帧。

## ABI 边界

- Linux 走 System V AMD64、Windows 走 Microsoft x64；按值 aggregate、shadow space、`sret` 的完整槽位规则在[平台与 ABI 参考](../spec/platform-abi.md)。
- `TypeId`、`dyn Trait`、句柄类型不能进 `extern` 签名；`i128`/`u128` 仅 Linux。
- 交给原生代码的 GC 对象必须 `std.mem.pin` 或拷到非移动缓冲；native 可解引用区域不得含受管引用槽。
- `repr(C)`/`repr(C,packed)`/`repr(transparent)`/`repr(u8)` 控制布局，见[类型系统](../spec/types.md)。

## intrinsic 与 asm

`std.ptr`/`std.platform` 的 IR 原语只能在 `unsafe` 块内调用；`asm`/`global_asm` 的目标方言与约束见[unsafe 与 intrinsic](../spec/unsafe.md#asm-global-asm)。
