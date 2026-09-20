//! 宿主 VM 支持：可执行内存映射与「按寄存器状态调用代码指针」的适配器。
//!
//! 只被 bench 二进制使用：compiler 保持 `forbid(unsafe_code)`，裸汇编与 mmap 只存在于此。
//! 通过 `#[path = "support/vm.rs"] mod vm;` 引入，模块名固定为 `vm`。
//!
//! 两个 bench 共用一个文件，各自只用到其中一部分（例如上下文切换 bench 不用
//! [`invoke`]），因此模块整体允许未使用项。

#![allow(dead_code)]

use std::arch::naked_asm;

/// 一块独占匿名 mapping。
pub struct Mapping {
    /// 映射基址。
    pub base: *mut u8,
    /// 映射字节数。
    pub bytes: usize,
}

/// 寄存器状态的 `u64` 字数：`0..16` 是 GPR（x86 编码编号，索引 4 是 `rsp` 不参与读写），
/// `16..48` 是 `xmm0`..`xmm15` 的 16 字节值（低半在前）。
pub const STATE_WORDS: usize = 48;

/// `decodes` 之外的通用寄存器数量。
pub const GPR_WORDS: usize = 16;

#[cfg(not(target_os = "windows"))]
mod os {
    use super::Mapping;

    unsafe extern "C" {
        fn mmap(
            address: *mut u8,
            bytes: usize,
            protection: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut u8;
        fn mprotect(address: *mut u8, bytes: usize, protection: i32) -> i32;
        fn munmap(address: *mut u8, bytes: usize) -> i32;
    }

    /// 预留一段匿名私有映射（初始不可访问）。
    pub fn reserve(bytes: usize) -> Mapping {
        // SAFETY: 匿名 mapping 没有外部文件与别名，失败哨兵在产生引用前检查。
        let base = unsafe { mmap(std::ptr::null_mut(), bytes, 0, 0x22, -1, 0) };
        assert_ne!(base.addr(), usize::MAX, "mmap 失败");
        Mapping { base, bytes }
    }

    /// 把整段映射设为可写（`executable = false`）或可读可执行。
    pub fn protect(base: *mut u8, bytes: usize, executable: bool) {
        // SAFETY: 调用者只传入已拥有 mapping 内的完整页；代码页从 RW 单向转为 RX。
        assert_eq!(
            unsafe { mprotect(base, bytes, if executable { 5 } else { 3 }) },
            0,
            "mprotect 失败"
        );
    }

    /// 释放映射。
    pub fn release(mapping: &Mapping) {
        // SAFETY: 映射内已无活跃引用。
        assert_eq!(
            unsafe { munmap(mapping.base, mapping.bytes) },
            0,
            "munmap 失败"
        );
    }
}

#[cfg(target_os = "windows")]
mod os {
    use super::Mapping;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualAlloc(
            address: *mut u8,
            bytes: usize,
            allocation: u32,
            protection: u32,
        ) -> *mut u8;
        fn VirtualProtect(address: *mut u8, bytes: usize, protection: u32, old: *mut u32) -> i32;
        fn VirtualFree(address: *mut u8, bytes: usize, operation: u32) -> i32;
    }

    /// 预留一段 `PAGE_NOACCESS` 的独立 reservation。
    pub fn reserve(bytes: usize) -> Mapping {
        // SAFETY: 只取得独立 reservation。
        let base = unsafe { VirtualAlloc(std::ptr::null_mut(), bytes, 0x2000, 1) };
        assert!(!base.is_null(), "VirtualAlloc reserve 失败");
        Mapping { base, bytes }
    }

    /// 提交可写页或把已提交页设为可读可执行。
    pub fn protect(base: *mut u8, bytes: usize, executable: bool) {
        if executable {
            let mut old = 0;
            // SAFETY: 代码已经完整写入，随后不再修改 RX 页。
            assert_ne!(
                unsafe { VirtualProtect(base, bytes, 0x20, &mut old) },
                0,
                "VirtualProtect 失败"
            );
        } else {
            // SAFETY: 只提交 reservation 中的 payload，不触碰两端 guard。
            assert_eq!(
                unsafe { VirtualAlloc(base, bytes, 0x1000, 4) },
                base,
                "VirtualAlloc commit 失败"
            );
        }
    }

    /// 释放整个 reservation。
    pub fn release(mapping: &Mapping) {
        // SAFETY: 映射内已无活跃引用。
        assert_ne!(
            unsafe { VirtualFree(mapping.base, 0, 0x8000) },
            0,
            "VirtualFree 失败"
        );
    }
}

pub use os::{protect, release, reserve};

impl Drop for Mapping {
    fn drop(&mut self) {
        release(self);
    }
}

/// 按状态装载 GPR 与 XMM、调用 `code`，随后把结果寄存器写回状态。
///
/// # Safety
/// `code` 必须是已映射为可执行的片段并以 `ret` 结束；片段不得修改 `rsp`；
/// `state` 必须指向至少 [`STATE_WORDS`] 个可写 `u64`。
#[unsafe(naked)]
pub unsafe extern "sysv64" fn invoke(_code: usize, _state: *mut u64) {
    naked_asm!(
        // 宿主 callee-saved 先保存：适配器会用被测片段的结果覆盖它们。
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // 40 字节暂存保证 `call` 时 rsp 16 字节对齐，并留出保存 r10/r11 的位置。
        "sub rsp, 40",
        "mov [rsp], rsi",
        "mov [rsp + 8], rdi",
        "mov rax, [rsi + 0]",
        "mov rcx, [rsi + 8]",
        "mov rdx, [rsi + 16]",
        "mov rbx, [rsi + 24]",
        "mov rbp, [rsi + 40]",
        "mov r8, [rsi + 64]",
        "mov r9, [rsi + 72]",
        "mov r10, [rsi + 80]",
        "mov r11, [rsi + 88]",
        "mov r12, [rsi + 96]",
        "mov r13, [rsi + 104]",
        "mov r14, [rsi + 112]",
        "mov r15, [rsi + 120]",
        "movups xmm0, [rsi + 128]",
        "movups xmm1, [rsi + 144]",
        "movups xmm2, [rsi + 160]",
        "movups xmm3, [rsi + 176]",
        "movups xmm4, [rsi + 192]",
        "movups xmm5, [rsi + 208]",
        "movups xmm6, [rsi + 224]",
        "movups xmm7, [rsi + 240]",
        "movups xmm8, [rsi + 256]",
        "movups xmm9, [rsi + 272]",
        "movups xmm10, [rsi + 288]",
        "movups xmm11, [rsi + 304]",
        "movups xmm12, [rsi + 320]",
        "movups xmm13, [rsi + 336]",
        "movups xmm14, [rsi + 352]",
        "movups xmm15, [rsi + 368]",
        // rdi 与 rsi 最后装载：rsi 之前一直是状态指针。
        "mov rdi, [rsi + 56]",
        "mov rsi, [rsi + 48]",
        "call qword ptr [rsp + 8]",
        // r10/r11 先落到暂存区，之后 r11 当状态指针、r10 用来中转它们的值。
        "mov [rsp + 16], r10",
        "mov [rsp + 24], r11",
        "mov r11, [rsp]",
        "mov [r11 + 0], rax",
        "mov [r11 + 8], rcx",
        "mov [r11 + 16], rdx",
        "mov [r11 + 24], rbx",
        "mov [r11 + 40], rbp",
        "mov [r11 + 48], rsi",
        "mov [r11 + 56], rdi",
        "mov [r11 + 64], r8",
        "mov [r11 + 72], r9",
        "mov r10, [rsp + 16]",
        "mov [r11 + 80], r10",
        "mov r10, [rsp + 24]",
        "mov [r11 + 88], r10",
        "mov [r11 + 96], r12",
        "mov [r11 + 104], r13",
        "mov [r11 + 112], r14",
        "mov [r11 + 120], r15",
        "movups [r11 + 128], xmm0",
        "movups [r11 + 144], xmm1",
        "movups [r11 + 160], xmm2",
        "movups [r11 + 176], xmm3",
        "movups [r11 + 192], xmm4",
        "movups [r11 + 208], xmm5",
        "movups [r11 + 224], xmm6",
        "movups [r11 + 240], xmm7",
        "movups [r11 + 256], xmm8",
        "movups [r11 + 272], xmm9",
        "movups [r11 + 288], xmm10",
        "movups [r11 + 304], xmm11",
        "movups [r11 + 320], xmm12",
        "movups [r11 + 336], xmm13",
        "movups [r11 + 352], xmm14",
        "movups [r11 + 368], xmm15",
        "add rsp, 40",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
    );
}
