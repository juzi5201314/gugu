//! x86_64 form 描述符表：编码器与 instruction verifier 的单一事实来源。
//!
//! 表内每条 form 显式给出操作数形状、访问语义、前缀、opcode map、opcode、立即数编码与
//! baseline 特性；编码器只写表内形式，verifier 按 baseline 拒绝超出可接受面的形式。
//! `poll_cost` 是每条机器指令的路径成本权重：当前统一为 1（饱和指令和即路径成本），
//! 表随 profile 校准更新，修改属于 backend schema 变更。

use std::num::NonZeroU8;

use super::reg::Clobbers;
use crate::target::CpuFeature;

pub(crate) use super::rows::FORMS;

/// form 在 [`FORMS`] 中的下标。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FormId(pub(crate) u16);

impl FormId {
    /// 返回表下标。
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

/// 操作数形状；编码器与 verifier 按此校验操作数实例。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperandKind {
    /// 8 位寄存器或内存。
    Rm8,
    /// 16 位寄存器或内存。
    Rm16,
    /// 32 位寄存器或内存。
    Rm32,
    /// 64 位寄存器或内存。
    Rm64,
    /// 8 位寄存器。
    R8,
    /// 16 位寄存器。
    R16,
    /// 32 位寄存器。
    R32,
    /// 64 位寄存器。
    R64,
    /// XMM 寄存器。
    Xmm,
    /// XMM 寄存器或 128 位内存。
    XmmRm,
    /// 仅内存的地址操作数（`lea`）。
    Mem,
    /// 8 位立即数。
    Imm8,
    /// 16 位立即数。
    Imm16,
    /// 32 位立即数。
    Imm32,
    /// 64 位立即数。
    Imm64,
    /// 分支目标（PC 相对 32 位）。
    Rel32,
    /// 分支目标（PC 相对 8 位）。
    Rel8,
    /// 固定使用 `cl` 的移位量操作数。
    Cl,
}

impl OperandKind {
    /// 返回契约与 dump 使用的规范操作数种类名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Rm8 => "rm8",
            Self::Rm16 => "rm16",
            Self::Rm32 => "rm32",
            Self::Rm64 => "rm64",
            Self::R8 => "r8",
            Self::R16 => "r16",
            Self::R32 => "r32",
            Self::R64 => "r64",
            Self::Xmm => "xmm",
            Self::XmmRm => "xmmrm",
            Self::Mem => "mem",
            Self::Imm8 => "imm8",
            Self::Imm16 => "imm16",
            Self::Imm32 => "imm32",
            Self::Imm64 => "imm64",
            Self::Rel32 => "rel32",
            Self::Rel8 => "rel8",
            Self::Cl => "cl",
        }
    }
}

/// 操作数访问语义；与 `Form::operands` 逐位置对应。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Access {
    /// 只读。
    Read,
    /// 只写。
    Write,
    /// 读写。
    ReadWrite,
    /// 只作地址计算（`lea` 的源操作数）。
    Address,
}

/// 强制指令前缀。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Prefix {
    /// 无前缀。
    None,
    /// `0x66` 操作数宽度覆盖。
    OperandSize66,
    /// `0xF3` 重复前缀（SSE 标量单精度等）。
    RepF3,
    /// `0xF2` 重复前缀（SSE 标量双精度等）。
    RepF2,
}

/// opcode map。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Map {
    /// 单字节 opcode。
    OneByte,
    /// `0F` 双字节 opcode。
    TwoByte,
    /// `0F 38` 三字节 opcode。
    ThreeByte38,
    /// `0F 3A` 三字节 opcode。
    ThreeByte3A,
    /// VEX 编码；当前仅登记超 baseline 形式，编码器拒绝。
    Vex,
}

/// 是否强制 REX.W。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RexW {
    /// 不强制；REX.W 由操作数宽度与 form 语义决定为无。
    Ignored,
    /// 强制 64 位操作数宽度。
    Force,
}

/// 立即数编码。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImmKind {
    /// 无立即数。
    None,
    /// 8 位立即数。
    Ib,
    /// 16 位立即数。
    Iw,
    /// 32 位立即数。
    Id,
    /// 64 位立即数（`mov r64, imm64`）。
    Io,
}

/// 显式 `lock` 前缀许可。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Lock {
    /// 禁止 `lock` 前缀。
    Forbidden,
    /// 允许 `lock` 前缀，但操作数必须是内存。
    Allowed,
}

/// 表内一条 form。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Form {
    /// 助记符。
    pub(crate) mnemonic: &'static str,
    /// 操作数形状；与 `access` 等长。
    pub(crate) operands: &'static [OperandKind],
    /// 操作数访问语义。
    pub(crate) access: &'static [Access],
    /// 强制前缀。
    pub(crate) prefix: Prefix,
    /// opcode map。
    pub(crate) map: Map,
    /// opcode 字节。
    pub(crate) opcode: u8,
    /// opcode 低三位携带寄存器编号（`+r` 形式）。
    pub(crate) reg_in_opcode: bool,
    /// ModRM.reg 携带的 opcode 扩展（`/n` 形式）。
    pub(crate) opcode_ext: Option<u8>,
    /// 固定 ModRM 字节（`mfence` 等无操作数形式）。
    pub(crate) fixed_modrm: Option<u8>,
    /// REX.W 策略。
    pub(crate) rex_w: RexW,
    /// 立即数编码。
    pub(crate) imm: ImmKind,
    /// `lock` 前缀许可。
    pub(crate) lock: Lock,
    /// 操作数之外被指令隐式破坏的寄存器（`mul`/`div` 的 `rdx:rax`、`cqo` 的 `rdx` 等）。
    pub(crate) clobbers: Clobbers,
    /// 最低可接受 CPU 特性。
    pub(crate) feature: CpuFeature,
    /// 每条机器指令的路径成本权重；见模块注释。
    pub(crate) poll_cost: NonZeroU8,
}

impl Form {
    pub(crate) const fn new(
        mnemonic: &'static str,
        operands: &'static [OperandKind],
        access: &'static [Access],
        opcode: u8,
        poll_cost: NonZeroU8,
    ) -> Self {
        Self {
            mnemonic,
            operands,
            access,
            prefix: Prefix::None,
            map: Map::OneByte,
            opcode,
            reg_in_opcode: false,
            opcode_ext: None,
            fixed_modrm: None,
            rex_w: RexW::Ignored,
            imm: ImmKind::None,
            lock: Lock::Forbidden,
            clobbers: Clobbers::NONE,
            feature: CpuFeature::X86_64,
            poll_cost,
        }
    }

    /// 登记隐式破坏的寄存器。
    pub(crate) const fn clobbers(self, clobbers: Clobbers) -> Self {
        Self { clobbers, ..self }
    }

    pub(crate) const fn prefix(self, prefix: Prefix) -> Self {
        Self { prefix, ..self }
    }

    pub(crate) const fn map(self, map: Map) -> Self {
        Self { map, ..self }
    }

    pub(crate) const fn ext(self, ext: u8) -> Self {
        Self {
            opcode_ext: Some(ext),
            ..self
        }
    }

    pub(crate) const fn fixed_modrm(self, modrm: u8) -> Self {
        Self {
            fixed_modrm: Some(modrm),
            ..self
        }
    }

    pub(crate) const fn reg_in_opcode(self) -> Self {
        Self {
            reg_in_opcode: true,
            ..self
        }
    }

    pub(crate) const fn rex_w(self) -> Self {
        Self {
            rex_w: RexW::Force,
            ..self
        }
    }

    pub(crate) const fn imm(self, imm: ImmKind) -> Self {
        Self { imm, ..self }
    }

    pub(crate) const fn lock_allowed(self) -> Self {
        Self {
            lock: Lock::Allowed,
            ..self
        }
    }

    pub(crate) const fn feature(self, feature: CpuFeature) -> Self {
        Self { feature, ..self }
    }
}

/// 每条机器指令的路径成本权重单位。
pub(crate) const POLL_COST_UNIT: NonZeroU8 = NonZeroU8::new(1).expect("poll cost 单位在 1..=64 内");

/// 按编号取 form。
pub(crate) fn form(id: FormId) -> &'static Form {
    try_form(id).expect("form 编号来自表内，越界说明表与调用方不同源")
}

/// 按编号取 form；越界返回 `None`（verifier 用它拒绝伪造编号）。
pub(crate) fn try_form(id: FormId) -> Option<&'static Form> {
    FORMS.get(id.index())
}

/// 按助记符与操作数形状查 form：同形状多行时取表内第一行（表内顺序即默认方向）。
///
/// 需要指定方向（如 `mov` 的 `0x89`/`0x8B`）时用 [`form_id_with_access`]。
#[cfg(test)]
pub(crate) fn form_id(mnemonic: &str, operands: &[OperandKind]) -> Option<FormId> {
    FORMS
        .iter()
        .position(|form| form.mnemonic == mnemonic && form.operands == operands)
        .and_then(|index| u16::try_from(index).ok())
        .map(FormId)
}

/// 按助记符、操作数形状与首操作数访问语义查 form。
///
/// 同形状反方向的两个 form（如 `mov r/m64, r64` 的 `0x89` 与 `0x8B`）只能靠访问语义区分；
/// 无操作数形式（`ret`、`mfence`）没有访问表，按形状命中。
pub(crate) fn form_id_with_access(
    mnemonic: &str,
    operands: &[OperandKind],
    first: Access,
) -> Option<FormId> {
    FORMS
        .iter()
        .position(|form| {
            form.mnemonic == mnemonic
                && form.operands == operands
                && (operands.is_empty() || form.access.first() == Some(&first))
        })
        .and_then(|index| u16::try_from(index).ok())
        .map(FormId)
}
