//! x86_64 寄存器模型：物理寄存器、虚拟编号与 clobber 位图。
//!
//! 虚拟寄存器只出现在 lowering 与 verifier 之间；`Assembled::constraints` 记录物理分配
//! 必须满足的操作数约束，物理寄存器实例化在 harness 与后续分配阶段完成。

use serde::{Deserialize, Serialize};

/// frame 局部槽的占位虚拟编号基址。
///
/// `Op::StackAddr` 的 lowering 产出 `Reg::Virtual(FRAME_SLOT_BASE + slot)` 作为占位基址，
/// 分配阶段把它改写成 `[rsp + local_offset(slot)]`。值编号与 lowering 临时编号必须
/// 小于该基址，两个空间才不会混淆（`select` 侧有 `debug_assert` 与上界校验）。
pub(crate) const FRAME_SLOT_BASE: u32 = 1 << 30;

/// 16 个通用寄存器；声明顺序即 ModRM/REX 的机器编码顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum Gpr {
    Rax,
    Rcx,
    Rdx,
    Rbx,
    Rsp,
    Rbp,
    Rsi,
    Rdi,
    R8,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
}

impl Gpr {
    /// 返回机器编码编号。
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }

    /// 判定低字节（`r8` 形态）是否可在无 REX 的情况下编码。
    ///
    /// `Rsp`/`Rbp`/`Rsi`/`Rdi` 的低字节编码指向 `ah`/`ch`/`dh`/`bh`，因此排除。
    pub(crate) const fn is_byte_encodable(self) -> bool {
        !matches!(self, Self::Rsp | Self::Rbp | Self::Rsi | Self::Rdi)
    }

    /// 返回汇编名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Rax => "rax",
            Self::Rcx => "rcx",
            Self::Rdx => "rdx",
            Self::Rbx => "rbx",
            Self::Rsp => "rsp",
            Self::Rbp => "rbp",
            Self::Rsi => "rsi",
            Self::Rdi => "rdi",
            Self::R8 => "r8",
            Self::R9 => "r9",
            Self::R10 => "r10",
            Self::R11 => "r11",
            Self::R12 => "r12",
            Self::R13 => "r13",
            Self::R14 => "r14",
            Self::R15 => "r15",
        }
    }

    /// 返回 clobber 位图中的位编号。
    pub(crate) const fn bit(self) -> u32 {
        1_u32 << self.code()
    }
}

/// 16 个 XMM 寄存器；声明顺序即机器编码顺序。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum Xmm {
    Xmm0,
    Xmm1,
    Xmm2,
    Xmm3,
    Xmm4,
    Xmm5,
    Xmm6,
    Xmm7,
    Xmm8,
    Xmm9,
    Xmm10,
    Xmm11,
    Xmm12,
    Xmm13,
    Xmm14,
    Xmm15,
}

impl Xmm {
    /// 返回机器编码编号。
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }

    /// 返回汇编名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Xmm0 => "xmm0",
            Self::Xmm1 => "xmm1",
            Self::Xmm2 => "xmm2",
            Self::Xmm3 => "xmm3",
            Self::Xmm4 => "xmm4",
            Self::Xmm5 => "xmm5",
            Self::Xmm6 => "xmm6",
            Self::Xmm7 => "xmm7",
            Self::Xmm8 => "xmm8",
            Self::Xmm9 => "xmm9",
            Self::Xmm10 => "xmm10",
            Self::Xmm11 => "xmm11",
            Self::Xmm12 => "xmm12",
            Self::Xmm13 => "xmm13",
            Self::Xmm14 => "xmm14",
            Self::Xmm15 => "xmm15",
        }
    }

    /// 返回 clobber 位图中的位编号。
    pub(crate) const fn bit(self) -> u32 {
        1_u32 << self.code()
    }
}

/// 操作数或结果位置：物理寄存器或 lowering 分配的虚拟编号。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Reg {
    /// 已绑定到物理通用寄存器。
    Gpr(Gpr),
    /// 已绑定到物理 XMM 寄存器。
    Xmm(Xmm),
    /// 待分配的虚拟寄存器；编号在单个 fragment 内稠密。
    Virtual(u32),
}

/// clobber 位图：位 `i` 表示物理寄存器 `i` 被序列破坏。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Clobbers {
    /// GPR 位图；位编号见 [`Gpr::bit`]。
    pub gpr: u32,
    /// XMM 位图；位编号见 [`Xmm::bit`]。
    pub xmm: u32,
}

impl Clobbers {
    /// 空 clobber 集合。
    pub(crate) const NONE: Self = Self { gpr: 0, xmm: 0 };

    /// 返回只含一个 GPR 的集合。
    pub(crate) const fn gpr(gpr: Gpr) -> Self {
        Self {
            gpr: gpr.bit(),
            xmm: 0,
        }
    }

    /// 返回只含一个 XMM 的集合。
    pub(crate) const fn xmm(xmm: Xmm) -> Self {
        Self {
            gpr: 0,
            xmm: xmm.bit(),
        }
    }

    /// 返回两个集合的并集。
    pub(crate) const fn union(self, other: Self) -> Self {
        Self {
            gpr: self.gpr | other.gpr,
            xmm: self.xmm | other.xmm,
        }
    }

    /// 判定是否包含该 GPR；仅供 clobber 断言。
    #[cfg(test)]
    pub(crate) const fn contains_gpr(self, gpr: Gpr) -> bool {
        self.gpr & gpr.bit() != 0
    }

    /// 判定是否包含该 XMM；仅供 clobber 断言。
    #[cfg(test)]
    pub(crate) const fn contains_xmm(self, xmm: Xmm) -> bool {
        self.xmm & xmm.bit() != 0
    }
}
