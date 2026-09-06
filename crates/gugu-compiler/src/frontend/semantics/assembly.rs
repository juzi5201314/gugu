//! 汇编约束在类型检查阶段形成；机器编码由后端消费，不重新猜测调用效应。
use super::super::ast::ExprId;
use super::model::Ty;
mod control;
pub(super) use control::managed_stack;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum AssemblyContext {
    Managed,
    Native,
    Naked,
    Global,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Direction {
    In,
    Out,
    Lateout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum RegisterClass {
    Gpr,
    HighByte,
    Xmm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Register {
    pub(crate) class: RegisterClass,
    pub(crate) index: u8,
    pub(crate) bits: u16,
}
impl Register {
    pub(crate) fn mask(self) -> u64 {
        debug_assert!(self.index < 16, "x86-64-v1 每个寄存器组只有 16 个寄存器");
        1_u64
            << (self.index
                + if self.class == RegisterClass::Xmm {
                    16
                } else {
                    0
                })
    }
    pub(super) fn parse(name: &str) -> Option<Self> {
        if let Some(index) = name
            .strip_prefix("xmm")
            .and_then(|index| index.parse::<u8>().ok())
            .filter(|index| *index < 16)
        {
            return Some(Self {
                class: RegisterClass::Xmm,
                index,
                bits: 128,
            });
        }
        for (index, aliases) in [
            ["al", "ax", "eax", "rax"],
            ["cl", "cx", "ecx", "rcx"],
            ["dl", "dx", "edx", "rdx"],
            ["bl", "bx", "ebx", "rbx"],
            ["spl", "sp", "esp", "rsp"],
            ["bpl", "bp", "ebp", "rbp"],
            ["sil", "si", "esi", "rsi"],
            ["dil", "di", "edi", "rdi"],
        ]
        .iter()
        .enumerate()
        {
            if let Some(width) = aliases.iter().position(|alias| *alias == name) {
                return Some(Self {
                    class: RegisterClass::Gpr,
                    index: index as u8,
                    bits: 8 << width,
                });
            }
        }
        if let Some(index) = ["ah", "ch", "dh", "bh"]
            .iter()
            .position(|alias| *alias == name)
        {
            return Some(Self {
                class: RegisterClass::HighByte,
                index: index as u8,
                bits: 8,
            });
        }
        let rest = name.strip_prefix('r')?;
        let (digits, bits) = match rest.as_bytes().last()? {
            b'b' => (&rest[..rest.len() - 1], 8),
            b'w' => (&rest[..rest.len() - 1], 16),
            b'd' => (&rest[..rest.len() - 1], 32),
            _ => (rest, 64),
        };
        let index = digits
            .parse::<u8>()
            .ok()
            .filter(|index| (8..16).contains(index))?;
        Some(Self {
            class: RegisterClass::Gpr,
            index,
            bits,
        })
    }
}

// 低 16 位为 GPR，随后 16 位为 XMM，最后两位为内存与标志破坏；总数固定小于 64。
pub(super) const MEMORY: u64 = 1 << 32;
pub(super) const FLAGS: u64 = 1 << 33;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Operand {
    pub(crate) direction: Direction,
    pub(crate) register: Register,
    pub(crate) expression: ExprId,
    pub(crate) ty: Ty,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct AssemblyPlan {
    pub(crate) expression: ExprId,
    pub(crate) context: AssemblyContext,
    pub(crate) template: String,
    pub(crate) operands: Vec<Operand>,
    pub(crate) clobbers: u64,
    pub(crate) stack_reserve: Option<u64>,
}
