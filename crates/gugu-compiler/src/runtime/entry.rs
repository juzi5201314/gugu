//! rt0 与镜像内 runtime 之间的启动边界：参数块 word 目录与启动载荷布局。
//!
//! rt0 是 writer 生成的机器码，镜像 runtime 是 Gugu 源；两侧只能靠这一份描述对齐。载荷由
//! writer 放在「rt0 完成后仍可写」的保留区，地址字段同时登记为相对重定位，由 rt0 在算完
//! 加载偏移后补写；`Rt0Payload` 与 `std/runtime/coroutine.gg` 的同名结构逐字段交叉校验。

use std::mem::{offset_of, size_of};

/// rt0 参数块的固定 word 目录；rt0 机器码、两个 writer 与 Gugu 侧镜像 runtime 共用。
pub(crate) mod rt0_params {
    /// 参数块总 word 数。
    pub(crate) const WORDS: usize = 16;
    /// 参数块字节数。
    pub(crate) const BYTES: u64 = (WORDS * 8) as u64;
    /// 链接期 `AT_PHDR` 基线；rt0 用它算加载偏移。
    pub(crate) const BIAS_BASELINE: usize = 0;
    /// 相对重定位表 VA。
    pub(crate) const RELOC_TABLE: usize = 1;
    /// 相对重定位条数。
    pub(crate) const RELOC_COUNT: usize = 2;
    /// rt0 在切换前收尾的只读区 VA（重定位表与 GOT 所在范围）。
    pub(crate) const PROTECT_VA: usize = 3;
    /// 只读区字节数；保留可写区在这段范围之外。
    pub(crate) const PROTECT_LEN: usize = 4;
    /// 用户 `main` 入口。
    pub(crate) const ENTRY: usize = 5;
    /// rt0 启动时装入 `r14` 的值：初始协程槽地址，供 prologue 栈检查读 `stack_check`。
    pub(crate) const BOOT_R14: usize = 6;
    /// 初始 `r15`（`LogicalProcessor*`）。
    pub(crate) const PROCESSOR: usize = 7;
    /// 镜像 runtime 入口 `rt0_start`；未绑定时为 0，rt0 走直调 `main` 的过渡路径。
    pub(crate) const RT0_START: usize = 8;
    /// 启动载荷 VA。
    pub(crate) const PAYLOAD_VA: usize = 9;
    /// 启动载荷字节数。
    pub(crate) const PAYLOAD_LEN: usize = 10;
    /// 初始协程槽 VA（可写保留区，零初始化）。
    pub(crate) const INITIAL_SLOT: usize = 11;
    /// runtime 状态区 VA（可写保留区，零初始化），首部是 `MorestackMailbox`。
    pub(crate) const RUNTIME_STATE: usize = 12;
}

/// 初始协程槽保留字节数；两个 writer 共用同一份预约。
pub(crate) const INITIAL_SLOT_BYTES: u64 = 128;
/// 状态区首部是 104 字节邮箱，后接 512 字节冷记录与 64 字节 panic 记录。
pub(crate) const RUNTIME_STATE_BYTES: u64 = 704;

/// rt0 交给镜像 runtime 的启动载荷；rt0 在其中写回加载偏移，其余字段由 writer 预置为链接期值。
#[repr(C, align(8))]
pub(crate) struct Rt0Payload {
    /// 载荷版本。
    pub(crate) revision: u64,
    /// 用户 `main` 的镜像内入口。
    pub(crate) main_entry: u64,
    /// 初始协程槽地址。
    pub(crate) initial_slot: u64,
    /// runtime 状态区地址（首部为 `MorestackMailbox`）。
    pub(crate) runtime_state: u64,
    /// rt0 写回的加载偏移；载荷整体落在可写保留区，参数块保持只读。
    pub(crate) bias: u64,
    /// main 返回哨兵片段地址；main 的返回槽预植这个地址。
    pub(crate) main_return: u64,
    /// 栈图节地址；Linux 已由 rt0 重定位，PE 是 RVA。
    pub(crate) stackmap: u64,
    /// 函数 RVA 的基址；Linux 已由 rt0 重定位，PE 是 RVA。
    pub(crate) fragment_base: u64,
    /// 栈图节字节数。
    pub(crate) stackmap_len: u64,
    /// 统一展开节地址；Linux 已重定位，PE 是 RVA。
    pub(crate) unwind: u64,
    /// 统一展开节字节数。
    pub(crate) unwind_len: u64,
}

impl Rt0Payload {
    /// 当前载荷版本；布局变化必须递增。
    pub(crate) const REVISION: u64 = 3;

    /// 载荷字节数。
    pub(crate) fn bytes() -> u64 {
        u64::try_from(size_of::<Self>()).expect("载荷适配u64")
    }
}

/// rt0 参数块的 Rust 视图；word 顺序即 `rt0_params` 的常量目录，Gugu 侧 `runtime/core.gg`
/// 的 `Rt0Params` 逐项交叉校验。
#[repr(C, align(8))]
pub(crate) struct Rt0Params {
    /// 链接期 `AT_PHDR` 基线。
    pub(crate) bias_baseline: u64,
    /// 相对重定位表 VA。
    pub(crate) reloc_table: u64,
    /// 相对重定位条数。
    pub(crate) reloc_count: u64,
    /// rt0 收尾的只读范围 VA。
    pub(crate) protect_va: u64,
    /// 只读范围字节数。
    pub(crate) protect_len: u64,
    /// 用户 `main` 入口。
    pub(crate) entry: u64,
    /// rt0 启动时装入 `r14` 的值（初始协程槽）。
    pub(crate) boot_r14: u64,
    /// 初始 `r15`（`LogicalProcessor*`）。
    pub(crate) processor: u64,
    /// 镜像 runtime 入口 `rt0_start`；未绑定时为 0。
    pub(crate) rt0_start: u64,
    /// 启动载荷 VA。
    pub(crate) payload_va: u64,
    /// 启动载荷字节数。
    pub(crate) payload_len: u64,
    /// 初始协程槽 VA。
    pub(crate) initial_slot: u64,
    /// runtime 状态区 VA。
    pub(crate) runtime_state: u64,
    /// 保留 word。
    pub(crate) reserved_13: u64,
    /// 保留 word。
    pub(crate) reserved_14: u64,
    /// 保留 word。
    pub(crate) reserved_15: u64,
}

const _: () = {
    assert!(size_of::<Rt0Params>() == 128 && align_of::<Rt0Params>() == 8);
    assert!(offset_of!(Rt0Params, entry) == 5 * 8);
    assert!(offset_of!(Rt0Params, rt0_start) == 8 * 8);
    assert!(offset_of!(Rt0Params, payload_va) == 9 * 8);
    assert!(offset_of!(Rt0Params, initial_slot) == 11 * 8);
};

const _: () = {
    assert!(size_of::<Rt0Payload>() == 88 && align_of::<Rt0Payload>() == 8);
    assert!(offset_of!(Rt0Payload, main_entry) == 8);
    assert!(offset_of!(Rt0Payload, initial_slot) == 16);
    assert!(offset_of!(Rt0Payload, runtime_state) == 24);
    assert!(offset_of!(Rt0Payload, bias) == 32);
    assert!(offset_of!(Rt0Payload, main_return) == 40);
    assert!(offset_of!(Rt0Payload, stackmap) == 48);
    assert!(offset_of!(Rt0Payload, fragment_base) == 56);
    assert!(offset_of!(Rt0Payload, stackmap_len) == 64);
    assert!(offset_of!(Rt0Payload, unwind) == 72);
    assert!(offset_of!(Rt0Payload, unwind_len) == 80);
};
