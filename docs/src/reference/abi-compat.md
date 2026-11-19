# ABI 与兼容性矩阵

权威定义见[平台与 ABI 参考](../spec/platform-abi.md)。

## 目标矩阵

| 目标名 | ABI | 镜像格式 | 导入机制 |
| --- | --- | --- | --- |
| `x86_64-linux` | System V AMD64 | ELF（PT_INTERP + DT_NEEDED 动态导入） | 动态导入表/内建桩 |
| `x86_64-windows` | Microsoft x64 | PE32+（AMD64） | IAT |

`--target` 取以上目标名；默认宿主。双目标编译的 CompilerIdentity、镜像计划与 action key 各自独立。

## 参数与返回槽位

- **SysV**：整数参数走 RDI/RSI/RDX/RCX/R8/R9，浮点走 XMM0–7；bank 耗尽按声明顺序落栈（16 字节边界）。`sret` 隐藏指针占参数槽 0。
- **Win64**：位置 0–3 共享槽 RCX/RDX/R8/R9 与 XMM0–3（位置而非类型决定），位置 ≥4 落栈且 32 字节 shadow space 固定保留。
- **aggregate 边界**：0/1/2/16/17 字节按值聚合的分类、整组 `memory` 回退、对齐填充与 `sret` 结果清空见[平台与 ABI 参考](../spec/platform-abi.md)的一致性检查节；`i128`/超宽按值聚合按 8 字节 lane 拆分归入同一矩阵。

## 兼容性规则

- `extern "C"` 签名禁用 `TypeId`、`dyn Trait`、句柄类型；`i128`/`u128` 仅 Linux（Windows 禁止）。
- `!` 可映射 C `_Noreturn`；导出边界 panic → abort（用 `std.panic.catch` 拦截）。
- 外部线程接入普通 `ForeignBridge` 需线程符号（Linux 经 libc 导入）满足后端 invariant；`ForeignLeaf`/`DirtyCpu` 禁止回调。
- `repr(C)`/`packed`/`transparent`/整数 repr 是跨边界的布局契约；未标 repr 的类型布局是实现细节，不能写进外部协议。
